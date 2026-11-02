use super::*;
use cloud_filter::placeholder::ReadType;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    os::windows::{fs::MetadataExt, io::AsRawHandle},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ};

pub(super) const STAGING_PREFIX: &str = ".cloud-mirror-upload-";

#[derive(Debug)]
pub(super) enum Work {
    Sync(PathBuf),
    DeleteFile(PathBuf),
    DeleteDirectory { path: PathBuf, id: i64 },
    Rename { source: PathBuf, target: PathBuf },
}

impl Work {
    fn touches(&self, path: &Path) -> bool {
        match self {
            Self::Sync(_) => false,
            Self::DeleteFile(source) | Self::DeleteDirectory { path: source, .. } => {
                path.starts_with(source)
            }
            Self::Rename { source, target, .. } => {
                path.starts_with(source) || path.starts_with(target)
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SyncWorker {
    sender: Option<Sender<Work>>,
    thread: Option<JoinHandle<()>>,
}

impl SyncWorker {
    pub(super) fn new(files: MirrorFiles) -> Self {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || run(files, receiver));
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }

    pub(super) fn enqueue(&self, work: Work) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(work);
        }
    }
}

impl Drop for SyncWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(files: MirrorFiles, receiver: Receiver<Work>) {
    let interval = Duration::from_millis(200);
    let mut pending = HashSet::new();
    let mut changes = VecDeque::new();
    let mut errors = HashMap::new();
    let mut change_error = None;
    let mut next_pass = Instant::now() + interval;
    loop {
        let work = match receiver.recv_timeout(next_pass.saturating_duration_since(Instant::now()))
        {
            Ok(work) => Some(work),
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => None,
        };
        for work in work.into_iter().chain(receiver.try_iter().take(1024)) {
            match work {
                Work::Sync(path) => {
                    pending.insert(path);
                }
                change => changes.push_back(change),
            }
        }
        if Instant::now() < next_pass {
            continue;
        }
        // Preserve callback order and finish remote name changes before uploading
        // into those paths. A failed command stays at the front for retry.
        while let Some(change) = changes.front() {
            match files.apply_change(change) {
                Ok(true) => {
                    changes.pop_front();
                    change_error = None;
                }
                Ok(false) => break, // A directory still has a deleting handle open.
                Err(error) => {
                    let message = format!("{error:#}");
                    if change_error.as_ref() != Some(&message) {
                        eprintln!("{change:?}: {message}; will retry");
                        change_error = Some(message);
                    }
                    break;
                }
            }
        }
        for path in std::mem::take(&mut pending) {
            if changes.iter().any(|change| change.touches(&path)) {
                pending.insert(path);
                continue;
            }
            match files.sync_path(&path) {
                Ok(true) => {
                    if errors.remove(&path).is_some() {
                        println!("sync retry resolved: {path:?}");
                    }
                }
                Ok(false) => {
                    pending.insert(path);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if errors.get(&path) != Some(&message) {
                        eprintln!("sync {path:?}: {message}; will retry");
                        errors.insert(path.clone(), message);
                    }
                    pending.insert(path);
                }
            }
        }
        next_pass = Instant::now() + interval;
    }
}

impl MirrorFiles {
    // Sync this entry only. Recheck its current metadata before writing remotely.
    fn sync_path(&self, path: &Path) -> anyhow::Result<bool> {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
        {
            return Ok(true);
        }
        if self.rename_pending(path) {
            return Ok(false);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(true);
        }
        let path = canonical_path(path)?;
        ensure!(
            path.starts_with(&self.client_path),
            "path is outside CLIENT"
        );
        if metadata.is_dir() {
            self.sync_directory(&path).context("sync directory")?;
        } else if metadata.is_file() {
            self.upload(&path).context("upload")?;
            self.apply_pin_state(&path)?;
        }
        Ok(true)
    }

    fn sync_directory(&self, path: &Path) -> anyhow::Result<()> {
        if self.readonly || path == self.client_path {
            return Ok(());
        }
        let mut placeholder = open_placeholder(path)?;
        let info = placeholder.info()?;
        if info.as_ref().is_some_and(|info| info.is_in_sync()) {
            return Ok(());
        }
        let relative = path.strip_prefix(&self.client_path)?;
        ensure!(valid_relative_path(relative), "invalid directory path");
        let destination = self.create_server_directory(relative)?;
        if info.is_none() {
            placeholder.convert_to_placeholder(
                ConvertOptions::default().blob(destination.as_os_str().as_encoded_bytes().to_vec()),
                None,
            )?;
        }
        // This acknowledges the directory's own remote entry. Children sync separately.
        placeholder.mark_in_sync(true, None)?;
        println!("synced directory {}", relative.display());
        Ok(())
    }

    pub(super) fn upload(&self, path: &Path) -> anyhow::Result<Option<u64>> {
        // Readonly keeps local edits untouched, including those found during startup scans.
        if self.readonly {
            return Ok(None);
        }
        // On the tested Windows build, appends can leave InSyncState set while
        // ModifiedDataSize is nonzero. Either signal must trigger an upload.
        if open_placeholder(path)?
            .info()?
            .is_some_and(|info| info.is_in_sync() && info.modified_data_size() == 0)
        {
            return Ok(None);
        }
        // Refuse an active writer and prevent new writers/renames while copying.
        // Keep this handle until the uploaded version is marked in sync.
        let mut source = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(path)?;
        if self.rename_pending(path) {
            return Ok(None);
        }
        let mut placeholder = open_placeholder(path)?;
        let info = placeholder.info()?;
        if info
            .as_ref()
            .is_some_and(|info| info.is_in_sync() && info.modified_data_size() == 0)
        {
            return Ok(None);
        }
        let destination = self.upload_destination(path)?;
        if info.is_none() {
            placeholder.convert_to_placeholder(
                ConvertOptions::default().blob(destination.as_os_str().as_encoded_bytes().to_vec()),
                None,
            )?;
        }
        let usn = source.usn()?;
        ensure!(usn != 0, "Windows did not return a file USN");

        let (staging, mut output) =
            StagingFile::new(destination.parent().context("server directory")?)?;
        // A new file (or one moved from another identity) has no known server base.
        let can_patch = info
            .as_ref()
            .is_some_and(|info| info.blob() == destination.as_os_str().as_encoded_bytes());
        let uploaded = copy_upload(
            &mut source,
            &placeholder,
            &destination,
            &mut output,
            can_patch,
        )?;
        output.sync_all()?;
        drop(output);
        let current_usn = source.usn()?;
        ensure!(
            current_usn == usn,
            "file changed during upload: {usn} -> {current_usn}"
        );
        staging
            .publish(&destination)
            .context("publish uploaded file")?;
        // On the tested Windows 10 build 19044, CfSetInSyncState with a nonzero USN fails
        // with ERROR_CLOUD_FILE_NOT_IN_SYNC despite matching immediately beforehand.
        // Retain the write/delete-denying source handle through this call instead;
        // do not use this no-USN form after releasing that handle.
        placeholder.mark_in_sync(true, None)?;
        println!(
            "uploaded {} ({uploaded} client bytes); marked in sync",
            path.strip_prefix(&self.client_path)?.display(),
        );
        Ok(Some(uploaded))
    }

    fn upload_destination(&self, path: &Path) -> anyhow::Result<PathBuf> {
        let relative = path.strip_prefix(&self.client_path)?;
        ensure!(valid_relative_path(relative), "invalid upload path");
        ensure!(
            !relative
                .file_name()
                .context("file name")?
                .to_string_lossy()
                .starts_with(STAGING_PREFIX),
            "reserved staging filename"
        );
        let parent = self.create_server_directory(relative.parent().context("relative parent")?)?;
        let destination = parent.join(relative.file_name().context("file name")?);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => ensure!(
                metadata.is_file()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "server destination must be an ordinary file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(destination)
    }

    fn create_server_directory(&self, relative: &Path) -> anyhow::Result<PathBuf> {
        let mut directory = self.server_path.clone();
        for component in relative.components() {
            ensure!(
                !component
                    .as_os_str()
                    .to_string_lossy()
                    .starts_with(STAGING_PREFIX),
                "reserved staging filename"
            );
            directory.push(component);
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let metadata = fs::symlink_metadata(&directory)?;
            ensure!(
                metadata.is_dir()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
                "server path must be an ordinary directory"
            );
        }
        Ok(directory)
    }
}

fn copy_upload(
    source: &mut File,
    placeholder: &Placeholder,
    destination: &Path,
    output: &mut File,
    can_patch: bool,
) -> anyhow::Result<u64> {
    let size = source.metadata()?.len();
    let base = if can_patch {
        match fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(destination)
        {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let Some(base) = base else {
        // New local files and missing server files need every byte.
        let reason = if can_patch {
            "server base missing"
        } else {
            "new file or identity"
        };
        println!("uploading {destination:?}: full file, {size} bytes ({reason})");
        let copied = std::io::copy(source, output)?;
        ensure!(copied == size, "short full-file upload");
        return Ok(copied);
    };

    let base_size = base.metadata()?.len();
    let mut ranges = placeholder.ranges(ReadType::Modified, ..size)?;
    println!(
        "uploading {destination:?}: {} modified ranges, size {base_size} -> {size}",
        ranges.len(),
    );
    // This is a local stand-in for copying an existing object on the server.
    // Only the patches below read client data, but staging still costs server I/O.
    let copied = std::io::copy(&mut (&base).take(size), output)?;
    ensure!(copied == base_size.min(size), "short server-base copy");
    // Truncation is a size change, not a modified range. Extensions can contain
    // zero-filled gaps, so copy the entire new tail even if CF omits those gaps.
    output.set_len(size)?;
    if base_size < size {
        ranges.push(base_size..size);
        ranges.sort_unstable_by_key(|range| range.start);
    }
    let mut uploaded = 0;
    let mut copied_until = 0;
    for range in ranges {
        let start = range.start.max(copied_until);
        if start >= range.end {
            continue;
        }
        source.seek(SeekFrom::Start(start))?;
        output.seek(SeekFrom::Start(start))?;
        let length = range.end - start;
        let copied = std::io::copy(&mut (&mut *source).take(length), output)?;
        ensure!(copied == length, "short modified-range upload");
        uploaded += copied;
        copied_until = range.end;
    }
    Ok(uploaded)
}

// Only remove the temporary file this instance successfully created.
struct StagingFile(Option<PathBuf>);

impl StagingFile {
    fn new(parent: &Path) -> std::io::Result<(Self, File)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = parent.join(format!(
                "{STAGING_PREFIX}{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((Self(Some(path)), file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn publish(mut self, destination: &Path) -> std::io::Result<()> {
        fs::rename(self.0.as_ref().expect("owned staging file"), destination)?;
        // The temporary name no longer belongs to us after publication.
        self.0.take();
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

impl MirrorFiles {
    pub(super) fn prepare_delete(&self, request: &Request, info: &info::Delete) -> CResult<()> {
        if info.is_undelete() {
            return Ok(());
        }
        if self.readonly {
            return Err(CloudErrorKind::AccessDenied);
        }
        let source = request.path();
        self.check_client_path(&source)?;
        if self.rename_pending(&source) {
            return Err(CloudErrorKind::InUse);
        }
        if info.is_directory() {
            // Validate the ID now: NTFS reports error 87 for a deleted ID later.
            open_by_id(&self.client_path, request.file_id(), true)
                .map_err(|_| CloudErrorKind::Unsuccessful)?;
        }
        Ok(())
    }

    pub(super) fn prepare_rename(&self, request: &Request, info: &info::Rename) -> CResult<()> {
        if self.readonly {
            return Err(CloudErrorKind::AccessDenied);
        }
        let source = request.path();
        let target = info.target_path();
        self.check_client_path(&source)?;
        if self.rename_pending(&source) || self.rename_pending(&target) {
            return Err(CloudErrorKind::InUse);
        }
        match (info.source_in_scope(), info.target_in_scope()) {
            (true, true) => self.check_client_path(&target),
            (true, false) => Ok(()), // Moving out retains the server copy.
            (false, true) => Err(CloudErrorKind::NotSupported),
            (false, false) => Err(CloudErrorKind::InvalidRequest),
        }
    }

    pub(super) fn check_client_path(&self, path: &Path) -> CResult<()> {
        if path
            .strip_prefix(&self.client_path)
            .is_ok_and(valid_relative_path)
        {
            Ok(())
        } else {
            Err(CloudErrorKind::InvalidRequest)
        }
    }

    fn rename_pending(&self, path: &Path) -> bool {
        self.renames.lock().unwrap().iter().any(|(target, source)| {
            [source, target]
                .into_iter()
                .any(|entry| path.starts_with(entry) || entry.starts_with(path))
        })
    }

    fn apply_change(&self, change: &Work) -> anyhow::Result<bool> {
        match change {
            Work::DeleteFile(path) => {
                self.remove_source(path, false)?;
                println!("deleted remote {}", path.display());
            }
            Work::DeleteDirectory { path, id } => {
                // deleted can precede undelete, and directories have no closed
                // callback. Wait for DELETE handles, then check the original ID.
                match open_by_id(&self.client_path, *id, false) {
                    Ok(_) => return Ok(true), // Still exists: keep the remote directory.
                    Err(error) if error.raw_os_error() == Some(32) => return Ok(false),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            || error.raw_os_error() == Some(87) => {}
                    Err(error) => return Err(error.into()),
                }
                self.remove_source(path, true)?;
                println!("deleted remote directory {}", path.display());
            }
            Work::Rename { source, target } => {
                let mut renames = self.renames.lock().unwrap();
                self.rename_source(source, target)?;
                renames.remove(target);
                println!(
                    "renamed remote {} -> {}",
                    source.display(),
                    target.display()
                );
            }
            Work::Sync(_) => unreachable!("path sync has its own queue"),
        }
        Ok(true)
    }
}

fn open_by_id(root: &Path, id: i64, share_delete: bool) -> std::io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::Storage::FileSystem::*;
    let volume = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(root)?;
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id },
    };
    let share = FILE_SHARE_READ
        | FILE_SHARE_WRITE
        | if share_delete {
            FILE_SHARE_DELETE
        } else {
            FILE_SHARE_MODE(0)
        };
    let handle = unsafe {
        OpenFileById(
            HANDLE(volume.as_raw_handle()),
            &descriptor,
            0,
            share,
            None,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    }
    .map_err(|error| std::io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

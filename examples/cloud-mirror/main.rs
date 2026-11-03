use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::windows::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf, Prefix},
    sync::{mpsc, Arc, Mutex},
};

use anyhow::{ensure, Context};
use clap::{Parser, Subcommand};
use cloud_filter::{
    error::{CResult, CloudErrorKind},
    ext::FileExt,
    filter::{info, ticket, Request, StateWatchEvent, SyncFilter},
    metadata::Metadata,
    placeholder::{ConvertOptions, PinState, Placeholder},
    placeholder_create_info::PlaceholderCreateInfo,
    root::{
        HydrationPolicy, HydrationType, PopulationType, SecurityId, Session, SyncRootId,
        SyncRootIdBuilder, SyncRootInfo,
    },
    utility::WriteAt,
};
use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

// MUST be a multiple of 4096
const CHUNK_SIZE_BYTES: usize = 65536;

mod writeback;
use writeback::{SyncWorker, Work};

const PROVIDER_NAME: &str = "CloudMirrorProvider";
const DISPLAY_NAME: &str = "Cloud Mirror";
const VERSION: &str = "1.0.0";

#[derive(Parser)]
#[command(
    name = "cloud-mirror",
    about = "Mirror a server directory through Cloud Files",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(required = true, value_name = "SERVER_PATH")]
    server_path: Option<PathBuf>,
    #[arg(required = true, value_name = "CLIENT_PATH")]
    client_path: Option<PathBuf>,
    /// Keep the server unchanged; retain local edits and deny placeholder deletion/renaming
    #[arg(long)]
    readonly: bool,
    /// Unregister after graceful shutdown; removes dehydrated placeholders
    #[arg(long)]
    unregister_on_exit: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Remove this client's registration; removes dehydrated placeholders
    Unregister { client_path: PathBuf },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let sync_root_id = SyncRootIdBuilder::new(PROVIDER_NAME)
        .user_security_id(SecurityId::current_user()?)
        .build();
    if let Some(Command::Unregister { client_path }) = args.command {
        ensure!(
            canonical_path(&sync_root_id.info()?.path())? == canonical_path(&client_path)?,
            "CLIENT_PATH does not match this example's registered root"
        );
        sync_root_id.unregister()?;
        println!("Cloud Mirror unregistered.");
        return Ok(());
    }
    let server_path = canonical_path(&args.server_path.expect("required by clap"))?;
    let client_path = canonical_path(&args.client_path.expect("required by clap"))?;
    ensure!(
        server_path.is_dir() && client_path.is_dir(),
        "SERVER_PATH and CLIENT_PATH must be directories"
    );
    ensure!(
        !server_path.starts_with(&client_path) && !client_path.starts_with(&server_path),
        "SERVER_PATH and CLIENT_PATH must not overlap"
    );

    register_root(
        &sync_root_id,
        &client_path,
        server_path.as_os_str().as_encoded_bytes(),
    )?;

    let result = Session::new()
        .connect(
            &client_path,
            CloudMirror::new(client_path.clone(), server_path, args.readonly),
        )
        .context("connect sync root")
        .and_then(|connection| {
            // Include edits made while disconnected, after the watcher is attached.
            connection
                .filter()
                .state_changed(StateWatchEvent::Changed(vec![client_path.clone()]));
            println!(
                "Cloud Mirror connected at {}. Press Ctrl-C to disconnect.",
                client_path.display()
            );
            let result = wait_for_ctrlc();
            drop(connection);
            result
        });
    // Disconnect before unregistering, including when connecting or waiting fails.
    result.and(if args.unregister_on_exit {
        sync_root_id.unregister().context("unregister on exit")
    } else {
        Ok(())
    })
}

fn register_root(
    sync_root_id: &SyncRootId,
    client_path: &Path,
    source: &[u8],
) -> anyhow::Result<()> {
    if sync_root_id.is_registered()? {
        let info = sync_root_id.info()?;
        ensure!(
            canonical_path(&info.path())? == client_path,
            "CLIENT_PATH does not match this example's registered root"
        );
        let registered_source = info.blob();
        ensure!(
            registered_source.is_empty() || registered_source == source,
            "SERVER_PATH differs from the source used to register this CLIENT_PATH"
        );
        // Only migrate an older registration. Re-registering on every start can
        // reset the root's population state and request its entries again.
        if !info.allow_pinning()
            || info.hydration_policy() != HydrationPolicy::AutoDehydrationAllowed
            || registered_source.is_empty()
        {
            sync_root_id.register(
                info.with_allow_pinning(true)
                    .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
                    .with_blob(source),
            )?;
        }
    } else {
        ensure!(
            fs::read_dir(client_path)?.next().is_none(),
            "use an empty CLIENT_PATH for the first registration"
        );
        sync_root_id.register(root_info(client_path)?.with_blob(source))?;
    }
    Ok(())
}

fn root_info(path: &Path) -> windows::core::Result<SyncRootInfo> {
    SyncRootInfo::default()
        .with_display_name(DISPLAY_NAME)
        .with_hydration_type(HydrationType::Full)
        .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
        .with_allow_pinning(true)
        .with_population_type(PopulationType::Full)
        .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
        .with_version(VERSION)
        .with_path(path)
}

fn valid_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn open_placeholder(path: &Path) -> std::io::Result<Placeholder> {
    // A data-access open can hydrate the file before we even query its state.
    // Attribute/no-access handles are sufficient for info, hydrate and pin operations.
    fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .map(Placeholder::from)
}

fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    // Callbacks use DOS/UNC paths, so keep canonical paths in the same form.
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Ok(path);
    };
    let mut normalized = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => PathBuf::from(r"\\").join(server).join(share),
        _ => return Ok(path),
    };
    normalized.extend(path.components().skip(2));
    Ok(normalized)
}

#[derive(Debug, Clone)]
struct MirrorFiles {
    /// Destination folder
    client_path: PathBuf,
    /// Source folder
    server_path: PathBuf,
    readonly: bool,
    renames: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
}

#[derive(Debug)]
struct CloudMirror {
    files: MirrorFiles,
    worker: SyncWorker,
    deleting_directories: Mutex<std::collections::HashSet<i64>>,
}

impl CloudMirror {
    fn new(client_path: PathBuf, server_path: PathBuf, readonly: bool) -> Self {
        let files = MirrorFiles {
            client_path,
            server_path,
            readonly,
            renames: Arc::default(),
        };
        let worker = SyncWorker::new(files.clone());
        Self {
            files,
            worker,
            deleting_directories: Mutex::default(),
        }
    }
}

impl SyncFilter for CloudMirror {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let range = info.required_file_range();
        println!(
            "fetch_data {:?} {range:?}, explicit={}",
            request.path(),
            info.explicit_hydration()
        );
        let mut server_file = self.files.with_path(&request.path(), |path| {
            File::open(path).map_err(cloud_io_error)
        })?;
        // A changed/truncated source must fail the request, not spin on zero-byte reads.
        if server_file.metadata().map_err(cloud_io_error)?.len() != request.file_size() {
            return Err(CloudErrorKind::NotInSync);
        }
        server_file
            .seek(SeekFrom::Start(range.start))
            .map_err(cloud_io_error)?;
        let mut position = range.start;
        let mut buffer = [0; CHUNK_SIZE_BYTES];
        while position < range.end {
            let length = (range.end - position).min(buffer.len() as u64) as usize;
            server_file
                .read_exact(&mut buffer[..length])
                .map_err(cloud_io_error)?;
            ticket
                .write_at(&buffer[..length], position)
                .map_err(|error| {
                    eprintln!("transfer_data offset={position}, length={length}");
                    cloud_api_error(error)
                })?;
            position += length as u64;
            // Progress reporting is advisory; the final transfer may have completed the request.
            let _ = ticket.report_progress(range.end - range.start, position - range.start);
        }

        Ok(())
    }

    fn cancel_fetch_data(&self, _request: Request, _info: info::CancelFetchData) {
        println!("cancel fetch data");
    }

    fn validate_data(
        &self,
        _request: Request,
        _ticket: ticket::ValidateData,
        _info: info::ValidateData,
    ) -> CResult<()> {
        println!("validate data");
        Err(CloudErrorKind::NotSupported)
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        info: info::FetchPlaceholders,
    ) -> CResult<()> {
        println!(
            "fetch_placeholders {:?} {:?}",
            request.path(),
            info.pattern()
        );
        let entries = self.files.with_path(&request.path(), |path| {
            fs::read_dir(path)
                .map_err(cloud_io_error)?
                .map(|entry| {
                    let entry = entry.map_err(cloud_io_error)?;
                    let stat = fs::symlink_metadata(entry.path()).map_err(cloud_io_error)?;
                    Ok((entry, stat))
                })
                .collect::<CResult<Vec<_>>>()
        })?;
        let mut placeholders = Vec::new();
        for (entry, stat) in entries {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(writeback::STAGING_PREFIX)
            {
                continue;
            }
            // This example mirrors ordinary files/directories, not symlinks or junctions.
            if std::os::windows::fs::MetadataExt::file_attributes(&stat) & 0x400 != 0 {
                continue;
            }
            if request
                .path()
                .join(entry.file_name())
                .try_exists()
                .map_err(cloud_io_error)?
            {
                continue;
            }
            placeholders.push(
                // TRANSFER_PLACEHOLDERS names are relative to the requested directory.
                PlaceholderCreateInfo::new(entry.file_name())
                    .metadata(Metadata::from(stat))
                    .mark_in_sync()
                    .blob(entry.path().into_os_string().into_encoded_bytes()),
            );
        }
        ticket
            .pass_with_placeholders(&mut placeholders)
            .map_err(cloud_api_error)?;
        for placeholder in placeholders {
            placeholder.result().map_err(cloud_api_error)?;
        }

        Ok(())
    }

    fn cancel_fetch_placeholders(&self, _request: Request, _info: info::CancelFetchPlaceholders) {
        println!("cancel fetch placeholders");
    }

    fn opened(&self, request: Request, _info: info::Opened) {
        println!("file opened {:?}", request.path());
    }

    fn closed(&self, request: Request, info: info::Closed) {
        self.worker.enqueue(if info.deleted() {
            Work::DeleteFile(request.path())
        } else {
            Work::Sync(request.path())
        });
    }

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        info: info::Dehydrate,
    ) -> CResult<()> {
        println!("dehydrate {:?}, reason={:?}", request.path(), info.reason());
        let size = self.files.with_path(&request.path(), |path| {
            fs::metadata(path)
                .map(|stat| stat.len())
                .map_err(cloud_io_error)
        })?;
        if size != request.file_size() {
            return Err(CloudErrorKind::NotInSync);
        }
        // Windows checks the in-sync and pin states. Never mark local edits in sync here.
        ticket.pass().map_err(cloud_api_error)
    }

    fn dehydrated(&self, _request: Request, _info: info::Dehydrated) {
        println!("dehydrated");
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        self.files.prepare_delete(&request, &info)?;
        if info.is_directory() {
            let mut directories = self.deleting_directories.lock().unwrap();
            if info.is_undelete() {
                directories.remove(&request.file_id());
            } else {
                directories.insert(request.file_id());
            }
        }
        let result = ticket.pass().map_err(cloud_api_error);
        if result.is_err() {
            self.deleting_directories
                .lock()
                .unwrap()
                .remove(&request.file_id());
        }
        result
    }

    fn deleted(&self, request: Request, _info: info::Deleted) {
        // Only directories need this notification; files wait for closed(deleted).
        if self
            .deleting_directories
            .lock()
            .unwrap()
            .remove(&request.file_id())
        {
            self.worker.enqueue(Work::DeleteDirectory {
                path: request.path(),
                id: request.file_id(),
            });
        }
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        self.files.prepare_rename(&request, &info)?;
        ticket.pass().map_err(cloud_api_error)
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        let source = info.source_path();
        let target = request.path();
        if self.files.check_client_path(&source).is_err()
            || self.files.check_client_path(&target).is_err()
        {
            return; // A move out of the root retains the server copy.
        }
        self.files
            .renames
            .lock()
            .unwrap()
            .insert(target.clone(), source.clone());
        self.worker.enqueue(Work::Rename { source, target });
    }

    fn state_changed(&self, event: StateWatchEvent) {
        match event {
            StateWatchEvent::Changed(mut paths) => {
                let mut seen = std::collections::HashSet::new();
                while let Some(path) = paths.pop() {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    let result = fs::symlink_metadata(&path).and_then(|metadata| {
                        if metadata.file_type().is_symlink()
                            || path.file_name().is_some_and(|name| {
                                name.to_string_lossy()
                                    .starts_with(writeback::STAGING_PREFIX)
                            })
                        {
                            return Ok(());
                        }
                        self.worker.enqueue(Work::Sync(path.clone()));
                        // Enumerate locally, enqueue as we go; no remote I/O here.
                        if metadata.is_dir() {
                            for entry in fs::read_dir(&path)? {
                                paths.push(entry?.path());
                            }
                        }
                        Ok(())
                    });
                    if let Err(error) = result {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            eprintln!("scan {path:?}: {error}");
                        }
                    }
                }
            }
            StateWatchEvent::Stopped(error) => {
                eprintln!("directory watcher stopped: {error}; restart to resume change detection");
            }
        }
    }
}

impl MirrorFiles {
    fn apply_pin_state(&self, path: &Path) -> anyhow::Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut placeholder = open_placeholder(path)?;
        let Some(info) = placeholder.info()? else {
            return Ok(());
        };
        match info.pin_state() {
            PinState::Pinned if info.on_disk_data_size() < metadata.len() as i64 => {
                placeholder.hydrate(..).context("hydrate pinned file")?;
                println!("hydrated {path:?} (pinned)");
            }
            PinState::Unpinned
                if info.on_disk_data_size() > 0
                    && info.is_in_sync()
                    && info.modified_data_size() == 0 =>
            {
                // Provider-initiated dehydration does not call our dehydrate callback.
                // Check the source here too, and release the query handle before
                // acquiring exclusive access. Windows rechecks in-sync atomically.
                let size = self
                    .with_path(path, |source| {
                        fs::metadata(source)
                            .map(|stat| stat.len())
                            .map_err(cloud_io_error)
                    })
                    .map_err(|error| anyhow::anyhow!("source: {error:?}"))?;
                ensure!(size == metadata.len(), "source size changed");
                drop(placeholder);
                let file = fs::OpenOptions::new()
                    .access_mode(0)
                    .share_mode(0)
                    .open(path)?;
                file.dehydrate(..).context("dehydrate unpinned file")?;
                println!("dehydrated {path:?} (unpinned)");
            }
            _ => {}
        }
        Ok(())
    }

    fn remove_source(&self, client_path: &Path, directory: bool) -> anyhow::Result<()> {
        let relative = client_path.strip_prefix(&self.client_path)?;
        ensure!(valid_relative_path(relative), "invalid deletion path");
        let result = (|| {
            let source = canonical_path(&self.server_path.join(relative))?;
            if source == self.server_path || !source.starts_with(&self.server_path) {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            if directory {
                fs::remove_dir_all(source)
            } else {
                fs::remove_file(source)
            }
        })();
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }

    fn rename_source(&self, source: &Path, target: &Path) -> anyhow::Result<()> {
        let source = canonical_path(
            &self
                .server_path
                .join(source.strip_prefix(&self.client_path)?),
        )?;
        ensure!(
            source != self.server_path && source.starts_with(&self.server_path),
            "source outside SERVER"
        );
        let relative = target.strip_prefix(&self.client_path)?;
        ensure!(valid_relative_path(relative), "invalid rename target");
        let parent = canonical_path(&self.server_path.join(relative.parent().unwrap()))?;
        ensure!(
            parent.starts_with(&self.server_path),
            "target outside SERVER"
        );
        fs::rename(source, parent.join(relative.file_name().unwrap()))?;
        Ok(())
    }

    // Hold the mapping until the remote handle/listing has been obtained, so
    // a successful remote rename cannot invalidate a path between lookup and open.
    fn with_path<T>(&self, path: &Path, read: impl FnOnce(&Path) -> CResult<T>) -> CResult<T> {
        let renames = self.renames.lock().unwrap();
        // Look up ancestors too: renaming a directory relocates its children.
        let source = path
            .ancestors()
            .find_map(|target| {
                let old = renames.get(target)?;
                path.strip_prefix(target)
                    .ok()
                    .map(|relative| old.join(relative))
            })
            .unwrap_or_else(|| path.to_owned());
        let relative = source
            .strip_prefix(&self.client_path)
            .map_err(|_| CloudErrorKind::InvalidRequest)?;
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(CloudErrorKind::InvalidRequest);
        }
        let source = canonical_path(&self.server_path.join(relative)).map_err(cloud_io_error)?;
        if !source.starts_with(&self.server_path) {
            return Err(CloudErrorKind::AccessDenied);
        }
        read(&source)
    }
}

fn cloud_io_error(error: std::io::Error) -> CloudErrorKind {
    eprintln!("source I/O: {error}");
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => CloudErrorKind::AccessDenied,
        std::io::ErrorKind::UnexpectedEof => CloudErrorKind::NotInSync,
        _ => CloudErrorKind::Unsuccessful,
    }
}

fn cloud_api_error(error: windows::core::Error) -> CloudErrorKind {
    eprintln!("Cloud Filter API: {error}");
    CloudErrorKind::Unsuccessful
}

fn wait_for_ctrlc() -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })?;
    rx.recv().context("wait for Ctrl-C")
}

#[cfg(test)]
mod tests;

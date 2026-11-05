use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    net::TcpStream,
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
    utility::{FileTime, WriteAt},
};
use ssh2::Sftp;
use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

mod writeback;
use writeback::{SyncWorker, Work};

// CF transfers use 4 KiB-aligned chunks, except at the end of the file.
const CHUNK_SIZE_BYTES: usize = 65536;
const PROVIDER_NAME: &str = "wincs";
const DISPLAY_NAME: &str = "Sftp";
const VERSION: &str = "1.0.0";

#[derive(Parser)]
#[command(
    name = "sftp",
    about = "Browse an SFTP home directory through Cloud Files",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// SSH endpoint, including port (for example, localhost:22)
    #[arg(required = true, value_name = "SERVER")]
    server: Option<String>,
    #[arg(required = true, value_name = "CLIENT_PATH")]
    client_path: Option<PathBuf>,
    /// SSH username; defaults to USERNAME. The password comes from PASSWORD.
    #[arg(long)]
    username: Option<String>,
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
        println!("SFTP unregistered.");
        return Ok(());
    }

    let client_path = canonical_path(&args.client_path.expect("required by clap"))?;
    ensure!(client_path.is_dir(), "CLIENT_PATH must be a directory");
    let server = args.server.expect("required by clap");
    let username = args
        .username
        .or_else(|| env::var("USERNAME").ok())
        .context("set --username or USERNAME")?;
    let password = env::var("PASSWORD").context("set PASSWORD for SSH authentication")?;
    let sftp = connect(&server, &username, &password)?;
    let remote_path = sftp
        .realpath(Path::new("."))
        .context("resolve SFTP home directory")?;
    let source = format!("{server}\0{username}\0{}", remote_path.display());
    register_root(&sync_root_id, &client_path, source.as_bytes())?;

    let result = Session::new()
        .connect(
            &client_path,
            Filter::new(
                Arc::new(sftp),
                client_path.clone(),
                remote_path,
                args.readonly,
            ),
        )
        .context("connect sync root")
        .and_then(|connection| {
            // Include edits made while disconnected, after the watcher is attached.
            connection
                .filter()
                .state_changed(StateWatchEvent::Changed(vec![client_path.clone()]));
            println!(
                "SFTP connected at {}. Press Ctrl-C to disconnect.",
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
            "SERVER, USERNAME or remote directory differs from this client's registered source"
        );
        // Migrate older registrations once, then reuse them on restart.
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

fn connect(server: &str, username: &str, password: &str) -> anyhow::Result<Sftp> {
    let mut session = ssh2::Session::new()?;
    session.set_blocking(true);
    session.set_timeout(10_000);
    let socket = TcpStream::connect(server).context("connect SSH server")?;
    socket.set_nodelay(true)?;
    session.set_tcp_stream(socket);
    session.handshake().context("SSH handshake")?;
    session
        .userauth_password(username, password)
        .context("SSH authentication")?;
    session.sftp().context("open SFTP session")
}

#[derive(Clone)]
struct MirrorFiles {
    sftp: Arc<Sftp>,
    client_path: PathBuf,
    server_path: PathBuf,
    readonly: bool,
    renames: Arc<Mutex<HashMap<PathBuf, PathBuf>>>,
}

struct Filter {
    files: MirrorFiles,
    worker: SyncWorker,
    deleting_directories: Mutex<std::collections::HashSet<i64>>,
}

impl Filter {
    fn new(sftp: Arc<Sftp>, client_path: PathBuf, server_path: PathBuf, readonly: bool) -> Self {
        let files = MirrorFiles {
            sftp,
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

impl MirrorFiles {
    // Resolve from the current client name so a rename cannot leave a stale identity path.
    fn remote_path(&self, path: &Path) -> CResult<PathBuf> {
        let relative = path
            .strip_prefix(&self.client_path)
            .map_err(|_| CloudErrorKind::InvalidRequest)?;
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(CloudErrorKind::InvalidRequest);
        }
        Ok(self.server_path.join(relative))
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
        read(&self.remote_path(&source)?)
    }

    fn remove_source(&self, client: &Path, directory: bool) -> anyhow::Result<()> {
        let source = self
            .remote_path(client)
            .map_err(|e| anyhow::anyhow!("path: {e:?}"))?;
        ensure!(source != self.server_path, "cannot delete server root");
        self.remove_remote(&source, directory)?;
        Ok(())
    }

    fn rename_source(&self, source: &Path, target: &Path) -> anyhow::Result<()> {
        let source = self
            .remote_path(source)
            .map_err(|e| anyhow::anyhow!("path: {e:?}"))?;
        let target = self
            .remote_path(target)
            .map_err(|e| anyhow::anyhow!("path: {e:?}"))?;
        self.sftp.rename(&source, &target, None)?;
        Ok(())
    }

    fn remove_remote(&self, path: &Path, directory: bool) -> Result<(), ssh2::Error> {
        if directory {
            let entries = match self.sftp.readdir(path) {
                Ok(entries) => entries,
                Err(error) if missing(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            for (entry, stat) in entries {
                self.remove_remote(&entry, stat.is_dir())?;
            }
        }
        let result = if directory {
            self.sftp.rmdir(path)
        } else {
            self.sftp.unlink(path)
        };
        // Parent/child notifications and retries may name an already removed entry.
        match result {
            Err(error) if missing(&error) => Ok(()),
            result => result,
        }
    }
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let range = info.required_file_range();
        println!("fetch_data {:?} {range:?}", request.path());
        let mut file = self.files.with_path(&request.path(), |path| {
            self.files.sftp.open(path).map_err(cloud_error)
        })?;
        if file.stat().map_err(cloud_error)?.size != Some(request.file_size()) {
            return Err(CloudErrorKind::NotInSync);
        }
        file.seek(SeekFrom::Start(range.start))
            .map_err(cloud_error)?;
        let mut position = range.start;
        let mut buffer = [0; CHUNK_SIZE_BYTES];
        while position < range.end {
            let length = (range.end - position).min(buffer.len() as u64) as usize;
            // SFTP reads may be short. Fill a chunk, or fail on EOF instead of retrying forever.
            file.read_exact(&mut buffer[..length])
                .map_err(cloud_error)?;
            ticket
                .write_at(&buffer[..length], position)
                .map_err(|error| {
                    eprintln!("transfer_data offset={position}, length={length}");
                    cloud_error(error)
                })?;
            position += length as u64;
            let _ = ticket.report_progress(range.end - range.start, position - range.start);
        }
        Ok(())
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        let client = request.path();
        let parent = self.files.remote_path(&client)?;
        println!("fetch_placeholders {parent:?}");
        let mut placeholders = Vec::new();
        for (path, stat) in self.files.with_path(&client, |path| {
            let entries = self.files.sftp.readdir(path).map_err(cloud_error)?;
            if entries
                .iter()
                .any(|(entry, _)| entry.parent() != Some(path))
            {
                return Err(CloudErrorKind::InvalidRequest);
            }
            Ok(entries)
        })? {
            // Only ordinary files and directories can be represented by this example.
            if !stat.is_file() && !stat.is_dir() {
                continue;
            }
            let name = path.file_name().ok_or(CloudErrorKind::InvalidRequest)?;
            if name
                .to_string_lossy()
                .starts_with(writeback::STAGING_PREFIX)
            {
                continue;
            }
            if client.join(name).try_exists().map_err(cloud_error)? {
                continue;
            }
            let metadata = if stat.is_dir() {
                Metadata::directory()
            } else {
                Metadata::file().size(stat.size.unwrap_or_default())
            };
            placeholders.push(
                PlaceholderCreateInfo::new(name)
                    .metadata(
                        metadata
                            .written(
                                stat.mtime
                                    .and_then(|time| FileTime::from_unix_time(time as _, 0).ok())
                                    .unwrap_or_default(),
                            )
                            .accessed(
                                stat.atime
                                    .and_then(|time| FileTime::from_unix_time(time as _, 0).ok())
                                    .unwrap_or_default(),
                            ),
                    )
                    .mark_in_sync()
                    .blob(path.into_os_string().into_encoded_bytes()),
            );
        }
        ticket
            .pass_with_placeholders(&mut placeholders)
            .map_err(cloud_error)?;
        for placeholder in placeholders {
            placeholder.result().map_err(cloud_error)?;
        }
        Ok(())
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
        let result = ticket.pass().map_err(cloud_error);
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
        ticket.pass().map_err(cloud_error)
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

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        let size = self.files.with_path(&request.path(), |path| {
            self.files
                .sftp
                .stat(path)
                .map(|stat| stat.size)
                .map_err(cloud_error)
        })?;
        if size != Some(request.file_size()) {
            return Err(CloudErrorKind::NotInSync);
        }
        ticket.pass().map_err(cloud_error)
    }

    fn closed(&self, request: Request, info: info::Closed) {
        self.worker.enqueue(if info.deleted() {
            Work::DeleteFile(request.path())
        } else {
            Work::Sync(request.path())
        });
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

fn open_placeholder(path: &Path) -> std::io::Result<Placeholder> {
    fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .map(Placeholder::from)
}

fn valid_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn missing(error: &ssh2::Error) -> bool {
    matches!(error.code(), ssh2::ErrorCode::SFTP(2 | 10))
}

fn cloud_error(error: impl std::fmt::Display) -> CloudErrorKind {
    eprintln!("SFTP: {error}");
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

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc,
    time::UNIX_EPOCH,
};

use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, SyncFilter},
    metadata::Metadata,
    placeholder::{ConvertOptions, Placeholder},
    placeholder_file::PlaceholderFile,
    root::{HydrationType, PopulationType, SecurityId, Session, SyncRootIdBuilder, SyncRootInfo},
    utility::{FileTime, WriteAt},
};

// MUST be a multiple of 4096
const CHUNK_SIZE_BYTES: usize = 65536;

const PROVIDER_NAME: &str = "CloudMirrorProvider";
const DISPLAY_NAME: &str = "Cloud Mirror";
const VERSION: &str = "1.0.0";

fn main() {
    let server_path = PathBuf::from(env::var("SERVER").expect("SERVER env var"));
    let client_path = PathBuf::from(env::var("CLIENT").expect("CLIENT env var"));

    let sync_root_id = SyncRootIdBuilder::new(PROVIDER_NAME)
        .user_security_id(SecurityId::current_user().unwrap())
        .build();
    // register the sync root if it isn't already registered
    if !sync_root_id.is_registered().unwrap() {
        sync_root_id
            .register(
                SyncRootInfo::default()
                    .with_display_name(DISPLAY_NAME)
                    .with_hydration_type(HydrationType::Full)
                    .with_population_type(PopulationType::Full)
                    .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                    .with_version(VERSION)
                    .with_recycle_bin_uri("http://cloudmirror.example.com/recyclebin")
                    .unwrap()
                    .with_path(Path::new(&client_path))
                    .unwrap(),
            )
            .unwrap()
    }

    mark_in_sync(&client_path, &client_path, &server_path);

    let connection = Session::new()
        .connect(
            &client_path,
            CloudMirror {
                client_path: client_path.clone(),
                server_path,
            },
        )
        .unwrap();

    wait_for_ctrlc();

    drop(connection);
    sync_root_id.unregister().unwrap();
}

#[derive(Debug)]
struct CloudMirror {
    /// Destination folder
    client_path: PathBuf,
    /// Source folder
    server_path: PathBuf,
}

impl SyncFilter for CloudMirror {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        // Server path stored in fetch_placeholders
        let path = Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(request.file_blob()) });

        let range = info.required_file_range();
        let end = range.end;
        let mut position = range.start;

        println!(
            "fetch_data {:?} {:?} {}",
            path,
            range,
            info.interrupted_hydration()
        );
        let mut server_file = File::open(path).map_err(|_| CloudErrorKind::InvalidRequest)?;
        server_file
            .seek(SeekFrom::Start(position))
            .map_err(|_| CloudErrorKind::InvalidRequest)?;

        let mut buffer = [0; CHUNK_SIZE_BYTES];
        loop {
            let mut bytes_read = server_file
                .read(&mut buffer)
                .map_err(|_| CloudErrorKind::InvalidRequest)?;

            let unaligned = bytes_read % 4096;
            if unaligned != 0 && position + (bytes_read as u64) < end {
                bytes_read -= unaligned;
                server_file
                    .seek(SeekFrom::Current(-(unaligned as i64)))
                    .unwrap();
            }
            ticket
                .write_at(&buffer[0..bytes_read], position)
                .map_err(|_| CloudErrorKind::InvalidRequest)?;
            position += bytes_read as u64;

            if position >= end {
                break;
            }

            ticket.report_progress(end, position).unwrap();
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
        let absolute = request.path();
        let path = absolute.strip_prefix(&self.client_path).unwrap();

        let dirs = fs::read_dir(&self.server_path.join(path)).unwrap();
        let mut placeholders = dirs
            .into_iter()
            .filter_map(|entry| {
                entry
                    .and_then(|e| {
                        Ok((
                            e.path()
                                .strip_prefix(&self.server_path)
                                .unwrap()
                                .to_path_buf(),
                            e.metadata()?,
                        ))
                    })
                    .ok()
            })
            // Only create placeholders that don't exist on client path
            .filter(|(relative_path, _)| !self.client_path.join(relative_path).exists())
            .map(|(relative_path, stat)| {
                println!("relative_path: {:?}, stat {:?}", relative_path, stat);
                println!("is file: {}, is dir: {}", stat.is_file(), stat.is_dir());

                let accessed = stat
                    .accessed()
                    .ok()
                    .and_then(|t| {
                        FileTime::from_unix_time(
                            t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as _,
                        )
                        .ok()
                    })
                    .unwrap_or_default();
                PlaceholderFile::new(&relative_path)
                    .metadata(
                        match stat.is_dir() {
                            true => Metadata::directory(),
                            false => Metadata::file(),
                        }
                        .size(stat.len())
                        .accessed(accessed),
                    )
                    .mark_in_sync()
                    .overwrite()
                    .blob(
                        self.server_path
                            .join(relative_path)
                            .into_os_string()
                            .into_encoded_bytes(),
                    )
            })
            .collect::<Vec<_>>();

        ticket.pass_with_placeholder(&mut placeholders).unwrap();

        Ok(())
    }

    fn cancel_fetch_placeholders(&self, _request: Request, _info: info::CancelFetchPlaceholders) {
        println!("cancel fetch placeholders");
    }

    fn opened(&self, request: Request, _info: info::Opened) {
        println!("file opened {:?}", request.path());
    }

    fn closed(&self, request: Request, _info: info::Closed) {
        println!("file closed {:?}", request.path());
    }

    fn dehydrate(
        &self,
        _request: Request,
        _ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        println!("dehydrate");
        Err(CloudErrorKind::NotSupported)
    }

    fn dehydrated(&self, _request: Request, _info: info::Dehydrated) {
        println!("dehydrated");
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        println!(
            "delete {:?}, is_undeleted: {}",
            request.path(),
            info.is_undelete()
        );
        if !info.is_undelete() {
            ticket.pass().unwrap();
            return Ok(());
        }

        // Server path stored in fetch_placeholders
        let path = Path::new(unsafe { OsStr::from_encoded_bytes_unchecked(request.file_blob()) });
        match info.is_directory() {
            true => fs::remove_dir_all(path).map_err(|_| CloudErrorKind::InvalidRequest)?,
            false => fs::remove_file(path).map_err(|_| CloudErrorKind::InvalidRequest)?,
        }
        ticket.pass().unwrap();
        Ok(())
    }

    fn deleted(&self, _request: Request, _info: info::Deleted) {
        println!("deleted");
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let src = request.path();
        let dest = info.target_path();

        println!(
            "rename {} to {}, source in scope: {}, target in scope: {}",
            src.display(),
            dest.display(),
            info.source_in_scope(),
            info.target_in_scope()
        );

        match (info.source_in_scope(), info.target_in_scope()) {
            (true, true) => {
                fs::rename(
                    src.strip_prefix(&self.client_path).unwrap(),
                    dest.strip_prefix(&self.client_path).unwrap(),
                )
                .map_err(|_| CloudErrorKind::InvalidRequest)?;
            }
            (true, false) => {}
            (false, true) => Err(CloudErrorKind::NotSupported)?, // TODO
            (false, false) => Err(CloudErrorKind::InvalidRequest)?,
        }

        ticket.pass().unwrap();
        Ok(())
    }

    fn renamed(&self, _request: Request, _info: info::Renamed) {
        println!("renamed");
    }

    fn state_changed(&self, changes: Vec<PathBuf>) {
        println!("state_changed: {:?}", changes);
    }
}

fn mark_in_sync(path: &Path, client: &Path, server: &Path) {
    for entry in path.read_dir().unwrap() {
        let entry = entry.unwrap();
        let remote_path = entry.path().strip_prefix(&client).unwrap().to_owned();

        let Ok(meta) = fs::metadata(server.join(&remote_path)) else {
            continue;
        };
        if meta.is_dir() != entry.file_type().unwrap().is_dir() {
            continue;
        }

        let mut options = ConvertOptions::default()
            .mark_in_sync()
            .blob(remote_path.clone().into_os_string().into_encoded_bytes());
        let mut placeholder = match meta.is_dir() {
            true => {
                options = options.has_children();
                let Ok(placeholder) = Placeholder::open(entry.path()) else {
                    continue;
                };
                placeholder
            }
            false => {
                let Ok(file) = File::open(entry.path()) else {
                    continue;
                };
                file.into()
            }
        };

        _ = placeholder
            .convert_to_placeholder(options, None)
            .inspect_err(|e| println!("convert_to_placeholder {:?}", e));

        if meta.is_dir() {
            mark_in_sync(&entry.path(), client, server);
        }
    }
}

fn wait_for_ctrlc() {
    let (tx, rx) = mpsc::channel();

    ctrlc::set_handler(move || {
        tx.send(()).unwrap();
    })
    .expect("Error setting Ctrl-C handler");

    rx.recv().unwrap();
}

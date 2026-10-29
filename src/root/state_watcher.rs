use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, ReadDir},
    io,
    os::windows::{
        ffi::OsStringExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, RecvTimeoutError, Sender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::filter::{StateWatchEvent, SyncFilter};
use windows::{
    core,
    Win32::{
        Foundation::{
            ERROR_INVALID_DATA, ERROR_IO_INCOMPLETE, ERROR_NOTIFY_ENUM_DIR, E_FAIL, HANDLE,
        },
        Storage::FileSystem::{
            ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
            FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        },
        System::{
            Threading::CreateEventW,
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        },
    },
};

pub(super) fn spawn_root_watcher<T: SyncFilter + 'static>(
    path: PathBuf,
    filter: Arc<T>,
) -> core::Result<(Sender<()>, JoinHandle<()>)> {
    let (cancel, receiver) = mpsc::channel();
    let (ready, started) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("cloud-filter-state-watch".into())
        .spawn(move || {
            let mut watch =
                match Watch::open(&path).and_then(|mut watch| watch.start().map(|()| watch)) {
                    Ok(watch) => watch,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
            if ready.send(Ok(())).is_err() || receiver.recv().is_err() {
                return;
            }

            let mut scan = None::<Scan>;
            let mut delay = Duration::ZERO;
            while matches!(receiver.recv_timeout(delay), Err(RecvTimeoutError::Timeout)) {
                let result = if let Some(paths) = &mut scan {
                    match paths.by_ref().take(64).collect::<io::Result<Vec<_>>>() {
                        Ok(paths) if paths.is_empty() => {
                            scan = None;
                            continue;
                        }
                        batch => batch
                            .map(StateWatchEvent::Changed)
                            .map_err(core::Error::from),
                    }
                } else {
                    if watch.pending { Ok(()) } else { watch.start() }
                        .and_then(|()| watch.complete(false))
                        .and_then(|bytes| watch.event(&path, bytes))
                };

                // Pending I/O and repeated overflow errors back off; batches continue immediately.
                delay = Duration::from_millis(300);
                match result {
                    Ok(event) => {
                        filter.state_changed(event);
                        delay = Duration::ZERO;
                    }
                    Err(error) if error.code() == ERROR_IO_INCOMPLETE.to_hresult() => {}
                    Err(error) if error.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                        match fs::read_dir(&path) {
                            Ok(entries) => scan = Some(Scan(vec![entries])),
                            Err(error) => {
                                filter.state_changed(StateWatchEvent::Stopped(error.into()));
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        filter.state_changed(StateWatchEvent::Stopped(error));
                        return;
                    }
                }
            }
        })
        .map_err(core::Error::from)?;
    match started.recv().unwrap_or_else(|_| Err(E_FAIL.into())) {
        Ok(()) => Ok((cancel, worker)),
        Err(error) => {
            drop(cancel);
            let _ = worker.join();
            Err(error)
        }
    }
}

// Allocated before submitting I/O and never moved out of its Box. The buffer
// and OVERLAPPED remain at stable addresses until completion, including on unwind.
struct Watch {
    directory: File,
    _event: OwnedHandle,
    buffer: [u32; 4 * 1024], // 16 KiB, DWORD-aligned for ReadDirectoryChangesW.
    overlapped: OVERLAPPED,
    pending: bool,
}

impl Watch {
    fn open(path: &Path) -> core::Result<Box<Self>> {
        let directory = OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY.0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED).0)
            .open(path)?;
        // A dedicated manual-reset event also lets Drop wait for cancellation to finish.
        let event = unsafe { CreateEventW(None, true, false, None)? };
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        Ok(Box::new(Self {
            directory,
            overlapped: OVERLAPPED {
                hEvent: HANDLE(event.as_raw_handle()),
                ..Default::default()
            },
            _event: event,
            buffer: [0; 4 * 1024],
            pending: false,
        }))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.directory.as_raw_handle())
    }

    fn start(&mut self) -> core::Result<()> {
        debug_assert!(!self.pending);
        self.overlapped = OVERLAPPED {
            hEvent: self.overlapped.hEvent,
            ..Default::default()
        };
        unsafe {
            ReadDirectoryChangesW(
                self.handle(),
                self.buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&self.buffer) as u32,
                true,
                FILE_NOTIFY_CHANGE_ATTRIBUTES,
                None,
                Some(&mut self.overlapped),
                None,
            )?;
        }
        self.pending = true;
        Ok(())
    }

    fn complete(&mut self, wait: bool) -> core::Result<u32> {
        let mut bytes = 0;
        let result =
            unsafe { GetOverlappedResult(self.handle(), &self.overlapped, &mut bytes, wait) };
        if !matches!(&result, Err(error) if error.code() == ERROR_IO_INCOMPLETE.to_hresult()) {
            self.pending = false;
        }
        result.map(|_| bytes)
    }

    fn event(&self, path: &Path, bytes: u32) -> core::Result<StateWatchEvent> {
        if bytes == 0 {
            return Err(ERROR_NOTIFY_ENUM_DIR.into());
        }
        if bytes as usize > std::mem::size_of_val(&self.buffer) {
            return Err(ERROR_INVALID_DATA.into());
        }
        // Completion has ended all kernel access, and bytes bounds the initialized data.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), bytes as usize)
        };
        parse_changes(path, bytes).map(StateWatchEvent::Changed)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if self.pending {
            // CancelIoEx only requests cancellation. Even ERROR_NOT_FOUND may
            // mean completion won the race; drain before freeing either buffer.
            let _ = unsafe { CancelIoEx(self.handle(), Some(&self.overlapped)) };
            let _ = self.complete(true);
        }
    }
}

// Keep directory iterators rather than collecting the tree before emitting batches.
struct Scan(Vec<ReadDir>);

impl Iterator for Scan {
    type Item = io::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let Some(entry) = self.0.last_mut()?.next() else {
                self.0.pop();
                continue;
            };
            let result = entry.and_then(|entry| {
                let path = entry.path();
                // file_type does not follow symlinks or junctions out of the root.
                if entry.file_type()?.is_dir() {
                    self.0.push(fs::read_dir(&path)?);
                }
                Ok(path)
            });
            // A descendant can disappear while the scan is in progress.
            if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::NotFound) {
                return Some(result);
            }
        }
    }
}

fn parse_changes(root: &Path, mut bytes: &[u8]) -> core::Result<Vec<PathBuf>> {
    let invalid = || core::Error::from(ERROR_INVALID_DATA);
    let mut paths = Vec::new();
    loop {
        if bytes.len() < 12 {
            return Err(invalid());
        }
        let next = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if length == 0 || !length.is_multiple_of(2) || length > bytes.len() - 12 {
            return Err(invalid());
        }
        let name: Vec<u16> = bytes[12..12 + length]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        paths.push(root.join(OsString::from_wide(&name)));
        if next == 0 {
            return Ok(paths);
        }
        if !next.is_multiple_of(4) || next < 12 + length || next >= bytes.len() {
            return Err(invalid());
        }
        bytes = &bytes[next..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_notifications_are_rejected_without_reading_past_buffer() {
        let root = Path::new(r"C:\root");
        assert!(parse_changes(root, &[0; 11]).is_err());
        let mut bytes = vec![0; 16];
        bytes[8..12].copy_from_slice(&100u32.to_le_bytes());
        assert!(parse_changes(root, &bytes).is_err());
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&(b'a' as u16).to_le_bytes());
        assert_eq!(parse_changes(root, &bytes).unwrap(), [root.join("a")]);
        bytes[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(parse_changes(root, &bytes).is_err());
    }
}

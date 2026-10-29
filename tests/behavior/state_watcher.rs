use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, StateWatchEvent, SyncFilter},
    root::{
        HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
        SyncRootInfo,
    },
};
use libtest_mimic::Failed;

struct Directory {
    path: PathBuf,
    registration: Option<SyncRootId>,
}

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cf-watch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self {
            path,
            registration: None,
        }
    }

    fn register(&mut self) {
        let id = SyncRootIdBuilder::new(self.path.file_name().unwrap())
            .user_security_id(SecurityId::current_user().unwrap())
            .build();
        id.register(
            SyncRootInfo::default()
                .with_display_name("State Watch Behavior Test")
                .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                .with_version("1.0.0")
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::AlwaysFull)
                .with_path(&self.path)
                .unwrap(),
        )
        .unwrap();
        self.registration = Some(id);
    }

    fn file(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, b"test").unwrap();
        path
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        if let Some(id) = &self.registration {
            if let Err(error) = id.unregister() {
                eprintln!("failed to unregister test root {:?}: {error}", self.path);
            }
        }
        // Tests toggle readonly. Restore only files in this uniquely created root,
        // including when an assertion unwinds before the normal cleanup.
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(metadata) = path.metadata() {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(false);
                    let _ = std::fs::set_permissions(path, permissions);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn change(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(!permissions.readonly());
    std::fs::set_permissions(path, permissions).unwrap();
}

struct Filter {
    events: Sender<StateWatchEvent>,
    // Tests can pause callbacks to force overflow or inspect an in-progress scan.
    release: Option<Mutex<Receiver<()>>>,
}

impl Filter {
    fn new(events: Sender<StateWatchEvent>) -> Self {
        Self {
            events,
            release: None,
        }
    }
}

impl SyncFilter for Filter {
    fn fetch_data(&self, _: Request, _: ticket::FetchData, _: info::FetchData) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }
    fn state_changed(&self, event: StateWatchEvent) {
        let _ = self.events.send(event);
        if let Some(release) = &self.release {
            // Bound the wait so even a failed assertion cannot hang connection drop.
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        }
    }
}

fn event(receiver: &Receiver<StateWatchEvent>) -> StateWatchEvent {
    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn changed(receiver: &Receiver<StateWatchEvent>, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            StateWatchEvent::Changed(paths) if paths.iter().any(|p| p == path) => return,
            StateWatchEvent::Changed(_) => {} // Other queued changes may arrive first.
            other => panic!("expected change to {path:?}, got {other:?}"),
        }
    }
}

pub fn startup_failures() -> Result<(), Failed> {
    let directory = Directory::new();
    let file = directory.file("not-a-directory");
    // Missing path fails opening the watcher; a file fails submitting its read;
    // an unregistered directory starts the read but fails the CF connection.
    for path in [directory.path.join("missing"), file, directory.path.clone()] {
        let (send, receive) = mpsc::channel();
        assert!(Session::new().connect(path, Filter::new(send)).is_err());
        assert!(matches!(
            receive.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
    Ok(())
}

pub fn unicode_paths_and_shutdown() -> Result<(), Failed> {
    let mut directory = Directory::new();
    let path = directory.file("中文.txt");
    directory.register();
    let (send, receive) = mpsc::channel();
    let connection = Session::new().connect(&directory.path, Filter::new(send))?;
    change(&path);
    changed(&receive, &path);
    drop(connection);
    assert!(receive
        .try_iter()
        .all(|e| !matches!(e, StateWatchEvent::Stopped(_))));
    assert!(matches!(
        receive.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    Ok(())
}

pub fn deleted_root() -> Result<(), Failed> {
    let mut directory = Directory::new();
    directory.register();
    let (send, receive) = mpsc::channel();
    let connection = Session::new().connect(&directory.path, Filter::new(send))?;
    std::fs::remove_dir(&directory.path)?;
    assert!(matches!(event(&receive), StateWatchEvent::Stopped(_)));
    drop(connection);
    assert!(matches!(
        receive.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    Ok(())
}

pub fn idle_disconnect() -> Result<(), Failed> {
    let mut directory = Directory::new();
    directory.register();
    // A pending read must be drained on disconnect. Reconnecting must still work.
    for _ in 0..2 {
        let (send, receive) = mpsc::channel();
        let connection = Session::new().connect(&directory.path, Filter::new(send))?;
        drop(connection);
        assert!(matches!(
            receive.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
    Ok(())
}

pub fn overflow_and_resume() -> Result<(), Failed> {
    let mut directory = Directory::new();
    let files: Vec<_> = (0..512)
        .map(|i| directory.file(&format!("{i}-{}.txt", "x".repeat(100))))
        .collect();
    let folders: Vec<_> = (0..70)
        .map(|i| {
            let path = directory.path.join(format!("folder-{i}"));
            std::fs::create_dir(&path).unwrap();
            path
        })
        .collect();
    directory.register();
    let (send, receive) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let filter = Filter {
        events: send,
        release: Some(Mutex::new(gate)),
    };
    let connection = Session::new().connect(&directory.path, filter)?;
    change(&files[0]);
    changed(&receive, &files[0]);
    // Overflow the real 16 KiB OS buffer while the first callback is paused.
    for path in &files[1..] {
        change(path);
    }
    release.send(()).unwrap();
    let StateWatchEvent::Changed(first) = event(&receive) else {
        panic!("expected a scan batch");
    };
    assert_eq!(first.len(), 64);
    let later = folders
        .iter()
        .find(|path| !first.contains(path))
        .unwrap()
        .join("late.txt");
    // Add a file to a directory that has not been reported yet. Both the continuing
    // scan and the resumed native watcher must tolerate changes during traversal.
    std::fs::write(&later, b"created during scan")?;
    let mut expected: std::collections::HashSet<_> =
        files.iter().chain(&folders).cloned().collect();
    expected.insert(later);
    let mut seen: std::collections::HashSet<_> = first.into_iter().collect();
    release.send(()).unwrap();
    while !expected.is_subset(&seen) {
        let StateWatchEvent::Changed(batch) = event(&receive) else {
            panic!("expected another scan batch");
        };
        assert!(!batch.is_empty() && batch.len() <= 64);
        seen.extend(batch);
        release.send(()).unwrap();
    }
    drop(release);
    change(&files[0]);
    changed(&receive, &files[0]);
    drop(connection);
    assert!(receive
        .try_iter()
        .all(|e| !matches!(e, StateWatchEvent::Stopped(_))));
    Ok(())
}

pub fn async_terminal_event() -> Result<(), Failed> {
    struct AsyncFilter(Sender<StateWatchEvent>);
    impl cloud_filter::filter::Filter for AsyncFilter {
        async fn fetch_data(
            &self,
            _: Request,
            _: ticket::FetchData,
            _: info::FetchData,
        ) -> CResult<()> {
            Err(CloudErrorKind::NotSupported)
        }
        async fn state_changed(&self, event: StateWatchEvent) {
            let _ = self.0.send(event);
        }
    }
    let mut directory = Directory::new();
    directory.register();
    let (send, receive) = mpsc::channel();
    let connection =
        Session::new().connect_async(&directory.path, AsyncFilter(send), |future| {
            futures::executor::block_on(future)
        })?;
    std::fs::remove_dir(&directory.path)?;
    assert!(matches!(event(&receive), StateWatchEvent::Stopped(_)));
    drop(connection);
    assert!(matches!(
        receive.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
    Ok(())
}

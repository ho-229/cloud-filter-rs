use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use cloud_filter::{
    metadata::Metadata,
    placeholder::Placeholder,
    placeholder_file::{BatchCreate, PlaceholderFile},
    root::{
        HydrationType, PopulationType, SecurityId, SyncRootId, SyncRootIdBuilder, SyncRootInfo,
    },
};
use libtest_mimic::Failed;
use windows::Wdk::Storage::FileSystem::{
    RtlQueryThreadPlaceholderCompatibilityMode, RtlSetThreadPlaceholderCompatibilityMode,
};
use windows::Win32::Storage::FileSystem::{
    GetFileType, FILE_FLAG_BACKUP_SEMANTICS, FILE_TYPE_DISK,
};

// Only clean up a unique directory successfully created by this test.
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> std::io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cloud-filter-behavior-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Registration(SyncRootId);

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.0.unregister();
    }
}

pub fn file_round_trip_transfers_ownership() -> Result<(), Failed> {
    let directory = TestDirectory::new()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.path().join("round-trip"))?;
    let placeholder = Placeholder::from(file);
    assert!(placeholder.state()?.is_none());
    {
        let handle = placeholder.win32_handle()?;
        assert_eq!(unsafe { GetFileType(handle.handle()) }, FILE_TYPE_DISK);
    }
    assert!(placeholder.fixed_size_info(usize::MAX).is_err());
    let mut file = File::try_from(placeholder)?;
    file.write_all(b"still open")?;
    file.seek(SeekFrom::Start(0))?;
    let mut data = String::new();
    file.read_to_string(&mut data)?;
    assert_eq!(data, "still open");

    // An unsupported device query must restore the thread's compatibility mode too.
    let device = Placeholder::from(File::open("NUL")?);
    let previous = unsafe { RtlQueryThreadPlaceholderCompatibilityMode() };
    assert!(device.state().is_err());
    assert_eq!(
        unsafe { RtlQueryThreadPlaceholderCompatibilityMode() },
        previous
    );
    Ok(())
}

pub fn placeholder_outputs_and_protected_handles_remain_valid() -> Result<(), Failed> {
    let directory = TestDirectory::new()?;
    let registration = Registration(
        SyncRootIdBuilder::new(directory.path().file_name().unwrap())
            .user_security_id(SecurityId::current_user()?)
            .build(),
    );
    registration
        .0
        .register(
            SyncRootInfo::default()
                .with_display_name("Cloud Filter Behavior Test")
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::Full)
                .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                .with_version("1.0.0")
                .with_path(directory.path())?,
        )
        .context("register temporary root")?;

    let usn = PlaceholderFile::new("owned.txt")
        .metadata(Metadata::file().size(0))
        .blob(b"old".to_vec())
        .blob(Vec::new())
        .blob(b"identity".to_vec())
        .create(directory.path())
        .context("create single placeholder")?;
    assert!(usn > 0, "return the USN written by Windows");
    let mut batch = [PlaceholderFile::new("batch.txt")
        .metadata(Metadata::file().size(0))
        .blob(b"batch".to_vec())];
    batch.create(directory.path()).context("create batch")?;
    assert!(batch[0].result()? > 0);

    PlaceholderFile::new("offline.bin")
        .metadata(Metadata::file().size(8192))
        .blob(b"offline".to_vec())
        .mark_in_sync()
        .create(directory.path())
        .context("create offline placeholder")?;
    let mut offline = Placeholder::from(
        OpenOptions::new()
            .access_mode(0)
            .open(directory.path().join("offline.bin"))?,
    );
    // Explicitly disguise placeholders to exercise the state query's scoped override.
    let previous = unsafe { RtlSetThreadPlaceholderCompatibilityMode(1) };
    assert!(previous >= 0);
    let state = offline.state();
    let restored = unsafe { RtlSetThreadPlaceholderCompatibilityMode(previous) };
    assert_eq!(restored, 1, "state must restore the caller's thread mode");
    let state = state
        .context("query offline state")?
        .context("offline placeholder state")?;
    assert!(state.in_sync());
    assert!(state.partial());
    assert!(state.partial_on_disk());
    assert_eq!(offline.info()?.unwrap().on_disk_data_size(), 0);
    offline.mark_in_sync(false, None)?;
    assert!(!offline.state()?.unwrap().in_sync());

    let root = Placeholder::from(
        OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(directory.path())?,
    );
    assert!(root.state()?.context("sync root state")?.sync_root());

    let placeholder = Placeholder::open(directory.path().join("owned.txt"))?;
    let info = placeholder.info()?.unwrap();
    assert_eq!(info.blob(), b"identity");
    assert_eq!(placeholder.fixed_size_info(8)?.unwrap().blob(), info.blob());
    {
        let handle = placeholder.win32_handle()?;
        let second = placeholder.win32_handle()?;
        assert_eq!(unsafe { GetFileType(handle.handle()) }, FILE_TYPE_DISK);
        drop(handle);
        assert_eq!(unsafe { GetFileType(second.handle()) }, FILE_TYPE_DISK);
    }
    assert!(placeholder.state()?.is_some());
    assert!(File::try_from(placeholder).is_err());
    Ok(())
}

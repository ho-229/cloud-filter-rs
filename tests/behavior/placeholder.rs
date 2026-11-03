use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use cloud_filter::{
    error::{CResult, CloudErrorKind},
    ext::FileExt,
    filter::{info, ticket, Request, SyncFilter},
    metadata::Metadata,
    placeholder::{Placeholder, UpdateOptions},
    placeholder_create_info::{BatchCreate, PlaceholderCreateInfo},
    root::Session,
};
use libtest_mimic::Failed;
use windows::Wdk::Storage::FileSystem::{
    RtlQueryThreadPlaceholderCompatibilityMode, RtlSetThreadPlaceholderCompatibilityMode,
};
use windows::Win32::Storage::FileSystem::{
    GetFileType, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES,
    FILE_TYPE_DISK,
};

use super::common::{open, Root};

pub fn file_usn_tracks_changes() -> Result<(), Failed> {
    let root = Root::new()?;
    let path = root.path.join("local.txt");
    fs::write(&path, b"original")?;
    let file = File::open(&path)?;
    let before = file.usn()?;
    assert!(before > 0);
    fs::write(&path, b"changed after the first query")?;
    assert!(file.usn()? > before, "query the latest journaled change");
    let overlapped = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED.0)
        .open(&path)?;
    assert_eq!(overlapped.usn()?, file.usn()?);
    assert!(File::open("NUL")?.usn().is_err());

    root.register(false)?;
    root.file("offline.bin")?;
    let offline = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .open(root.path.join("offline.bin"))?;
    assert!(offline.usn()? > 0);
    let placeholder = Placeholder::from(offline);
    assert_eq!(
        placeholder.info()?.unwrap().on_disk_data_size(),
        0,
        "USN queries must not hydrate data"
    );
    Ok(())
}

pub fn file_round_trip_transfers_ownership() -> Result<(), Failed> {
    let directory = Root::new()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.path.join("round-trip"))?;
    let placeholder = Placeholder::from(file);
    assert!(placeholder.state()?.is_none());
    {
        let handle = placeholder.win32_handle()?;
        assert_eq!(unsafe { GetFileType(handle.handle()) }, FILE_TYPE_DISK);
    }
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
    let directory = Root::new()?;
    directory.register(false)?;

    let usn = PlaceholderCreateInfo::new("owned.txt")
        .metadata(Metadata::file().size(0))
        .blob(b"identity".to_vec())
        .create(directory.path.as_path())
        .context("create single placeholder")?;
    assert!(usn > 0, "return the USN written by Windows");

    PlaceholderCreateInfo::new("offline.bin")
        .metadata(Metadata::file().size(8192))
        .blob(b"offline".to_vec())
        .mark_in_sync()
        .create(directory.path.as_path())
        .context("create offline placeholder")?;
    let mut offline = Placeholder::from(
        OpenOptions::new()
            .access_mode(0)
            .share_mode(0)
            .open(directory.path.join("offline.bin"))?,
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

    let written = nt_time::FileTime::new(133_000_000_000_000_000);
    offline.update(
        UpdateOptions::default()
            .metadata(Metadata::file().written(written).size(42))
            .mark_in_sync()
            .dehydrate(),
        None,
    )?;
    let metadata = fs::metadata(directory.path.join("offline.bin"))?;
    assert_eq!(metadata.len(), 42);
    assert_eq!(metadata.last_write_time(), written.to_raw());
    assert!(offline.info()?.unwrap().is_in_sync());
    assert_eq!(offline.info()?.unwrap().on_disk_data_size(), 0);

    let root = Placeholder::from(
        OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(directory.path.as_path())?,
    );
    assert!(root.state()?.context("sync root state")?.sync_root());

    let placeholder = Placeholder::open(directory.path.join("owned.txt"))?;
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

// Exercise both native creation paths, including population completion and
// partial failure. Successful HRESULTs alone do not prove the metadata is valid.
pub fn placeholder_batches_preserve_identities() -> Result<(), Failed> {
    fn placeholders(identities: &[(bool, Vec<u8>)]) -> Vec<PlaceholderCreateInfo> {
        identities
            .iter()
            .enumerate()
            .map(|(index, (directory, blob))| {
                let entry = PlaceholderCreateInfo::new(format!("item-{index}"));
                if *directory {
                    entry.metadata(Metadata::directory()).has_no_children()
                } else {
                    entry.metadata(Metadata::file().size(4096))
                }
                .mark_in_sync()
                .blob(blob.clone())
            })
            .collect()
    }

    // Equal or increasing lengths can still switch from compressed to
    // uncompressed metadata. A deterministic identity makes this reproducible.
    let random_identity = |length| {
        let mut seed = 0x1234_5678u32;
        (0..length)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect::<Vec<_>>()
    };
    // The original shrinking-identity case; each fixture is (is_directory, identity).
    let mut identities: Vec<_> = [112, 139, 117, 105, 103]
        .into_iter()
        .enumerate()
        .map(|(i, len)| (false, vec![i as u8 + 1; len]))
        .collect();
    identities.extend([
        // Equal lengths with different compressibility defeat length sorting.
        (false, vec![1; 512]),
        (false, random_identity(512)),
        // Increasing lengths can cross the same compression boundary.
        (false, vec![1; 139]),
        (false, random_identity(331)),
        // File and directory metadata can also cross it with increasing lengths.
        (false, vec![1; 139]),
        (true, vec![2; 140]),
    ]);

    struct Populate {
        identities: Vec<(bool, Vec<u8>)>,
        results: Arc<Mutex<Vec<windows::core::Result<()>>>>,
    }
    impl SyncFilter for Populate {
        fn fetch_data(&self, _: Request, _: ticket::FetchData, _: info::FetchData) -> CResult<()> {
            Err(CloudErrorKind::NotSupported)
        }

        fn fetch_placeholders(
            &self,
            request: Request,
            ticket: ticket::FetchPlaceholders,
            _: info::FetchPlaceholders,
        ) -> CResult<()> {
            let mut batch = placeholders(&self.identities);
            let mut result = Ok(());
            for page in batch.chunks_mut(3) {
                // Empty submissions and intermediate pages must leave population
                // pending. Only the callback's successful return finishes it.
                let empty = ticket.pass_with_placeholders(&mut []);
                let submitted = ticket.pass_with_placeholders(page);
                assert!(open(&request.path())
                    .unwrap()
                    .state()
                    .unwrap()
                    .unwrap()
                    .partial());
                // Keep submitting later pages even if an earlier entry failed.
                result = result.and(empty).and(submitted);
            }
            // Enumeration can return entries despite a population error, so
            // capture the provider's result independently of the client exit code.
            self.results.lock().unwrap().push(result.clone());
            result.map_err(|_| CloudErrorKind::Unsuccessful)
        }
    }

    for (on_demand, existing, empty) in [
        (false, None, false),
        (true, None, false),
        (false, Some(3), false),
        (true, Some(3), false),
        // A failure in the last entry must not complete population either.
        (true, Some(identities.len() - 1), false),
        // With no submissions, a successful callback must still finish an empty directory.
        (false, None, true),
        (true, None, true),
    ] {
        let identities = if empty { &[][..] } else { &identities[..] };
        println!("batch: on_demand={on_demand}, existing={existing:?}, empty={empty}");
        let root = Root::new()?;
        if let Some(index) = existing {
            fs::write(root.path.join(format!("item-{index}")), b"existing")?;
        }
        root.register(false)?;
        let results = Arc::new(Mutex::new(Vec::new()));
        let _connection = if on_demand {
            Some(Session::new().connect(
                &root.path,
                Populate {
                    identities: identities.to_vec(),
                    results: results.clone(),
                },
            )?)
        } else {
            let mut batch = placeholders(identities);
            let result = batch.create(&root.path);
            assert_eq!(result.is_err(), existing.is_some(), "{result:?}");
            // An error must not skip later entries or overwrite their results.
            for (index, placeholder) in batch.iter().enumerate() {
                if Some(index) == existing {
                    assert!(placeholder.result().is_err(), "missing collision result");
                    assert_eq!(
                        result.as_ref().unwrap_err().code(),
                        placeholder.result().unwrap_err().code()
                    );
                } else {
                    assert!(placeholder.result()? > 0, "missing creation USN");
                }
            }
            None
        };
        if on_demand {
            // Enumeration must originate outside the provider process. Enumerate
            // successful roots twice to check that completion prevents another fetch.
            for _ in 0..if existing.is_some() { 1 } else { 2 } {
                let output = Command::new("powershell.exe")
                    .args(["-NoProfile", "-NonInteractive", "-Command",
                        "$ErrorActionPreference = 'Stop'; Get-ChildItem -LiteralPath $env:CF_BATCH_ROOT | Select-Object -ExpandProperty Name"])
                    .env("CF_BATCH_ROOT", &root.path)
                    .output()?;
                if existing.is_none() {
                    assert!(
                        output.status.success(),
                        "enumerate: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let mut actual: Vec<_> = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_owned)
                        .collect();
                    let mut expected: Vec<_> =
                        (0..identities.len()).map(|i| format!("item-{i}")).collect();
                    actual.sort();
                    expected.sort();
                    assert_eq!(
                        actual, expected,
                        "enumeration completed before all entries were created"
                    );
                }
            }
            let results = results.lock().unwrap();
            assert!(!results.is_empty(), "missing population callback");
            assert!(
                results
                    .iter()
                    .all(|result| result.is_err() == existing.is_some()),
                "{results:?}"
            );
            // Windows may retry a failed population; only success forbids another fetch.
            if existing.is_none() {
                assert_eq!(results.len(), 1, "unexpected repeated population");
            }
            assert_eq!(
                open(&root.path)?.state()?.context("root state")?.partial(),
                existing.is_some()
            );
        }
        // Corruption surfaces when reopening metadata, even if creation returned
        // S_OK. Check the full identity without hydrating file contents.
        for (index, (directory, blob)) in identities.iter().enumerate() {
            let path = root.path.join(format!("item-{index}"));
            if Some(index) == existing {
                assert_eq!(fs::read(path)?, b"existing");
                continue;
            }
            let info = open(&path)?.info()?.context("placeholder metadata")?;
            assert_eq!(info.blob(), blob);
            assert!(info.is_in_sync());
            assert_eq!(info.on_disk_data_size(), 0);
            assert_eq!(fs::metadata(path)?.is_dir(), *directory);
        }
    }
    Ok(())
}

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, SyncFilter},
    metadata::{Metadata, MetadataExt},
    placeholder_create_info::PlaceholderCreateInfo,
    root::{HydrationType, Session},
    utility::WriteAt,
};
use libtest_mimic::Failed;
use windows::{
    core,
    Win32::Foundation::{
        ERROR_CLOUD_FILE_INVALID_REQUEST, ERROR_CLOUD_FILE_NOT_SUPPORTED, ERROR_NOT_SUPPORTED,
    },
};

use super::common::{open, Client, Root};

const FILE_SIZE: u64 = 8193;
const ORIGINAL_WRITE_TIME: i64 = 132_536_736_000_000_000;
const WRITE_TIME: i64 = 132_537_600_000_000_000;

struct Restart {
    policy: HydrationType,
    validation: bool,
    options: ticket::RestartHydrationOptions<'static>,
    restarted: AtomicBool,
    result: Mutex<Option<core::Result<()>>>,
}

impl Restart {
    fn restart(
        &self,
        restart: impl Fn(ticket::RestartHydrationOptions<'static>) -> core::Result<()>,
    ) -> CResult<()> {
        // Resizing pending hydration stalled in CldFlt on build 19044. Reject it
        // before the native call, and verify this leaves the ticket usable.
        if self.policy == HydrationType::Full {
            let resize = ticket::RestartHydrationOptions::default()
                .metadata(Metadata::default().size(FILE_SIZE - 1));
            assert_eq!(
                restart(resize).unwrap_err().code(),
                ERROR_NOT_SUPPORTED.to_hresult()
            );
        }
        let result = restart(self.options.clone());
        *self.result.lock().unwrap() = Some(result.clone());
        result.map_err(|_| CloudErrorKind::NotSupported)
    }
}

impl SyncFilter for Restart {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        if !self.validation && !self.restarted.swap(true, Ordering::SeqCst) {
            return self.restart(|options| ticket.restart_hydration(options));
        }
        // The external client may retry a rejected read. Keep rejecting it instead
        // of accidentally making an unsupported restart look successful.
        if self.policy != HydrationType::Full {
            return Err(CloudErrorKind::NotSupported);
        }
        let range = info.required_file_range();
        let byte = if request.file_blob() == b"new" {
            b'n'
        } else {
            b'o'
        };
        ticket
            .write_at(&vec![byte; (range.end - range.start) as usize], range.start)
            .map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn validate_data(
        &self,
        _: Request,
        ticket: ticket::ValidateData,
        info: info::ValidateData,
    ) -> CResult<()> {
        if self.validation && !self.restarted.swap(true, Ordering::SeqCst) {
            return self.restart(|options| ticket.restart_hydration(options));
        }
        ticket
            .pass(info.file_range())
            .map_err(|_| CloudErrorKind::Unsuccessful)
    }
}

pub fn restart_hydration() -> Result<(), Failed> {
    for (policy, validation, update) in [
        (HydrationType::Full, false, false),
        (HydrationType::Full, false, true),
        (HydrationType::Full, true, true),
        (HydrationType::Partial, false, false),
    ] {
        let root = Root::new()?;
        root.register_with_hydration(policy, validation)?;
        PlaceholderCreateInfo::new("file.txt")
            .metadata(
                Metadata::file()
                    .size(FILE_SIZE)
                    .last_write_time(ORIGINAL_WRITE_TIME),
            )
            .blob(b"old".to_vec())
            .create(&root.path)?;
        let options = if update {
            ticket::RestartHydrationOptions::default()
                .metadata(
                    Metadata::default()
                        .size(FILE_SIZE)
                        .last_write_time(WRITE_TIME),
                )
                .blob(b"new")
                .mark_in_sync()
        } else {
            ticket::RestartHydrationOptions::default()
        };
        let connection = Session::new().block_implicit_hydration().connect(
            &root.path,
            Restart {
                policy,
                validation,
                options,
                restarted: AtomicBool::new(false),
                result: Mutex::new(None),
            },
        )?;
        if policy == HydrationType::Partial {
            Client::run(
                &root,
                r#"
$ErrorActionPreference = 'Stop'
try {
    [IO.File]::ReadAllText((Join-Path $env:CF_PROBE_ROOT 'file.txt')) | Out-Null
    throw 'Partial-policy restart unexpectedly succeeded'
} catch {
    if (($_.Exception.GetBaseException().HResult -band 0xffff) -ne 379) { throw }
}
exit 0
"#,
            )?;
            let result = connection.filter().result.lock().unwrap();
            let code = result.as_ref().unwrap().as_ref().unwrap_err().code();
            // Docs specify NOT_SUPPORTED; build 19044 returns INVALID_REQUEST.
            assert!(
                code == ERROR_CLOUD_FILE_NOT_SUPPORTED.to_hresult()
                    || code == ERROR_CLOUD_FILE_INVALID_REQUEST.to_hresult(),
                "{code:?}"
            );
        } else {
            // Validation restarts after old bytes reached disk. The pending read
            // must return only replacement bytes, including the unaligned EOF.
            let script = format!(
                r#"
$ErrorActionPreference = 'Stop'
$content = [IO.File]::ReadAllText((Join-Path $env:CF_PROBE_ROOT 'file.txt'))
if ($content -cne ('{}' * {FILE_SIZE})) {{ throw 'Restart returned stale contents' }}
"#,
                if update { 'n' } else { 'o' },
            );
            Client::run(&root, &script)?;
            let result = connection.filter().result.lock().unwrap().take();
            result.unwrap()?;
            let path = root.path.join("file.txt");
            let info = open(&path)?.info()?.unwrap();
            assert_eq!(info.blob(), if update { b"new" } else { b"old" });
            let metadata = std::fs::metadata(path)?;
            assert_eq!(metadata.len(), FILE_SIZE);
            assert_eq!(info.is_in_sync(), update);
            assert_eq!(
                std::os::windows::fs::MetadataExt::last_write_time(&metadata),
                if update {
                    WRITE_TIME
                } else {
                    ORIGINAL_WRITE_TIME
                } as u64
            );
        }
    }
    Ok(())
}

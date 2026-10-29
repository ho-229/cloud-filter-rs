//! Native lifecycle and callback probes. Each test owns an isolated temporary root.
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Context};
use cloud_filter::{
    error::{CResult, CloudErrorKind},
    ext::FileExt,
    filter::{info, ticket, Request, SyncFilter},
    metadata::Metadata,
    placeholder::{ConvertOptions, Placeholder, UpdateOptions},
    placeholder_create_info::PlaceholderCreateInfo,
    root::{
        HydrationPolicy, HydrationType, PopulationType, SecurityId, Session, SyncRootId,
        SyncRootIdBuilder, SyncRootInfo,
    },
    utility::WriteAt,
};
use libtest_mimic::Failed;
use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

struct Root {
    path: PathBuf,
    id: SyncRootId,
}

impl Root {
    fn new() -> anyhow::Result<Self> {
        let name = format!(
            "cf-lifecycle-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        // On the tested host WinRT rejected roots under AppData/Temp, while
        // native CF registration succeeded. Keep lifecycle probes outside it.
        let parent = std::env::current_dir()?.join("target");
        fs::create_dir_all(&parent)?;
        let path = parent.join(&name);
        fs::create_dir(&path)?;
        Ok(Self {
            path,
            id: SyncRootIdBuilder::new(name)
                .user_security_id(SecurityId::current_user()?)
                .build(),
        })
    }

    fn register(&self, validation: bool) -> anyhow::Result<()> {
        self.id.register(
            SyncRootInfo::default()
                .with_display_name("Cloud Filter Lifecycle Test")
                .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                .with_version("1.0.0")
                .with_hydration_type(HydrationType::Partial)
                .with_hydration_policy(if validation {
                    HydrationPolicy::ValidationRequired | HydrationPolicy::AutoDehydrationAllowed
                } else {
                    HydrationPolicy::AutoDehydrationAllowed.into()
                })
                .with_population_type(PopulationType::Full)
                .with_path(&self.path)?,
        )?;
        Ok(())
    }

    fn file(&self, name: &str) -> anyhow::Result<()> {
        PlaceholderCreateInfo::new(name)
            .metadata(Metadata::file().size(8192))
            .blob(name.as_bytes().to_vec())
            .mark_in_sync()
            .create(&self.path)?;
        Ok(())
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = self.id.unregister();
        // Also clean up this test-owned native registration if WinRT lookup failed.
        if let Ok(path) = widestring::U16CString::from_os_str(&self.path) {
            let _ = unsafe {
                windows::Win32::Storage::CloudFilters::CfUnregisterSyncRoot(windows::core::PCWSTR(
                    path.as_ptr(),
                ))
            };
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn open(path: &Path) -> anyhow::Result<Placeholder> {
    Ok(OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?
        .into())
}

#[derive(Debug, Clone)]
struct Event {
    kind: &'static str,
    name: String,
}

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<Event>>>);

impl Probe {
    fn record(&self, kind: &'static str, request: &Request, detail: impl std::fmt::Debug) {
        let name = request
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        println!(
            "{kind}: {name}, pid={:?}, {detail:?}",
            request.try_process().map(|p| p.id())
        );
        self.0.lock().unwrap().push(Event { kind, name });
    }

    fn count(&self, kind: &str, name: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind && e.name == name)
            .count()
    }

    fn wait(&self, kind: &str, name: &str) -> anyhow::Result<()> {
        self.wait_for(kind, name, Duration::from_secs(5))
    }

    fn wait_for(&self, kind: &str, name: &str, duration: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + duration;
        while self.count(kind, name) == 0 {
            ensure!(
                Instant::now() < deadline,
                "missing {kind} for {name}: {:?}",
                self.0.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

fn api_error(_: windows::core::Error) -> CloudErrorKind {
    CloudErrorKind::Unsuccessful
}

impl SyncFilter for Probe {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        self.record("fetch_data", &request, &info);
        if request.path().file_name().unwrap() == "pending.bin" {
            return Ok(());
        }
        let range = info.required_file_range();
        let end = range.end.min(request.file_size());
        ticket
            .write_at(&vec![b'x'; (end - range.start) as usize], range.start)
            .map_err(api_error)
    }
    fn validate_data(
        &self,
        request: Request,
        ticket: ticket::ValidateData,
        info: info::ValidateData,
    ) -> CResult<()> {
        self.record("validate_data", &request, &info);
        ticket.pass(info.file_range()).map_err(api_error)
    }
    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        _: info::FetchPlaceholders,
    ) -> CResult<()> {
        self.record("fetch_placeholders", &request, ());
        if request.path().file_name().unwrap() == "pending-dir" {
            return Ok(());
        }
        ticket.pass_with_placeholder(&mut []).map_err(api_error)
    }
    fn cancel_fetch_data(&self, request: Request, info: info::CancelFetchData) {
        self.record("cancel_fetch_data", &request, info);
    }
    fn cancel_fetch_placeholders(&self, request: Request, info: info::CancelFetchPlaceholders) {
        self.record("cancel_fetch_placeholders", &request, info);
    }
    fn opened(&self, request: Request, info: info::Opened) {
        self.record("opened", &request, info);
    }
    fn closed(&self, request: Request, info: info::Closed) {
        self.record("closed", &request, info);
    }
    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        info: info::Dehydrate,
    ) -> CResult<()> {
        self.record("dehydrate", &request, info);
        ticket.pass().map_err(api_error)
    }
    fn dehydrated(&self, request: Request, info: info::Dehydrated) {
        self.record("dehydrated", &request, info);
    }
    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        self.record("delete", &request, info);
        ticket.pass().map_err(api_error)
    }
    fn deleted(&self, request: Request, info: info::Deleted) {
        self.record("deleted", &request, info);
    }
    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        self.record("rename", &request, info);
        ticket.pass().map_err(api_error)
    }
    fn renamed(&self, request: Request, info: info::Renamed) {
        self.record("renamed", &request, info);
    }
}

// Bound subprocess lifetime, including on assertion failure; scripts operate only on this root.
struct Client(Child);
impl Client {
    fn start(root: &Root, script: &str) -> anyhow::Result<Self> {
        Ok(Self(
            Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("CF_PROBE_ROOT", &root.path)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        ))
    }
    fn run(root: &Root, script: &str) -> anyhow::Result<()> {
        let mut client = Self::start(root, script)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = client.0.try_wait()? {
                ensure!(status.success(), "client: {status}");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "client timed out: {script}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn registration_lifecycle() -> Result<(), Failed> {
    registration_lifecycle_inner().map_err(Into::into)
}

fn registration_lifecycle_inner() -> anyhow::Result<()> {
    let root = Root::new()?;
    fs::create_dir(root.path.join("existing-dir"))?;
    fs::write(root.path.join("existing-dir/local.txt"), b"local contents")?;
    root.register(false)
        .context("register nonempty directory")?;
    ensure!(open(&root.path.join("existing-dir"))?.state()?.is_none());
    ensure!(open(&root.path.join("existing-dir/local.txt"))?
        .info()?
        .is_none());
    root.file("offline.bin")?;
    root.file("partial.bin")?;
    root.file("full.bin")?;
    for (name, full) in [("unpopulated-dir", false), ("populated-dir", true)] {
        let entry = PlaceholderCreateInfo::new(name)
            .metadata(Metadata::directory())
            .blob(name.as_bytes().to_vec());
        if full { entry.has_no_children() } else { entry }.create(&root.path)?;
    }
    let probe = Probe::default();
    let connection = Session::new().connect(&root.path, probe.clone())?;
    open(&root.path.join("partial.bin"))?.hydrate(0..4096)?;
    open(&root.path.join("full.bin"))?.hydrate(..)?;
    let original = open(&root.path.join("full.bin"))?.info()?.unwrap();
    Client::run(
        &root,
        "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath $env:CF_PROBE_ROOT | Out-Null",
    )?;
    drop(connection);
    ensure!(root.id.is_registered()?);
    let connection = Session::new().connect(&root.path, probe.clone())?;
    let unregister_connected = root.id.unregister();
    println!(
        "WinRT unregister while connected: {unregister_connected:?}; registered={:?}",
        root.id.is_registered()
    );
    ensure!(
        unregister_connected.is_err(),
        "unregister unexpectedly succeeded while connected"
    );
    println!(
        "native root after failed WinRT unregister: {:?}",
        open(&root.path)?.state()?
    );
    drop(connection);
    ensure!(
        open(&root.path.join("offline.bin"))?
            .info()?
            .unwrap()
            .on_disk_data_size()
            == 0
    );
    root.register(false)
        .context("update existing registration")?;
    let updated = open(&root.path.join("full.bin"))?.info()?.unwrap();
    ensure!(updated.file_id() == original.file_id() && updated.blob() == original.blob());
    println!(
        "root state after updating registration: {:?}",
        open(&root.path)?.state()?
    );
    let connection = Session::new().connect(&root.path, probe.clone())?;
    Client::run(&root, "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'existing-dir') | Out-Null")?;
    ensure!(probe.count("fetch_placeholders", "existing-dir") == 0);
    open(&root.path.join("existing-dir"))?.convert_to_placeholder(
        ConvertOptions::default()
            .has_children()
            .blob(b"existing-dir".to_vec()),
        None,
    )?;
    Client::run(&root, "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'existing-dir') | Out-Null")?;
    probe.wait("fetch_placeholders", "existing-dir")?;
    ensure!(fs::read(root.path.join("existing-dir/local.txt"))? == b"local contents");
    drop(connection);
    root.file("locked.bin")?;
    let locked = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(root.path.join("locked.bin"))?;
    root.id
        .unregister()
        .context("unregister disconnected root")?;
    drop(locked);
    ensure!(!root.id.is_registered()?);
    for name in ["offline.bin", "partial.bin", "unpopulated-dir"] {
        println!(
            "after unregister: {name}, exists={}",
            root.path.join(name).exists()
        );
        ensure!(
            !root.path.join(name).exists(),
            "unexpected survivor: {name}"
        );
    }
    for name in ["full.bin", "populated-dir", "existing-dir"] {
        ensure!(
            open(&root.path.join(name))?.state()?.is_none(),
            "not reverted: {name}"
        );
    }
    ensure!(fs::read(root.path.join("full.bin"))? == vec![b'x'; 8192]);
    println!(
        "skipped locked placeholder after unregister: {:?}",
        open(&root.path.join("locked.bin"))?.state()?
    );
    root.register(false)
        .context("re-register surviving nonempty tree")?;
    ensure!(open(&root.path.join("full.bin"))?.info()?.is_none());
    ensure!(PlaceholderCreateInfo::new("full.bin")
        .metadata(Metadata::file().size(8192))
        .blob(b"replacement".to_vec())
        .create(&root.path)
        .is_err());
    ensure!(fs::read(root.path.join("full.bin"))? == vec![b'x'; 8192]);
    ensure!(open(&root.path.join("existing-dir"))?.state()?.is_none());
    ensure!(open(&root.path.join("locked.bin"))?.info()?.unwrap().blob() == b"locked.bin");
    ensure!(fs::read(root.path.join("existing-dir/local.txt"))? == b"local contents");
    println!("re-register succeeded; reverted children stay ordinary, skipped placeholders retain identity");
    Ok(())
}

pub fn callback_triggers() -> Result<(), Failed> {
    callback_triggers_inner().map_err(Into::into)
}

fn callback_triggers_inner() -> anyhow::Result<()> {
    let root = Root::new()?;
    root.register(true)?;
    for name in ["provider.bin", "client.bin"] {
        root.file(name)?;
    }
    PlaceholderCreateInfo::new("enumerated-dir")
        .metadata(Metadata::directory())
        .blob(b"enumerated-dir".to_vec())
        .create(&root.path)?;
    let probe = Probe::default();
    let _connection = Session::new().connect(&root.path, probe.clone())?;
    fs::read_dir(root.path.join("enumerated-dir"))?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(probe.count("fetch_placeholders", "enumerated-dir") == 0);
    Client::run(&root, "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'enumerated-dir') | Out-Null")?;
    probe.wait("fetch_placeholders", "enumerated-dir")?;
    let count = probe.count("fetch_placeholders", "enumerated-dir");
    Client::run(&root, "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'enumerated-dir') | Out-Null")?;
    ensure!(probe.count("fetch_placeholders", "enumerated-dir") == count);
    open(&root.path.join("enumerated-dir"))?
        .update(UpdateOptions::default().has_children(), None)?;
    Client::run(&root, "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'enumerated-dir') | Out-Null")?;
    ensure!(probe.count("fetch_placeholders", "enumerated-dir") > count);
    open(&root.path.join("provider.bin"))?.hydrate(..)?;
    probe.wait("fetch_data", "provider.bin")?;
    probe.wait("validate_data", "provider.bin")?;
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(0)
        .open(root.path.join("provider.bin"))?;
    file.dehydrate(..)?;
    drop(file);
    fs::rename(
        root.path.join("provider.bin"),
        root.path.join("provider-renamed.bin"),
    )?;
    fs::remove_file(root.path.join("provider-renamed.bin"))?;
    println!("provider-origin events: {:?}", probe.0.lock().unwrap());
    Client::run(&root, "$ErrorActionPreference='Stop'; [IO.File]::ReadAllBytes((Join-Path $env:CF_PROBE_ROOT 'client.bin')) | Out-Null")?;
    probe.wait("fetch_data", "client.bin")?;
    probe.wait("validate_data", "client.bin")?;
    probe.wait("opened", "client.bin")?;
    probe.wait("closed", "client.bin")?;
    Client::run(
        &root,
        r#"
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CloudProbe {
    [DllImport("cldapi.dll")]
    public static extern int CfDehydratePlaceholder(IntPtr file, long offset, long length, uint flags, IntPtr overlapped);
}
'@
$file = [IO.File]::Open((Join-Path $env:CF_PROBE_ROOT 'client.bin'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
try { [Runtime.InteropServices.Marshal]::ThrowExceptionForHR([CloudProbe]::CfDehydratePlaceholder($file.SafeFileHandle.DangerousGetHandle(), 0, -1, 0, [IntPtr]::Zero)) }
finally { $file.Dispose() }
"#,
    )?;
    probe.wait("dehydrate", "client.bin")?;
    probe.wait("dehydrated", "client.bin")?;
    ensure!(
        open(&root.path.join("client.bin"))?
            .info()?
            .unwrap()
            .on_disk_data_size()
            == 0
    );
    Client::run(&root, "$ErrorActionPreference='Stop'; Rename-Item -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'client.bin') -NewName 'renamed.bin'; Remove-Item -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'renamed.bin')")?;
    probe.wait("rename", "client.bin")?;
    probe.wait("renamed", "renamed.bin")?;
    probe.wait("delete", "renamed.bin")?;
    probe.wait("deleted", "renamed.bin")?;
    Ok(())
}

// Explicit opt-in: waiting for the native timeout takes about a minute.
pub fn callback_cancellation_timeout() -> Result<(), Failed> {
    callback_cancellation_timeout_inner().map_err(Into::into)
}

fn callback_cancellation_timeout_inner() -> anyhow::Result<()> {
    let root = Root::new()?;
    root.register(false)?;
    root.file("pending.bin")?;
    PlaceholderCreateInfo::new("pending-dir")
        .metadata(Metadata::directory())
        .blob(b"pending-dir".to_vec())
        .create(&root.path)?;
    let probe = Probe::default();
    let _connection = Session::new().connect(&root.path, probe.clone())?;
    let mut clients = Vec::new();
    for (name, script, fetch, cancel) in [
        ("pending.bin", "$ErrorActionPreference='Stop'; [IO.File]::ReadAllBytes((Join-Path $env:CF_PROBE_ROOT 'pending.bin')) | Out-Null", "fetch_data", "cancel_fetch_data"),
        ("pending-dir", "$ErrorActionPreference='Stop'; Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'pending-dir') | Out-Null", "fetch_placeholders", "cancel_fetch_placeholders"),
    ] {
        clients.push(Client::start(&root, script)?);
        probe.wait(fetch, name)?;
        println!("waiting for native {cancel} for {name}");
    }
    let start = Instant::now();
    probe.wait_for("cancel_fetch_data", "pending.bin", Duration::from_secs(80))?;
    probe.wait_for(
        "cancel_fetch_placeholders",
        "pending-dir",
        Duration::from_secs(5),
    )?;
    println!("cancellation callbacks after {:?}", start.elapsed());
    Ok(())
}

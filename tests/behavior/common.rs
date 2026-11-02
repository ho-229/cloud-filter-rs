//! Isolated native roots and bounded external client processes for behavior tests.
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::ensure;
use cloud_filter::{
    metadata::Metadata,
    placeholder::Placeholder,
    placeholder_create_info::PlaceholderCreateInfo,
    root::{
        HydrationPolicy, HydrationType, PopulationType, SecurityId, SyncRootId, SyncRootIdBuilder,
        SyncRootInfo,
    },
};
use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

pub(super) struct Root {
    pub path: PathBuf,
    pub id: SyncRootId,
}

impl Root {
    pub fn new() -> anyhow::Result<Self> {
        let name = format!(
            "cf-behavior-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        // On the tested host WinRT rejected roots under AppData/Temp, while
        // native CF registration succeeded. Keep these probes outside it.
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

    pub fn register(&self, validation: bool) -> anyhow::Result<()> {
        self.register_with_hydration(HydrationType::Partial, validation)
    }

    pub fn register_with_hydration(
        &self,
        hydration: HydrationType,
        validation: bool,
    ) -> anyhow::Result<()> {
        self.id.register(
            SyncRootInfo::default()
                .with_display_name("Cloud Filter Behavior Test")
                .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
                .with_version("1.0.0")
                .with_hydration_type(hydration)
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

    pub fn file(&self, name: &str) -> anyhow::Result<()> {
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

pub(super) fn open(path: &Path) -> anyhow::Result<Placeholder> {
    Ok(OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?
        .into())
}

// Bound subprocess lifetime, including on assertion failure; scripts operate only on this root.
pub(super) struct Client(Child);
impl Client {
    pub fn start(root: &Root, script: &str) -> anyhow::Result<Self> {
        Ok(Self(
            Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("CF_PROBE_ROOT", &root.path)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        ))
    }
    pub fn run(root: &Root, script: &str) -> anyhow::Result<()> {
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

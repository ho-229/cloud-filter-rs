use super::*;
use cloud_filter::root::SyncRootId;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

// Every test owns a fresh directory and registration, never the user's SERVER/CLIENT.
struct TestRoot {
    path: PathBuf,
    registration: SyncRootId,
}

impl TestRoot {
    fn new() -> anyhow::Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = env::temp_dir().join(format!("cloud-mirror-{}-{stamp}", std::process::id()));
        fs::create_dir(&path)?;
        let root = Self {
            path: canonical_path(&path)?,
            registration: SyncRootIdBuilder::new(format!("CloudMirrorTest{stamp}"))
                .user_security_id(SecurityId::current_user()?)
                .build(),
        };
        fs::create_dir(root.path.join("server"))?;
        fs::create_dir(root.path.join("client"))?;
        register_root(
            &root.registration,
            &root.path.join("client"),
            root.path.join("server").as_os_str().as_encoded_bytes(),
        )?;
        Ok(root)
    }

    fn mirror(&self) -> CloudMirror {
        self.mirror_with_readonly(false)
    }

    fn mirror_with_readonly(&self, readonly: bool) -> CloudMirror {
        CloudMirror::new(self.path.join("client"), self.path.join("server"), readonly)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = self.registration.unregister();
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[path = "../common/behavior.rs"]
mod behavior;

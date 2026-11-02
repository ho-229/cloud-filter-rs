use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

struct RegistrationRoot {
    path: PathBuf,
    id: SyncRootId,
}

impl RegistrationRoot {
    fn new() -> anyhow::Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let name = format!("SftpTest{}-{stamp}", std::process::id());
        let path = env::current_dir()?.join("target").join(&name);
        fs::create_dir(&path)?;
        Ok(Self {
            path: canonical_path(&path)?,
            id: SyncRootIdBuilder::new(name)
                .user_security_id(SecurityId::current_user()?)
                .build(),
        })
    }
}

impl Drop for RegistrationRoot {
    fn drop(&mut self) {
        let _ = self.id.unregister();
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn registration_binds_legacy_source_and_preserves_files() -> anyhow::Result<()> {
    let root = RegistrationRoot::new()?;
    let source = b"localhost:2222\0test\0/";
    register_root(&root.id, &root.path, source)?;
    assert_eq!(root.id.info()?.blob(), source);
    root.id.unregister()?;

    let local_file = root.path.join("local.txt");
    fs::write(&local_file, b"local edits")?;

    // A fresh registration rejects local contents; an existing registration may retain them.
    assert!(register_root(&root.id, &root.path, source).is_err());
    assert!(!root.id.is_registered()?);
    root.id.register(root_info(&root.path)?)?;
    PlaceholderCreateInfo::new("remote.bin")
        .metadata(Metadata::file().size(8192))
        .mark_in_sync()
        .blob(b"/remote.bin".to_vec())
        .create(&root.path)?;

    register_root(&root.id, &root.path, source)?;
    assert_eq!(root.id.info()?.blob(), source);
    register_root(&root.id, &root.path, source)?;
    for other in [
        b"localhost:2223\0test\0/".as_slice(),
        b"localhost:2222\0other\0/".as_slice(),
        b"localhost:2222\0test\0/other".as_slice(),
    ] {
        assert!(register_root(&root.id, &root.path, other).is_err());
        assert_eq!(root.id.info()?.blob(), source);
    }
    assert_eq!(fs::read(&local_file)?, b"local edits");

    // Inspect metadata without hydrating the offline file during migration or restart.
    use std::os::windows::fs::OpenOptionsExt;
    let placeholder = cloud_filter::placeholder::Placeholder::from(
        fs::OpenOptions::new()
            .access_mode(0)
            .open(root.path.join("remote.bin"))?,
    );
    let info = placeholder.info()?.context("placeholder info")?;
    assert_eq!(info.on_disk_data_size(), 0);
    assert_eq!(info.blob(), b"/remote.bin");
    Ok(())
}

// The same behavior cases run against local storage and an isolated SFTPGo process.
struct TestRoot {
    path: PathBuf,
    registration: SyncRootId,
    server: Option<std::process::Child>,
    sftp: Option<Arc<Sftp>>,
}

impl TestRoot {
    fn new() -> anyhow::Result<Self> {
        use std::{
            net::TcpListener,
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let name = format!("SftpBehavior{}-{stamp}", std::process::id());
        let path = env::current_dir()?.join("target").join(&name);
        fs::create_dir(&path)?;
        let mut root = Self {
            path: canonical_path(&path)?,
            registration: SyncRootIdBuilder::new(name)
                .user_security_id(SecurityId::current_user()?)
                .build(),
            server: None,
            sftp: None,
        };
        fs::create_dir(root.path.join("server"))?;
        fs::create_dir(root.path.join("client"))?;
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let address = format!("127.0.0.1:{port}");
        let binary = env::var_os("SFTPGO_TEST_EXE")
            .map(PathBuf::from)
            .unwrap_or(env::current_dir()?.join("target/sftpgo-2.7.6/sftpgo.exe"));
        let log = File::create(root.path.join("sftpgo.log"))?;
        root.server = Some(
            Command::new(binary)
                .args([
                    "portable",
                    "--username",
                    "test",
                    "--password",
                    "test",
                    "--sftpd-port",
                    &port.to_string(),
                    "--permissions",
                    "*",
                ])
                .arg("--directory")
                .arg(root.path.join("server"))
                .arg("--config-dir")
                .arg(&root.path)
                .env("SFTPGO_SFTPD__BINDINGS__0__ADDRESS", "127.0.0.1")
                .current_dir(&root.path)
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()
                .context("start SFTPGo (set SFTPGO_TEST_EXE to its executable)")?,
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match connect(&address, "test", "test") {
                Ok(sftp) => {
                    root.sftp = Some(Arc::new(sftp));
                    break;
                }
                Err(error) => {
                    ensure!(
                        root.server.as_mut().unwrap().try_wait()?.is_none()
                            && Instant::now() < deadline,
                        "SFTPGo startup: {error:#}\n{}",
                        fs::read_to_string(root.path.join("sftpgo.log"))?
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let source = format!("{address}\0test\0/");
        register_root(
            &root.registration,
            &root.path.join("client"),
            source.as_bytes(),
        )?;
        Ok(root)
    }

    fn mirror(&self) -> Filter {
        self.mirror_with_readonly(false)
    }

    fn mirror_with_readonly(&self, readonly: bool) -> Filter {
        Filter::new(
            self.sftp.as_ref().unwrap().clone(),
            self.path.join("client"),
            PathBuf::from("/"),
            readonly,
        )
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = self.registration.unregister();
        self.sftp.take();
        if let Some(server) = &mut self.server {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[path = "../common/behavior.rs"]
mod behavior;

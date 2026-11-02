// Included by both examples' test modules, each with its own backend-specific TestRoot.
use super::*;
use cloud_filter::placeholder::ReadType;
use std::{
    io::Write,
    os::windows::io::AsRawHandle,
    time::{Duration, Instant},
};

fn disk_size(path: &Path) -> anyhow::Result<i64> {
    Ok(open_placeholder(path)?
        .info()?
        .context("placeholder info")?
        .on_disk_data_size())
}

fn wait_for_disk_size(path: &Path, expected: i64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = disk_size(path)?;
        if actual == expected {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "on-disk size remained {actual}, expected {expected}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Match the examples' startup: watch first, then discover offline changes.
fn connect<F: SyncFilter + 'static>(
    client: &Path,
    filter: F,
) -> anyhow::Result<cloud_filter::root::Connection<F>> {
    let connection = Session::new().connect(client, filter)?;
    connection
        .filter()
        .state_changed(StateWatchEvent::Changed(vec![client.to_owned()]));
    Ok(connection)
}

fn populate(path: &Path) -> anyhow::Result<()> {
    // A provider's own directory enumeration does not trigger population.
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference = 'Stop'; Get-ChildItem -LiteralPath $env:CLOUD_MIRROR_TEST_ROOT -Recurse | Out-Null"])
        .env("CLOUD_MIRROR_TEST_ROOT", path)
        .output()?;
    ensure!(
        output.status.success(),
        "populate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn client_command(client: &Path, command: &str, name: &str) -> anyhow::Result<()> {
    let binary = env::current_exe()?.with_file_name(format!("cf-cli{}", env::consts::EXE_SUFFIX));
    let output = std::process::Command::new(binary)
        .arg(command)
        .arg(client.join(name))
        .output()
        .context("run cf-cli (build it with cargo build --example cf-cli first)")?;
    ensure!(
        output.status.success(),
        "cf-cli {command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn client_action(client: &Path, command: &str, name: &str, content: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new(env::current_exe()?)
        .args([
            "--exact",
            "tests::behavior::command_client",
            "--ignored",
            "--nocapture",
        ])
        .env("CLOUD_MIRROR_TEST_ROOT", client)
        .env("CLOUD_MIRROR_TEST_COMMAND", command)
        .env("CLOUD_MIRROR_TEST_FILE", name)
        .env("CLOUD_MIRROR_TEST_CONTENT", content)
        .output()?;
    ensure!(
        output.status.success(),
        "client {command}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
#[ignore = "subprocess entry point for client filesystem operations"]
fn command_client() -> anyhow::Result<()> {
    let client = PathBuf::from(env::var_os("CLOUD_MIRROR_TEST_ROOT").context("test client root")?);
    let name = PathBuf::from(env::var_os("CLOUD_MIRROR_TEST_FILE").context("test client file")?);
    let command = env::var("CLOUD_MIRROR_TEST_COMMAND")?;
    match command.as_str() {
        "write" => {
            fs::write(client.join(name), env::var("CLOUD_MIRROR_TEST_CONTENT")?)?;
            Ok(())
        }
        "append" => {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(client.join(name))?;
            file.write_all(env::var("CLOUD_MIRROR_TEST_CONTENT")?.as_bytes())?;
            file.sync_all()?;
            Ok(())
        }
        "rename" => {
            fs::rename(
                client.join(name),
                client.join(env::var("CLOUD_MIRROR_TEST_CONTENT")?),
            )?;
            Ok(())
        }
        "delete" => {
            fs::remove_file(client.join(name))?;
            Ok(())
        }
        "delete-dir" => {
            fs::remove_dir_all(client.join(name))?;
            Ok(())
        }
        "rename-collision" => {
            use windows::{core::PCWSTR, Win32::Storage::FileSystem::MoveFileW};
            let source = widestring::U16CString::from_os_str(client.join(name))?;
            let target = widestring::U16CString::from_os_str(client.join("occupied.txt"))?;
            let error = unsafe { MoveFileW(PCWSTR(source.as_ptr()), PCWSTR(target.as_ptr())) }
                .expect_err("destination introduced during callback must prevent rename");
            ensure!(
                matches!(error.code().0 & 0xffff, 80 | 183),
                "unexpected rename failure: {error}"
            );
            Ok(())
        }
        "undelete" | "undelete-ex" => {
            use windows::Win32::{Foundation::HANDLE, Storage::FileSystem::*};
            let file = fs::OpenOptions::new()
                .access_mode(DELETE.0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                .open(client.join(name))?;
            for deleting in [true, false] {
                unsafe {
                    if command == "undelete-ex" {
                        let value = FILE_DISPOSITION_INFO_EX {
                            Flags: if deleting {
                                FILE_DISPOSITION_FLAG_DELETE
                            } else {
                                FILE_DISPOSITION_FLAG_DO_NOT_DELETE
                            },
                        };
                        SetFileInformationByHandle(
                            HANDLE(file.as_raw_handle()),
                            FileDispositionInfoEx,
                            (&value as *const FILE_DISPOSITION_INFO_EX).cast(),
                            std::mem::size_of_val(&value) as u32,
                        )?;
                    } else {
                        let value = FILE_DISPOSITION_INFO {
                            DeleteFile: deleting.into(),
                        };
                        SetFileInformationByHandle(
                            HANDLE(file.as_raw_handle()),
                            FileDispositionInfo,
                            (&value as *const FILE_DISPOSITION_INFO).cast(),
                            std::mem::size_of_val(&value) as u32,
                        )?;
                    }
                }
                // Let the worker observe DELETE_COMPLETION while the application
                // can still undo its disposition. The remote copy must survive.
                std::thread::sleep(Duration::from_millis(400));
            }
            Ok(())
        }
        "delete-denied" | "rename-denied" => {
            let result = if command == "delete-denied" {
                fs::remove_file(client.join(name))
            } else {
                fs::rename(client.join(name), client.join("renamed.txt"))
            };
            let error = result.expect_err("readonly must reject the operation");
            assert_eq!(
                error.raw_os_error(),
                Some(windows::Win32::Foundation::ERROR_CLOUD_FILE_ACCESS_DENIED.0 as i32)
            );
            Ok(())
        }
        _ => anyhow::bail!("unknown test action: {command}"),
    }
}

fn wait_for_upload(client: &Path, server: &Path, content: &[u8]) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let in_sync = open_placeholder(client)?
            .info()?
            .is_some_and(|info| info.is_in_sync() && info.modified_data_size() == 0);
        if in_sync && fs::read(server).is_ok_and(|data| data == content) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "upload did not complete for {} (in_sync={in_sync})",
            client.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hydrate_dehydrate_and_restart() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let mirror = root.mirror();
    fs::create_dir(root.path.join("server").join("nested"))?;
    let cases = [
        ("empty.txt", Vec::new()),
        ("small.txt", b"unaligned EOF".to_vec()),
        (
            "nested/中文 file.bin",
            (0..CHUNK_SIZE_BYTES * 128 + 17)
                .map(|n| (n % 251) as u8)
                .collect(),
        ),
    ];
    for (name, data) in &cases {
        fs::write(root.path.join("server").join(name), data)?;
    }
    let client = mirror.files.client_path.clone();
    let connection = connect(&client, mirror)?;
    // Exercise population callbacks, including names relative to a nested directory.
    populate(&client)?;
    for (name, data) in &cases {
        let path = client.join(name);
        assert_eq!(fs::metadata(&path)?.len(), data.len() as u64);
        assert_eq!(disk_size(&path)?, 0);
        if !data.is_empty() {
            let state = open_placeholder(&path)?
                .state()?
                .context("dehydrated state")?;
            assert!(state.partial() && state.partial_on_disk() && state.in_sync());
        }
        client_command(&client, "status", name)?;
        assert_eq!(disk_size(&path)?, 0, "status must not hydrate the file");
        client_command(&client, "hydrate", name).with_context(|| format!("hydrate {name}"))?;
        if !data.is_empty() {
            assert!(disk_size(&path)? > 0);
            let state = open_placeholder(&path)?
                .state()?
                .context("hydrated state")?;
            assert!(!state.partial() && !state.partial_on_disk() && state.in_sync());
        }
        assert_eq!(fs::read(&path)?, *data);
        client_command(&client, "dehydrate", name).with_context(|| format!("dehydrate {name}"))?;
        assert_eq!(disk_size(&path)?, 0);
        assert_eq!(fs::metadata(&path)?.len(), data.len() as u64);
        if !data.is_empty() {
            assert!(open_placeholder(&path)?.state()?.unwrap().partial_on_disk());
        }
    }
    drop(connection);
    // Re-registering policies on restart must retain dehydrated placeholders.
    let registration = &root.registration;
    registration.register(root_info(&client)?)?;
    for (name, _) in &cases {
        assert_eq!(disk_size(&client.join(name))?, 0);
    }
    let _connection = connect(&client, root.mirror())?;
    for (name, data) in &cases {
        // Ordinary reads must hydrate again after reconnecting.
        assert_eq!(fs::read(client.join(name))?, *data);
    }
    Ok(())
}

#[test]
fn dehydration_preserves_pinned_and_modified_data() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let mirror = root.mirror();
    fs::write(
        root.path.join("server").join("file.txt"),
        b"original contents",
    )?;
    let client = mirror.files.client_path.clone();
    let _connection = connect(&client, mirror)?;
    populate(&client)?;
    let name = Path::new("file.txt");
    client_command(&client, "pin", "file.txt")?;
    wait_for_disk_size(&client.join(name), 17)?;
    assert!(client_command(&client, "dehydrate", "file.txt").is_err());
    assert_eq!(fs::read(client.join(name))?, b"original contents");
    client_command(&client, "unpin", "file.txt")?;
    wait_for_disk_size(&client.join(name), 0)?;
    client_command(&client, "hydrate", "file.txt")?;
    // Keep a writer open so the background uploader cannot finish before the assertion.
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(client.join(name))?;
    writer.write_all(b"local edits must survive")?;
    writer.sync_all()?;
    // A no-access handle can inspect state while a writer remains open.
    let placeholder = open_placeholder(&client.join(name))?;
    assert!(!placeholder.info()?.unwrap().is_in_sync());
    // The explicit dehydration command will reject the open file or its dirty state.
    assert!(client_command(&client, "dehydrate", "file.txt").is_err());
    assert_eq!(fs::read(client.join(name))?, b"local edits must survive");
    Ok(())
}

#[test]
fn local_writes_and_new_files_round_trip() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("edit.txt"), b"old value")?;
    let _connection = connect(&client, root.mirror())?;
    populate(&client)?;
    client_command(&client, "hydrate", "edit.txt")?;
    for content in ["new value", "a longer edit, with an unaligned EOF", ""] {
        client_action(&client, "write", "edit.txt", content)?;
        wait_for_upload(
            &client.join("edit.txt"),
            &server.join("edit.txt"),
            content.as_bytes(),
        )?;
        client_command(&client, "dehydrate", "edit.txt")?;
        assert_eq!(disk_size(&client.join("edit.txt"))?, 0);
        client_command(&client, "hydrate", "edit.txt")?;
        assert_eq!(fs::read(client.join("edit.txt"))?, content.as_bytes());
    }
    client_action(&client, "append", "edit.txt", "append to the empty file")?;
    wait_for_upload(
        &client.join("edit.txt"),
        &server.join("edit.txt"),
        b"append to the empty file",
    )?;
    fs::create_dir_all(client.join("new/nested"))?;
    for (name, content) in [
        ("new/nested/中文.txt", "new local file"),
        ("new/empty.txt", ""),
    ] {
        client_action(&client, "write", name, content)?;
        wait_for_upload(&client.join(name), &server.join(name), content.as_bytes())?;
        client_command(&client, "dehydrate", name)?;
        assert_eq!(disk_size(&client.join(name))?, 0);
        client_command(&client, "hydrate", name)?;
        assert_eq!(fs::read(client.join(name))?, content.as_bytes());
    }
    Ok(())
}

#[test]
fn modified_ranges_drive_incremental_uploads() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let mirror = root.mirror();
    let files = mirror.files.clone();
    let client = files.client_path.join("ranges.bin");
    let server = root.path.join("server").join("ranges.bin");
    // More than one page of separated dirty ranges, beyond the old API's buffer-sized query.
    let stride = CHUNK_SIZE_BYTES * 2;
    let mut expected = vec![0x5a; stride * 70 + 17];
    fs::write(&server, &expected)?;
    let connection = connect(&files.client_path, mirror)?;
    populate(&files.client_path)?;
    client_command(&files.client_path, "hydrate", "ranges.bin")?;
    // Run uploads explicitly so the background worker cannot consume the ranges
    // before we inspect them. All data was hydrated while connected.
    drop(connection);
    let placeholder = open_placeholder(&client)?;
    assert!(placeholder.ranges(ReadType::Modified, ..)?.is_empty());
    assert_eq!(placeholder.ranges(ReadType::Any, 7..23)?, vec![7..23]);
    assert_eq!(placeholder.ranges(ReadType::Validated, 7..23)?, vec![7..23]);
    assert!(placeholder.ranges(ReadType::Any, ..0)?.is_empty());
    assert!(placeholder.ranges(ReadType::Any, u64::MAX..).is_err());
    assert!(placeholder
        .ranges(ReadType::Any, expected.len() as u64..)?
        .is_empty());
    let mut writer = fs::OpenOptions::new().write(true).open(&client)?;
    for index in 0..70 {
        let offset = index * stride + 13;
        writer.seek(SeekFrom::Start(offset as u64))?;
        writer.write_all(b"patch")?;
        expected[offset..offset + 5].copy_from_slice(b"patch");
    }
    writer.sync_all()?;
    drop(writer);
    let modified = placeholder.ranges(ReadType::Modified, ..expected.len() as u64)?;
    assert!(
        modified.len() > 64,
        "exercise native pagination: {modified:?}"
    );
    for index in 0..70 {
        let offset = (index * stride + 13) as u64;
        assert!(modified.iter().any(|range| range.contains(&offset)));
    }
    let dirty_bytes: u64 = modified.iter().map(|range| range.end - range.start).sum();
    assert!(dirty_bytes < expected.len() as u64);
    assert_eq!(files.upload(&client)?, Some(dirty_bytes));
    assert_eq!(fs::read(&server)?, expected);
    assert!(placeholder.ranges(ReadType::Modified, ..)?.is_empty());
    drop(placeholder);

    // A dirty state does not necessarily imply changed content ranges.
    open_placeholder(&client)?.mark_in_sync(false, None)?;
    assert_eq!(files.upload(&client)?, Some(0));
    assert_eq!(fs::read(&server)?, expected);

    let tail = b"appended past EOF";
    let mut writer = fs::OpenOptions::new().append(true).open(&client)?;
    writer.write_all(tail)?;
    writer.sync_all()?;
    drop(writer);
    expected.extend_from_slice(tail);
    let uploaded = files.upload(&client)?.context("append upload")?;
    assert!(uploaded >= tail.len() as u64 && uploaded < expected.len() as u64);
    assert_eq!(fs::read(&server)?, expected);

    let writer = fs::OpenOptions::new().write(true).open(&client)?;
    let shorter = stride + 19;
    writer.set_len(shorter as u64)?;
    writer.sync_all()?;
    drop(writer);
    expected.truncate(shorter);
    files.upload(&client)?.context("truncate upload")?;
    assert_eq!(fs::read(&server)?, expected);

    let writer = fs::OpenOptions::new().write(true).open(&client)?;
    writer.set_len((shorter + stride) as u64)?;
    writer.sync_all()?;
    drop(writer);
    expected.resize(shorter + stride, 0);
    files
        .upload(&client)?
        .context("zero-filled extension upload")?;
    assert_eq!(fs::read(&server)?, expected);

    // Shrink and regrow before uploading: the final size alone cannot expose
    // the discarded data, and old server bytes must not survive in the gap.
    let writer = fs::OpenOptions::new().write(true).open(&client)?;
    writer.set_len(4003)?;
    writer.set_len(expected.len() as u64)?;
    writer.sync_all()?;
    drop(writer);
    expected[4003..].fill(0);
    files
        .upload(&client)?
        .context("truncate and regrow upload")?;
    assert_eq!(fs::read(&server)?, expected);

    let writer = fs::OpenOptions::new().write(true).open(&client)?;
    writer.set_len(0)?;
    writer.sync_all()?;
    drop(writer);
    assert_eq!(files.upload(&client)?, Some(0));
    assert!(fs::read(&server)?.is_empty());
    assert!(open_placeholder(&client)?
        .ranges(ReadType::Modified, ..)?
        .is_empty());
    Ok(())
}

#[test]
fn incremental_upload_falls_back_when_server_base_is_missing() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let mirror = root.mirror();
    let files = mirror.files.clone();
    let client = files.client_path.join("missing.bin");
    let server = root.path.join("server").join("missing.bin");
    let mut expected = vec![1; CHUNK_SIZE_BYTES * 4];
    fs::write(&server, &expected)?;
    let connection = connect(&files.client_path, mirror)?;
    populate(&files.client_path)?;
    client_command(&files.client_path, "hydrate", "missing.bin")?;
    drop(connection);
    let mut writer = fs::OpenOptions::new().write(true).open(&client)?;
    writer.seek(SeekFrom::Start(123))?;
    writer.write_all(b"edit")?;
    writer.sync_all()?;
    drop(writer);
    expected[123..127].copy_from_slice(b"edit");
    fs::remove_file(&server)?;
    assert_eq!(files.upload(&client)?, Some(expected.len() as u64));
    assert_eq!(fs::read(&server)?, expected);
    Ok(())
}

#[test]
fn writeback_waits_for_writer_and_retries_publish_failure() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("edit.txt"), b"original")?;
    let _connection = connect(&client, root.mirror())?;
    populate(&client)?;
    client_command(&client, "hydrate", "edit.txt")?;
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(client.join("edit.txt"))?;
    writer.write_all(b"first part")?;
    writer.sync_all()?;
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(fs::read(server.join("edit.txt"))?, b"original");
    assert!(!open_placeholder(&client.join("edit.txt"))?
        .info()?
        .unwrap()
        .is_in_sync());
    writer.write_all(b" and final part")?;
    writer.sync_all()?;
    drop(writer);
    wait_for_upload(
        &client.join("edit.txt"),
        &server.join("edit.txt"),
        b"first part and final part",
    )?;

    // Hold the server file open without delete sharing so replacement must fail.
    let server_reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
        .open(server.join("edit.txt"))?;
    client_action(&client, "write", "edit.txt", "retry after server unlocks")?;
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        fs::read(server.join("edit.txt"))?,
        b"first part and final part"
    );
    assert!(!open_placeholder(&client.join("edit.txt"))?
        .info()?
        .unwrap()
        .is_in_sync());
    assert!(client_command(&client, "dehydrate", "edit.txt").is_err());
    drop(server_reader);
    wait_for_upload(
        &client.join("edit.txt"),
        &server.join("edit.txt"),
        b"retry after server unlocks",
    )?;
    assert!(!fs::read_dir(&server)?.any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(writeback::STAGING_PREFIX)));
    Ok(())
}

#[test]
fn reconnect_uploads_offline_edits() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("edit.txt"), b"original")?;
    let connection = connect(&client, root.mirror())?;
    populate(&client)?;
    client_command(&client, "hydrate", "edit.txt")?;
    drop(connection);
    fs::write(client.join("edit.txt"), b"offline edit")?;
    fs::write(
        client.join("offline-new.txt"),
        b"created while disconnected",
    )?;
    let _connection = connect(&client, root.mirror())?;
    wait_for_upload(
        &client.join("edit.txt"),
        &server.join("edit.txt"),
        b"offline edit",
    )?;
    wait_for_upload(
        &client.join("offline-new.txt"),
        &server.join("offline-new.txt"),
        b"created while disconnected",
    )?;
    Ok(())
}

#[test]
fn readonly_preserves_server_and_local_edits() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("edit.txt"), b"original")?;
    fs::write(server.join("pin.bin"), vec![1; 8192])?;
    let mirror = root.mirror_with_readonly(true);
    let connection = connect(&client, mirror)?;
    populate(&client)?;
    client_command(&client, "hydrate", "edit.txt")?;
    assert_eq!(fs::read(client.join("edit.txt"))?, b"original");
    // Use another process so Windows delivers the delete/rename callbacks.
    client_action(&client, "delete-denied", "edit.txt", "")?;
    client_action(&client, "rename-denied", "edit.txt", "")?;
    client_action(&client, "write", "edit.txt", "local edits")?;
    client_action(&client, "write", "new.txt", "local new file")?;
    fs::create_dir_all(client.join("local/empty"))?;

    // Pin handling still runs even though uploads are disabled.
    client_command(&client, "pin", "pin.bin")?;
    wait_for_disk_size(&client.join("pin.bin"), 8192)?;
    client_command(&client, "unpin", "pin.bin")?;
    wait_for_disk_size(&client.join("pin.bin"), 0)?;
    client_command(&client, "unpin", "edit.txt")?;
    assert!(client_command(&client, "dehydrate", "edit.txt").is_err());
    drop(connection);

    let connection = connect(&client, root.mirror_with_readonly(true))?;
    client_command(&client, "pin", "pin.bin")?;
    wait_for_disk_size(&client.join("pin.bin"), 8192)?;
    assert_eq!(fs::read(server.join("edit.txt"))?, b"original");
    assert!(!server.join("new.txt").exists());
    assert!(!server.join("local").exists());
    assert!(open_placeholder(&client.join("local"))?.info()?.is_none());
    assert!(!server.join("renamed.txt").exists());
    assert_eq!(fs::read(client.join("edit.txt"))?, b"local edits");
    assert_eq!(fs::read(client.join("new.txt"))?, b"local new file");
    assert!(open_placeholder(&client.join("new.txt"))?.info()?.is_none());
    let info = open_placeholder(&client.join("edit.txt"))?
        .info()?
        .context("edited placeholder")?;
    assert!(!info.is_in_sync() || info.modified_data_size() > 0);
    drop(connection);

    // Returning to writable mode can publish the retained local changes.
    let _connection = connect(&client, root.mirror())?;
    wait_for_upload(
        &client.join("edit.txt"),
        &server.join("edit.txt"),
        b"local edits",
    )?;
    wait_for_upload(
        &client.join("new.txt"),
        &server.join("new.txt"),
        b"local new file",
    )?;
    wait_for_directory_upload(&client.join("local/empty"), &server.join("local/empty"))?;
    Ok(())
}

#[test]
fn local_directories_become_synced_placeholders() -> anyhow::Result<()> {
    struct IdleFilter;
    impl SyncFilter for IdleFilter {
        fn fetch_data(&self, _: Request, _: ticket::FetchData, _: info::FetchData) -> CResult<()> {
            Err(CloudErrorKind::NotSupported)
        }

        fn fetch_placeholders(
            &self,
            _: Request,
            ticket: ticket::FetchPlaceholders,
            _: info::FetchPlaceholders,
        ) -> CResult<()> {
            ticket
                .pass_with_placeholder(&mut [])
                .map_err(|_| CloudErrorKind::Unsuccessful)
        }
    }

    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    let mirror = root.mirror();
    let files = mirror.files.clone();
    drop(mirror);
    let idle = connect(&client, IdleFilter)?;
    populate(&client)?;
    let directory = "local/nested/tool/package/data";
    let file = "local/nested/tool/package/data/file.txt";
    fs::create_dir_all(client.join(directory))?;
    fs::write(client.join(file), b"local contents")?;
    files
        .upload(&client.join(file))?
        .context("initial file upload")?;
    assert!(open_placeholder(&client.join("local"))?.info()?.is_none());
    drop(idle);
    // A synced child does not turn ordinary parent directories into placeholders.
    let mirror = root.mirror();
    let connection = connect(&client, mirror)?;
    for name in Path::new(directory)
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
    {
        wait_for_directory_upload(&client.join(name), &server.join(name))?;
    }
    assert_eq!(fs::read(client.join(file))?, b"local contents");
    client_command(&client, "unpin", file)?;
    wait_for_disk_size(&client.join(file), 0)?;
    client_action(&client, "delete-dir", "local", "")?;
    assert!(!client.join("local").exists());
    wait_for_server_path(&server.join("local"), false)?;
    drop(connection);

    // Startup reconciliation also publishes directories with no files beneath them.
    fs::create_dir_all(client.join("empty/nested"))?;
    let _connection = connect(&client, root.mirror())?;
    for name in ["empty", "empty/nested"] {
        wait_for_directory_upload(&client.join(name), &server.join(name))?;
    }
    Ok(())
}

fn wait_for_directory_upload(client: &Path, server: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = open_placeholder(client)?.info()?;
        if info.as_ref().is_some_and(|info| info.is_in_sync()) && server.is_dir() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "directory {} did not sync: {info:?}",
            client.display(),
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shortened_source_fails_hydration() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let mirror = root.mirror();
    fs::write(
        root.path.join("server").join("file.bin"),
        vec![1; CHUNK_SIZE_BYTES * 2],
    )?;
    let client = mirror.files.client_path.clone();
    let server = root.path.join("server");
    let _connection = connect(&client, mirror)?;
    populate(&client)?;
    fs::write(server.join("file.bin"), b"short")?;
    assert!(client_command(&client, "hydrate", "file.bin").is_err());
    assert_eq!(disk_size(&client.join("file.bin"))?, 0);
    Ok(())
}

#[test]
fn deletion_succeeds_when_server_entries_are_already_missing() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::create_dir_all(server.join("folder/nested"))?;
    fs::write(server.join("file.txt"), b"file contents")?;
    fs::write(server.join("folder/nested/child.txt"), b"child contents")?;
    fs::write(server.join("next.txt"), b"next operation")?;
    let _connection = connect(&client, root.mirror())?;
    populate(&client)?;

    // A prior delete can remove the remote tree before local deletion finishes.
    // Missing remote files and directories must not block later remote work,
    // including child deletes after their remote parent has been removed.
    fs::remove_file(server.join("file.txt"))?;
    fs::remove_dir_all(server.join("folder"))?;
    client_action(&client, "delete", "file.txt", "")?;
    client_action(&client, "delete-dir", "folder", "")?;
    assert!(!client.join("file.txt").exists());
    assert!(!client.join("folder").exists());
    // Local success alone says nothing about the asynchronous remote deletes.
    // A subsequent rename must make it through the same ordered command queue.
    client_action(&client, "rename", "next.txt", "renamed.txt")?;
    wait_for_server_path(&server.join("renamed.txt"), true)?;
    assert_eq!(fs::read(server.join("renamed.txt"))?, b"next operation");
    Ok(())
}

fn wait_for_server_path(path: &Path, exists: bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.try_exists()? != exists {
        ensure!(
            Instant::now() < deadline,
            "remote path did not reach exists={exists}: {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn undo_delete_preserves_files_and_directories() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("file.txt"), b"keep these contents")?;
    fs::create_dir(server.join("folder"))?;
    let mirror = root.mirror();
    let _connection = connect(&client, mirror)?;
    populate(&client)?;
    for command in ["undelete", "undelete-ex"] {
        for name in ["file.txt", "folder"] {
            client_action(&client, command, name, "")?;
            std::thread::sleep(Duration::from_millis(300)); // Let the queued directory check run.
            assert!(server.join(name).exists());
            assert!(client.join(name).exists());
        }
    }
    // Hydrate after undo: the remote copy was the only copy of these bytes.
    client_command(&client, "hydrate", "file.txt")?;
    assert_eq!(fs::read(client.join("file.txt"))?, b"keep these contents");
    Ok(())
}

#[test]
fn namespace_retry_keeps_renamed_content_readable() -> anyhow::Result<()> {
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("file.txt"), b"remote contents")?;
    fs::create_dir(server.join("folder"))?;
    fs::write(server.join("folder/child.txt"), b"nested contents")?;
    let mirror = root.mirror();
    let _connection = connect(&client, mirror)?;
    populate(&client)?;
    // Deny remote rename/delete while permitting the local CF operation.
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(server.join("file.txt"))?;
    client_action(&client, "rename", "file.txt", "renamed.txt")?;
    std::thread::sleep(Duration::from_millis(600));
    assert!(server.join("file.txt").exists());
    assert!(!server.join("renamed.txt").exists());
    // While the remote rename is pending, downloads use the old location.
    client_command(&client, "hydrate", "renamed.txt")?;
    assert_eq!(fs::read(client.join("renamed.txt"))?, b"remote contents");
    drop(lock);
    wait_for_server_path(&server.join("renamed.txt"), true)?;
    assert_eq!(fs::read(server.join("renamed.txt"))?, b"remote contents");
    assert!(!server.join("file.txt").exists());
    // Once renamed remotely, downloads must stop using the old location.
    client_command(&client, "dehydrate", "renamed.txt")?;
    client_command(&client, "hydrate", "renamed.txt")?;
    assert_eq!(fs::read(client.join("renamed.txt"))?, b"remote contents");

    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(server.join("renamed.txt"))?;
    client_action(&client, "delete", "renamed.txt", "")?;
    std::thread::sleep(Duration::from_millis(600));
    assert!(!client.join("renamed.txt").exists());
    assert!(server.join("renamed.txt").exists());
    drop(lock);
    wait_for_server_path(&server.join("renamed.txt"), false)?;

    // A directory mapping must also serve downloads beneath the new prefix.
    let lock = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .share_mode(1)
        .open(server.join("folder"))?;
    client_action(&client, "rename", "folder", "renamed-folder")?;
    std::thread::sleep(Duration::from_millis(600));
    assert!(server.join("folder").exists());
    assert!(!server.join("renamed-folder").exists());
    client_command(&client, "hydrate", "renamed-folder/child.txt")?;
    assert_eq!(
        fs::read(client.join("renamed-folder/child.txt"))?,
        b"nested contents"
    );
    drop(lock);
    wait_for_server_path(&server.join("renamed-folder"), true)?;
    assert!(!server.join("folder").exists());
    client_command(&client, "dehydrate", "renamed-folder/child.txt")?;
    client_command(&client, "hydrate", "renamed-folder/child.txt")?;
    assert_eq!(
        fs::read(client.join("renamed-folder/child.txt"))?,
        b"nested contents"
    );
    client_action(&client, "delete-dir", "renamed-folder", "")?;
    wait_for_server_path(&server.join("renamed-folder"), false)?;
    Ok(())
}

// Delay delivery to the example, not the OS operation: the local namespace has
// already changed. A path notification alone must not delete or rename remotely.
#[test]
fn namespace_changes_wait_for_completion() -> anyhow::Result<()> {
    enum Completion {
        Renamed(Request, info::Renamed),
        Closed(Request, info::Closed),
    }
    struct Deferred<F> {
        inner: Arc<F>,
        completions: std::sync::mpsc::Sender<Completion>,
    }
    impl<F: SyncFilter> SyncFilter for Deferred<F> {
        fn fetch_data(
            &self,
            request: Request,
            ticket: ticket::FetchData,
            info: info::FetchData,
        ) -> CResult<()> {
            self.inner.fetch_data(request, ticket, info)
        }
        fn fetch_placeholders(
            &self,
            request: Request,
            ticket: ticket::FetchPlaceholders,
            info: info::FetchPlaceholders,
        ) -> CResult<()> {
            self.inner.fetch_placeholders(request, ticket, info)
        }
        fn rename(
            &self,
            request: Request,
            ticket: ticket::Rename,
            info: info::Rename,
        ) -> CResult<()> {
            self.inner.rename(request, ticket, info)
        }
        fn renamed(&self, request: Request, info: info::Renamed) {
            let _ = self.completions.send(Completion::Renamed(request, info));
        }
        fn delete(
            &self,
            request: Request,
            ticket: ticket::Delete,
            info: info::Delete,
        ) -> CResult<()> {
            self.inner.delete(request, ticket, info)
        }
        fn deleted(&self, request: Request, info: info::Deleted) {
            self.inner.deleted(request, info);
        }
        fn closed(&self, request: Request, info: info::Closed) {
            if info.deleted() {
                let _ = self.completions.send(Completion::Closed(request, info));
            } else {
                self.inner.closed(request, info);
            }
        }
        fn state_changed(&self, event: StateWatchEvent) {
            self.inner.state_changed(event);
        }
    }
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("file.txt"), b"remote contents")?;
    let mirror = Arc::new(root.mirror());
    let (sender, receiver) = std::sync::mpsc::channel();
    let _connection = connect(
        &client,
        Deferred {
            inner: mirror.clone(),
            completions: sender,
        },
    )?;
    populate(&client)?;

    client_action(&client, "rename", "file.txt", "renamed.txt")?;
    let Completion::Renamed(request, info) = receiver.recv_timeout(Duration::from_secs(5))? else {
        anyhow::bail!("expected rename completion");
    };
    assert!(client.join("renamed.txt").exists());
    std::thread::sleep(Duration::from_millis(600)); // Let path sync run without completion.
    assert!(server.join("file.txt").exists());
    assert!(!server.join("renamed.txt").exists());
    mirror.renamed(request, info);
    wait_for_server_path(&server.join("renamed.txt"), true)?;
    assert_eq!(fs::read(server.join("renamed.txt"))?, b"remote contents");

    client_action(&client, "delete", "renamed.txt", "")?;
    let Completion::Closed(request, info) = receiver.recv_timeout(Duration::from_secs(5))? else {
        anyhow::bail!("expected deleting close");
    };
    assert!(!client.join("renamed.txt").exists());
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(fs::read(server.join("renamed.txt"))?, b"remote contents");
    mirror.closed(request, info);
    wait_for_server_path(&server.join("renamed.txt"), false)?;
    Ok(())
}

#[test]
fn failed_local_rename_keeps_remote_name() -> anyhow::Result<()> {
    struct Collision<F>(F);
    impl<F: SyncFilter> SyncFilter for Collision<F> {
        fn fetch_data(
            &self,
            request: Request,
            ticket: ticket::FetchData,
            info: info::FetchData,
        ) -> CResult<()> {
            self.0.fetch_data(request, ticket, info)
        }
        fn fetch_placeholders(
            &self,
            request: Request,
            ticket: ticket::FetchPlaceholders,
            info: info::FetchPlaceholders,
        ) -> CResult<()> {
            self.0.fetch_placeholders(request, ticket, info)
        }
        fn rename(
            &self,
            request: Request,
            ticket: ticket::Rename,
            info: info::Rename,
        ) -> CResult<()> {
            // Windows has reached our approval callback, but the destination can
            // still collide before its local operation actually completes.
            fs::write(info.target_path(), b"occupied").map_err(|_| CloudErrorKind::Unsuccessful)?;
            self.0.rename(request, ticket, info)
        }
        fn closed(&self, request: Request, info: info::Closed) {
            self.0.closed(request, info);
        }
        fn renamed(&self, request: Request, info: info::Renamed) {
            self.0.renamed(request, info);
        }
    }
    let root = TestRoot::new()?;
    let client = root.path.join("client");
    let server = root.path.join("server");
    fs::write(server.join("file.txt"), b"keep original")?;
    let mirror = root.mirror();
    let _connection = connect(&client, Collision(mirror))?;
    populate(&client)?;
    client_action(&client, "rename-collision", "file.txt", "")?;
    assert_eq!(fs::read(server.join("file.txt"))?, b"keep original");
    client_command(&client, "hydrate", "file.txt")?;
    assert_eq!(fs::read(client.join("file.txt"))?, b"keep original");
    assert_eq!(fs::read(client.join("occupied.txt"))?, b"occupied");
    Ok(())
}

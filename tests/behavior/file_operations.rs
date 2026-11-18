//! The same user operations through PowerShell filesystem commands and Windows Shell.
use std::{env, fs, process::Command};

use anyhow::{ensure, Context};
use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, SyncFilter},
    metadata::Metadata,
    placeholder_create_info::PlaceholderCreateInfo,
    root::{HydrationType, Session},
    utility::WriteAt,
};
use libtest_mimic::Failed;

use super::common::{open, Client, Root};

const DIRECTORIES: &[&str] = &["tree", r"tree\a", r"tree\a\b", r"tree\d", r"tree\d\e"];
const FILES: &[&str] = &[r"tree\a\b\file.bin", r"tree\d\e\file.bin"];

#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Filesystem,
    Shell,
}

impl Backend {
    fn run(self, root: &Root, action: &str, path: &str, value: &str) -> anyhow::Result<()> {
        // Both clients run outside the provider so CF delivers user-operation callbacks.
        let mut command = match self {
            Self::Filesystem => {
                let mut command = Command::new("powershell.exe");
                command.args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    include_str!("filesystem.ps1"),
                ]);
                command
            }
            Self::Shell => {
                let mut command = Command::new(env::current_exe()?);
                command.arg("--shell-client");
                command
            }
        };
        Client::run_command(
            command
                .env("CF_PROBE_ROOT", &root.path)
                .env("CF_ACTION", action)
                .env("CF_PATH", path)
                .env("CF_VALUE", value),
        )
        .with_context(|| format!("{self:?}: {action} {path}"))
    }
}

// Fixed remote contents, without a backing store or example writeback machinery.
struct Provider;

impl SyncFilter for Provider {
    fn fetch_data(
        &self,
        _: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let range = info.required_file_range();
        ticket
            .write_at(&vec![b'x'; (range.end - range.start) as usize], range.start)
            .map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        _: info::FetchPlaceholders,
    ) -> CResult<()> {
        if request.path().file_name().unwrap() == "remote" {
            ticket
                .pass_with_placeholders(&mut [PlaceholderCreateInfo::new("file.bin")
                    .metadata(Metadata::file().size(8192))
                    .blob(b"remote-file".to_vec())
                    .mark_in_sync()])
                .map_err(|_| CloudErrorKind::Unsuccessful)?;
        }
        Ok(())
    }

    fn delete(&self, _: Request, ticket: ticket::Delete, _: info::Delete) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn rename(&self, _: Request, ticket: ticket::Rename, _: info::Rename) -> CResult<()> {
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }
}

pub fn file_operations(backend: Backend) -> Result<(), Failed> {
    let root = Root::new()?;
    root.register_with_hydration(HydrationType::Full, false)?;
    PlaceholderCreateInfo::new("remote")
        .metadata(Metadata::directory())
        .blob(b"remote-directory".to_vec())
        .mark_in_sync()
        .create(&root.path)?;
    let _connection = Session::new().connect(&root.path, Provider)?;
    let run = |action: &str, path: &str, value: &str| backend.run(&root, action, path, value);

    run("mkdir", "local", "")?;
    run("create", r"local\new.txt", "")?;
    assert!(root.path.join("local").is_dir());
    assert_eq!(fs::metadata(root.path.join(r"local\new.txt"))?.len(), 0);

    // Enumeration populates children without downloading file contents.
    run("list", "remote", "file.bin")?;
    let file = root.path.join(r"remote\file.bin");
    let original = open(&file)?.info()?.unwrap();
    assert_eq!(original.on_disk_data_size(), 0);
    run("read", r"remote\file.bin", &"x".repeat(8192))?;
    assert_eq!(open(&file)?.info()?.unwrap().on_disk_data_size(), 8192);
    run("write", r"remote\file.bin", "edited")?;
    assert_eq!(fs::read(&file)?, b"edited");
    assert!(!open(&file)?.info()?.unwrap().is_in_sync());

    // Rename both kinds of placeholder, then move the file into an ordinary directory.
    run("rename", r"remote\file.bin", "renamed.bin")?;
    assert!(!file.try_exists()?);
    run("rename", "remote", "renamed")?;
    assert!(!root.path.join("remote").try_exists()?);
    run("move", r"renamed\renamed.bin", r"local\moved.bin")?;
    assert!(!root.path.join(r"renamed\renamed.bin").try_exists()?);
    let moved = open(&root.path.join(r"local\moved.bin"))?.info()?.unwrap();
    assert_eq!(moved.file_id(), original.file_id());
    assert_eq!(moved.blob(), original.blob());
    run("copy", r"local\moved.bin", r"local\copy.bin")?;
    assert_eq!(fs::read(root.path.join(r"local\copy.bin"))?, b"edited");
    run("delete", r"local\moved.bin", "")?;
    assert!(!root.path.join(r"local\moved.bin").try_exists()?);
    assert!(root.path.join(r"local\copy.bin").try_exists()?);
    run("delete", "local", "")?;
    run("delete", "renamed", "")?;
    assert_eq!(fs::read_dir(&root.path)?.count(), 0);
    Ok(())
}

#[derive(PartialEq)]
enum Tree {
    OrdinaryDirectories,
    Hydrated,
    Offline,
}

fn delete_tree(tree: Tree, backend: Backend) -> anyhow::Result<()> {
    let root = Root::new()?;
    if tree == Tree::OrdinaryDirectories {
        // Registration preserves these ordinary directories as a control for cloud directories.
        for name in DIRECTORIES {
            fs::create_dir(root.path.join(name))?;
        }
    }
    root.register_with_hydration(HydrationType::Full, false)?;
    for name in DIRECTORIES {
        let path = root.path.join(name);
        if tree != Tree::OrdinaryDirectories {
            PlaceholderCreateInfo::new(path.file_name().unwrap())
                .metadata(Metadata::directory())
                .blob(name.as_bytes().to_vec())
                .has_no_children()
                .mark_in_sync()
                .create(path.parent().unwrap())?;
        }
    }
    for name in FILES {
        let path = root.path.join(name);
        PlaceholderCreateInfo::new(path.file_name().unwrap())
            .metadata(Metadata::file().size(8192))
            .blob(name.as_bytes().to_vec())
            .mark_in_sync()
            .create(path.parent().unwrap())?;
    }
    let _connection = Session::new().connect(&root.path, Provider)?;
    if tree == Tree::Hydrated {
        Client::run(
            &root,
            r#"
$ErrorActionPreference = 'Stop'
Get-ChildItem -LiteralPath (Join-Path $env:CF_PROBE_ROOT 'tree') -File -Recurse |
    ForEach-Object { [IO.File]::ReadAllBytes($_.FullName) | Out-Null }
"#,
        )?;
    }
    for name in FILES {
        assert_eq!(
            open(&root.path.join(name))?
                .info()?
                .unwrap()
                .on_disk_data_size(),
            if tree == Tree::Hydrated { 8192 } else { 0 }
        );
    }
    backend.run(&root, "delete", "tree", "")?;
    let mut remaining = Vec::new();
    for name in DIRECTORIES {
        if root.path.join(name).try_exists()? {
            remaining.push(name);
        }
    }
    ensure!(
        remaining.is_empty(),
        "deletion left directories: {remaining:?}"
    );
    Ok(())
}

pub fn filesystem_offline_tree() -> Result<(), Failed> {
    delete_tree(Tree::Offline, Backend::Filesystem).map_err(Into::into)
}

pub fn shell_ordinary_directories() -> Result<(), Failed> {
    delete_tree(Tree::OrdinaryDirectories, Backend::Shell).map_err(Into::into)
}

pub fn shell_hydrated_tree() -> Result<(), Failed> {
    delete_tree(Tree::Hydrated, Backend::Shell).map_err(Into::into)
}

pub fn shell_offline_tree() -> Result<(), Failed> {
    // Windows issue 6060921: S_OK and aborted=FALSE can still leave empty ancestors.
    delete_tree(Tree::Offline, Backend::Shell).map_err(Into::into)
}

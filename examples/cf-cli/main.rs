use std::{fs, os::windows::fs::OpenOptionsExt, path::PathBuf};

use anyhow::{ensure, Context};
use clap::{Parser, ValueEnum};
use cloud_filter::{
    ext::FileExt,
    placeholder::{PinOptions, PinState, Placeholder},
};

#[derive(Parser)]
#[command(
    name = "cf-cli",
    about = "Inspect and manage a Cloud Files placeholder"
)]
struct Args {
    #[arg(value_enum)]
    command: Command,
    /// File path, absolute or relative to the current directory
    path: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum Command {
    /// Inspect state without downloading file contents
    Status,
    /// Download all file contents (requires a running provider)
    Hydrate,
    /// Release local contents of an unpinned, in-sync file
    Dehydrate,
    /// Request that the provider keeps this file available locally
    Pin,
    /// Request that the provider releases this file's local contents
    Unpin,
}

fn main() -> anyhow::Result<()> {
    let Args { command, path } = Args::parse();
    ensure!(path.is_file(), "expected a file path");
    let open = || {
        // Opening for data access could hydrate before status is even queried.
        fs::OpenOptions::new()
            .access_mode(0)
            .open(&path)
            .map(Placeholder::from)
    };
    match command {
        Command::Dehydrate => {
            // Dehydration needs exclusive access; fail if another handle is using the file.
            fs::OpenOptions::new()
                .access_mode(0)
                .share_mode(0)
                .open(&path)?
                .dehydrate(..)
                .context("dehydrate (file must be in sync and not pinned)")?;
        }
        Command::Hydrate => open()?.hydrate(..).context("hydrate")?,
        Command::Pin => {
            open()?.mark_pin(PinState::Pinned, PinOptions::default())?;
        }
        Command::Unpin => {
            open()?.mark_pin(PinState::Unpinned, PinOptions::default())?;
        }
        Command::Status => {}
    }
    let info = open()?.info()?.context("file is not a placeholder")?;
    println!(
        "{}: logical={}, on_disk={}, modified={}, in_sync={}, pin={:?}",
        path.display(),
        fs::metadata(&path)?.len(),
        info.on_disk_data_size(),
        info.modified_data_size(),
        info.is_in_sync(),
        info.pin_state()
    );
    Ok(())
}

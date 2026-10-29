use std::process::ExitCode;

use libtest_mimic::{run, Arguments, Trial};

mod async_filter;
mod lifecycle;
mod placeholder;
mod properties;
mod state_watcher;
mod sync_filter;

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let tests = vec![
        Trial::test("registration_lifecycle", lifecycle::registration_lifecycle),
        Trial::test("callback_triggers", lifecycle::callback_triggers),
        Trial::test(
            "callback_cancellation_timeout",
            lifecycle::callback_cancellation_timeout,
        )
        .with_ignored_flag(true),
        Trial::test(
            "file_round_trip",
            placeholder::file_round_trip_transfers_ownership,
        ),
        Trial::test(
            "placeholder_handles",
            placeholder::placeholder_outputs_and_protected_handles_remain_valid,
        ),
        Trial::test("custom_states", properties::custom_states_show_in_explorer),
        Trial::test("sync_filter", sync_filter::test),
        Trial::test(
            "state_watch_startup_failures",
            state_watcher::startup_failures,
        ),
        Trial::test(
            "state_watch_unicode_paths_and_shutdown",
            state_watcher::unicode_paths_and_shutdown,
        ),
        Trial::test("state_watch_deleted_root", state_watcher::deleted_root),
        Trial::test(
            "state_watch_idle_disconnect",
            state_watcher::idle_disconnect,
        ),
        Trial::test(
            "state_watch_overflow_and_resume",
            state_watcher::overflow_and_resume,
        ),
        Trial::test(
            "state_watch_async_terminal_event",
            state_watcher::async_terminal_event,
        ),
    ];

    let conclusion = run(&args, tests);
    if conclusion.has_failed() {
        return conclusion.exit_code();
    }

    let tests = vec![Trial::test("async_filter", async_filter::test)];
    let conclusion = run(&args, tests);

    conclusion.exit_code()
}

fn test_list_folders(root: &str) {
    let output = powershell_script::run(&format!("Get-ChildItem {root} -Recurse -Name"))
        .expect("run script");
    assert_eq!(
        "dir1\r\n\
        test1.txt\r\n\
        dir1\\test2.txt\r\n",
        output.stdout().expect("stdout"),
    );
}

fn test_update_metadata(root: &str) {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use cloud_filter::{
        metadata::Metadata,
        placeholder::{Placeholder, UpdateOptions},
    };
    use nt_time::FileTime;

    let path = format!("{root}\\test1.txt");
    let written = FileTime::new(133_000_000_000_000_000);
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(0)
        .open(&path)
        .expect("open placeholder");
    Placeholder::from(file)
        .update(
            UpdateOptions::default()
                .metadata(Metadata::file().written(written).size(42))
                .mark_in_sync()
                .dehydrate(),
            None,
        )
        .expect("update placeholder");

    let metadata = std::fs::metadata(&path).expect("metadata");
    assert_eq!(metadata.len(), 42);
    assert_eq!(metadata.last_write_time(), written.to_raw());
}

fn test_read_file(root: &str) {
    for relative in ["test1.txt", "dir1\\test2.txt"] {
        let path = format!("{root}\\{relative}");
        let output =
            powershell_script::run(&format!("Get-Content {path} -Raw")).expect("run script");
        assert_eq!(output.stdout().expect("stdout"), format!("{relative}\r\n"));
    }
}

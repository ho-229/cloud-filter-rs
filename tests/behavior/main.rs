use std::process::ExitCode;

use libtest_mimic::{run, Arguments, Trial};

mod async_filter;
mod common;
mod file_operations;
mod lifecycle;
mod placeholder;
mod properties;
mod shell;
mod state_watcher;
mod tickets;

fn main() -> ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--shell-client")) {
        return match shell::client() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Shell client: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    let args = Arguments::from_args();
    let mut tests = vec![
        Trial::test(
            "invalid_inputs",
            placeholder::invalid_inputs_leave_placeholders_unchanged,
        ),
        Trial::test("file_usn", placeholder::file_usn_tracks_changes),
        Trial::test("restart_hydration", tickets::restart_hydration),
        Trial::test("sync_root_membership", lifecycle::sync_root_membership),
        Trial::test("sync_root_info", lifecycle::sync_root_info),
        Trial::test("registration_lifecycle", lifecycle::registration_lifecycle),
        Trial::test("callback_triggers", lifecycle::callback_triggers),
        Trial::test(
            "acknowledgment_is_not_completion",
            lifecycle::acknowledgment_is_not_completion,
        ),
        Trial::test(
            "placeholder_batches_preserve_identities",
            placeholder::placeholder_batches_preserve_identities,
        ),
        Trial::test(
            "callback_cancellation_timeout",
            lifecycle::callback_cancellation_timeout,
        )
        .with_ignored_flag(true),
        Trial::test("placeholder_handles", placeholder::placeholder_handles),
        Trial::test(
            "shell::custom_states",
            properties::custom_states_show_in_explorer,
        ),
        Trial::test(
            "filesystem::delete_offline_tree",
            file_operations::filesystem_offline_tree,
        ),
        Trial::test(
            "shell::delete_ordinary_directories",
            file_operations::shell_ordinary_directories,
        ),
        Trial::test(
            "shell::delete_hydrated_tree",
            file_operations::shell_hydrated_tree,
        ),
        Trial::test(
            "shell::delete_offline_tree",
            file_operations::shell_offline_tree,
        )
        .with_ignored_flag(true),
        Trial::test("async_filter", async_filter::test),
        Trial::test(
            "state_watch_startup_failures",
            state_watcher::startup_failures,
        ),
        Trial::test(
            "state_watch_changes_and_reconnect",
            state_watcher::changes_and_reconnect,
        ),
        Trial::test("state_watch_deleted_root", state_watcher::deleted_root),
        Trial::test(
            "state_watch_overflow_and_resume",
            state_watcher::overflow_and_resume,
        ),
        Trial::test(
            "state_watch_async_terminal_event",
            state_watcher::async_terminal_event,
        ),
    ];
    for (name, backend) in [
        ("filesystem", file_operations::Backend::Filesystem),
        ("shell", file_operations::Backend::Shell),
    ] {
        tests.push(Trial::test(format!("{name}::file_operations"), move || {
            file_operations::file_operations(backend)
        }));
    }

    run(&args, tests).exit_code()
}

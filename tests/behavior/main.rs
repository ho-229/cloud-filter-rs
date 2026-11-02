use std::process::ExitCode;

use libtest_mimic::{run, Arguments, Trial};

mod async_filter;
mod common;
mod lifecycle;
mod placeholder;
mod properties;
mod state_watcher;
mod tickets;

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let tests = vec![
        Trial::test("file_usn", placeholder::file_usn_tracks_changes),
        Trial::test("restart_hydration", tickets::restart_hydration),
        Trial::test("sync_root_membership", lifecycle::sync_root_membership),
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
        Trial::test(
            "file_round_trip",
            placeholder::file_round_trip_transfers_ownership,
        ),
        Trial::test(
            "placeholder_handles",
            placeholder::placeholder_outputs_and_protected_handles_remain_valid,
        ),
        Trial::test("custom_states", properties::custom_states_show_in_explorer),
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

    run(&args, tests).exit_code()
}

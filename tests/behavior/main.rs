use std::process::ExitCode;

use libtest_mimic::{run, Arguments, Trial};

mod sync_filter;

fn main() -> ExitCode {
    let args = Arguments::from_args();
    let tests = vec![Trial::test("sync_filter", sync_filter::test)];

    let conclusion = run(&args, tests);

    conclusion.exit()
}

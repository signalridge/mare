//! `mare __sandbox-exec`: the runner's entry point.

use super::runner_args::RunnerArgs;
#[cfg(windows)]
use super::win;
use super::{RUNNER_FAILURE_EXIT, RUNNER_SIGNATURE};

/// Entry point of `mare __sandbox-exec …`: start the command confined and mirror its exit code.
pub fn runner_main(raw: &[String]) -> i32 {
    #[cfg(windows)]
    if raw.first().is_some_and(|arg| arg == "--pipe-in") {
        return match crate::winsandbox_user::win::run_ipc(raw) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("{RUNNER_SIGNATURE}{error}");
                RUNNER_FAILURE_EXIT
            }
        };
    }
    let a = match RunnerArgs::parse(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{RUNNER_SIGNATURE}{e}");
            return RUNNER_FAILURE_EXIT;
        }
    };
    #[cfg(windows)]
    {
        let r = match a.as_user.clone() {
            Some(which) => crate::winsandbox_user::win::run_outer(&a, &which),
            None => win::run(&a),
        };
        match r {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{RUNNER_SIGNATURE}{e}");
                RUNNER_FAILURE_EXIT
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = a;
        eprintln!("{RUNNER_SIGNATURE}the Windows sandbox runner only runs on Windows");
        RUNNER_FAILURE_EXIT
    }
}

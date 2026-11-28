//! Windows process sandbox, default tier: a write-restricted
//! token plus ACL grants (MIT / Apache-2.0).
//!
//! How it works:
//! - **identities**: every writable root gets a capability SID. Workspace roots derive theirs
//!   deterministically from the canonical path (`S-1-4-x-y`), so the root's ACE is written once
//!   per workspace and later sandboxes skip the (tree-propagating) DACL rewrite. The private temp
//!   directory derives its SID from its random path (`S-1-4-x-y-1`): a new process — a resumed or
//!   forked session included — gets a new temp directory and a new identity, and never inherits
//!   the previous one's grant;
//! - **grants** (parent, unsandboxed): an inheritable allow ACE for the root's SID, an inheritable
//!   `FILE_DELETE_CHILD` deny for Everyone (no ambient deletes through a granted parent), and a Low
//!   mandatory label (NO_WRITE_UP) so a Low-integrity process may write there;
//! - **runner** (`mare __sandbox-exec …`, spawned in place of the command like `sandbox-exec` /
//!   `bwrap`): creates a `WRITE_RESTRICTED` token whose restricting SIDs are the logon SID,
//!   Everyone and the granted capability SIDs, lowers it to Low integrity, gives it a default
//!   DACL that lets it create its own pipes (PowerShell pipelines fail with ACCESS_DENIED
//!   otherwiseCreateProcessAsUserW` on
//!   the runner's own stdio. The runner already sits in mare's Job Object, so the confined child
//!   does too and dies with the tree. Reads are not restricted, so the tier reports `partial`;
//!   the network is not controlled without the enhanced tier's WFP filters (reported as such).
//!
//! The pure parts (SID derivation, runner arguments, helper lookup) build on every platform and
//! are unit-tested everywhere; the Win32 calls live in [`win`].
//!
//! This module holds both Windows tiers: the default tier above (`grants`, `temp`,
//! `runner_args`, `runner`, and `win` for Win32) and the enhanced tier in [`enhanced`]
//! (`enhanced_win` for Win32), still reachable as `crate::winsandbox_user`.

#[cfg(windows)]
#[path = "desktop_win.rs"]
mod desktop;
pub mod enhanced;
mod grants;
mod runner;
mod runner_args;
#[cfg(any(windows, test))]
mod runner_ipc;
mod temp;
#[cfg(windows)]
#[path = "default_win.rs"]
pub(crate) mod win;

pub use grants::{prepare_grants, readonly_capability_sid, temp_write_sid, workspace_write_sid};
pub(crate) use grants::{prepare_root_grants, verify_cwd_inheritance};
pub use runner::runner_main;
pub use runner_args::{RunnerArgs, helper_exe};
#[cfg(not(unix))]
pub(crate) use temp::pid_alive;
#[cfg(windows)]
pub(crate) use temp::pid_start_time;
pub use temp::{TEMP_NOT_ISOLATED, TempGrant, disjoint, session_temp_dir, session_temp_grant, temp_grant};

pub use crate::platform::cmdline::{argv_to_command_line, quote_windows_arg};

/// argv[1] that turns the `mare` binary into the sandbox runner.
pub const RUNNER_ARG: &str = "__sandbox-exec";
/// Prefix of the runner's own diagnostics on stderr: "the sandbox failed to start".
pub const RUNNER_SIGNATURE: &str = "mare-sandbox: ";
/// Exit code of a runner that could not start the command.
pub const RUNNER_FAILURE_EXIT: i32 = 127;

#[cfg(test)]
mod tests;

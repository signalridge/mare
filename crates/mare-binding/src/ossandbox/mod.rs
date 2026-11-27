//! OS process sandbox: the `sandbox` option of `mare.proc.spawn/run`
//! and `mare.proc.sandbox.*` (MIT):
//! - three levels on one ladder: `read-only` → `workspace-write` → `danger-full-access`
//!   (`full-access` is accepted as an alias); the session's level ([`current`]) changes only
//!   through [`set_current`];
//! - writable roots = workspace + explicit extra directories + platform temp areas, all
//!   canonical. Extra directories must be existing absolute physical paths and are re-validated
//!   at every spawn (a directory swapped for a symlink between calls is refused);
//! - backends: macOS Seatbelt ("allow default + deny file-write* + write allowlist"); Linux
//!   bubblewrap, then Landlock — each probed once and cached; Windows a write-restricted,
//!   Low-integrity token with ACL grants ([`crate::winsandbox`], reported `partial`: reads and the
//!   network are not controlled). No usable backend → `SANDBOX_UNAVAILABLE`, never an unconfined
//!   run;
//! - capability matching: a policy *requires* dimensions
//!   (`fs_write`, `fs_read`, `net_block`, `net_proxy`, `unix_socket`, `spawn`); a backend *provides* some
//!   ([`Backend::caps`]). A missing required dimension refuses the call with
//!   `SANDBOX_INSUFFICIENT` naming it unless explicitly accepted (`accept_partial`). The
//!   permissions plugin defaults to accepting only `fs_protect` as partial; an explicit empty
//!   `accept_partial` remains strict.
//!   Proxy environment variables never count as `net_proxy`;
//! - "the sandbox failed to start" and "the sandbox denied the command" are distinct results.
//!
//! Network-by-domain goes through the local proxy in [`crate::netproxy`]: `network` is `true`
//! (open), `false` (blocked) or `{ allow, deny, default, allow_local }`. Seatbelt lets the
//! command connect only to the proxy's port; bubblewrap runs it in a loopback-only network
//! namespace bridged to the proxy; Landlock can only block.
//!
//! The session's current policy is enforced by core for every primitive called inside a tool
//! execution context.

use std::path::PathBuf;

use mlua::{Lua, Table};

use crate::netproxy::NetPolicy;

pub(crate) mod access;
mod backend;
mod bwrap;
#[cfg(target_os = "linux")]
mod bwrap_guard;
#[cfg(target_os = "linux")]
mod bwrap_lifecycle;
mod caps;
mod classify;
mod deny_path;
pub mod deny_read;
#[doc(hidden)]
pub mod expect;
mod landlock;
mod level;
mod lua_api;
#[cfg(target_os = "linux")]
mod mount_inspection;
mod msys;
#[cfg(target_os = "linux")]
mod netns;
#[cfg(target_os = "linux")]
mod netns_inner;
mod policy;
mod prepared;
mod prexec;
pub mod protected;
pub mod read_mask;
mod report;
mod roots;
pub use roots::validate_read_grant;
mod seatbelt;
mod session;
mod spec;

pub use backend::{Sandbox, SandboxBackend, backend, probe_caps, warm};
#[cfg(target_os = "linux")]
pub(crate) use bwrap_lifecycle::inherit as inherit_lifecycle;
pub use caps::Cap;
pub use classify::classify;
pub(crate) use classify::path_candidates;
pub use level::Level;
pub use lua_api::install;
pub use msys::{
    SANDBOX_MSYS_UNSUPPORTED, STATUS_DLL_INIT_FAILED, is_msys_program, msys_failure, msys_report, msys_verdict,
};
#[cfg(target_os = "linux")]
pub use netns::{NETNS_ARG, NetnsBridge, probe as net_proxy_probe};
#[cfg(target_os = "linux")]
pub use netns_inner::netns_main;
pub(crate) use policy::root_protected_subpaths;
pub(crate) use prepared::ProtectionMonitor;
/// Whether a failed spawn still has a live process in its original group. On Linux procfs lets
/// us exclude zombies; when inspection is unavailable retain the conservative platform check.
pub(crate) fn failed_spawn_tree_alive(pid: Option<u32>) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(pid) = pid
        && let Ok(live) = bwrap_guard::group_has_live_processes(pid)
    {
        return live;
    }
    crate::platform::tree_alive(pid)
}

#[cfg(unix)]
pub use landlock::landlock_abi;
pub use prepared::{Prepared, Wrapped, ensure_supported};
pub use prexec::SocketBlock;
#[cfg(target_os = "linux")]
pub use prexec::{child_restrict, landlock_restrict, seccomp_filter};
pub use report::{capability_summary, net_proxy_line, temp_isolation_note};
pub use roots::validate_root;
pub(crate) use roots::{SYSTEM_READ_ROOTS, is_write_device};
pub use session::{
    PolicyHandle, ceiling, check_current, check_set_current, current, enforce_ceiling, generation, lift_ceiling,
    lift_permission_ceiling, lifted, policy_handle, revoke_permission_lifts, set_current, set_inherited_ceiling,
    set_permission_reads, set_policy_change_hook, stage_permission_reads, take_permission_reads,
};
pub use spec::PolicyUd;

pub use crate::realpath::{canonical_path_key, expand, gitdir_from_file, path_within, resolve};

/// Error code prefix when no backend can enforce a confined level.
pub const SANDBOX_UNAVAILABLE: &str = "SANDBOX_UNAVAILABLE";
/// Error code prefix for a refused extra writable directory.
pub const SANDBOX_INVALID_ROOT: &str = "SANDBOX_INVALID_ROOT";
/// Error code prefix when the backend lacks a capability the call requires.
pub const SANDBOX_INSUFFICIENT: &str = "SANDBOX_INSUFFICIENT";

#[derive(Clone, Debug)]
pub enum Network {
    Open,
    /// filtered through the local proxy (a blocked network is a proxy that denies everything)
    Proxy(NetPolicy),
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub level: Level,
    /// canonical workspace root (writable under workspace-write unless explicit roots replace it)
    pub root: PathBuf,
    /// extra writable directories as validated (canonical, absolute, physical)
    pub extra: Vec<PathBuf>,
    /// whether the workspace root is part of the writable set (`write = {...}` replaces it)
    pub root_writable: bool,
    /// Exclude environment/platform temporary areas from workspace-write defaults.
    pub exclude_tmpdir_env_var: bool,
    /// Exclude the default Unix /tmp write root. Explicit roots remain explicit grants.
    pub exclude_slash_tmp: bool,
    /// every canonical writable root (root + extra + temp areas); empty under read-only
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    /// Explicit denied-read paths and the identities observed at policy construction.
    pub deny_read: Vec<DenyReadPath>,
    pub restrict_read: bool,
    /// the read set is `readable` alone: writable roots granted on top don't add reads (an
    /// escalation kept sandboxed for the session's denied reads, [`crate::deny_read`])
    pub reads_pinned: bool,
    pub network: Network,
    /// raw seatbelt profile override (macOS only)
    pub profile: Option<String>,
    /// Unix sockets reachable while the network is restricted (canonical paths). Policy layer and
    /// kernel configuration use the same list.
    pub unix_sockets: Vec<PathBuf>,
    /// dimensions this policy accepts as unenforced (`accept_partial`); the permissions plugin
    /// supplies `fs_protect` here only when its option is omitted.
    pub accept_partial: Vec<Cap>,
    /// Protected paths from the policy's project resource root (`.mare/` and existing `.mcp.json`).
    pub project_protected: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenyReadPath {
    pub input: PathBuf,
    pub target: PathBuf,
    pub identity: Option<(u64, u64)>,
    /// None means the path did not resolve when the policy was built.
    pub is_dir: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Seatbelt,
    Bwrap,
    /// `partial` = an older Landlock ABI that can't cover every file effect
    Landlock {
        partial: bool,
    },
    /// Windows default tier: restricted token + ACL grants (writes only; partial)
    WinAcl,
    /// Windows enhanced tier: sandbox account + deny-read ACLs + WFP, self-test passed (full)
    WinUser,
    None,
}

/// A structured sandbox denial (only produced when the sandbox — not the program — refused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// "write" | "read" | "network" | "unavailable"
    pub kind: &'static str,
    pub path: Option<String>,
    pub host: Option<String>,
}

impl Denial {
    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("sandbox_denied", self.kind)?;
        t.set("path", self.path.clone())?;
        t.set("host", self.host.clone())?;
        Ok(t)
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod host_protection_tests;
// Linux: the bwrap skip it drives exists only there
#[cfg(all(test, target_os = "linux"))]
mod skip_report_tests;

#[cfg(all(test, windows))]
mod win_tests;

/// Platform-independent pieces (run on every OS).
#[cfg(test)]
mod portable_tests;

#[cfg(all(test, target_os = "linux"))]
mod roundtrip_tests;

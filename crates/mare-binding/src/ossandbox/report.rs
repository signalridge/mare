//! Honest reporting: the capability summary, a policy's report, the temp-isolation note.

use std::fmt::Write as _;
use std::path::PathBuf;

use mlua::{Lua, Table};

use super::backend::backend;
use super::caps::Cap;
use super::level::Level;
use super::{Backend, Network, Policy, SANDBOX_UNAVAILABLE};
use crate::netproxy::ProxyHandle;

/// One line for `mare doctor` / `/permission`: the backend and the dimensions it enforces here.
pub fn capability_summary() -> (bool, String) {
    let b = backend();
    if b == Backend::None {
        return (false, format!("no sandbox backend usable — confined calls are refused ({SANDBOX_UNAVAILABLE})"));
    }
    let have = b.caps();
    let missing: Vec<&str> = Cap::ALL.iter().filter(|c| !have.contains(c)).map(|c| c.name()).collect();
    let mut s = format!("{}: {}", b.name(), have.iter().map(|c| c.name()).collect::<Vec<_>>().join(", "));
    if !missing.is_empty() {
        let _ = write!(
            s,
            "; not enforceable: {} (the permissions plugin accepts fs_protect by default and reports it as partial; other missing required capabilities need accept_partial)",
            missing.join(", ")
        );
    }
    (true, s)
}

/// One line for `mare doctor` (`sandbox net_proxy`): whether a filtered network (domain rules)
/// can be enforced here, and how — or why not. `None` where there's nothing to say (no backend:
/// the `sandbox` line already says every confined call is refused).
pub fn net_proxy_line() -> Option<(bool, String)> {
    let b = backend();
    if b == Backend::None {
        return None;
    }
    if b.caps().contains(&Cap::NetProxy) {
        let how = match b {
            Backend::Bwrap => "loopback-only network namespace bridged to the filtering proxy; direct connections fail",
            Backend::Seatbelt => "the profile allows only the filtering proxy's port",
            _ => "only the filtering proxy's loopback ports are reachable",
        };
        return Some((true, format!("{}: {how}", b.name())));
    }
    // annotated: off Linux every arm is a bare `.into()`, so nothing else fixes the type
    let why: String = match b {
        #[cfg(target_os = "linux")]
        Backend::Bwrap => super::netns::probe().err().unwrap_or_else(|| "narrowed by MARE_SANDBOX_CAPS".into()),
        Backend::Landlock { .. } => {
            "Landlock can only block the network (install bubblewrap for per-domain rules)".into()
        }
        _ => "not available on this backend".into(),
    };
    Some((false, format!("{}: {why}; per-domain network rules are refused unless accept_partial", b.name())))
}

/// on the Windows tiers, the note that the private temp directory is not separately
/// isolated because one of the writable `roots` contains %TEMP% (mare started in the home folder,
/// at `C:\` or in %TEMP%); `None` elsewhere.
pub fn temp_isolation_note(roots: &[PathBuf]) -> Option<&'static str> {
    if !matches!(backend(), Backend::WinAcl | Backend::WinUser) {
        return None;
    }
    matches!(crate::winsandbox::session_temp_grant(roots), Ok(crate::winsandbox::TempGrant::Shared(_)))
        .then_some(crate::winsandbox::TEMP_NOT_ISOLATED)
}

impl Policy {
    /// What this policy enforces on this platform: every required
    /// dimension is either `enforced` or `unavailable`; `partial` only when something required is
    /// unavailable; [`ensure_supported`] allows dimensions listed in `accept_partial`.
    ///
    /// [`ensure_supported`]: crate::ossandbox::ensure_supported
    pub fn report(&self, lua: &Lua, proxy: Option<&ProxyHandle>) -> mlua::Result<Table> {
        let b = backend();
        self.report_with_caps(lua, proxy, b, &self.required(), &self.missing(b))
    }

    pub(super) fn report_with_caps(
        &self,
        lua: &Lua,
        proxy: Option<&ProxyHandle>,
        b: Backend,
        required: &[Cap],
        missing: &[Cap],
    ) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("level", self.level.as_str())?;
        let enforced = lua.create_table()?;
        let unavailable = lua.create_table()?;
        for c in required {
            if missing.contains(c) { unavailable.push(c.name())? } else { enforced.push(c.name())? }
        }
        match &self.network {
            Network::Open => t.set("network", "open")?,
            Network::Proxy(p) => t.set("network", if p.is_deny_all() { "blocked" } else { "filtered" })?,
        }
        if let Some(p) = proxy {
            t.set("proxy", format!("127.0.0.1:{}", p.port()))?;
        }
        if b == Backend::WinAcl {
            // honest reporting: reads are never restricted by the default tier
            t.set("reads", "not controlled")?;
            if !matches!(self.network, Network::Open) {
                t.set("network_control", "not controlled (no WFP filters; proxy variables only)")?;
            }
            // installed but untrustworthy enhanced tier: say why it isn't used
            #[cfg(windows)]
            if let Some(why) = crate::winsandbox_user::fallback_reason() {
                t.set("enhanced", why)?;
            }
        }
        if b == Backend::WinUser {
            t.set(
                "reads",
                "sandbox account + deny-read ACLs (Users-readable volumes remain readable; allowlist fs_read not enforced)",
            )?;
            if !matches!(self.network, Network::Open) {
                t.set(
                    "network_control",
                    "WFP proxy ports are shared across calls; per-call isolation is not enforced",
                )?;
            }
        }
        if missing.contains(&Cap::UnixSocket) && !self.unix_sockets.is_empty() {
            t.set(
                "unix_sockets",
                "unavailable: Linux seccomp blocks all AF_UNIX sockets when the network is restricted and cannot allowlist specific unix_sockets",
            )?;
        }
        if self.restrict_read
            && let Some(note) = crate::read_mask::reads_note(b)
        {
            t.set("reads", note)?;
        }
        if missing.contains(&Cap::FsDenyRead) {
            let mut reason = super::deny_path::gap_reason(b, self.profile.is_some()).to_owned();
            if self.deny_read.iter().any(|d| d.is_dir.is_none()) {
                reason.push_str("; unresolved entries are not masked, including if created later");
            }
            t.set("denied_reads", reason)?;
        }
        if b == (Backend::Landlock { partial: true }) {
            // older ABI (< 3): truncate(2) isn't covered — still write-restricted otherwise
            t.set("note", "landlock ABI < 3: file truncation is not restricted")?;
        }
        if b == Backend::Bwrap && missing.contains(&Cap::FsProtect) {
            t.set(
                "protected_paths",
                "unavailable: a protected path cannot be safely mounted or capability is narrowed",
            )?;
        }
        // read-only `.git` / `.mare`: Seatbelt and bubblewrap carve them out of the writable
        // roots; Landlock can't (it only adds grants) and the Windows ACL tier doesn't deny-ACL
        // them, so there only mare's own file tools enforce it
        if self.level == Level::WorkspaceWrite
            && matches!(b, Backend::Landlock { .. } | Backend::WinAcl | Backend::WinUser)
        {
            t.set("protected_paths", "policy layer only (processes can still write .git/.mare)")?;
        }
        if self.level == Level::WorkspaceWrite
            && let Some(note) = temp_isolation_note(&self.workspace_roots())
        {
            t.set("temp", note)?;
        }
        if !missing.is_empty() {
            let acc = lua.create_table()?;
            for c in missing {
                acc.push(c.name())?;
            }
            t.set("accepted_partial", acc)?;
        }
        if self.reads_pinned {
            t.set("reads_kept", "escalated, kept sandboxed: the session's denied reads still apply")?;
        }
        t.set("mechanism", b.name())?;
        t.set("enforcement", if missing.is_empty() { "full" } else { "partial" })?;
        t.set("enforced", enforced)?;
        t.set("unavailable", unavailable)?;
        Ok(t)
    }
}

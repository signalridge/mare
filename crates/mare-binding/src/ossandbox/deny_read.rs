//! Denied reads survive escalation.
//!
//! In mare the session's read restriction is `restrict_read` (reads limited to the readable and
//! writable roots plus the system roots). It exists only inside the OS sandbox, so running a call
//! outside it would silently grant every denied read. When the session's effective policy (its
//! current policy within any inherited ceiling) restricts reads, every tool call keeps them:
//! - an approved `all` / `danger-full-access` escalation, or a rule allowing one, runs with the
//!   session's own sandbox instead of none;
//! - any other per-call policy (an approved `network` / `write:` increment, a rule's scope, a
//!   sub-agent's lifted ceiling) keeps what it was granted — network, extra writable roots — with
//!   its read set pinned to the session's: granted writable roots don't add reads ([`Policy::
//!   reads_pinned`]), so an explicit deny entry wins over a write grant.
//!
//! Without a read restriction nothing changes: an approved escalation leaves the sandbox. A
//! backend that can't enforce `fs_read` refuses such a call ([`refusal`]) instead of dropping the
//! restriction.
//!
//! The Windows enhanced tier denies reads on its own (deny-read ACEs on the sandbox accounts).
//! Only the entries the user configured count as denied reads (only a policy's explicit deny entries
//! reach the ACLs and keep escalations sandboxed): with one, an approved escalation runs with the
//! session's sandbox; with the built-in defaults alone (`~/.ssh`, `~/.aws`, …) it leaves the
//! sandbox as approved ([`decide`]).

use mlua::Lua;

use super::policy::within;
use crate::ossandbox::{Backend, Cap, Policy, SANDBOX_INSUFFICIENT, SANDBOX_UNAVAILABLE};

/// A tool call's effective policy with the session's denied reads kept (called last by
/// [`crate::exec_ctx::active`]). `None` in and out = unconfined.
pub fn keep(lua: &Lua, effective: Option<Policy>) -> Option<Policy> {
    keep_with_read_grants(lua, effective, &[])
}

/// Explicit structured read grants add readable roots; literal deny_read entries still win.
/// Callers must pass only the approved intersection of the frozen permission request.
pub fn keep_with_read_grants(lua: &Lua, effective: Option<Policy>, reads: &[std::path::PathBuf]) -> Option<Policy> {
    let mut session = crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua));
    if let Some(ref mut policy) = session {
        policy.readable.extend(reads.iter().cloned());
    }
    let acl = session.is_some()
        && crate::ossandbox::backend() == Backend::WinUser
        && crate::winsandbox_user::user_deny_read_active();
    decide(effective, session, acl)
}

/// [`keep`] given the session's effective policy and whether the active backend denies reads
/// the user configured on its own (`acl`: the Windows enhanced tier's user deny-read entries).
pub fn decide(effective: Option<Policy>, session: Option<Policy>, acl: bool) -> Option<Policy> {
    match session {
        Some(s) if s.restrict_read || !s.deny_read.is_empty() => {
            // Session acceptance covers its own denies, not additional strict denies in a
            // nested policy. Both policies must accept the gap when the nested one adds paths.
            let extra_denies_are_accepted = effective.as_ref().is_none_or(|p| {
                p.deny_read.iter().all(|d| s.deny_read.contains(d)) || p.accept_partial.contains(&Cap::FsDenyRead)
            });
            let mut p = if s.restrict_read { preserve(effective, &s) } else { effective.unwrap_or_else(|| s.clone()) };
            for d in &s.deny_read {
                if !p.deny_read.contains(d) {
                    p.deny_read.push(d.clone());
                }
            }
            if !s.deny_read.is_empty() {
                // Only the session can accept this read gap, not an escalated tool request.
                p.accept_partial.retain(|c| *c != Cap::FsDenyRead);
                if s.accept_partial.contains(&Cap::FsDenyRead) && extra_denies_are_accepted {
                    p.accept_partial.push(Cap::FsDenyRead);
                    p.accept_partial.sort();
                }
            }
            Some(p)
        }
        // the deny-read ACEs bind every sandboxed process: an unconfined call takes the
        // session's sandbox, anything else already
        // runs inside one
        Some(s) if acl => Some(effective.unwrap_or(s)),
        _ => effective,
    }
}

/// `effective` under a session policy that restricts reads: unconfined becomes the session's own
/// policy; a policy reading more than the session keeps its other dimensions with the session's
/// read set pinned; anything already within it is returned as is.
pub fn preserve(effective: Option<Policy>, session: &Policy) -> Policy {
    match effective {
        None => pin(session.clone(), session),
        Some(p) if reads_within(&p, session) => p,
        Some(p) => pin(p, session),
    }
}

fn reads_within(p: &Policy, session: &Policy) -> bool {
    match (p.read_roots(), session.read_roots()) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(pr), Some(sr)) => pr.iter().all(|r| sr.iter().any(|s| r.starts_with(s))),
    }
}

fn pin(mut p: Policy, session: &Policy) -> Policy {
    let sr = session.read_roots().unwrap_or_default();
    p.readable = match p.read_roots() {
        Some(pr) => within(&pr, &sr),
        None => sr,
    };
    p.restrict_read = true;
    p.reads_pinned = true;
    // the read dimension is the session's: so is whether it may go unenforced
    p.accept_partial.retain(|c| *c != Cap::FsRead);
    if session.accept_partial.contains(&Cap::FsRead) {
        p.accept_partial.push(Cap::FsRead);
        p.accept_partial.sort();
    }
    p
}

/// The refusal for a kept-sandboxed escalation on a backend that can't enforce `fs_read`.
pub fn refusal(b: Backend) -> String {
    format!(
        "{SANDBOX_INSUFFICIENT}: this call was approved to run with wider permissions, but the session restricts \
         reads, so it stays sandboxed to keep them (denied reads are never dropped by an escalation); the {} \
         sandbox can't enforce fs_read (reads restricted to the allowed roots) — refusing. Lift the read \
         restriction for this session (/permission) to run it, or accept unenforced reads explicitly: \
         mare.plugin(\"permissions\", {{ accept_partial = {{ \"fs_read\" }} }})",
        b.name()
    )
}

/// One line for `mare doctor` / `/permission caps`: what an approved escalation does here when
/// the session restricts reads. `ok` = it can run (kept sandboxed).
pub fn capability_line(b: Backend, caps: &[Cap]) -> (bool, String) {
    if b == Backend::None {
        return (
            false,
            format!(
                "escalations under restricted reads: refused — no sandbox backend can keep the denied reads \
                 ({SANDBOX_UNAVAILABLE})"
            ),
        );
    }
    if caps.contains(&Cap::FsRead) {
        let win = if b == Backend::WinUser {
            "; so do they under deny-read entries you configured (sandbox.deny_read, not the built-in defaults)"
        } else {
            ""
        };
        (
            true,
            format!(
                "escalations under restricted reads: stay sandboxed with the denied reads kept ({} enforces fs_read); \
                 only the approved network / writable roots widen{win}",
                b.name()
            ),
        )
    } else {
        (
            false,
            format!(
                "escalations under restricted reads: refused — {} can't enforce fs_read{} ({SANDBOX_INSUFFICIENT}); \
                 without a read restriction they leave the sandbox as approved",
                b.name(),
                crate::read_mask::no_fs_read_reason(b)
            ),
        )
    }
}

/// Per-path denies are a separate dimension from an allowlist of readable roots.
pub fn explicit_capability_line(b: Backend, caps: &[Cap]) -> (bool, String) {
    if caps.contains(&Cap::FsDenyRead) {
        (true, format!("{}: fs_deny_read available", b.name()))
    } else {
        (
            false,
            format!(
                "{}: fs_deny_read unavailable — {}; strict calls are refused unless explicitly accepted as PARTIAL",
                b.name(),
                super::deny_path::gap_reason(b, false)
            ),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ossandbox::Network;
    use mlua::{Lua, Value};
    use std::path::Path;
    use std::path::PathBuf;

    fn policy(lua: &Lua, spec: &str, cwd: &Path) -> Option<Policy> {
        let v: Value = lua.load(spec).eval().unwrap();
        Policy::from_lua(lua, &v, cwd).unwrap()
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(t.path()).unwrap();
        let (ws, extra) = (root.join("ws"), root.join("extra"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        (t, ws, extra)
    }

    #[test]
    fn an_unconfined_escalation_runs_with_the_session_policy() {
        let lua = Lua::new();
        let (_t, ws, _) = dirs();
        let session = policy(&lua, r#"{ level = "read-only", restrict_read = true }"#, &ws).unwrap();
        let kept = preserve(None, &session);
        assert!(kept.restrict_read && kept.reads_pinned);
        assert_eq!(kept.level, session.level);
        let home = PathBuf::from(std::env::var("HOME").unwrap()).join(".ssh");
        for p in [ws.join("f"), home] {
            assert_eq!(kept.can_read(&p), session.can_read(&p), "{}", p.display());
        }
    }

    #[test]
    fn an_increment_keeps_what_was_granted_but_not_the_reads() {
        let lua = Lua::new();
        let (_t, ws, extra) = dirs();
        lua.globals().set("EXTRA", extra.to_string_lossy().to_string()).unwrap();
        let session =
            policy(&lua, r#"{ level = "read-only", restrict_read = true, readable = { "." } }"#, &ws).unwrap();
        // what an approved `network` + `write:<extra>` increment asks for: no read restriction
        let esc = policy(&lua, r#"{ level = "workspace-write", writable = { EXTRA }, network = true }"#, &ws).unwrap();
        assert!(esc.can_read(&extra.join("secret")), "unrestricted before");
        let kept = preserve(Some(esc), &session);
        assert!(matches!(kept.network, Network::Open), "the approved network stays open");
        assert!(kept.can_write(&extra.join("out")), "the approved root stays writable");
        assert!(!kept.can_read(&extra.join("secret")), "a granted root adds no reads");
        assert!(kept.can_read(&ws.join("file")), "the session's readable roots stay readable");
        // a tool's own (unrestricted) policy can't widen them again
        let own = policy(&lua, r#"{ level = "workspace-write", network = true }"#, &ws).unwrap();
        let narrowed = kept.intersect(&own);
        assert!(narrowed.reads_pinned && !narrowed.can_read(&extra.join("secret")));
    }

    /// Windows enhanced tier: its built-in deny-read defaults don't keep an escalation sandboxed,
    /// a user-configured entry does (`acl`).
    #[test]
    fn only_user_deny_read_entries_keep_an_escalation_sandboxed() {
        let lua = Lua::new();
        let (_t, ws, extra) = dirs();
        let session = policy(&lua, r#"{ level = "workspace-write", network = false }"#, &ws).unwrap();
        // defaults only (acl = false): an approved `all` escalation leaves the sandbox
        assert!(decide(None, Some(session.clone()), false).is_none());
        // a user entry (acl = true): it runs with the session's sandbox, reads untouched
        let kept = decide(None, Some(session.clone()), true).expect("stays sandboxed");
        assert_eq!(kept.level, session.level);
        assert!(!kept.restrict_read && !kept.reads_pinned, "the ACEs deny the reads, not the policy");
        assert!(!kept.can_write(Path::new("/usr/mare-denyread2-out")), "the session's write roots only");
        // an increment keeps what it was granted either way
        lua.globals().set("EXTRA", extra.to_string_lossy().to_string()).unwrap();
        let inc = policy(&lua, r#"{ level = "workspace-write", writable = { EXTRA }, network = true }"#, &ws);
        let got = decide(inc, Some(session.clone()), true).unwrap();
        assert!(got.can_write(&extra.join("out")) && matches!(got.network, Network::Open));
        // reverse: no session sandbox (danger-full-access) → nothing to keep, even with entries
        assert!(decide(None, None, true).is_none());
        // a read restriction still wins over both
        let restricted = policy(&lua, r#"{ level = "workspace-write", restrict_read = true }"#, &ws).unwrap();
        for acl in [false, true] {
            assert!(decide(None, Some(restricted.clone()), acl).unwrap().reads_pinned);
        }
    }

    #[test]
    fn a_policy_within_the_session_reads_is_untouched() {
        let lua = Lua::new();
        let (_t, ws, _) = dirs();
        let session =
            policy(&lua, r#"{ level = "workspace-write", restrict_read = true, network = true }"#, &ws).unwrap();
        let same = preserve(Some(session.clone()), &session);
        assert!(!same.reads_pinned, "the session's own policy is not an escalation");
    }

    #[test]
    fn the_session_decides_whether_unenforced_reads_are_accepted() {
        let lua = Lua::new();
        let (_t, ws, _) = dirs();
        let session = policy(&lua, r#"{ restrict_read = true }"#, &ws).unwrap();
        let esc = policy(&lua, r#"{ network = true, accept_partial = { "fs_read", "network" } }"#, &ws).unwrap();
        let kept = preserve(Some(esc), &session);
        assert!(!kept.accept_partial.contains(&Cap::FsRead));
        assert!(kept.accept_partial.contains(&Cap::NetProxy));
    }

    #[test]
    fn a_backend_without_fs_read_reports_the_refusal() {
        let (ok, line) = capability_line(Backend::Landlock { partial: false }, &[Cap::FsWrite, Cap::NetBlock]);
        assert!(!ok && line.contains("refused") && line.contains("landlock") && line.contains("fs_read"), "{line}");
        assert!(line.contains("probe failed"), "the reason shows: {line}");
        let (ok, line) = capability_line(Backend::Bwrap, &[Cap::FsWrite, Cap::NetBlock, Cap::UnixSocket]);
        assert!(!ok && line.contains("bwrap") && line.contains("read-masking probe failed"), "{line}");
        let (ok, line) = capability_line(Backend::WinAcl, &[Cap::FsWrite, Cap::UnixSocket]);
        assert!(!ok && line.contains("windows-acl"), "{line}");
        let (ok, line) = capability_line(Backend::Seatbelt, &[Cap::FsRead, Cap::FsWrite]);
        assert!(ok && line.contains("stay sandboxed") && !line.contains("deny_read"), "{line}");
        // DENYREAD2: the Linux backends keep escalations sandboxed once their probe found fs_read
        for b in [Backend::Bwrap, Backend::Landlock { partial: false }] {
            let (ok, line) = capability_line(b, &[Cap::FsRead, Cap::FsWrite]);
            assert!(ok && line.contains("stay sandboxed") && line.contains(b.name()), "{line}");
        }
        let (ok, line) = capability_line(Backend::WinUser, &crate::ossandbox::backend::probe_caps(Backend::WinUser));
        assert!(!ok && line.contains("windows-user") && line.contains("fs_read"), "{line}");
        assert!(!capability_line(Backend::None, &[]).0);
        let r = refusal(Backend::Bwrap);
        assert!(r.starts_with(SANDBOX_INSUFFICIENT) && r.contains("fs_read") && r.contains("stays sandboxed"), "{r}");
    }
}

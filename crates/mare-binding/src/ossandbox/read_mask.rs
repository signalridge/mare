//! Restricted reads on Linux.
//!
//! mare's read restriction is an allow-list: with `restrict_read` a process reads only the
//! system roots, the policy's read roots (readable + writable, or readable alone when pinned by
//! [`crate::deny_read`]) and mare's own passthrough directories ([`visible`]). Everything else
//! is denied — there is no deny list to expand, so there are no globs, no expansion and nothing
//! to cap: the denied set is "the rest of the filesystem", described by what stays visible.
//!
//! - **bubblewrap** (restricted-read mode: `--tmpfs /` and only the readable roots bound back):
//!   the namespace starts from an empty
//!   tmpfs root instead of `--ro-bind / /`, each visible root is bound read-only on top, the
//!   writable roots are bound read-write afterwards as before, and the root is remounted
//!   read-only last, so a write outside the roots still fails with `EROFS` instead of landing in
//!   the throwaway tmpfs. A denied path is simply absent (`ENOENT`). A root that is writable but
//!   not readable (an approved `write:` grant while the reads are pinned) can't be expressed — a
//!   bind mount is readable — so such a call is refused ([`bwrap_check`]), never run with the
//!   extra reads.
//! - **Landlock** is an allow-list itself: read/execute rights are granted beneath the visible
//!   roots only (instead of beneath `/`), full rights beneath the writable roots that are
//!   visible, and write rights alone beneath the ones that aren't ([`landlock_grants`]).
//!   Reading and listing are handled since Landlock ABI 1, so every kernel the backend runs on
//!   can do it; the probe decides ([`landlock_probe`]).

use std::path::{Path, PathBuf};

use crate::ossandbox::{Backend, Policy, SANDBOX_INSUFFICIENT};

pub use super::prexec::LandlockGrants;

/// The roots a process under `policy` may read, outermost first and existing only; `None` =
/// reads unrestricted.
pub fn visible(policy: &Policy) -> Option<Vec<PathBuf>> {
    let mut v = system_roots();
    v.extend(policy.read_roots()?);
    Some(outermost(v.into_iter().filter(|p| p.exists()).collect()))
}

/// Full effective read set for host protection: `visible(policy)` unioned with `policy.writable` (temp
/// roots, workspace roots, and approved writable grants) and any backend `extra_binds` (such as
/// the netns proxy bridge). `None` when `policy.restrict_read` is false.
pub fn effective_read_set(policy: &Policy, extra_binds: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut v = visible(policy)?;
    v.extend(policy.writable.iter().cloned());
    v.extend(extra_binds.iter().cloned());
    Some(outermost(v))
}

fn system_roots() -> Vec<PathBuf> {
    crate::ossandbox::SYSTEM_READ_ROOTS.iter().map(PathBuf::from).collect()
}

/// `v` without the paths inside another one of it, sorted.
fn outermost(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v.dedup();
    let all = v.clone();
    v.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    v
}

fn covered(p: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

// ---- bubblewrap ----------------------------------------------------------------------------

/// Last bwrap mount operation under a read restriction: the tmpfs root read-only again (only
/// that mount — the writable binds and the private `/tmp` on it stay writable).
pub const BWRAP_SEAL: [&str; 2] = ["--remount-ro", "/"];

/// Replace the leading `--ro-bind / /` of `args` with an empty tmpfs root and `visible` bound
/// read-only on it (`/dev` and `/proc` are mounted fresh by the arguments that follow). Returns
/// whether it did: a visible `/` keeps the host root.
pub fn bwrap_mask(args: &mut Vec<String>, visible: &[PathBuf]) -> bool {
    debug_assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
    if visible.iter().any(|p| p == Path::new("/")) {
        return false;
    }
    let mut head: Vec<String> = vec!["--tmpfs".into(), "/".into()];
    for r in visible {
        if r.starts_with("/dev") || r.starts_with("/proc") {
            continue;
        }
        let s = r.to_string_lossy().into_owned();
        // `-try`: a root removed since it was checked is skipped, not a failed start
        head.extend(["--ro-bind-try".into(), s.clone(), s]);
    }
    args.splice(0..3, head);
    true
}

/// A bwrap call can't grant a write without the read: refuse a policy whose writable roots lie
/// outside its read set instead of exposing them.
pub fn bwrap_check(policy: &Policy) -> Result<(), String> {
    let Some(vis) = visible(policy) else { return Ok(()) };
    let write_only: Vec<String> = policy
        .workspace_roots()
        .into_iter()
        .filter(|w| !covered(w, &vis))
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    if write_only.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{SANDBOX_INSUFFICIENT}: this call may write {} but the session's read restriction doesn't let it read \
         there; the bwrap sandbox can't make a directory writable without making it readable (a bind mount is \
         both) — refusing rather than widening the reads. Add the directory to the readable roots or lift the \
         read restriction (/permission)",
        write_only.join(", ")
    ))
}

/// The bwrap arguments of the `fs_read` probe: masked root with only the system roots, sealed,
/// running a check that a path outside them (`/var`) is gone.
pub fn bwrap_probe_args(base: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    let vis = outermost(system_roots().into_iter().filter(|p| p.exists()).collect());
    bwrap_mask(&mut a, &vis);
    a.extend(BWRAP_SEAL.iter().map(|s| (*s).to_string()));
    a.extend(["--", "sh", "-c", "test ! -e /var"].map(String::from));
    a
}

// ---- Landlock ------------------------------------------------------------------------------

/// Landlock grants for `policy` given its writable paths `rw`.
pub fn landlock_grants(policy: &Policy, rw: Vec<PathBuf>) -> LandlockGrants {
    match visible(policy) {
        None => LandlockGrants { read: None, rw, write_only: vec![] },
        Some(vis) => {
            let (rw, write_only) = rw.into_iter().partition(|p| covered(p, &vis));
            LandlockGrants { read: Some(vis), rw, write_only }
        }
    }
}

/// The Landlock `fs_read` probe: a child reading only the system roots can't list `/var`.
#[cfg(target_os = "linux")]
pub fn landlock_probe() -> bool {
    use std::os::unix::process::CommandExt;
    let g = LandlockGrants {
        read: Some(outermost(system_roots().into_iter().filter(|p| p.exists()).collect())),
        rw: vec!["/dev/null".into()],
        write_only: vec![],
    };
    let mut c = std::process::Command::new("sh");
    c.args(["-c", "! ls /var >/dev/null 2>&1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: only the Landlock syscalls in the forked child (grants built here)
    unsafe {
        c.pre_exec(move || crate::ossandbox::child_restrict(Some(&g), None));
    }
    c.status().is_ok_and(|s| s.success())
}

/// Why `b` has no `fs_read` here, for `mare doctor` / `/permission caps`.
pub fn no_fs_read_reason(b: Backend) -> &'static str {
    match b {
        Backend::Bwrap => {
            " here: the read-masking probe failed (an empty tmpfs root with only the allowed roots bound, \
             remounted read-only) — an old bubblewrap, or MARE_SANDBOX_CAPS narrowed it"
        }
        Backend::Landlock { .. } => {
            " here: the read-restricting Landlock probe failed (read rights granted beneath the allowed roots \
             only) — or MARE_SANDBOX_CAPS narrowed it"
        }
        Backend::WinAcl => " (the default tier never restricts reads; `mare sandbox setup` installs the enhanced tier)",
        Backend::WinUser => {
            " (Windows enhanced accounts can still read Users-readable volumes; allowlist fs_read is not enforced)"
        }
        _ => "",
    }
}

/// `report()` line for a read-restricted call on a Linux backend.
pub fn reads_note(b: Backend) -> Option<&'static str> {
    match b {
        Backend::Bwrap => Some("masked: only the allowed roots are mounted (empty read-only root)"),
        Backend::Landlock { .. } => Some("controlled: Landlock read rights beneath the allowed roots only"),
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ossandbox::classify::hidden_read;
    use mlua::{Lua, Value};

    fn policy(spec: &str, cwd: &Path) -> Policy {
        let lua = Lua::new();
        let v: Value = lua.load(spec).eval().unwrap();
        Policy::from_lua(&lua, &v, cwd).unwrap().unwrap()
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
    fn the_visible_set_is_outermost_and_existing() {
        assert_eq!(
            outermost(vec!["/a/b".into(), "/a".into(), "/c".into(), "/a".into()]),
            vec![PathBuf::from("/a"), PathBuf::from("/c")]
        );
        let (_t, ws, _) = dirs();
        let p = policy(r#"{ level = "workspace-write", restrict_read = true }"#, &ws);
        let v = visible(&p).unwrap();
        assert!(covered(&ws.join("f"), &v), "{v:?}");
        assert!(v.iter().all(|r| r.exists()));
        assert!(visible(&policy(r#"{ level = "workspace-write" }"#, &ws)).is_none());
    }

    #[test]
    fn bwrap_starts_from_an_empty_root_under_restricted_reads() {
        let (_t, ws, _) = dirs();
        let p = policy(r#"{ level = "workspace-write", restrict_read = true }"#, &ws);
        let a = p.bwrap_args(&ws);
        assert_eq!(a[..2], ["--tmpfs", "/"]);
        assert!(!a.windows(3).any(|w| w == ["--ro-bind", "/", "/"]), "{a:?}");
        let ws_s = ws.to_string_lossy().into_owned();
        assert!(a.windows(3).any(|w| w == ["--bind", ws_s.as_str(), ws_s.as_str()]), "{a:?}");
        // sealed after the last mount (only namespace flags and --chdir follow)
        let seal = a.windows(2).position(|w| w == BWRAP_SEAL).unwrap();
        assert!(a[seal + 2..].iter().all(|s| !s.ends_with("bind") && !s.ends_with("bind-try") && s != "--tmpfs"));
        assert!(a.iter().all(|s| s != "/dev" || a.contains(&"--dev".to_string())));
        // reverse check: without the restriction the host root stays
        let open = policy(r#"{ level = "workspace-write" }"#, &ws).bwrap_args(&ws);
        assert_eq!(open[..3], ["--ro-bind", "/", "/"]);
        assert!(!open.windows(2).any(|w| w == BWRAP_SEAL));
        // a readable `/` keeps the host root too
        let mut b: Vec<String> = ["--ro-bind", "/", "/"].map(String::from).to_vec();
        assert!(!bwrap_mask(&mut b, &[PathBuf::from("/")]));
    }

    #[test]
    fn a_write_outside_the_pinned_reads_is_refused_on_bwrap() {
        let lua = Lua::new();
        let (_t, ws, _) = dirs();
        // outside the temp areas, which the session reads
        let home = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let extra = dunce::canonicalize(home.path()).unwrap();
        lua.globals().set("EXTRA", extra.to_string_lossy().to_string()).unwrap();
        let session = policy(r#"{ level = "workspace-write", restrict_read = true }"#, &ws);
        let v: Value = lua.load(r#"{ level = "workspace-write", writable = { EXTRA } }"#).eval().unwrap();
        let esc = Policy::from_lua(&lua, &v, &ws).unwrap().unwrap();
        let kept = crate::deny_read::preserve(Some(esc), &session);
        let e = bwrap_check(&kept).unwrap_err();
        assert!(e.starts_with(SANDBOX_INSUFFICIENT) && e.contains(&*extra.to_string_lossy()), "{e}");
        assert!(bwrap_check(&session).is_ok(), "the session's own roots are readable");
        // Landlock grants it as write-only instead
        let g = landlock_grants(&kept, kept.workspace_roots());
        assert!(g.write_only.contains(&extra) && g.rw.contains(&ws), "{g:?}");
        assert!(!covered(&extra, g.read.as_ref().unwrap()));
    }

    #[test]
    fn landlock_reads_everywhere_without_a_restriction() {
        let (_t, ws, _) = dirs();
        let p = policy(r#"{ level = "workspace-write" }"#, &ws);
        let g = landlock_grants(&p, vec![ws.clone()]);
        assert_eq!(g, LandlockGrants { read: None, rw: vec![ws], write_only: vec![] });
    }

    #[test]
    fn a_masked_path_reads_as_a_read_denial_on_bwrap() {
        let (_t, ws, _) = dirs();
        let p = policy(r#"{ level = "workspace-write", restrict_read = true }"#, &ws);
        let line = "cat: /nonexistent-home/u/.ssh/id: No such file or directory";
        let d = hidden_read(&p, &ws, line, Backend::Bwrap).unwrap();
        assert_eq!((d.kind, d.path.as_deref()), ("read", Some("/nonexistent-home/u/.ssh/id")));
        assert!(hidden_read(&p, &ws, line, Backend::Seatbelt).is_none());
        let inside = format!("cat: {}: No such file or directory", ws.join("gone").display());
        assert!(hidden_read(&p, &ws, &inside, Backend::Bwrap).is_none(), "a missing file inside is the program's");
    }
}

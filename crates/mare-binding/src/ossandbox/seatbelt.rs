//! macOS Seatbelt: a policy's SBPL profile.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::roots::{SYSTEM_READ_ROOTS, WRITE_DEVICES};
use super::{Network, Policy};

pub(super) const SEATBELT_EXEC: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_cast)]
const MACOS_TIOCSTI: u64 = libc::TIOCSTI as u64;
#[cfg(not(target_os = "macos"))]
const MACOS_TIOCSTI: u64 = 0x8001_7472;

fn collect_protected_ancestors(writable_roots: &[PathBuf], protected: &[PathBuf], out: &mut BTreeSet<PathBuf>) {
    for root in writable_roots {
        let resolved_root = crate::realpath::resolve(root).unwrap_or_else(|_| root.clone());
        for path in protected {
            let resolved_path = crate::realpath::resolve(path).ok().filter(|r| r != path);
            for target in std::iter::once(path).chain(resolved_path.iter()) {
                for base in [root, &resolved_root] {
                    for ancestor in target.ancestors().skip(1) {
                        if !ancestor.starts_with(base) {
                            break;
                        }
                        out.insert(ancestor.to_path_buf());
                    }
                }
            }
        }
    }
}

impl Policy {
    /// Seatbelt profile (SBPL). `proxy_port` = loopback port the process may connect to.
    pub fn seatbelt(&self, proxy_port: Option<u16>) -> String {
        if let Some(p) = &self.profile {
            return p.clone();
        }
        let q = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let mut s = String::from("(version 1)\n(allow default)\n");
        s.push_str("(deny file-write*)\n(allow file-write*\n");
        for d in WRITE_DEVICES {
            let _ = writeln!(s, "  (literal \"{d}\")");
        }
        s.push_str("  (regex #\"^/dev/fd/\") (regex #\"^/dev/tty\")\n");
        for w in &self.writable {
            let _ = writeln!(s, "  (subpath \"{}\")", q(w));
        }
        s.push_str(")\n");
        // read-only subpaths (.git, .mare); later rules win, so grants inside them come after
        let protected = self.protected_subpaths();
        let granted = if protected.is_empty() { vec![] } else { self.granted_in_protected() };
        if !protected.is_empty() {
            s.push_str("(deny file-write*\n");
            for p in &protected {
                let qp = q(p);
                let _ = writeln!(s, "  (literal \"{qp}\")");
                let _ = writeln!(s, "  (subpath \"{qp}\")");
            }
            s.push_str(")\n");
            if !granted.is_empty() {
                s.push_str("(allow file-write*\n");
                for g in &granted {
                    let qg = q(g);
                    let _ = writeln!(s, "  (literal \"{qg}\")");
                    let _ = writeln!(s, "  (subpath \"{qg}\")");
                }
                s.push_str(")\n");
            }
        }
        if self.restrict_read {
            s.push_str("(deny file-read*)\n(allow file-read* (literal \"/\")\n");
            for r in SYSTEM_READ_ROOTS {
                let _ = writeln!(s, "  (subpath \"{r}\")");
            }
            for r in self.read_roots().unwrap_or_default() {
                let _ = writeln!(s, "  (subpath \"{}\")", q(&r));
            }
            // stat/readdir of the ancestors of allowed roots (path traversal)
            s.push_str(")\n(allow file-read-metadata)\n");
        }
        if let Network::Proxy(_) = self.network {
            // Unix sockets: only the listed ones (askpass included) — the policy layer allows
            // exactly the same set
            s.push_str("(deny network*)\n");
            for sock in self.allowed_unix_sockets() {
                let _ = writeln!(s, "(allow network-outbound (remote unix-socket (path-literal \"{}\")))", q(&sock));
            }
            if let Some(port) = proxy_port {
                let _ = writeln!(s, "(allow network-outbound (remote ip \"localhost:{port}\"))");
            }
        }
        // Keep this after all read and write grants. Deny both file-read* and file-write*
        // with both (literal ...) and (subpath ...) so denied files/dirs cannot be renamed out.
        // Protected subpaths explicitly reopened by a grant no longer need directory-unlink
        // pinning inside the reopened subtree.
        let mut ancestor_targets: Vec<PathBuf> =
            protected.into_iter().filter(|p| !granted.iter().any(|g| p.starts_with(g))).collect();
        if self.deny_read.iter().any(|d| d.is_dir.is_some()) {
            s.push_str("(deny file-read* file-write*\n");
            for d in self.deny_read.iter().filter(|d| d.is_dir.is_some()) {
                let qt = q(&d.target);
                let _ = writeln!(s, "  (literal \"{qt}\")");
                let _ = writeln!(s, "  (subpath \"{qt}\")");
                ancestor_targets.push(d.target.clone());
                if d.input != d.target {
                    let qi = q(&d.input);
                    let _ = writeln!(s, "  (literal \"{qi}\")");
                    let _ = writeln!(s, "  (subpath \"{qi}\")");
                    ancestor_targets.push(d.input.clone());
                }
            }
            s.push_str(")\n");
        }
        for entry in super::protected::host_protected() {
            let Ok(resolved) = crate::realpath::resolve(&entry.path) else {
                s.push_str("(deny file-read* file-write*)\n");
                continue;
            };
            let qr = q(&resolved);
            let _ = writeln!(s, "(deny file-read* file-write* (literal \"{qr}\"))");
            let _ = writeln!(s, "(deny file-read* file-write* (subpath \"{qr}\"))");
            ancestor_targets.push(resolved.clone());
            if entry.path != resolved {
                let qe = q(&entry.path);
                let _ = writeln!(s, "(deny file-read* file-write* (literal \"{qe}\"))");
                let _ = writeln!(s, "(deny file-read* file-write* (subpath \"{qe}\"))");
                ancestor_targets.push(entry.path.clone());
            }
        }
        // Protect writable ancestors of all protected/denied/host-protected paths against directory
        // unlink/rename so pathname carveouts cannot be relocated out of their rules.
        let mut protected_ancestors = BTreeSet::new();
        collect_protected_ancestors(&self.writable, &ancestor_targets, &mut protected_ancestors);
        for ancestor in protected_ancestors {
            let _ = writeln!(
                s,
                "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal \"{}\")))",
                q(&ancestor)
            );
        }
        // Block fcntls that mutate files through read-only fds (F_MAKECOMPRESSED=80, F_TRANSFEREXTENTS=110)
        // and block TIOCSTI terminal input injection.
        s.push_str("(deny system-fcntl (fcntl-command 80 110))\n");
        let _ = writeln!(s, "(deny file-ioctl (ioctl-command {MACOS_TIOCSTI}))");
        s
    }
}

/// Whether a generated Seatbelt `profile` contains the deny rule for every `Refuse` entry in
/// [`super::protected::host_protected`].
pub(super) fn profile_masks_host_protected(profile: &str) -> bool {
    let q = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    super::protected::host_protected().into_iter().filter(|e| e.on_gap == super::protected::GapAction::Refuse).all(
        |entry| {
            let Ok(resolved) = crate::realpath::resolve(&entry.path) else { return false };
            let qr = q(&resolved);
            let lit_rule = format!("(deny file-read* file-write* (literal \"{qr}\"))");
            let sub_rule = format!("(deny file-read* file-write* (subpath \"{qr}\"))");
            profile.contains(&lit_rule) && profile.contains(&sub_rule)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ossandbox::Level;
    use mlua::{Lua, Value};

    #[test]
    fn seatbelt_profile_emits_literal_subpath_ancestor_unlink_fcntl_and_tiocsti() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let nested = root.join("sub");
        let secret = nested.join("secret.txt");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&secret, "top-secret").unwrap();

        let lua = Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set("level", "workspace-write").unwrap();
        spec.set("deny_read", vec![secret.to_string_lossy().into_owned()]).unwrap();
        let p = Policy::from_lua(&lua, &Value::Table(spec), &root).unwrap().unwrap();
        let profile = p.seatbelt(None);

        let q = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let mare = root.join(".mare");
        assert!(profile.contains(&format!("(literal \"{}\")", q(&mare))), "{profile}");
        assert!(profile.contains(&format!("(subpath \"{}\")", q(&mare))), "{profile}");
        assert!(profile.contains("(deny file-read* file-write*\n"), "{profile}");
        assert!(profile.contains(&format!("(literal \"{}\")", q(&secret))), "{profile}");
        assert!(profile.contains(&format!("(subpath \"{}\")", q(&secret))), "{profile}");
        for anc in [&root, &nested] {
            let rule =
                format!("(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal \"{}\")))", q(anc));
            assert!(profile.contains(&rule), "missing ancestor rule {rule} in:\n{profile}");
        }
        assert!(profile.contains("(deny system-fcntl (fcntl-command 80 110))"), "{profile}");
        assert!(profile.contains(&format!("(deny file-ioctl (ioctl-command {MACOS_TIOCSTI}))")), "{profile}");
    }

    #[test]
    fn seatbelt_readonly_also_blocks_fcntl_and_tiocsti() {
        let dir = tempfile::tempdir().unwrap();
        let p = Policy::build(Level::ReadOnly, dir.path(), None, &Value::Boolean(true)).unwrap();
        let profile = p.seatbelt(None);
        assert!(profile.contains("(deny system-fcntl (fcntl-command 80 110))"));
        assert!(profile.contains(&format!("(deny file-ioctl (ioctl-command {MACOS_TIOCSTI}))")));
    }

    #[test]
    fn seatbelt_granted_external_git_allows_write_without_synthesizing_git_mare() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let ext_repo = root.join("ext-repo");
        let ext_git = ext_repo.join(".git");
        let ext_other = root.join("ext-other");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(&ext_git).unwrap();
        std::fs::create_dir_all(ext_other.join(".git")).unwrap();

        let lua = Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set("level", "workspace-write").unwrap();
        spec.set("writable", vec![ext_git.to_string_lossy().into_owned(), ext_other.to_string_lossy().into_owned()])
            .unwrap();
        let p = Policy::from_lua(&lua, &Value::Table(spec), &ws).unwrap().unwrap();
        let protected = p.protected_subpaths();
        assert!(!protected.contains(&ext_git.join(".mare")), "must not synthesize .mare inside .git: {protected:?}");
        assert!(protected.contains(&ws.join(".git")), "workspace .git must stay protected: {protected:?}");
        assert!(protected.contains(&ext_other.join(".git")), "ordinary extra root .git must stay protected");
        assert!(protected.contains(&ext_other.join(".mare")), "ordinary extra root .mare must stay protected");

        let profile = p.seatbelt(None);
        let q = |p: &Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        assert!(profile.contains(&format!("(subpath \"{}\")", q(&ext_git))), "{profile}");
        assert!(!profile.contains(&q(&ext_git.join(".mare"))), "unexpected .git/.mare rule:\n{profile}");
        assert!(
            !profile.contains(&format!(
                "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal \"{}\")))",
                q(&ext_git)
            )),
            "must not emit directory unlink deny for granted external .git:\n{profile}"
        );

        // When both an external repo root and its `.git` are granted, `.git` is reopened by a
        // trailing allow rule and has no residual directory-unlink deny on `.git` itself.
        let spec2 = lua.create_table().unwrap();
        spec2.set("level", "workspace-write").unwrap();
        spec2
            .set("writable", vec![ext_repo.to_string_lossy().into_owned(), ext_git.to_string_lossy().into_owned()])
            .unwrap();
        let p2 = Policy::from_lua(&lua, &Value::Table(spec2), &ws).unwrap().unwrap();
        let profile2 = p2.seatbelt(None);
        let deny_idx = profile2.find("(deny file-write*\n  (literal \"").unwrap();
        assert!(
            profile2[deny_idx..].contains(&format!("(literal \"{}\")", q(&ext_git))),
            "protected deny block must still list ext_git:\n{profile2}"
        );
        let allow_idx = profile2[deny_idx..].find(&format!("(allow file-write*\n  (literal \"{}\")", q(&ext_git)));
        assert!(allow_idx.is_some(), "explicitly granted .git must be reopened after deny:\n{profile2}");
        assert!(
            !profile2.contains(&format!(
                "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal \"{}\")))",
                q(&ext_git)
            )),
            "reopened .git must not keep directory unlink deny:\n{profile2}"
        );
    }
}

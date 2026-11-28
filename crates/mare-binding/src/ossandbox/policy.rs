//! Policy algebra (no Lua state): building, required dimensions, writable and protected roots,
//! intersection.

use std::path::{Path, PathBuf};

use mare_shared::lua::rt_err;
use mlua::{Table, Value};

use super::caps::Cap;
use super::level::Level;
use super::roots::{dedup, temp_roots, validate_root};
use super::{Network, Policy};
use crate::netproxy::NetPolicy;
use crate::realpath::{expand, read_commondir_from_gitdir, read_gitdir_from_file, resolve};

fn push_with_target(out: &mut Vec<PathBuf>, path: PathBuf) -> std::io::Result<()> {
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        out.push(resolve(&path)?);
    }
    out.push(path);
    Ok(())
}

fn is_git_metadata_root(root: &Path) -> bool {
    root.components().any(|c| match c {
        std::path::Component::Normal(name) => {
            if cfg!(any(target_os = "macos", windows)) {
                name.to_string_lossy().eq_ignore_ascii_case(".git")
            } else {
                name == ".git"
            }
        }
        _ => false,
    })
}

fn push_root_protected(out: &mut Vec<PathBuf>, root: &Path) -> std::io::Result<()> {
    let git = root.join(".git");
    if mare_shared::git::marker_present(root) {
        if git.is_file()
            && let Some(gitdir) = read_gitdir_from_file(&git)?
        {
            let resolved_gitdir = resolve(&gitdir)?;
            if let Some(commondir) = read_commondir_from_gitdir(&resolved_gitdir)? {
                out.push(commondir);
            }
            out.push(resolved_gitdir);
        }
        push_with_target(out, git)?;
    }
    push_with_target(out, root.join(".mare"))?;
    Ok(())
}

fn project_resource_protected(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut v = vec![];
    let rr = mare_shared::project::resource_root(root);
    if let Some(mare) = rr.mare_dir() {
        push_with_target(&mut v, mare)?;
    }
    if let Some(proj_root) = rr.root {
        let mcp = proj_root.join(".mcp.json");
        match std::fs::symlink_metadata(&mcp) {
            Ok(_) => push_with_target(&mut v, mcp)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing `<root>/.mcp.json` is not protected because bwrap only
                // synthesizes a missing `.mare`; MCP project servers still need their own
                // approval.
            }
            Err(_) => v.push(mcp),
        }
    }
    Ok(v)
}

/// Protected subpaths inside `root` (`.mare`, `.git` + worktree gitdir, resource root `.mare`, `.mcp.json`).
pub(crate) fn root_protected_subpaths(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let root = resolve(root)?;
    let mut out = vec![];
    push_root_protected(&mut out, &root)?;
    out.extend(project_resource_protected(&root)?);
    Ok(dedup(out))
}

impl Policy {
    pub(super) fn build(level: Level, cwd: &Path, t: Option<&Table>, net: &Value) -> mlua::Result<Self> {
        let list = |k: &str| -> mlua::Result<Vec<String>> {
            Ok(match t {
                Some(t) => t.get::<Option<Vec<String>>>(k)?.unwrap_or_default(),
                None => vec![],
            })
        };
        let exclude = |name: &str| -> mlua::Result<bool> {
            match t.map(|t| t.get::<Value>(name)).transpose()?.unwrap_or(Value::Nil) {
                Value::Nil => Ok(false),
                Value::Boolean(value) => Ok(value),
                _ => Err(rt_err(format!("sandbox.{name}: expected boolean"))),
            }
        };
        // `write = {...}` replaces the workspace root as the root set; `writable` adds to it
        let replaced: Option<Vec<String>> = match t {
            Some(t) => t.get("write")?,
            None => None,
        };
        let mut extra_raw: Vec<PathBuf> = list("writable")?.iter().map(|p| mare_shared::paths::expand(p)).collect();
        extra_raw.extend(replaced.iter().flatten().map(|p| expand(p, cwd)));
        let extra = dedup(extra_raw.iter().map(|p| validate_root(p)).collect::<Result<Vec<_>, _>>().map_err(rt_err)?);
        // a missing field is the strictest value: no `network` = blocked,
        // at every level — "not written" never means "open"
        let network = match net {
            Value::Nil => Network::Proxy(NetPolicy::deny_all()),
            Value::Boolean(true) => Network::Open,
            Value::Boolean(false) => Network::Proxy(NetPolicy::deny_all()),
            Value::Table(t) => {
                Network::Proxy(NetPolicy::from_lua(t).map_err(|e| rt_err(format!("sandbox.network: {e}")))?)
            }
            _ => return Err(rt_err("sandbox.network: expected boolean or table")),
        };
        let mut p = Self {
            level,
            root: resolve(cwd).map_err(rt_err)?,
            extra,
            root_writable: replaced.is_none(),
            exclude_tmpdir_env_var: exclude("exclude_tmpdir_env_var")?,
            exclude_slash_tmp: exclude("exclude_slash_tmp")?,
            writable: vec![],
            readable: dedup(
                list("readable")?
                    .iter()
                    .map(|p| resolve(&expand(p, cwd)))
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(rt_err)?,
            ),
            deny_read: list("deny_read")?
                .iter()
                .map(|p| super::deny_path::build(p, cwd).map_err(rt_err))
                .collect::<mlua::Result<Vec<_>>>()?,
            restrict_read: match t {
                Some(t) => t.get::<Option<bool>>("restrict_read")?.unwrap_or(false),
                None => false,
            },
            reads_pinned: false,
            network,
            profile: match t {
                Some(t) => t.get("profile")?,
                None => None,
            },
            unix_sockets: dedup(
                list("unix_sockets")?
                    .iter()
                    .map(|p| resolve(&expand(p, cwd)))
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(rt_err)?,
            ),
            accept_partial: {
                let mut v = vec![];
                for n in list("accept_partial")? {
                    v.extend(Cap::parse_accept(&n).map_err(rt_err)?);
                }
                v.sort();
                v.dedup();
                v
            },
            project_protected: project_resource_protected(cwd).map_err(rt_err)?,
        };
        p.writable = p.compute_writable();
        Ok(p)
    }

    /// Dimensions this policy needs enforced.
    pub fn required(&self) -> Vec<Cap> {
        self.required_for_paths(&self.protected_subpaths())
    }

    pub(super) fn required_for_paths(&self, paths: &[PathBuf]) -> Vec<Cap> {
        let mut v = vec![Cap::FsWrite];
        if self.level == Level::WorkspaceWrite {
            let grants: Vec<_> = self.extra.iter().filter(|g| paths.iter().any(|p| g.starts_with(p))).collect();
            if paths.iter().any(|p| !grants.iter().any(|g| p.starts_with(g))) {
                v.push(Cap::FsProtect);
            }
        }
        if self.restrict_read {
            v.push(Cap::FsRead);
        }
        if !self.deny_read.is_empty() {
            v.push(Cap::FsDenyRead);
        }
        // require `fs_host_protect` whenever any registered Refuse-on-gap secret is
        // `Present` or `Unknown`. Whether it is satisfied by a mask or by `fs_read` is decided
        // in `host_protect_satisfied` / `Prepared::from_policy`.
        if super::protected::host_protected().iter().any(|entry| {
            entry.on_gap == super::protected::GapAction::Refuse
                && super::protected::secret_status(entry).is_present_or_unknown()
        }) {
            v.push(Cap::FsHostProtect);
        }
        if let Network::Proxy(np) = &self.network {
            v.push(if np.is_deny_all() { Cap::NetBlock } else { Cap::NetProxy });
            // a restricted network must not leak through local daemons' sockets
            v.push(Cap::UnixSocket);
        }
        v
    }

    /// Unix sockets reachable under this policy's restricted network. Restricted sandboxes never
    /// expose the TUI askpass socket (setuid `sudo` is blocked by userns / `NO_NEW_PRIVS` anyway).
    pub fn allowed_unix_sockets(&self) -> Vec<PathBuf> {
        dedup(self.unix_sockets.clone())
    }

    pub(super) fn compute_writable(&self) -> Vec<PathBuf> {
        if self.level != Level::WorkspaceWrite {
            return vec![];
        }
        let mut w = self.workspace_roots();
        w.extend(temp_roots(self.exclude_tmpdir_env_var, self.exclude_slash_tmp));
        dedup(w)
    }

    /// Workspace root (unless replaced) + extra roots — the set bind-mounted / granted by every
    /// backend (temp areas are each backend's own business).
    pub fn workspace_roots(&self) -> Vec<PathBuf> {
        if self.level != Level::WorkspaceWrite {
            return vec![];
        }
        let mut w = vec![];
        if self.root_writable {
            w.push(self.root.clone());
        }
        w.extend(self.extra.iter().cloned());
        dedup(w)
    }

    /// The same policy at another level (an approved one-call escalation); `None` for
    /// danger-full-access.
    pub fn with_level(&self, level: Level) -> Option<Self> {
        if level == Level::FullAccess {
            return None;
        }
        // only the file-write dimension changes: network, reads and sockets stay as they were
        let mut p = self.clone();
        p.level = level;
        p.writable = p.compute_writable();
        Some(p)
    }

    /// Intersection with another policy: the
    /// dimensions are independent and each takes what both sides allow — file writes only under
    /// roots both grant, the network only where both open it, sockets both list. Used where a
    /// policy may only narrow another (a tool's own policy inside a tool call, an approval
    /// answer against the request). Never fails: what can't be matched is dropped.
    pub fn intersect(&self, o: &Self) -> Self {
        // a raw profile is opaque: keep this side's (the context's) when both set one
        let profile = self.profile.clone().or(o.profile.clone());
        let network = match (&self.network, &o.network) {
            (Network::Open, n) | (n, Network::Open) => n.clone(),
            (Network::Proxy(a), Network::Proxy(b)) => {
                if a.is_deny_all() || b.is_deny_all() {
                    Network::Proxy(NetPolicy::deny_all())
                } else {
                    Network::Proxy(a.intersect(b))
                }
            }
        };
        let level = self.level.min(o.level);
        let mut p = self.clone();
        p.level = level;
        p.network = network;
        p.profile = profile;
        p.exclude_tmpdir_env_var = self.exclude_tmpdir_env_var || o.exclude_tmpdir_env_var;
        p.exclude_slash_tmp = self.exclude_slash_tmp || o.exclude_slash_tmp;
        for d in &o.deny_read {
            if !p.deny_read.contains(d) {
                p.deny_read.push(d.clone());
            }
        }
        p.restrict_read = self.restrict_read || o.restrict_read;
        p.readable = match (self.restrict_read, o.restrict_read) {
            (true, true) => within(&self.readable, &o.readable),
            (true, false) => self.readable.clone(),
            (false, true) => o.readable.clone(),
            (false, false) => dedup(self.readable.iter().chain(o.readable.iter()).cloned().collect()),
        };
        intersect_pinned(self, o, &mut p);
        p.unix_sockets = self.unix_sockets.iter().filter(|s| o.unix_sockets.contains(s)).cloned().collect();
        p.accept_partial = self.accept_partial.iter().filter(|c| o.accept_partial.contains(c)).copied().collect();
        // A policy without deny-read imposes no additional deny-read requirement. Requiring it
        // to repeat another policy's acceptance loses provenance during ordinary narrowing.
        // If both specify denies, each must accept its own gap, even for the same path.
        if !p.deny_read.is_empty() {
            let accepts = (self.deny_read.is_empty() || self.accept_partial.contains(&Cap::FsDenyRead))
                && (o.deny_read.is_empty() || o.accept_partial.contains(&Cap::FsDenyRead));
            p.accept_partial.retain(|c| *c != Cap::FsDenyRead);
            if accepts {
                p.accept_partial.push(Cap::FsDenyRead);
                p.accept_partial.sort();
            }
        }
        if level == Level::WorkspaceWrite {
            let mut roots = within(&self.workspace_roots(), &o.workspace_roots());
            if p.exclude_tmpdir_env_var || p.exclude_slash_tmp {
                // One side may allow a default temp root while the other permits only an
                // explicit descendant. Keep that descendant, never resurrect the parent.
                let effective = within(&self.writable, &o.writable);
                let implicit = temp_roots(p.exclude_tmpdir_env_var, p.exclude_slash_tmp);
                roots.extend(effective.iter().filter(|r| !implicit.contains(r)).cloned());
                // Preserve explicit workspace-root provenance (and its metadata protection)
                // whenever the complete root remains inside the effective intersection.
                roots.extend(
                    self.workspace_roots()
                        .into_iter()
                        .chain(o.workspace_roots())
                        .filter(|r| effective.iter().any(|allowed| r.starts_with(allowed))),
                );
            }
            let roots = dedup(roots);
            p.root_writable = roots.contains(&p.root);
            p.extra = roots.into_iter().filter(|r| *r != p.root).collect();
        } else {
            p.extra = vec![];
        }
        p.writable = p.compute_writable();
        for path in &o.project_protected {
            if !p.project_protected.contains(path) {
                p.project_protected.push(path.clone());
            }
        }
        p
    }

    /// Re-check the extra roots right before a spawn (one swapped for a symlink is refused).
    pub fn revalidate(&self) -> Result<(), String> {
        for r in &self.extra {
            validate_root(r)?;
        }
        for d in &self.deny_read {
            super::deny_path::validate(d)?;
        }
        Ok(())
    }

    /// Read-only subpaths inside workspace roots: an existing `<root>/.git` (including a
    /// worktree pointer's gitdir) and `<root>/.mare`, including before it exists.
    pub fn protected_subpaths(&self) -> Vec<PathBuf> {
        let mut out = vec![];
        for root in self.workspace_roots() {
            // An explicitly granted `.git` (or worktree gitdir under `.git`) is git metadata,
            // not a project root: never synthesize `<root>/.mare` inside an external `.git`.
            if root != self.root && is_git_metadata_root(&root) {
                continue;
            }
            if push_root_protected(&mut out, &root).is_err() {
                out.push(root.clone());
            }
        }
        // Protect `<root>/.mare` and existing `<root>/.mcp.json` whenever the resource root is
        // inside (or equal to) a writable workspace root (`dedup` collapses workspace `.mare`).
        let ws = self.workspace_roots();
        for p in &self.project_protected {
            if let Some(parent) = p.parent()
                && ws.iter().any(|w| parent.starts_with(w))
            {
                out.push(p.clone());
            }
        }
        dedup(out)
    }

    /// Extra roots granted inside a protected subpath (an approved `write:.git`): writable again,
    /// only their own subtree.
    pub fn granted_in_protected(&self) -> Vec<PathBuf> {
        let protected = self.protected_subpaths();
        self.extra.iter().filter(|e| protected.iter().any(|s| e.starts_with(s))).cloned().collect()
    }
}

impl Policy {
    /// The roots a process may read besides the system roots; `None` = reads unrestricted.
    pub fn read_roots(&self) -> Option<Vec<PathBuf>> {
        if !self.restrict_read {
            return None;
        }
        let mut v = self.readable.clone();
        if !self.reads_pinned {
            v.extend(self.writable.iter().cloned());
        }
        Some(dedup(v))
    }
}

/// Roots of `a` inside some root of `b`, and roots of `b` inside some root of `a`: the paths
/// both sides cover.
pub(crate) fn within(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = a.iter().filter(|x| b.iter().any(|y| x.starts_with(y))).cloned().collect();
    v.extend(b.iter().filter(|y| a.iter().any(|x| y.starts_with(x))).cloned());
    dedup(v)
}

/// [`Policy::intersect`] with a pinned side: the result reads only where both sides read, and
/// stays pinned (a tool's own policy never widens an escalation's kept reads).
pub(crate) fn intersect_pinned(a: &Policy, b: &Policy, out: &mut Policy) {
    if !(a.reads_pinned || b.reads_pinned) {
        return;
    }
    let roots = match (a.read_roots(), b.read_roots()) {
        (Some(x), Some(y)) => within(&x, &y),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => return,
    };
    out.restrict_read = true;
    out.readable = roots;
    out.reads_pinned = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_pinned_handles_both_restricted_and_both_unrestricted() {
        let mut a = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        a.writable.clear();
        a.restrict_read = true;
        a.readable = vec![PathBuf::from("/tmp/a")];
        a.reads_pinned = true;
        let mut b = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        b.writable.clear();
        b.restrict_read = true;
        b.readable = vec![PathBuf::from("/tmp/a/sub")];
        let mut out = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        intersect_pinned(&a, &b, &mut out);
        assert!(out.restrict_read && out.reads_pinned);
        assert_eq!(out.readable, vec![PathBuf::from("/tmp/a/sub")]);

        let mut c = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        c.reads_pinned = true;
        let d = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        let mut out2 = Policy::build(Level::WorkspaceWrite, Path::new("/tmp"), None, &Value::Nil).unwrap();
        intersect_pinned(&c, &d, &mut out2);
        assert!(!out2.restrict_read);
    }

    #[test]
    fn network_option_rejects_invalid_lua_type() {
        let lua = mlua::Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.set("network", 42).unwrap();
        let err = Policy::from_lua(&lua, &mlua::Value::Table(tbl), Path::new("/tmp")).unwrap_err();
        assert!(err.to_string().contains("sandbox.network: expected boolean or table"), "{err}");
    }
}

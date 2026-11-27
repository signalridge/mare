//! Policy-layer access checks, with the rules the spawned sandbox enforces.

use std::path::{Path, PathBuf};

use super::backend::backend;
use super::caps::Cap;
use super::roots::{SYSTEM_READ_ROOTS, is_write_device};
use super::{Backend, Denial, Network, Policy, SANDBOX_INSUFFICIENT, SANDBOX_UNAVAILABLE};
use crate::realpath::{expand, path_within, resolve};

pub(crate) struct ProtectedHostMatcher {
    entries: Vec<(PathBuf, Option<PathBuf>, super::protected::HostShape)>,
}

impl ProtectedHostMatcher {
    pub(crate) fn load() -> Self {
        let entries = super::protected::host_protected()
            .into_iter()
            .map(|entry| {
                let resolved = resolve(&entry.path).ok();
                (entry.path, resolved, entry.shape)
            })
            .collect();
        Self { entries }
    }

    pub(crate) fn matches_part(&self, part: &Path) -> bool {
        self.entries.iter().any(|(raw, resolved, _)| {
            path_within(part, raw) || resolved.as_ref().is_none_or(|resolved_entry| path_within(part, resolved_entry))
        })
    }

    pub(crate) fn matches_resolved(&self, resolved_path: &Path, include_ancestors: bool) -> bool {
        self.entries.iter().any(|(_, resolved, shape)| {
            let Some(resolved_entry) = resolved else {
                return true;
            };
            match shape {
                super::protected::HostShape::Dir => {
                    path_within(resolved_path, resolved_entry)
                        || (include_ancestors && path_within(resolved_entry, resolved_path))
                }
                super::protected::HostShape::File => {
                    resolved_path == resolved_entry || (include_ancestors && path_within(resolved_entry, resolved_path))
                }
            }
        })
    }
}

fn is_protected_host_token_path(path: &Path, include_ancestors: bool) -> bool {
    let Some(resolved_path) = super::deny_path::access_path(path) else {
        return true;
    };
    ProtectedHostMatcher::load().matches_resolved(&resolved_path, include_ancestors)
}

impl Policy {
    pub fn can_write_deny_read(&self, path: &Path) -> bool {
        if is_protected_host_token_path(path, true) {
            return false;
        }
        if !self.deny_read.is_empty() {
            if self.deny_read.iter().any(|d| d.is_dir.is_none()) {
                return false;
            }
            if self.deny_read.iter().any(|d| super::deny_path::validate(d).is_err()) {
                return false;
            }
            let Some(p) = super::deny_path::access_path(path) else { return false };
            // Mutation of a denied path or one of its ancestors can move protected content
            // into view; snapshots also read pre-existing contents during writes and renames.
            if self.deny_read.iter().any(|d| path_within(&p, &d.target) || path_within(&d.target, &p)) {
                return false;
            }
        }
        true
    }

    pub fn can_write(&self, path: &Path) -> bool {
        if !self.can_write_deny_read(path) {
            return false;
        }
        let Ok(p) = resolve(path) else { return false };
        if is_write_device(&p) {
            return true;
        }
        if !self.writable.iter().any(|r| p.starts_with(r)) {
            return false;
        }
        // `.git` / `.mare` (and their ancestors against rename/removal) stay read-only inside
        // writable roots unless explicitly granted (
        // protocol/src/permissions.rs read-only subpaths); compared case-insensitively on
        // macOS/Windows, where `.GIT/config` is the same file.
        !self.protected_subpaths().iter().any(|s| path_within(&p, s) || path_within(s, &p))
            || self.granted_in_protected().iter().any(|g| path_within(&p, g))
    }

    pub fn is_host_protected_path(&self, path: &Path) -> bool {
        is_protected_host_token_path(path, false)
    }

    pub fn can_read(&self, path: &Path) -> bool {
        if is_protected_host_token_path(path, false) {
            return false;
        }
        if !self.deny_read.is_empty() {
            if self.deny_read.iter().any(|d| d.is_dir.is_none()) {
                return false;
            }
            if self.deny_read.iter().any(|d| super::deny_path::validate(d).is_err()) {
                return false;
            }
            let Some(p) = super::deny_path::access_path(path) else { return false };
            if self.deny_read.iter().any(|d| path_within(&p, &d.target)) {
                return false;
            }
        }
        if !self.restrict_read {
            return true;
        }
        let Ok(p) = resolve(path) else { return false };
        self.read_roots().unwrap_or_default().iter().any(|r| p.starts_with(r))
            || SYSTEM_READ_ROOTS.iter().any(|r| p.starts_with(r))
    }

    /// The marker used in tool results for a refused file access.
    pub fn denied_marker(&self) -> String {
        self.level.marker()
    }

    /// The marker for one denial: file accesses are [`Level::marker`] plus the access and the
    /// denied path when the evidence names it (`[sandbox: file access denied under
    /// workspace-write mode: write /etc/x]`), so the user and the model see what was refused.
    ///
    /// [`Level::marker`]: crate::ossandbox::Level::marker
    pub fn denial_marker(&self, d: &Denial) -> String {
        match d.kind {
            "network" => format!(
                "[sandbox: network access{} denied under {} mode]",
                d.host.as_ref().map(|h| format!(" to {h}")).unwrap_or_default(),
                self.level.as_str()
            ),
            "unavailable" => format!("[sandbox: unavailable — {SANDBOX_UNAVAILABLE}]"),
            "insufficient" => format!(
                "[sandbox: {SANDBOX_INSUFFICIENT} — {} not enforceable here]",
                d.path.as_deref().unwrap_or("a required capability")
            ),
            kind @ ("write" | "read") if d.path.is_some() => format!(
                "[sandbox: file access denied under {} mode: {kind} {}]",
                self.level.as_str(),
                d.path.as_deref().unwrap_or_default()
            ),
            _ => self.level.marker(),
        }
    }

    /// Check one operation with the same rules the spawned sandbox enforces: `op` = "write" |
    /// "read" (target = path, relative to `cwd`), "network" (target = host, host:port or URL;
    /// "unix:<path>" for Unix sockets) or "exec" (refused only without a usable backend).
    pub fn check(&self, op: &str, target: &str, cwd: &Path) -> Result<(), Denial> {
        match op {
            "write" | "read" => {
                let p = expand(target, cwd);
                let ok = if op == "write" { self.can_write(&p) } else { self.can_read(&p) };
                if ok {
                    Ok(())
                } else {
                    Err(Denial {
                        kind: if op == "write" { "write" } else { "read" },
                        path: Some(p.to_string_lossy().into_owned()),
                        host: None,
                    })
                }
            }
            "network" => match &self.network {
                Network::Open => Ok(()),
                Network::Proxy(np) => {
                    // Unix sockets reach local daemons: under a restricted network only the
                    // explicitly listed ones — the same list the kernel profile allows
                    if let Some(sock) = target.strip_prefix("unix:") {
                        let Ok(p) = resolve(&expand(sock, cwd)) else {
                            return Err(Denial { kind: "network", path: None, host: Some("unix socket".into()) });
                        };
                        return if self.allowed_unix_sockets().contains(&p) {
                            Ok(())
                        } else {
                            Err(Denial { kind: "network", path: None, host: Some(format!("unix:{}", p.display())) })
                        };
                    }
                    let host = host_of(target);
                    if np.check_host(&host).is_ok() {
                        Ok(())
                    } else {
                        Err(Denial { kind: "network", path: None, host: Some(host) })
                    }
                }
            },
            // the same matching a spawn does
            "exec" => {
                let b = backend();
                if b == Backend::None {
                    return Err(Denial { kind: "unavailable", path: None, host: None });
                }
                let refused: Vec<&str> =
                    self.missing(b).into_iter().filter(|c| !self.accept_partial.contains(c)).map(Cap::name).collect();
                if refused.is_empty() {
                    Ok(())
                } else {
                    Err(Denial { kind: "insufficient", path: Some(refused.join(",")), host: None })
                }
            }
            _ => Err(Denial { kind: "write", path: Some(target.to_string()), host: None }),
        }
    }
}

/// `https://user@host:443/x` / `host:443` / `[::1]:80` / `unix:/p` → the host part.
pub(super) fn host_of(target: &str) -> String {
    if target.starts_with("unix:") {
        return target.to_string();
    }
    let t = target.split("://").nth(1).unwrap_or(target);
    let t = t.split(['/', '?', '#']).next().unwrap_or(t);
    let t = t.rsplit_once('@').map_or(t, |(_, h)| h);
    if let Some(rest) = t.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    match t.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => t.to_string(),
    }
}

//! Workspace identity and trust/config state.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use anyhow::Result;

use crate::Host;

type SiblingTrustSync = Rc<dyn Fn(bool, bool) -> Pin<Box<dyn Future<Output = Result<()>>>>>;

/// Canonical workspace identity.
/// `env_id` is always `"local"` in single-machine deployments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceId {
    pub env_id: &'static str,
    pub cwd: PathBuf,
}

impl WorkspaceId {
    pub const LOCAL_ENV_ID: &'static str = "local";

    /// Build a canonical local [`WorkspaceId`] by resolving symlinks, lexical `.` / `..`,
    /// and platform case-folding (Windows & Linux WSL `/mnt/<drive>`).
    pub fn local(cwd: impl AsRef<Path>) -> std::io::Result<Self> {
        let canonical = mare_shared::paths::normalize_workspace_path(cwd.as_ref())?;
        Ok(Self { env_id: Self::LOCAL_ENV_ID, cwd: canonical })
    }
}

/// Resolve project trust for a workspace session (shared by RPC and ACP):
/// - CLI `--no-trust` (`anchor_cli_trust == Some(false)`) forces untrusted everywhere;
/// - CLI `--trust` (`anchor_cli_trust == Some(true)`) applies only to the launch workspace
///   (`same_ws == true`) and never extends to sibling workspaces;
/// - otherwise consults saved trust via [`mare_config::trust::lookup_checked`], falling back to
///   `anchor_startup_trusted` for the launch workspace and `false` for other workspaces;
/// - fails closed (`false`) if `trust.json` is corrupt.
pub fn resolve_workspace_trust(
    target_cwd: &Path,
    same_ws: bool,
    anchor_startup_trusted: bool,
    anchor_cli_trust: Option<bool>,
) -> bool {
    if anchor_cli_trust == Some(false) {
        return false;
    }
    if same_ws && let Some(cli_trust) = anchor_cli_trust {
        return cli_trust;
    }
    match mare_config::trust::lookup_checked(target_cwd) {
        Ok(Some(saved)) => saved,
        Ok(None) => {
            if same_ws {
                anchor_startup_trusted
            } else {
                false
            }
        }
        Err(err) => {
            tracing::warn!(cwd = %target_cwd.display(), "failing closed on corrupt trust file: {err:#}");
            false
        }
    }
}

/// Per-workspace trust and effective config snapshot held by a [`crate::Host`] (and shared by
/// `WorkspaceRegistry`).
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub root: PathBuf,
    pub trusted: Cell<bool>,
    pub session_only: Cell<bool>,
    pub config_snapshot: RefCell<serde_json::Value>,
    pub baseline_config: RefCell<serde_json::Value>,
    sibling_sync: RefCell<Option<SiblingTrustSync>>,
}

impl WorkspaceState {
    pub fn new(cwd: PathBuf, trusted: bool, config_snapshot: serde_json::Value) -> std::io::Result<Self> {
        let id = WorkspaceId::local(&cwd)?;
        let root = id.cwd.clone();
        Ok(Self {
            id,
            root,
            trusted: Cell::new(trusted),
            session_only: Cell::new(false),
            baseline_config: RefCell::new(config_snapshot.clone()),
            config_snapshot: RefCell::new(config_snapshot),
            sibling_sync: RefCell::new(None),
        })
    }

    pub(crate) fn set_sibling_sync(&self, sync: SiblingTrustSync) {
        *self.sibling_sync.borrow_mut() = Some(sync);
    }

    pub(crate) async fn sync_siblings(&self, trusted: bool, saved: bool) -> Result<()> {
        let sync = self.sibling_sync.borrow().clone();
        if let Some(sync) = sync {
            sync(trusted, saved).await?;
        }
        Ok(())
    }

    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    pub fn is_trusted(&self) -> bool {
        self.trusted.get()
    }

    pub fn set_trusted(&self, trusted: bool) {
        self.trusted.set(trusted);
    }

    pub fn is_session_only(&self) -> bool {
        self.session_only.get()
    }

    pub fn set_session_only(&self, session_only: bool) {
        self.session_only.set(session_only);
    }

    pub fn update_config(&self, config: serde_json::Value) {
        if !self.is_session_only() {
            *self.baseline_config.borrow_mut() = config.clone();
        }
        *self.config_snapshot.borrow_mut() = config;
    }

    pub fn trust_decision(&self) -> Option<bool> {
        mare_config::trust::decision(&self.id.cwd)
    }

    pub fn save_trust(&self, trusted: bool) -> Result<()> {
        mare_config::trust::set(&self.id.cwd, trusted)
    }

    pub fn summarize_resources(&self) -> Vec<(&'static str, usize)> {
        mare_config::trust::summarize(&self.id.cwd)
    }
}

/// Snapshot of a loaded workspace tracked by [`WorkspaceRegistry`].
#[derive(Clone, Debug)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub root: PathBuf,
    pub trusted: bool,
    pub config_snapshot: serde_json::Value,
    pub session_ids: Vec<String>,
}

struct WorkspaceEntry {
    root: PathBuf,
    trusted_override: Option<bool>,
    sessions: BTreeMap<String, Weak<Host>>,
}

impl WorkspaceEntry {
    fn prune(&mut self) {
        self.sessions.retain(|_, w| w.strong_count() > 0);
    }

    fn snapshot(&mut self, id: &WorkspaceId) -> Option<WorkspaceSnapshot> {
        self.prune();
        let live: Vec<(&String, Arc<Host>)> =
            self.sessions.iter().filter_map(|(sid, w)| w.upgrade().map(|h| (sid, h))).collect();
        let (trusted, config_snapshot) = if let Some((_, host)) =
            live.iter().find(|(_, h)| !h.workspace.is_session_only() && !h.is_pending_session_trust_revoke())
        {
            (
                self.trusted_override.unwrap_or_else(|| host.workspace.is_trusted()),
                host.workspace.config_snapshot.borrow().clone(),
            )
        } else {
            let (_, first) = live.first()?;
            (self.trusted_override.unwrap_or(false), first.workspace.baseline_config.borrow().clone())
        };
        let session_ids = live.into_iter().map(|(sid, _)| sid.clone()).collect();
        Some(WorkspaceSnapshot { id: id.clone(), root: self.root.clone(), trusted, config_snapshot, session_ids })
    }
}

/// Multi-session registry indexing active sessions by canonical [`WorkspaceId`].
///
/// Sessions opened in the same canonical workspace (including via symlinks) share trust updates
/// without affecting sessions in other workspaces.
#[derive(Default)]
pub struct WorkspaceRegistry {
    entries: Rc<RefCell<HashMap<WorkspaceId, WorkspaceEntry>>>,
}

impl WorkspaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune_empty(entries: &mut HashMap<WorkspaceId, WorkspaceEntry>) {
        entries.retain(|_, entry| {
            entry.prune();
            !entry.sessions.is_empty()
        });
    }

    /// Active workspace-level trust override, if any live session is attached to `workspace_id`.
    pub fn trusted_override(&self, workspace_id: &WorkspaceId) -> Option<bool> {
        let mut entries = self.entries.borrow_mut();
        Self::prune_empty(&mut entries);
        entries.get(workspace_id).and_then(|e| e.trusted_override)
    }

    /// Whether `session_id` is currently attached to canonical `workspace_id`.
    pub fn contains_session(&self, workspace_id: &WorkspaceId, session_id: &str) -> bool {
        let mut entries = self.entries.borrow_mut();
        Self::prune_empty(&mut entries);
        entries
            .get(workspace_id)
            .and_then(|entry| entry.sessions.get(session_id))
            .is_some_and(|weak| weak.strong_count() > 0)
    }

    /// Attach a loaded session `Host` to its canonical workspace entry, returning any active
    /// trust override set on that workspace so the caller can synchronize runtime policy.
    pub fn attach(&self, session_id: &str, host: &Arc<Host>) -> Option<bool> {
        let ws_id = host.workspace.id().clone();
        let override_trust = {
            let mut entries = self.entries.borrow_mut();
            let entry = entries.entry(ws_id.clone()).or_insert_with(|| WorkspaceEntry {
                root: ws_id.cwd.clone(),
                trusted_override: None,
                sessions: BTreeMap::new(),
            });
            entry.prune();
            entry.sessions.insert(session_id.to_string(), Arc::downgrade(host));
            entry.trusted_override
        };
        let entries_weak = Rc::downgrade(&self.entries);
        let self_weak = Arc::downgrade(host);
        host.workspace.set_sibling_sync(Rc::new(move |trusted, saved| {
            let entries_weak = entries_weak.clone();
            let ws_key = ws_id.clone();
            let self_weak = self_weak.clone();
            Box::pin(async move {
                let Some(entries_rc) = entries_weak.upgrade() else { return Ok(()) };
                let siblings: Vec<Arc<Host>> = {
                    let mut entries = entries_rc.borrow_mut();
                    Self::prune_empty(&mut entries);
                    let Some(entry) = entries.get_mut(&ws_key) else { return Ok(()) };
                    entry.trusted_override = Some(trusted);
                    entry
                        .sessions
                        .values()
                        .filter_map(Weak::upgrade)
                        .filter(|h| !std::ptr::eq(Arc::as_ptr(h), self_weak.as_ptr()))
                        .collect()
                };
                let mut failure = None;
                for sibling in siblings {
                    if let Err(error) = sibling.apply_trust(trusted, mare_shared::WORKSPACE_TRUST_SOURCE, saved).await
                        && failure.is_none()
                    {
                        failure = Some(error);
                    }
                }
                match failure {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })
        }));
        override_trust
    }

    /// Detach `session_id` from `workspace_id`, removing the entry once no live sessions remain.
    pub fn detach(&self, workspace_id: &WorkspaceId, session_id: &str) {
        let mut entries = self.entries.borrow_mut();
        let Some(entry) = entries.get_mut(workspace_id) else { return };
        entry.sessions.remove(session_id);
        entry.prune();
        if entry.sessions.is_empty() {
            entries.remove(workspace_id);
        }
    }

    /// Return a live snapshot for `workspace_id`, if at least one session is loaded there.
    pub fn get(&self, workspace_id: &WorkspaceId) -> Option<WorkspaceSnapshot> {
        let mut entries = self.entries.borrow_mut();
        let entry = entries.get_mut(workspace_id)?;
        let snap = entry.snapshot(workspace_id);
        if snap.is_none() {
            entries.remove(workspace_id);
        }
        snap
    }

    /// Snapshot all currently loaded workspaces sorted by canonical `cwd`.
    pub fn list(&self) -> Vec<WorkspaceSnapshot> {
        let mut entries = self.entries.borrow_mut();
        Self::prune_empty(&mut entries);
        let mut out: Vec<WorkspaceSnapshot> = entries.iter_mut().filter_map(|(id, e)| e.snapshot(id)).collect();
        out.sort_by(|a, b| a.id.cwd.cmp(&b.id.cwd));
        out
    }

    /// Number of workspaces with at least one active session.
    pub fn len(&self) -> usize {
        let mut entries = self.entries.borrow_mut();
        Self::prune_empty(&mut entries);
        entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Update trust for every active session in `workspace_id`, propagating to Lua runtime
    /// (`mare.trusted`, project plugins/config, and `permissions` sandbox) and optionally
    /// persisting via `mare_config::trust::set`. Persistence runs before mutating in-memory
    /// overrides so write failures leave state consistent. Returns `Ok(true)` if the workspace is
    /// loaded, `Ok(false)` otherwise.
    pub async fn set_trust(&self, workspace_id: &WorkspaceId, trusted: bool, persist: bool) -> Result<bool> {
        let hosts: Vec<Arc<Host>> = {
            let mut entries = self.entries.borrow_mut();
            Self::prune_empty(&mut entries);
            let Some(entry) = entries.get(workspace_id) else {
                return Ok(false);
            };
            entry.sessions.values().filter_map(Weak::upgrade).collect()
        };
        if hosts.is_empty() {
            return Ok(false);
        }
        if persist {
            mare_config::trust::set(&workspace_id.cwd, trusted)?;
        }
        {
            let mut entries = self.entries.borrow_mut();
            if let Some(entry) = entries.get_mut(workspace_id) {
                entry.trusted_override = Some(trusted);
            }
        }
        let mut failure = None;
        for host in hosts {
            if let Err(error) = host.apply_trust(trusted, mare_shared::WORKSPACE_TRUST_SOURCE, persist).await
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "mare-ws-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn workspace_id_always_uses_local_env_id_and_canonicalizes_cwd() {
        let dir = TempDir::new();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();

        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let id_real = WorkspaceId::local(&real).unwrap();
            let id_link = WorkspaceId::local(&link).unwrap();
            assert_eq!(id_real.env_id, "local");
            assert_eq!(id_real, id_link, "symlink and target must resolve to the same WorkspaceId");
        }

        #[cfg(not(unix))]
        {
            let id = WorkspaceId::local(&real).unwrap();
            assert_eq!(id.env_id, "local");
        }

        // Nonexistent child path resolves its existing symlinked ancestors without panicking.
        let missing = dir.path().join("does-not-exist-yet");
        let id_missing = WorkspaceId::local(&missing).unwrap();
        assert_eq!(id_missing.env_id, "local");
        assert_eq!(
            id_missing.cwd,
            mare_shared::paths::normalize_workspace_path(dir.path()).unwrap().join("does-not-exist-yet")
        );
    }

    #[test]
    fn workspace_state_tracks_trust_and_config_snapshot() {
        let dir = TempDir::new();
        let ws = WorkspaceState::new(dir.path().to_path_buf(), false, serde_json::json!({"a": 1})).unwrap();
        assert_eq!(ws.id().env_id, "local");
        assert!(!ws.is_trusted());
        ws.set_trusted(true);
        assert!(ws.is_trusted());
        ws.update_config(serde_json::json!({"a": 2}));
        assert_eq!(*ws.config_snapshot.borrow(), serde_json::json!({"a": 2}));
    }

    #[tokio::test]
    async fn unloaded_workspace_set_trust_returns_false_without_polluting_state() {
        let dir = TempDir::new();
        let ws_id = WorkspaceId::local(dir.path()).unwrap();
        let reg = WorkspaceRegistry::new();
        assert!(!reg.set_trust(&ws_id, true, false).await.unwrap());
        assert!(reg.is_empty());
        assert!(reg.trusted_override(&ws_id).is_none());
    }

    #[test]
    fn resolve_workspace_trust_honors_cli_flags_and_workspace_scope() {
        let dir = TempDir::new();
        let sibling = TempDir::new();
        // `--no-trust` forces untrusted everywhere, even on the launch workspace.
        assert!(!resolve_workspace_trust(dir.path(), true, true, Some(false)));
        assert!(!resolve_workspace_trust(sibling.path(), false, true, Some(false)));
        // `--trust` applies only to the launch workspace, never to siblings.
        assert!(resolve_workspace_trust(dir.path(), true, false, Some(true)));
        assert!(!resolve_workspace_trust(sibling.path(), false, true, Some(true)));
        // Without CLI flags or saved decision, launch workspace falls back to startup trust; siblings default false.
        assert!(resolve_workspace_trust(dir.path(), true, true, None));
        assert!(!resolve_workspace_trust(dir.path(), true, false, None));
        assert!(!resolve_workspace_trust(sibling.path(), false, true, None));
    }
}

//! Multi-session host manager facade over [`SessionOrchestrator`].
//!
//! Each live session owns an isolated [`SessionRuntime`] backed by its own `Arc<Host>` (Lua VM,
//! `Kernel`, `EventLog`, `AnswererSlot`, `InboundHeld`, `SessionPromptReceipts`), all running on
//! `HostManager`'s single-threaded Tokio `LocalSet`.

use std::collections::HashSet;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

use crate::session_orchestrator::SessionOrchestrator;
pub use crate::session_orchestrator::{
    CloseRetryOutcome, DEFAULT_CLOSE_MAX_RETRIES, DEFAULT_CLOSE_RETRY_BACKOFF, ManagerShutdownReport, SessionError,
    SessionRuntime, SessionShutdownEntry, close_until_confirmed, close_with_retry,
};
use crate::{Host, HostOptions, ShutdownReport, WebRegistry, WorkspaceId, WorkspaceRegistry};

/// Process-level session manager running on the single-threaded Tokio `LocalSet`.
pub struct HostManager {
    orchestrator: Rc<SessionOrchestrator>,
}

impl Default for HostManager {
    fn default() -> Self {
        Self { orchestrator: SessionOrchestrator::new_multi() }
    }
}

impl HostManager {
    /// Create a multi-runtime [`HostManager`] backed by a [`crate::Capacity::Multi`] orchestrator.
    pub fn new() -> Rc<Self> {
        Rc::new(Self { orchestrator: SessionOrchestrator::new_multi() })
    }

    /// Create a single-host [`HostManager`] with its own [`crate::Capacity::Single`]
    /// orchestrator (used by attached single-host RPC so multiple `from_host` runtimes sharing one
    /// `Host` never collide while keeping the host itself `Single`).
    pub fn new_single(host: &Arc<Host>) -> Rc<Self> {
        let orchestrator =
            host.orchestrator().filter(|o| !o.is_multi()).unwrap_or_else(|| SessionOrchestrator::new_single(host));
        host.set_orchestrator(orchestrator.clone());
        Rc::new(Self { orchestrator })
    }

    /// Underlying [`SessionOrchestrator`] owning runtime set, receipts, and lifecycle orchestration.
    pub fn orchestrator(&self) -> &Rc<SessionOrchestrator> {
        &self.orchestrator
    }

    /// Register a callback invoked synchronously whenever an active [`Host`] transitions in-place
    /// (`session/changed`) to a new `session_id`. Returns a handle for [`Self::remove_session_rekey`].
    pub fn on_session_rekey(&self, hook: impl Fn(&str, &str, &Rc<SessionRuntime>) + 'static) -> u64 {
        self.orchestrator.on_session_rekey(hook)
    }

    /// Remove a previously registered session rekey callback by handle id.
    pub fn remove_session_rekey(&self, id: u64) {
        self.orchestrator.remove_session_rekey(id);
    }

    /// Number of currently registered session rekey callbacks.
    pub fn rekey_hook_count(&self) -> usize {
        self.orchestrator.rekey_hook_count()
    }

    /// Host-wide HTTP/WebSocket route and asset registry.
    pub fn web(&self) -> &Rc<WebRegistry> {
        self.orchestrator.web()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.orchestrator.is_shutting_down()
    }

    /// Registry indexing loaded sessions by canonical [`WorkspaceId`].
    pub fn workspaces(&self) -> &WorkspaceRegistry {
        self.orchestrator.workspaces()
    }

    /// Look up an active session runtime, returning [`SessionError::NotLoaded`] or
    /// [`SessionError::Closing`] when absent.
    pub fn session(&self, session_id: &str) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.session(session_id)
    }

    pub fn is_closing(&self, session_id: &str) -> bool {
        self.orchestrator.is_closing(session_id)
    }

    pub fn is_loading(&self, session_id: &str) -> bool {
        self.orchestrator.is_loading(session_id)
    }

    pub fn len(&self) -> usize {
        self.orchestrator.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orchestrator.is_empty()
    }

    pub fn active_ids(&self) -> Vec<String> {
        self.orchestrator.active_ids()
    }

    pub fn active_and_closing_ids(&self) -> HashSet<String> {
        self.orchestrator.active_and_closing_ids()
    }

    /// Wait until any in-flight unload for `session_id` finishes.
    pub async fn wait_closing(&self, session_id: &str) {
        self.orchestrator.wait_closing(session_id).await
    }

    /// Register an already-started `Host` into `HostManager`.
    /// Every in-memory session receives a non-empty `session_id` (ephemeral `--no-session` hosts
    /// generate one if the session plugin did not assign one).
    pub async fn register_host(
        &self,
        host: Arc<Host>,
        expected_session_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.register_host(host, expected_session_id, ephemeral).await
    }

    /// Register an already-started `Host` while explicitly preserving its existing orchestrator
    /// capacity (`Single` for attached RPC).
    pub async fn register_host_keeping_capacity(
        &self,
        host: Arc<Host>,
        expected_session_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.register_host_keeping_capacity(host, expected_session_id, ephemeral).await
    }

    /// Create and register a new session without unloading any existing sessions.
    pub async fn create_session(&self, opts: HostOptions) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.create_session(opts).await
    }

    /// Load/resume a session without unloading any existing sessions, holding a concurrency
    /// reservation on `session_id` across `Host::new(opts).await`.
    pub async fn load_session(
        self: &Rc<Self>,
        session_id: Option<&str>,
        opts: HostOptions,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.load_session(session_id, opts).await
    }

    /// Load/resume a session with a hook invoked on the newly constructed [`Host`] right before
    /// `app/ready` runs, so RPC callers can wire UI/capability handlers before `ui.select` or
    /// `session/loaded` listeners inspect `mare.ui.connected()`.
    pub async fn load_session_with_before_ready<F, Fut>(
        self: &Rc<Self>,
        session_id: Option<&str>,
        opts: HostOptions,
        before_ready: F,
    ) -> Result<Rc<SessionRuntime>, SessionError>
    where
        F: FnOnce(Arc<Host>) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.orchestrator.load_session_with_before_ready(session_id, opts, before_ready).await
    }

    /// Fork an already-loaded session into a new independent [`SessionRuntime`] without unloading
    /// or mutating the source session.
    pub async fn fork_session(
        &self,
        source_session_id: &str,
        new_opts: HostOptions,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.orchestrator.fork_session(source_session_id, new_opts).await
    }

    /// Update trust on canonical `workspace_id`, propagating runtime policy to all active sessions
    /// attached to it and optionally persisting the decision to disk.
    pub async fn set_workspace_trust(
        &self,
        workspace_id: &WorkspaceId,
        trusted: bool,
        persist: bool,
    ) -> Result<bool, SessionError> {
        self.orchestrator.set_workspace_trust(workspace_id, trusted, persist).await
    }

    /// Update trust on canonical `workspace_id` on behalf of `session_id`, verifying that
    /// `session_id` is currently loaded and attached to `workspace_id`.
    pub async fn set_workspace_trust_for_session(
        &self,
        session_id: &str,
        workspace_id: &WorkspaceId,
        trusted: bool,
        persist: bool,
    ) -> Result<bool, SessionError> {
        self.orchestrator.set_workspace_trust_for_session(session_id, workspace_id, trusted, persist).await
    }

    /// Close only `session_id` synchronously (waits for `abort` + `shutdown_report`), keeping the
    /// session marked `closing` for the full `.await` duration.
    pub async fn close_session(self: &Rc<Self>, session_id: &str) -> Result<ShutdownReport, SessionError> {
        self.orchestrator.close_session(session_id).await
    }

    /// Begin closing `session_id` in the background, keeping its id marked `closing` until
    /// shutdown finishes (or up to bounded retries when `retry_until_confirmed` is true, aborted on
    /// manager shutdown).
    pub fn begin_close<F>(
        self: &Rc<Self>,
        session_id: &str,
        retry_until_confirmed: bool,
        on_first_attempt: F,
    ) -> Result<(), SessionError>
    where
        F: FnOnce(Result<(), String>) + 'static,
    {
        self.orchestrator.begin_close(session_id, retry_until_confirmed, on_first_attempt)
    }

    /// Cancel and concurrently drain every active and in-flight closing session runtime, wait for
    /// any in-flight `Host::new` loads to roll back, and return the aggregated quiescent
    /// [`ManagerShutdownReport`]. Concurrent callers share the in-flight shutdown report.
    pub async fn shutdown_all_report(&self) -> ManagerShutdownReport {
        self.orchestrator.shutdown_all_report().await
    }

    /// Shut down every active session runtime without calling `host.abort()` (preserving queued
    /// durable inputs across clean EOF/disconnect).
    pub async fn shutdown_all_no_abort(&self) -> ManagerShutdownReport {
        self.orchestrator.shutdown_all_no_abort().await
    }

    /// Cancel and shut down every active session runtime concurrently and await all in-flight closes.
    pub async fn shutdown_all(&self) {
        self.orchestrator.shutdown_all().await
    }
}

impl crate::RuntimeSet for HostManager {
    fn reserve(&self, host: &Arc<Host>, session_id: &str) -> Result<(), SessionError> {
        self.orchestrator.reserve(host, session_id)
    }

    fn rekey(&self, host: &Arc<Host>, new_sid: &str) -> Result<(), SessionError> {
        self.orchestrator.rekey(host, new_sid)
    }

    fn release(&self, host: &Arc<Host>, session_id: &str) {
        self.orchestrator.release(host, session_id)
    }

    fn list(&self) -> Vec<crate::LoadedSessionEntry> {
        self.orchestrator.list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_orchestrator::host_session_info_field;
    use mare_shared::ui::Mode;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "mare-mgr-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts(cwd: PathBuf, no_session: bool) -> HostOptions {
        HostOptions {
            cwd,
            mode: Mode::Print,
            config: serde_json::json!({
                "shutdown_grace_ms": 50,
                "plugins": { "permissions": { "enabled": false } }
            }),
            load_init: false,
            project_trusted: false,
            args: serde_json::json!({ "no_session": no_session }),
            no_user_plugins: true,
        }
    }

    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: TempDir,
        prev: Option<std::ffi::OsString>,
    }
    impl HomeGuard {
        fn enter() -> Self {
            let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = TempDir::new();
            let prev = std::env::var_os("MARE_HOME");
            unsafe { std::env::set_var("MARE_HOME", dir.path()) };
            Self { _lock: lock, _dir: dir, prev }
        }
        fn path(&self) -> &std::path::Path {
            self._dir.path()
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var("MARE_HOME", v) },
                None => unsafe { std::env::remove_var("MARE_HOME") },
            }
        }
    }

    #[test]
    fn unit_process_restores_nextest_home_after_homeguard() {
        let home = std::env::var_os("MARE_HOME").expect("private MARE_HOME");
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        {
            let inner = HomeGuard::enter();
            assert_ne!(inner.path(), std::path::Path::new(&home));
        }
        assert_eq!(std::env::var_os("MARE_HOME"), Some(home));
    }

    #[tokio::test]
    async fn create_keeps_multiple_sessions_loaded_and_isolated() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let d2 = TempDir::new();
                let mgr = HostManager::new();

                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let s2 = mgr.create_session(opts(d2.path().to_path_buf(), true)).await.unwrap();

                assert!(!s1.session_id.is_empty() && !s2.session_id.is_empty());
                assert_ne!(s1.session_id, s2.session_id);
                assert!(s1.ephemeral && s2.ephemeral);
                assert_ne!(s1.workspace, s2.workspace);
                assert_eq!(mgr.len(), 2);
                assert!(mgr.session(&s1.session_id).is_ok());
                assert!(mgr.session(&s2.session_id).is_ok());

                // Missing session returns session_not_loaded.
                let err = mgr.session("nonexistent-session").err().unwrap();
                assert_eq!(err.code(), "session_not_loaded");

                // Closing s1 leaves s2 active.
                let report = mgr.close_session(&s1.session_id).await.unwrap();
                assert!(!report.stop_unconfirmed());
                assert_eq!(mgr.len(), 1);
                assert_eq!(mgr.session(&s1.session_id).err().unwrap().code(), "session_not_loaded");
                assert!(mgr.session(&s2.session_id).is_ok());

                mgr.shutdown_all().await;
                assert!(mgr.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn register_host_rejects_duplicate_and_closing_sessions() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();
                let no_sess_opts = || {
                    let mut o = opts(d1.path().to_path_buf(), true);
                    o.config["plugins"]["session"] = serde_json::json!({ "enabled": false });
                    o
                };

                let h1 = Host::new(no_sess_opts()).await.unwrap();
                h1.kernel.set_session_identity(Some("dup-sid".into()));
                let rt1 = mgr.register_host(h1, Some("dup-sid"), true).await.unwrap();
                assert_eq!(rt1.session_id, "dup-sid");

                // Duplicate id is refused.
                let h_dup = Host::new(no_sess_opts()).await.unwrap();
                h_dup.kernel.set_session_identity(Some("dup-sid".into()));
                let dup_err = mgr.register_host(h_dup, Some("dup-sid"), true).await.err().unwrap();
                assert_eq!(dup_err.code(), "session_already_loaded");

                // Hold a blocking scope task on rt1 so begin_close stays in `closing`.
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(rt1.host.lua(), move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx.blocking_recv();
                })
                .unwrap();
                entered_rx.await.unwrap();

                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                mgr.begin_close("dup-sid", true, move |res| {
                    let _ = done_tx.send(res);
                })
                .unwrap();
                assert!(mgr.is_closing("dup-sid"));
                assert_eq!(mgr.session("dup-sid").err().unwrap().code(), "session_closing");
                assert_eq!(mgr.begin_close("dup-sid", false, |_| {}).err().unwrap().code(), "session_closing");
                assert_eq!(
                    mgr.load_session(Some("dup-sid"), opts(d1.path().to_path_buf(), true)).await.err().unwrap().code(),
                    "session_closing"
                );
                let h_closing = Host::new(no_sess_opts()).await.unwrap();
                h_closing.kernel.set_session_identity(Some("dup-sid".into()));
                assert_eq!(
                    mgr.register_host(h_closing, Some("dup-sid"), true).await.err().unwrap().code(),
                    "session_closing"
                );

                let first_res = done_rx.await.unwrap();
                assert!(
                    first_res.as_ref().is_err_and(|e| e.contains("stop_unconfirmed")),
                    "first attempt while task is still blocked must report stop_unconfirmed: {first_res:?}"
                );
                let _ = release_tx.send(());
                mgr.wait_closing("dup-sid").await;
                assert!(!mgr.is_closing("dup-sid"));
                assert_eq!(mgr.session("dup-sid").err().unwrap().code(), "session_not_loaded");

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn serialized_config_queue_preserves_fifo_per_session_without_blocking_other_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let d2 = TempDir::new();
                let mgr = HostManager::new();
                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let s2 = mgr.create_session(opts(d2.path().to_path_buf(), true)).await.unwrap();

                let (s1_entered_tx, s1_entered_rx) = tokio::sync::oneshot::channel();
                let (s1_release_tx, s1_release_rx) = tokio::sync::oneshot::channel();
                let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

                let l1 = log.clone();
                let rx1 = s1
                    .enqueue_config(move |_| async move {
                        l1.lock().unwrap().push("s1-first-start");
                        let _ = s1_entered_tx.send(());
                        let _ = s1_release_rx.await;
                        l1.lock().unwrap().push("s1-first-end");
                    })
                    .unwrap();
                let l2 = log.clone();
                let rx2 = s1
                    .enqueue_config(move |_| async move {
                        l2.lock().unwrap().push("s1-second");
                    })
                    .unwrap();
                s1_entered_rx.await.unwrap();

                // s2 completes immediately even while s1's first config mutation is paused.
                let l_other = log.clone();
                s2.run_config(move |_| async move {
                    l_other.lock().unwrap().push("s2-done");
                })
                .await
                .unwrap();

                assert_eq!(*log.lock().unwrap(), vec!["s1-first-start", "s2-done"]);
                let _ = s1_release_tx.send(());
                rx1.await.unwrap();
                rx2.await.unwrap();
                assert_eq!(*log.lock().unwrap(), vec!["s1-first-start", "s2-done", "s1-first-end", "s1-second"]);

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn fork_session_rejects_missing_or_ephemeral_source_without_mutating_source() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();

                let missing = mgr.fork_session("missing-sid", opts(d1.path().to_path_buf(), true)).await;
                assert_eq!(missing.err().unwrap().code(), "session_not_loaded");

                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let err = mgr.fork_session(&s1.session_id, opts(d1.path().to_path_buf(), true)).await.err().unwrap();
                assert!(matches!(err, SessionError::HostFailed(_)));
                assert!(mgr.session(&s1.session_id).is_ok());

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn close_with_retry_is_bounded_and_cancellable() {
        let calls = Rc::new(Cell::new(0usize));
        let done = Rc::new(Cell::new(false));
        let outcome = close_with_retry(
            true,
            3,
            Duration::ZERO,
            CancellationToken::new(),
            {
                let calls = calls.clone();
                move || {
                    calls.set(calls.get() + 1);
                    async { Err("stop_unconfirmed".to_string()) }
                }
            },
            |first| assert!(first.is_err()),
            {
                let done = done.clone();
                move || done.set(true)
            },
        )
        .await;
        assert_eq!(calls.get(), 4);
        assert_eq!(outcome, CloseRetryOutcome { confirmed: false, cancelled: false, retries: 3 });
        assert!(done.get());

        // Cancellation interrupts retries immediately before hitting max_retries.
        let cancel = CancellationToken::new();
        let cancel_on_first = cancel.clone();
        let cancel_calls = Rc::new(Cell::new(0usize));
        let cancelled = close_with_retry(
            true,
            10,
            Duration::from_secs(30),
            cancel,
            {
                let cancel_calls = cancel_calls.clone();
                move || {
                    cancel_calls.set(cancel_calls.get() + 1);
                    async { Err("stop_unconfirmed".to_string()) }
                }
            },
            move |_| cancel_on_first.cancel(),
            || {},
        )
        .await;
        assert_eq!(cancel_calls.get(), 1);
        assert!(cancelled.cancelled && !cancelled.confirmed && cancelled.retries == 0);
    }

    #[tokio::test]
    async fn begin_close_unconfirmed_releases_closing_when_shutdown_all_cancels() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();
                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let sid = s1.session_id.clone();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s1.host.lua(), move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx.blocking_recv();
                })
                .unwrap();
                entered_rx.await.unwrap();
                drop(s1);

                let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                mgr.begin_close(&sid, true, move |res| {
                    let _ = first_tx.send(res);
                })
                .unwrap();
                assert!(first_rx.await.unwrap().is_err());
                assert!(mgr.is_closing(&sid));

                // shutdown_all cancels retry loop and completes promptly despite stuck blocking task.
                tokio::time::timeout(Duration::from_secs(2), mgr.shutdown_all())
                    .await
                    .expect("shutdown_all must converge even with unconfirmed background close");
                assert!(!mgr.is_closing(&sid));
                let _ = release_tx.send(());
            })
            .await;
    }

    #[tokio::test]
    async fn close_cancels_config_worker_drops_queued_tasks_and_releases_host() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();
                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let sid = s1.session_id.clone();
                let weak_host = Arc::downgrade(&s1.host);

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let rx1 = s1
                    .enqueue_config(move |_| async move {
                        let _ = entered_tx.send(());
                        let _ = release_rx.await;
                    })
                    .unwrap();
                let ran_second = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let ran_second_c = ran_second.clone();
                let rx2 = s1
                    .enqueue_config(move |_| async move {
                        ran_second_c.store(true, std::sync::atomic::Ordering::SeqCst);
                    })
                    .unwrap();
                entered_rx.await.unwrap();

                let s1_ref = Rc::clone(&s1);
                drop(s1);
                let report = mgr.close_session(&sid).await.unwrap();
                assert!(!report.stop_unconfirmed());

                // In-flight and queued tasks must be dropped without succeeding and new enqueues must be refused.
                assert!(rx1.await.is_err());
                assert!(rx2.await.is_err());
                assert!(!ran_second.load(std::sync::atomic::Ordering::SeqCst));
                assert_eq!(s1_ref.enqueue_config(|_| async {}).err().unwrap().code(), "internal");
                drop(s1_ref);
                let _ = release_tx.send(());
                tokio::task::yield_now().await;
                assert!(weak_host.upgrade().is_none(), "HostManager and config worker must not leak Host");

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn close_session_marks_closing_across_shutdown_await() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();
                let mut o = opts(d1.path().to_path_buf(), true);
                o.config["shutdown_grace_ms"] = serde_json::json!(200);
                let s1 = mgr.create_session(o).await.unwrap();
                let sid = s1.session_id.clone();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s1.host.lua(), move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx.blocking_recv();
                })
                .unwrap();
                entered_rx.await.unwrap();
                drop(s1);

                let close_task = tokio::task::spawn_local({
                    let mgr = Rc::clone(&mgr);
                    let sid = sid.clone();
                    async move { mgr.close_session(&sid).await }
                });
                // Yield once so close_session enters abort()/shutdown_report().await.
                tokio::task::yield_now().await;

                assert!(mgr.is_closing(&sid), "close_session must keep sid in closing across .await");
                assert_eq!(mgr.session(&sid).err().unwrap().code(), "session_closing");
                assert_eq!(
                    mgr.load_session(Some(&sid), opts(d1.path().to_path_buf(), true)).await.err().unwrap().code(),
                    "session_closing"
                );

                let _ = release_tx.send(());
                let wait_task = tokio::task::spawn_local({
                    let mgr = Rc::clone(&mgr);
                    let sid = sid.clone();
                    async move { mgr.wait_closing(&sid).await }
                });
                let report = close_task.await.unwrap().unwrap();
                let _ = report;
                wait_task.await.unwrap();
                assert!(!mgr.is_closing(&sid));
                assert_eq!(mgr.session(&sid).err().unwrap().code(), "session_not_loaded");

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn load_session_reserves_loading_slot_cleans_on_failure_and_shutdown_rejects_new_sessions() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();

                // Concurrent load_session calls reserve `loading` deterministically and clean up on failure.
                let t1 = tokio::task::spawn_local({
                    let mgr = Rc::clone(&mgr);
                    let o1 = opts(d1.path().to_path_buf(), true);
                    async move { mgr.load_session(Some("race-sid"), o1).await }
                });
                // t1 fails with SessionIdMismatch (random UUID != "race-sid") and cleans up `loading`.
                let first_res = t1.await.unwrap();
                assert!(matches!(first_res, Err(SessionError::SessionIdMismatch { .. })));
                assert!(!mgr.is_loading("race-sid"), "LoadingGuard must clean up after load_session error");

                // Also deterministically verify loading reservation by pinning load_session across its first yield.
                {
                    let mut f1 =
                        std::pin::pin!(mgr.load_session(Some("pinned-sid"), opts(d1.path().to_path_buf(), true)));
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    assert!(f1.as_mut().poll(&mut cx).is_pending());
                    assert!(mgr.is_loading("pinned-sid"));
                    let dup_loading = mgr.load_session(Some("pinned-sid"), opts(d1.path().to_path_buf(), true)).await;
                    assert_eq!(dup_loading.err().unwrap().code(), "session_loading");
                }
                assert!(!mgr.is_loading("pinned-sid"), "dropping in-flight load_session must release LoadingGuard");

                // Once shutdown_all begins, create/load/register are all refused with shutting_down.
                mgr.shutdown_all().await;
                assert!(mgr.is_shutting_down());
                assert_eq!(
                    mgr.create_session(opts(d1.path().to_path_buf(), true)).await.err().unwrap().code(),
                    "shutting_down"
                );
                assert_eq!(
                    mgr.load_session(Some("after-shutdown"), opts(d1.path().to_path_buf(), true))
                        .await
                        .err()
                        .unwrap()
                        .code(),
                    "shutting_down"
                );
                let h = Host::new(opts(d1.path().to_path_buf(), true)).await.unwrap();
                assert_eq!(mgr.register_host(h, None, true).await.err().unwrap().code(), "shutting_down");
            })
            .await;
    }

    #[tokio::test]
    async fn persistent_close_releases_lock_for_reload_and_fork_and_vms_stay_isolated() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let d2 = TempDir::new();
                let mgr = HostManager::new();

                // Create two persisted sessions in distinct directories.
                let faux_opts = || {
                    let mut o = opts(d1.path().to_path_buf(), false);
                    o.config = serde_json::json!({
                        "model": "faux/echo",
                        "shutdown_grace_ms": 50,
                        "plugins": {
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": r#"[{"text":"saved"},{"text":"forked"}]"# }
                        }
                    });
                    o
                };
                let s1 = mgr.create_session(faux_opts()).await.unwrap();
                s1.host.prompt("persist-me", serde_json::json!({})).await.unwrap();
                let s2 = mgr.create_session(opts(d2.path().to_path_buf(), false)).await.unwrap();
                let sid1 = s1.session_id.clone();
                let path1 = host_session_info_field(&s1.host, "path").await.expect("persisted path1");

                // Multi-session Lua VM + EventLog isolation check.
                s1.host.lua().globals().set("ISOLATION_MARKER", "only-s1").unwrap();
                assert_eq!(s2.host.lua().globals().get::<Option<String>>("ISOLATION_MARKER").unwrap(), None);
                s1.host
                    .kernel
                    .event_log()
                    .admit_json("session/note", serde_json::json!({"text": "s1-only"}))
                    .await
                    .unwrap();
                assert_eq!(s1.host.kernel.event_log().session_identity().0.as_deref(), Some(sid1.as_str()));
                assert_ne!(s2.host.kernel.event_log().session_identity().0.as_deref(), Some(sid1.as_str()));

                // Positive fork_session path keeps source active with its own persisted file and state.
                let forked = mgr.fork_session(&sid1, faux_opts()).await.unwrap();
                forked.host.prompt("forked-only", serde_json::json!({})).await.unwrap();
                let forked_id = forked.session_id.clone();
                let forked_path = host_session_info_field(&forked.host, "path").await.expect("forked path");
                assert_ne!(forked_id, sid1);
                assert_ne!(forked_path, path1);
                assert!(mgr.session(&sid1).is_ok());
                assert_eq!(
                    s1.host.lua().globals().get::<Option<String>>("ISOLATION_MARKER").unwrap().as_deref(),
                    Some("only-s1")
                );
                assert_eq!(forked.host.lua().globals().get::<Option<String>>("ISOLATION_MARKER").unwrap(), None);
                drop(forked);
                drop(s1);
                drop(s2);

                // Close s1 so its SessionFile OS lock is released, then reload the exact same id.
                let report = mgr.close_session(&sid1).await.unwrap();
                assert!(!report.stop_unconfirmed());
                let mut reload_opts = opts(d1.path().to_path_buf(), false);
                reload_opts.args = serde_json::json!({ "resume": path1 });
                let reloaded = mgr.load_session(Some(&sid1), reload_opts).await.unwrap();
                assert_eq!(reloaded.session_id, sid1);
                assert!(mgr.session(&forked_id).is_ok());

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn close_session_cancellation_releases_closing_and_wakes_waiter() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let mgr = HostManager::new();
                let mut o = opts(d1.path().to_path_buf(), true);
                o.config["shutdown_grace_ms"] = serde_json::json!(200);
                let s1 = mgr.create_session(o).await.unwrap();
                let sid = s1.session_id.clone();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s1.host.lua(), move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx.blocking_recv();
                })
                .unwrap();
                entered_rx.await.unwrap();
                drop(s1);

                let close_task = tokio::task::spawn_local({
                    let mgr = Rc::clone(&mgr);
                    let sid = sid.clone();
                    async move { mgr.close_session(&sid).await }
                });
                tokio::task::yield_now().await;
                assert!(mgr.is_closing(&sid));

                let wait_task = tokio::task::spawn_local({
                    let mgr = Rc::clone(&mgr);
                    let sid = sid.clone();
                    async move { mgr.wait_closing(&sid).await }
                });
                tokio::task::yield_now().await;

                // Cancelling the in-flight close_session future drops ClosingGuard, clears `closing`,
                // and wakes wait_closing instead of leaking the slot.
                close_task.abort();
                let _ = close_task.await;
                tokio::time::timeout(Duration::from_secs(2), wait_task)
                    .await
                    .expect("wait_closing must wake when close_session is cancelled")
                    .unwrap();
                assert!(!mgr.is_closing(&sid));

                let _ = release_tx.send(());
                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn shutdown_all_terminates_when_in_flight_close_session_completes_before_prior_closing_entry() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mgr = HostManager::new();

                // Slow background begin_close entry holding closing[s0] across shutdown_grace_ms.
                let d0 = TempDir::new();
                let mut o0 = opts(d0.path().to_path_buf(), true);
                o0.config["shutdown_grace_ms"] = serde_json::json!(200);
                let s0 = mgr.create_session(o0).await.unwrap();
                let s0_id = s0.session_id.clone();
                let (e0_tx, e0_rx) = tokio::sync::oneshot::channel();
                let (r0_tx, r0_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s0.host.lua(), move || {
                    let _ = e0_tx.send(());
                    let _ = r0_rx.blocking_recv();
                })
                .unwrap();
                e0_rx.await.unwrap();
                drop(s0);

                // Fast in-flight close_session entries (multiple ids so HashMap order visits s0 before
                // at least one fast entry, triggering the watch<()> edge-loss deadlock before the fix).
                let mut fast_sessions = Vec::new();
                let mut fast_dirs = Vec::new();
                for _ in 0..4 {
                    let df = TempDir::new();
                    let mut of = opts(df.path().to_path_buf(), true);
                    of.config["shutdown_grace_ms"] = serde_json::json!(10);
                    let sf = mgr.create_session(of).await.unwrap();
                    let sf_id = sf.session_id.clone();
                    let (ef_tx, ef_rx) = tokio::sync::oneshot::channel();
                    let (rf_tx, rf_rx) = tokio::sync::oneshot::channel();
                    mare_binding::blocking::spawn(sf.host.lua(), move || {
                        let _ = ef_tx.send(());
                        let _ = rf_rx.blocking_recv();
                    })
                    .unwrap();
                    ef_rx.await.unwrap();
                    drop(sf);
                    fast_sessions.push((sf_id, rf_tx));
                    fast_dirs.push(df);
                }

                let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                mgr.begin_close(&s0_id, true, move |res| {
                    let _ = first_tx.send(res);
                })
                .unwrap();
                assert!(first_rx.await.unwrap().is_err());

                // Start the 4 fast close_session tasks after s0 is in its retry loop so all 5 are
                // concurrently in `self.closing` when shutdown_all drains them.
                let mut fast_releases = Vec::new();
                let mut fast_tasks = Vec::new();
                for (sf_id, rf_tx) in fast_sessions {
                    let close_task = tokio::task::spawn_local({
                        let mgr = Rc::clone(&mgr);
                        async move { mgr.close_session(&sf_id).await }
                    });
                    fast_releases.push(rf_tx);
                    fast_tasks.push(close_task);
                }

                // Ensure every fast close_session task has registered in `self.closing`.
                tokio::task::yield_now().await;
                assert_eq!(mgr.orchestrator().closing_len(), 5);

                // Unblock all fast closes so they finish while shutdown_all awaits s0's grace window.
                for rf_tx in fast_releases {
                    let _ = rf_tx.send(());
                }
                let shutdown_res = tokio::time::timeout(Duration::from_secs(3), mgr.shutdown_all()).await;
                assert!(shutdown_res.is_ok(), "shutdown_all deadlocked waiting on completed close_session");
                assert_eq!(mgr.orchestrator().closing_len(), 0);

                for task in fast_tasks {
                    let _ = task.await;
                }
                drop(fast_dirs);
                let _ = r0_tx.send(());
            })
            .await;
    }

    #[tokio::test]
    async fn workspace_registry_shares_trust_across_canonical_workspace_and_isolates_others() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let base = TempDir::new();
                let d1 = base.path().join("ws-a");
                let d2 = base.path().join("ws-b");
                std::fs::create_dir_all(&d1).unwrap();
                std::fs::create_dir_all(&d2).unwrap();
                #[cfg(unix)]
                let d1_alias = {
                    let link = base.path().join("ws-a-link");
                    std::os::unix::fs::symlink(&d1, &link).unwrap();
                    link
                };
                #[cfg(not(unix))]
                let d1_alias = d1.clone();

                let ws_opts = |cwd: PathBuf| {
                    let mut o = opts(cwd, true);
                    o.no_user_plugins = false;
                    o
                };
                let mgr = HostManager::new();
                let s1 = mgr.create_session(ws_opts(d1.clone())).await.unwrap();
                let s2 = mgr.create_session(ws_opts(d1_alias.clone())).await.unwrap();
                let s3 = mgr.create_session(ws_opts(d2)).await.unwrap();

                assert_eq!(s1.workspace, s2.workspace);
                assert_ne!(s1.workspace, s3.workspace);
                assert_eq!(mgr.workspaces().len(), 2);

                // Updating trust on ws-a updates all active sessions in ws-a without touching ws-b.
                assert!(mgr.set_workspace_trust(&s1.workspace, true, false).await.unwrap());
                assert!(s1.host.workspace.is_trusted());
                assert!(s2.host.workspace.is_trusted());
                assert!(!s3.host.workspace.is_trusted());

                // Newly attached session in ws-a inherits the active trust override.
                let s4 = mgr.create_session(ws_opts(d1_alias)).await.unwrap();
                assert!(s4.host.workspace.is_trusted());
                assert_eq!(mgr.workspaces().get(&s1.workspace).unwrap().session_ids.len(), 3);

                // Partial detach retains workspace trust and remaining session until last detaches.
                let _ = mgr.close_session(&s1.session_id).await.unwrap();
                assert_eq!(mgr.workspaces().trusted_override(&s2.workspace), Some(true));
                assert_eq!(mgr.workspaces().get(&s2.workspace).unwrap().session_ids.len(), 2);

                // Detaching remaining ws-a sessions prunes only ws-a.
                for sid in [&s2.session_id, &s4.session_id] {
                    let _ = mgr.close_session(sid).await.unwrap();
                }
                assert!(mgr.workspaces().get(&s1.workspace).is_none());
                assert_eq!(mgr.workspaces().len(), 1);
                assert!(mgr.workspaces().get(&s3.workspace).is_some());

                let report = mgr.shutdown_all_report().await;
                assert_eq!(report.sessions.len(), 1);
                assert!(!report.stop_unconfirmed());
                assert!(mgr.workspaces().is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn workspace_trust_revocation_synchronizes_runtime_plugins_and_sandbox() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let base = TempDir::new();
                let d1 = base.path().join("ws-a");
                let d2 = base.path().join("ws-b");
                std::fs::create_dir_all(d1.join(".mare/plugins")).unwrap();
                std::fs::create_dir_all(&d2).unwrap();
                std::fs::write(
                    d1.join(".mare/plugins/proj-probe.lua"),
                    "return { name = 'proj-probe', apply = function() end }\n",
                )
                .unwrap();

                #[cfg(unix)]
                let d1_alias = {
                    let link = base.path().join("ws-a-link");
                    std::os::unix::fs::symlink(&d1, &link).unwrap();
                    link
                };
                #[cfg(not(unix))]
                let d1_alias = d1.clone();

                let runtime_opts = |cwd: PathBuf, trusted: bool| HostOptions {
                    cwd,
                    mode: Mode::Print,
                    config: serde_json::json!({
                        "shutdown_grace_ms": 50,
                        "plugins": { "permissions": { "preset": "auto" } }
                    }),
                    load_init: true,
                    project_trusted: trusted,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: false,
                };

                let sandbox_level = |host: &Host| -> String {
                    host.lua().load("return mare.proc.sandbox.current().level").eval().unwrap()
                };
                let lua_trusted =
                    |host: &Host| -> bool { host.lua().load("return mare.trusted == true").eval().unwrap() };
                let proj_plugin_active = |host: &Host| -> bool {
                    host.kernel
                        .plugin_status()
                        .into_iter()
                        .any(|(n, origin, active)| n == "proj-probe" && origin == "project" && active)
                };

                let mgr = HostManager::new();
                let s1 = mgr.create_session(runtime_opts(d1.clone(), true)).await.unwrap();
                let s2 = mgr.create_session(runtime_opts(d1_alias.clone(), true)).await.unwrap();
                let s3 = mgr.create_session(runtime_opts(d2, true)).await.unwrap();

                for s in [&s1, &s2] {
                    assert!(s.host.workspace.is_trusted());
                    assert!(lua_trusted(&s.host));
                    assert!(proj_plugin_active(&s.host));
                    assert_eq!(sandbox_level(&s.host), "workspace-write");
                }

                // Cross-workspace authorization check rejects session belonging to ws-b.
                let mismatch = mgr
                    .set_workspace_trust_for_session(&s3.session_id, &s1.workspace, false, false)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(mismatch.code(), "permission_denied");
                assert!(lua_trusted(&s1.host));

                // Revoking trust on ws-a synchronously downgrades ALL attached sessions' Lua state,
                // project plugins, and sandbox policy while leaving ws-b untouched.
                assert!(
                    mgr.set_workspace_trust_for_session(&s1.session_id, &s1.workspace, false, false).await.unwrap()
                );
                for s in [&s1, &s2] {
                    assert!(!s.host.workspace.is_trusted());
                    assert!(!lua_trusted(&s.host));
                    assert!(!proj_plugin_active(&s.host), "project plugin must unload when trust is revoked");
                    assert_eq!(sandbox_level(&s.host), "read-only", "sandbox must downgrade on revocation");
                }
                assert!(lua_trusted(&s3.host));
                assert_eq!(sandbox_level(&s3.host), "workspace-write");

                // A newly created session in ws-a with project_trusted=true in opts still inherits
                // the active untrusted override and never loads project plugins or writable sandbox.
                let s4 = mgr.create_session(runtime_opts(d1_alias, true)).await.unwrap();
                assert!(!s4.host.workspace.is_trusted());
                assert!(!lua_trusted(&s4.host));
                assert!(!proj_plugin_active(&s4.host));
                assert_eq!(sandbox_level(&s4.host), "read-only");

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn host_answer_trust_syncs_siblings_and_persist_failure_is_atomic() {
        let home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let ws_opts = |cwd: PathBuf| {
                    let mut o = opts(cwd, true);
                    o.no_user_plugins = false;
                    o
                };
                let mgr = HostManager::new();
                let s1 = mgr.create_session(ws_opts(d1.path().to_path_buf())).await.unwrap();
                let s2 = mgr.create_session(ws_opts(d1.path().to_path_buf())).await.unwrap();
                assert!(!s1.host.workspace.is_trusted() && !s2.host.workspace.is_trusted());

                // Host::answer_trust on one session synchronizes sibling sessions + workspace override.
                s1.host.answer_trust(crate::TrustChoice::Trust).await.unwrap();
                assert!(s1.host.workspace.is_trusted());
                assert!(s2.host.workspace.is_trusted());
                assert_eq!(mgr.workspaces().trusted_override(&s1.workspace), Some(true));

                // Persistence failure leaves trusted_override and live sessions unmodified.
                let state_dir = home.path().join("state");
                std::fs::create_dir_all(&state_dir).unwrap();
                let _ = std::fs::remove_file(state_dir.join("trust.json"));
                std::fs::write(state_dir.join("trust.json"), "{corrupt").unwrap();
                let err = mgr.set_workspace_trust(&s1.workspace, false, true).await;
                assert!(err.is_err());
                assert_eq!(mgr.workspaces().trusted_override(&s1.workspace), Some(true));
                assert!(s1.host.workspace.is_trusted());
                assert!(s2.host.workspace.is_trusted());

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn shutdown_all_report_drains_closing_queue_concurrently_and_shares_in_flight_report() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let d2 = TempDir::new();
                let d3 = TempDir::new();
                let mgr = HostManager::new();
                let s1 = mgr.create_session(opts(d1.path().to_path_buf(), true)).await.unwrap();
                let s2 = mgr.create_session(opts(d2.path().to_path_buf(), true)).await.unwrap();
                let s3 = mgr.create_session(opts(d3.path().to_path_buf(), true)).await.unwrap();

                // Block s1 inside begin_close retry loop AND block active s2 simultaneously.
                let (e1_tx, e1_rx) = tokio::sync::oneshot::channel();
                let (r1_tx, r1_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s1.host.lua(), move || {
                    let _ = e1_tx.send(());
                    let _ = r1_rx.blocking_recv();
                })
                .unwrap();
                e1_rx.await.unwrap();

                let (e2_tx, e2_rx) = tokio::sync::oneshot::channel();
                let (r2_tx, r2_rx) = tokio::sync::oneshot::channel();
                mare_binding::blocking::spawn(s2.host.lua(), move || {
                    let _ = e2_tx.send(());
                    let _ = r2_rx.blocking_recv();
                })
                .unwrap();
                e2_rx.await.unwrap();

                let s1_id = s1.session_id.clone();
                let s2_id = s2.session_id.clone();
                let s3_id = s3.session_id.clone();
                drop(s1);
                drop(s2);
                drop(s3);

                let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                mgr.begin_close(&s1_id, true, move |res| {
                    let _ = first_tx.send(res);
                })
                .unwrap();
                assert!(first_rx.await.unwrap().is_err());
                assert!(mgr.is_closing(&s1_id));

                // Concurrent shutdown_all_report callers both receive the identical aggregated report
                // covering active (s2, s3) AND closing-queue (s1) sessions.
                let (rep_a, rep_b) = tokio::join!(mgr.shutdown_all_report(), mgr.shutdown_all_report());
                assert_eq!(rep_a, rep_b);
                assert_eq!(rep_a.sessions.len(), 3);
                assert!(rep_a.stop_unconfirmed());
                let e1 =
                    rep_a.sessions.iter().find(|e| e.session_id == s1_id).expect("closing session s1 must be reported");
                let e2 =
                    rep_a.sessions.iter().find(|e| e.session_id == s2_id).expect("active session s2 must be reported");
                let e3 =
                    rep_a.sessions.iter().find(|e| e.session_id == s3_id).expect("idle session s3 must be reported");
                assert!(e1.report.stop_unconfirmed());
                assert!(e1.report.unconfirmed_tasks.contains(&"blocking_scope"));
                assert!(e2.report.stop_unconfirmed());
                assert!(!e3.report.stop_unconfirmed());
                assert!(mgr.is_empty());
                assert!(!mgr.is_closing(&s1_id));
                assert!(mgr.workspaces().is_empty());

                let _ = r1_tx.send(());
                let _ = r2_tx.send(());
            })
            .await;
    }

    #[tokio::test]
    async fn persisted_session_with_sandbox_mode_downgrades_when_resumed_after_trust_revocation() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let write_target = d1.path().join("probe.txt");
                let persisted_opts = |trusted: bool, resume_path: Option<&str>| {
                    let mut args = serde_json::json!({ "no_session": false });
                    if let Some(path) = resume_path {
                        args["resume"] = serde_json::Value::String(path.to_string());
                    }
                    HostOptions {
                        cwd: d1.path().to_path_buf(),
                        mode: Mode::Print,
                        config: serde_json::json!({
                            "model": "faux/echo",
                            "shutdown_grace_ms": 50,
                            "plugins": {
                                "permissions": { "preset": "auto" },
                                "provider-faux": { "script_json": r#"[{"text":"saved"}]"# }
                            }
                        }),
                        load_init: true,
                        project_trusted: trusted,
                        args,
                        no_user_plugins: false,
                    }
                };

                let sandbox_level = |host: &Host| -> String {
                    host.lua().load("return mare.proc.sandbox.current().level").eval().unwrap()
                };
                let can_write = |host: &Host| -> bool {
                    host.lua()
                        .load(format!("return mare.proc.sandbox.check('write', {:?})", write_target.to_string_lossy()))
                        .eval()
                        .unwrap()
                };

                let mgr = HostManager::new();
                let s1 = mgr.create_session(persisted_opts(true, None)).await.unwrap();
                let sid1 = s1.session_id.clone();
                let ws_id = s1.workspace.clone();
                assert_eq!(sandbox_level(&s1.host), "workspace-write");
                assert!(can_write(&s1.host));

                // Persist the session, explicitly write a `sandbox/mode: workspace-write` record
                // via the session service, and confirm it landed on disk before revoking trust.
                s1.host.prompt("persist-sandbox", serde_json::json!({})).await.unwrap();
                {
                    let lua = s1.host.lua();
                    let sess_tbl = s1.host.service_table("session").unwrap();
                    lua.globals().set("S_TRUST_TEST", sess_tbl).unwrap();
                    lua.load("assert(S_TRUST_TEST:set_sandbox_mode('workspace-write'))").exec().unwrap();
                }
                let path1 = host_session_info_field(&s1.host, "path").await.expect("persisted path1");
                let contents = std::fs::read_to_string(&path1).unwrap();
                assert!(contents.contains("sandbox/mode"), "persisted session must record sandbox/mode");

                // Close s1, keep an active anchor session on the same workspace to revoke trust
                // runtime-wide, then resume the persisted session even if caller opts pass trusted=true.
                drop(s1);
                let _ = mgr.close_session(&sid1).await.unwrap();
                let _s_anchor = mgr.create_session(persisted_opts(true, None)).await.unwrap();
                assert!(mgr.set_workspace_trust(&ws_id, false, true).await.unwrap());

                let resumed = mgr.load_session(Some(&sid1), persisted_opts(true, Some(&path1))).await.unwrap();
                assert!(!resumed.host.workspace.is_trusted());
                assert_eq!(
                    sandbox_level(&resumed.host),
                    "read-only",
                    "untrusted resume must override persisted sandbox/mode down to read-only"
                );
                assert!(!can_write(&resumed.host), "untrusted resumed sandbox must deny workspace writes");

                // Verify workspace trust lookup also reflects false.
                assert!(!mgr.workspaces().get(&ws_id).unwrap().trusted);

                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn list_sessions_from_lua_sees_sibling_runtimes() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let opts = || HostOptions {
                    cwd: d1.path().to_path_buf(),
                    mode: Mode::Print,
                    config: serde_json::json!({
                        "model": "faux/echo",
                        "shutdown_grace_ms": 50,
                        "plugins": {
                            "preset": "minimal",
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": "[]" }
                        }
                    }),
                    load_init: false,
                    project_trusted: true,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                };
                let mgr = HostManager::new();
                let s1 = mgr.create_session(opts()).await.unwrap();
                let s2 = mgr.create_session(opts()).await.unwrap();
                let kernel_tbl = s1.host.service_table("kernel").unwrap();
                s1.host.lua().globals().set("K_TEST", kernel_tbl).unwrap();
                let raw: String = s1.host.lua().load("return mare.json.encode(K_TEST:list_sessions())").eval().unwrap();
                let list: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
                let ids: Vec<&str> = list.iter().filter_map(|v| v["session_id"].as_str()).collect();
                assert!(ids.contains(&s1.session_id.as_str()), "missing s1 in {raw}");
                assert!(ids.contains(&s2.session_id.as_str()), "missing s2 in {raw}");
                mgr.shutdown_all().await;
            })
            .await;
    }

    #[tokio::test]
    async fn standalone_host_lists_its_open_session() {
        let _home = HomeGuard::enter();
        tokio::task::LocalSet::new()
            .run_until(async {
                let d1 = TempDir::new();
                let host = Host::new(HostOptions {
                    cwd: d1.path().to_path_buf(),
                    mode: Mode::Print,
                    config: serde_json::json!({
                        "model": "faux/echo",
                        "shutdown_grace_ms": 50,
                        "plugins": {
                            "preset": "minimal",
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": "[]" }
                        }
                    }),
                    load_init: false,
                    project_trusted: true,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let kernel_tbl = host.service_table("kernel").unwrap();
                host.lua().globals().set("K_TEST", kernel_tbl).unwrap();
                let raw: String = host.lua().load("return mare.json.encode(K_TEST:list_sessions())").eval().unwrap();
                let list: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
                assert_eq!(list.len(), 1, "standalone host must list its open session: {raw}");
                assert!(list[0]["session_id"].as_str().is_some_and(|s| !s.is_empty()), "{raw}");
                assert!(list[0]["session_epoch"].as_str().is_some_and(|s| !s.is_empty()), "{raw}");
                host.shutdown().await;
            })
            .await;
    }
}

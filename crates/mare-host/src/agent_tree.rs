//! Root-shared admission and structured lifetime for child VM threads.
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

struct Budget {
    active: usize,
    max_active: usize,
    max_depth: usize,
}
struct CancelNode {
    reason: Mutex<Option<serde_json::Value>>,
    parent: Option<Arc<Self>>,
}

impl CancelNode {
    fn child(parent: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self { reason: Mutex::new(None), parent: Some(parent.clone()) })
    }
}

#[derive(Default)]
struct PolicyNodeState {
    handle: Option<mare_binding::ossandbox::PolicyHandle>,
    /// Tightening received while this node was reserved but had not yet attached its VM handle.
    /// Never inherited by future children: newly spawned children snapshot the parent's live
    /// policy via `apply_inheritance` at spawn time.
    pending_tighten: Option<mare_binding::ossandbox::Policy>,
    children: Vec<Weak<PolicyNode>>,
}

#[derive(Default)]
struct PolicyNode {
    state: Mutex<PolicyNodeState>,
}

impl PolicyNode {
    fn child(parent: &Arc<Self>) -> Arc<Self> {
        let mut parent_st = parent.state.lock().unwrap();
        let node = Arc::new(Self::default());
        parent_st.children.retain(|w| w.strong_count() > 0);
        parent_st.children.push(Arc::downgrade(&node));
        node
    }

    fn attach(&self, handle: mare_binding::ossandbox::PolicyHandle) {
        let mut st = self.state.lock().unwrap();
        if let Some(policy) = st.pending_tighten.take() {
            handle.tighten_ceiling(&policy);
        }
        st.handle = Some(handle);
    }

    fn tighten_descendants(&self, parent_policy: &mare_binding::ossandbox::Policy) {
        self.tighten_subtree(parent_policy, false);
    }

    fn tighten_subtree(&self, parent_policy: &mare_binding::ossandbox::Policy, include_self: bool) {
        let live = {
            let mut st = self.state.lock().unwrap();
            if include_self {
                if let Some(handle) = st.handle.as_ref() {
                    handle.tighten_ceiling(parent_policy);
                } else {
                    st.pending_tighten = Some(match st.pending_tighten.as_ref() {
                        Some(existing) => existing.intersect(parent_policy),
                        None => parent_policy.clone(),
                    });
                }
            }
            let mut out = Vec::new();
            st.children.retain(|w| {
                if let Some(strong) = w.upgrade() {
                    out.push(strong);
                    true
                } else {
                    false
                }
            });
            out
        };
        for child in live {
            child.tighten_subtree(parent_policy, true);
        }
    }
}

#[derive(Clone)]
pub(crate) struct Scope {
    budget: Arc<Mutex<Budget>>,
    state: Arc<Mutex<usize>>,
    changed: Arc<Notify>,
    root_counter: Option<(Arc<Mutex<usize>>, Arc<Notify>)>,
    pub(crate) cancel: CancellationToken,
    reason: Arc<CancelNode>,
    policy: Arc<PolicyNode>,
    depth: usize,
}
pub(crate) struct Reservation {
    parent: Scope,
}
impl Scope {
    pub(crate) fn root(config: &serde_json::Value) -> Result<Self, &'static str> {
        if config.get("agents").is_some_and(|value| !value.is_object()) {
            return Err("agents must be an object");
        }
        let limit = |key, default| match config.pointer(key) {
            None => Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .ok_or("agents.max_active and agents.max_depth must be non-negative integers representable as usize"),
        };
        Ok(Self {
            budget: Arc::new(Mutex::new(Budget {
                active: 0,
                max_active: limit("/agents/max_active", 16)?,
                max_depth: limit("/agents/max_depth", 4)?,
            })),
            state: Arc::new(Mutex::new(0)),
            changed: Arc::new(Notify::new()),
            root_counter: None,
            cancel: CancellationToken::new(),
            reason: Arc::new(CancelNode { reason: Mutex::new(None), parent: None }),
            policy: Arc::new(PolicyNode::default()),
            depth: 0,
        })
    }
    pub(crate) fn configure(&self, config: &serde_json::Value) -> Result<(), &'static str> {
        if self.depth == 0 {
            let configured = Self::root(config)?;
            let configured = configured.budget.lock().unwrap();
            let mut budget = self.budget.lock().unwrap();
            budget.max_active = configured.max_active;
            budget.max_depth = configured.max_depth;
        }
        Ok(())
    }

    /// A replaceable session lifetime with its own independent active count and notification,
    /// while still reporting active reservations to the root scope for process shutdown draining.
    pub(crate) fn session_scope(&self) -> Self {
        let root_counter = self.root_counter.clone().unwrap_or_else(|| (self.state.clone(), self.changed.clone()));
        Self {
            budget: self.budget.clone(),
            state: Arc::new(Mutex::new(0)),
            changed: Arc::new(Notify::new()),
            root_counter: Some(root_counter),
            cancel: self.cancel.child_token(),
            reason: CancelNode::child(&self.reason),
            policy: PolicyNode::child(&self.policy),
            depth: self.depth,
        }
    }
    pub(crate) fn reserve(&self) -> Result<(Self, Reservation), &'static str> {
        let mut budget = self.budget.lock().unwrap();
        if self.cancel.is_cancelled() {
            return Err("agent_tree_closed");
        }
        if self.depth >= budget.max_depth {
            return Err("agent_depth_limit");
        }
        if budget.active >= budget.max_active {
            return Err("agent_capacity_limit");
        }
        budget.active += 1;
        *self.state.lock().unwrap() += 1;
        if let Some((ref root_state, _)) = self.root_counter {
            *root_state.lock().unwrap() += 1;
        }
        Ok((
            Self {
                budget: self.budget.clone(),
                state: Arc::new(Mutex::new(0)),
                changed: Arc::new(Notify::new()),
                root_counter: None,
                cancel: self.cancel.child_token(),
                reason: CancelNode::child(&self.reason),
                policy: PolicyNode::child(&self.policy),
                depth: self.depth + 1,
            },
            Reservation { parent: self.clone() },
        ))
    }

    /// Attach a VM's sandbox policy handle to this scope so parent tightenings propagate to it.
    pub(crate) fn attach_policy(&self, handle: mare_binding::ossandbox::PolicyHandle) {
        self.policy.attach(handle);
    }

    /// Propagate a tightened parent sandbox policy to all active descendant subagent VMs.
    pub(crate) fn tighten_descendants(&self, parent_policy: &mare_binding::ossandbox::Policy) {
        self.policy.tighten_descendants(parent_policy);
    }
    pub(crate) fn active_count(&self) -> usize {
        *self.state.lock().unwrap()
    }

    /// Record the origin before cancelling, serialized with admission and other cancellations.
    pub(crate) fn cancel_as(&self, source: &'static str) {
        let _budget = self.budget.lock().unwrap();
        if !self.cancel.is_cancelled() {
            *self.reason.reason.lock().unwrap() = Some(serde_json::json!({ "source": source }));
            self.cancel.cancel();
        }
    }

    /// Preserve the causal path through ancestor scopes when cancellation propagates.
    pub(crate) fn cancel_reason(&self) -> serde_json::Value {
        let mut current = Some(self.reason.clone());
        let mut hops = 0;
        while let Some(node) = current {
            if let Some(mut reason) = node.reason.lock().unwrap().clone() {
                for _ in 0..hops {
                    reason = serde_json::json!({ "source": "parent", "cause": reason });
                }
                return reason;
            }
            hops += 1;
            current = node.parent.clone();
        }
        serde_json::json!({ "source": "parent" })
    }

    pub(crate) async fn drain_as(&self, source: &'static str) {
        self.cancel_as(source);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if *self.state.lock().unwrap() == 0 {
                break;
            }
            changed.await;
        }
    }

    pub(crate) async fn drain(&self) {
        self.drain_as("host").await;
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.parent.budget.lock().unwrap().active -= 1;
        *self.parent.state.lock().unwrap() -= 1;
        self.parent.changed.notify_waiters();
        if let Some((ref root_state, ref root_changed)) = self.parent.root_counter {
            *root_state.lock().unwrap() -= 1;
            root_changed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_limits_are_rejected_without_replacing_the_existing_budget() {
        let root = Scope::root(&serde_json::json!({"agents":{"max_active":0}})).unwrap();
        for invalid in [
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::json!({"max_active":-1}),
            serde_json::json!({"max_depth":1.5}),
            serde_json::json!({"max_active":"1"}),
            serde_json::json!({"max_depth":null}),
        ] {
            let config = serde_json::json!({"agents":invalid});
            assert!(Scope::root(&config).is_err(), "{config}");
            assert!(root.configure(&config).is_err(), "{config}");
            assert!(
                matches!(root.reserve(), Err("agent_capacity_limit")),
                "invalid configuration must not raise zero capacity"
            );
        }
    }
    #[tokio::test]
    async fn root_capacity_counts_siblings_descendants_and_releases() {
        let root = Scope::root(&serde_json::json!({"agents":{"max_active":2,"max_depth":2}})).unwrap();
        let (child, first) = root.reserve().unwrap();
        let (grandchild, second) = child.reserve().unwrap();
        assert!(matches!(root.reserve(), Err("agent_capacity_limit")));
        assert!(matches!(grandchild.reserve(), Err("agent_depth_limit")));
        drop(second);
        let (_, replacement) = root.reserve().unwrap();
        drop(replacement);
        drop(first);
        root.drain().await;
        assert!(matches!(root.reserve(), Err("agent_tree_closed")));
    }
    #[tokio::test]
    async fn shutdown_cancels_descendants_and_waits_for_startup_reservations() {
        let root = Scope::root(&serde_json::json!({})).unwrap();
        let (child, first) = root.reserve().unwrap();
        let (grandchild, second) = child.reserve().unwrap();
        let drain = root.drain();
        tokio::pin!(drain);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(10), &mut drain).await.is_err());
        assert!(child.cancel.is_cancelled());
        assert!(grandchild.cancel.is_cancelled());
        assert!(matches!(child.reserve(), Err("agent_tree_closed")));
        drop(second);
        drop(first);
        drain.await;
    }
    #[tokio::test]
    async fn cancelling_branch_propagates_without_cancelling_sibling() {
        let root = Scope::root(&serde_json::json!({})).unwrap();
        let (child, first) = root.reserve().unwrap();
        let (sibling, other) = root.reserve().unwrap();
        let (grandchild, second) = child.reserve().unwrap();
        child.cancel.cancel();
        assert!(grandchild.cancel.is_cancelled());
        assert!(!sibling.cancel.is_cancelled());
        assert!(matches!(grandchild.reserve(), Err("agent_tree_closed")));
        drop(second);
        child.drain().await;
        drop(first);
        drop(other);
        root.drain().await;
    }
    #[test]
    fn concurrent_admission_never_exceeds_root_budget() {
        let root = Scope::root(&serde_json::json!({"agents":{"max_active":1}})).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|threads| {
            for _ in 0..16 {
                let root = root.clone();
                let barrier = barrier.clone();
                let admitted = admitted.clone();
                threads.spawn(move || {
                    barrier.wait();
                    let reservation = root.reserve().ok();
                    if reservation.is_some() {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    barrier.wait();
                    drop(reservation);
                });
            }
        });
        assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn session_scopes_have_independent_active_counts_while_root_tracks_all() {
        let root = Scope::root(&serde_json::json!({})).unwrap();
        let gen1 = root.session_scope();
        let (_child1, stuck_res) = gen1.reserve().unwrap();
        assert_eq!(gen1.active_count(), 1);
        assert_eq!(root.active_count(), 1);

        // Even while gen1 still holds a stuck reservation, a fresh gen2 session_scope must have
        // its own independent active_count == 0 and drain immediately without timing out.
        let gen2 = root.session_scope();
        assert_eq!(gen2.active_count(), 0, "new session_scope must not inherit old generation's stuck active count");
        let drained =
            tokio::time::timeout(std::time::Duration::from_millis(200), gen2.drain_as("session_switch")).await;
        assert!(drained.is_ok(), "gen2 drain must not block on gen1's stuck reservation");

        // Root drain must still track gen1's stuck reservation until it drops.
        let root_drain = tokio::spawn({
            let root = root.clone();
            async move { root.drain().await }
        });
        tokio::task::yield_now().await;
        assert!(!root_drain.is_finished(), "root.drain must wait for stuck gen1 reservation");
        drop(stuck_res);
        tokio::time::timeout(std::time::Duration::from_millis(500), root_drain)
            .await
            .expect("root.drain must finish after stuck reservation drops")
            .unwrap();
    }

    fn test_lua_with_sandbox(cwd: &std::path::Path) -> mlua::Lua {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        let proc = lua.create_table().unwrap();
        mare_binding::ossandbox::install(&lua, &proc, cwd.to_string_lossy().to_string()).unwrap();
        mare.set("proc", proc).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua
    }

    fn read_only_policy(cwd: &std::path::Path) -> mare_binding::ossandbox::Policy {
        mare_binding::ossandbox::Policy {
            level: mare_binding::ossandbox::Level::ReadOnly,
            root: cwd.to_path_buf(),
            extra: vec![],
            root_writable: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            writable: vec![],
            readable: vec![],
            deny_read: vec![],
            restrict_read: false,
            reads_pinned: false,
            network: mare_binding::ossandbox::Network::Open,
            profile: None,
            unix_sockets: vec![],
            accept_partial: vec![],
            project_protected: vec![],
        }
    }

    #[test]
    fn policy_node_tighten_descendants_propagates_to_reserved_and_attached_children() {
        let cwd = std::env::temp_dir();
        let ro = read_only_policy(&cwd);
        let root = Scope::root(&serde_json::json!({})).unwrap();
        let session = root.session_scope();

        // Child 1 is already attached before parent tightens; child 2 is reserved before parent
        // tightens and attaches concurrently afterward. Both must be tightened.
        let (attached_child, _res1) = session.reserve().unwrap();
        let lua1 = test_lua_with_sandbox(&cwd);
        let handle1 = mare_binding::ossandbox::policy_handle(&lua1).unwrap();
        attached_child.attach_policy(handle1);

        let (pending_child, _res2) = session.reserve().unwrap();
        root.tighten_descendants(&ro);

        let level1: String = lua1.load("return mare.proc.sandbox.current().level").eval().unwrap();
        assert_eq!(level1, "read-only", "already-attached child must be tightened immediately");

        let lua2 = test_lua_with_sandbox(&cwd);
        let handle2 = mare_binding::ossandbox::policy_handle(&lua2).unwrap();
        let gen_before: u64 = lua2.load("return mare.proc.sandbox.generation()").eval().unwrap();
        pending_child.attach_policy(handle2);
        let gen_after: u64 = lua2.load("return mare.proc.sandbox.generation()").eval().unwrap();
        let level2: String = lua2.load("return mare.proc.sandbox.current().level").eval().unwrap();
        assert!(gen_after > gen_before, "pending child attach must inherit tightening received after reserve");
        assert_eq!(level2, "read-only");
    }

    #[test]
    fn policy_node_tighten_descendants_does_not_poison_children_reserved_after_parent_widens() {
        let cwd = std::env::temp_dir();
        let ro = read_only_policy(&cwd);
        let root = Scope::root(&serde_json::json!({})).unwrap();
        let session = root.session_scope();

        // Parent starts in a restricted policy (e.g. read-only or auto), triggering tighten_descendants.
        root.tighten_descendants(&ro);

        // Later the parent widens to full-access and reserves a brand-new child under the same
        // session_scope (or a fresh session_scope). The new child snapshots the live parent policy
        // in apply_inheritance and must not inherit the stale pre-widening policy.
        for parent_scope in [&session, &root.session_scope()] {
            let (new_child, _res) = parent_scope.reserve().unwrap();
            let lua = test_lua_with_sandbox(&cwd);
            let handle = mare_binding::ossandbox::policy_handle(&lua).unwrap();
            let gen_before: u64 = lua.load("return mare.proc.sandbox.generation()").eval().unwrap();
            new_child.attach_policy(handle);
            let gen_after: u64 = lua.load("return mare.proc.sandbox.generation()").eval().unwrap();
            let is_unconfined: bool = lua.load("return mare.proc.sandbox.current() == nil").eval().unwrap();
            assert_eq!(gen_after, gen_before, "new child must not receive stale pre-widening tightenings");
            assert!(is_unconfined, "new child under widened full-access parent must remain unconfined");
        }
    }
}

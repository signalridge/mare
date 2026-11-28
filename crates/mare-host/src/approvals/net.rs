//! Network approval: a sandboxed process asking for a host outside the allow list.

use std::sync::Arc;

use mlua::{MultiValue, Value};

use crate::Host;

async fn recheck_net_policy(
    h: &Host,
    host: &str,
    port: u16,
    reply: &mut tokio::sync::oneshot::Sender<bool>,
    session_cancel: &tokio_util::sync::CancellationToken,
) -> bool {
    for _ in 0..4 {
        if session_cancel.is_cancelled() {
            return true;
        }
        let check_epoch = h.kernel.policy_epoch();
        if h.kernel.voter_count("net/approval") > 0 {
            let lua = h.lua();
            let Ok(t) = lua.create_table() else {
                return true;
            };
            let _ = t.set("host", host);
            let _ = t.set("port", port);
            let check = h.kernel.policy("net/approval", MultiValue::from_vec(vec![Value::Table(t)]));
            let denied = tokio::select! {
                biased;
                _ = reply.closed() => true,
                _ = session_cancel.cancelled() => true,
                res = check => match res {
                    Ok(vote) => {
                        let d = vote.get::<String>("decision").map(|d| d == "deny").unwrap_or(true);
                        d || net_vote_violates_sandbox(lua, &vote, host, port, &h.cwd).unwrap_or(true)
                    }
                    Err(_) => true,
                },
            };
            if denied {
                return true;
            }
        }
        if h.kernel.policy_epoch() == check_epoch {
            return session_cancel.is_cancelled();
        }
    }
    true
}

impl Host {
    /// Network approval: a sandboxed process asking for a host outside the allow
    /// list becomes a `net/approval` policy vote (fail-closed; nobody listening = refused).
    /// `allow_session` / `allow_amend` are cached per session and host:port;
    /// every outcome is reported as `approval/requested` and `approval/resolved`.
    pub(crate) fn install_net_approver(self: &Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mare_binding::netproxy::NetAsk>(256);
        mare_binding::netproxy::set_approver(self.lua(), Some(mare_binding::netproxy::NetApprover::new(tx)));
        let weak = Arc::downgrade(self);
        tokio::task::spawn_local(async move {
            let mut cache: std::collections::HashSet<(String, String, u16)> = Default::default();
            let origin_matches =
                |current: &(Option<String>, String), origin: Option<&mare_binding::netauth::NetAskOrigin>| match origin
                {
                    Some(o) => o.session_id == current.0 && o.session_epoch == current.1,
                    None => true,
                };
            while let Some(mut ask) = rx.recv().await {
                if ask.reply.is_closed() {
                    continue;
                }
                let Some(h) = weak.upgrade() else {
                    let _ = ask.reply.send(false);
                    continue;
                };
                let current_identity = h.kernel.event_log().session_identity();
                if !origin_matches(&current_identity, ask.origin.as_ref()) {
                    let approval_id = mare_binding::codec::random_hex(16);
                    h.emit_approval_pair(
                        &approval_id,
                        "network",
                        None,
                        Some(serde_json::json!({ "host": ask.host, "port": ask.port })),
                        None,
                        false,
                        &serde_json::json!({
                            "call": { "tool_call_id": "", "name": "network" },
                            "decision": "deny",
                            "source": "cancelled",
                        }),
                    )
                    .await;
                    let _ = ask.reply.send(false);
                    continue;
                }
                let (frozen_sid, frozen_epoch) = match &ask.origin {
                    Some(o) => (o.session_id.as_deref().unwrap_or("").to_string(), o.session_epoch.clone()),
                    None => (current_identity.0.as_deref().unwrap_or("").to_string(), current_identity.1.clone()),
                };
                let revision = match h.service_table(mare_shared::APPROVALS_SERVICE_NAME) {
                    Some(service) => match service.get::<mlua::Function>("version") {
                        Ok(f) => match f.call_async::<String>(service).await {
                            Ok(version) => version,
                            Err(_) => {
                                let _ = ask.reply.send(false);
                                continue;
                            }
                        },
                        Err(_) => {
                            let _ = ask.reply.send(false);
                            continue;
                        }
                    },
                    None => "disabled".into(),
                };
                let perm_revision = async |host: &Self| -> u64 {
                    let Some(service) = host.service_table(mare_shared::PERMISSIONS_SERVICE_NAME) else {
                        return 0;
                    };
                    let Ok(f) = service.get::<mlua::Function>("policy_revision") else {
                        return 0;
                    };
                    f.call_async::<u64>(service).await.unwrap_or(0)
                };
                let mut epoch = h.kernel.policy_epoch();
                let mut perm_rev = perm_revision(&h).await;
                let mut sb_gen = mare_binding::ossandbox::generation(h.lua());
                let mut key = (
                    format!("{frozen_sid}:{frozen_epoch}:{revision}:{epoch}:{perm_rev}:{sb_gen}"),
                    ask.host.clone(),
                    ask.port,
                );
                let session_cancel = h.session_cancel_token();
                if cache.contains(&key) {
                    // Re-evaluate mandatory net/approval vetoes even on a cached session grant
                    // so dynamic listener state or deny precedence still blocks.
                    let vetoed = recheck_net_policy(&h, &ask.host, ask.port, &mut ask.reply, &session_cancel).await;
                    let still_same_session = !session_cancel.is_cancelled()
                        && origin_matches(&h.kernel.event_log().session_identity(), ask.origin.as_ref());
                    if vetoed || !still_same_session {
                        let approval_id = mare_binding::codec::random_hex(16);
                        h.emit_approval_pair(
                            &approval_id,
                            "network",
                            None,
                            Some(serde_json::json!({ "host": ask.host, "port": ask.port })),
                            None,
                            false,
                            &serde_json::json!({
                                "call": { "tool_call_id": "", "name": "network" },
                                "decision": "deny",
                                "source": if still_same_session { "policy" } else { "cancelled" },
                            }),
                        )
                        .await;
                        let _ = ask.reply.send(false);
                        continue;
                    }
                    let _ = ask.reply.send(true);
                    continue;
                }
                let approval_id = mare_binding::codec::random_hex(16);
                let req_opts = h.lua().create_table().ok();
                if let Some(opts) = &req_opts {
                    let _ = opts.set("approval_id", approval_id.as_str());
                    let _ = opts.set("emit_resolved", false);
                }
                // If the proxy client disappears or the originating session switches/cancels,
                // drop the policy wait immediately so stale grants never leak across sessions.
                let answer = tokio::select! {
                    biased;
                    _ = ask.reply.closed() => None,
                    _ = session_cancel.cancelled() => None,
                    answer = h.ask_net(&ask.host, ask.port, req_opts.as_ref()) => Some(answer),
                };
                let (mut allowed, mut decision, mut source) = match answer {
                    Some(Ok((d, grant, src))) => {
                        let ok = matches!(d.as_str(), "allow" | "allow_session" | "allow_amend");
                        (ok, if ok { grant } else { d }, src)
                    }
                    Some(Err(_)) => (false, "deny".to_string(), "error".to_string()),
                    None => (false, "deny".to_string(), "cancelled".to_string()),
                };
                let post_identity = h.kernel.event_log().session_identity();
                if session_cancel.is_cancelled() || !origin_matches(&post_identity, ask.origin.as_ref()) {
                    allowed = false;
                    decision = "deny".into();
                    source = "cancelled".into();
                } else if allowed
                    && (h.kernel.policy_epoch() != epoch
                        || perm_revision(&h).await != perm_rev
                        || mare_binding::ossandbox::generation(h.lua()) != sb_gen)
                {
                    if recheck_net_policy(&h, &ask.host, ask.port, &mut ask.reply, &session_cancel).await
                        || session_cancel.is_cancelled()
                        || !origin_matches(&h.kernel.event_log().session_identity(), ask.origin.as_ref())
                    {
                        allowed = false;
                        decision = "deny".into();
                        source = if !session_cancel.is_cancelled()
                            && origin_matches(&h.kernel.event_log().session_identity(), ask.origin.as_ref())
                        {
                            "policy".into()
                        } else {
                            "cancelled".into()
                        };
                    } else {
                        epoch = h.kernel.policy_epoch();
                        perm_rev = perm_revision(&h).await;
                        sb_gen = mare_binding::ossandbox::generation(h.lua());
                        key.0 = format!("{frozen_sid}:{frozen_epoch}:{revision}:{epoch}:{perm_rev}:{sb_gen}");
                    }
                }
                // A vanished proxy request or switched session must never leave a session-wide grant behind.
                if ask.reply.send(allowed).is_ok() {
                    if allowed && (decision == "allow_session" || decision == "allow_amend") {
                        cache.insert(key);
                    }
                } else {
                    decision = "deny".into();
                    source = "cancelled".into();
                }
                let requested_emitted =
                    req_opts.as_ref().and_then(|t| t.get::<Option<bool>>("requested").ok().flatten()).unwrap_or(false);
                h.emit_approval_pair(
                    &approval_id,
                    "network",
                    None,
                    Some(serde_json::json!({ "host": ask.host, "port": ask.port })),
                    None,
                    requested_emitted,
                    &serde_json::json!({
                        "call": { "tool_call_id": "", "name": "network" },
                        "decision": decision,
                        "source": source,
                    }),
                )
                .await;
            }
        });
    }

    async fn ask_net(
        &self,
        host: &str,
        port: u16,
        req_opts: Option<&mlua::Table>,
    ) -> mlua::Result<(String, String, String)> {
        // The network policy requires an affirmative grant; listener presence alone
        // is not approval (observers can abstain or disappear before dispatch).
        let lua = self.lua();
        let req = lua.create_table()?;
        req.set("host", host)?;
        req.set("port", port)?;
        let policy = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (policy_kernel, policy_action, captured_policy) = (self.kernel.clone(), req.clone(), policy.clone());
        let check_policy = lua.create_async_function(move |lua, ()| {
            let (kernel, action, captured) = (policy_kernel.clone(), policy_action.clone(), captured_policy.clone());
            mare_kernel::as_host(async move {
                let vote = if kernel.voter_count("net/approval") > 0 {
                    let vote = kernel
                        .policy_with(
                            "net/approval",
                            MultiValue::from_vec(vec![Value::Table(action)]),
                            mare_kernel::PolicyOptions { require_explicit_approval: true },
                        )
                        .await?;
                    if vote.get::<Option<mlua::Table>>("sandboxes")?.is_none()
                        && let Some(constraints) = vote.get::<Option<mlua::Table>>("sandbox_constraints")?
                    {
                        vote.set("sandboxes", constraints)?;
                    }
                    vote
                } else {
                    let vote = lua.create_table()?;
                    vote.set("decision", "ask")?;
                    vote
                };
                *captured.borrow_mut() = Some(vote.clone());
                Ok(vote)
            })
        })?;
        let (kernel, host_owned) = (self.kernel.clone(), host.to_string());
        let answer = lua.create_async_function(move |lua, binding: Option<mlua::Table>| {
            let kernel = kernel.clone();
            let policy = policy.borrow().clone();
            // The answerer gets its own request table: the binding must not leak into the
            // action the policy vote re-checks after the answer.
            let action = lua.create_table();
            let host_owned = host_owned.clone();
            mare_kernel::as_host(async move {
                let action = action?;
                action.set("host", host_owned)?;
                action.set("port", port)?;
                if let Some(binding) = binding {
                    action.set("approval_binding", binding)?;
                }
                let has_host = kernel.voter_count("net/approval-host") > 0;
                if !has_host
                    && let Some(policy) = policy
                    && policy.get::<String>("decision")? == "allow"
                {
                    return Ok(policy);
                }
                kernel
                    .policy_with(
                        "net/approval-host",
                        MultiValue::from_vec(vec![Value::Table(action)]),
                        mare_kernel::PolicyOptions { require_explicit_approval: true },
                    )
                    .await
            })
        })?;
        let review_action = lua.create_table()?;
        review_action.set("host", host)?;
        review_action.set("port", port)?;
        let history = self
            .call_service(mare_shared::AGENT_SERVICE_NAME, "messages", MultiValue::new())
            .await
            .ok()
            .and_then(|values| values.into_iter().next())
            .unwrap_or(Value::Nil);
        review_action.set("history", history)?;
        let v = if let Some(service) = self.service_table(mare_shared::APPROVALS_SERVICE_NAME) {
            let request: mlua::Function = service.get("request")?;
            let (result, receipt): (mlua::Table, Value) = match req_opts {
                Some(opts) => {
                    request
                        .call_async((service, "network", review_action, check_policy, answer, Value::Nil, opts))
                        .await?
                }
                None => request.call_async((service, "network", review_action, check_policy, answer)).await?,
            };
            if !receipt.is_nil() {
                let receipt = mare_shared::lua::lua_to_json(lua, receipt)?;
                if !mare_binding::approval::consume_receipt(lua, &receipt, None, None) {
                    result.set("decision", "deny")?;
                }
            }
            result
        } else {
            let vote = lua.create_table()?;
            vote.set("decision", "deny")?;
            vote.set("unavailable", true)?;
            vote.set("source", "none")?;
            vote
        };
        let mut d: String = v.get("decision")?;
        if v.get::<Option<bool>>("unavailable")?.unwrap_or(false) {
            d = "unavailable".into();
        }
        let mut source = v
            .get::<Option<String>>("source")?
            .unwrap_or_else(|| if d == "unavailable" { "none".into() } else { "policy".into() });
        if matches!(d.as_str(), "allow" | "allow_session" | "allow_amend")
            && net_vote_violates_sandbox(lua, &v, host, port, &self.cwd)?
        {
            // A network answer only grants this host/port. Every policy profile must still
            // permit that network operation; a reviewer cannot erase its network ceiling.
            d = "deny".into();
            source = "policy".into();
        }
        let grant =
            v.get::<Option<String>>("grant")?.unwrap_or_else(|| if d == "allow" { "allow".into() } else { d.clone() });
        Ok((d, grant, source))
    }
}

fn net_vote_violates_sandbox(
    lua: &mlua::Lua,
    vote: &mlua::Table,
    host: &str,
    port: u16,
    cwd: &std::path::Path,
) -> mlua::Result<bool> {
    let target = format!("{host}:{port}");
    for field in ["sandbox_constraints", "sandboxes"] {
        if let Some(constraints) = vote.get::<Option<mlua::Table>>(field)? {
            for constraint in constraints.sequence_values::<Value>() {
                if let Some(policy) = mare_binding::ossandbox::Policy::from_lua(lua, &constraint?, cwd)?
                    && policy.check("network", &target, cwd).is_err()
                {
                    return Ok(true);
                }
            }
        }
    }
    if let Some(sandbox) = vote.get::<Option<Value>>("sandbox")?
        && let Some(policy) = mare_binding::ossandbox::Policy::from_lua(lua, &sandbox, cwd)?
        && policy.check("network", &target, cwd).is_err()
    {
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Host, HostOptions};
    use mare_binding::netauth::{NetPolicy, approver};
    use mare_shared::ui::Mode;

    static RESOLVER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct ResolverGuard<'a> {
        _lock: tokio::sync::MutexGuard<'a, ()>,
    }

    impl<'a> ResolverGuard<'a> {
        async fn acquire(map: std::collections::HashMap<String, Vec<std::net::IpAddr>>) -> Self {
            let lock = RESOLVER_TEST_LOCK.lock().await;
            mare_binding::netauth::set_resolver_override(Some(map));
            Self { _lock: lock }
        }
    }

    impl Drop for ResolverGuard<'_> {
        fn drop(&mut self) {
            mare_binding::netauth::set_resolver_override(None);
        }
    }

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "mare-net-test-{}-{}",
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

    fn opts(cwd: std::path::PathBuf, mode: Mode) -> HostOptions {
        HostOptions {
            cwd,
            mode,
            config: serde_json::json!({}),
            load_init: false,
            project_trusted: false,
            args: serde_json::json!({ "no_session": true }),
            no_user_plugins: true,
        }
    }

    #[tokio::test]
    async fn child_startup_events_use_assigned_session_identity() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let scope = crate::agent_tree::Scope::root(&serde_json::json!({})).unwrap();
                let child = Host::new_child(
                    opts(tmp.path().to_path_buf(), Mode::Child),
                    None,
                    Default::default(),
                    scope,
                    None,
                    None,
                    "child-startup-id".into(),
                )
                .await
                .unwrap();

                let (id, epoch) = child.kernel.event_log().session_identity();
                assert_eq!(id.as_deref(), Some("child-startup-id"));
                assert!(epoch.ends_with(":1"), "startup must retain its initial epoch: {epoch}");
                assert_eq!(mare_binding::peers::session_cell(child.lua()).get().as_deref(), Some("child-startup-id"));

                let mut replay =
                    child.kernel.subscribe_cursor(Some(&epoch), Some(0), false, Arc::new(|_| Some(true)), None).cursor;
                let mut found_approval = false;
                while let Ok(Some(record)) = replay.try_recv() {
                    if let mare_kernel::CursorRecord::Event { event, .. } = record
                        && event.name == "approval/changed"
                    {
                        assert_eq!(event.session_id.as_deref(), Some("child-startup-id"));
                        assert_eq!(event.session_epoch, epoch);
                        found_approval = true;
                    }
                }
                assert!(found_approval, "startup approval event must be replayable in the child epoch");
                child.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn parent_to_child_explicit_net_approver_delegation_is_isolated() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let tmp = TempDir::new();
                let p1 = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                let p2 = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();

                let asked1 = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
                let asked2 = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

                let a1 = asked1.clone();
                let f1 = p1
                    .lua()
                    .create_function(move |lua, req: mlua::Table| {
                        let host: String = req.get("host")?;
                        a1.lock().unwrap().push(host);
                        let res = lua.create_table()?;
                        res.set("decision", "allow")?;
                        res.set("grant", "allow")?;
                        Ok(res)
                    })
                    .unwrap();
                // a voter of the host's fiber
                let (c1, o1) = (p1.kernel.host_ctx().unwrap(), p1.kernel.host_owner().unwrap());
                p1.kernel.host_voter(&c1, o1, "net/approval", f1, mare_kernel::HostVoter::default()).unwrap();

                let a2 = asked2.clone();
                let f2 = p2
                    .lua()
                    .create_function(move |lua, req: mlua::Table| {
                        let host: String = req.get("host")?;
                        a2.lock().unwrap().push(host);
                        let res = lua.create_table()?;
                        res.set("decision", "deny")?;
                        Ok(res)
                    })
                    .unwrap();
                let (c2, o2) = (p2.kernel.host_ctx().unwrap(), p2.kernel.host_owner().unwrap());
                p2.kernel.host_voter(&c2, o2, "net/approval", f2, mare_kernel::HostVoter::default()).unwrap();

                // Child explicitly receives p1's network approver even though p2 was created after p1.
                let scope = crate::agent_tree::Scope::root(&serde_json::json!({})).unwrap();
                let child = Host::new_child(
                    opts(tmp.path().to_path_buf(), Mode::Child),
                    None,
                    Default::default(),
                    scope,
                    approver(p1.lua()),
                    None,
                    "child-net-1".into(),
                )
                .await
                .unwrap();
                assert_eq!(child.kernel.event_log().session_identity().0.as_deref(), Some("child-net-1"));

                let child_approver = approver(child.lua());
                assert!(child_approver.is_some());
                // Use an IP-free public hostname with resolver override so no real DNS is needed.
                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "delegated.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;
                let res = NetPolicy::deny_all().authorize("delegated.test", 443, child_approver.as_ref()).await;
                drop(_resolver);
                assert!(res.is_ok(), "{res:?}");
                assert_eq!(*asked1.lock().unwrap(), vec!["delegated.test", "delegated.test"]);
                assert!(asked2.lock().unwrap().is_empty());

                // A child created without an explicit parent approver fails closed.
                let scope2 = crate::agent_tree::Scope::root(&serde_json::json!({})).unwrap();
                let orphan = Host::new_child(
                    opts(tmp.path().to_path_buf(), Mode::Child),
                    None,
                    Default::default(),
                    scope2,
                    None,
                    None,
                    "child-net-2".into(),
                )
                .await
                .unwrap();
                assert_eq!(orphan.kernel.event_log().session_identity().0.as_deref(), Some("child-net-2"));
                assert!(approver(orphan.lua()).is_none());
                assert!(
                    NetPolicy::deny_all()
                        .authorize("delegated.test", 443, approver(orphan.lua()).as_ref())
                        .await
                        .is_err()
                );

                orphan.shutdown().await;
                child.shutdown().await;
                p2.shutdown().await;
                p1.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn net_cache_hit_rechecks_when_policy_epoch_changes_during_yield() {
        const EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let h = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
                let entered_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(entered_tx)));
                let release_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
                let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let lua = h.lua();
                // voters of the host's fiber
                let (hc, ho) = (h.kernel.host_ctx().unwrap(), h.kernel.host_owner().unwrap());
                let (resolved_tx, mut resolved_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let on_resolved = lua
                    .create_function(move |_, event: mlua::Table| {
                        let result: mlua::Table = event.get("result")?;
                        let _ = resolved_tx.send(result.get("decision")?);
                        Ok(())
                    })
                    .unwrap();
                h.kernel.host_on(&hc, ho, "approval/resolved", on_resolved, mare_kernel::HostOn::default()).unwrap();
                let c_calls = calls.clone();
                let f = lua
                    .create_async_function(move |lua, ()| {
                        let c_calls = c_calls.clone();
                        let entered_tx = entered_tx.clone();
                        let release_rx = release_rx.clone();
                        async move {
                            let n = c_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            // 1st + 2nd calls populate cache (pre-check + post-check); 3rd call is cache-hit recheck.
                            if n == 2 {
                                if let Some(tx) = entered_tx.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                if let Some(rx) = release_rx.lock().await.take() {
                                    let _ = rx.await;
                                }
                            }
                            let t = lua.create_table()?;
                            t.set("decision", "allow")?;
                            t.set("grant", "allow_session")?;
                            Ok(t)
                        }
                    })
                    .unwrap();
                h.kernel.host_voter(&hc, ho, "net/approval", f, mare_kernel::HostVoter::default()).unwrap();

                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "epoch-race.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;
                let ap = mare_binding::netproxy::approver(lua).unwrap();
                let first =
                    mare_binding::netauth::NetPolicy::deny_all().authorize("epoch-race.test", 443, Some(&ap)).await;
                assert!(first.is_ok(), "first request should be cached as allow_session: {first:?}");
                let first_resolved = tokio::time::timeout(EVENT_TIMEOUT, resolved_rx.recv())
                    .await
                    .expect("first approval event timed out")
                    .expect("approval event listener closed");
                assert!(first_resolved.starts_with("allow"), "{first_resolved}");

                let policy = mare_binding::netauth::NetPolicy::deny_all();
                let mut second = Box::pin(policy.authorize("epoch-race.test", 443, Some(&ap)));
                tokio::select! {
                    biased;
                    res = &mut second => panic!("cache recheck returned before the voter was released: {res:?}"),
                    entered = tokio::time::timeout(EVENT_TIMEOUT, entered_rx) => {
                        entered.expect("cache recheck timed out").unwrap();
                    }
                }

                // Register a new net/approval deny listener while the cache-hit recheck is yielding.
                let deny_fn = lua
                    .create_function(|lua, ()| {
                        let t = lua.create_table()?;
                        t.set("decision", "deny")?;
                        t.set("reason", "late deny listener")?;
                        Ok(t)
                    })
                    .unwrap();
                h.kernel.host_voter(&hc, ho, "net/approval", deny_fn, mare_kernel::HostVoter::default()).unwrap();
                let (audit_entered_tx, audit_entered_rx) = tokio::sync::oneshot::channel::<()>();
                let (audit_release_tx, audit_release_rx) = tokio::sync::oneshot::channel::<()>();
                let audit_entered_tx = Arc::new(std::sync::Mutex::new(Some(audit_entered_tx)));
                let audit_release_rx = Arc::new(tokio::sync::Mutex::new(Some(audit_release_rx)));
                let on_denial_resolved = lua
                    .create_async_function(move |_, event: mlua::Table| {
                        let entered = audit_entered_tx.clone();
                        let release = audit_release_rx.clone();
                        async move {
                            let result: mlua::Table = event.get("result")?;
                            if result.get::<String>("decision")? != "deny" {
                                return Err(mlua::Error::runtime("expected resolved denial"));
                            }
                            if let Some(tx) = entered.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release.lock().await.take() {
                                let _ = rx.await;
                            }
                            Ok(())
                        }
                    })
                    .unwrap();
                h.kernel
                    .host_on(&hc, ho, "approval/resolved", on_denial_resolved, mare_kernel::HostOn::default())
                    .unwrap();
                let _ = release_tx.send(());

                tokio::select! {
                    biased;
                    res = &mut second => panic!("cached denial replied before its audit event: {res:?}"),
                    entered = tokio::time::timeout(EVENT_TIMEOUT, audit_entered_rx) => {
                        entered.expect("resolved denial event timed out").unwrap();
                    }
                }
                let _ = audit_release_tx.send(());
                let res = tokio::time::timeout(EVENT_TIMEOUT, &mut second).await.expect("cached denial did not reply");
                let decision = tokio::time::timeout(EVENT_TIMEOUT, resolved_rx.recv())
                    .await
                    .expect("denial event timed out")
                    .expect("approval event listener closed");
                drop(_resolver);
                assert!(res.is_err(), "cache-hit recheck must catch newly registered deny listener: {res:?}");
                assert_eq!(decision, "deny", "cached veto must emit its resolved denial");
                h.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn net_ask_freezes_origin_session_and_rejects_after_session_switch() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let h = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                let lua = h.lua();
                let (hc, ho) = (h.kernel.host_ctx().unwrap(), h.kernel.host_owner().unwrap());

                lua.load(
                    r#"
                    VOTES = 0
                    RESOLVED = {}
                    "#,
                )
                .exec()
                .unwrap();
                h.apply_session_identity(Some("s1".into()), true).unwrap();

                let voter = lua
                    .create_function(|lua, ()| {
                        let n: i64 = lua.globals().get("VOTES")?;
                        lua.globals().set("VOTES", n + 1)?;
                        let t = lua.create_table()?;
                        t.set("decision", "allow")?;
                        t.set("grant", "allow_session")?;
                        Ok(t)
                    })
                    .unwrap();
                h.kernel.host_voter(&hc, ho, "net/approval", voter, mare_kernel::HostVoter::default()).unwrap();

                let on_res = lua
                    .create_function(|lua, ev: mlua::Table| {
                        let resolved: mlua::Table = lua.globals().get("RESOLVED")?;
                        let res_tbl: mlua::Table = ev.get("result")?;
                        let entry = lua.create_table()?;
                        entry.set("decision", res_tbl.get::<String>("decision")?)?;
                        entry.set("source", res_tbl.get::<String>("source")?)?;
                        resolved.set(resolved.raw_len() + 1, entry)?;
                        Ok(())
                    })
                    .unwrap();
                h.kernel.host_on(&hc, ho, "approval/resolved", on_res, mare_kernel::HostOn::default()).unwrap();

                let ap_s1 = mare_binding::netproxy::approver(lua).unwrap();

                // Switch to session s2 before s1's captured approver fires.
                h.apply_session_identity(Some("s2".into()), true).unwrap();

                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "frozen-s1.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;

                let stale_res = NetPolicy::deny_all().authorize("frozen-s1.test", 443, Some(&ap_s1)).await;
                assert!(
                    stale_res.is_err(),
                    "approver captured in s1 must be rejected after switch to s2: {stale_res:?}"
                );
                let votes_after_stale: i64 = lua.globals().get("VOTES").unwrap();
                assert_eq!(votes_after_stale, 0, "stale s1 NetAsk must not invoke s2 net/approval voter");

                let resolved_len: usize = lua.load("return #RESOLVED").eval().unwrap();
                assert!(resolved_len >= 1, "stale s1 NetAsk must emit approval/resolved");
                let (dec, src): (String, String) =
                    lua.load("return RESOLVED[1].decision, RESOLVED[1].source").eval().unwrap();
                assert_eq!((dec.as_str(), src.as_str()), ("deny", "cancelled"));

                // Fresh approver captured in s2 still works and wasn't poisoned by s1.
                let ap_s2 = mare_binding::netproxy::approver(lua).unwrap();
                let s2_res = NetPolicy::deny_all().authorize("frozen-s1.test", 443, Some(&ap_s2)).await;
                drop(_resolver);
                assert!(s2_res.is_ok(), "fresh s2 approver should succeed: {s2_res:?}");

                h.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn net_ask_inflight_when_session_switches_never_caches_or_allows() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let h = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                let lua = h.lua();
                let (hc, ho) = (h.kernel.host_ctx().unwrap(), h.kernel.host_owner().unwrap());

                h.apply_session_identity(Some("s1".into()), true).unwrap();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
                let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
                let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
                let voter = lua
                    .create_async_function(move |lua, ()| {
                        let entered_tx = entered_tx.clone();
                        let release_rx = release_rx.clone();
                        async move {
                            if let Some(tx) = entered_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release_rx.lock().await.take() {
                                let _ = rx.await;
                            }
                            let t = lua.create_table()?;
                            t.set("decision", "allow")?;
                            t.set("grant", "allow_session")?;
                            Ok(t)
                        }
                    })
                    .unwrap();
                h.kernel.host_voter(&hc, ho, "net/approval", voter, mare_kernel::HostVoter::default()).unwrap();

                let ap_s1 = mare_binding::netproxy::approver(lua).unwrap();
                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "inflight-switch.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;

                let req = tokio::task::spawn_local({
                    let ap = ap_s1.clone();
                    async move { NetPolicy::deny_all().authorize("inflight-switch.test", 443, Some(&ap)).await }
                });
                entered_rx.await.unwrap();

                // Switch session while the s1 NetAsk is paused in the approval gate.
                h.apply_session_identity(Some("s2".into()), true).unwrap();
                let _ = release_tx.send(());

                let res = req.await.unwrap();
                drop(_resolver);
                assert!(
                    res.is_err(),
                    "in-flight s1 NetAsk must be cancelled/denied when session switches mid-approval: {res:?}"
                );
                h.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn child_inherited_net_approver_rejects_after_parent_s1_s2_s1_switch_and_cancels_recheck() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let parent = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                let lua = parent.lua();
                let (hc, ho) = (parent.kernel.host_ctx().unwrap(), parent.kernel.host_owner().unwrap());

                parent.apply_session_identity(Some("s1".into()), true).unwrap();

                let scope = crate::agent_tree::Scope::root(&serde_json::json!({})).unwrap();
                let child = Host::new_child(
                    opts(tmp.path().to_path_buf(), Mode::Child),
                    None,
                    Default::default(),
                    scope,
                    approver(lua),
                    None,
                    "child-recheck".into(),
                )
                .await
                .unwrap();

                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
                let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
                let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let c_calls = calls.clone();
                let voter = lua
                    .create_async_function(move |lua, ()| {
                        let n = c_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let entered_tx = entered_tx.clone();
                        let release_rx = release_rx.clone();
                        async move {
                            // 1st+2nd calls populate s1 cache; 3rd call enters cached recheck_net_policy.
                            if n == 2 {
                                if let Some(tx) = entered_tx.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                if let Some(rx) = release_rx.lock().await.take() {
                                    let _ = rx.await;
                                }
                            }
                            let t = lua.create_table()?;
                            t.set("decision", "allow")?;
                            t.set("grant", "allow_session")?;
                            Ok(t)
                        }
                    })
                    .unwrap();
                parent.kernel.host_voter(&hc, ho, "net/approval", voter, mare_kernel::HostVoter::default()).unwrap();

                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "child-recheck.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;

                // Prime s1 cache while parent is on initial s1 epoch.
                let child_ap = approver(child.lua()).expect("child inherits parent approver");
                assert!(NetPolicy::deny_all().authorize("child-recheck.test", 443, Some(&child_ap)).await.is_ok());

                // Second call hits cache -> enters recheck_net_policy and blocks on voter without releasing.
                let blocking_recheck = tokio::task::spawn_local({
                    let ap = child_ap.clone();
                    async move { NetPolicy::deny_all().authorize("child-recheck.test", 443, Some(&ap)).await }
                });
                entered_rx.await.unwrap();

                // Switch s1 -> s2 -> s1 (new epoch on the same session_id) must immediately cancel the
                // blocked recheck_net_policy without waiting for the voter.
                parent.apply_session_identity(Some("s2".into()), true).unwrap();
                let ev2 = lua.create_table().unwrap();
                ev2.set("session_id", "s2").unwrap();
                parent.kernel.emit("session/changed", MultiValue::from_vec(vec![Value::Table(ev2)])).await;
                let recheck_res = tokio::time::timeout(std::time::Duration::from_secs(2), blocking_recheck)
                    .await
                    .expect("session switch must cancel recheck_net_policy immediately")
                    .unwrap();
                assert!(recheck_res.is_err(), "cancelled recheck must deny: {recheck_res:?}");
                let _ = release_tx.send(());

                // Even after reloading the same session id `s1`, epoch changed so child's old approver is rejected.
                parent.apply_session_identity(Some("s1".into()), true).unwrap();
                let ev1 = lua.create_table().unwrap();
                ev1.set("session_id", "s1").unwrap();
                parent.kernel.emit("session/changed", MultiValue::from_vec(vec![Value::Table(ev1)])).await;
                let stale_same_id = NetPolicy::deny_all().authorize("child-recheck.test", 443, Some(&child_ap)).await;
                assert!(
                    stale_same_id.is_err(),
                    "reloaded s1 with new epoch must still reject stale child NetApprover: {stale_same_id:?}"
                );

                drop(_resolver);
                child.shutdown().await;
                parent.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn recheck_net_policy_denies_when_cancelled_or_voter_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let tmp = TempDir::new();
                let host = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                host.revoke_session_only_trust().await.unwrap();
                host.revoke_session_only_trust_with_gate().await.unwrap();

                // Session-only trust revocation refuses while reload_active is set, and clears flags once inactive.
                host.workspace.set_trusted(true);
                host.workspace.set_session_only(true);
                host.reload_active.set(true);
                let err = host.revoke_session_only_trust().await.unwrap_err().to_string();
                assert!(err.contains("reload gate"), "{err}");
                host.reload_active.set(false);
                host.revoke_session_only_trust().await.unwrap();
                assert!(!host.workspace.is_session_only());
                assert!(!host.workspace.is_trusted());

                let cancelled = tokio_util::sync::CancellationToken::new();
                cancelled.cancel();
                let (mut tx1, _rx1) = tokio::sync::oneshot::channel();
                assert!(recheck_net_policy(&host, "example.com", 443, &mut tx1, &cancelled).await);

                let active = tokio_util::sync::CancellationToken::new();
                let (hc, ho) = (host.kernel.host_ctx().unwrap(), host.kernel.host_owner().unwrap());
                let err_voter = host
                    .lua()
                    .create_function(|_, _: mlua::MultiValue| -> mlua::Result<Value> {
                        Err(mlua::Error::runtime("voter boom"))
                    })
                    .unwrap();
                host.kernel.host_voter(&hc, ho, "net/approval", err_voter, mare_kernel::HostVoter::default()).unwrap();
                let (mut tx2, _rx2) = tokio::sync::oneshot::channel();
                assert!(recheck_net_policy(&host, "example.com", 443, &mut tx2, &active).await);

                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn allow_session_network_cache_invalidates_when_sandbox_policy_generation_changes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tmp = TempDir::new();
                let host = Host::new(opts(tmp.path().to_path_buf(), Mode::Print)).await.unwrap();
                let lua = host.lua();
                let (hc, ho) = (host.kernel.host_ctx().unwrap(), host.kernel.host_owner().unwrap());
                host.apply_session_identity(Some("s-net-gen".into()), true).unwrap();

                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let c_calls = calls.clone();
                let voter = lua
                    .create_function(move |lua, ()| {
                        c_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let t = lua.create_table()?;
                        t.set("decision", "allow")?;
                        t.set("grant", "allow_session")?;
                        Ok(t)
                    })
                    .unwrap();
                host.kernel.host_voter(&hc, ho, "net/approval", voter, mare_kernel::HostVoter::default()).unwrap();

                let ap = approver(lua).unwrap();
                let _resolver = ResolverGuard::acquire(std::collections::HashMap::from([(
                    "policy-gen.test".to_string(),
                    vec!["93.184.216.34".parse().unwrap()],
                )]))
                .await;

                // 1. Under workspace-write, grant `allow_session`; second call hits cache.
                lua.load("mare.proc.sandbox.set_current({ level = 'workspace-write' })").exec().unwrap();
                assert!(NetPolicy::deny_all().authorize("policy-gen.test", 443, Some(&ap)).await.is_ok());
                assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);

                // Replace voter with one that denies new prompts but abstains on rechecks? Even simpler:
                // track explicit approval/requested count via event log or remove voter and verify deny!
                // Wait: with a `deny` voter on the second prompt, a cache hit only runs `recheck_net_policy`
                // which would ALSO see `deny`, whereas counting `approval/requested` distinguishes cache hit
                // (0 new `approval/requested`) vs cache miss (1 new `approval/requested`).
                let mut cursor =
                    host.kernel.open_cursor(Arc::new(|name| (name == "approval/requested").then_some(true)));
                // Cache hit under same sandbox generation must NOT emit a new approval/requested.
                assert!(NetPolicy::deny_all().authorize("policy-gen.test", 443, Some(&ap)).await.is_ok());
                assert!(
                    cursor.try_recv().ok().flatten().is_none(),
                    "same sandbox generation must reuse allow_session cache"
                );

                // 2. Tightening sandbox policy bumps ossandbox generation -> cache must miss and re-prompt.
                lua.load("mare.proc.sandbox.set_current({ level = 'read-only' })").exec().unwrap();
                assert!(NetPolicy::deny_all().authorize("policy-gen.test", 443, Some(&ap)).await.is_ok());
                assert!(
                    cursor.try_recv().ok().flatten().is_some(),
                    "sandbox policy change must invalidate allow_session network cache and emit approval/requested"
                );

                drop(_resolver);
                host.shutdown().await;
            })
            .await;
    }
}

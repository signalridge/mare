//! Per-connection event stream pump, kernel connection listeners, and session-subscription updates.

use super::*;

/// Attach per-connection kernel event listeners (`ui/markdown-changed`, `fs/watch-event`,
/// `session/deleted`) onto one session's connection fiber. `fiber_uid` is this host VM's own
/// fiber uid (never shared across independent Host VMs).
pub(super) fn attach_connection_listeners_on(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Arc<Host>,
    ctx_id: u64,
    fiber_uid: Option<u64>,
) {
    // `ui/markdown-changed` is declared `expose = "process"` (carries internal `owner` fiber uid) so it bypasses taps;
    // deliver it only to the connection fiber that owns the document, projecting secrets on exit.
    let Ok(ctx) = super::tree_hooks::connection_table_on(host, ctx_id) else {
        return;
    };
    let md_conn = Rc::downgrade(conn);
    if let Ok(listener) = host.lua().create_function(move |lua, payload: mlua::Value| {
        let Some(conn) = md_conn.upgrade() else {
            return Ok(());
        };
        let Ok(ev_json) = mare_shared::lua::lua_to_json(lua, payload) else {
            return Ok(());
        };
        if fiber_uid.is_some_and(|uid| ev_json.get("owner").and_then(J::as_u64) == Some(uid))
            && let Some(presentation) = ev_json.get("presentation")
        {
            conn.notify(methods::MARKDOWN_CHANGED, presentation.clone());
        }
        Ok(())
    }) && let Err(error) =
        host.kernel.host_on(&ctx, ctx_id, "ui/markdown-changed", listener, mare_kernel::HostOn::default())
    {
        tracing::warn!("rpc markdown-changed listener: {error}");
    }
    let fs_conn = Rc::downgrade(conn);
    if let Ok(fs_listener) = host.lua().create_function(move |lua, payload: mlua::Value| {
        let Some(conn) = fs_conn.upgrade() else {
            return Ok(());
        };
        let Ok(ev_json) = mare_shared::lua::lua_to_json(lua, payload) else {
            return Ok(());
        };
        if fiber_uid.is_some_and(|uid| ev_json.get("owner").and_then(J::as_u64) == Some(uid))
            && let Some(watch_id) = ev_json.get("watch_id").and_then(J::as_str)
        {
            let changed_paths: Vec<String> = ev_json
                .get("changed_paths")
                .and_then(J::as_array)
                .map(|arr| arr.iter().filter_map(J::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let truncated = ev_json.get("truncated").and_then(J::as_bool).unwrap_or(false);
            let note = proto::FsChangedNotification { watch_id: watch_id.to_string(), changed_paths, truncated };
            conn.notify(methods::FS_CHANGED, serde_json::to_value(note).unwrap_or(J::Null));
        }
        Ok(())
    }) && let Err(error) =
        host.kernel.host_on(&ctx, ctx_id, "fs/watch-event", fs_listener, mare_kernel::HostOn::default())
    {
        tracing::warn!("rpc fs-changed listener: {error}");
    }
    let del_host = Arc::downgrade(host);
    let del_conn = Rc::downgrade(conn);
    let sid_fallback = session_id.to_owned();
    if let Ok(on_deleted) = host.lua().create_function(move |lua, payload: mlua::Value| {
        let Ok(ev) = mare_shared::lua::lua_to_json(lua, payload) else {
            return Ok(());
        };
        let sid = ev.get("session_id").and_then(J::as_str).unwrap_or(sid_fallback.as_str());
        if let Some(host) = del_host.upgrade() {
            host.prompt_receipts.remove_session(sid);
            if let Some(orch) = host.orchestrator() {
                orch.remove_receipts_for_session(sid);
            }
            if let Some(conn) = del_conn.upgrade()
                && conn.subscriptions.borrow_mut().remove(sid).is_some()
            {
                host.kernel.taps_changed();
            }
        }
        Ok(())
    }) && let Err(error) =
        host.kernel.host_on(&ctx, ctx_id, "session/deleted", on_deleted, mare_kernel::HostOn::default())
    {
        tracing::warn!("rpc session/deleted listener: {error}");
    }
}

fn forward_record(ev_conn: &Rc<Conn>, rec: mare_kernel::CursorRecord) -> bool {
    match rec {
        mare_kernel::CursorRecord::Lagged { .. } => {
            // Subscription lag must be handled together with its new baseline below.
            ev_conn.fail_stream();
            false
        }
        mare_kernel::CursorRecord::Dropped { count, session_id, session_epoch } => {
            ev_conn.notify(
                methods::DROPPED,
                serde_json::to_value(proto::headless::Dropped {
                    count,
                    session_id,
                    session_epoch: Some(session_epoch),
                })
                .unwrap_or(J::Null),
            );
            true
        }
        mare_kernel::CursorRecord::Event { event: ev, selected } => {
            if (selected || ev.name == proto::event_registry::EVENT_STATUS_CHANGED)
                && let Err(error) = proto::events::validate_protocol(&ev.name, &ev.payload)
            {
                tracing::warn!("rpc: invalid built-in event {}: {error}", ev.name);
                ev_conn.fail_stream();
                return false;
            }
            if selected {
                ev_conn.send_raw_event(&ev.wire_json);
            }
            true
        }
    }
}

pub(super) fn sync_session_tracking(
    conn: &Rc<Conn>,
    tracking: &mut HashMap<String, (Arc<Host>, mare_kernel::EventCursor)>,
) -> Vec<Rc<SessionRuntime>> {
    let active = conn.rpc.active_sessions();
    let active_hosts: HashMap<&str, &Arc<Host>> = active.iter().map(|r| (r.session_id.as_str(), &r.host)).collect();
    tracking.retain(|sid, (host, _)| active_hosts.get(sid.as_str()).is_some_and(|cur| Arc::ptr_eq(host, cur)));
    for rt in &active {
        if !tracking.contains_key(&rt.session_id) {
            let cur = rt.host.kernel.open_cursor_with_wake(
                Arc::new(|name| (name == proto::event_registry::EVENT_STATUS_CHANGED).then_some(false)),
                Some(conn.sub_wake.clone()),
            );
            tracking.insert(rt.session_id.clone(), (rt.host.clone(), cur));
        }
    }
    let mut failures = conn.tracking_failures.borrow_mut();
    *failures = tracking.iter().map(|(sid, (_, cur))| (sid.clone(), cur.failure())).collect();
    active
}

fn slot_key_for_host(subs: &HashMap<String, SubscriptionSlot>, sid: &str, host: &Arc<Host>) -> Option<String> {
    if subs.get(sid).is_some_and(|slot| slot.host.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host))) {
        return Some(sid.to_owned());
    }
    subs.iter().find_map(|(key, slot)| slot.host.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)).then(|| key.clone()))
}

fn existing_slot_key_for_host(conn: &Conn, sid: &str, host: &Arc<Host>) -> Option<String> {
    slot_key_for_host(&conn.subscriptions.borrow(), sid, host)
}

fn activate_new_default_subs(conn: &Rc<Conn>, new_subs: Vec<(String, SubscriptionSlot, Arc<Host>)>) {
    if new_subs.is_empty() {
        return;
    }
    let mut hosts = Vec::new();
    {
        let mut subs = conn.subscriptions.borrow_mut();
        for (sid, slot, h) in new_subs {
            if subs.contains_key(&sid) || slot_key_for_host(&subs, &sid, &h).is_some() {
                continue;
            }
            subs.insert(sid, slot);
            hosts.push(h);
        }
    }
    for h in hosts {
        h.kernel.taps_changed();
    }
}

fn reconcile_session_cursors_initial(
    conn: &Rc<Conn>,
    tracking: &mut HashMap<String, (Arc<Host>, mare_kernel::EventCursor)>,
) {
    let active = sync_session_tracking(conn, tracking);
    let mut new_subs: Vec<(String, SubscriptionSlot, Arc<Host>)> = Vec::new();
    for rt in &active {
        if conn.default_subscribe_all.get()
            && access::authorize(conn, &rt.host, access::Op::EventsStream).is_allowed()
            && existing_slot_key_for_host(conn, &rt.session_id, &rt.host).is_none()
            && !conn.suppressed_subscriptions.borrow().contains(&rt.session_id)
            && !conn.deferred_subscriptions.borrow().contains_key(&rt.session_id)
            && let Some(ctx_id) = conn.ctx_id_for_host(&rt.host)
        {
            let filter = conn.default_filter.borrow().clone();
            if let Ok((_, _, _, PendingActivation::Cursor { session_id, slot, .. })) =
                conn.open_subscription_on(&rt.session_id, &rt.host, ctx_id, None, None, true, filter, false, false)
            {
                new_subs.push((session_id, *slot, rt.host.clone()));
            }
        }
    }
    activate_new_default_subs(conn, new_subs);
}

async fn reconcile_session_cursors(
    conn: &Rc<Conn>,
    tracking: &mut HashMap<String, (Arc<Host>, mare_kernel::EventCursor)>,
) {
    let active = sync_session_tracking(conn, tracking);
    let mut new_subs: Vec<(String, SubscriptionSlot, Arc<Host>)> = Vec::new();
    for rt in &active {
        if conn.default_subscribe_all.get()
            && access::authorize(conn, &rt.host, access::Op::EventsStream).is_allowed()
            && existing_slot_key_for_host(conn, &rt.session_id, &rt.host).is_none()
            && !conn.suppressed_subscriptions.borrow().contains(&rt.session_id)
            && !conn.deferred_subscriptions.borrow().contains_key(&rt.session_id)
            && let Ok(ctx_id) = conn.ensure_session_ctx(&rt.session_id, &rt.host).await
            && !conn.deferred_subscriptions.borrow().contains_key(&rt.session_id)
        {
            let filter = conn.default_filter.borrow().clone();
            if let Ok((_, _, _, PendingActivation::Cursor { session_id, slot, .. })) =
                conn.open_subscription_on(&rt.session_id, &rt.host, ctx_id, None, None, true, filter, false, false)
            {
                new_subs.push((session_id, *slot, rt.host.clone()));
            }
        }
    }
    activate_new_default_subs(conn, new_subs);
}

fn drain_all(ev_conn: &Rc<Conn>, tracking: &mut HashMap<String, (Arc<Host>, mare_kernel::EventCursor)>) -> bool {
    if ev_conn.has_tap_failure() {
        ev_conn.fail_stream();
        return false;
    }
    for (_, tracking_cursor) in tracking.values_mut() {
        loop {
            match tracking_cursor.try_recv() {
                Ok(Some(mare_kernel::CursorRecord::Lagged { .. })) => {}
                Ok(Some(rec)) => {
                    if !forward_record(ev_conn, rec) {
                        return false;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    ev_conn.fail_stream();
                    return false;
                }
            }
        }
    }
    // Snapshot the buffered records out of each subscription without holding `RefCell` across `send`.
    // Stop draining a slot immediately upon `Lagged` so we re-cut a baseline before reading further.
    struct DrainedRecord {
        host: SyncWeak<Host>,
        ctx_id: u64,
        cursor_id: u64,
        filter: Arc<std::sync::RwLock<EventFilter>>,
        record: mare_kernel::CursorRecord,
    }
    let drained = {
        let mut subs = ev_conn.subscriptions.borrow_mut();
        let mut batch = Vec::new();
        let mut failed = false;
        for slot in subs.values_mut() {
            if slot.failure.borrow().is_some() {
                failed = true;
                break;
            }
            let cursor_id = slot.cursor.id();
            loop {
                match slot.cursor.try_recv() {
                    Ok(Some(rec)) => {
                        let is_lagged = matches!(rec, mare_kernel::CursorRecord::Lagged { .. });
                        batch.push(DrainedRecord {
                            host: slot.host.clone(),
                            ctx_id: slot.ctx_id,
                            cursor_id,
                            filter: Arc::clone(&slot.filter),
                            record: rec,
                        });
                        if is_lagged {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                break;
            }
        }
        (batch, failed)
    };
    if drained.1 {
        ev_conn.fail_stream();
        return false;
    }
    let mut need_rewake = false;
    for item in drained.0 {
        match item.record {
            mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => {
                let Some(slot_host) = item.host.upgrade() else { continue };
                let retain_filter =
                    |name: &str| event_selected_for_rpc(&slot_host, ev_conn.is_owner(), &item.filter, name);
                let (extra, include_state, page_access) = ev_conn.baseline_access(&slot_host);
                match slot_host.kernel.reanchor_with_baseline(
                    item.ctx_id,
                    item.cursor_id,
                    &retain_filter,
                    extra,
                    include_state,
                    page_access,
                ) {
                    Ok(Some(snap)) => {
                        ev_conn.notify(
                            methods::LAGGED,
                            serde_json::to_value(proto::headless::Lagged {
                                from_seq,
                                to_seq,
                                session_id: snap.session_id.clone(),
                                session_epoch: Some(snap.session_epoch.clone()),
                            })
                            .unwrap_or(J::Null),
                        );
                        ev_conn.send_snapshot_on(&slot_host, item.ctx_id, snap);
                        need_rewake = true;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("rpc: baseline re-cut after lag failed: {e}");
                        ev_conn.fail_stream();
                        return false;
                    }
                }
            }
            other => {
                if let mare_kernel::CursorRecord::Event { ref event, selected: true } = other
                    && !item
                        .host
                        .upgrade()
                        .is_some_and(|h| access::authorize(ev_conn, &h, access::Op::Event(&event.name)).is_allowed())
                {
                    continue;
                }
                if !forward_record(ev_conn, other) {
                    return false;
                }
            }
        }
    }
    if need_rewake {
        ev_conn.sub_wake.notify_one();
    }
    true
}

/// Spawn the background local task that drains validation and subscribed event cursors across
/// every active `SessionRuntime`.
pub(super) fn spawn_event_pump(
    conn: &Rc<Conn>,
    _host: &Arc<Host>,
    mut event_barriers: mpsc::Receiver<oneshot::Sender<()>>,
) -> tokio::task::JoinHandle<()> {
    let mut tracking: HashMap<String, (Arc<Host>, mare_kernel::EventCursor)> = HashMap::new();
    reconcile_session_cursors_initial(conn, &mut tracking);

    let ev_conn = conn.clone();
    let sub_wake = conn.sub_wake.clone();
    let mut wake_rx = conn.rpc.event_wake.subscribe();
    tokio::task::spawn_local(async move {
        reconcile_session_cursors(&ev_conn, &mut tracking).await;
        loop {
            let notified = sub_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !drain_all(&ev_conn, &mut tracking) {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut notified => {}
                res = wake_rx.changed() => {
                    if res.is_err() {
                        break;
                    }
                    let _ = wake_rx.borrow_and_update();
                    reconcile_session_cursors(&ev_conn, &mut tracking).await;
                }
                barrier = event_barriers.recv() => {
                    reconcile_session_cursors(&ev_conn, &mut tracking).await;
                    if !drain_all(&ev_conn, &mut tracking) {
                        return;
                    }
                    match barrier {
                        Some(tx) => {
                            let _ = tx.send(());
                        }
                        None => return,
                    }
                }
            }
        }
    })
}

pub(super) async fn handle_events_subscribe(
    conn: &Rc<Conn>,
    ep: proto::EventsSubscribeParams,
    activations: &mut Vec<PendingActivation>,
) -> Result<proto::EventsSubscribeResult, proto::RpcError> {
    let scope = validate_registration_scope(conn, ep.scope, ep.session_id.as_deref(), true).await?;
    let anchor = conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone());
    let perm_host = match &scope {
        ResolvedRegistrationScope::Session(sid) => {
            let (rt, _) = resolve_session_target(conn, sid, None).await?;
            rt.host.clone()
        }
        ResolvedRegistrationScope::All => anchor.clone(),
    };
    access::authorize(
        conn,
        &perm_host,
        access::Op::EventsSubscribe { include: ep.include.as_deref(), exclude: ep.exclude.as_deref().unwrap_or(&[]) },
    )
    .into_error(methods::EVENTS_SUBSCRIBE)?;
    let new_filter = EventFilter { include: ep.include.clone(), exclude: ep.exclude.clone().unwrap_or_default() };
    let filter_change = ep.include.is_some() || ep.exclude.is_some();
    let filter_only = ep.session_epoch.is_none() && ep.last_seq.is_none();

    match scope {
        ResolvedRegistrationScope::Session(requested) => {
            let (rt, identity) = resolve_session_target(conn, &requested, None).await?;
            let target_sid = &identity.id;
            let ctx_id = conn.ensure_session_ctx(target_sid, &rt.host).await?;
            if filter_only && filter_change && conn.subscriptions.borrow().contains_key(target_sid) {
                let (cur_epoch, cur_tail) = rt.host.kernel.event_log().session_tail();
                activations.push(PendingActivation::Filter {
                    session_ids: vec![target_sid.clone()],
                    filter: new_filter,
                    default_subscribe_all: Some(false),
                    narrow_to_session: Some(target_sid.clone()),
                });
                return Ok(proto::EventsSubscribeResult {
                    mode: proto::SubscribeMode::Live,
                    session_epoch: cur_epoch,
                    seq: cur_tail + 1,
                });
            }
            let (mode, session_epoch, seq, activation) = conn.open_subscription_on(
                target_sid,
                &rt.host,
                ctx_id,
                ep.session_epoch.as_deref(),
                ep.last_seq,
                false,
                new_filter,
                filter_only,
                true,
            )?;
            activations.push(activation);
            Ok(proto::EventsSubscribeResult { mode, session_epoch, seq })
        }
        ResolvedRegistrationScope::All => {
            if !filter_only {
                return Err(proto::RpcError::core(
                    proto::ErrorCode::InvalidArguments,
                    "session_epoch and last_seq require an explicit session_id",
                ));
            }
            let sessions = conn.rpc.active_sessions();
            let preferred_sid =
                conn.rpc.anchor_session_id.borrow().clone().or_else(|| sessions.first().map(|f| f.session_id.clone()));
            let mut reply: Option<proto::EventsSubscribeResult> = None;
            let mut filter_ids = Vec::new();
            for rt in &sessions {
                let is_preferred = preferred_sid.as_deref() == Some(rt.session_id.as_str());
                if let Some(existing_key) = existing_slot_key_for_host(conn, &rt.session_id, &rt.host) {
                    if !filter_ids.contains(&existing_key) {
                        filter_ids.push(existing_key);
                    }
                    if reply.is_none() && is_preferred {
                        let (cur_epoch, cur_tail) = rt.host.kernel.event_log().session_tail();
                        reply = Some(proto::EventsSubscribeResult {
                            mode: proto::SubscribeMode::Live,
                            session_epoch: cur_epoch,
                            seq: cur_tail + 1,
                        });
                    }
                } else {
                    let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await?;
                    let (mode, session_epoch, seq, activation) = conn.open_subscription_on(
                        &rt.session_id,
                        &rt.host,
                        ctx_id,
                        None,
                        None,
                        true,
                        new_filter.clone(),
                        true,
                        false,
                    )?;
                    activations.push(activation);
                    if reply.is_none() && is_preferred {
                        reply = Some(proto::EventsSubscribeResult { mode, session_epoch, seq });
                    }
                }
            }
            if reply.is_none()
                && let Some(rt) = sessions.first()
            {
                let (cur_epoch, cur_tail) = rt.host.kernel.event_log().session_tail();
                reply = Some(proto::EventsSubscribeResult {
                    mode: proto::SubscribeMode::Live,
                    session_epoch: cur_epoch,
                    seq: cur_tail + 1,
                });
            }
            if !filter_ids.is_empty() {
                activations.push(PendingActivation::Filter {
                    session_ids: filter_ids,
                    filter: new_filter,
                    default_subscribe_all: Some(true),
                    narrow_to_session: None,
                });
            } else {
                *conn.default_filter.borrow_mut() = new_filter;
                conn.default_subscribe_all.set(true);
                conn.explicit_sessions.borrow_mut().clear();
            }
            if let Some(r) = reply {
                return Ok(r);
            }
            let (_, cur_epoch) = anchor.kernel.event_log().session_identity();
            Ok(proto::EventsSubscribeResult { mode: proto::SubscribeMode::Live, session_epoch: cur_epoch, seq: 1 })
        }
    }
}

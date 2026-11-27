//! Approvals answered by the host: policy listeners that forward to `approval.request`.

use super::*;

/// `capabilities.approvals` as (re-)`initialize` sets it: the forwarding voters are registered
/// when it turns on and released when it turns off; an unchanged value keeps what is there.
/// Turning it off also ends the approvals the host is being asked right now: each ends
/// unanswered, a deny.
pub(super) fn set_approvals_on(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Arc<Host>,
    ctx_id: u64,
    on: bool,
) -> mlua::Result<()> {
    if on == host.answerer.has_approval_voters(conn.id) {
        return Ok(());
    }
    if on {
        return subscribe_approvals_on(conn, session_id, host, ctx_id);
    }
    for release in host.answerer.take_approval_voters(conn.id) {
        if let Err(e) = release.call::<()>(()) {
            tracing::warn!("approval voter release: {e}");
        }
    }
    conn.cancel_pending_for_session(Some(methods::APPROVAL_REQUEST), session_id, "approvals capability withdrawn");
    Ok(())
}

/// The host answers approvals (`capabilities.approvals`, primary connection): a `tool/approval-host`
/// (fallback after policy/hook `tool/approval` listeners, and required for human-only approvals)
/// and a `net/approval` policy listener on the connection ctx forward to `approval.request`.
/// Fail-closed: no answer / invalid answer / disconnect = deny.
fn subscribe_approvals_on(conn: &Rc<Conn>, session_id: &str, host: &Arc<Host>, ctx_id: u64) -> mlua::Result<()> {
    let lua = host.lua();
    // voters of the connection's fiber, withdrawn with it; they wait for a person
    let ctx = super::tree_hooks::connection_table_on(host, ctx_id)?;
    for (event, kind) in [("tool/approval-host", "tool"), ("net/approval-host", "network")] {
        let weak = Rc::downgrade(conn);
        let wh = Arc::downgrade(host);
        let sid = session_id.to_owned();
        let f = lua.create_async_function(move |lua, args: MultiValue| {
            let weak = weak.clone();
            let wh = wh.clone();
            let sid = sid.clone();
            async move {
                // the host's answer (five approval decisions) as a policy vote:
                // allow-class → {decision = "allow", grant}, abort → {decision = "deny", abort}
                let vote = |d: &str, reason: Option<String>, amendment: Option<J>| -> mlua::Result<Value> {
                    let t = lua.create_table()?;
                    match d {
                        "allow" | "allow_session" | "allow_amend" => {
                            t.set("decision", "allow")?;
                            t.set("grant", d)?;
                            if let Some(a) = amendment.filter(|a| a.is_object()) {
                                t.set("amendment", json_to_lua(&lua, &a)?)?;
                            }
                        }
                        "abort" => {
                            t.set("decision", "deny")?;
                            t.set("abort", true)?;
                        }
                        _ => t.set("decision", "deny")?,
                    }
                    t.set("source", "user")?;
                    t.set("reason", reason)?;
                    Ok(Value::Table(t))
                };
                // nobody can answer: `unavailable`
                let unavailable = |reason: &str| -> mlua::Result<Value> {
                    let t = lua.create_table()?;
                    t.set("decision", "deny")?;
                    t.set("unavailable", true)?;
                    t.set("source", "none")?;
                    t.set("reason", reason)?;
                    Ok(Value::Table(t))
                };
                let (Some(conn), Some(host)) = (weak.upgrade(), wh.upgrade()) else {
                    return unavailable("RPC host disconnected");
                };
                let identity = host.kernel.event_log().session_identity();
                let sid = identity.0.clone().filter(|s| !s.is_empty()).unwrap_or(sid);
                // a decision that took its voters before the host withdrew the capability or revoked
                // remote_admin (as mcp.rs checks its own at call time)
                if !conn.is_answerer_on(&host)
                    || !conn.caps.borrow().approvals
                    || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
                {
                    return unavailable("RPC host withdrew capabilities.approvals");
                }
                let cap_rev = conn.cap_revision.get();
                let args = mare_kernel::strip_event_envelope(args);
                if kind != "tool" && args.len() > 1 {
                    return vote("deny", Some(format!("{kind} approval accepts at most 1 arguments")), None);
                }
                let event = format!("{kind}/approval");
                let projected = match mare_kernel::project_event_args(&lua, &event, args) {
                    Ok(projected) => projected,
                    Err(e) => {
                        return vote("deny", Some(format!("approval argument conversion failed: {e}")), None);
                    }
                };
                let cancel = projected.cancel_token;
                let mut a = projected.json_args;
                a.resize(3, J::Null);
                let s = |v: &J, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
                let approval_id = a[0]
                    .get("approval_binding")
                    .and_then(|binding| binding.get("request"))
                    .and_then(|id| id.as_str())
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let turn_id = match s(&a[0], "turn_id").filter(|id| !id.is_empty()) {
                    Some(id) => Some(id),
                    None => {
                        let state = if let Some(c) = cancel.as_ref() {
                            tokio::select! {
                                biased;
                                _ = c.cancelled() => None,
                                s = host.state() => Some(s),
                            }
                        } else {
                            Some(host.state().await)
                        };
                        state
                            .as_ref()
                            .and_then(|st| st.get("turn_id"))
                            .and_then(J::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                    }
                };
                let params = if kind == "tool" {
                    // (call, info, verdict)
                    let (call, info, verdict) = (&a[0], &a[1], &a[2]);
                    let args = call.get("arguments").cloned().unwrap_or(J::Null);
                    let profile = verdict
                        .get("requested_permissions")
                        .filter(|v| !v.is_null())
                        .or_else(|| args.get("additional_permissions"));
                    let requested_permissions = match profile {
                        Some(value) => match serde_json::from_value::<proto::PermissionProfile>(value.clone()) {
                            Ok(profile) => Some(profile),
                            Err(e) => return vote("deny", Some(format!("invalid permission request: {e}")), None),
                        },
                        None => None,
                    };
                    let scope = if requested_permissions.is_some() {
                        match s(verdict, "permission_scope").or_else(|| s(&args, "scope")) {
                            Some(scope) => {
                                match serde_json::from_value::<proto::PermissionGrantScope>(J::String(scope)) {
                                    Ok(scope) => Some(scope),
                                    Err(e) => {
                                        return vote("deny", Some(format!("invalid permission scope: {e}")), None);
                                    }
                                }
                            }
                            None => None,
                        }
                    } else {
                        None
                    };
                    proto::ApprovalRequestParams {
                        session_id: sid.clone(),
                        turn_id,
                        kind: kind.into(),
                        approval_id,
                        tool_call_id: s(call, "id"),
                        tool: s(call, "name"),
                        command: s(&args, "command"),
                        diff: s(&args, "patch").or_else(|| s(&args, "input")).or_else(|| s(&args, "diff")),
                        requested_permissions,
                        scope,
                        arguments: args,
                        cwd: s(call, "cwd").unwrap_or_else(|| host.cwd.to_string_lossy().into()),
                        reason: s(verdict, "reason"),
                        side_effect: s(info, "side_effect"),
                        unconfined: info.get("unconfined").and_then(|v| v.as_bool()).unwrap_or(false),
                        proposed_amendment: verdict.get("amendment").cloned().filter(|v| !v.is_null()),
                        host: None,
                    }
                } else {
                    // ({host, port, reason?})
                    let req = &a[0];
                    let peer_host = match (s(req, "host"), req.get("port").and_then(|p| p.as_u64())) {
                        (Some(h), Some(p)) => Some(format!("{h}:{p}")),
                        (h, _) => h,
                    };
                    proto::ApprovalRequestParams {
                        session_id: sid.clone(),
                        turn_id,
                        kind: kind.into(),
                        approval_id,
                        host: peer_host,
                        cwd: host.cwd.to_string_lossy().into(),
                        reason: s(req, "reason"),
                        ..Default::default()
                    }
                };
                let params_json = match serde_json::to_value(params) {
                    Ok(v) => v,
                    Err(e) => {
                        return vote("deny", Some(format!("approval params serialization failed: {e}")), None);
                    }
                };
                // Drain already-queued kernel tap events (notably `approval/requested`) onto the
                // wire before emitting the `approval.request` callback so clients observe the
                // lifecycle event before the interactive prompt.
                let flushed = if let Some(c) = cancel.as_ref() {
                    tokio::select! {
                        biased;
                        _ = c.cancelled() => false,
                        ok = conn.flush_events() => ok,
                    }
                } else {
                    conn.flush_events().await
                };
                #[cfg(test)]
                tests::wait_flush_gate().await;
                if !flushed || cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return unavailable("RPC host: no answer to approval.request");
                }
                if host.kernel.event_log().session_identity() != identity {
                    return unavailable("session changed before approval.request");
                }
                if conn.closed.get()
                    || conn.cap_revision.get() != cap_rev
                    || !conn.is_answerer_on(&host)
                    || !conn.caps.borrow().approvals
                    || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
                {
                    return unavailable("RPC host withdrew capabilities.approvals");
                }
                let request = conn.call_with_timeout_for_session(
                    methods::APPROVAL_REQUEST,
                    &sid,
                    params_json,
                    conn.approval_timeout.get(),
                );
                let result = if let Some(cancel) = cancel {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        value = request => if cancel.is_cancelled() { None } else { value },
                    }
                } else {
                    request.await
                };
                if host.kernel.event_log().session_identity() != identity {
                    return unavailable("session changed during approval.request");
                }
                if conn.closed.get()
                    || conn.cap_revision.get() != cap_rev
                    || !conn.is_answerer_on(&host)
                    || !conn.caps.borrow().approvals
                    || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
                {
                    return unavailable("RPC host withdrew capabilities.approvals");
                }
                let Some(v) = result else {
                    return unavailable("RPC host: no answer to approval.request");
                };
                match serde_json::from_value::<proto::ApprovalRequestResult>(v) {
                    Ok(r) => {
                        let decision = r.decision.as_str().to_string();
                        let result = vote(&decision, r.reason, r.amendment)?;
                        if let Value::Table(ref t) = result {
                            t.set("source", "host")?;
                            if matches!(decision.as_str(), "allow" | "allow_session" | "allow_amend")
                                && let Some(binding) =
                                    a[0].get("approval_binding").and_then(mare_binding::approval::Binding::from_json)
                            {
                                let arguments = if kind == "tool" {
                                    a[0].get("arguments").cloned().unwrap_or(J::Null)
                                } else {
                                    J::Null
                                };
                                let token = mare_binding::approval::issue(&lua, binding, arguments, &decision);
                                t.set("human_approval", token)?;
                            }
                            if let Some(permissions) = r.permissions {
                                t.set(
                                    "permissions",
                                    json_to_lua(
                                        &lua,
                                        &serde_json::to_value(permissions).map_err(mlua::Error::external)?,
                                    )?,
                                )?;
                            }
                            if let Some(scope) = r.scope {
                                t.set("scope", scope.as_str())?;
                            }
                        }
                        Ok(result)
                    }
                    Err(e) => vote("deny", Some(format!("RPC host: invalid approval answer: {e}")), None),
                }
            }
        })?;
        let opts = mare_kernel::HostVoter { timeout: Some(std::time::Duration::ZERO), ..Default::default() };
        let release = host.kernel.host_voter(&ctx, ctx_id, event, f, opts)?;
        host.answerer.push_approval_voter(conn.id, release);
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::sync::oneshot;

    std::thread_local! {
        static FLUSH_GATE: std::cell::RefCell<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) async fn wait_flush_gate() {
        let pair = FLUSH_GATE.with(|cell| cell.borrow_mut().take());
        if let Some((entered, release)) = pair {
            let _ = entered.send(());
            let _ = release.await;
        }
    }

    pub(in crate::rpc) fn arm_flush_gate() -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        FLUSH_GATE.with(|cell| {
            *cell.borrow_mut() = Some((entered_tx, release_rx));
        });
        (entered_rx, release_tx)
    }

    pub(in crate::rpc) fn clear_flush_gate() {
        FLUSH_GATE.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

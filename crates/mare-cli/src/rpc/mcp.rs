//! RPC `mcp.elicitation` callback forwarding.

use super::*;

/// Forward `mcp/elicit` bail events to the host's `mcp.elicitation` callback when
/// `capabilities.elicitation` is enabled on the active answerer connection.
pub(super) fn set_elicitation_on(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Arc<Host>,
    ctx_id: u64,
    on: bool,
) -> mlua::Result<()> {
    if on == host.answerer.has_elicitation(conn.id) {
        return Ok(());
    }
    if !on {
        if let Some(release) = host.answerer.take_elicitation_release(conn.id)
            && let Err(e) = release.call::<()>(())
        {
            tracing::warn!("elicitation listener release: {e}");
        }
        conn.cancel_pending_for_session(Some(methods::MCP_ELICITATION), session_id, "elicitation capability withdrawn");
        return Ok(());
    }
    let lua = host.lua();
    let weak = Rc::downgrade(conn);
    let wh = Arc::downgrade(host);
    let sid = session_id.to_owned();
    let f = lua.create_async_function(move |lua, args: MultiValue| {
        let weak = weak.clone();
        let wh = wh.clone();
        let sid = sid.clone();
        async move {
            let (Some(conn), Some(host)) = (weak.upgrade(), wh.upgrade()) else {
                return Ok(Value::Nil);
            };
            if !conn.is_answerer_on(&host)
                || !conn.caps.borrow().elicitation
                || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
            {
                return Ok(Value::Nil);
            }
            let cap_rev = conn.cap_revision.get();
            let payload = args.front().cloned().map(|v| lua_to_json(&lua, v)).transpose()?.unwrap_or(J::Null);
            let cancel = args.get(1).and_then(|v| match v {
                Value::UserData(ud) => ud.borrow::<mare_binding::task::Token>().ok().map(|t| t.0.clone()),
                _ => None,
            });
            let p = payload.get("params").cloned().unwrap_or(J::Null);
            let s = |k: &str| p.get(k).and_then(J::as_str).map(str::to_string);
            let identity = host.kernel.event_log().session_identity();
            let sid = identity.0.clone().filter(|s| !s.is_empty()).unwrap_or(sid);
            let turn_id = {
                let state = if let Some(c) = cancel.as_ref() {
                    tokio::select! {
                        biased;
                        _ = c.cancelled() => None,
                        st = host.state() => Some(st),
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
            };
            if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                return json_to_lua(&lua, &json!({ "action": "cancel" }));
            }
            if host.kernel.event_log().session_identity() != identity
                || conn.closed.get()
                || conn.cap_revision.get() != cap_rev
                || !conn.is_answerer_on(&host)
                || !conn.caps.borrow().elicitation
                || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
            {
                return Ok(Value::Nil);
            }
            let req = proto::McpElicitationParams {
                session_id: sid.clone(),
                turn_id,
                server: payload.get("server").and_then(J::as_str).unwrap_or_default().to_string(),
                request_id: payload.get("id").cloned().filter(|v| !v.is_null()),
                mode: s("mode"),
                message: s("message").unwrap_or_default(),
                requested_schema: p.get("requestedSchema").cloned().filter(|v| !v.is_null()),
                url: s("url"),
                elicitation_id: s("elicitationId"),
                meta: p.get("_meta").cloned().filter(|v| !v.is_null()),
            };
            let request = conn.call_with_timeout_for_session(
                methods::MCP_ELICITATION,
                &sid,
                serde_json::to_value(req).unwrap_or(J::Null),
                conn.approval_timeout.get(),
            );
            let (cancelled, result) = if let Some(cancel) = cancel {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => (true, None),
                    value = request => if cancel.is_cancelled() { (true, None) } else { (false, value) },
                }
            } else {
                (false, request.await)
            };
            let answer = if cancelled {
                json!({ "action": "cancel" })
            } else if conn.closed.get()
                || conn.cap_revision.get() != cap_rev
                || !conn.is_answerer_on(&host)
                || !conn.caps.borrow().elicitation
                || !access::authorize(&conn, &host, access::Op::Answerer).is_allowed()
            {
                json!({ "action": "decline" })
            } else if let Some(v) = result {
                match serde_json::from_value::<proto::McpElicitationResult>(v) {
                    Ok(r) => {
                        let mut out = json!({ "action": r.action.as_str() });
                        if let Some(content) = r.content
                            && let Some(obj) = out.as_object_mut()
                        {
                            obj.insert("content".into(), content);
                        }
                        out
                    }
                    Err(e) => {
                        tracing::debug!("rpc callback {} returned invalid result: {e}", methods::MCP_ELICITATION);
                        json!({ "action": "decline" })
                    }
                }
            } else {
                json!({ "action": "decline" })
            };
            json_to_lua(&lua, &answer)
        }
    })?;
    // a listener of the connection's fiber, withdrawn with it or on answerer revoke
    let ctx = super::tree_hooks::connection_table_on(host, ctx_id)?;
    let release = host.kernel.host_on(&ctx, ctx_id, "mcp/elicit", f, mare_kernel::HostOn::default())?;
    host.answerer.set_elicitation_release(conn.id, release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::rpc_harness::{HOST_LOCK, TestRpc, test_host};
    use super::*;

    #[tokio::test]
    async fn peer_connection_rejects_mcp_management_methods_and_elicitation_capability() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = HOST_LOCK.lock().await;
                let dir = tempfile::tempdir().unwrap();
                let host = test_host(dir.path()).await;
                let mut rpc = TestRpc::connect_peer(host.clone(), None);

                let init = rpc
                    .call_id(
                        1,
                        "initialize",
                        json!({ "protocol_version": "3.0", "capabilities": { "elicitation": true } }),
                    )
                    .await;
                assert_eq!(init["result"]["capabilities"]["callbacks"]["elicitation"], false, "{init}");

                for (id, method) in [
                    (2, "mcp.list_servers"),
                    (3, "mcp.list_tools"),
                    (4, "mcp.connect"),
                    (5, "mcp.reconnect"),
                    (6, "mcp.list_resources"),
                    (7, "mcp.read_resource"),
                    (8, "mcp.list_prompts"),
                    (9, "mcp.get_prompt"),
                ] {
                    let resp = rpc.call_id(id, method, json!({})).await;
                    assert_eq!(resp["error"]["data"]["code"], "denied", "{method}: {resp}");
                }
                for (id, legacy) in [
                    (10, "mcp.servers"),
                    (11, "mcp.list"),
                    (12, "mcp.resources"),
                    (13, "mcp.read"),
                    (14, "mcp.prompts"),
                    (15, "mcp.get"),
                ] {
                    let resp = rpc.call_id(id, legacy, json!({})).await;
                    assert_eq!(resp["error"]["code"], codes::METHOD_NOT_FOUND, "{legacy}: {resp}");
                }

                let server_task = rpc.disconnect();
                server_task.abort();
                let _ = server_task.await;
                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn mcp_elicitation_callback_includes_session_id() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = HOST_LOCK.lock().await;
                let dir = tempfile::tempdir().unwrap();
                let host = test_host(dir.path()).await;
                let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let mut rpc = TestRpc::connect(host.clone());

                let init = rpc
                    .call_id(
                        1,
                        "initialize",
                        json!({ "protocol_version": "3.0", "capabilities": { "elicitation": true } }),
                    )
                    .await;
                assert_eq!(init["result"]["capabilities"]["callbacks"]["elicitation"], true, "{init}");

                let elicit_host = host.clone();
                let elicit_task = tokio::task::spawn_local(async move {
                    elicit_host
                        .bail_json(
                            "mcp/elicit",
                            json!({
                                "server": "demo-mcp",
                                "id": 7,
                                "params": { "mode": "form", "message": "enter code" }
                            }),
                        )
                        .await
                });

                let req = loop {
                    let msg = rpc.read().await;
                    if msg["method"] == methods::MCP_ELICITATION {
                        break msg;
                    }
                };
                assert_eq!(req["params"]["session_id"], sid, "mcp.elicitation must include session_id: {req}");
                assert_eq!(req["params"]["server"], "demo-mcp", "{req}");
                assert_eq!(req["params"]["message"], "enter code", "{req}");
                rpc.send(json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": { "action": "accept", "content": { "code": "42" } }
                }))
                .await;

                let answer = elicit_task.await.unwrap().expect("mcp/elicit bail answer");
                assert_eq!(answer["action"], "accept", "{answer}");
                assert_eq!(answer["content"]["code"], "42", "{answer}");

                let server_task = rpc.disconnect();
                server_task.abort();
                let _ = server_task.await;
                host.shutdown().await;
            })
            .await;
    }
}

use super::*;

#[test]
fn rpc_responses_errors_callbacks_and_notifications_hide_registered_values() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    lua.globals().set("mare", mare.clone()).unwrap();
    mare_binding::projection::install(&lua, &mare).unwrap();
    let secret = "RPC_WIRE_SECRET_42\nquoted\"value";
    lua.globals().set("SECRET", secret).unwrap();
    let _lease: mlua::Value = lua.load("return mare.projection.register(SECRET)").eval().unwrap();
    let projector = mare_shared::wire_projection::hook(&lua).unwrap();
    let messages = [
        Message::response(RequestId::Number(1), json!({ "nested": [secret] })),
        Message::rpc_error(
            Some(RequestId::Number(2)),
            proto::RpcError::core(proto::ErrorCode::InvalidArguments, format!("bad {secret}"))
                .with_data(json!({ "reason": secret })),
        ),
        Message::request(RequestId::Number(3), methods::CALLBACK_CALL, json!({ "arguments": { "text": secret } })),
        Message::notification("probe/notification", json!({ "text": secret })),
        Message::notification(methods::EVENT, json!({ "data": { "text": secret } })),
    ];
    for message in messages {
        let wire = serde_json::to_string(&project_message(&message, &projector)).unwrap();
        assert!(!wire.contains("RPC_WIRE_SECRET_42"), "registered value leaked: {wire}");
        assert!(wire.contains("[[mare-value:"), "projected value missing: {wire}");
    }
}

#[tokio::test]
async fn multi_session_conn_projects_secrets_from_non_anchor_session() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir1 = tempfile::tempdir().unwrap();
            let dir2 = tempfile::tempdir().unwrap();
            let make_opts = |cwd: &std::path::Path| mare_host::HostOptions {
                cwd: cwd.to_path_buf(),
                mode: mare_shared::ui::Mode::Rpc,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(make_opts(dir1.path())).await.unwrap();
            let rt2 = manager.create_session(make_opts(dir2.path())).await.unwrap();
            const SECRET: &str = "MULTI_SESSION_RPC_SECRET_99";
            rt2.host
                .lua()
                .load(format!(r#"LEAKED_HOLD = mare.projection.register("{SECRET}")"#))
                .exec()
                .unwrap();

            let rpc = RpcRuntime::new(
                manager.clone(),
                make_opts(dir1.path()),
                rt1.host.clone(),
                Some(rt1.session_id.clone()),
                false,
            );
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            cw.write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}"#,
            )
            .await
            .unwrap();
            cw.write_all(b"\n").await.unwrap();
            cw.flush().await.unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                cr.read_line(&mut line).await.unwrap();
                let parsed: J = serde_json::from_str(&line).unwrap();
                if parsed["id"] == 1 {
                    break;
                }
            }
            let conn = rpc.connections.borrow().values().next().unwrap().upgrade().unwrap();

            // 1) Conn::send() projects secrets registered in sibling S2 even while S1 is self.host.
            conn.send(&Message::notification("probe/multi", json!({ "text": SECRET })));
            line.clear();
            cr.read_line(&mut line).await.unwrap();
            assert!(!line.contains(SECRET), "Conn::send leaked S2 secret: {line}");
            assert!(line.contains("[[mare-value:"), "Conn::send missed token: {line}");

            // 2) Conn::send_raw_event() re-projects across S2 even for events emitted from S1.
            let raw_params = json!({ "event": "probe", "data": { "text": SECRET } }).to_string();
            conn.send_raw_event(&raw_params);
            line.clear();
            cr.read_line(&mut line).await.unwrap();
            assert!(!line.contains(SECRET), "send_raw_event leaked S2 secret: {line}");
            assert!(line.contains("[[mare-value:"), "send_raw_event missed token: {line}");

            // 3) Close anchor S1: Conn::send() still projects S2's secret (skip any close/provider event lines).
            let _ = manager.close_session(&rt1.session_id).await;
            conn.send(&Message::response(RequestId::Number(9), json!({ "leak": SECRET })));
            loop {
                line.clear();
                cr.read_line(&mut line).await.unwrap();
                let parsed: J = serde_json::from_str(&line).unwrap();
                if parsed["id"] == 9 {
                    break;
                }
            }
            assert!(!line.contains(SECRET), "post-close send leaked S2 secret: {line}");
            assert!(line.contains("[[mare-value:"), "post-close missed token: {line}");

            // 4) CALLBACK_CALL rejects S2 secrets embedded in JSON object keys.
            let cb_res = conn
                .call_result_tagged(methods::CALLBACK_CALL, None, None, json!({ SECRET: "v" }), None)
                .await;
            assert!(cb_res.is_err(), "CALLBACK_CALL must reject secret in JSON object key: {cb_res:?}");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            let _ = manager.shutdown_all().await;
        })
        .await;
}

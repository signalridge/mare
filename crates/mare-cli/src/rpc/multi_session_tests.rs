use std::collections::HashSet;
use std::ffi::OsString;

use super::serve::serve_manager;
use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;

struct EnvGuard(&'static str, Option<OsString>);

impl EnvGuard {
    fn set(key: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: callers hold `HOST_LOCK` across env-mutating RPC tests and run in isolated nextest processes.
        unsafe { std::env::set_var(key, val) };
        Self(key, prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: paired with `EnvGuard::set` under `HOST_LOCK`.
        unsafe {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

async fn read_reply(cr: &mut (impl AsyncBufReadExt + Unpin), id: i64) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("method").is_none() && msg["id"] == id {
            return msg;
        }
    }
}

async fn prompt_and_wait(
    cr: &mut (impl AsyncBufReadExt + Unpin),
    cw: &mut (impl AsyncWriteExt + Unpin),
    id: i64,
    session_id: &str,
    input: J,
) -> J {
    rpc_send(
        cw,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "agent.prompt",
            "params": { "session_id": session_id, "input": input }
        }),
    )
    .await;
    let rep = read_reply(cr, id).await;
    if rep.get("error").is_some() {
        return rep;
    }
    let turn_id = rep["result"]["turn_id"].as_str().unwrap_or_else(|| panic!("expected turn_id: {rep}")).to_string();
    let wait_id = id + 10_000;
    rpc_send(
        cw,
        json!({
            "jsonrpc": "2.0",
            "id": wait_id,
            "method": "agent.wait",
            "params": { "session_id": session_id, "turn_id": turn_id }
        }),
    )
    .await;
    read_reply(cr, wait_id).await
}

async fn read_event(cr: &mut (impl AsyncBufReadExt + Unpin), name: &str) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg["method"] == "event" && msg["params"]["type"] == name {
            return msg;
        }
    }
}

async fn setup_manager_rpc() -> (
    tempfile::TempDir,
    EnvGuard,
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
    Rc<RpcRuntime>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    setup_manager_rpc_mode(mare_plugin::Mode::Print).await
}

async fn setup_manager_rpc_mode(
    mode: mare_plugin::Mode,
) -> (
    tempfile::TempDir,
    EnvGuard,
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
    Rc<RpcRuntime>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    let host_guard = HOST_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let home_guard = EnvGuard::set("MARE_HOME", home.path());
    let dir = tempfile::tempdir().unwrap();
    let opts = mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode,
        config: json!({
            "model": "faux/echo",
            "plugins": { "permissions": { "enabled": false } }
        }),
        load_init: false,
        project_trusted: true,
        args: json!({}),
        no_user_plugins: true,
    };
    let manager = mare_host::HostManager::new();
    let rt = manager.create_session(opts.clone()).await.unwrap();
    let s1 = rt.session_id.clone();
    let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(s1.clone()), false);

    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);

    rpc_send(
        &mut cw,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "3.0",
                "capabilities": {
                    "callbacks": true,
                    "approvals": true,
                    "elicitation": true
                }
            }
        }),
    )
    .await;
    let init = read_reply(&mut cr, 1).await;
    assert!(init.get("error").is_none(), "{init}");
    (home, home_guard, dir, host_guard, rpc, cr, cw, task, s1)
}

#[tokio::test]
async fn rpc_session_create_load_close_routes_across_sessions_without_stopping_siblings() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            std::fs::write(dir1.path().join("who.txt"), "one").unwrap();
            std::fs::write(dir2.path().join("who.txt"), "two").unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": dir2.path().to_string_lossy() }
                }),
            )
            .await;
            let created = read_reply(&mut cr, 2).await;
            assert!(created.get("error").is_none(), "{created}");
            let s2 = created["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(s1, s2);

            // `options.set` / `options.get` isolate per-session state without disturbing sibling sessions.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"options.set","params":{"session_id":s1,"values":{"plugins.permissions.mode":"workspace-write"}}}),
            )
            .await;
            let _ = read_reply(&mut cr, 3).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"session.get","params":{"session_id":s2}}),
            )
            .await;
            let snap2 = read_reply(&mut cr, 4).await;
            assert!(snap2.get("error").is_none(), "{snap2}");
            assert_eq!(
                dunce::canonicalize(snap2["result"]["cwd"].as_str().unwrap()).unwrap(),
                dunce::canonicalize(dir2.path()).unwrap(),
                "{snap2}"
            );

            // `workspace.set_trust` updates only the addressed workspace.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"workspace.set_trust","params":{"cwd":dir2.path().to_string_lossy(),"trusted":false,"save":false}}),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"workspace.get","params":{"cwd":dir1.path().to_string_lossy()}}),
            )
            .await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"workspace.get","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 6).await["result"]["trusted"], true);
            assert_eq!(read_reply(&mut cr, 7).await["result"]["trusted"], false);

            // Persist `s2` with a full turn before closing so `session.load` can reload it.
            let p2 = prompt_and_wait(&mut cr, &mut cw, 70, &s2, json!("persist s2")).await;
            assert!(p2.get("error").is_none(), "{p2}");

            // `session.close` unloads only `s2`; sibling `s1` and the process stay up.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"session.close","params":{"session_id":s2}}),
            )
            .await;
            let closed = read_reply(&mut cr, 8).await;
            assert!(closed.get("error").is_none(), "{closed}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"agent.get","params":{"session_id":s1}}),
            )
            .await;
            assert!(read_reply(&mut cr, 9).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"agent.get","params":{"session_id":s2}}),
            )
            .await;
            let dead = read_reply(&mut cr, 10).await;
            assert_eq!(dead["error"]["data"]["code"], "unknown_session", "{dead}");

            // `session.load` reloads persisted `s2` by session id back into HostManager.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":11,"method":"session.load","params":{"session_id":s2}}),
            )
            .await;
            let loaded = read_reply(&mut cr, 11).await;
            assert_eq!(loaded["result"]["session_id"], s2, "{loaded}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"agent.get","params":{"session_id":s2}}),
            )
            .await;
            assert!(read_reply(&mut cr, 12).await.get("error").is_none());

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_scope_all_injects_into_future_sessions_and_isolates_session_scope() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let faux_path = home.path().join("faux-tool.json");
            std::fs::write(
                &faux_path,
                json!([
                    { "tool_calls": [{ "name": "global_echo", "arguments": {} }] },
                    { "text": "done" }
                ])
                .to_string(),
            )
            .unwrap();
            let _faux_guard = EnvGuard::set("MARE_FAUX_SCRIPT", &faux_path);
            let dir2 = tempfile::tempdir().unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools.register",
                    "params": {
                        "scope": "all",
                        "name": "global_echo",
                        "description": "global tool",
                        "parameters": { "type": "object", "properties": {} },
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let r2 = read_reply(&mut cr, 2).await;
            assert!(r2.get("error").is_none(), "{r2}");
            let global_reg_id = r2["result"]["registration_id"].as_str().unwrap().to_owned();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools.register",
                    "params": {
                        "scope": "session",
                        "session_id": s1,
                        "name": "only_s1",
                        "description": "local tool",
                        "parameters": { "type": "object", "properties": {} },
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let r3 = read_reply(&mut cr, 3).await;
            assert!(r3.get("error").is_none(), "{r3}");

            // Create `s2` AFTER registering `Scope::All`; it must receive `global_echo` only.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 4).await["result"]["session_id"].as_str().unwrap().to_string();

            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let tools1 = call_service(&rt1.host, "tools", "list", vec![]).await.unwrap();
            let tools2 = call_service(&rt2.host, "tools", "list", vec![]).await.unwrap();
            let names = |v: &J| -> HashSet<String> {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.get("name").and_then(J::as_str).map(str::to_string))
                    .collect()
            };
            let set1 = names(&tools1);
            let set2 = names(&tools2);
            assert!(set1.contains("global_echo") && set1.contains("only_s1"), "{tools1}");
            assert!(set2.contains("global_echo") && !set2.contains("only_s1"), "{tools2}");

            // Running a prompt in `s2` fires `global_echo` via `callback.call`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":90,"method":"agent.prompt","params":{"session_id":s2,"input":"run global tool"}}),
            )
            .await;
            let expected_cwd = dir2.path().to_string_lossy().into_owned();
            let mut got_tool_call = false;
            let turn_id;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "approval.request" {
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"decision":"allow"}})).await;
                } else if msg["method"] == "callback.call" && msg["params"]["callback"] == "execute" {
                    assert_eq!(msg["params"]["registration_id"], global_reg_id, "{msg}");
                    assert_eq!(msg["params"]["arguments"]["ctx"]["cwd"], expected_cwd, "{msg}");
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":"ok"}],"is_error":false}}),
                    )
                    .await;
                    got_tool_call = true;
                } else if msg["id"] == 90 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    turn_id = msg["result"]["turn_id"].as_str().map(str::to_owned);
                    break;
                }
            }
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":91,"method":"agent.wait","params":{"session_id":s2,"turn_id":turn_id.unwrap()}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "approval.request" {
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"decision":"allow"}})).await;
                } else if msg["method"] == "callback.call" && msg["params"]["callback"] == "execute" {
                    assert_eq!(msg["params"]["registration_id"], global_reg_id, "{msg}");
                    assert_eq!(msg["params"]["arguments"]["ctx"]["cwd"], expected_cwd, "{msg}");
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":"ok"}],"is_error":false}}),
                    )
                    .await;
                    got_tool_call = true;
                } else if msg["id"] == 91 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    assert!(got_tool_call, "wait reply={msg}, tool={got_tool_call}");
                    break;
                }
            }

            // Closing `s1` leaves `s2`'s global registration active.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"session.close","params":{"session_id":s1}}),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            assert!(rt2.host.service_table("tools").is_some());

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_multi_session_events_and_peer_message_route_by_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"events.subscribe","params":{"scope":"all","include":["custom/ping"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            rt1.host.emit_json("custom/ping", json!({"from": 1})).await;
            rt2.host.emit_json("custom/ping", json!({"from": 2})).await;

            let ev1 = read_event(&mut cr, "custom/ping").await;
            let ev2 = read_event(&mut cr, "custom/ping").await;
            let seen: HashSet<(Option<&str>, Option<i64>)> = [
                (ev1["params"]["session_id"].as_str(), ev1["params"]["data"]["from"].as_i64()),
                (ev2["params"]["session_id"].as_str(), ev2["params"]["data"]["from"].as_i64()),
            ]
            .into_iter()
            .collect();
            assert!(seen.contains(&(Some(s1.as_str()), Some(1))), "{ev1} / {ev2}");
            assert!(seen.contains(&(Some(s2.as_str()), Some(2))), "{ev1} / {ev2}");

            // Narrow subscription to `s2` only; emit on `s1` then `s2`, only `s2` arrives.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"scope":"session","session_id":s2,"include":["custom/ping"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 4).await.get("error").is_none());
            rt1.host.emit_json("custom/ping", json!({"from": 10})).await;
            rt2.host.emit_json("custom/ping", json!({"from": 20})).await;
            let only_s2 = read_event(&mut cr, "custom/ping").await;
            assert_eq!(only_s2["params"]["session_id"], s2, "{only_s2}");
            assert_eq!(only_s2["params"]["data"]["from"], 20, "{only_s2}");

            // `peer.message` routes to the active default session (`s2` after `session.create`).
            let lua2 = rt2.host.lua();
            let ctx2 = rt2.host.kernel.host_ctx().unwrap();
            let own2 = rt2.host.kernel.host_owner().unwrap();
            let seen_s2 = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
            let seen_s2_c = seen_s2.clone();
            let bail_fn = lua2
                .create_function(move |_, args: MultiValue| {
                    if let Some(Value::Table(t)) = args.front()
                        && let Ok(text) = t.get::<String>("text")
                    {
                        seen_s2_c.borrow_mut().push(text);
                    }
                    Ok(true)
                })
                .unwrap();
            let _rel = rt2.host.kernel.host_on(&ctx2, own2, "peer/inbound", bail_fn, mare_kernel::HostOn::default()).unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "peer.message",
                    "params": { "text": "hello session two" }
                }),
            )
            .await;
            let sent = read_reply(&mut cr, 5).await;
            assert_eq!(sent["result"]["status"], "handled", "{sent}");
            assert_eq!(seen_s2.borrow().as_slice(), &["hello session two"]);

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

async fn setup_manager_rpc_with(
    config: J,
    args: J,
    project_trusted: bool,
) -> (
    tempfile::TempDir,
    EnvGuard,
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
    Rc<RpcRuntime>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    let host_guard = HOST_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let home_guard = EnvGuard::set("MARE_HOME", home.path());
    let dir = tempfile::tempdir().unwrap();
    let opts = mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Print,
        config,
        load_init: false,
        project_trusted,
        args,
        no_user_plugins: true,
    };
    let manager = mare_host::HostManager::new();
    let rt = manager.create_session(opts.clone()).await.unwrap();
    let s1 = rt.session_id.clone();
    let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(s1.clone()), false);

    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);

    rpc_send(
        &mut cw,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocol_version": "3.0", "capabilities": { "approvals": true, "callbacks": true, "elicitation": true } }
        }),
    )
    .await;
    let init = read_reply(&mut cr, 1).await;
    assert!(init.get("error").is_none(), "{init}");
    (home, home_guard, dir, host_guard, rpc, cr, cw, task, s1)
}

async fn second_runtime(rpc: &Rc<RpcRuntime>, dir: &std::path::Path) -> Rc<SessionRuntime> {
    let mut opts = rpc.base_opts.borrow().clone();
    opts.cwd = dir.to_owned();
    rpc.manager.create_session(opts).await.unwrap()
}

fn allow(host: &Host, allow: &[&str]) {
    let mut config = host.kernel.config();
    config["peer"] = json!({"allow":allow});
    host.kernel.set_config(config);
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

#[tokio::test]
async fn peer_authorization_uses_the_target_host_in_both_directions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = second_runtime(&rpc, dir.path()).await;
            let s2 = rt2.session_id.clone();
            assert_eq!(rpc.anchor_session_id.borrow().as_deref(), Some(s1.as_str()));
            allow(&rt1.host, &["options.get", "agent.get"]);
            allow(&rt2.host, &[]);
            let (peer, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let peer_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr, sw, None));
            let (pr, mut pw) = tokio::io::split(peer);
            let mut pr = BufReader::new(pr);
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut pr, 1).await.get("error").is_none());

            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":2,"method":"options.get","params":{"session_id":s2,"keys":["model"]}}),
            )
            .await;
            let denied = read_reply(&mut pr, 2).await;
            assert!(denied.get("error").is_some(), "anchor grant must not authorize target: {denied}");
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":3,"method":"options.get","params":{"session_id":s1,"keys":["model"]}}),
            )
            .await;
            let allowed = read_reply(&mut pr, 3).await;
            assert!(allowed.get("error").is_none(), "target grant should authorize: {allowed}");

            allow(&rt1.host, &[]);
            allow(&rt2.host, &["options.get", "agent.get"]);
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":4,"method":"options.get","params":{"session_id":s2,"keys":["model"]}}),
            )
            .await;
            let allowed = read_reply(&mut pr, 4).await;
            assert!(allowed.get("error").is_none(), "target grant must not depend on anchor: {allowed}");
            rpc_send(&mut pw, json!({"jsonrpc":"2.0","id":5,"method":"agent.get","params":{"session_id":s2}})).await;
            let allowed_contract = read_reply(&mut pr, 5).await;
            assert!(allowed_contract.get("error").is_none(), "nested grant must follow target: {allowed_contract}");

            drop((pw, pr));
            peer_task.await.unwrap();
            drop((cw, cr));
            task.await.unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn ignored_session_id_cannot_retarget_host_scoped_protocol_authorization() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, _dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
        let dir2 = tempfile::tempdir().unwrap();
        let rt1 = rpc.manager.session(&s1).unwrap();
        let rt2 = second_runtime(&rpc, dir2.path()).await;
        allow(&rt1.host, &[]);
        allow(&rt2.host, &["config.describe"]);
        let (peer, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let peer_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr, sw, None));
        let (pr, mut pw) = tokio::io::split(peer);
        let mut pr = BufReader::new(pr);
        rpc_send(&mut pw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
        assert!(read_reply(&mut pr, 1).await.get("error").is_none());
        // config.describe ignores session_id and executes on the default host.
        rpc_send(&mut pw, json!({"jsonrpc":"2.0","id":2,"method":"config.describe","params":{"session_id":rt2.session_id,"keys":["model"]}})).await;
        let reply = read_reply(&mut pr, 2).await;
        assert!(reply.get("error").is_some(), "ignored session_id authorized the default host: {reply}");
        drop((pw, pr));
        peer_task.await.unwrap();
        drop((cw, cr));
        task.await.unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

fn seed_entry(host: &Host, marker: &str) {
    let lua = host.kernel.lua();
    let setup: mlua::Function = lua
        .load(
            r#"
            local session, text = ...
            session:persist_message({ message = { role = "user", content = { { type = "text", text = text } } } })
            "#,
        )
        .into_function()
        .unwrap();
    setup.call::<()>((host.kernel.service("session").unwrap(), marker)).unwrap();
}

async fn read_snapshot(cr: &mut (impl AsyncBufReadExt + Unpin)) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg["method"] == "snapshot" {
            return msg;
        }
    }
}

#[tokio::test]
async fn ignored_session_id_cannot_retarget_host_scoped_contract_authorization() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = second_runtime(&rpc, dir2.path()).await;
            allow(&rt1.host, &[]);
            allow(&rt2.host, &["session.create", "plugins.list"]);

            let (peer, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let peer_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr, sw, None));
            let (pr, mut pw) = tokio::io::split(peer);
            let mut pr = BufReader::new(pr);
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut pr, 1).await.get("error").is_none());

            // Host-scoped contract methods without `session_id` in their schema must authorize against
            // the default host, never a foreign host named by an extraneous `session_id`.
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"session_id":rt2.session_id}}),
            )
            .await;
            let create_reply = read_reply(&mut pr, 2).await;
            assert!(
                create_reply.get("error").is_some(),
                "ignored session_id authorized session.create on foreign host: {create_reply}"
            );

            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":3,"method":"plugins.list","params":{"session_id":rt2.session_id}}),
            )
            .await;
            let pkg_reply = read_reply(&mut pr, 3).await;
            assert!(
                pkg_reply.get("error").is_some(),
                "ignored session_id authorized plugins.list on foreign host: {pkg_reply}"
            );

            drop((pw, pr));
            peer_task.await.unwrap();
            drop((cw, cr));
            task.await.unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn cross_host_baseline_handle_ownership_and_revocation() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
        let rt1 = rpc.manager.session(&s1).unwrap();
        let rt2 = second_runtime(&rpc, dir.path()).await;
        let s2 = rt2.session_id.clone();
        seed_entry(&rt1.host, "HOST1_ENTRY");
        seed_entry(&rt2.host, "HOST2_ENTRY");

        allow(&rt1.host, &[]);
        allow(&rt2.host, &["events.subscribe", "baseline.read"]);

        let (peer1, server1) = tokio::io::duplex(65536);
        let (sr1, sw1) = tokio::io::split(server1);
        let peer1_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr1, sw1, None));
        let (pr1, mut pw1) = tokio::io::split(peer1);
        let mut pr1 = BufReader::new(pr1);
        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
        assert!(read_reply(&mut pr1, 1).await.get("error").is_none());

        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"session_id":s2,"session_epoch":"old","last_seq":0}})).await;
        let sub2 = read_reply(&mut pr1, 2).await;
        assert_eq!(sub2["result"]["mode"], "baseline", "{sub2}");
        let snap2 = read_snapshot(&mut pr1).await;
        let bid2 = snap2["params"]["baseline_id"].as_str().unwrap().to_owned();

        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":3,"method":"baseline.read","params":{"baseline_id":bid2,"part":"entries"}})).await;
        let page2 = read_reply(&mut pr1, 3).await;
        assert!(page2.to_string().contains("HOST2_ENTRY"), "target host baseline.read failed despite target grant: {page2}");

        // Second peer must not read Peer1's handle on rt2 even when both hosts allow baseline.read.
        allow(&rt1.host, &["events.subscribe", "baseline.read"]);
        let (peer2, server2) = tokio::io::duplex(65536);
        let (sr2, sw2) = tokio::io::split(server2);
        let peer2_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr2, sw2, None));
        let (pr2, mut pw2) = tokio::io::split(peer2);
        let mut pr2 = BufReader::new(pr2);
        rpc_send(&mut pw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
        assert!(read_reply(&mut pr2, 1).await.get("error").is_none());
        rpc_send(&mut pw2, json!({"jsonrpc":"2.0","id":2,"method":"baseline.read","params":{"baseline_id":bid2,"part":"entries"}})).await;
        let stolen = read_reply(&mut pr2, 2).await;
        assert!(!stolen.to_string().contains("HOST2_ENTRY"), "foreign peer read Peer1 baseline handle: {stolen}");
        assert_eq!(stolen["error"]["data"]["code"], "baseline.stale", "{stolen}");

        // Peer1 also opens a baseline on rt1; revoking rt2 invalidates bid2 without breaking bid1.
        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"session_id":s1,"session_epoch":"old","last_seq":0}})).await;
        let sub1 = read_reply(&mut pr1, 4).await;
        assert_eq!(sub1["result"]["mode"], "baseline", "{sub1}");
        let snap1 = read_snapshot(&mut pr1).await;
        let bid1 = snap1["params"]["baseline_id"].as_str().unwrap().to_owned();

        allow(&rt2.host, &["events.subscribe"]);
        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":5,"method":"baseline.read","params":{"baseline_id":bid2,"part":"active_items"}})).await;
        let revoked2 = read_reply(&mut pr1, 5).await;
        assert!(revoked2.get("error").is_some(), "{revoked2}");

        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":6,"method":"baseline.read","params":{"baseline_id":bid1,"part":"entries"}})).await;
        let page1 = read_reply(&mut pr1, 6).await;
        assert!(page1.to_string().contains("HOST1_ENTRY"), "revoking rt2 broke rt1 baseline: {page1}");

        allow(&rt2.host, &["events.subscribe", "baseline.read"]);
        rpc_send(&mut pw1, json!({"jsonrpc":"2.0","id":7,"method":"baseline.read","params":{"baseline_id":bid2,"part":"active_items"}})).await;
        let regranted2 = read_reply(&mut pr1, 7).await;
        assert_eq!(
            regranted2["error"]["data"]["code"], "baseline.stale",
            "revoked handle must stay stale after regrant: {regranted2}"
        );

        drop((pw2, pr2));
        peer2_task.await.unwrap();
        drop((pw1, pr1));
        peer1_task.await.unwrap();
        drop((cw, cr));
        task.await.unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

#[tokio::test]
async fn remote_admin_scope_all_denied_when_any_target_host_forbids_it() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, _dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
        let dir2 = tempfile::tempdir().unwrap();
        let rt1 = rpc.manager.session(&s1).unwrap();
        let rt2 = second_runtime(&rpc, dir2.path()).await;
        for (host, value) in [(&rt1.host, true), (&rt2.host, false)] {
            let mut config = host.kernel.config();
            config["webserver"] = json!({"remote_admin": value});
            host.kernel.set_config(config);
        }
        let (client, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let web_task = tokio::task::spawn_local(super::serve_inner(
            rpc.clone(), sr, sw, Principal::Owner { remote_admin: false }, false, None, std::future::pending(),
        ));
        let (reader, mut writer) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}})).await;
        assert!(read_reply(&mut reader, 1).await.get("error").is_none());

        rpc_send(
            &mut writer,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools.register",
                "params": {
                    "scope": "all",
                    "name": "remote_tool",
                    "description": "d",
                    "parameters": { "type": "object", "properties": {} },
                    "callbacks": ["execute"]
                }
            }),
        )
        .await;
        let reply = read_reply(&mut reader, 2).await;
        assert_eq!(reply["error"]["data"]["code"], "denied", "scope=all tools.register must be denied if any host forbids remote_admin: {reply}");
        let tools2 = call_service(&rt2.host, "tools", "list", vec![]).await.unwrap();
        let has_remote_tool = tools2
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t.get("name").and_then(J::as_str) == Some("remote_tool"));
        assert!(!has_remote_tool, "denied scope=all tools.register leaked tool onto rt2: {tools2}");

        drop((writer, reader));
        web_task.await.unwrap();
        drop((cw, cr));
        task.await.unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

#[tokio::test]
async fn workspace_trust_checks_remote_admin_for_every_session_in_workspace() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = second_runtime(&rpc, dir.path()).await;
            assert_eq!(rt1.workspace, rt2.workspace);
            assert!(rt1.host.is_project_trusted() && rt2.host.is_project_trusted());
            for (host, enabled) in [(&rt1.host, true), (&rt2.host, false)] {
                let mut config = host.kernel.config();
                config["webserver"] = json!({"remote_admin": enabled});
                host.kernel.set_config(config);
            }
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let web_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (reader, mut writer) = tokio::io::split(client);
            let mut reader = BufReader::new(reader);
            rpc_send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut reader, 1).await.get("error").is_none());
            let params = json!({"cwd": dir.path(), "trusted": false, "save": false});
            rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":2,"method":"workspace.set_trust","params":params})).await;
            let denied = read_reply(&mut reader, 2).await;
            assert_eq!(denied["error"]["data"]["code"], "denied", "{denied}");
            assert!(
                rt1.host.is_project_trusted() && rt2.host.is_project_trusted(),
                "denial must not modify either host"
            );

            let mut config = rt2.host.kernel.config();
            config["webserver"]["remote_admin"] = json!(true);
            rt2.host.kernel.set_config(config);
            rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":3,"method":"workspace.set_trust","params":params})).await;
            let allowed = read_reply(&mut reader, 3).await;
            assert!(allowed.get("error").is_none(), "{allowed}");
            assert!(!rt1.host.is_project_trusted() && !rt2.host.is_project_trusted());
            drop((writer, reader));
            web_task.await.unwrap();
            drop((cw, cr));
            task.await.unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_admin_is_checked_on_the_target_host() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, _dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
        let dir2 = tempfile::tempdir().unwrap();
        let rt1 = rpc.manager.session(&s1).unwrap();
        let rt2 = second_runtime(&rpc, dir2.path()).await;
        let s2 = rt2.session_id.clone();
        for (host, value) in [(&rt1.host, true), (&rt2.host, false)] {
            let mut config = host.kernel.config();
            config["webserver"] = json!({"remote_admin":value});
            host.kernel.set_config(config);
        }
        // A nonlocal Web owner has no unconditional local-admin privilege.
        let (client, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let web_task = tokio::task::spawn_local(super::serve_inner(
            rpc.clone(), sr, sw, Principal::Owner { remote_admin: false }, false, None, std::future::pending(),
        ));
        let (reader, mut writer) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
        assert!(read_reply(&mut reader, 1).await.get("error").is_none());
        rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":2,"method":"options.set","params":{"session_id":s1,"values":{"effort":"high"}}})).await;
        let allowed = read_reply(&mut reader, 2).await;
        assert!(allowed.get("error").is_none(), "target host allows remote admin: {allowed}");
        rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":3,"method":"options.set","params":{"session_id":s2,"values":{"effort":"high"}}})).await;
        let denied = read_reply(&mut reader, 3).await;
        assert_eq!(denied["error"]["data"]["code"], "denied", "target host denies remote admin: {denied}");
        for (host, value) in [(&rt1.host, false), (&rt2.host, true)] {
            let mut config = host.kernel.config();
            config["webserver"] = json!({"remote_admin":value});
            host.kernel.set_config(config);
        }
        rpc_send(&mut writer, json!({"jsonrpc":"2.0","id":4,"method":"options.set","params":{"session_id":s2,"values":{"effort":"high"}}})).await;
        let allowed = read_reply(&mut reader, 4).await;
        assert!(allowed.get("error").is_none(), "target admin grant must be live: {allowed}");
        drop((writer, reader));
        web_task.await.unwrap();
        drop((cw, cr));
        task.await.unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

#[tokio::test]
async fn authorization_rechecks_resolved_target_host_when_fallback_differs_from_anchor() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = second_runtime(&rpc, dir.path()).await;

            // Anchor (rt1) allows remote_admin and peer commands.run / session.update,
            // while target (rt2) denies both.
            {
                let mut c1 = rt1.host.kernel.config();
                c1["webserver"] = json!({"remote_admin": true});
                c1["peer"] = json!({"allow": ["commands.run", "session.update"]});
                rt1.host.kernel.set_config(c1);

                let mut c2 = rt2.host.kernel.config();
                c2["webserver"] = json!({"remote_admin": false});
                c2["peer"] = json!({"allow": []});
                rt2.host.kernel.set_config(c2);
            }

            // Simulate rt2 reporting a live session_id not indexed under manager.sessions (so initial
            // resolve_contract_target / request_target_host fall back to anchor for perm_host, while
            // find_session_runtime / resolve_session_target resolve rt2).
            let drifted_sid = "drifted-s2-identity";
            let s2_svc = rt2.host.service_table("session").expect("session service");
            rt2.host
                .lua()
                .load(
                    r#"
                    local svc, new_id = ...
                    local orig = svc.info
                    svc.info = function(self, ...)
                        local res = orig(self, ...)
                        res.session_id = new_id
                        return res
                    end
                    "#,
                )
                .call::<()>((s2_svc, drifted_sid))
                .unwrap();

            // 1. Protocol options.set (admin = true) via remote non-admin owner must be denied on rt2.
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let web_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (reader, mut writer) = tokio::io::split(client);
            let mut reader = BufReader::new(reader);
            rpc_send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut reader, 1).await.get("error").is_none());

            rpc_send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":2,"method":"options.set","params":{"session_id":drifted_sid,"values":{"effort":"high"}}}),
            )
            .await;
            let denied_protocol = read_reply(&mut reader, 2).await;
            assert_eq!(
                denied_protocol["error"]["data"]["code"], "denied",
                "protocol options.set resolved via fallback must authorize against resolved host rt2: {denied_protocol}"
            );
            drop((writer, reader));
            web_task.await.unwrap();

            // 2. Scope::Host fallback_named_rt (commands.run) & Scope::Session contract (session.update) via peer
            //    must both be denied on rt2 even when anchor allows them.
            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let peer_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                psr,
                psw,
                Principal::Peer,
                false,
                None,
                std::future::pending(),
            ));
            let (p_reader, mut p_writer) = tokio::io::split(p_client);
            let mut p_reader = BufReader::new(p_reader);
            rpc_send(
                &mut p_writer,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut p_reader, 1).await.get("error").is_none());

            rpc_send(
                &mut p_writer,
                json!({"jsonrpc":"2.0","id":2,"method":"commands.run","params":{"session_id":drifted_sid,"line":"/help"}}),
            )
            .await;
            let denied_host_scope = read_reply(&mut p_reader, 2).await;
            assert_eq!(
                denied_host_scope["error"]["data"]["code"], "denied",
                "Scope::Host fallback_named_rt must authorize against resolved host rt2: {denied_host_scope}"
            );

            rpc_send(
                &mut p_writer,
                json!({"jsonrpc":"2.0","id":3,"method":"session.update","params":{"session_id":drifted_sid,"name":"unauthorized-rename"}}),
            )
            .await;
            let denied_session_contract = read_reply(&mut p_reader, 3).await;
            assert_eq!(
                denied_session_contract["error"]["data"]["code"], "denied",
                "Scope::Session contract must authorize against resolved host rt2: {denied_session_contract}"
            );

            drop((p_writer, p_reader));
            peer_task.await.unwrap();
            drop((cw, cr));
            task.await.unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

async fn cancel_waits_on_target_reload(method: &str, resource_field: &str, service: &str) {
    let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
    let dir2 = tempfile::tempdir().unwrap();
    let rt1 = rpc.manager.session(&s1).unwrap();
    let rt2 = second_runtime(&rpc, dir2.path()).await;
    let sid = rt2.session_id.clone();
    for host in [&rt1.host, &rt2.host] {
        let service_value = host.kernel.service(service).unwrap();
        host.lua().load(r#"local s = ...; local old = s.cancel; s.cancel = function(self, ...) _G.__p0_cancel_called = true; return old(self, ...) end"#)
            .into_function().unwrap().call::<()>(service_value).unwrap();
    }
    let conn = rpc.connections.borrow().values().find_map(std::rc::Weak::upgrade).unwrap();
    let mut params = Some(json!({"session_id":sid}));
    params.as_mut().unwrap().as_object_mut().unwrap().insert(resource_field.to_owned(), json!("missing"));
    let hold = rt2.host.kernel.reload_hold();
    let mut waiting = Box::pin(super::super::envelope::reload_settled(
        &conn,
        method,
        &params,
        tokio_util::sync::CancellationToken::new(),
    ));
    assert!(matches!(poll_once(waiting.as_mut()), std::task::Poll::Pending), "{method} bypassed target reload barrier");
    let mut agent_cancel = Box::pin(super::super::envelope::reload_settled(
        &conn,
        "agent.cancel",
        &params,
        tokio_util::sync::CancellationToken::new(),
    ));
    assert!(
        matches!(poll_once(agent_cancel.as_mut()), std::task::Poll::Ready(Ok(()))),
        "agent.cancel must remain exempt"
    );
    drop(hold);
    waiting.await.unwrap();
    let anchor_hold = rt1.host.kernel.reload_hold();
    let mut unrelated = Box::pin(super::super::envelope::reload_settled(
        &conn,
        method,
        &params,
        tokio_util::sync::CancellationToken::new(),
    ));
    assert!(
        matches!(poll_once(unrelated.as_mut()), std::task::Poll::Ready(Ok(()))),
        "unrelated anchor reload blocked target"
    );
    drop(anchor_hold);

    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":20,"method":method,"params":params})).await;
    let _ = read_reply(&mut cr, 20).await;
    let target_called: bool = rt2.host.lua().load("return _G.__p0_cancel_called == true").eval().unwrap();
    let anchor_called: bool = rt1.host.lua().load("return _G.__p0_cancel_called == true").eval().unwrap();
    assert!(target_called && !anchor_called, "{method} must execute on target host");
    drop((cw, cr));
    task.await.unwrap();
    let _ = rpc.manager.shutdown_all().await;
}

#[tokio::test]
async fn subagent_cancel_waits_for_target_host_reload() {
    tokio::task::LocalSet::new()
        .run_until(cancel_waits_on_target_reload("subagent.cancel", "subagent_id", "subagent"))
        .await;
}

#[tokio::test]
async fn cells_cancel_waits_for_target_host_reload() {
    tokio::task::LocalSet::new().run_until(cancel_waits_on_target_reload("cells.cancel", "cell_id", "cells")).await;
}

#[tokio::test]
async fn peer_omitted_scope_after_default_session_close_refuses_instead_of_all() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["events.subscribe", "tools.register", "custom/ping"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                json!({}),
                true,
            )
            .await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            // Close active default session `s2` while `s1` remains open: default_session becomes None.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.close","params":{"session_id":s2}}))
                .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            // Peer connects after default session closed while s1 remains open.
            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let p_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), psr, psw, false, None, std::future::pending()));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"callbacks":true}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // Omitting scope and session_id on a peer must fail with invalid_arguments, NEVER Scope::All.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"include":["custom/ping"]}}),
            )
            .await;
            let sub_res = read_reply(&mut pcr, 2).await;
            assert_eq!(
                sub_res["error"]["data"]["code"].as_str(),
                Some("invalid_arguments"),
                "peer must not fall back to Scope::All when default session is closed: {sub_res}"
            );

            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools.register",
                    "params": {
                        "name": "p_leak", "description": "x", "parameters": { "type": "object", "properties": {} },
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let tool_res = read_reply(&mut pcr, 3).await;
            assert_eq!(
                tool_res["error"]["data"]["code"].as_str(),
                Some("invalid_arguments"),
                "peer tool registration must not fall back to Scope::All: {tool_res}"
            );
            assert!(rpc.manager.session(&s1).is_ok());

            drop(pcw);
            drop(pcr);
            tokio::time::timeout(Duration::from_secs(5), p_task).await.unwrap().unwrap();
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn session_create_load_fork_use_host_manager_and_support_close_and_subscribe() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Seed one completed turn in s1 so fork has a completed boundary.
            let p1 = prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("seed")).await;
            assert!(p1.get("error").is_none(), "{p1}");

            // session.fork must create an independent SessionRuntime in HostManager.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.fork","params":{"session_id":s1}}))
                .await;
            let fk = read_reply(&mut cr, 3).await;
            assert!(fk.get("error").is_none(), "{fk}");
            let fork_id = fk["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(fork_id, s1);
            assert!(rpc.manager.session(&fork_id).is_ok(), "forked id must be registered in HostManager");
            assert!(rpc.manager.session(&s1).is_ok(), "source s1 must stay open after fork");

            // Subscribe strictly to fork_id events, then close fork_id.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"events.subscribe","params":{"scope":"session","session_id":fork_id,"include":["custom/ping"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 4).await.get("error").is_none());
            rpc.manager.session(&s1).unwrap().host.emit_json("custom/ping", json!({"from":"s1"})).await;
            rpc.manager.session(&fork_id).unwrap().host.emit_json("custom/ping", json!({"from":"fork"})).await;
            let ev = read_event(&mut cr, "custom/ping").await;
            assert_eq!(ev["params"]["session_id"], fork_id, "{ev}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"session.close","params":{"session_id":fork_id}}),
            )
            .await;
            let close_fk = read_reply(&mut cr, 5).await;
            assert!(close_fk.get("error").is_none(), "{close_fk}");
            assert!(rpc.manager.session(&s1).is_ok(), "closing fork must keep s1 open");

            // session.create must create a closeable SessionRuntime in HostManager and emit
            // `session/changed` with `session_id` (never bare `id`).
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.create","params":{}})).await;
            let sn = read_reply(&mut cr, 6).await;
            assert!(sn.get("error").is_none(), "{sn}");
            let new_id = sn["result"]["session_id"].as_str().unwrap().to_string();
            let new_rt = rpc.manager.session(&new_id).expect("session.create id must be in HostManager");
            let (_, new_epoch) = new_rt.host.kernel.event_log().session_identity();
            let mut sub = new_rt.host.kernel.event_log().subscribe_cursor(
                Some(&new_epoch),
                Some(0),
                false,
                Arc::new(|n| Some(n == "session/changed")),
                None,
            );
            let mut found_changed = None;
            while let Ok(Some(rec)) = sub.cursor.try_recv() {
                if let mare_kernel::CursorRecord::Event { event, .. } = rec
                    && event.name == "session/changed"
                {
                    found_changed = Some(event.payload);
                    break;
                }
            }
            let changed = found_changed.expect("session.create must admit session/changed");
            assert_eq!(changed["session_id"], new_id, "{changed}");
            assert!(changed.get("id").is_none(), "bare id must be absent: {changed}");

            assert!(prompt_and_wait(&mut cr, &mut cw, 7, &new_id, json!("persist")).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":71,"method":"session.update","params":{"session_id":new_id,"name":"new-persisted"}}),
            )
            .await;
            let ren = read_reply(&mut cr, 71).await;
            let new_path = ren["result"]["path"].as_str().expect("persisted session path after turn").to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"session.close","params":{"session_id":new_id}}),
            )
            .await;
            assert!(read_reply(&mut cr, 8).await.get("error").is_none());

            // session.load must load into HostManager and be closeable by session_id.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"session.load","params":{"path":new_path}}),
            )
            .await;
            let sr = read_reply(&mut cr, 9).await;
            assert!(sr.get("error").is_none(), "{sr}");
            let resumed_id = sr["result"]["session_id"].as_str().unwrap().to_string();
            assert_eq!(resumed_id, new_id);
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"session.close","params":{"session_id":resumed_id}}),
            )
            .await;
            assert!(read_reply(&mut cr, 10).await.get("error").is_none());

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn events_subscribe_status_changed_isolated_by_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            // Subscribe initially to ALL sessions (`scope: "all"`), then narrow exclusively to `s2`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":30,"method":"events.subscribe","params":{"scope":"all","include":["status/changed"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 30).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"events.subscribe","params":{"scope":"session","session_id":s2,"include":["status/changed"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            // Run s1, emit status/changed on s1, then close s1, then run s2.
            let mut s2_events = 0usize;
            async fn read_reply_checking_status(
                cr: &mut (impl AsyncBufReadExt + Unpin),
                id: i64,
                expected_sid: &str,
                s2_events: &mut usize,
            ) -> J {
                loop {
                    let msg = rpc_read(cr).await;
                    if msg["method"] == "event" && msg["params"]["type"] == "status/changed" {
                        assert_eq!(
                            msg["params"]["session_id"], expected_sid,
                            "status/changed leaked from another session: {msg}"
                        );
                        *s2_events += 1;
                    }
                    if msg["id"] == id {
                        return msg;
                    }
                }
            }
            rpc.manager
                .session(&s1)
                .unwrap()
                .host
                .emit_json(
                    "status/changed",
                    json!({ "state": "working", "message": "s1-only", "session": {}, "cwd": "", "seq": 1 }),
                )
                .await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.prompt","params":{"session_id":s1,"input":"p1"}}),
            )
            .await;
            let r4 = read_reply_checking_status(&mut cr, 4, &s2, &mut s2_events).await;
            assert!(r4.get("error").is_none(), "{r4}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":40,"method":"agent.wait","params":{"session_id":s1,"turn_id":r4["result"]["turn_id"]}}),
            )
            .await;
            assert!(read_reply_checking_status(&mut cr, 40, &s2, &mut s2_events).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.close","params":{"session_id":s1}}))
                .await;
            assert!(read_reply_checking_status(&mut cr, 5, &s2, &mut s2_events).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"agent.prompt","params":{"session_id":s2,"input":"p2"}}),
            )
            .await;
            let r6 = read_reply_checking_status(&mut cr, 6, &s2, &mut s2_events).await;
            assert!(r6.get("error").is_none(), "{r6}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":60,"method":"agent.wait","params":{"session_id":s2,"turn_id":r6["result"]["turn_id"]}}),
            )
            .await;
            assert!(read_reply_checking_status(&mut cr, 60, &s2, &mut s2_events).await.get("error").is_none());

            // Wait for any trailing async `status/changed` emission if not already delivered before prompt reply.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while s2_events == 0 {
                let msg = tokio::time::timeout_at(deadline, rpc_read(&mut cr))
                    .await
                    .expect("expected at least one status/changed event for s2");
                if msg["method"] == "event" && msg["params"]["type"] == "status/changed" {
                    assert_eq!(msg["params"]["session_id"], s2, "status/changed leaked from another session: {msg}");
                    s2_events += 1;
                }
            }
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":7,"method":"options.get","params":{"session_id":s2}}))
                .await;
            assert!(read_reply_checking_status(&mut cr, 7, &s2, &mut s2_events).await.get("error").is_none());
            assert!(s2_events >= 1, "expected at least one status/changed event for s2");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn abort_on_other_session_does_not_cancel_in_flight_approval_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir2 = tempfile::tempdir().unwrap();
            let target_dir = tempfile::tempdir().unwrap();
            let target_file = target_dir.path().join("approved.txt");
            let bash_target = target_file.to_string_lossy().replace('\\', "/");
            let script = json!([
                {"tool_calls": [{"name": "bash", "arguments": {"command": format!("touch '{bash_target}'"), "sandbox_permissions": "all", "justification": "tdd"}}]},
                {"text": "approved-done"}
            ]);
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "plugins": {
                        "permissions": { "preset": "read-only" },
                        "provider-faux": { "script_json": script.to_string() }
                    }
                }),
                json!({}),
                false,
            )
            .await;

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            // Start s1 prompt that will block awaiting approval.request.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"agent.prompt","params":{"session_id":s1,"input":"go"}}),
            )
            .await;
            let mut p10_start = None;
            let mut approval_req = None;
            while p10_start.is_none() || approval_req.is_none() {
                let msg = rpc_read(&mut cr).await;
                if msg.get("method").is_none() && msg["id"] == 10 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    p10_start = Some(msg);
                } else if msg["method"] == "approval.request" {
                    approval_req = Some(msg);
                }
            }
            let turn_id = p10_start.unwrap()["result"]["turn_id"].as_str().unwrap().to_string();
            let approval_id = approval_req.unwrap()["id"].clone();

            // Cancel s2 while s1's approval.request is still unanswered.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"agent.cancel","params":{"session_id":s2}})).await;
            let mut saw_cancel_for_s1 = false;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "$/cancel_request" && msg["params"]["id"] == approval_id {
                    saw_cancel_for_s1 = true;
                }
                if msg["id"] == 11 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }
            assert!(!saw_cancel_for_s1, "agent.cancel(s2) must NOT cancel s1's pending approval.request");

            // Now approve s1's request; prompt turn must complete cleanly (not denied/aborted).
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":approval_id,"result":{"decision":"allow"}})).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"agent.wait","params":{"session_id":s1,"turn_id":turn_id}}),
            )
            .await;
            let p10 = read_reply(&mut cr, 12).await;
            assert!(p10.get("error").is_none(), "{p10}");
            assert!(target_file.exists(), "approved tool must execute after s2 abort");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn fork_with_persisted_image_hydrates_verified_attachment_and_preserves_unbound_refusal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let png_b64 =
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";

            // 1. Live session receives inline verified image and completes turn, persisting attachment.
            let p1 = prompt_and_wait(
                &mut cr,
                &mut cw,
                2,
                &s1,
                json!([
                    { "type": "text", "text": "describe image" },
                    { "type": "image", "mime": "image/png", "data": png_b64 }
                ]),
            )
            .await;
            assert!(p1.get("error").is_none(), "{p1}");

            // Forking live `s1` via `session.fork` MUST succeed and retain image data.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.fork","params":{"session_id":s1}})).await;
            let fk = read_reply(&mut cr, 3).await;
            assert!(fk.get("error").is_none(), "session.fork on live image session failed: {fk}");
            let fork_id = fk["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(fork_id, s1);

            // 2. Unbound disk session with an attachment loaded via `path` must STILL refuse fork.
            let unbound_src = call_service(&rpc.manager.session(&s1).unwrap().host, "session", "get", vec![json!(s1)])
                .await
                .unwrap()["path"]
                .as_str()
                .unwrap()
                .to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.close","params":{"session_id":s1}}))
                .await;
            assert!(read_reply(&mut cr, 4).await.get("error").is_none());

            // After closing source `s1`, `fork_id` must still retain the image block and answer follow-up prompts.
            let fk_host = rpc.manager.session(&fork_id).unwrap().host.clone();
            let fk_msgs = call_service(&fk_host, "agent", "messages", vec![]).await.unwrap();
            let fk_msgs_str = serde_json::to_string(&fk_msgs).unwrap();
            assert!(
                fk_msgs_str.contains(png_b64) || fk_msgs_str.contains("image"),
                "fork_id must retain hydrated image after source closes: {fk_msgs}"
            );
            let fk_p = prompt_and_wait(&mut cr, &mut cw, 41, &fork_id, json!("follow up on fork")).await;
            assert!(fk_p.get("error").is_none(), "follow-up prompt on fork_id failed: {fk_p}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.load","params":{"path":unbound_src}}))
                .await;
            let loaded = read_reply(&mut cr, 5).await;
            assert!(loaded.get("error").is_none(), "{loaded}");
            let loaded_id = loaded["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"session.fork","params":{"session_id":loaded_id}}),
            )
            .await;
            let ref_fk = read_reply(&mut cr, 6).await;
            assert!(
                ref_fk.get("error").is_some(),
                "fork of persisted image session loaded via unbound path must still refuse: {ref_fk}"
            );

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn options_set_propagates_to_subsequent_session_create_and_fork() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Seed one completed turn in s1 so fork works later.
            assert!(prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("seed")).await.get("error").is_none());

            // Set runtime approval policy via `options.set`.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "options.set",
                    "params": {
                        "session_id": s1,
                        "values": {
                            "plugins.approvals.policy": "never",
                            "effort": "high"
                        }
                    }
                }),
            )
            .await;
            let os = read_reply(&mut cr, 3).await;
            assert!(os.get("error").is_none(), "options.set failed: {os}");

            // Newly created session must inherit committed runtime options.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{}})).await;
            let s2 = read_reply(&mut cr, 4).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"options.get","params":{"session_id":s2,"keys":["plugins.approvals.policy","effort"]}}),
            )
            .await;
            let g2 = read_reply(&mut cr, 5).await;
            assert_eq!(g2["result"]["plugins.approvals.policy"], "never", "{g2}");
            assert_eq!(g2["result"]["effort"], "high", "{g2}");

            // Newly forked session must also inherit committed runtime options.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.fork","params":{"session_id":s1}}))
                .await;
            let fk = read_reply(&mut cr, 6).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"options.get","params":{"session_id":fk,"keys":["plugins.approvals.policy","effort"]}}),
            )
            .await;
            let gfk = read_reply(&mut cr, 7).await;
            assert_eq!(gfk["result"]["plugins.approvals.policy"], "never", "{gfk}");
            assert_eq!(gfk["result"]["effort"], "high", "{gfk}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn abort_on_other_session_does_not_cancel_in_flight_mcp_elicitation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            // Trigger an in-flight `mcp/elicit` on `s1`.
            let h1 = rpc.manager.session(&s1).unwrap().host.clone();
            let elicit_fut = tokio::task::spawn_local(async move {
                h1.bail_json(
                    "mcp/elicit",
                    json!({ "elicitationId": "el-1", "server": "srv", "mode": "form", "message": "ask" }),
                )
                .await
            });
            let elicit_req = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "mcp.elicitation" {
                    break msg;
                }
            };
            let elicit_id = elicit_req["id"].clone();

            // Cancel `s2`: must NOT cancel `s1`'s in-flight `mcp.elicitation`.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"agent.cancel","params":{"session_id":s2}})).await;
            let mut saw_cancel = false;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "$/cancel_request" && msg["params"]["id"] == elicit_id {
                    saw_cancel = true;
                }
                if msg["id"] == 10 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }
            assert!(!saw_cancel, "abort(s2) must not cancel s1's mcp.elicitation");

            // Reply to `s1`'s elicitation and verify clean delivery.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":elicit_id,"result":{"action":"accept","content":{"ok":true}}}),
            )
            .await;
            let res = tokio::time::timeout(Duration::from_secs(5), elicit_fut).await.unwrap().unwrap().unwrap();
            assert_eq!(res["action"], "accept", "{res}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn apply_initialize_on_session_on_revoke_is_session_isolated() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr1, mut cw1, task1, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr1, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            // Start a second owner connection `conn2` and manually revoke `conn1`'s answerer lease ONLY on `s2`.
            let rt2 = rpc.manager.session(&s2).unwrap();
            rt2.host.answerer.acquire(rt2.host.lua(), mare_host::ConnId(999_999), Box::new(|_| {}));

            // Triggering `mcp/elicit` on `s1` MUST still work on `conn1` (its s1 lease + caps stay intact).
            let h1 = rpc.manager.session(&s1).unwrap().host.clone();
            let elicit_fut = tokio::task::spawn_local(async move {
                h1.bail_json(
                    "mcp/elicit",
                    json!({ "elicitationId": "el-s1", "server": "srv", "mode": "form", "message": "ask s1" }),
                )
                .await
            });
            let req = loop {
                let msg = rpc_read(&mut cr1).await;
                if msg["method"] == "mcp.elicitation" {
                    break msg;
                }
            };
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":req["id"],"result":{"action":"accept","content":{"ok":true}}}),
            )
            .await;
            let out = tokio::time::timeout(Duration::from_secs(5), elicit_fut).await.unwrap().unwrap().unwrap();
            assert_eq!(out["action"], "accept", "{out}");

            drop(cw1);
            drop(cr1);
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn options_set_runtime_effort_remains_overridable_on_created_and_forked_sessions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Seed one turn on s1 so fork works.
            assert!(prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("seed")).await.get("error").is_none());

            // Runtime options.set sets effort="high" on s1.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"options.set","params":{"session_id":s1,"values":{"effort":"high"}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            // Created s2 inherits "high", but MUST remain overridable to "low" (never locked as CLI layer).
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{}})).await;
            let s2 = read_reply(&mut cr, 4).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"options.set","params":{"session_id":s2,"values":{"effort":"low"}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"options.get","params":{"session_id":s2,"keys":["effort"]}}),
            )
            .await;
            let g2 = read_reply(&mut cr, 6).await;
            assert_eq!(g2["result"]["effort"], "low", "created session effort must not be locked at CLI layer: {g2}");
            assert!(prompt_and_wait(&mut cr, &mut cw, 7, &s2, json!("after effort override")).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"options.get","params":{"session_id":s2,"keys":["effort"]}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 8).await["result"]["effort"], "low");

            // Forked fk inherits from s1 and MUST also remain overridable to "low".
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":9,"method":"session.fork","params":{"session_id":s1}}))
                .await;
            let fk = read_reply(&mut cr, 9).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"options.set","params":{"session_id":fk,"values":{"effort":"low"}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 10).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":11,"method":"options.get","params":{"session_id":fk,"keys":["effort"]}}),
            )
            .await;
            let gfk = read_reply(&mut cr, 11).await;
            assert_eq!(gfk["result"]["effort"], "low", "forked session effort must not be locked at CLI layer: {gfk}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn options_set_model_with_load_init_does_not_override_loaded_session_model() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join("config")).unwrap();
            std::fs::write(
                home.path().join("config/init.lua"),
                "mare.setup({ model = 'faux/echo', plugins = { permissions = { enabled = false } } })\n",
            )
            .unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({}),
                load_init: true,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let s1 = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{}}})).await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // Create and persist historical session s_hist with faux/echo, then close it.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let s_hist = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();
            assert!(prompt_and_wait(&mut cr, &mut cw, 3, &s_hist, json!("keep echo")).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.close","params":{"session_id":s_hist}}))
                .await;
            assert!(read_reply(&mut cr, 4).await.get("error").is_none());

            // Switch runtime default model on active s1 via options.set.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"options.set","params":{"session_id":s1,"values":{"model":"faux/echo2"}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());

            // Loading historical session s_hist under load_init=true MUST restore its own faux/echo model!
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.load","params":{"session_id":s_hist}}))
                .await;
            let load_hist = read_reply(&mut cr, 6).await;
            assert!(load_hist.get("error").is_none(), "session.load s_hist failed: {load_hist}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"options.get","params":{"session_id":s_hist,"keys":["model"]}}),
            )
            .await;
            let gh = read_reply(&mut cr, 7).await;
            assert_eq!(gh["result"]["model"], "faux/echo", "session.load must preserve loaded model under load_init=true: {gh}");

            // Newly created blank session MUST inherit runtime default model faux/echo2.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"session.create","params":{}})).await;
            let s_new = read_reply(&mut cr, 8).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"options.get","params":{"session_id":s_new,"keys":["model"]}}),
            )
            .await;
            let gn = read_reply(&mut cr, 9).await;
            assert_eq!(gn["result"]["model"], "faux/echo2", "session.create must use runtime default model: {gn}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn peer_session_create_and_load_enforce_project_boundary() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.create", "session.load", "session.list"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                json!({}),
                true,
            )
            .await;
            let dir2 = tempfile::tempdir().unwrap();
            // Primary creates & persists a session in dir2 (another project).
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let other_sid = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();
            assert!(prompt_and_wait(&mut cr, &mut cw, 3, &other_sid, json!("other proj")).await.get("error").is_none());

            // Persist a same-project session on s1 as well, so we verify same-project load still works.
            assert!(prompt_and_wait(&mut cr, &mut cw, 4, &s1, json!("same proj")).await.get("error").is_none());

            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{}}})).await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // Cross-project session.create MUST be denied for restricted peer.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let p_create = read_reply(&mut pcr, 2).await;
            assert_eq!(p_create["error"]["data"]["code"], "denied", "cross-project create must be denied: {p_create}");

            // Cross-project session.load (while active) MUST be refused.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.load","params":{"session_id":other_sid}}),
            )
            .await;
            let p_load_live = read_reply(&mut pcr, 3).await;
            assert!(p_load_live.get("error").is_some(), "cross-project live load must fail: {p_load_live}");

            // Close other_sid so it sits only on disk, then verify cross-project session.load STILL fails!
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.close","params":{"session_id":other_sid}}))
                .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":4,"method":"session.load","params":{"session_id":other_sid}}),
            )
            .await;
            let p_load_disk = read_reply(&mut pcr, 4).await;
            assert!(p_load_disk.get("error").is_some(), "cross-project disk load must fail: {p_load_disk}");

            // Same-project session.create & session.load remain allowed for the peer.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":5,"method":"session.create","params":{}})).await;
            assert!(read_reply(&mut pcr, 5).await.get("error").is_none());
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":6,"method":"session.load","params":{"session_id":s1}}))
                .await;
            assert!(read_reply(&mut pcr, 6).await.get("error").is_none());

            drop(pcw);
            drop(pcr);
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn session_snapshot_carries_model_effort_and_fork_rejects_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            assert!(prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("turn 1")).await.get("error").is_none());

            // session.snapshot carries model/effort overrides, returns loaded=false (no session_epoch),
            // and MUST NOT spawn an active SessionRuntime.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.snapshot",
                    "params": { "session_id": s1, "model": "faux/echo", "effort": "high" }
                }),
            )
            .await;
            let snap_rep = read_reply(&mut cr, 3).await;
            assert!(snap_rep.get("error").is_none(), "snapshot failed: {snap_rep}");
            assert_eq!(snap_rep["result"]["loaded"], false, "{snap_rep}");
            assert!(snap_rep["result"].get("session_epoch").is_none(), "unloaded snapshot must omit session_epoch: {snap_rep}");
            let snap_sid = snap_rep["result"]["session_id"].as_str().unwrap().to_string();
            assert!(rpc.manager.session(&snap_sid).is_err(), "snapshot must not leak an active runtime");

            // Loading that snapshot confirms effort override was persisted and returns live session_epoch.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.load","params":{"session_id":snap_sid}}))
                .await;
            let loaded_snap = read_reply(&mut cr, 4).await;
            assert!(loaded_snap.get("error").is_none(), "{loaded_snap}");
            assert!(!loaded_snap["result"]["session_epoch"].as_str().unwrap_or("").is_empty(), "{loaded_snap}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"options.get","params":{"session_id":snap_sid,"keys":["model","effort"]}}),
            )
            .await;
            let opts = read_reply(&mut cr, 5).await;
            assert_eq!(opts["result"]["model"], "faux/echo", "{opts}");
            assert_eq!(opts["result"]["effort"], "high", "{opts}");

            // Also verify model override with session.snapshot -> faux/echo2.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 51,
                    "method": "session.snapshot",
                    "params": { "session_id": s1, "model": "faux/echo2" }
                }),
            )
            .await;
            let snap2_rep = read_reply(&mut cr, 51).await;
            assert!(snap2_rep.get("error").is_none(), "snapshot model=echo2 failed: {snap2_rep}");
            assert_eq!(snap2_rep["result"]["loaded"], false, "{snap2_rep}");
            assert!(snap2_rep["result"].get("session_epoch").is_none(), "{snap2_rep}");
            let snap2_sid = snap2_rep["result"]["session_id"].as_str().unwrap().to_string();
            assert!(rpc.manager.session(&snap2_sid).is_err());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":52,"method":"session.load","params":{"session_id":snap2_sid}}))
                .await;
            assert!(read_reply(&mut cr, 52).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":53,"method":"options.get","params":{"session_id":snap2_sid,"keys":["model"]}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 53).await["result"]["model"], "faux/echo2");

            // Protocol 3.0 removes switch/model/effort on session.fork (both switch=false and switch=true).
            for (rid, params) in [
                (6, json!({ "session_id": s1, "switch": false })),
                (7, json!({ "session_id": s1, "switch": true, "model": "faux/echo2" })),
            ] {
                rpc_send(
                    &mut cw,
                    json!({ "jsonrpc": "2.0", "id": rid, "method": "session.fork", "params": params }),
                )
                .await;
                let bad = read_reply(&mut cr, rid).await;
                assert_eq!(bad["error"]["data"]["code"], "invalid_arguments", "fork with {params} must refuse: {bad}");
            }

            // Standard session.fork creates a live runtime with session_epoch and cleans up on session.close.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"session.fork","params":{"session_id":s1,"name":"live-fork"}})).await;
            let live_fork = read_reply(&mut cr, 8).await;
            assert!(live_fork.get("error").is_none(), "{live_fork}");
            assert_eq!(live_fork["result"]["loaded"], true, "{live_fork}");
            let fork_sid = live_fork["result"]["session_id"].as_str().unwrap().to_string();
            let fork_epoch = live_fork["result"]["session_epoch"].as_str().unwrap_or("").to_string();
            assert!(!fork_epoch.is_empty(), "{live_fork}");
            assert!(rpc.manager.session(&fork_sid).is_ok());

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":9,"method":"session.close","params":{"session_id":fork_sid,"expected_epoch":fork_epoch}})).await;
            assert!(read_reply(&mut cr, 9).await.get("error").is_none());
            assert!(rpc.manager.session(&fork_sid).is_err(), "session.close must unload fork runtime");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn terminal_routes_to_target_session_isolates_handles_and_cleans_up_on_close() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            std::fs::write(dir1.path().join("marker1.txt"), "m1").unwrap();
            std::fs::write(dir2.path().join("marker2.txt"), "m2").unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"terminal.create","params":{"session_id":s1,"cmd":"sh","args":["-c","pwd; ls marker1.txt; sleep 30"]}}),
            )
            .await;
            let t1_rep = read_reply(&mut cr, 3).await;
            assert!(t1_rep.get("error").is_none(), "{t1_rep}");
            let t1 = t1_rep["result"]["terminal_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"terminal.create","params":{"session_id":s2,"cmd":"sh","args":["-c","pwd; ls marker2.txt; sleep 30"]}}),
            )
            .await;
            let t2_rep = read_reply(&mut cr, 4).await;
            assert!(t2_rep.get("error").is_none(), "{t2_rep}");
            let t2 = t2_rep["result"]["terminal_id"].as_str().unwrap().to_string();

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"terminal.list","params":{"session_id":s1}})).await;
            let l1 = read_reply(&mut cr, 5).await;
            assert_eq!(l1["result"]["data"].as_array().unwrap().len(), 1, "{l1}");
            assert_eq!(l1["result"]["data"][0]["terminal_id"], t1, "{l1}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"terminal.list","params":{"session_id":s2}})).await;
            let l2 = read_reply(&mut cr, 6).await;
            assert_eq!(l2["result"]["data"].as_array().unwrap().len(), 1, "{l2}");
            assert_eq!(l2["result"]["data"][0]["terminal_id"], t2, "{l2}");

            // Closing s1 cleans up s1's terminal without disturbing s2's terminal.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":7,"method":"session.close","params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr, 7).await.get("error").is_none());

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"terminal.list","params":{"session_id":s2}})).await;
            let l2_after = read_reply(&mut cr, 8).await;
            assert_eq!(l2_after["result"]["data"].as_array().unwrap().len(), 1, "{l2_after}");
            assert_eq!(l2_after["result"]["data"][0]["terminal_id"], t2, "{l2_after}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn capability_update_across_sessions_is_atomic_fail_closed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let baseline1 = rt1.host.kernel.voter_count("tool/approval-host");
            let baseline2 = rt2.host.kernel.voter_count("tool/approval-host");

            // Attach responder across both sessions and enable all capabilities.
            for (id, sid) in [(3, &s1), (4, &s2)] {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"ui.attach","params":{"session_id":sid,"responder":true}})).await;
                assert!(read_reply(&mut cr, id).await.get("error").is_none());
            }
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":true}}}),
            )
            .await;
            let on_rep = read_reply(&mut cr, 5).await;
            assert!(on_rep.get("error").is_none(), "{on_rep}");
            assert!(rt1.host.answerer.holder().is_some());
            assert!(rt2.host.answerer.holder().is_some());
            assert!(rt1.host.kernel.voter_count("tool/approval-host") >= baseline1);
            assert!(rt2.host.kernel.voter_count("tool/approval-host") >= baseline2);

            let vb1 = rt1.host.kernel.voter_count("tool/before");
            let vb2 = rt2.host.kernel.voter_count("tool/before");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"hooks.subscribe","params":{"event":"tool/before","scope":"session","session_id":s1,"callbacks":["handle"]}})).await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":61,"method":"hooks.subscribe","params":{"event":"tool/before","scope":"session","session_id":s2,"callbacks":["handle"]}})).await;
            assert!(read_reply(&mut cr, 61).await.get("error").is_none());
            assert_eq!(rt1.host.kernel.voter_count("tool/before"), vb1 + 1);
            assert_eq!(rt2.host.kernel.voter_count("tool/before"), vb2 + 1);

            // Revoke all capabilities atomically: both sessions lose UI responder, approval voters, and callbacks fail closed.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            let off_rep = read_reply(&mut cr, 7).await;
            assert!(off_rep.get("error").is_none(), "{off_rep}");
            assert_eq!(off_rep["result"]["capabilities"], json!({"callbacks":false,"ui":false,"approvals":false,"elicitation":false}));
            assert!(rt1.host.answerer.holder().is_none(), "s1 answerer must be released on withdrawal");
            assert!(rt2.host.answerer.holder().is_none(), "s2 answerer must be released on withdrawal");
            assert_eq!(rt1.host.kernel.voter_count("tool/approval-host"), baseline1.saturating_sub(1));
            assert_eq!(rt2.host.kernel.voter_count("tool/approval-host"), baseline2.saturating_sub(1));

            for (label, rt) in [("s1", &rt1), ("s2", &rt2)] {
                let lua = rt.host.lua();
                let ctx = rt.host.kernel.host_ctx().unwrap();
                lua.globals().set("cap_ctx", ctx).unwrap();
                let vote: mlua::Table =
                    lua.load("return cap_ctx:get('policy'):decide('tool/before', { {} })").eval_async().await.unwrap();
                assert_eq!(vote.get::<String>("decision").unwrap(), "deny", "{label} must fail closed after callback revocation: reason={:?}", vote.get::<Option<String>>("reason"));
            }

            // Re-enable does not duplicate approval voters across either session.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":true}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 8).await.get("error").is_none());
            assert_eq!(rt1.host.kernel.voter_count("tool/approval-host"), baseline1);
            assert_eq!(rt2.host.kernel.voter_count("tool/approval-host"), baseline2);

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn capability_update_disconnect_rollback_does_not_reopen_ctx_or_reacquire_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let baseline1 = rt1.host.kernel.voter_count("tool/approval-host");

            for (id, sid) in [(3, &s1), (4, &s2)] {
                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":id,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
                )
                .await;
                assert!(read_reply(&mut cr, id).await.get("error").is_none());
            }

            let conn = rpc
                .connections
                .borrow()
                .values()
                .find_map(|w| w.upgrade())
                .expect("active owner connection");
            // Drop s2's cached ctx so `apply_connection_capabilities` enters `ensure_session_ctx` for s2,
            // and simulate connection close + drain while it awaits `tree_hooks::connect_host`.
            let removed_s2 = conn.session_ctxs.borrow_mut().remove(&s2);
            if let Some((w, cid)) = removed_s2
                && let Some(h) = w.upgrade()
            {
                let _ = h.kernel.dispose_ctx(cid).await;
            }
            let conn_closer = conn.clone();
            let rt1_c = rt1.clone();
            let rt2_c = rt2.clone();
            let closer = tokio::task::spawn_local(async move {
                conn_closer.closed.set(true);
                for (wh, cid) in conn_closer.drain_all_ctxs() {
                    if let Some(h) = wh.upgrade() {
                        super::tree_hooks::disconnect_host(&h, cid).await;
                        let _ = h.kernel.dispose_ctx(cid).await;
                    }
                }
                rt1_c.host.answerer.release(rt1_c.host.lua(), conn_closer.id, "disconnected");
                rt2_c.host.answerer.release(rt2_c.host.lua(), conn_closer.id, "disconnected");
            });

            let cap_fut = super::apply_connection_capabilities(
                &conn,
                proto::ClientCapabilities { callbacks: true, ui: true, approvals: true, elicitation: true },
            );
            let (cap_res, _) = tokio::join!(cap_fut, closer);
            assert!(cap_res.is_err(), "closed connection capability update must fail: {cap_res:?}");

            assert!(
                conn.session_ctxs.borrow().is_empty() && conn.anchor_ctx.borrow().is_none(),
                "disconnect rollback must not reopen disposed kernel ctxs"
            );
            assert!(
                rt1.host.answerer.holder().is_none() && rt2.host.answerer.holder().is_none(),
                "disconnect rollback must not reacquire AnswererSlot"
            );
            assert_eq!(
                rt1.host.kernel.voter_count("tool/approval-host"),
                baseline1.saturating_sub(1),
                "disconnect rollback must not leave approval voters attached"
            );

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn ui_attach_after_close_does_not_mutate_caps_or_acquire_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();
            let conn = rpc.connections.borrow().values().find_map(|w| w.upgrade()).expect("active owner connection");
            conn.caps.borrow_mut().ui = false;
            rt1.host.answerer.release(rt1.host.lua(), conn.id, "reset");

            let permit = conn.cap_gate.acquire().await.unwrap();
            let conn_attach = conn.clone();
            let sid = s1.clone();
            let attach_fut = tokio::task::spawn_local(async move {
                let params = Some(json!({ "session_id": sid, "responder": true }));
                super::ui_bridge::handle_ui_attach(&conn_attach, &params).await
            });
            tokio::task::yield_now().await;
            conn.closed.set(true);
            drop(permit);

            let res = attach_fut.await.unwrap();
            assert!(res.is_err(), "ui.attach on closed connection must fail: {res:?}");
            assert!(!conn.caps.borrow().ui, "closed ui.attach must not set caps.ui = true");
            assert!(rt1.host.answerer.holder().is_none(), "closed ui.attach must not acquire AnswererSlot");

            drop(conn);
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn scope_host_snapshot_rechecks_epoch_across_await() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let p1 = prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("seed s1")).await;
            assert!(p1.get("error").is_none(), "{p1}");
            let e1 = rpc.manager.session(&s1).unwrap().host.kernel.event_log().session_identity().1;

            let dir2 = tempfile::tempdir().unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2_rep = read_reply(&mut cr, 3).await;
            assert!(s2_rep.get("error").is_none(), "{s2_rep}");

            // Calling session.snapshot on s1 while s2 is default/anchor targets s1.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":4,
                    "method":"session.snapshot",
                    "params":{"session_id":s1,"expected_epoch":e1,"name":"targeted-snap"}
                }),
            )
            .await;
            let snap_rep = read_reply(&mut cr, 4).await;
            assert!(snap_rep.get("error").is_none(), "{snap_rep}");
            assert_eq!(snap_rep["result"]["parent_session_id"], s1, "{snap_rep}");

            // Close + reload s1 while session.snapshot is awaiting `ensure_session_ctx` -> must fail with stale_session.
            let conn = rpc
                .connections
                .borrow()
                .values()
                .find_map(|w| w.upgrade())
                .expect("active owner connection");
            let removed_s1 = conn.session_ctxs.borrow_mut().remove(&s1);
            if let Some((w, cid)) = removed_s1
                && let Some(h) = w.upgrade()
            {
                let _ = h.kernel.dispose_ctx(cid).await;
            }
            let rpc_reload = rpc.clone();
            let s1_reload = s1.clone();
            let snap_fut = super::services::call(
                &conn,
                "session",
                "snapshot",
                json!({"session_id": s1, "expected_epoch": e1, "name": "race-snap"}),
            );
            let reload_fut = async move {
                let _ = rpc_reload.manager.close_session(&s1_reload).await;
                rpc_reload.on_session_closed(&s1_reload);
                let opts = rpc_reload.clone_clean_session_opts(None, Some(false)).unwrap();
                let _ = rpc_reload.manager.load_session(Some(&s1_reload), opts).await;
            };
            let (race_res, _) = tokio::join!(snap_fut, reload_fut);
            let err = race_res.expect_err("snapshot across close+reload await must fail with stale_session");
            assert_eq!(err.error_code(), Some("stale_session"), "{err:?}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn generic_contract_registration_scope_all_rollback_release_and_cancel() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let dir3 = tempfile::tempdir().unwrap();

            // Close s1 first so we also test `scope = "all"` when 0 sessions are currently loaded.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.close","params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr, 2).await.get("error").is_none());

            // Both `hooks.subscribe` and `message.register_type` share the unified registration envelope schema.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":21,"method":"services.list","params":{}})).await;
            let catalog = read_reply(&mut cr, 21).await;
            for (svc, method_name) in [("hooks", "subscribe"), ("message", "register_type")] {
                let props = &catalog["result"]["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["name"] == svc)
                    .unwrap()["methods"][method_name]["parameters"]["properties"];
                assert_eq!(props["scope"]["enum"], json!(["all", "session"]), "{svc}.{method_name}: {props}");
                assert_eq!(props["session_id"]["type"], json!("string"), "{svc}.{method_name}: {props}");
            }

            // Register global `hooks.subscribe` and `message.register_type` while 0 sessions exist.
            rpc_send(&mut cw, json!({
                "jsonrpc":"2.0","id":3,"method":"hooks.subscribe",
                "params":{"scope":"all","event":"tool/before","callbacks":["handle"]}
            })).await;
            let hook_reg_rep = read_reply(&mut cr, 3).await;
            assert!(hook_reg_rep.get("error").is_none(), "{hook_reg_rep}");
            let hook_reg_id = hook_reg_rep["result"]["registration_id"].as_str().unwrap().to_string();

            rpc_send(&mut cw, json!({
                "jsonrpc":"2.0","id":4,"method":"message.register_type",
                "params":{"scope":"all","type":"global_note"}
            })).await;
            let msg_reg_rep = read_reply(&mut cr, 4).await;
            assert!(msg_reg_rep.get("error").is_none(), "{msg_reg_rep}");
            let msg_reg_id = msg_reg_rep["result"]["registration_id"].as_str().unwrap().to_string();

            // Create S1 and S2; both receive global_note + gate/check listener with the SAME registration_id.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            let s1_new = read_reply(&mut cr, 5).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            let s2 = read_reply(&mut cr, 6).await["result"]["session_id"].as_str().unwrap().to_string();

            // Register a session-only message type on S1 AND a conflicting one on S2 to test rollback.
            rpc_send(&mut cw, json!({
                "jsonrpc":"2.0","id":7,"method":"message.register_type",
                "params":{"scope":"session","session_id":s1_new,"type":"only_s1"}
            })).await;
            assert!(read_reply(&mut cr, 7).await.get("error").is_none());

            rpc_send(&mut cw, json!({
                "jsonrpc":"2.0","id":8,"method":"commands.register",
                "params":{"scope":"session","session_id":s2,"name":"dup_cmd","description":"conflict","callbacks":["run"]}
            })).await;
            assert!(read_reply(&mut cr, 8).await.get("error").is_none());

            // Scope="all" with duplicate on S2 must fail AND roll back S1 (`dup_cmd` absent on S1).
            rpc_send(&mut cw, json!({
                "jsonrpc":"2.0","id":9,"method":"commands.register",
                "params":{"scope":"all","name":"dup_cmd","description":"conflict","callbacks":["run"]}
            })).await;
            let rollback_rep = read_reply(&mut cr, 9).await;
            assert!(!rollback_rep["error"].is_null(), "expected duplicate rollback error: {rollback_rep}");

            let msg_types = |v: &J| -> HashSet<String> {
                v["data"].as_array().into_iter().flatten()
                    .filter_map(|t| t.get("type").and_then(J::as_str).map(str::to_string))
                    .collect()
            };
            let cmd_names = |v: &J| -> HashSet<String> {
                v["data"].as_array().into_iter().flatten()
                    .filter_map(|t| t.get("name").and_then(J::as_str).map(str::to_string))
                    .collect()
            };
            let rt1 = rpc.manager.session(&s1_new).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let t1 = msg_types(&call_service(&rt1.host, "message", "list_types", vec![]).await.unwrap());
            let t2 = msg_types(&call_service(&rt2.host, "message", "list_types", vec![]).await.unwrap());
            let c1 = cmd_names(&call_service(&rt1.host, "commands", "list", vec![]).await.unwrap());
            let c2 = cmd_names(&call_service(&rt2.host, "commands", "list", vec![]).await.unwrap());
            assert!(t1.contains("global_note") && t1.contains("only_s1"), "{t1:?}");
            assert!(t2.contains("global_note") && !t2.contains("only_s1"), "{t2:?}");
            assert!(!c1.contains("dup_cmd") && c2.contains("dup_cmd"), "c1={c1:?} c2={c2:?}");
            assert!(rt1.host.kernel.voter_count("tool/before") >= 1 && rt2.host.kernel.voter_count("tool/before") >= 1);

            // Close S1; S2 keeps global registrations.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"session.close","params":{"session_id":s1_new}})).await;
            assert!(read_reply(&mut cr, 10).await.get("error").is_none());
            let t2_after_close = msg_types(&call_service(&rt2.host, "message", "list_types", vec![]).await.unwrap());
            assert!(t2_after_close.contains("global_note"), "{t2_after_close:?}");

            // Trigger tool/before vote on S2; callback.call must carry registration_id == hook_reg_id and session_id == s2.
            let host2 = rt2.host.clone();
            let vote_task = tokio::task::spawn_local(async move {
                let lua = host2.lua();
                let ctx = host2.kernel.host_ctx().unwrap();
                lua.globals().set("_gate_ctx", ctx).unwrap();
                lua.load("return _gate_ctx:get('policy'):decide('tool/before', { {} })").eval_async::<mlua::Table>().await
            });
            let cb_req = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" && msg["params"]["registration_id"] == hook_reg_id {
                    break msg;
                }
            };
            assert_eq!(cb_req["params"]["session_id"], s2, "{cb_req}");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":cb_req["id"],"result":{"value":{"decision":"deny","reason":"global voter blocked"}}})).await;
            let vote_res = vote_task.await.unwrap().unwrap();
            assert_eq!(vote_res.get::<String>("decision").unwrap(), "deny");

            // Releasing `msg_reg_id` removes `global_note` from S2 and prevents S3 from getting it.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"registrations.release","params":{"registration_id":msg_reg_id}})).await;
            assert!(read_reply(&mut cr, 11).await.get("error").is_none());
            let t2_after_rel = msg_types(&call_service(&rt2.host, "message", "list_types", vec![]).await.unwrap());
            assert!(!t2_after_rel.contains("global_note"), "{t2_after_rel:?}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":12,"method":"session.create","params":{"cwd":dir3.path().to_string_lossy()}})).await;
            let s3 = read_reply(&mut cr, 12).await["result"]["session_id"].as_str().unwrap().to_string();
            let rt3 = rpc.manager.session(&s3).unwrap();
            let t3 = msg_types(&call_service(&rt3.host, "message", "list_types", vec![]).await.unwrap());
            assert!(!t3.contains("global_note"), "released global registration must not resurrect on S3: {t3:?}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn withdraw_callbacks_releases_tools_and_providers_without_touching_hooks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let dir3 = tempfile::tempdir().unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":3,"method":"tools.register",
                    "params":{"scope":"all","name":"wd_tool","description":"wd probe","parameters":{"type":"object","properties":{}},"side_effect":"read","callbacks":["execute"]}
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":4,"method":"llm.register_provider",
                    "params":{"scope":"all","provider":"wd_prov","default_model":"m1","models":[{"model_id":"m1"}],"callbacks":["stream"]}
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 4).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":5,"method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/before","callbacks":["handle"]}
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());

            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let has_tool = |rt: &Rc<SessionRuntime>, name: &str| {
                let host = rt.host.clone();
                let name = name.to_string();
                async move {
                    let list = call_service(&host, "tools", "list", vec![]).await.unwrap();
                    list.as_array().into_iter().flatten().any(|t| t["name"] == name)
                }
            };
            let has_model = |rt: &Rc<SessionRuntime>, model_ref: &str| {
                let host = rt.host.clone();
                let model_ref = model_ref.to_string();
                async move {
                    let list = call_service(&host, "llm", "list_models", vec![]).await.unwrap();
                    list["data"].as_array().into_iter().flatten().any(|m| m["ref"] == model_ref)
                }
            };

            assert!(has_tool(&rt1, "wd_tool").await && has_tool(&rt2, "wd_tool").await);
            assert!(has_model(&rt1, "wd_prov/m1").await && has_model(&rt2, "wd_prov/m1").await);
            let voters_before_s1 = rt1.host.kernel.voter_count("tool/before");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());

            // Non-hook callback registrations are released across all active sessions immediately.
            assert!(!has_tool(&rt1, "wd_tool").await && !has_tool(&rt2, "wd_tool").await);
            assert!(!has_model(&rt1, "wd_prov/m1").await && !has_model(&rt2, "wd_prov/m1").await);

            // hooks.subscribe remains registered fail-closed.
            assert_eq!(rt1.host.kernel.voter_count("tool/before"), voters_before_s1);
            let lua = rt1.host.lua();
            let ctx = rt1.host.kernel.host_ctx().unwrap();
            lua.globals().set("wd_ctx", ctx).unwrap();
            let vote: mlua::Table =
                lua.load("return wd_ctx:get('policy'):decide('tool/before', { {} })").eval_async().await.unwrap();
            assert_eq!(vote.get::<String>("decision").unwrap(), "deny");

            // Future sessions & re-enabling callbacks must not resurrect withdrawn tools/providers.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 7).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"session.create","params":{"cwd":dir3.path().to_string_lossy()}}),
            )
            .await;
            let s3 = read_reply(&mut cr, 8).await["result"]["session_id"].as_str().unwrap().to_string();
            let rt3 = rpc.manager.session(&s3).unwrap();
            assert!(!has_tool(&rt1, "wd_tool").await && !has_tool(&rt3, "wd_tool").await);
            assert!(!has_model(&rt1, "wd_prov/m1").await && !has_model(&rt3, "wd_prov/m1").await);

            let _ = dir1;
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn llm_register_provider_s2_rollback_restores_s1_and_replace_cleans_global_registrations() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let dir3 = tempfile::tempdir().unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();
            let rt1 = rpc.manager.session(&s1).unwrap();
            let rt2 = rpc.manager.session(&s2).unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":3,"method":"llm.register_provider",
                    "params":{"scope":"all","provider":"stack_prov","default_model":"orig","models":[{"model_id":"orig"}],"callbacks":["stream"]}
                }),
            )
            .await;
            let r1_rep = read_reply(&mut cr, 3).await;
            assert!(r1_rep.get("error").is_none(), "{r1_rep}");
            let r1 = r1_rep["result"]["registration_id"].as_str().unwrap().to_string();

            // Force S2 failure on `replace = true` after S1 has already pushed/installed `v2`.
            rt2.host
                .lua()
                .load(
                    r#"
                    _s2_orig_table_copy = table.copy
                    table.copy = function(t)
                        if type(t) == "table" and t.provider == "stack_prov" and t.replace == true then
                            error("forced s2 replace failure", 0)
                        end
                        return _s2_orig_table_copy(t)
                    end
                    "#,
                )
                .exec()
                .unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":4,"method":"llm.register_provider",
                    "params":{"scope":"all","provider":"stack_prov","replace":true,"default_model":"v2","models":[{"model_id":"v2"}],"callbacks":["stream"]}
                }),
            )
            .await;
            let fail_rep = read_reply(&mut cr, 4).await;
            assert!(!fail_rep["error"].is_null(), "expected S2 install failure: {fail_rep}");

            // S1 rollback must restore `stack_prov/orig` instead of clearing `providers["stack_prov"]` to nil.
            let m1 = call_service(&rt1.host, "llm", "list_models", vec![]).await.unwrap();
            let refs1: HashSet<String> = m1["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["ref"].as_str().map(str::to_string))
                .collect();
            assert!(refs1.contains("stack_prov/orig") && !refs1.contains("stack_prov/v2"), "S1 not restored: {refs1:?}");

            // Clear S2 fault and replace cleanly: old `r1` must be removed from global_registrations.
            rt2.host.lua().load("table.copy = _s2_orig_table_copy").exec().unwrap();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":5,"method":"llm.register_provider",
                    "params":{"scope":"all","provider":"stack_prov","replace":true,"default_model":"v2","models":[{"model_id":"v2"}],"callbacks":["stream"]}
                }),
            )
            .await;
            let r2_rep = read_reply(&mut cr, 5).await;
            assert!(r2_rep.get("error").is_none(), "{r2_rep}");
            let r2 = r2_rep["result"]["registration_id"].as_str().unwrap().to_string();
            assert_ne!(r1, r2);

            // Superseded `r1` must be removed from connection `global_registrations` while `r2` stays registered.
            let (has_r1, has_r2) = {
                let conns = rpc.connections.borrow();
                let conn = conns.values().find_map(std::rc::Weak::upgrade).unwrap();
                let globals = conn.global_registrations.borrow();
                (globals.by_id.contains_key(&r1), globals.by_id.contains_key(&r2))
            };
            assert!(!has_r1 && has_r2, "expected r1 removed and r2 kept: has_r1={has_r1} has_r2={has_r2}");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"registrations.release","params":{"registration_id":r1}})).await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"session.create","params":{"cwd":dir3.path().to_string_lossy()}}),
            )
            .await;
            let s3 = read_reply(&mut cr, 7).await["result"]["session_id"].as_str().unwrap().to_string();
            let rt3 = rpc.manager.session(&s3).unwrap();
            let m3 = call_service(&rt3.host, "llm", "list_models", vec![]).await.unwrap();
            let refs3: HashSet<String> = m3["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["ref"].as_str().map(str::to_string))
                .collect();
            assert!(refs3.contains("stack_prov/v2") && !refs3.contains("stack_prov/orig"), "{refs3:?}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"registrations.release","params":{"registration_id":r2}})).await;
            assert!(read_reply(&mut cr, 8).await.get("error").is_none());
            for rt in [&rt1, &rt2, &rt3] {
                let m = call_service(&rt.host, "llm", "list_models", vec![]).await.unwrap();
                assert!(
                    !m["data"].as_array().into_iter().flatten().any(|x| x["provider"] == "stack_prov"),
                    "{m}"
                );
            }

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn multi_session_release_create_race_builtin_override_empty_models_and_tool_call_delta() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let sample = dir1.path().join("hello.txt");
            std::fs::write(&sample, "REAL_FILE_CONTENT").unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // 1. Omitted `models` and empty `models: []` succeed cleanly.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"llm.register_provider","params":{"provider":"empty_omitted","callbacks":["stream"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 2).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"llm.register_provider","params":{"provider":"empty_arr","models":[],"callbacks":["stream"]}}),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            // 2. Concurrent `registrations.release` and `session.create` must never resurrect released tool on new session.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"tools.register","params":{"scope":"all","name":"race_tool","description":"race","parameters":{"type":"object","properties":{}},"side_effect":"read","callbacks":["execute"]}}),
            )
            .await;
            let race_id = read_reply(&mut cr, 4).await["result"]["registration_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"registrations.release","params":{"registration_id":race_id}})).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            let (mut r5, mut r6) = (None, None);
            while r5.is_none() || r6.is_none() {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 5 {
                    r5 = Some(msg);
                } else if msg["id"] == 6 {
                    r6 = Some(msg);
                }
            }
            let (r5, r6) = (r5.unwrap(), r6.unwrap());
            assert!(r5.get("error").is_none() && r6.get("error").is_none(), "r5={r5} r6={r6}");
            let s2 = r6["result"]["session_id"].as_str().unwrap().to_string();
            let rt2 = rpc.manager.session(&s2).unwrap();
            let s2_tools = call_service(&rt2.host, "tools", "list", vec![]).await.unwrap();
            assert!(
                !s2_tools.as_array().into_iter().flatten().any(|t| t["name"] == "race_tool"),
                "released tool leaked into concurrently created S2: {s2_tools}"
            );

            // 3. Overriding built-in `read` dispatches to callback, and releasing restores built-in `read`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"tools.register","params":{"scope":"session","session_id":s1,"name":"read","description":"custom read","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"side_effect":"read","callbacks":["execute"]}}),
            )
            .await;
            let read_reg = read_reply(&mut cr, 7).await["result"]["registration_id"].as_str().unwrap().to_string();
            let rt1 = rpc.manager.session(&s1).unwrap();
            let host1 = rt1.host.clone();
            let sample_path = sample.to_string_lossy().to_string();
            let mut exec1 = Box::pin(tokio::task::spawn_local(async move {
                let lua = host1.lua();
                let ctx = host1.kernel.host_ctx().unwrap();
                lua.globals().set("_t_ctx1", ctx).unwrap();
                lua.load(format!(
                    "local r = _t_ctx1:get('tools'):execute({{ id='c1', name='read', arguments={{ path={:?} }} }}, {{ on_state = function() return true end }}); return tostring(r.content)",
                    sample_path
                ))
                .eval_async::<String>()
                .await
            }));
            let cb_req = loop {
                tokio::select! {
                    done = &mut exec1 => {
                        panic!("tools:execute finished before callback.call: {done:?}");
                    }
                    msg = rpc_read(&mut cr) => {
                        if msg["method"] == "callback.call" && msg["params"]["registration_id"] == read_reg {
                            break msg;
                        }
                    }
                }
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":cb_req["id"],"result":{"content":"OVERRIDE_READ_OK"}})).await;
            assert_eq!(exec1.await.unwrap().unwrap(), "OVERRIDE_READ_OK");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"registrations.release","params":{"registration_id":read_reg}})).await;
            assert!(read_reply(&mut cr, 8).await.get("error").is_none());
            let host1b = rt1.host.clone();
            let sample_path2 = sample.to_string_lossy().to_string();
            let exec2: String = host1b
                .lua()
                .load(format!(
                    "local r = _t_ctx1:get('tools'):execute({{ id='c2', name='read', arguments={{ path={:?} }} }}, {{ on_state = function() return true end }}); return tostring(r.content)",
                    sample_path2
                ))
                .eval_async()
                .await
                .unwrap();
            assert!(exec2.contains("REAL_FILE_CONTENT"), "restored built-in read failed: {exec2}");

            // 4. `callback.update` with `tool_call_delta` and `tool_call_id` streams item/delta with tool_call_id.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":9,"method":"llm.register_provider",
                    "params":{"scope":"session","session_id":s1,"provider":"delta_prov","default_model":"d1","models":[{"model_id":"d1"}],"callbacks":["stream"]}
                }),
            )
            .await;
            let dreg = read_reply(&mut cr, 9).await["result"]["registration_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"llm.set_model","params":{"session_id":s1,"ref":"delta_prov/d1"}})).await;
            assert!(read_reply(&mut cr, 10).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":methods::EVENTS_SUBSCRIBE,"params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr, 11).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":12,"method":"agent.prompt","params":{"session_id":s1,"input":"delta go"}})).await;
            assert!(read_reply(&mut cr, 12).await.get("error").is_none());

            let mut saw_tc_delta = false;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" && msg["params"]["registration_id"] == dreg {
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","method":"callback.update","params":{"id":msg["id"],"partial":{"type":"tool_call_start","tool_call_id":"tc-upd-1","name":"read"}}}),
                    )
                    .await;
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","method":"callback.update","params":{"id":msg["id"],"partial":{"type":"tool_call_delta","tool_call_id":"tc-upd-1","delta":"{\"path\":\"x\"}"}}}),
                    )
                    .await;
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"role":"assistant","content":[{"type":"text","text":"delta-done"}]}}),
                    )
                    .await;
                } else if msg["method"] == "event"
                    && msg["params"]["type"] == "item/delta"
                    && msg["params"]["data"]["kind"] == "tool_arguments"
                {
                    assert_eq!(msg["params"]["data"]["tool_call_id"], "tc-upd-1", "{msg}");
                    assert_eq!(msg["params"]["data"]["delta"], "{\"path\":\"x\"}", "{msg}");
                    saw_tc_delta = true;
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/failed" {
                    panic!("turn failed unexpectedly in delta test: {msg}");
                } else if msg["method"] == "stream.failed" {
                    panic!("stream failed unexpectedly in delta test: {msg}");
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/completed" {
                    break;
                }
            }
            assert!(saw_tc_delta, "tool_call_delta with tool_call_id was not observed via callback.update");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn multi_session_side_isolation_turn_cancel_idempotency_continue_and_missing_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) =
                setup_manager_rpc_mode(mare_plugin::Mode::Rpc).await;
            let dir2 = tempfile::tempdir().unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":methods::CONNECTION_SET_CAPABILITIES,"params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // `agent.get` without `session_id` fails -32602.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"agent.get","params":{}})).await;
            let missing_sid = read_reply(&mut cr, 2).await;
            assert_eq!(missing_sid["error"]["code"], codes::INVALID_PARAMS, "{missing_sid}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s2 = read_reply(&mut cr, 3).await["result"]["session_id"].as_str().unwrap().to_string();

            // Register controllable `stream` providers per session so side turns stay open deterministically.
            for (id, sid, pname) in [(4, &s1, "sidep1"), (5, &s2, "sidep2")] {
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc":"2.0","id":id,"method":"llm.register_provider",
                        "params":{"scope":"session","session_id":sid,"provider":pname,"default_model":"m","models":[{"model_id":"m"}],"callbacks":["stream"]}
                    }),
                )
                .await;
                assert!(read_reply(&mut cr, id).await.get("error").is_none());
                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":id + 10,"method":"llm.set_model","params":{"session_id":sid,"ref":format!("{pname}/m")}}),
                )
                .await;
                assert!(read_reply(&mut cr, id + 10).await.get("error").is_none());
            }

            // Seed one main turn in each session so `/side` can open on both S1 and S2.
            for (idx, sid) in [(20, &s1), (21, &s2)] {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":idx,"method":"agent.prompt","params":{"session_id":sid,"input":"seed"}})).await;
                let seed_rep = read_reply(&mut cr, idx).await;
                assert!(seed_rep.get("error").is_none(), "{seed_rep}");
                let seed_tid = seed_rep["result"]["turn_id"].as_str().unwrap().to_string();
                let cb = loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" && msg["params"]["callback"] == "stream" {
                        break msg;
                    }
                };
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":cb["id"],"result":{"role":"assistant","content":[{"type":"text","text":"seeded"}]}})).await;
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":idx + 10,"method":"agent.wait","params":{"session_id":sid,"turn_id":seed_tid}})).await;
                let waited = read_reply(&mut cr, idx + 10).await;
                assert_eq!(waited["result"]["status"], "completed", "{waited}");

                let side_host = rpc.manager.session(sid).unwrap().host.clone();
                let side_info = call_service(&side_host, "conversations", "start", vec![]).await.unwrap();
                assert_eq!(side_info["selected"], true, "failed to open side on {sid}: {side_info}");
            }

            // On focused S1 side: `agent.continue` returns `agent.rejected`, stale `expected_epoch` returns `stale_session`,
            // and `request_id` enforces replay & conflict before routing into side.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":100,"method":"agent.continue","params":{"session_id":s1}})).await;
            let cont_err = read_reply(&mut cr, 100).await;
            assert_eq!(cont_err["error"]["data"]["code"], "agent.rejected", "{cont_err}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":101,"method":"agent.prompt","params":{"session_id":s1,"expected_epoch":"wrong-epoch","input":"stale"}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 101).await["error"]["data"]["code"], "stale_session");

            let mut cb_s1: Option<J> = None;
            async fn read_reply_keep_cb(
                cr: &mut (impl AsyncBufReadExt + Unpin),
                id: i64,
                sid: &str,
                cb: &mut Option<J>,
            ) -> J {
                loop {
                    let msg = rpc_read(cr).await;
                    if msg["id"] == id {
                        return msg;
                    }
                    if cb.is_none() && msg["method"] == "callback.call" && msg["params"]["session_id"] == sid {
                        *cb = Some(msg);
                    }
                }
            }

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":102,"method":"agent.prompt","params":{"session_id":s1,"request_id":"side-rid-1","input":"side-q1"}}),
            )
            .await;
            let sp1 = read_reply_keep_cb(&mut cr, 102, &s1, &mut cb_s1).await;
            assert!(sp1.get("error").is_none(), "{sp1}");
            let side_t1 = sp1["result"]["turn_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":103,"method":"agent.prompt","params":{"session_id":s1,"request_id":"side-rid-1","input":"side-q1"}}),
            )
            .await;
            let dup = read_reply_keep_cb(&mut cr, 103, &s1, &mut cb_s1).await;
            assert_eq!(dup["result"]["duplicate"], true, "{dup}");
            assert_eq!(dup["result"]["turn_id"], side_t1, "{dup}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":104,"method":"agent.prompt","params":{"session_id":s1,"request_id":"side-rid-1","input":"different"}}),
            )
            .await;
            assert_eq!(
                read_reply_keep_cb(&mut cr, 104, &s1, &mut cb_s1).await["error"]["data"]["code"],
                "request_id_conflict"
            );

            // Wait until S1 side turn 1 hits stream callback, then enqueue side turn 2 on S1.
            let cb_s1 = match cb_s1 {
                Some(msg) => msg,
                None => tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let msg = rpc_read(&mut cr).await;
                        if msg["method"] == "callback.call" && msg["params"]["session_id"] == s1 {
                            break msg;
                        }
                    }
                })
                .await
                .expect("waiting for cb_s1"),
            };
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":105,"method":"agent.prompt","params":{"session_id":s1,"input":"side-q2"}}),
            )
            .await;
            let sp2 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 105))
                .await
                .expect("waiting for sp2");
            assert_eq!(sp2["result"]["status"], "queued", "{sp2}");
            let side_t2 = sp2["result"]["turn_id"].as_str().unwrap().to_string();

            // S2 remains idle/isolated while S1 side has active turn + queued turn.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":106,"method":"agent.get","params":{"session_id":s1}})).await;
            let g1 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 106))
                .await
                .expect("waiting for g1");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":107,"method":"agent.get","params":{"session_id":s2}})).await;
            let g2 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 107))
                .await
                .expect("waiting for g2");
            assert_eq!(g1["result"]["busy"], true, "{g1}");
            assert_eq!(g1["result"]["queued"], 1, "{g1}");
            assert_eq!(g2["result"]["busy"], false, "{g2}");

            // Park S1 side (`not active()`), then cancel queued `side_t2` and running `side_t1` by `turn_id`!
            let rt1_side = rpc.manager.session(&s1).unwrap();
            let toggled = call_service(&rt1_side.host, "conversations", "toggle", vec![])
                .await
                .unwrap();
            assert_eq!(toggled["selected"], false, "{toggled}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":109,"method":"agent.cancel","params":{"session_id":s1,"turn_id":side_t2}}),
            )
            .await;
            let can2 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 109))
                .await
                .expect("waiting for can2");
            assert_eq!(can2["result"]["turn_id"], side_t2, "{can2}");
            assert_eq!(can2["result"]["removed"].as_array().unwrap().len(), 1, "{can2}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":110,"method":"agent.cancel","params":{"session_id":s1,"turn_id":side_t1}}),
            )
            .await;
            let (mut can1, mut saw_cancel_req) = (None, false);
            tokio::time::timeout(Duration::from_secs(4), async {
                while can1.is_none() || !saw_cancel_req {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == 110 {
                        can1 = Some(msg);
                    } else if msg["method"] == "$/cancel_request" && msg["params"]["id"] == cb_s1["id"] {
                        saw_cancel_req = true;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("waiting for can1 ({}) / cancel_req ({saw_cancel_req})", can1.is_some()));
            let can1 = can1.unwrap();
            assert_eq!(can1["result"]["turn_id"], side_t1, "{can1}");
            assert!(saw_cancel_req);

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":111,"method":"agent.wait","params":{"session_id":s1,"turn_id":side_t1}})).await;
            let wait1 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 111))
                .await
                .expect("waiting for wait1");
            assert_eq!(wait1["result"]["status"], "cancelled", "{wait1}");

            // S2's side conversation still works independently.
            let mut cb_s2: Option<J> = None;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":112,"method":"agent.prompt","params":{"session_id":s2,"input":"s2-side"}})).await;
            let s2_turn = tokio::time::timeout(Duration::from_secs(4), read_reply_keep_cb(&mut cr, 112, &s2, &mut cb_s2))
                .await
                .expect("waiting for s2_turn")["result"]["turn_id"]
                .as_str()
                .unwrap()
                .to_string();
            let cb_s2 = match cb_s2 {
                Some(msg) => msg,
                None => tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let msg = rpc_read(&mut cr).await;
                        if msg["method"] == "callback.call" && msg["params"]["session_id"] == s2 {
                            break msg;
                        }
                    }
                })
                .await
                .expect("waiting for cb_s2"),
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":cb_s2["id"],"result":{"role":"assistant","content":[{"type":"text","text":"s2-side-ok"}]}})).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":113,"method":"agent.wait","params":{"session_id":s2,"turn_id":s2_turn}})).await;
            let wait2 = tokio::time::timeout(Duration::from_secs(4), read_reply(&mut cr, 113))
                .await
                .expect("waiting for wait2");
            assert_eq!(wait2["result"]["status"], "completed", "{wait2}");
            assert_eq!(wait2["result"]["text"], "s2-side-ok", "{wait2}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn peer_session_single_host_five_restrictions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir1 = tempfile::tempdir().unwrap();
            let dir2 = tempfile::tempdir().unwrap();

            // Create and persist a session in another project (`dir2`) while primary owns `dir2`.
            let opts2 = mare_host::HostOptions {
                cwd: dir2.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let h2 = mare_host::Host::new(opts2).await.unwrap();
            let (oc, os) = tokio::io::duplex(65536);
            let (osr, osw) = tokio::io::split(os);
            let t2 = tokio::task::spawn_local(super::serve::serve(h2.clone(), osr, osw, true, std::future::pending()));
            let (ocr, mut ocw) = tokio::io::split(oc);
            let mut ocr = BufReader::new(ocr);
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            let init2 = read_reply(&mut ocr, 1).await;
            assert!(init2.get("error").is_none(), "{init2}");
            let s_other_id = init2["result"]["session"]["session_id"].as_str().unwrap().to_string();
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":2,"method":"session.get","params":{"session_id":s_other_id}})).await;
            let s_other_snap = read_reply(&mut ocr, 2).await;
            assert!(s_other_snap.get("error").is_none(), "h2 session.get failed: {s_other_snap}");
            let s_other_path = s_other_snap["result"]["path"].as_str().unwrap().to_string();
            assert!(prompt_and_wait(&mut ocr, &mut ocw, 3, &s_other_id, json!("other project msg")).await.get("error").is_none());
            drop(ocw);
            drop(ocr);
            tokio::time::timeout(Duration::from_secs(5), t2).await.unwrap().unwrap();
            h2.shutdown().await;

            // Start single-host server in `dir1` with peer permissions excluding `llm.set_model`,
            // `llm.set_effort`, `session.update`, and `session.list` (tested first for list hiding).
            let allow_no_list_no_llm = json!([
                "session.create",
                "session.load",
                "session.get",
                "session.fork",
                "options.get",
                "agent.prompt",
                "agent.wait"
            ]);
            let opts1 = mare_host::HostOptions {
                cwd: dir1.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": allow_no_list_no_llm },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let host = mare_host::Host::new(opts1).await.unwrap();
            let (c1, s1_io) = tokio::io::duplex(65536);
            let (s1r, s1w) = tokio::io::split(s1_io);
            let primary_task = tokio::task::spawn_local(super::serve::serve(host.clone(), s1r, s1w, true, std::future::pending()));
            let (cr1, mut cw1) = tokio::io::split(c1);
            let mut cr1 = BufReader::new(cr1);
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            let init1 = read_reply(&mut cr1, 1).await;
            assert!(init1.get("error").is_none(), "{init1}");
            let s_a = init1["result"]["session"]["session_id"].as_str().unwrap().to_string();

            // Persist three same-project sessions: `s_a` (archived), `s_b` (`dup`, `faux/echo2`), `s_c` (`dup`, `faux/echo`).
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"session.get","params":{"session_id":s_a}})).await;
            let s_a_snap = read_reply(&mut cr1, 2).await;
            assert!(s_a_snap.get("error").is_none(), "s_a session.get failed: {s_a_snap}");
            let s_a_path = s_a_snap["result"]["path"].as_str().unwrap().to_string();
            assert!(prompt_and_wait(&mut cr1, &mut cw1, 3, &s_a, json!("in dir1 a")).await.get("error").is_none());

            // Create s_b in dir1, switch model to faux/echo2, persist turn, then switch to s_c with faux/echo.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{"name":"dup"}})).await;
            let s_b_snap = read_reply(&mut cr1, 4).await;
            assert!(s_b_snap.get("error").is_none(), "s_b session.create failed: {s_b_snap}");
            let s_b = s_b_snap["result"]["session_id"].as_str().unwrap().to_string();
            let s_b_path = s_b_snap["result"]["path"].as_str().unwrap().to_string();
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":5,"method":"options.set","params":{"session_id":s_b,"values":{"model":"faux/echo2"}}})).await;
            assert!(read_reply(&mut cr1, 5).await.get("error").is_none());
            assert!(prompt_and_wait(&mut cr1, &mut cw1, 6, &s_b, json!("echo2 turn")).await.get("error").is_none());

            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":7,"method":"session.create","params":{"name":"dup"}})).await;
            let s_c = read_reply(&mut cr1, 7).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":8,"method":"options.set","params":{"session_id":s_c,"values":{"model":"faux/echo"}}})).await;
            assert!(read_reply(&mut cr1, 8).await.get("error").is_none());
            assert!(prompt_and_wait(&mut cr1, &mut cw1, 9, &s_c, json!("echo1 turn")).await.get("error").is_none());

            // Archive `s_a` so ambiguous/archived selector masking without `session.list` is also testable.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":14,"method":"session.archive","params":{"session_id":s_a}})).await;
            assert!(read_reply(&mut cr1, 14).await.get("error").is_none());

            // Connect restricted peer via single-host `serve_peer`.
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer(host.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // Restriction 1 (`lifecycle.lua:484,512`): session.load by path or path-like selector refused,
            // ambiguous title hides candidate IDs when `session.list` is not granted, archived hides short id.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":20,"method":"session.load","params":{"path":s_other_path}})).await;
            let load_by_path = read_reply(&mut pcr, 20).await;
            assert!(load_by_path.get("error").is_some(), "peer session.load by path must fail: {load_by_path}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":21,"method":"session.load","params":{"path":s_b_path}})).await;
            let load_same_path = read_reply(&mut pcr, 21).await;
            assert!(load_same_path.get("error").is_some(), "peer session.load by same-project path must fail: {load_same_path}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":22,"method":"session.load","params":{"session_id":s_other_path}})).await;
            let load_pathlike = read_reply(&mut pcr, 22).await;
            assert_eq!(load_pathlike["error"]["data"]["code"], "invalid_arguments", "{load_pathlike}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":23,"method":"session.load","params":{"session_id":s_other_id}})).await;
            let load_other_id = read_reply(&mut pcr, 23).await;
            assert_eq!(load_other_id["error"]["data"]["code"], "invalid_arguments", "{load_other_id}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":24,"method":"session.load","params":{"session_id":"dup"}})).await;
            let load_dup = read_reply(&mut pcr, 24).await;
            let dup_msg = load_dup["error"]["message"].as_str().unwrap_or("");
            assert!(dup_msg.contains("matches more than one session") && !dup_msg.contains(&s_b[..8]), "ambiguous selector without session.list must hide candidates: {load_dup}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":25,"method":"session.load","params":{"session_id":&s_a[..18]}})).await;
            let load_arch = read_reply(&mut pcr, 25).await;
            let arch_msg = load_arch["error"]["message"].as_str().unwrap_or("");
            assert!(arch_msg.contains("that session is archived") && !arch_msg.contains(&s_a[..8]), "archived selector without session.list must hide short id: {load_arch}");

            // Restriction 2 (`lifecycle.lua:656`): session.create with outside cwd denied.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":26,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            let create_other = read_reply(&mut pcr, 26).await;
            assert_eq!(create_other["error"]["data"]["code"], "denied", "single-host peer session.create out of project must deny: {create_other}");

            // Restriction 3 (`lifecycle.lua:53-70`): D.authorize_settings refuses `session.load` when
            // `llm.set_model` is not permitted, leaving active session & model unchanged. (`session.fork`
            // is not granted in `peer.allow`, so RPC admission rejects it before `fork.lua:491`.)
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":27,"method":"session.load","params":{"session_id":s_b}})).await;
            let load_model_denied = read_reply(&mut pcr, 27).await;
            assert_eq!(load_model_denied["error"]["data"]["code"], "denied", "session.load switching model without llm.set_model must deny: {load_model_denied}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":29,"method":"session.fork","params":{"session_id":s_b}})).await;
            let fork_ungranted_denied = read_reply(&mut pcr, 29).await;
            assert_eq!(fork_ungranted_denied["error"]["data"]["code"], "denied", "ungranted session.fork must deny peer at RPC entry: {fork_ungranted_denied}");

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":30,"method":"session.get","params":{"session_id":s_c}})).await;
            let active_after = read_reply(&mut pcr, 30).await;
            assert_eq!(active_after["result"]["session_id"], s_c, "refused switches must keep active session unchanged: {active_after}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":31,"method":"options.get","params":{"session_id":s_c,"keys":["model"]}})).await;
            assert_eq!(read_reply(&mut pcr, 31).await["result"]["model"], "faux/echo");

            // Restriction 4 (`queries.lua:195`): session.get refuses cross-project id, path, and active S.path.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":32,"method":"session.get","params":{"session_id":s_other_id}})).await;
            assert_eq!(read_reply(&mut pcr, 32).await["error"]["data"]["code"], "unknown_session");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":33,"method":"session.get","params":{"session_id":s_other_path}})).await;
            assert_eq!(read_reply(&mut pcr, 33).await["error"]["data"]["code"], "unknown_session");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":34,"method":"session.get","params":{"session_id":s_a_path}})).await;
            assert_eq!(read_reply(&mut pcr, 34).await["error"]["data"]["code"], "unknown_session");
            // Same-project session.get works cleanly.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":35,"method":"session.get","params":{"session_id":s_b}})).await;
            let get_same = read_reply(&mut pcr, 35).await;
            assert_eq!(get_same["result"]["session_id"], s_b, "{get_same}");

            // Restriction 5 (`queries.lua:329`): test `session.list` and granted `llm.set_model` on a fresh host in `dir1`.
            drop(pcw);
            drop(pcr);
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            drop(cw1);
            drop(cr1);
            tokio::time::timeout(Duration::from_secs(5), primary_task).await.unwrap().unwrap();
            host.shutdown().await;

            let opts1b = mare_host::HostOptions {
                cwd: dir1.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.list", "session.load", "session.get", "llm.set_model", "llm.set_effort"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let host2 = mare_host::Host::new(opts1b).await.unwrap();
            let (pc2, ps2) = tokio::io::duplex(65536);
            let (ps2r, ps2w) = tokio::io::split(ps2);
            let peer_task2 = tokio::task::spawn_local(super::serve::serve_peer(host2.clone(), ps2r, ps2w, None));
            let (pc2r, mut pc2w) = tokio::io::split(pc2);
            let mut pc2r = BufReader::new(pc2r);
            rpc_send(&mut pc2w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            assert!(read_reply(&mut pc2r, 1).await.get("error").is_none());

            rpc_send(&mut pc2w, json!({"jsonrpc":"2.0","id":40,"method":"session.list","params":{"all":true}})).await;
            assert_eq!(read_reply(&mut pc2r, 40).await["error"]["data"]["code"], "denied");
            rpc_send(&mut pc2w, json!({"jsonrpc":"2.0","id":41,"method":"session.list","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            assert_eq!(read_reply(&mut pc2r, 41).await["error"]["data"]["code"], "denied");
            rpc_send(&mut pc2w, json!({"jsonrpc":"2.0","id":42,"method":"session.list","params":{}})).await;
            let list_ok = read_reply(&mut pc2r, 42).await;
            let listed_ids: Vec<&str> = list_ok["result"]["data"].as_array().unwrap().iter().filter_map(|r| r["session_id"].as_str()).collect();
            assert!(!listed_ids.contains(&s_other_id.as_str()), "peer session.list must omit other project: {list_ok}");
            assert!(listed_ids.contains(&s_c.as_str()), "peer session.list must include current project: {list_ok}");

            // With llm.set_model + llm.set_effort granted, loading s_b (which switches model to faux/echo2) succeeds.
            rpc_send(&mut pc2w, json!({"jsonrpc":"2.0","id":43,"method":"session.load","params":{"session_id":s_b}})).await;
            let load_ok = read_reply(&mut pc2r, 43).await;
            assert!(load_ok.get("error").is_none(), "granted peer session.load failed: {load_ok}");
            assert_eq!(load_ok["result"]["session_id"], s_b);

            drop(pc2w);
            drop(pc2r);
            tokio::time::timeout(Duration::from_secs(5), peer_task2).await.unwrap().unwrap();
            host2.shutdown().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn peer_session_multi_host_target_restrictions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "peer": {
                        "allow": [
                            "session.create",
                            "session.load",
                            "session.get",
                            "session.list",
                            "session.list_items",
                            "session.list_records",
                            "options.get"
                        ]
                    },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                json!({}),
                true,
            )
            .await;
            let dir2 = tempfile::tempdir().unwrap();

            // Create same-project second Host session `s_same2` and cross-project Host session `s_other`.
            assert!(prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("s1 turn")).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{}})).await;
            let s_same2 = read_reply(&mut cr, 3).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            let s_other_snap = read_reply(&mut cr, 4).await;
            let s_other = s_other_snap["result"]["session_id"].as_str().unwrap().to_string();
            let s_other_path = s_other_snap["result"]["path"].as_str().unwrap().to_string();
            assert!(prompt_and_wait(&mut cr, &mut cw, 5, &s_other, json!("other turn")).await.get("error").is_none());

            // Connect restricted peer to manager RPC (anchor host in dir1).
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // Cross-project live session on another Host (`s_other`) MUST be rejected with `unknown_session`
            // on every peer-grantable session method (`session.get`, `session.list_items`, `session.list_records`).
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":20,"method":"session.get","params":{"session_id":s_other}})).await;
            let get_live_other = read_reply(&mut pcr, 20).await;
            assert_eq!(get_live_other["error"]["data"]["code"], "unknown_session", "multi-host peer session.get must reject cross-project target Host: {get_live_other}");

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":21,"method":"session.list_items","params":{"session_id":s_other}})).await;
            let items_other = read_reply(&mut pcr, 21).await;
            assert_eq!(items_other["error"]["data"]["code"], "unknown_session", "multi-host peer session.list_items on cross-project Host must return unknown_session: {items_other}");

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":22,"method":"session.list_records","params":{"session_id":s_other,"kind":"notes"}})).await;
            let records_other = read_reply(&mut pcr, 22).await;
            assert_eq!(records_other["error"]["data"]["code"], "unknown_session", "multi-host peer session.list_records on cross-project Host must return unknown_session: {records_other}");

            // Same-project session on secondary Host (`s_same2`) IS reachable via session.get and session.list_items.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":23,"method":"session.get","params":{"session_id":s_same2}})).await;
            let get_same2 = read_reply(&mut pcr, 23).await;
            assert_eq!(get_same2["result"]["session_id"], s_same2, "{get_same2}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":29,"method":"session.list_items","params":{"session_id":s_same2}})).await;
            let items_same2 = read_reply(&mut pcr, 29).await;
            assert!(items_same2.get("error").is_none(), "same-project second Host session.list_items must succeed: {items_same2}");

            // Close `s_other` (disk-only) -> session.get, session.list_items, session.load by path still refused!
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.close","params":{"session_id":s_other}})).await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":24,"method":"session.get","params":{"session_id":s_other}})).await;
            assert_eq!(read_reply(&mut pcr, 24).await["error"]["data"]["code"], "unknown_session");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":30,"method":"session.list_items","params":{"session_id":s_other}})).await;
            assert_eq!(read_reply(&mut pcr, 30).await["error"]["data"]["code"], "unknown_session");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":25,"method":"session.load","params":{"path":s_other_path}})).await;
            assert!(read_reply(&mut pcr, 25).await.get("error").is_some());
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":26,"method":"session.list","params":{"all":true}})).await;
            assert_eq!(read_reply(&mut pcr, 26).await["error"]["data"]["code"], "denied");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":27,"method":"session.list","params":{"cwd":dir2.path().to_string_lossy()}})).await;
            assert_eq!(read_reply(&mut pcr, 27).await["error"]["data"]["code"], "denied");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":28,"method":"session.list","params":{}})).await;
            let list_rep = read_reply(&mut pcr, 28).await;
            let ids: Vec<&str> = list_rep["result"]["data"].as_array().unwrap().iter().filter_map(|r| r["session_id"].as_str()).collect();
            assert!(!ids.contains(&s_other.as_str()), "multi-host list must exclude other project: {list_rep}");
            assert!(ids.contains(&s1.as_str()), "multi-host list must include same project: {list_rep}");

            drop(pcw);
            drop(pcr);
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_owner_runtime_remote_admin_toggle_updates_permitted_for_caller() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
            std::fs::write(
                home.path().join("config/plugins/admin-probe.lua"),
                r#"return {
    name = "admin-probe",
    provide = { "admin-probe" },
    contracts = {
        ["admin-probe"] = {
            methods = {
                check_logout = {
                    doc = "Return whether llm.logout is permitted for caller",
                    expose = "both",
                    access = "peer_default",
                    scope = "session",
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = { type = "object", properties = { allowed = { type = "boolean" } }, required = { "allowed" } },
                },
            },
        },
    },
    apply = function(ctx)
        local Probe = require("cordis").class("AdminProbe", mare.Service)
        function Probe:constructor(c)
            Probe.super.constructor(self, c, "admin-probe")
        end
        function Probe:check_logout()
            return { allowed = mare.contracts.permitted("llm", "logout", self.ctx) }
        end
        ctx:plugin(Probe)
    end,
}
"#,
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "webserver": { "remote_admin": true },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            assert!(rt.host.kernel.problems().is_empty(), "host problems: {:?}", rt.host.kernel.problems());
            let s1 = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(s1.clone()), false);

            // Connect a non-admin remote owner (`Principal::Owner { remote_admin: false }`) while
            // `webserver.remote_admin` is `true`, so `conn.session_ctxs[s1]` caches `ctx_id`.
            let (rc, rs) = tokio::io::duplex(65536);
            let (rsr, rsw) = tokio::io::split(rs);
            let remote_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                super::Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(rc);
            let mut rcr = BufReader::new(rcr);
            rpc_send(&mut rcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}})).await;
            assert!(read_reply(&mut rcr, 1).await.get("error").is_none());

            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":2,"method":"admin-probe.check_logout","params":{"session_id":s1}}),
            )
            .await;
            let before = read_reply(&mut rcr, 2).await;
            assert_eq!(before["result"]["allowed"], true, "remote_admin=true should allow llm.logout: {before}");

            // Toggle `webserver.remote_admin` to `false` at runtime on `rt.host.kernel`.
            let mut updated_cfg = rt.host.kernel.config();
            updated_cfg["webserver"]["remote_admin"] = json!(false);
            rt.host.kernel.set_config(updated_cfg);

            // Same remote connection (reusing cached `ctx_id`) must see `permitted_for_caller` drop to `false` synchronously.
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":3,"method":"admin-probe.check_logout","params":{"session_id":s1}}),
            )
            .await;
            let after = read_reply(&mut rcr, 3).await;
            assert_eq!(
                after["result"]["allowed"], false,
                "disabling webserver.remote_admin must revoke cached remote owner ctx_id permissions: {after}"
            );

            drop(rcw);
            drop(rcr);
            tokio::time::timeout(Duration::from_secs(5), remote_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn rpc_load_locked_session_cancelled_select_returns_cancelled_not_session_id_mismatch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let faux_path = home.path().join("faux.json");
            std::fs::write(&faux_path, json!([{ "text": "owner idle" }]).to_string()).unwrap();
            let _faux_guard = EnvGuard::set("MARE_FAUX_SCRIPT", &faux_path);

            let owner_opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let owner_mgr = mare_host::HostManager::new();
            let owner_rt = owner_mgr.create_session(owner_opts.clone()).await.unwrap();
            let owner_sid = owner_rt.session_id.clone();
            // Run and persist a turn on `owner_rt` so the session is indexed on disk while holding the lock.
            let owner_started =
                call_service(&owner_rt.host, "agent", "prompt", vec![json!("create locked session")]).await.unwrap();
            let owner_turn_id = owner_started["turn_id"].as_str().unwrap().to_owned();
            let _ = call_service(&owner_rt.host, "agent", "wait", vec![json!(owner_turn_id)]).await.unwrap();

            let reader_mgr = mare_host::HostManager::new();
            let reader_rt = reader_mgr.create_session(owner_opts.clone()).await.unwrap();
            let reader_sid = reader_rt.session_id.clone();
            let reader_rpc = RpcRuntime::new(reader_mgr, owner_opts, reader_rt.host.clone(), Some(reader_sid), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task =
                tokio::task::spawn_local(serve_manager(reader_rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": "3.0",
                        "capabilities": { "ui": true, "callbacks": true }
                    }
                }),
            )
            .await;
            let init = read_reply(&mut cr, 1).await;
            assert!(init.get("error").is_none(), "{init}");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": { "session_id": owner_sid }
                }),
            )
            .await;

            let mut saw_view_readonly = false;
            let load_reply = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "ui.select" {
                    let items = msg["params"]["items"].as_array().cloned().unwrap_or_default();
                    if items.iter().any(|i| i["label"] == "View read-only") {
                        saw_view_readonly = true;
                    }
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "index": null } })).await;
                } else if msg["id"] == 2 {
                    break msg;
                }
            };
            assert!(saw_view_readonly, "locked session.load must present View read-only ui.select");
            assert_eq!(
                load_reply["error"]["data"]["code"], "cancelled",
                "cancelling View read-only select must return cancelled, not invalid_arguments/mismatch: {load_reply}"
            );
            assert!(
                load_reply["error"]["message"].as_str().unwrap_or_default().contains("session switch cancelled"),
                "unexpected error message: {load_reply}"
            );
            assert!(reader_rpc.manager.session(&owner_sid).is_err(), "failed load must not stay registered");
            assert!(!reader_rpc.manager.is_loading(&owner_sid), "failed load must release loading reservation");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = reader_rpc.manager.shutdown_all().await;
            let _ = owner_mgr.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn approval_started_before_transition_is_not_sent_for_new_session() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, _dir, _guard, rpc, cr, cw, task, old_sid) = setup_manager_rpc().await;
        let rt = rpc.manager.session(&old_sid).unwrap();
        struct GateReset;
        impl Drop for GateReset {
            fn drop(&mut self) { super::forward_approvals::tests::clear_flush_gate(); }
        }
        let _reset = GateReset;
        let (entered, release) = super::forward_approvals::tests::arm_flush_gate();
        let host = rt.host.clone();
        let approval = tokio::task::spawn_local(async move {
            let arg = json_to_lua(host.lua(), &json!({"host":"example.com","port":443})).unwrap();
            host.kernel.policy("net/approval-host", MultiValue::from_vec(vec![arg])).await
        });
        tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
        let svc = rt.host.service_table("session").unwrap();
        rt.host.lua().globals().set("TRANSITION_SVC", svc).unwrap();
        rt.host.lua().load(r#"local i=TRANSITION_SVC:info(); return TRANSITION_SVC:transition({mode='new'}, {session_id=i.session_id, session_epoch=i.session_epoch})"#).call_async::<mlua::Table>(()).await.unwrap();
        assert!(rpc.manager.active_ids().iter().any(|sid| sid != &old_sid));
        let _ = release.send(());
        let result = tokio::time::timeout(Duration::from_secs(2), approval).await.expect("old-session approval must fail closed").unwrap().unwrap();
        assert_eq!(result.get::<String>("decision").unwrap(), "deny");
        drop((cw, cr));
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

#[tokio::test]
async fn transition_new_rekeys_registrations_and_elicitation_pending() {
    tokio::task::LocalSet::new().run_until(async {
        let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, old_sid) = setup_manager_rpc().await;
        let rt = rpc.manager.session(&old_sid).unwrap();
        let conn = rpc.connections.borrow().values().find_map(Weak::upgrade).unwrap();
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"message.register_type","params":{"scope":"session","session_id":old_sid,"type":"rekey_test"}})).await;
        let registered = read_reply(&mut cr, 2).await;
        let registration_id = registered["result"]["registration_id"].as_str().expect("registration succeeds").to_owned();
        let svc = rt.host.service_table("session").unwrap();
        rt.host.lua().globals().set("TRANSITION_SVC", svc).unwrap();
        rt.host.lua().load(r#"local i = TRANSITION_SVC:info(); return TRANSITION_SVC:transition({mode="new"}, {session_id=i.session_id, session_epoch=i.session_epoch})"#).call_async::<mlua::Table>(()).await.unwrap();
        let new_sid = rpc.manager.active_ids().into_iter().find(|sid| sid != &old_sid).unwrap();
        {
            let regs = conn.global_registrations.borrow();
            let rec = &regs.by_id[&registration_id];
            assert!(matches!(&rec.scope, ResolvedRegistrationScope::Session(s) if s == &new_sid));
            assert_eq!(rec.active_session_instances.iter().collect::<Vec<_>>(), vec![&new_sid]);
        }
        conn.global_registrations.borrow_mut().on_session_closed(&old_sid);
        assert!(conn.global_registrations.borrow().by_id.contains_key(&registration_id));
        let host = rt.host.clone();
        let elicit = tokio::task::spawn_local(async move { host.bail_json("mcp/elicit", json!({"server":"srv","params":{"message":"ask"}})).await });
        let request = loop {
            let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
            if m["method"] == "mcp.elicitation" { break m; }
        };
        let id = request["id"].as_str().unwrap();
        assert_eq!(conn.pending.borrow().get(id).unwrap().session_id.as_deref(), Some(new_sid.as_str()));
        let host = rt.host.clone();
        let approval = tokio::task::spawn_local(async move {
            let arg = json_to_lua(host.lua(), &json!({"host":"example.com","port":443})).unwrap();
            host.kernel.policy("net/approval-host", MultiValue::from_vec(vec![arg])).await
        });
        let request = loop {
            let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
            if m["method"] == "approval.request" { break m; }
        };
        let id = request["id"].as_str().unwrap();
        assert_eq!(conn.pending.borrow().get(id).unwrap().session_id.as_deref(), Some(new_sid.as_str()));
        rt.host.answerer.acquire(rt.host.lua(), mare_host::ConnId(999_999), Box::new(|_| {}));
        tokio::time::timeout(Duration::from_secs(2), elicit).await.expect("elicitation must end on takeover").unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), approval).await.expect("approval must end on takeover").unwrap().unwrap();
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"registrations.release","params":{"registration_id":registration_id}})).await;
        assert!(read_reply(&mut cr, 3).await.get("error").is_none());
        let types = call_service(&rt.host, "message", "list_types", vec![]).await.unwrap();
        assert!(!types["data"].as_array().unwrap().iter().any(|v| v["type"] == "rekey_test"));
        drop((cw, cr));
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        let _ = rpc.manager.shutdown_all().await;
    }).await;
}

#[tokio::test]
async fn rpc_session_in_place_new_transition_updates_host_manager_so_original_reloads_from_disk() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (home, _home_guard, _dir, _guard, rpc, mut cr, mut cw, task, _s0) =
                setup_manager_rpc_mode(mare_plugin::Mode::Rpc).await;
            let faux_path = home.path().join("faux.json");
            std::fs::write(&faux_path, json!([{ "text": "orig turn" }]).to_string()).unwrap();
            let _faux_guard = EnvGuard::set("MARE_FAUX_SCRIPT", &faux_path);

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let created = read_reply(&mut cr, 2).await;
            assert!(created.get("error").is_none(), "{created}");
            let orig_id = created["result"]["session_id"].as_str().unwrap().to_string();
            let p_res = prompt_and_wait(&mut cr, &mut cw, 20, &orig_id, json!("persist orig")).await;
            assert!(p_res.get("error").is_none(), "{p_res}");

            let orig_rt = rpc.manager.session(&orig_id).unwrap();
            call_service(
                &orig_rt.host,
                "session",
                "add_record",
                vec![json!("ui/card"), json!({ "kind": "proposed_plan", "title": "Plan", "markdown": "# Orig Plan" })],
            )
            .await
            .unwrap();

            let recs_before = call_service(
                &orig_rt.host,
                "session",
                "list_records",
                vec![json!("ui/card")],
            )
            .await
            .unwrap();
            assert_eq!(recs_before["data"].as_array().map(|a| a.len()), Some(1), "{recs_before}");

            // Trigger an in-place session:transition({ mode = "new" }) on the same Host.
            let session_svc = orig_rt.host.service_table("session").unwrap();
            orig_rt.host.lua().globals().set("TRANSITION_SVC", session_svc).unwrap();
            let trans_res = orig_rt
                .host
                .lua()
                .load(
                    r#"
                    local info = TRANSITION_SVC:info()
                    return TRANSITION_SVC:transition({ mode = "new" }, { session_id = info.session_id, session_epoch = info.session_epoch })
                    "#,
                )
                .call_async::<mlua::Table>(())
                .await
                .unwrap();
            assert_eq!(trans_res.get::<String>("status").unwrap(), "applied");

            let new_info = rpc
                .manager
                .active_ids()
                .into_iter()
                .filter_map(|id| rpc.manager.session(&id).ok())
                .find(|rt| std::ptr::eq(Arc::as_ptr(&rt.host), Arc::as_ptr(&orig_rt.host)))
                .expect("transitioned host must stay registered in HostManager");
            assert_ne!(
                new_info.session_id, orig_id,
                "HostManager must rekey runtime from {orig_id} to the new transitioned session id"
            );
            assert!(
                rpc.manager.session(&orig_id).is_err(),
                "HostManager must not keep transitioned host indexed under old id {orig_id}"
            );

            // Loading orig_id over RPC must reload orig_id from disk and read its persisted ui/card.
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 3, "method": "session.load", "params": { "session_id": orig_id } }),
            )
            .await;
            let loaded = read_reply(&mut cr, 3).await;
            assert!(loaded.get("error").is_none(), "{loaded}");
            assert_eq!(loaded["result"]["session_id"], orig_id, "{loaded}");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.list_records",
                    "params": { "session_id": orig_id, "kind": "ui/card" }
                }),
            )
            .await;
            let recs_after = read_reply(&mut cr, 4).await;
            assert!(recs_after.get("error").is_none(), "{recs_after}");
            let data = recs_after["result"]["data"].as_array().unwrap();
            assert_eq!(data.len(), 1, "reloaded {orig_id} must expose its persisted ui/card: {recs_after}");
            assert_eq!(data[0]["markdown"], "# Orig Plan", "{recs_after}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_session_load_connects_ui_before_session_loaded_without_duplicate_notify() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let probe_log = home.path().join("loaded-ui-states.txt");
            std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
            std::fs::write(
                home.path().join("config/plugins/replay_notify_probe.lua"),
                format!(
                    r#"return {{
    name = "replay-notify-probe",
    inject = {{ "session" }},
    apply = function(ctx)
        ctx:on("session/loaded", function()
            local connected = mare.ui.connected()
            local f = assert(io.open({log:?}, "a"))
            f:write(tostring(connected) .. "\n")
            f:close()
            if connected then
                mare.ui.notify("Replayed 1 hook audit entry(s)", "info")
            end
        end)
    end,
}}"#,
                    log = probe_log.to_string_lossy()
                ),
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let s0 = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(s0.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": "3.0",
                        "capabilities": { "ui": true, "callbacks": true }
                    }
                }),
            )
            .await;
            let init = read_reply(&mut cr, 1).await;
            assert!(init.get("error").is_none(), "{init}");

            // Create and persist session s1, then close it so session.load reloads it via Host::new.
            let faux_path = home.path().join("faux.json");
            std::fs::write(&faux_path, json!([{ "text": "replay setup" }]).to_string()).unwrap();
            let _faux_guard = EnvGuard::set("MARE_FAUX_SCRIPT", &faux_path);
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let s1 = read_reply(&mut cr, 2).await["result"]["session_id"].as_str().unwrap().to_string();
            let p_res = prompt_and_wait(&mut cr, &mut cw, 20, &s1, json!("persist s1")).await;
            assert!(p_res.get("error").is_none(), "{p_res}");
            let s1_rt = rpc.manager.session(&s1).unwrap();
            call_service(&s1_rt.host, "session", "add_record", vec![json!("ui/probe"), json!({ "ok": true })])
                .await
                .unwrap();

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 3, "method": "session.close", "params": { "session_id": s1 } }),
            )
            .await;
            assert!(read_reply(&mut cr, 3).await.get("error").is_none());

            let _ = std::fs::remove_file(&probe_log);
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 4, "method": "session.load", "params": { "session_id": s1 } }),
            )
            .await;

            let mut notifies = Vec::new();
            let loaded = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "ui.notify" {
                    notifies.push(msg["params"].clone());
                } else if msg["id"] == 4 {
                    break msg;
                }
            };
            assert!(loaded.get("error").is_none(), "{loaded}");
            let states = std::fs::read_to_string(&probe_log).unwrap_or_default();
            assert_eq!(states.trim(), "true", "mare.ui.connected() must be true during session/loaded: {states:?}");
            let replay_notifies: Vec<_> = notifies
                .iter()
                .filter(|n| n["text"].as_str().is_some_and(|t| t.contains("Replayed 1 hook audit")))
                .collect();
            assert_eq!(replay_notifies.len(), 1, "expected exactly 1 replay notify, got: {notifies:?}");
            assert_eq!(replay_notifies[0]["session_id"], s1, "{replay_notifies:?}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn session_receipts_are_bounded_and_replayed_across_connections() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, cr, cw, task, sid) = setup_manager_rpc().await;
            let conn = rpc.connections.borrow().values().find_map(Weak::upgrade).unwrap();
            let anchor = rpc.anchor_host().unwrap();
            let caller1 = super::super::session_ext::caller_for_conn(&conn, &anchor);
            let orch = rpc.manager.orchestrator();
            for i in 0..257 {
                orch.store_receipt(
                    caller1.receipt_scope(),
                    "create",
                    Some(format!("request-{i}")),
                    json!({"n":i}),
                    &sid,
                    &json!({"saved":i}),
                );
            }
            assert_eq!(orch.receipts_len(), 256);
            assert!(
                orch.lookup_receipt(caller1.receipt_scope(), "create", Some("request-0"), &json!({"n":0}))
                    .unwrap()
                    .is_none()
            );
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task2 =
                tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            tokio::task::yield_now().await;
            let conn2 = rpc.connections.borrow().values().filter_map(Weak::upgrade).find(|c| c.id != conn.id).unwrap();
            let caller2 = super::super::session_ext::caller_for_conn(&conn2, &anchor);
            let receipt = orch
                .lookup_receipt(caller2.receipt_scope(), "create", Some("request-256"), &json!({"n":256}))
                .unwrap()
                .unwrap();
            assert_eq!(receipt, (sid, json!({"saved":256})));
            assert!(
                orch.lookup_receipt(caller2.receipt_scope(), "create", Some("request-256"), &json!({"n":999})).is_err()
            );
            drop(client);
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn peer_event_subscription_rechecks_revocation_before_sending() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, cr, cw, task, sid) = setup_manager_rpc().await;
            let rt = rpc.manager.session(&sid).unwrap();
            allow(&rt.host, &["events.subscribe"]);

            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);

            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": "3.0", "capabilities": {} }
                }),
            )
            .await;
            let p_init = read_reply(&mut pcr, 1).await;
            assert!(p_init.get("error").is_none(), "{p_init}");

            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "events.subscribe",
                    "params": { "session_id": sid, "include": ["host/notice"] }
                }),
            )
            .await;
            let sub_rep = read_reply(&mut pcr, 2).await;
            assert!(sub_rep.get("error").is_none(), "{sub_rep}");

            // Before revocation: `notice` reaches peer.
            rt.host
                .kernel
                .event_log()
                .admit_json("host/notice", json!({ "level": "info", "text": "before-revoke" }))
                .await
                .unwrap();
            let ev1 = loop {
                let msg = rpc_read(&mut pcr).await;
                if msg["method"] == "event" && msg["params"]["type"] == "host/notice" {
                    break msg;
                }
            };
            assert_eq!(ev1["params"]["data"]["text"], "before-revoke");

            // Admit a second `notice` WHILE `events` is still granted, then revoke `peer.allow`
            // synchronously before the LocalSet event pump drains the queued cursor record, and
            // sync via `status.get` to prove the pre-send re-authorization suppresses it.
            rt.host
                .kernel
                .event_log()
                .admit_json("host/notice", json!({ "level": "info", "text": "queued-before-revoke" }))
                .await
                .unwrap();
            allow(&rt.host, &[]);
            rpc_send(&mut pcw, json!({ "jsonrpc": "2.0", "id": 3, "method": "status.get", "params": {} })).await;
            loop {
                let msg = rpc_read(&mut pcr).await;
                if msg["method"] == "event" && msg["params"]["type"] == "host/notice" {
                    panic!("revoked peer must not receive pre-revoke queued event: {msg}");
                }
                if msg["id"] == 3 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            drop((pcw, pcr, cw, cr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// RPC `options.set` must not write into `base_opts.config` (the `cli` config layer), so setting
/// `plugins.permissions.preset = "full-access"` at runtime cannot bypass the untrusted ceiling
/// when creating a new session in an untrusted workspace.
#[tokio::test]
async fn rpc_options_set_does_not_elevate_into_cli_layer_or_bypass_untrusted_ceiling() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_env = EnvGuard::set("MARE_HOME", home.path().as_os_str());
            let trusted_dir = tempfile::tempdir().unwrap();
            let untrusted_dir = tempfile::tempdir().unwrap();

            let base_opts = mare_host::HostOptions {
                cwd: trusted_dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": true } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let s1_rt = manager.create_session(base_opts.clone()).await.unwrap();
            let s1 = s1_rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, base_opts, s1_rt.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // Write `plugins.permissions.preset = "full-access"` via primary `options.set` (sync_base = true).
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "options.set",
                    "params": { "session_id": s1, "values": { "plugins.permissions.preset": "full-access" } }
                }),
            )
            .await;
            let set_rep = read_reply(&mut cr, 2).await;
            assert!(set_rep.get("error").is_none(), "options.set failed: {set_rep}");

            // `base_opts.config` (the startup CLI layer) must not have been mutated.
            let base_cfg = rpc.clone_base_opts(None).unwrap().config;
            assert!(
                base_cfg.pointer("/plugins/permissions/preset").is_none(),
                "options.set must not pollute CLI-layer base_opts.config: {base_cfg}"
            );

            // Creating a new session in an untrusted workspace must stay at `read-only`, not `full-access`.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.create",
                    "params": { "cwd": untrusted_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let created = read_reply(&mut cr, 3).await;
            assert!(created.get("error").is_none(), "session.create failed: {created}");
            let s2 = created["result"]["session_id"].as_str().unwrap().to_string();
            let s2_rt = rpc.manager.session(&s2).unwrap();
            assert!(!s2_rt.host.is_project_trusted(), "untrusted_dir session must be untrusted");

            let (preset, src_layer): (String, Option<String>) = s2_rt
                .host
                .lua()
                .load(
                    r#"
                    local srcs = mare.config_sources and mare.config_sources() or {}
                    local src = srcs["plugins.permissions.preset"]
                    return (src and src.layer) or nil
                    "#,
                )
                .eval::<Option<String>>()
                .map(|layer| {
                    let perm = s2_rt.host.service_table("permissions").unwrap();
                    let cur: String = perm.get::<mlua::Function>("preset").unwrap().call(perm).unwrap();
                    (cur, layer)
                })
                .unwrap();
            assert_eq!(preset, "read-only", "untrusted session must not inherit full-access from RPC options.set");
            assert_ne!(
                src_layer.as_deref(),
                Some("cli"),
                "RPC options.set must not appear as cli layer in new session: {src_layer:?}"
            );

            // Creating a new session in a trusted workspace DOES inherit the runtime default
            // (with source layer `runtime`, not `cli`).
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.create",
                    "params": { "cwd": trusted_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let trusted_created = read_reply(&mut cr, 4).await;
            assert!(trusted_created.get("error").is_none(), "trusted session.create failed: {trusted_created}");
            let s3 = trusted_created["result"]["session_id"].as_str().unwrap().to_string();
            let s3_rt = rpc.manager.session(&s3).unwrap();
            assert!(s3_rt.host.is_project_trusted(), "trusted_dir session must be trusted");
            let (trusted_preset, trusted_layer): (String, Option<String>) = s3_rt
                .host
                .lua()
                .load(
                    r#"
                    local srcs = mare.config_sources and mare.config_sources() or {}
                    local src = srcs["plugins.permissions.preset"]
                    return (src and src.layer) or nil
                    "#,
                )
                .eval::<Option<String>>()
                .map(|layer| {
                    let perm = s3_rt.host.service_table("permissions").unwrap();
                    let cur: String = perm.get::<mlua::Function>("preset").unwrap().call(perm).unwrap();
                    (cur, layer)
                })
                .unwrap();
            assert_eq!(
                trusted_preset, "full-access",
                "trusted session must receive runtime default from RPC options.set"
            );
            assert_eq!(trusted_layer.as_deref(), Some("runtime"), "{trusted_layer:?}");

            // Overwriting or clearing a parent option key must prune stale `k.` subkeys so the
            // last write wins when replayed on subsequent `session.create`.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "options.set",
                    "params": { "session_id": s3, "values": { "plugins.permissions.dangerous.enabled": false } }
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "options.set",
                    "params": { "session_id": s3, "values": { "plugins.permissions.dangerous": { "enabled": true } } }
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());
            assert!(
                !rpc.runtime_defaults.borrow().options.contains_key("plugins.permissions.dangerous.enabled"),
                "writing parent key must prune stale child key: {:?}",
                rpc.runtime_defaults.borrow().options
            );
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "session.create",
                    "params": { "cwd": trusted_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let s4 = read_reply(&mut cr, 7).await["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "options.get",
                    "params": { "session_id": s4, "keys": ["plugins.permissions.dangerous.enabled"] }
                }),
            )
            .await;
            let g4 = read_reply(&mut cr, 8).await;
            assert_eq!(
                g4["result"]["plugins.permissions.dangerous.enabled"], true,
                "parent overwrite must win over earlier subkey write on session.create: {g4}"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn user_session_override_still_gets_one_runtime_per_create() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let plugins = home.path().join("config/plugins");
            let state_dir = home.path().join("state");
            std::fs::create_dir_all(&plugins).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            let seq_path = state_dir.join("override_seq.txt");
            let plugin_src = format!(
                r##"return {{
  name = "session", override = true, provide = {{ "session" }},
  apply = function(ctx)
    local seq_path = {seq_path:?}
    local function next_id()
      local n = 0
      local f = io.open(seq_path, "r")
      if f then
        n = tonumber(f:read("*a")) or 0
        f:close()
      end
      n = n + 1
      local w = assert(io.open(seq_path, "w"))
      w:write(tostring(n))
      w:close()
      return "override-" .. n
    end
    local cur = {{ id = next_id(), path = nil }}
    cur.path = "/override/" .. cur.id .. ".jsonl"
    local function snap()
      return {{
        session_id = cur.id,
        session_epoch = cur.id .. "#0:0",
        cwd = mare.cwd,
        path = cur.path,
        loaded = true,
        created_at_ms = 0,
        updated_at_ms = 0,
      }}
    end
    local impl = {{
      commit = function() return true end,
      append_compaction = function() return true end,
      append_prune = function() return true end,
      set_sandbox_mode = function(_, spec) return mare.proc.sandbox.set_current(spec) end,
      info = function() return {{ session_id = cur.id, session_epoch = cur.id .. "#0:0", path = cur.path, count = 0 }} end,
      available = function() return true end,
      switching = function() return false end,
      child = function() return nil end,
      children = function() return {{}} end,
      short_id = function(_, id) return id end,
      load_transcript = function() end,
      get = function() return snap() end,
      list = function() return {{ data = {{ snap() }} }} end,
      entries = function() return {{}} end,
      list_items = function() return {{ data = {{}} }} end,
      add_record = function() return {{ record_id = "r1", durable = false }} end,
      list_records = function() return {{ data = {{}} }} end,
      leaf = function() return nil end,
      entry_id = function() return nil end,
      file_changes = function() return {{}} end,
      path_to = function() return {{}} end,
      set_leaf = function() return true end,
      update = function() return snap() end,
      label_item = function() return {{ item_id = "e1" }} end,
      archive = function() return {{}} end,
      unarchive = function() return {{}} end,
      delete = function() return {{}} end,
      rewind = function() return snap() end,
      transition = function() return {{ status = "rejected", error = "unsupported" }} end,
      persist_message = function() return "m1" end,
      record_tool_state = function() return "t1" end,
      record_request = function() return "r1" end,
      record_request_end = function() return "re1" end,
      record_request_aborted = function() return "ra1" end,
      record_run_end = function() return true end,
      baseline_cut = function() return {{ parts = {{}} }} end,
      snapshot = function() return {{ session_id = cur.id, path = cur.path, leaf_entry_id = nil }} end,
      snapshot_context = function() return {{ entries = {{}}, message_count = 0 }} end,
      search = function() return {{ data = {{}}, corpus_revision = "g0", incomplete = false }} end,
      search_items = function() return {{ data = {{}}, corpus_revision = "g0", incomplete = false }} end,
      get_item = function() return {{}} end,
      trace = function() return {{}} end,
      trace_item = function() return {{}} end,
    }}
    local Session = require("cordis").class("Session", mare.Service)
    function Session:constructor(c)
      Session.super.constructor(self, c, "session")
    end
    for k, fn in pairs(impl) do
      Session[k] = fn
    end
    ctx:plugin(Session)
  end,
}}"##,
                seq_path = seq_path.to_string_lossy()
            );
            std::fs::write(plugins.join("session.lua"), plugin_src).unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let mgr = mare_host::HostManager::new();
            let s1 = mgr.create_session(opts.clone()).await.unwrap();
            assert_eq!(
                s1.host.kernel.service_provider("session"),
                Some(("session".to_string(), mare_kernel::Origin::User)),
                "user override must provide the session service"
            );
            let rpc = RpcRuntime::new(mgr, opts, s1.host.clone(), Some(s1.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            let init = read_reply(&mut cr, 1).await;
            assert!(init["error"].is_null(), "{init}");

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let created = read_reply(&mut cr, 2).await;
            assert!(created["error"].is_null(), "{created}");
            let s2_id = created["result"]["session_id"].as_str().expect("session_id").to_string();
            assert_ne!(s2_id, s1.session_id);
            assert_eq!(
                rpc.manager.len(),
                2,
                "user session override must still get an independent SessionRuntime per session.create"
            );
            let rt1 = rpc.manager.session(&s1.session_id).expect("original runtime must stay loaded");
            let rt2 = rpc.manager.session(&s2_id).expect("new runtime must be registered");
            assert!(!Arc::ptr_eq(&rt1.host, &rt2.host), "each created session must own its own Host VM");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn lua_create_matches_wire_create_in_a_multi_runtime_host() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).expect("initial runtime");
            let before_epoch = rt1.host.kernel.event_log().session_identity().1;

            // Calling `session.create` from Lua inside `s1`'s VM must go through the same
            // multi-runtime SessionOrchestrator as wire RPC, creating a second independent
            // SessionRuntime without switching `s1` in place.
            let lua_created = super::call_service(&rt1.host, "session", "create", vec![json!({ "name": "from-lua" })])
                .await
                .expect("Lua session:create must succeed");
            let s2 = lua_created["session_id"].as_str().expect("session_id").to_owned();
            assert_ne!(s2, s1, "Lua session.create must yield a new session id");
            assert_eq!(rpc.manager.len(), 2, "Lua session.create must spawn a second SessionRuntime in Multi mode");
            assert_eq!(
                rt1.host.kernel.event_log().session_identity(),
                (Some(s1.clone()), before_epoch),
                "source runtime s1 must NOT switch in place when Lua calls session.create in Multi mode"
            );
            let rt2 = rpc.manager.session(&s2).expect("new runtime must be registered in HostManager");
            assert!(!Arc::ptr_eq(&rt1.host, &rt2.host), "new runtime must have its own Host VM");

            // Wire session.create must behave identically, creating a third runtime.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 10, "method": "session.create", "params": {} })).await;
            let wire_reply = read_reply(&mut cr, 10).await;
            assert!(wire_reply["error"].is_null(), "{wire_reply}");
            let s3 = wire_reply["result"]["session_id"].as_str().expect("wire session_id").to_owned();
            assert_ne!(s3, s1);
            assert_ne!(s3, s2);
            assert_eq!(rpc.manager.len(), 3);

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn restricted_caller_is_confined_identically_from_lua_and_wire() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir1, _g, rpc, cr, cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.create", "session.list"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                json!({}),
                true,
            )
            .await;
            let outside = tempfile::tempdir().unwrap();
            let outside_str = outside.path().to_string_lossy().to_string();

            // 1. Wire restricted peer calling `session.create` outside the project must be denied.
            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let p_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), psr, psw, false, None, std::future::pending()));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0", "capabilities": {} } }),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await["error"].is_null());

            rpc_send(
                &mut pcw,
                json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": { "cwd": outside_str } }),
            )
            .await;
            let wire_denied = read_reply(&mut pcr, 2).await;
            assert_eq!(wire_denied["error"]["data"]["code"], "denied", "wire must deny outside cwd: {wire_denied}");

            // 2. Same restricted caller context invoking `session.create` through Lua export must be
            // denied identically, while same-project `session.create` succeeds and spawns a runtime.
            let rt1 = rpc.manager.session(&s1).expect("s1 runtime");
            let restricted_ctx = rt1.host.kernel.open_owner("<restricted-peer>", "root").unwrap();
            rt1.host.kernel.set_caller_access(
                restricted_ctx,
                Some(mare_kernel::CallerAccess {
                    is_peer: true,
                    allows: Rc::new(|svc, method, _| svc == "session" && matches!(method, "create" | "list")),
                    allows_session_path: Rc::new(|| false),
                }),
            );
            let lua_denied = rt1
                .host
                .kernel
                .call_service_export(
                    restricted_ctx,
                    None,
                    "session",
                    "create",
                    json!({ "cwd": outside_str }),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await;
            let err = lua_denied.expect_err("restricted Lua caller must be denied outside cwd");
            assert_eq!(err.code(), "denied", "Lua must deny outside cwd identically to wire: {err:?}");

            let lua_allowed = rt1
                .host
                .kernel
                .call_service_export(
                    restricted_ctx,
                    None,
                    "session",
                    "create",
                    json!({ "cwd": dir1.path().to_string_lossy() }),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .expect("restricted Lua caller within project must succeed");
            let s2 = lua_allowed["session_id"].as_str().expect("session_id").to_owned();
            assert_ne!(s2, s1);
            assert_eq!(rpc.manager.len(), 2, "same-project Lua create must register a new runtime");

            let _ = rt1.host.kernel.dispose_ctx(restricted_ctx).await;
            drop((pcw, pcr, cw, cr));
            tokio::time::timeout(Duration::from_secs(5), p_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn request_id_replays_once_across_lua_and_wire() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _g, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).expect("s1 runtime");

            // 1. Create via Lua with `request_id`.
            let lua_snap = super::call_service(
                &rt1.host,
                "session",
                "create",
                vec![json!({ "name": "shared-idem", "request_id": "idem-lua-wire-1" })],
            )
            .await
            .expect("Lua create with request_id must succeed");
            let created_id = lua_snap["session_id"].as_str().expect("session_id").to_owned();
            assert_eq!(rpc.manager.len(), 2, "first create spawns a second runtime");

            // 2. Replay the same `request_id` + params over owner wire RPC -> same snapshot, no 3rd runtime.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 20,
                    "method": "session.create",
                    "params": { "name": "shared-idem", "request_id": "idem-lua-wire-1" }
                }),
            )
            .await;
            let wire_replay = read_reply(&mut cr, 20).await;
            assert!(wire_replay["error"].is_null(), "{wire_replay}");
            assert_eq!(wire_replay["result"]["session_id"], created_id, "wire must replay Lua receipt: {wire_replay}");
            assert_eq!(rpc.manager.len(), 2, "replay must not create a third runtime");

            // 3. Reusing the same `request_id` with different params over wire must fail with request_id_conflict.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 21,
                    "method": "session.create",
                    "params": { "name": "different-name", "request_id": "idem-lua-wire-1" }
                }),
            )
            .await;
            let conflict = read_reply(&mut cr, 21).await;
            assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn lifecycle_calls_targeting_non_default_session_preserve_its_connection_ctx() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = crate::rpc::serve::tests::HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": {
                        "permissions": { "enabled": false },
                        "provider-faux": { "script_json": r#"[{"text":"r1"},{"text":"r2"}]"# }
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let mgr = mare_host::HostManager::new();
            let s1 = mgr.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(mgr, opts, s1.host.clone(), Some(s1.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            let init = read_reply(&mut cr, 1).await;
            assert!(init["error"].is_null(), "{init}");

            // Create a second session S2 and prompt it so it is persisted on disk.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let c2 = read_reply(&mut cr, 2).await;
            assert!(c2["error"].is_null(), "{c2}");
            let s2_id = c2["result"]["session_id"].as_str().unwrap().to_owned();
            let s2_rt = rpc.manager.session(&s2_id).unwrap();
            s2_rt.host.prompt("seed-s2", json!({})).await.unwrap();

            // Allocate a real session ctx on S2's own host via a session-scoped call.
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 3, "method": "agent.list_messages", "params": { "session_id": s2_id } }),
            )
            .await;
            let msgs = read_reply(&mut cr, 3).await;
            assert!(msgs["error"].is_null(), "{msgs}");

            let conn = rpc
                .connections
                .borrow()
                .values()
                .find_map(std::rc::Weak::upgrade)
                .expect("live connection");
            let (orig_host_weak, orig_ctx_id) = conn
                .session_ctxs
                .borrow()
                .get(&s2_id)
                .cloned()
                .expect("S2 must have a cached session ctx");
            assert!(
                orig_host_weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, &s2_rt.host)),
                "initial S2 ctx must belong to S2 host"
            );

            // Switch default back to S1 so S2 is non-default.
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 4, "method": "session.load", "params": { "session_id": s1.session_id } }),
            )
            .await;
            assert!(read_reply(&mut cr, 4).await["error"].is_null());

            // Call host-provided lifecycle methods targeting non-default S2: fork, then load.
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 5, "method": "session.fork", "params": { "session_id": s2_id } }),
            )
            .await;
            let forked = read_reply(&mut cr, 5).await;
            assert!(forked["error"].is_null(), "{forked}");
            let s3_id = forked["result"]["session_id"].as_str().unwrap().to_owned();

            let (after_fork_weak, after_fork_ctx) = conn
                .session_ctxs
                .borrow()
                .get(&s2_id)
                .cloned()
                .expect("S2 ctx must remain cached after fork");
            assert_eq!(after_fork_ctx, orig_ctx_id, "session.fork must not overwrite S2 ctx_id");
            assert!(
                after_fork_weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, &s2_rt.host)),
                "session.fork must not rebind S2 ctx to anchor host"
            );

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 6, "method": "session.load", "params": { "session_id": s2_id } }),
            )
            .await;
            assert!(read_reply(&mut cr, 6).await["error"].is_null());
            let (after_load_weak, after_load_ctx) = conn
                .session_ctxs
                .borrow()
                .get(&s2_id)
                .cloned()
                .expect("S2 ctx must remain cached after load");
            assert_eq!(after_load_ctx, orig_ctx_id, "session.load must not overwrite S2 ctx_id");
            assert!(
                after_load_weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, &s2_rt.host)),
                "session.load must not rebind S2 ctx to anchor host"
            );

            // Close the third session and confirm S2 ctx is untouched.
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 7, "method": "session.close", "params": { "session_id": s3_id } }),
            )
            .await;
            assert!(read_reply(&mut cr, 7).await["error"].is_null());
            assert_eq!(
                conn.session_ctxs.borrow().get(&s2_id).map(|(_, id)| *id),
                Some(orig_ctx_id),
                "closing the third session must not disturb S2 ctx"
            );

            drop((conn, cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn peer_granted_session_update_stays_project_confined_and_cannot_load_by_path() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = crate::rpc::serve::tests::HOST_LOCK.lock().await;
            let project_dir = tempfile::tempdir().unwrap();
            let outside_dir = tempfile::tempdir().unwrap();
            let outside_session = outside_dir.path().join("outside.jsonl");
            std::fs::write(
                &outside_session,
                format!(
                    "{}\n",
                    json!({
                        "type": "session",
                        "version": mare_store::CURRENT_VERSION,
                        "session_id": "99999999-9999-9999-9999-999999999999",
                        "cwd": outside_dir.path().to_string_lossy(),
                    })
                ),
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: project_dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "peer": { "allow": ["session.update", "session.create", "session.load", "session.list"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let mgr = mare_host::HostManager::new();
            let s1 = mgr.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(mgr, opts, s1.host.clone(), Some(s1.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            // Non-owner peer connection with session.update granted.
            let task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, false, None, std::future::pending()));

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await["error"].is_null());

            // Granting session.update to a peer must NOT let it create a session outside the project.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": outside_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let create_err = read_reply(&mut cr, 2).await;
            assert_eq!(
                create_err["error"]["data"]["code"], "denied",
                "peer granted session.update must stay project-confined: {create_err}"
            );

            // Granting session.update to a peer must NOT let it load a session by filesystem path.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.load",
                    "params": { "path": outside_session.to_string_lossy() }
                }),
            )
            .await;
            let load_err = read_reply(&mut cr, 3).await;
            assert!(load_err["error"].is_object(), "peer granted session.update must not load by path: {load_err}");
            assert!(
                load_err["error"]["message"].as_str().is_some_and(|m| m.contains("primary connection")),
                "expected primary-connection path restriction, got: {load_err}"
            );

            // Direct kernel export with CallerAccess { is_peer: true } and `session.update`
            // explicitly allowed (modeling a peer explicitly granted `session.update`):
            // caller construction must still treat the peer as non-owner (confined, no path load).
            let peer_ctx = s1.host.kernel.open_owner("peer-with-update", "user").unwrap();
            s1.host.kernel.set_caller_access(
                peer_ctx,
                Some(mare_kernel::CallerAccess {
                    is_peer: true,
                    allows: std::rc::Rc::new(|service, _method, _access| service == "session"),
                    allows_session_path: std::rc::Rc::new(|| false),
                }),
            );
            let direct_create = s1
                .host
                .kernel
                .call_service_export(
                    peer_ctx,
                    None,
                    "session",
                    "create",
                    json!({ "cwd": outside_dir.path().to_string_lossy() }),
                    Default::default(),
                )
                .await
                .expect_err("peer with session.update granted must still be denied outside-project create");
            assert_eq!(direct_create.code(), "denied", "{direct_create}");
            let direct_load = s1
                .host
                .kernel
                .call_service_export(
                    peer_ctx,
                    None,
                    "session",
                    "load",
                    json!({ "path": outside_session.to_string_lossy() }),
                    Default::default(),
                )
                .await
                .expect_err("peer with session.update granted must still be rejected from loading by path");
            assert_eq!(direct_load.code(), "invalid_arguments", "{direct_load}");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// A plugin handling a remote service export call on session `S1` that invokes
/// `ctx.session:close(S1)` must detach and schedule async shutdown without self-deadlocking
/// or getting aborted mid-shutdown before returning to the RPC caller.
#[tokio::test]
async fn plugin_export_closing_own_session_does_not_deadlock_or_abort() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _hg = HOST_LOCK.lock().await;
            let _home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", _home.path());
            let dir = tempfile::tempdir().unwrap();
            let plugins_dir = _home.path().join("config/plugins");
            std::fs::create_dir_all(&plugins_dir).unwrap();
            std::fs::write(
                plugins_dir.join("selfclose.lua"),
                r#"
                return {
                    name = "selfclose",
                    inject = { "session" },
                    provide = { "selfclose" },
                    contracts = {
                        selfclose = {
                            doc = "test service that closes its own session",
                            methods = {
                                close_me = {
                                    doc = "close the current session",
                                    expose = "both",
                                    scope = "session",
                                    params = { type = "object", properties = {}, additionalProperties = false },
                                    order = {},
                                    result = {
                                        type = "object",
                                        properties = { closed = { type = "boolean" } },
                                        required = { "closed" },
                                    },
                                },
                            },
                        },
                    },
                    apply = function(ctx)
                        local SelfClose = require("cordis").class("SelfClose", mare.Service)
                        function SelfClose:constructor(c)
                            SelfClose.super.constructor(self, c, "selfclose")
                        end
                        function SelfClose:close_me()
                            local id = ctx.session:info().session_id
                            ctx.session:close(id)
                            return { closed = true }
                        end
                        ctx:plugin(SelfClose)
                    end,
                }
                "#,
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_path_buf(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 500,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let initial = manager.create_session(opts.clone()).await.expect("initial session");
            let s1 = initial.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, initial.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await["error"].is_null());

            // Invoke the plugin export on S1 that calls `ctx.session:close(S1)`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"selfclose.close_me","params":{"session_id":s1}}),
            )
            .await;
            let reply = tokio::time::timeout(Duration::from_secs(3), read_reply(&mut cr, 2))
                .await
                .expect("plugin export closing its own session must not deadlock");
            assert!(reply["error"].is_null(), "plugin export closing its own session must not abort: {reply}");
            assert_eq!(reply["result"]["closed"], true, "{reply}");

            // Background close must finish cleanly and remove S1 from both active and closing maps.
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    rpc.manager.wait_closing(&s1).await;
                    if rpc.manager.session(&s1).is_err() && !rpc.manager.is_closing(&s1) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("session S1 must finish closing cleanly after plugin export returns");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// When all sessions are closed and `ensure_live_anchor_host` constructs a fallback anchor host,
/// the fallback host must already have the Multi orchestrator attached before `app/ready` fires.
#[tokio::test]
async fn rpc_fallback_anchor_host_sees_multi_orchestrator_during_app_ready() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _hg = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let plugins_dir = home.path().join("config/plugins");
            std::fs::create_dir_all(&plugins_dir).unwrap();
            std::fs::write(
                plugins_dir.join("anchor-ready-cap.lua"),
                r#"
                return {
                    name = "anchor-ready-cap",
                    inject = { "kernel" },
                    apply = function(ctx)
                        ctx:on("app/ready", function()
                            mare.g.anchor_ready_capacity = ctx.kernel:session_capacity()
                        end)
                    end,
                }
                "#,
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_path_buf(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let initial = manager.create_session(opts.clone()).await.expect("initial session");
            let s1 = initial.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, initial.host.clone(), Some(s1.clone()), false);

            // Close the only session and clear the fallback host so `ensure_live_anchor_host`
            // must construct a fresh fallback anchor host.
            let _ = rpc.manager.close_session(&s1).await.unwrap();
            rpc.on_session_closed(&s1);
            rpc.shutdown_fallback_host(None).await;
            assert!(rpc.anchor_host().is_none(), "no live anchor host should remain before fallback creation");

            let fallback = rpc.ensure_live_anchor_host().await.expect("fallback anchor host must boot");
            let cap: String = fallback.lua().load("return tostring(mare.g.anchor_ready_capacity)").eval().unwrap();
            assert_eq!(
                cap, "multi",
                "fallback anchor host constructed by ensure_live_anchor_host must see multi capacity during app/ready"
            );

            rpc.shutdown_fallback_host(None).await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Unauthorized peers are refused all 10 remote session write methods, and
/// coarse/wildcard entries (`session`, `session.*`, `session.update*`) never grant access.
#[tokio::test]
async fn peer_ungranted_and_wildcard_grants_cannot_call_session_write_methods() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, cr, cw, task, sid) = setup_manager_rpc().await;
            let rt = rpc.manager.session(&sid).unwrap();
            allow(&rt.host, &[]);

            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            let write_calls: Vec<(&str, J)> = vec![
                ("session.fork", json!({ "session_id": sid })),
                ("session.snapshot", json!({ "session_id": sid })),
                ("session.close", json!({ "session_id": sid })),
                ("session.rewind", json!({ "session_id": sid })),
                ("session.add_record", json!({ "session_id": sid, "kind": "note", "data": {} })),
                ("session.update", json!({ "session_id": sid, "name": "nope" })),
                ("session.label_item", json!({ "session_id": sid, "label": "nope" })),
                ("session.archive", json!({ "session_id": sid })),
                ("session.unarchive", json!({ "session_id": sid })),
                ("session.delete", json!({ "session_id": sid })),
            ];
            let mut id = 10i64;
            for (method, params) in &write_calls {
                id += 1;
                rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
                let rep = read_reply(&mut pcr, id).await;
                assert_eq!(
                    rep["error"]["data"]["code"], "denied",
                    "ungranted peer calling {method} must be denied: {rep}"
                );
            }

            // Coarse service names and wildcards in `peer.allow` must NOT grant any session method.
            allow(&rt.host, &["session", "session.*", "session.update*", "session.delete*"]);
            for (method, params) in [
                ("session.update", json!({ "session_id": sid, "name": "wild" })),
                ("session.delete", json!({ "session_id": sid })),
                ("session.archive", json!({ "session_id": sid })),
                ("session.snapshot", json!({ "session_id": sid })),
                ("session.create", json!({})),
                ("session.get", json!({ "session_id": sid })),
                ("session.list", json!({})),
                ("session.search_items", json!({ "session_id": sid, "query": "x" })),
            ] {
                id += 1;
                rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
                let rep = read_reply(&mut pcr, id).await;
                assert_eq!(
                    rep["error"]["data"]["code"], "denied",
                    "wildcard/service-name peer.allow must NOT grant {method}: {rep}"
                );
            }

            drop((pcw, pcr, cw, cr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Granting `session.update` to a peer must NEVER turn that peer into an
/// unrestricted caller able to escape the host project on `create`, named-session calls,
/// confined reads (`search_items`), `session.list`, or `session.get`.
#[tokio::test]
async fn peer_granted_session_update_never_bypasses_project_scope() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _guard, rpc, mut ocr, mut ocw, owner_task, s_same) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let anchor_rt = rpc.manager.session(&s_same).unwrap();
            allow(&anchor_rt.host, &["session.update", "session.create", "session.get", "session.list", "session.search_items"]);
            assert!(prompt_and_wait(&mut ocr, &mut ocw, 2, &s_same, json!("same project msg")).await.get("error").is_none());

            // Owner creates a second live session in sibling project `dir2`.
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let s_other_rep = read_reply(&mut ocr, 3).await;
            assert!(s_other_rep.get("error").is_none(), "{s_other_rep}");
            let s_other = s_other_rep["result"]["session_id"].as_str().unwrap().to_string();
            let s_other_path = s_other_rep["result"]["path"].as_str().unwrap().to_string();
            let other_rt = rpc.manager.session(&s_other).unwrap();
            allow(&other_rt.host, &["session.update", "session.create", "session.get", "session.list", "session.search_items"]);
            assert!(
                prompt_and_wait(&mut ocr, &mut ocw, 4, &s_other, json!("CROSS_PROJECT_SECRET_TOKEN"))
                    .await
                    .get("error")
                    .is_none()
            );

            // Connect peer (has `session.update` granted!).
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // 1. `session.create` outside host project must still be denied.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":10,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let create_out = read_reply(&mut pcr, 10).await;
            assert_eq!(
                create_out["error"]["data"]["code"], "denied",
                "peer with session.update must NOT create sessions outside host project: {create_out}"
            );

            // 2. Named-session `session.update` targeting live session in `dir2` must fail closed (`unknown_session`).
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":11,"method":"session.update","params":{"session_id":s_other,"name":"pwned"}}),
            )
            .await;
            let upd_out = read_reply(&mut pcr, 11).await;
            assert_eq!(
                upd_out["error"]["data"]["code"], "unknown_session",
                "peer with session.update must NOT mutate live session outside host project: {upd_out}"
            );

            // 3. Confined read `session.search_items` targeting live session in `dir2` must fail closed (`unknown_session`).
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":12,"method":"session.search_items","params":{"session_id":s_other,"query":"CROSS_PROJECT_SECRET_TOKEN"}}),
            )
            .await;
            let search_out = read_reply(&mut pcr, 12).await;
            assert_eq!(
                search_out["error"]["data"]["code"], "unknown_session",
                "peer with session.update must NOT bypass project scope on confined search_items: {search_out}"
            );

            // 4. `session.list` with `all = true` or outside `cwd` must be denied.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":13,"method":"session.list","params":{"all":true}})).await;
            let list_all = read_reply(&mut pcr, 13).await;
            assert_eq!(
                list_all["error"]["data"]["code"], "denied",
                "peer with session.update must NOT list sessions across all projects: {list_all}"
            );
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":14,"method":"session.list","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let list_cwd = read_reply(&mut pcr, 14).await;
            assert_eq!(
                list_cwd["error"]["data"]["code"], "denied",
                "peer with session.update must NOT list sessions for another cwd: {list_cwd}"
            );

            // 5. `session.get` with cross-project live id or path must fail closed (`unknown_session`).
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":15,"method":"session.get","params":{"session_id":s_other}}))
                .await;
            let get_other = read_reply(&mut pcr, 15).await;
            assert_eq!(
                get_other["error"]["data"]["code"], "unknown_session",
                "peer with session.update targeting live cross-project session.get must fail closed: {get_other}"
            );
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":16,"method":"session.get","params":{"session_id":s_other_path}}),
            )
            .await;
            let get_other_path = read_reply(&mut pcr, 16).await;
            assert_eq!(
                get_other_path["error"]["data"]["code"], "unknown_session",
                "peer with session.update must NOT resolve cross-project session by path: {get_other_path}"
            );

            // 6. Same-project `session.update` succeeds cleanly.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":17,"method":"session.update","params":{"session_id":s_same,"name":"peer-renamed"}}),
            )
            .await;
            let upd_same = read_reply(&mut pcr, 17).await;
            assert_eq!(upd_same["result"]["name"], "peer-renamed", "{upd_same}");

            drop((pcw, pcr, ocw, ocr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// A peer granted `session.{archive,unarchive,delete,snapshot}` must NOT address
/// sessions by file path (even in the same project) and must NOT resolve or mutate sessions
/// belonging to another project by id.
#[tokio::test]
async fn peer_granted_session_mutations_cannot_use_paths_or_escape_project() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _guard, rpc, mut ocr, mut ocw, owner_task, s_anchor) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let anchor_rt = rpc.manager.session(&s_anchor).unwrap();
            allow(&anchor_rt.host, &["session.archive", "session.unarchive", "session.delete", "session.snapshot"]);

            // Create and persist an inactive session in host project `dir1` and another in `dir2`.
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"name":"same-idle"}}))
                .await;
            let same_rep = read_reply(&mut ocr, 2).await;
            assert!(same_rep.get("error").is_none(), "{same_rep}");
            let s_same_id = same_rep["result"]["session_id"].as_str().unwrap().to_string();
            let s_same_path = same_rep["result"]["path"].as_str().unwrap().to_string();
            assert!(
                prompt_and_wait(&mut ocr, &mut ocw, 3, &s_same_id, json!("same idle msg")).await.get("error").is_none()
            );
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":4,"method":"session.close","params":{"session_id":s_same_id}}),
            )
            .await;
            assert!(read_reply(&mut ocr, 4).await.get("error").is_none());

            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":5,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy(),"name":"other-idle"}}),
            )
            .await;
            let other_rep = read_reply(&mut ocr, 5).await;
            assert!(other_rep.get("error").is_none(), "{other_rep}");
            let s_other_id = other_rep["result"]["session_id"].as_str().unwrap().to_string();
            let s_other_path = other_rep["result"]["path"].as_str().unwrap().to_string();
            assert!(
                prompt_and_wait(&mut ocr, &mut ocw, 6, &s_other_id, json!("OTHER_PROJECT_PRIVATE"))
                    .await
                    .get("error")
                    .is_none()
            );
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":7,"method":"session.close","params":{"session_id":s_other_id}}),
            )
            .await;
            assert!(read_reply(&mut ocr, 7).await.get("error").is_none());
            let other_before = std::fs::read_to_string(&s_other_path).unwrap();

            // Connect peer with archive/unarchive/delete/snapshot granted.
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            let mut id = 20i64;
            for method in ["session.snapshot", "session.archive", "session.unarchive", "session.delete"] {
                // Path to other project session -> denied.
                id += 1;
                rpc_send(
                    &mut pcw,
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"session_id":s_other_path}}),
                )
                .await;
                let r_other_path = read_reply(&mut pcr, id).await;
                assert_eq!(
                    r_other_path["error"]["data"]["code"], "denied",
                    "{method} must deny peer passing cross-project file path: {r_other_path}"
                );

                // Path to same project session -> denied.
                id += 1;
                rpc_send(
                    &mut pcw,
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"session_id":s_same_path}}),
                )
                .await;
                let r_same_path = read_reply(&mut pcr, id).await;
                assert_eq!(
                    r_same_path["error"]["data"]["code"], "denied",
                    "{method} must deny peer passing same-project file path: {r_same_path}"
                );

                // Cross-project session id -> fail closed (`unknown_session`).
                id += 1;
                rpc_send(
                    &mut pcw,
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"session_id":s_other_id}}),
                )
                .await;
                let r_other_id = read_reply(&mut pcr, id).await;
                assert_eq!(
                    r_other_id["error"]["data"]["code"], "unknown_session",
                    "{method} must fail closed for peer passing cross-project session id: {r_other_id}"
                );
            }

            // Other project's session file must be completely untouched on disk.
            assert_eq!(
                std::fs::read_to_string(&s_other_path).unwrap(),
                other_before,
                "other project session file must remain untouched"
            );

            // Positive checks: authorized peer CAN snapshot, archive, unarchive, and delete a
            // same-project inactive session by its session_id.
            id += 1;
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":id,"method":"session.snapshot","params":{"session_id":s_same_id}}),
            )
            .await;
            let snap_ok = read_reply(&mut pcr, id).await;
            assert!(snap_ok.get("error").is_none(), "same-project snapshot by id must succeed: {snap_ok}");
            let snap_id = snap_ok["result"]["session_id"].as_str().unwrap().to_string();

            id += 1;
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":id,"method":"session.archive","params":{"session_id":snap_id}}),
            )
            .await;
            assert!(read_reply(&mut pcr, id).await.get("error").is_none(), "same-project archive by id must succeed");

            id += 1;
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":id,"method":"session.unarchive","params":{"session_id":snap_id}}),
            )
            .await;
            assert!(read_reply(&mut pcr, id).await.get("error").is_none(), "same-project unarchive by id must succeed");

            id += 1;
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":id,"method":"session.delete","params":{"session_id":snap_id}}),
            )
            .await;
            assert!(read_reply(&mut pcr, id).await.get("error").is_none(), "same-project delete by id must succeed");

            drop((pcw, pcr, ocw, ocr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Once the owner switches `default_session_id` to a sibling project (`dir2`), a peer's
/// `initialize` snapshot and default-session calls (`list`, `get`, `rewind` with or without
/// `expected_epoch`, omitting `session_id`) must never touch or expose `dir2`'s session.
#[tokio::test]
async fn peer_default_session_calls_never_escape_project_after_owner_switches_default() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _guard, rpc, mut ocr, mut ocw, owner_task, s_same) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let same_rt = rpc.manager.session(&s_same).unwrap();
            allow(&same_rt.host, &["session.list", "session.get", "session.rewind"]);
            assert!(prompt_and_wait(&mut ocr, &mut ocw, 2, &s_same, json!("same turn 1")).await.get("error").is_none());

            // Owner creates a session in `dir2` -> server `default_session_id` switches to `s_other`.
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy(),"name":"other-default"}}),
            )
            .await;
            let other_rep = read_reply(&mut ocr, 3).await;
            assert!(other_rep.get("error").is_none(), "{other_rep}");
            let s_other = other_rep["result"]["session_id"].as_str().unwrap().to_string();
            let other_rt = rpc.manager.session(&s_other).unwrap();
            allow(&other_rt.host, &["session.list", "session.get", "session.rewind"]);
            assert!(prompt_and_wait(&mut ocr, &mut ocw, 4, &s_other, json!("other turn 1")).await.get("error").is_none());
            assert!(prompt_and_wait(&mut ocr, &mut ocw, 5, &s_other, json!("other turn 2")).await.get("error").is_none());

            // Fetch `s_other`'s live `session_epoch` and `leaf_id` as owner.
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":6,"method":"session.get","params":{"session_id":s_other}}))
                .await;
            let other_snap_before = read_reply(&mut ocr, 6).await;
            let s_other_epoch = other_snap_before["result"]["session_epoch"].as_str().unwrap().to_string();
            let s_other_leaf_before = other_snap_before["result"]["leaf_entry_id"].as_str().unwrap().to_string();

            // Case A: `s_same` is still open in `dir1`, while `default_session_id == s_other` (`dir2`).
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            let p_init = read_reply(&mut pcr, 1).await;
            assert!(p_init.get("error").is_none(), "{p_init}");
            assert_ne!(
                p_init["result"]["session"]["session_id"].as_str(),
                Some(s_other.as_str()),
                "peer initialize must NOT expose cross-project default session_id: {p_init}"
            );
            assert_eq!(
                p_init["result"]["session"]["session_id"].as_str(),
                Some(s_same.as_str()),
                "peer initialize must stay within host project: {p_init}"
            );

            // Peer `session.list` (no cwd) must only list `dir1` sessions, never `s_other`.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":10,"method":"session.list","params":{}})).await;
            let list_rep = read_reply(&mut pcr, 10).await;
            assert!(list_rep.get("error").is_none(), "{list_rep}");
            let listed_ids: Vec<&str> = list_rep["result"]["data"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|row| row["session_id"].as_str())
                .collect();
            assert!(!listed_ids.contains(&s_other.as_str()), "peer session.list leaked dir2 session: {list_rep}");

            // Peer `session.get` targeting `s_other` fails closed.
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":11,"method":"session.get","params":{"session_id":s_other}}))
                .await;
            let get_rep = read_reply(&mut pcr, 11).await;
            assert_eq!(
                get_rep["error"]["data"]["code"], "unknown_session",
                "peer session.get must fail closed on cross-project default session: {get_rep}"
            );

            // Peer `session.rewind` with `expected_epoch = s_other_epoch` and NO `session_id` MUST NOT rewind `s_other`.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":12,"method":"session.rewind","params":{"expected_epoch":s_other_epoch}}),
            )
            .await;
            let rewind_epoch_rep = read_reply(&mut pcr, 12).await;
            assert_eq!(
                rewind_epoch_rep["error"]["data"]["code"], "invalid_arguments",
                "peer session.rewind with cross-project expected_epoch and no session_id must fail: {rewind_epoch_rep}"
            );

            // Verify `s_other` was NOT rewound.
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":7,"method":"session.get","params":{"session_id":s_other}}))
                .await;
            let other_snap_mid = read_reply(&mut ocr, 7).await;
            assert_eq!(
                other_snap_mid["result"]["leaf_entry_id"].as_str(),
                Some(s_other_leaf_before.as_str()),
                "s_other must be untouched after Case A peer rewind: {other_snap_mid}"
            );

            drop((pcw, pcr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();

            // Case B: owner closes `s_same`, leaving ONLY `s_other` (`dir2`) active in the server.
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":8,"method":"session.close","params":{"session_id":s_same}}),
            )
            .await;
            assert!(read_reply(&mut ocr, 8).await.get("error").is_none());

            let (pc2, ps2) = tokio::io::duplex(65536);
            let (ps2r, ps2w) = tokio::io::split(ps2);
            let peer_task2 = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), ps2r, ps2w, None));
            let (pcr2, mut pcw2) = tokio::io::split(pc2);
            let mut pcr2 = BufReader::new(pcr2);
            rpc_send(
                &mut pcw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            let p_init2 = read_reply(&mut pcr2, 1).await;
            assert!(p_init2.get("error").is_none(), "{p_init2}");
            assert!(
                p_init2["result"].get("session").is_none() || p_init2["result"]["session"].is_null(),
                "peer initialize must not leak dir2 session when no dir1 session is active: {p_init2}"
            );

            // Ensure fallback host in `dir1` has `peer.allow` set so the test exercises `resolve_session_target`, not `denied`.
            let proj_anchor = rpc.ensure_project_anchor_host().await.unwrap();
            allow(&proj_anchor, &["session.list", "session.get", "session.rewind"]);

            // Peer `session.rewind` with `expected_epoch = s_other_epoch` and NO `session_id` must be rejected.
            rpc_send(
                &mut pcw2,
                json!({"jsonrpc":"2.0","id":20,"method":"session.rewind","params":{"expected_epoch":s_other_epoch}}),
            )
            .await;
            let rewind_b = read_reply(&mut pcr2, 20).await;
            assert_eq!(
                rewind_b["error"]["data"]["code"], "invalid_arguments",
                "peer session.rewind without session_id must fail closed: {rewind_b}"
            );

            // Peer `session.rewind` with NO `session_id` and NO `expected_epoch` must not rewind `s_other`.
            rpc_send(&mut pcw2, json!({"jsonrpc":"2.0","id":21,"method":"session.rewind","params":{}})).await;
            let _rewind_b2 = read_reply(&mut pcr2, 21).await;

            // Verify `s_other` is STILL completely untouched.
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":9,"method":"session.get","params":{"session_id":s_other}}))
                .await;
            let other_snap_after = read_reply(&mut ocr, 9).await;
            assert_eq!(
                other_snap_after["result"]["leaf_entry_id"].as_str(),
                Some(s_other_leaf_before.as_str()),
                "s_other must be untouched after Case B peer rewind: {other_snap_after}"
            );

            drop((pcw2, pcr2, ocw, ocr));
            tokio::time::timeout(Duration::from_secs(5), peer_task2).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn plugin_admin_true_method_is_denied_for_non_admin_remote_owner() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _hg = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let plugins = home.path().join("config/plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            std::fs::write(
                plugins.join("vault.lua"),
                r#"return {
  name = "vault",
  provide = { "vault" },
  contracts = {
    vault = {
      doc = "Test plugin declaring an admin-gated remote method.",
      methods = {
        secret = {
          doc = "Privileged operation requiring remote_admin.",
          expose = "both",
          admin = true,
          scope = "host",
          params = { type = "object", properties = {}, additionalProperties = false },
          order = {},
          result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
        },
        public_info = {
          doc = "Non-admin owner operation.",
          expose = "both",
          scope = "host",
          params = { type = "object", properties = {}, additionalProperties = false },
          order = {},
          result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
        },
      },
    },
  },
  apply = function(ctx)
    local Vault = require("cordis").class("Vault", mare.Service)
    function Vault:constructor(c)
      Vault.super.constructor(self, c, "vault")
    end
    function Vault:secret() return { ok = true } end
    function Vault:public_info() return { ok = true } end
    ctx:plugin(Vault)
  end,
}"#,
            )
            .unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "webserver": { "remote_admin": false },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let mgr = mare_host::HostManager::new();
            let s1 = mgr.create_session(opts.clone()).await.unwrap();
            assert!(
                s1.host.kernel.service_provider("vault").is_some(),
                "vault plugin with admin=true contract must load cleanly"
            );
            let rpc = RpcRuntime::new(mgr, opts, s1.host.clone(), Some(s1.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await["error"].is_null());

            // Plugin method declaring `admin = true` MUST be rejected when remote_admin = false.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"vault.secret","params":{}})).await;
            let secret_rep = read_reply(&mut cr, 2).await;
            assert_eq!(
                secret_rep["error"]["data"]["code"], "denied",
                "plugin admin=true method must be denied for non-admin remote owner: {secret_rep}"
            );

            // Non-admin method on the same plugin MUST succeed.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"vault.public_info","params":{}})).await;
            let pub_rep = read_reply(&mut cr, 3).await;
            assert_eq!(pub_rep["result"]["ok"], true, "non-admin method must succeed: {pub_rep}");

            // Built-in workspace.set_trust (admin=true) MUST also be denied.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"workspace.set_trust","params":{"cwd":dir.path().to_string_lossy(),"trusted":true}}),
            )
            .await;
            let ws_rep = read_reply(&mut cr, 4).await;
            assert_eq!(
                ws_rep["error"]["data"]["code"], "denied",
                "workspace.set_trust must be denied for non-admin remote owner: {ws_rep}"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn concurrent_create_response_precedes_new_session_events() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Subscribe to all sessions so newly created sessions auto-subscribe on this connection.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"scope":"all"}}))
                .await;
            let sub = read_reply(&mut cr, 2).await;
            assert!(sub["error"].is_null(), "{sub}");

            // 1. Deterministic request-local isolation check:
            //    Task A opens a new runtime inside its own REQUEST_ACTIVATIONS scope and pauses before
            //    returning; concurrent Task B runs an unrelated contract call (`session.list`) via
            //    `dispatch::handle_with_activations`. Task B must NOT steal Task A's queued activation, and Task A's
            //    activation must remain intact in Task A's scope (and be discarded if a call errors).
            let conn = rpc.connections.borrow().values().find_map(|w| w.upgrade()).expect("active owner connection");
            let anchor = rpc.ensure_live_anchor_host().await.unwrap();
            let caller = super::session_ext::caller_for_conn(&conn, &anchor);
            let rt_paused =
                rpc.manager.create_session(rpc.clone_clean_session_opts(None, Some(false)).unwrap()).await.unwrap();
            let paused_sid = rt_paused.session_id.clone();

            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let conn_a = conn.clone();
            let rpc_a = rpc.clone();
            let rt_a = rt_paused.clone();
            let entered_a = entered.clone();
            let release_a = release.clone();
            let task_a = tokio::task::spawn_local(async move {
                super::services::REQUEST_ACTIVATIONS
                    .scope(std::cell::RefCell::new(Vec::new()), async {
                        super::services::CALL_CONN
                            .scope(Rc::downgrade(&conn_a), async {
                                rpc_a.manager.orchestrator().ext().opened(&caller, &rt_a, false, None).await;
                            })
                            .await;
                        entered_a.notify_one();
                        release_a.notified().await;
                        super::services::REQUEST_ACTIVATIONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()))
                    })
                    .await
            });

            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("task A must finish opened() and pause");

            // Concurrent contract call B runs while task A is paused after opened().
            let (res_b, acts_b) =
                super::dispatch::handle_with_activations(&conn, "session.list", &Some(json!({}))).await;
            assert!(res_b.is_ok(), "concurrent session.list must succeed: {res_b:?}");
            assert!(
                acts_b.is_empty(),
                "concurrent request B must not steal request A's pending activations (got {})",
                acts_b.len()
            );
            assert!(
                !conn.subscriptions.borrow().contains_key(&paused_sid),
                "new session must not be activated before request A completes"
            );

            // Release task A: its own request-local scope still holds the activation (and dropping
            // it without calling activate_subscriptions simulates an erroring request leaving no
            // stale activation behind on `conn`).
            release.notify_one();
            let acts_a = task_a.await.unwrap();
            assert_eq!(acts_a.len(), 1, "task A must retain its own request-local activation");
            drop(acts_a);
            assert!(
                !conn.subscriptions.borrow().contains_key(&paused_sid),
                "dropped activation from an erroring request must not leak onto conn"
            );

            // 2. End-to-end wire concurrency check: fire `session.create` concurrently with rapid
            //    `agent.get` calls and verify `session.create`'s reply precedes any event for the new session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"session.create","params":{}})).await;
            for req_id in 11..=18u64 {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":req_id,"method":"agent.get","params":{"session_id":s1}}))
                    .await;
            }
            let mut pre_create_frames: Vec<J> = Vec::new();
            let mut create_sid = None;
            let mut saw_new_event = false;
            let mut fast_replies = 0usize;
            tokio::time::timeout(Duration::from_secs(10), async {
                while create_sid.is_none() || !saw_new_event || fast_replies < 8 {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("id") == Some(&json!(10)) {
                        assert!(msg["error"].is_null(), "session.create failed: {msg}");
                        create_sid = Some(msg["result"]["session_id"].as_str().expect("session_id").to_string());
                    } else if let Some(id) = msg.get("id").and_then(J::as_u64)
                        && (11..=18).contains(&id)
                    {
                        assert!(msg["error"].is_null(), "concurrent agent.get failed: {msg}");
                        fast_replies += 1;
                    } else if let Some(ref new_sid) = create_sid {
                        if msg.get("method").and_then(J::as_str) == Some("event")
                            && msg["params"].get("session_id").and_then(J::as_str) == Some(new_sid.as_str())
                        {
                            saw_new_event = true;
                        }
                    } else {
                        pre_create_frames.push(msg);
                    }
                }
            })
            .await
            .expect("timed out waiting for concurrent session.create + new-session event + fast replies");

            let new_sid = create_sid.unwrap();
            for frame in &pre_create_frames {
                let frame_sid = frame.get("params").and_then(|p| p.get("session_id")).and_then(J::as_str);
                assert_ne!(
                    frame_sid,
                    Some(new_sid.as_str()),
                    "no event for new session `{new_sid}` may arrive before session.create's own response: {frame}"
                );
            }
            assert!(saw_new_event, "expected auto-subscribed event for `{new_sid}` after session.create reply");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn lua_initiated_session_create_auto_subscribes_owner_connection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Subscribe to all sessions on the owner connection.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"scope":"all"}}))
                .await;
            let sub = read_reply(&mut cr, 2).await;
            assert!(sub["error"].is_null(), "{sub}");

            // Trigger `/new` through `commands.run`, which calls `ctx.session:create()` from Lua
            // rather than directly calling the RPC `session.create` method.
            let wait_budget = crate::testkit::scaled_test_timeout(Duration::from_secs(5));
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"commands.run","params":{"line":"/new"}})).await;
            let mut saw_cmd_reply = false;
            let mut saw_new_event = None;
            tokio::time::timeout(wait_budget, async {
                while !saw_cmd_reply || saw_new_event.is_none() {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("id") == Some(&json!(3)) {
                        assert!(msg["error"].is_null(), "commands.run /new failed: {msg}");
                        saw_cmd_reply = true;
                    } else if msg.get("method").and_then(J::as_str) == Some("event")
                        && let Some(sid) = msg["params"].get("session_id").and_then(J::as_str)
                        && sid != s1
                    {
                        saw_new_event = Some(sid.to_owned());
                    }
                }
            })
            .await
            .expect("owner connection must automatically subscribe to Lua-initiated /new session");
            let new_sid = saw_new_event.unwrap();
            assert!(rpc.manager.session(&new_sid).is_ok(), "new session `{new_sid}` must exist in HostManager");

            drop((cw, cr));
            tokio::time::timeout(wait_budget, task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn agent_cancel_sends_cancel_request_for_pending_approval() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let target_dir = tempfile::tempdir().unwrap();
            let target_file = target_dir.path().join("cancelled-write.txt");
            let bash_target = target_file.to_string_lossy().replace('\\', "/");
            let script = json!([
                {"tool_calls": [{"name": "bash", "arguments": {"command": format!("touch '{bash_target}'"), "sandbox_permissions": "all", "justification": "tdd"}}]},
                {"text": "never-done"}
            ]);
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, sid) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "plugins": { "provider-faux": { "script_json": script.to_string() } }
                }),
                json!({ "permission": "read-only" }),
                false,
            )
            .await;

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"go"}}),
            )
            .await;
            let mut p2 = None;
            let mut approval_req = None;
            tokio::time::timeout(Duration::from_secs(5), async {
                while p2.is_none() || approval_req.is_none() {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("method").is_none() && msg["id"] == 2 {
                        assert!(msg["error"].is_null(), "{msg}");
                        p2 = Some(msg);
                    } else if msg.get("method").and_then(J::as_str) == Some(methods::APPROVAL_REQUEST) {
                        approval_req = Some(msg);
                    }
                }
            })
            .await
            .expect("expected agent.prompt reply and approval.request");
            let approval_id = approval_req.unwrap()["id"].clone();

            // Calling agent.cancel on the session MUST cancel the pending approval.request via $/cancel_request.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"agent.cancel","params":{"session_id":sid}})).await;
            let mut saw_cancel_req = false;
            let mut saw_reply = false;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !saw_cancel_req || !saw_reply {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("method").and_then(J::as_str) == Some("$/cancel_request")
                        && msg["params"].get("id") == Some(&approval_id)
                    {
                        saw_cancel_req = true;
                    }
                    if msg.get("id") == Some(&json!(3)) {
                        assert!(msg["error"].is_null(), "{msg}");
                        saw_reply = true;
                    }
                }
            })
            .await
            .expect("agent.cancel must send $/cancel_request for pending approval.request");
            assert!(!target_file.exists(), "cancelled tool must not execute");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn workspace_set_trust_ignores_cancel_after_commit_and_updates_all_sessions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _hg = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: false,
            };
            let mgr = mare_host::HostManager::new();
            let rt1 = mgr.create_session(opts.clone()).await.unwrap();
            let rt2 = mgr.create_session(opts.clone()).await.unwrap();
            assert!(!rt1.host.workspace.is_trusted());
            assert!(!rt2.host.workspace.is_trusted());

            // Pause inside the first host's `trust/changed` listener before the second host runs.
            let (paused_tx, mut paused_rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
            let (resume_tx, resume_rx) = tokio::sync::watch::channel(false);
            for (idx, rt) in [(1u8, &rt1), (2u8, &rt2)] {
                let ptx = paused_tx.clone();
                let rrx = resume_rx.clone();
                let on_trust = rt
                    .host
                    .lua()
                    .create_async_function(move |_, ev: mlua::Table| {
                        let ptx = ptx.clone();
                        let mut rrx = rrx.clone();
                        async move {
                            if ev.get::<bool>("trusted").unwrap_or(false) {
                                let _ = ptx.send(idx);
                                let _ = rrx.wait_for(|&go| go).await;
                            }
                            Ok(())
                        }
                    })
                    .unwrap();
                let c = mare_kernel::test_ctx(&rt.host.kernel).unwrap();
                let cordis: mlua::Table = c.as_table().expect("test_ctx table").get("cordis").unwrap();
                let on: mlua::Function = cordis.get("on").unwrap();
                on.call::<mlua::Value>((cordis, "trust/changed", on_trust)).unwrap();
            }

            let rpc = RpcRuntime::new(mgr, opts, rt1.host.clone(), Some(rt1.session_id.clone()), false);
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await["error"].is_null());

            // Start workspace.set_trust across both sessions sharing `dir`.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "workspace.set_trust",
                    "params": { "cwd": dir.path().to_string_lossy(), "trusted": true, "save": false }
                }),
            )
            .await;

            // Wait until the first host finishes its trust update and blocks inside its trust/changed listener.
            let first = tokio::time::timeout(Duration::from_secs(5), paused_rx.recv())
                .await
                .expect("first host must pause in trust/changed")
                .expect("paused_tx stays live");
            // At this exact point, one host is already trusted while the other is still untrusted.
            assert_ne!(
                rt1.host.workspace.is_trusted(),
                rt2.host.workspace.is_trusted(),
                "mid-flight workspace.set_trust should have updated exactly one host so far (first={first})"
            );

            // Send $/cancel_request for id=2 followed by a fence request id=3 so we know cancellation was processed.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":2}})).await;
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"flags.list","params":{}})).await;
            let fence = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("id") == Some(&json!(2)) {
                        panic!("workspace.set_trust must not reply/cancel before completing all hosts: {msg}");
                    }
                    if msg.get("id") == Some(&json!(3)) {
                        return msg;
                    }
                }
            })
            .await
            .expect("fence flags.list must reply");
            assert!(fence["error"].is_null(), "{fence}");

            // Unblock both hosts so workspace.set_trust finishes applying trust to all sessions.
            let _ = resume_tx.send(true);

            let set_reply = tokio::time::timeout(Duration::from_secs(5), read_reply(&mut cr, 2))
                .await
                .expect("committed workspace.set_trust must return its completed reply");
            assert!(set_reply["error"].is_null(), "committed workspace.set_trust must not fail: {set_reply}");
            assert!(rt1.host.workspace.is_trusted(), "session 1 must be trusted");
            assert!(rt2.host.workspace.is_trusted(), "session 2 must be trusted");
            let t1: bool = rt1.host.lua().load("return mare.trusted == true").eval().unwrap();
            let t2: bool = rt2.host.lua().load("return mare.trusted == true").eval().unwrap();
            assert!(t1 && t2, "both session VMs must see mare.trusted == true");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn agent_continue_injects_caller_source_for_owner_and_peer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _hg = EnvGuard::set("MARE_HOME", home.path());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "peer": { "allow": ["agent.continue"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            };
            let mgr = mare_host::HostManager::new();
            let rt = mgr.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();

            // Wrap `agent.continue` on the session host to record the `source` option passed by the RPC bridge.
            let agent_tbl = rt.host.service_table("agent").expect("agent service table");
            let wrap: mlua::Function = rt
                .host
                .lua()
                .load(
                    r#"
                    local agent = ...
                    SEEN_CONTINUE_SOURCES = {}
                    agent.continue = function(_self, opts)
                        SEEN_CONTINUE_SOURCES[#SEEN_CONTINUE_SOURCES + 1] =
                            (type(opts) == "table" and opts.source) or "NIL"
                        return { turn_id = "t-probe" }
                    end
                    "#,
                )
                .into_function()
                .unwrap();
            wrap.call::<()>(agent_tbl).unwrap();

            let rpc = RpcRuntime::new(mgr, opts, rt.host.clone(), Some(sid.clone()), false);

            // Owner connection: omitted source defaults to "rpc"; explicit source is preserved.
            let (oc, os) = tokio::io::duplex(65536);
            let (osr, osw) = tokio::io::split(os);
            let (ocr, mut ocw) = tokio::io::split(oc);
            let mut ocr = BufReader::new(ocr);
            let owner_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), osr, osw, true, None, std::future::pending()));
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut ocr, 1).await["error"].is_null());

            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":2,"method":"agent.continue","params":{"session_id":sid}}))
                .await;
            let r2 = read_reply(&mut ocr, 2).await;
            assert!(r2["error"].is_null(), "{r2}");

            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.continue","params":{"session_id":sid,"source":"tui"}}),
            )
            .await;
            let r3 = read_reply(&mut ocr, 3).await;
            assert!(r3["error"].is_null(), "{r3}");

            // Peer connection: both omitted and forged source are forced to "peer".
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            let peer_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), psr, psw, false, None, std::future::pending()));
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":10,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 10).await["error"].is_null());

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":11,"method":"agent.continue","params":{"session_id":sid}}))
                .await;
            let r11 = read_reply(&mut pcr, 11).await;
            assert!(r11["error"].is_null(), "{r11}");

            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":12,"method":"agent.continue","params":{"session_id":sid,"source":"rpc"}}),
            )
            .await;
            let r12 = read_reply(&mut pcr, 12).await;
            assert!(r12["error"].is_null(), "{r12}");

            let seen: Vec<String> = rt.host.lua().load("return SEEN_CONTINUE_SOURCES").eval().unwrap();
            assert_eq!(seen, vec!["rpc", "tui", "peer", "peer"]);

            drop((ocw, ocr, pcw, pcr));
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn permissions_rpc_get_list_set_multi_session_and_access_control() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "webserver": { "remote_admin": false },
                    "plugins": {
                        "permissions": {
                            "enabled": true,
                            "preset": "auto",
                            "presets": {
                                "strict-ro": { "like": "read-only", "desc": "custom strict read-only" }
                            },
                            "rules": [
                                { "argv": ["echo"], "decision": "allow" }
                            ]
                        }
                    }
                }),
                json!({}),
                true,
            )
            .await;

            // Create a second session `s2` via RPC.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let created = read_reply(&mut cr, 2).await;
            assert!(created["error"].is_null(), "session.create failed: {created}");
            let s2 = created["result"]["session_id"].as_str().expect("s2 session_id").to_owned();
            assert_ne!(s1, s2);

            // 1. permissions.list returns built-in + custom presets with `current` marker.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"permissions.list","params":{"session_id":s1}}))
                .await;
            let list1 = read_reply(&mut cr, 3).await;
            assert!(list1["error"].is_null(), "permissions.list failed: {list1}");
            let items = list1["result"]["data"].as_array().expect("data array");
            let names: Vec<&str> = items.iter().filter_map(|it| it["name"].as_str()).collect();
            for expected in ["read-only", "auto", "full-access", "strict-ro"] {
                assert!(names.contains(&expected), "missing preset `{expected}` in {names:?}");
            }
            let current_names: Vec<&str> = items
                .iter()
                .filter(|it| it["current"] == json!(true))
                .filter_map(|it| it["name"].as_str())
                .collect();
            assert_eq!(current_names, vec!["auto"], "only auto should be current on s1: {items:?}");

            // 2. permissions.get returns current preset, metadata, effective rules, grants, policy_revision.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"permissions.get","params":{"session_id":s1}}))
                .await;
            let get1 = read_reply(&mut cr, 4).await;
            assert!(get1["error"].is_null(), "permissions.get failed: {get1}");
            assert_eq!(get1["result"]["preset"], "auto");
            assert_eq!(get1["result"]["label"], "AUTO");
            assert_eq!(get1["result"]["level"], "workspace-write");
            assert_eq!(get1["result"]["like"], "auto");
            assert!(get1["result"]["policy_revision"].is_u64());
            assert!(get1["result"]["grants"].is_object());
            let rules = get1["result"]["rules"].as_array().expect("rules array");
            assert!(
                rules.iter().any(|r| r["argv"] == json!(["echo"]) && r["decision"] == "allow"),
                "effective rules must include configured echo allow rule: {rules:?}"
            );

            // 3. permissions.set switches s2 independently of s1 and returns updated permission_state.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"permissions.set","params":{"session_id":s2,"name":"strict-ro"}}),
            )
            .await;
            let set2 = read_reply(&mut cr, 5).await;
            assert!(set2["error"].is_null(), "permissions.set on s2 failed: {set2}");
            assert_eq!(set2["result"]["preset"], "strict-ro");
            assert_eq!(set2["result"]["level"], "read-only");
            assert_eq!(set2["result"]["like"], "read-only");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"permissions.get","params":{"session_id":s1}}))
                .await;
            let get1_after = read_reply(&mut cr, 6).await;
            assert_eq!(get1_after["result"]["preset"], "auto", "s1 preset must remain unchanged: {get1_after}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":7,"method":"permissions.list","params":{"session_id":s2}}))
                .await;
            let list2 = read_reply(&mut cr, 7).await;
            let cur2: Vec<&str> = list2["result"]["data"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|it| it["current"] == json!(true))
                .filter_map(|it| it["name"].as_str())
                .collect();
            assert_eq!(cur2, vec!["strict-ro"]);

            // 4. Unknown preset returns coded error `permissions.unknown_preset`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"permissions.set","params":{"session_id":s2,"name":"no-such-preset"}}),
            )
            .await;
            let bad = read_reply(&mut cr, 8).await;
            assert_eq!(
                bad["error"]["data"]["code"], "permissions.unknown_preset",
                "unknown preset must return permissions.unknown_preset: {bad}"
            );

            // 5. Remote owner with webserver.remote_admin=false can read (get/list) but cannot mutate (set).
            let (rc, rs) = tokio::io::duplex(65536);
            let (rsr, rsw) = tokio::io::split(rs);
            let (rcr, mut rcw) = tokio::io::split(rc);
            let mut rcr = BufReader::new(rcr);
            let remote_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":20,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut rcr, 20).await["error"].is_null());

            rpc_send(&mut rcw, json!({"jsonrpc":"2.0","id":21,"method":"permissions.get","params":{"session_id":s1}}))
                .await;
            let r_get = read_reply(&mut rcr, 21).await;
            assert!(r_get["error"].is_null(), "remote non-admin owner must read permissions.get: {r_get}");
            assert_eq!(r_get["result"]["preset"], "auto");

            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":22,"method":"permissions.list","params":{"session_id":s1}}),
            )
            .await;
            let r_list = read_reply(&mut rcr, 22).await;
            assert!(r_list["error"].is_null(), "remote non-admin owner must read permissions.list: {r_list}");

            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":23,"method":"permissions.set","params":{"session_id":s1,"name":"read-only"}}),
            )
            .await;
            let r_set = read_reply(&mut rcr, 23).await;
            assert_eq!(
                r_set["error"]["data"]["code"], "denied",
                "remote non-admin owner must be denied permissions.set: {r_set}"
            );
            assert!(
                r_set["error"]["message"].as_str().unwrap_or_default().contains("remote_admin"),
                "expected remote_admin denial reason: {r_set}"
            );

            // 6. Peer connection: denied by default; peer.allow grants get/list, never set (access=primary).
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            let peer_task =
                tokio::task::spawn_local(serve_manager(rpc.clone(), psr, psw, false, None, std::future::pending()));
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":30,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 30).await["error"].is_null());

            for (id, meth, params) in [
                (31, "permissions.get", json!({"session_id": s1})),
                (32, "permissions.list", json!({"session_id": s1})),
                (33, "permissions.set", json!({"session_id": s1, "name": "read-only"})),
            ] {
                rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":id,"method":meth,"params":params})).await;
                let rep = read_reply(&mut pcr, id).await;
                assert_eq!(rep["error"]["data"]["code"], "denied", "default peer must be denied {meth}: {rep}");
            }

            let rt1 = rpc.manager.session(&s1).unwrap();
            let mut cfg1 = rt1.host.kernel.config();
            cfg1["peer"] = json!({"allow": ["permissions.get", "permissions.list", "permissions.set"]});
            rt1.host.kernel.set_config(cfg1);

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":34,"method":"permissions.get","params":{"session_id":s1}}))
                .await;
            let p_get = read_reply(&mut pcr, 34).await;
            assert!(p_get["error"].is_null(), "peer.allow must grant permissions.get: {p_get}");
            assert!(
                p_get["result"].get("rules").is_none() && p_get["result"].get("grants").is_none(),
                "peer permissions.get must omit admin-only rules and grants: {p_get}"
            );

            // llm.list_auth_keys requires admin: denied for remote non-admin owner and peers, allowed for local admin.
            rpc_send(&mut rcw, json!({"jsonrpc":"2.0","id":29,"method":"llm.list_auth_keys","params":{}})).await;
            let r_keys = read_reply(&mut rcr, 29).await;
            assert_eq!(
                r_keys["error"]["data"]["code"], "denied",
                "remote non-admin owner must be denied llm.list_auth_keys: {r_keys}"
            );
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":37,"method":"llm.list_auth_keys","params":{}})).await;
            let p_keys = read_reply(&mut pcr, 37).await;
            assert_eq!(
                p_keys["error"]["data"]["code"], "denied",
                "peer must be denied llm.list_auth_keys: {p_keys}"
            );
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":38,"method":"llm.list_auth_keys","params":{}})).await;
            let o_keys = read_reply(&mut cr, 38).await;
            assert!(o_keys["error"].is_null(), "local admin owner must be allowed llm.list_auth_keys: {o_keys}");
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":35,"method":"permissions.list","params":{"session_id":s1}}))
                .await;
            assert!(read_reply(&mut pcr, 35).await["error"].is_null(), "peer.allow must grant permissions.list");
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":36,"method":"permissions.set","params":{"session_id":s1,"name":"read-only"}}),
            )
            .await;
            let p_set = read_reply(&mut pcr, 36).await;
            assert_eq!(
                p_set["error"]["data"]["code"], "denied",
                "peer.allow must never grant primary-only permissions.set: {p_set}"
            );

            drop((cw, cr, rcw, rcr, pcw, pcr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), remote_task).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn commands_run_optional_session_id_routes_and_validates() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Enable UI callbacks on owner connection so we can verify CALL_SESSION propagation on reverse callbacks.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "connection.set_capabilities",
                    "params": { "capabilities": { "callbacks": true, "ui": true, "approvals": false, "elicitation": false } }
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 2).await["error"].is_null());

            // Create a second loaded session `s2`. (Wait: check which session is currently anchor.)
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.create","params":{}})).await;
            let created = read_reply(&mut cr, 3).await;
            assert!(created["error"].is_null(), "{created}");
            let s2 = created["result"]["session_id"].as_str().expect("s2").to_owned();
            assert_ne!(s1, s2);
            let anchor_sid = rpc.anchor_session_id.borrow().clone().expect("anchor session");
            let other_sid = if anchor_sid == s1 { s2.clone() } else { s1.clone() };

            // 1. Omitting session_id runs on the current anchor session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"commands.run","params":{"line":"/name anchor-name"}}))
                .await;
            let r4 = read_reply(&mut cr, 4).await;
            assert!(r4["error"].is_null(), "commands.run without session_id failed: {r4}");

            // 2. Passing session_id=other_sid routes AWAY from anchor and acts only on `other_sid`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"commands.run","params":{"line":"/name targeted-name","session_id":other_sid}}),
            )
            .await;
            let r5 = read_reply(&mut cr, 5).await;
            assert!(r5["error"].is_null(), "commands.run with session_id=other_sid failed: {r5}");

            let rt_anchor = rpc.manager.session(&anchor_sid).unwrap();
            let rt_other = rpc.manager.session(&other_sid).unwrap();
            let snap_anchor = rt_anchor.host.session_snapshot(&anchor_sid).await.unwrap();
            let snap_other = rt_other.host.session_snapshot(&other_sid).await.unwrap();
            assert_eq!(
                snap_anchor["name"], "anchor-name",
                "anchor session must be renamed by commands.run without session_id: {snap_anchor}"
            );
            assert_eq!(
                snap_other["name"], "targeted-name",
                "non-anchor session must be renamed by commands.run with explicit session_id: {snap_other}"
            );

            // 3. Unknown session_id fails closed with `unknown_session` and does not fall back to anchor.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"commands.run","params":{"line":"/name ghost","session_id":"no-such-session"}}),
            )
            .await;
            let r8 = read_reply(&mut cr, 8).await;
            assert_eq!(
                r8["error"]["data"]["code"], "unknown_session",
                "unknown session_id must fail with unknown_session: {r8}"
            );
            let snap_anchor_after = rt_anchor.host.session_snapshot(&anchor_sid).await.unwrap();
            assert_eq!(
                snap_anchor_after["name"], "anchor-name",
                "failed session_id must not mutate anchor: {snap_anchor_after}"
            );

            // 4. Empty/whitespace session_id fails with `invalid_arguments`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"commands.run","params":{"line":"/help","session_id":"   "}}),
            )
            .await;
            let r10 = read_reply(&mut cr, 10).await;
            assert_eq!(
                r10["error"]["data"]["code"], "invalid_arguments",
                "blank session_id must fail with invalid_arguments: {r10}"
            );

            // 5. Reverse UI callbacks triggered by commands.run(session_id=other_sid) carry session_id=other_sid.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.attach","params":{"session_id":other_sid,"responder":true}}),
            )
            .await;
            let r11 = read_reply(&mut cr, 11).await;
            assert!(r11["error"].is_null(), "ui.attach failed: {r11}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"commands.run","params":{"line":"/model","session_id":other_sid}}),
            )
            .await;
            let mut saw_cb_sid = None;
            let mut cmd_reply = None;
            tokio::time::timeout(Duration::from_secs(5), async {
                while cmd_reply.is_none() {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("method").and_then(J::as_str) == Some(methods::UI_SELECT) {
                        saw_cb_sid = msg["params"].get("session_id").and_then(J::as_str).map(str::to_owned);
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"index":null}})).await;
                    } else if msg.get("id") == Some(&json!(12)) {
                        cmd_reply = Some(msg);
                    }
                }
            })
            .await
            .expect("commands.run /model must complete");
            let reply12 = cmd_reply.unwrap();
            assert!(reply12["error"].is_null(), "interactive commands.run on non-anchor session failed: {reply12}");
            assert_eq!(
                saw_cb_sid.as_deref(),
                Some(other_sid.as_str()),
                "ui.select callback must carry targeted non-anchor session_id"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn replay_receipt_does_not_return_closed_session_and_close_clears_receipts() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, _s1) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();
            let create_params = json!({
                "cwd": dir2.path().to_string_lossy(),
                "request_id": "req-create-then-close"
            });

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"session.create","params":create_params})).await;
            let r1 = read_reply(&mut cr, 10).await;
            assert!(r1["error"].is_null(), "{r1}");
            let created_sid = r1["result"]["session_id"].as_str().unwrap().to_owned();

            // While alive, replaying with the same request_id still returns the live session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"session.create","params":create_params})).await;
            let r_replay = read_reply(&mut cr, 11).await;
            assert!(r_replay["error"].is_null(), "{r_replay}");
            assert_eq!(r_replay["result"]["session_id"].as_str(), Some(created_sid.as_str()));

            // Close the created session.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"session.close","params":{"session_id":created_sid}}),
            )
            .await;
            assert!(read_reply(&mut cr, 12).await["error"].is_null());

            // Retrying with the same request_id must NOT replay the dead session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":13,"method":"session.create","params":create_params})).await;
            let r2 = read_reply(&mut cr, 13).await;
            assert!(r2["error"].is_null(), "{r2}");
            let fresh_sid = r2["result"]["session_id"].as_str().unwrap().to_owned();
            assert_ne!(fresh_sid, created_sid, "closed session receipt must not replay dead session_id");
            assert!(rpc.manager.session(&fresh_sid).is_ok(), "re-executed session.create must yield a live session");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn sync_session_tracking_replaces_stale_host_on_same_session_id_reload() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _home_guard, _dir1, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let seeded = prompt_and_wait(&mut cr, &mut cw, 2, &s1, json!("seed reload")).await;
            assert!(seeded.get("error").is_none(), "{seeded}");
            let conn = rpc.connections.borrow().values().find_map(|w| w.upgrade()).expect("active owner connection");
            let host1 = rpc.manager.session(&s1).unwrap().host.clone();

            let mut tracking = HashMap::new();
            let _ = super::stream_pump::sync_session_tracking(&conn, &mut tracking);
            assert!(
                tracking.get(&s1).is_some_and(|(h, _)| Arc::ptr_eq(h, &host1)),
                "initial tracking must bind to host1"
            );

            // Simulate close + immediate reload of the exact same session_id before the next
            // watch tick runs sync_session_tracking.
            let _ = rpc.manager.close_session(&s1).await.unwrap();
            let mut opts = rpc.clone_clean_session_opts(None, Some(false)).unwrap();
            opts.args["resume"] = json!(s1);
            let rt2 = rpc.manager.load_session(Some(&s1), opts).await.unwrap();
            let host2 = rt2.host.clone();
            assert!(!Arc::ptr_eq(&host1, &host2), "reloaded session must have a distinct Host instance");

            let _ = super::stream_pump::sync_session_tracking(&conn, &mut tracking);
            assert!(
                tracking.get(&s1).is_some_and(|(h, _)| Arc::ptr_eq(h, &host2)),
                "sync_session_tracking must replace stale Host cursor when session_id reloads onto a new Host"
            );

            drop(conn);
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// A `remote_admin = false` remote owner must NOT probe, read, archive, unarchive, or delete
/// sessions by arbitrary filesystem path via `session.get` / `archive` / `unarchive` / `delete`,
/// by-id lookups must not trigger path-access checks, and `session.snapshot` receipts must be
/// caller-scoped and invalidated on `session/deleted`.
#[tokio::test]
async fn remote_admin_false_cannot_access_or_delete_sessions_by_path_and_snapshot_receipts_are_scoped_and_cleaned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc_with(
                json!({
                    "model": "faux/echo",
                    "webserver": { "remote_admin": false },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                json!({}),
                true,
            )
            .await;

            // Create an arbitrary outside session file with a sibling `.attachments` directory.
            let outside_dir = tempfile::tempdir().unwrap();
            let victim_path = outside_dir.path().join("victim.jsonl");
            let victim_atts = outside_dir.path().join("victim.attachments");
            std::fs::create_dir_all(&victim_atts).unwrap();
            std::fs::write(victim_atts.join("keep.txt"), "precious").unwrap();
            let victim_header = json!({
                "type": "session",
                "version": mare_store::CURRENT_VERSION,
                "session_id": "11111111-2222-3333-4444-555555555555",
                "cwd": outside_dir.path().to_string_lossy(),
                "name": "outside-secret",
                "created_at_ms": 1700000000000i64
            });
            std::fs::write(&victim_path, format!("{victim_header}\n")).unwrap();
            let victim_str = victim_path.to_string_lossy().to_string();

            // Connect a remote owner with `remote_admin = false`.
            let (r_client, r_server) = tokio::io::duplex(131072);
            let (rsr, rsw) = tokio::io::split(r_server);
            let remote_task = tokio::task::spawn_local(super::serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(r_client);
            let mut rcr = BufReader::new(rcr);
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            let _ = read_reply(&mut rcr, 1).await;

            // All four path-addressing calls MUST be denied for remote_admin=false owner.
            for (id, method) in [
                (10, "session.get"),
                (11, "session.archive"),
                (12, "session.unarchive"),
                (13, "session.delete"),
            ] {
                rpc_send(
                    &mut rcw,
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"session_id":victim_str}}),
                )
                .await;
                let rep = read_reply(&mut rcr, id).await;
                assert!(
                    rep.get("error").is_some(),
                    "remote_admin=false must NOT allow `{method}` by arbitrary filesystem path: {rep}"
                );
                assert_eq!(
                    rep["error"]["data"]["code"], "denied",
                    "`{method}` by path must fail with denied: {rep}"
                );
            }
            assert!(victim_path.exists(), "victim.jsonl must not be deleted by remote_admin=false");
            assert!(victim_atts.join("keep.txt").exists(), "sibling attachments dir must not be deleted");
            let lines = std::fs::read_to_string(&victim_path).unwrap().lines().count();
            assert_eq!(lines, 1, "victim.jsonl must not be modified by remote_admin=false archive/unarchive");

            // Also test session.snapshot receipt invalidation on session.delete.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":20,"method":"agent.prompt","params":{"session_id":s1,"input":"hello"}}),
            )
            .await;
            let p_rep = read_reply(&mut cr, 20).await;
            if let Some(tid) = p_rep["result"]["turn_id"].as_str() {
                let rt1 = rpc.manager.session(&s1).unwrap();
                let _ = call_service(&rt1.host, "agent", "wait", vec![json!(tid)]).await;
            }
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":21,"method":"session.snapshot","params":{"session_id":s1,"request_id":"snap-req-1"}}),
            )
            .await;
            let snap1 = read_reply(&mut cr, 21).await;
            assert!(snap1.get("error").is_none(), "session.snapshot #1 failed: {snap1}");
            let snap1_id = snap1["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":22,"method":"session.delete","params":{"session_id":snap1_id}}),
            )
            .await;
            let del = read_reply(&mut cr, 22).await;
            assert!(del.get("error").is_none(), "session.delete of snapshot failed: {del}");

            // Reusing the same request_id after deleting the snapshot must NOT replay the deleted snapshot.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":23,"method":"session.snapshot","params":{"session_id":s1,"request_id":"snap-req-1"}}),
            )
            .await;
            let snap2 = read_reply(&mut cr, 23).await;
            assert!(snap2.get("error").is_none(), "session.snapshot #2 failed: {snap2}");
            let snap2_id = snap2["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(
                snap2_id, snap1_id,
                "replaying request_id after deleting the previous snapshot must create a fresh snapshot, not return deleted {snap1_id}"
            );

            // Verify by-id session.get / archive / unarchive do NOT invoke `allows_session_path`
            // (no spurious path-denial audit on normal by-id selectors).
            let rt1 = rpc.manager.session(&s1).unwrap();
            let path_checks = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let path_checks_cb = path_checks.clone();
            let probe_ctx = rt1.host.kernel.open_owner("<id-only-probe>", "root").unwrap();
            rt1.host.kernel.set_caller_access(
                probe_ctx,
                Some(mare_kernel::CallerAccess {
                    is_peer: false,
                    allows: std::rc::Rc::new(|_, _, _| true),
                    allows_session_path: std::rc::Rc::new(move || {
                        path_checks_cb.set(path_checks_cb.get() + 1);
                        false
                    }),
                }),
            );
            let get_by_id = rt1
                .host
                .kernel
                .call_service_export(probe_ctx, None, "session", "get", json!({ "session_id": snap2_id }), Default::default())
                .await;
            assert!(get_by_id.is_ok(), "by-id session.get failed: {get_by_id:?}");
            let arch_by_id = rt1
                .host
                .kernel
                .call_service_export(probe_ctx, None, "session", "archive", json!({ "session_id": snap2_id }), Default::default())
                .await;
            assert!(arch_by_id.is_ok(), "by-id session.archive failed: {arch_by_id:?}");
            let unarch_by_id = rt1
                .host
                .kernel
                .call_service_export(probe_ctx, None, "session", "unarchive", json!({ "session_id": snap2_id }), Default::default())
                .await;
            assert!(unarch_by_id.is_ok(), "by-id session.unarchive failed: {unarch_by_id:?}");
            assert_eq!(
                path_checks.get(),
                0,
                "by-id session.get/archive/unarchive must NOT invoke allows_session_path"
            );

            drop((rcw, rcr));
            tokio::time::timeout(Duration::from_secs(5), remote_task).await.unwrap().unwrap();
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn connection_setup_rekey_does_not_orphan_ctx_or_duplicate_listeners() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, cr, cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();

            // Connect a second owner connection and initialize it.
            let (client2, server2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 =
                tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": "3.0" }
                }),
            )
            .await;
            let init2 = read_reply(&mut cr2, 1).await;
            assert!(init2.get("error").is_none(), "{init2}");

            // Simulate an in-place rekey of s1 -> s1_rekeyed, then re-invoke ensure_session_ctx
            // for the same Host under the new id. It must reuse the existing ctx rather than
            // creating a duplicate ctx with duplicate ui/markdown-changed listeners.
            let new_sid = "s1-rekeyed-setup";
            rt1.host.kernel.set_session_identity(Some(new_sid.to_string()));

            let conns: Vec<Rc<Conn>> = rpc.connections.borrow().values().filter_map(std::rc::Weak::upgrade).collect();
            assert_eq!(conns.len(), 2, "both connections must be registered in rpc.connections");
            for conn in &conns {
                conn.rekey_session(&s1, new_sid, &rt1.host);
            }
            let md_listeners_before = rt1.host.kernel.listener_count("ui/markdown-changed");
            let fs_listeners_before = rt1.host.kernel.listener_count("fs/watch-event");
            assert_eq!(md_listeners_before, 2, "each of the 2 connections should register one markdown listener");
            assert_eq!(fs_listeners_before, 2, "each of the 2 connections should register one fs listener");

            for conn in &conns {
                let ctx_before = conn.session_ctxs.borrow().get(new_sid).map(|(_, cid)| *cid);
                assert!(ctx_before.is_some(), "rekey must migrate session_ctxs to {new_sid}");
                assert!(
                    !conn.session_ctxs.borrow().contains_key(&s1),
                    "rekey must not leave stale {s1} entry in session_ctxs"
                );
                let ctx_after = conn.ensure_session_ctx(new_sid, &rt1.host).await.unwrap();
                assert_eq!(ctx_before, Some(ctx_after), "ensure_session_ctx must reuse the migrated ctx");
            }

            assert_eq!(
                rt1.host.kernel.listener_count("ui/markdown-changed"),
                md_listeners_before,
                "rekey + ensure_session_ctx must not duplicate markdown-changed listeners"
            );
            assert_eq!(
                rt1.host.kernel.listener_count("fs/watch-event"),
                fs_listeners_before,
                "rekey + ensure_session_ctx must not duplicate fs/watch-event listeners"
            );

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Peers must never subscribe to or invoke any session/workspace method on a live session outside
/// the daemon's project scope — even with default `events.subscribe` or granted `peer.allow`.
#[tokio::test]
async fn peer_cannot_access_other_project_sessions_via_subscribe_or_service_calls() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir1, _guard, rpc, mut ocr, mut ocw, owner_task, s_same) = setup_manager_rpc().await;
            let dir2 = tempfile::tempdir().unwrap();

            // Owner opens a second live session in sibling project `dir2`.
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir2.path().to_string_lossy()}}),
            )
            .await;
            let create_rep = read_reply(&mut ocr, 2).await;
            assert!(create_rep.get("error").is_none(), "owner session.create failed: {create_rep}");
            let s_other = create_rep["result"]["session_id"].as_str().unwrap().to_string();

            // Connect peer with default (empty) `peer.allow`.
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            // 1. Default peer (empty `peer.allow`) cannot subscribe to events on another project's session.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":10,"method":"events.subscribe","params":{"session_id":s_other,"include":["status/changed"]}}),
            )
            .await;
            let sub_other = read_reply(&mut pcr, 10).await;
            assert_eq!(
                sub_other["error"]["data"]["code"], "unknown_session",
                "peer events.subscribe on sibling project session must fail closed: {sub_other}"
            );

            // 2. Even when global-style `peer.allow` grants protocol/service methods on both hosts,
            // cross-project session-targeted calls must still fail closed with `unknown_session`.
            let same_rt = rpc.manager.session(&s_same).unwrap();
            let other_rt = rpc.manager.session(&s_other).unwrap();
            allow(&same_rt.host, &["options.get", "agent.list_queue", "agent.list_messages", "agent.prompt"]);
            allow(&other_rt.host, &["options.get", "agent.list_queue", "agent.list_messages", "agent.prompt"]);

            for (id, method, params) in [
                (19, "options.get", json!({"session_id": s_other})),
                (20, "agent.list_queue", json!({"session_id": s_other})),
                (21, "agent.list_messages", json!({"session_id": s_other})),
                (22, "agent.prompt", json!({"session_id": s_other, "text": "cross-project"})),
            ] {
                rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
                let rep = read_reply(&mut pcr, id).await;
                assert_eq!(
                    rep["error"]["data"]["code"], "unknown_session",
                    "peer {method} on sibling project session must fail closed: {rep}"
                );
            }

            // 3. Same-project session calls remain allowed for the peer.
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":30,"method":"agent.list_messages","params":{"session_id":s_same}}),
            )
            .await;
            let same_rep = read_reply(&mut pcr, 30).await;
            assert!(same_rep.get("error").is_none(), "same-project peer agent.list_messages must succeed: {same_rep}");

            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":31,"method":"events.subscribe","params":{"session_id":s_same,"include":["status/changed"]}}),
            )
            .await;
            let sub_same = read_reply(&mut pcr, 31).await;
            assert!(sub_same.get("error").is_none(), "same-project peer events.subscribe must succeed: {sub_same}");

            drop((pcw, pcr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            drop((ocw, ocr));
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn peer_discovery_endpoints_stay_pinned_to_host_project_after_owner_switches_anchor() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, dir_a, _guard, rpc, mut ocr, mut ocw, owner_task, s_a) = setup_manager_rpc().await;
            let dir_b = tempfile::tempdir().unwrap();
            let cwd_a = dunce::canonicalize(dir_a.path()).unwrap();
            let cwd_b = dunce::canonicalize(dir_b.path()).unwrap();

            // Owner opens a session in sibling project B, making B the process default anchor.
            rpc_send(
                &mut ocw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{"cwd":dir_b.path().to_string_lossy()}}),
            )
            .await;
            let create_b = read_reply(&mut ocr, 2).await;
            assert!(create_b.get("error").is_none(), "{create_b}");
            let s_b = create_b["result"]["session_id"].as_str().unwrap().to_string();
            assert_eq!(rpc.anchor_session().unwrap().session_id, s_b);

            let rt_a = rpc.manager.session(&s_a).unwrap();
            let rt_b = rpc.manager.session(&s_b).unwrap();
            allow(&rt_a.host, &["services.list", "agent.list_queue"]);
            allow(&rt_b.host, &["services.list", "agent.prompt"]);

            // 1. Peer connects while both A and B are open, with B as default anchor.
            let (pc1, ps1) = tokio::io::duplex(65536);
            let (psr1, psw1) = tokio::io::split(ps1);
            let peer1_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr1, psw1, None));
            let (pcr1, mut pcw1) = tokio::io::split(pc1);
            let mut pcr1 = BufReader::new(pcr1);

            rpc_send(
                &mut pcw1,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            let init1 = read_reply(&mut pcr1, 1).await;
            assert!(init1.get("error").is_none(), "{init1}");
            assert_eq!(init1["result"]["session"]["session_id"].as_str(), Some(s_a.as_str()));
            let init_methods: Vec<&str> =
                init1["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(init_methods.contains(&"agent.list_queue"), "expected project A grant: {init_methods:?}");
            assert!(!init_methods.contains(&"agent.prompt"), "must not leak project B grant: {init_methods:?}");

            rpc_send(&mut pcw1, json!({"jsonrpc":"2.0","id":2,"method":"peer.info","params":{}})).await;
            let info1 = read_reply(&mut pcr1, 2).await;
            assert!(info1.get("error").is_none(), "{info1}");
            let info_cwd = dunce::canonicalize(info1["result"]["cwd"].as_str().unwrap()).unwrap();
            assert_eq!(info_cwd, cwd_a, "peer.info.cwd must stay in host project A, not {cwd_b:?}");
            let allowed1: Vec<&str> =
                info1["result"]["allowed"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(allowed1.contains(&"agent.list_queue"), "expected project A grant in peer.info: {allowed1:?}");
            assert!(!allowed1.contains(&"agent.prompt"), "must not leak project B grant in peer.info: {allowed1:?}");

            rpc_send(&mut pcw1, json!({"jsonrpc":"2.0","id":3,"method":"services.list","params":{}})).await;
            let svcs1 = read_reply(&mut pcr1, 3).await;
            assert!(svcs1.get("error").is_none(), "{svcs1}");
            let agent_methods1: Vec<&str> = svcs1["result"]["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "agent")
                .and_then(|s| s["methods"].as_object())
                .map(|m| m.keys().map(String::as_str).collect())
                .unwrap_or_default();
            assert!(agent_methods1.contains(&"list_queue"), "services.list must reflect project A: {agent_methods1:?}");
            assert!(!agent_methods1.contains(&"prompt"), "services.list must not leak project B: {agent_methods1:?}");

            drop((pcw1, pcr1));
            tokio::time::timeout(Duration::from_secs(5), peer1_task).await.unwrap().unwrap();

            // 2. Owner closes project A's session leaving only project B open; peer discovery
            //    must still use a project-A fallback host and never leak project B's cwd or grants.
            rpc_send(&mut ocw, json!({"jsonrpc":"2.0","id":3,"method":"session.close","params":{"session_id":s_a}}))
                .await;
            assert!(read_reply(&mut ocr, 3).await.get("error").is_none());

            let (pc2, ps2) = tokio::io::duplex(65536);
            let (psr2, psw2) = tokio::io::split(ps2);
            let peer2_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr2, psw2, None));
            let (pcr2, mut pcw2) = tokio::io::split(pc2);
            let mut pcr2 = BufReader::new(pcr2);

            rpc_send(
                &mut pcw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            let init2 = read_reply(&mut pcr2, 1).await;
            assert!(init2.get("error").is_none(), "{init2}");
            assert!(init2["result"]["session"].is_null(), "must not expose project B session: {init2}");
            let init_methods2: Vec<&str> =
                init2["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(!init_methods2.contains(&"agent.prompt"), "must not leak project B grant: {init_methods2:?}");

            rpc_send(&mut pcw2, json!({"jsonrpc":"2.0","id":2,"method":"peer.info","params":{}})).await;
            let info2 = read_reply(&mut pcr2, 2).await;
            assert!(info2.get("error").is_none(), "{info2}");
            let info_cwd2 = dunce::canonicalize(info2["result"]["cwd"].as_str().unwrap()).unwrap();
            assert_eq!(info_cwd2, cwd_a, "fallback peer.info.cwd must stay in project A, not {cwd_b:?}");

            // Project A's fallback host does not inherit project B's `services.list` / `agent.prompt` grants.
            rpc_send(&mut pcw2, json!({"jsonrpc":"2.0","id":3,"method":"services.list","params":{}})).await;
            let svcs2 = read_reply(&mut pcr2, 3).await;
            assert!(
                svcs2.get("error").is_some(),
                "fallback services.list must not inherit project B's peer.allow grant: {svcs2}"
            );

            drop((pcw2, pcr2));
            tokio::time::timeout(Duration::from_secs(5), peer2_task).await.unwrap().unwrap();
            drop((ocw, ocr));
            tokio::time::timeout(Duration::from_secs(5), owner_task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_service_callback_uses_live_session_id_after_in_place_rekey() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();

            // Register a session-scoped remote service from the owner connection.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "services.register",
                    "params": {
                        "scope": "session",
                        "session_id": s1,
                        "name": "rekey_remote_cb",
                        "methods": {
                            "ping": {
                                "scope": "session",
                                "parameters": { "type": "object", "properties": {}, "additionalProperties": false },
                                "result": { "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"] }
                            }
                        }
                    }
                }),
            )
            .await;
            let reg = read_reply(&mut cr, 2).await;
            assert!(reg.get("error").is_none(), "services.register failed: {reg}");

            // Rekey the host session in place without re-registering the remote service.
            let new_sid = "s1-rekeyed-remote-cb";
            rt1.host.kernel.set_session_identity(Some(new_sid.to_string()));

            // Invoke the service directly from Lua (without `CALL_SESSION` task-local).
            let host_for_call = rt1.host.clone();
            let call_task = tokio::task::spawn_local(async move {
                super::call_service(&host_for_call, "rekey_remote_cb", "ping", vec![]).await
            });

            // Read the incoming `callback.call` server request and assert it carries `new_sid`.
            let cb_req = loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(5), cr.read_line(&mut line)).await.unwrap().unwrap();
                let msg: J = serde_json::from_str(line.trim()).unwrap();
                if msg.get("method").and_then(J::as_str) == Some("callback.call") {
                    break msg;
                }
            };
            assert_eq!(
                cb_req["params"]["session_id"].as_str(),
                Some(new_sid),
                "Lua-initiated remote service callback must report live post-rekey session_id: {cb_req}"
            );
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": cb_req["id"], "result": { "ok": true } })).await;

            let call_res = tokio::time::timeout(Duration::from_secs(5), call_task).await.unwrap().unwrap().unwrap();
            assert_eq!(call_res["ok"], json!(true), "{call_res}");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Closing the only open session in a multi-session server shuts down its Host (which is also
/// referenced by `fallback_host`). Subsequent contract calls from both the owner (`session.list`,
/// `session.create`) and a newly connected peer (`initialize` + `session.list`) must not fail
/// with `unknown method` on the dead fallback Host.
#[tokio::test]
async fn closing_only_session_recovers_live_anchor_for_owner_and_peer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;

            // Owner closes the only open session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.close","params":{"session_id":s1}}))
                .await;
            let closed = read_reply(&mut cr, 2).await;
            assert!(closed.get("error").is_none(), "session.close failed: {closed}");
            assert!(rpc.manager.active_ids().is_empty(), "no active sessions should remain");

            // Owner calls `session.list` right after closing the only session.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"session.list","params":{}})).await;
            let owner_list = read_reply(&mut cr, 3).await;
            assert!(
                owner_list.get("error").is_none(),
                "owner session.list after closing only session must succeed via live fallback anchor: {owner_list}"
            );

            // Owner creates a second session, then closes it so `fallback_host` again holds a shut-down Host.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"session.create","params":{"name":"s2"}})).await;
            let created = read_reply(&mut cr, 4).await;
            assert!(
                created.get("error").is_none(),
                "owner session.create after closing only session must succeed: {created}"
            );
            let s2 = created["result"]["session_id"].as_str().unwrap().to_owned();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.close","params":{"session_id":s2}}))
                .await;
            let closed2 = read_reply(&mut cr, 5).await;
            assert!(closed2.get("error").is_none(), "second session.close failed: {closed2}");

            // Allow peers to call `session.list` on newly booted project fallback anchors.
            rpc.base_opts.borrow_mut().config["peer"] = json!({ "allow": ["session.list"] });

            // Fresh peer connects while only the shut-down session Host sits in `fallback_host`.
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(super::serve::serve_peer_manager(rpc.clone(), psr, psw, None));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{}}}),
            )
            .await;
            let pinit = read_reply(&mut pcr, 1).await;
            assert!(pinit.get("error").is_none(), "peer initialize after closing only session failed: {pinit}");

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":2,"method":"session.list","params":{}})).await;
            let peer_list = read_reply(&mut pcr, 2).await;
            assert!(
                peer_list.get("error").is_none(),
                "peer session.list after closing only session must succeed via live project anchor: {peer_list}"
            );

            drop((pcw, pcr));
            tokio::time::timeout(Duration::from_secs(5), peer_task).await.unwrap().unwrap();
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// If a host's session epoch changes while a session-scoped remote service callback is awaiting
/// `host.state()`, the callback must fail with `Unavailable` rather than re-reading the new
/// session's identity and forwarding arguments constructed for the old session.
#[tokio::test]
async fn session_scoped_callback_rejects_epoch_change_during_state_read() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_home, _hg, _dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_manager_rpc().await;
            let rt1 = rpc.manager.session(&s1).unwrap();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "services.register",
                    "params": {
                        "scope": "session",
                        "session_id": s1,
                        "name": "stale_epoch_cb",
                        "methods": {
                            "ping": {
                                "scope": "session",
                                "parameters": { "type": "object", "properties": {}, "additionalProperties": false },
                                "result": { "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"] }
                            }
                        }
                    }
                }),
            )
            .await;
            let reg = read_reply(&mut cr, 2).await;
            assert!(reg.get("error").is_none(), "services.register failed: {reg}");

            // Hook `agent.get` (called by `host.state()`) so the host rotates `session_epoch` mid-await.
            let host_for_hook = rt1.host.clone();
            let rotate_fn = rt1
                .host
                .lua()
                .create_function(move |_, ()| {
                    host_for_hook.kernel.set_session_identity(Some("s1-rotated-mid-state".to_string()));
                    Ok(())
                })
                .unwrap();
            rt1.host.lua().globals().set("_TEST_ROTATE_EPOCH", rotate_fn).unwrap();
            let agent_tbl = rt1.host.service_table("agent").unwrap();
            let orig_get: mlua::Function = agent_tbl.get("get").unwrap();
            rt1.host.lua().globals().set("_TEST_ORIG_AGENT_GET", orig_get).unwrap();
            let wrapped: mlua::Function = rt1
                .host
                .lua()
                .load("return function(self, ...) _TEST_ROTATE_EPOCH(); return _TEST_ORIG_AGENT_GET(self, ...) end")
                .eval()
                .unwrap();
            agent_tbl.set("get", wrapped).unwrap();

            let err = super::call_service(&rt1.host, "stale_epoch_cb", "ping", vec![])
                .await
                .expect_err("epoch change during host.state() must fail callback as stale/unavailable");
            let msg = format!("{err:?}");
            assert!(
                msg.contains("Unavailable") || msg.contains("unavailable"),
                "expected Unavailable error when session epoch changes mid-state read, got: {msg}"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

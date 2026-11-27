#![cfg(unix)]

use std::process::Stdio;

use serde_json::{Value as J, json};

use crate::common::*;

/// The terminal service as the probe plugin `owner` reads it (`ctx:get`, traced): a mare.Service
/// method's caller is that plugin, which owns the terminals it opens.
async fn terminal_of(host: &mare_host::Host, owner: &str) -> mlua::Table {
    eval::<mlua::Table>(host, &format!("return mare._pctx[{owner:?}]:get('terminal')")).await
}

async fn call_proxy(proxy: &mlua::Table, method: &str, arg: Option<mlua::Table>) -> mlua::Result<mlua::Table> {
    let f: mlua::Function = proxy.get(method)?;
    match arg {
        Some(a) => f.call_async::<mlua::Table>((proxy.clone(), a)).await,
        None => f.call_async::<mlua::Table>(proxy.clone()).await,
    }
}

#[test]
fn lua_terminal_service_owner_isolation_and_dispose_cleanup() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a", "b"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy_a = terminal_of(&host, "a").await;
        let proxy_b = terminal_of(&host, "b").await;

        let opts = lua.create_table().unwrap();
        opts.set("name", "owner-a").unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("args", vec!["-c", "printf 'READY\\n'; exec tail -f /dev/null"]).unwrap();
        let opened = call_proxy(&proxy_a, "create", Some(opts)).await.unwrap();
        let sid: String = opened.get("terminal_id").unwrap();
        let pid: u32 = opened.get("pid").unwrap();
        assert_eq!(opened.get::<String>("status").unwrap(), "running");
        assert!(pid_alive(pid));

        assert_eq!(
            call_proxy(&proxy_a, "list", None).await.unwrap().get::<mlua::Table>("data").unwrap().len().unwrap(),
            1
        );
        assert_eq!(
            call_proxy(&proxy_b, "list", None).await.unwrap().get::<mlua::Table>("data").unwrap().len().unwrap(),
            0
        );

        for (method, extra) in [
            ("read", vec![]),
            ("send", vec![("text", "hi")]),
            ("signal", vec![("signal", "SIGTERM")]),
            ("close", vec![]),
        ] {
            let arg = lua.create_table().unwrap();
            arg.set("terminal_id", sid.as_str()).unwrap();
            for (k, v) in extra {
                arg.set(k, v).unwrap();
            }
            let err = call_proxy(&proxy_b, method, Some(arg)).await.unwrap_err().to_string();
            assert!(err.contains("unknown terminal"), "{method}: {err}");
        }
        let resize_arg = lua.create_table().unwrap();
        resize_arg.set("terminal_id", sid.as_str()).unwrap();
        resize_arg.set("cols", 90).unwrap();
        resize_arg.set("rows", 30).unwrap();
        assert!(
            call_proxy(&proxy_b, "resize", Some(resize_arg))
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown terminal")
        );

        // the owner plugin goes: its terminals with it
        unload_plugin(&host.kernel, "ctx-a").await;
        assert!(!pid_alive(pid), "disposing owner ctx must stop all owned terminal processes");
    }));
}

#[test]
fn lua_terminal_read_offset_truncation_and_exit_status() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;

        let opts = lua.create_table().unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("args", vec!["-c", "printf 'line1\\nline2\\nline3\\nline4\\nline5\\n'; exit 7"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let sid: String = opened.get("terminal_id").unwrap();

        let mut last = opened;
        for _ in 0..32 {
            if last.get::<String>("status").unwrap() != "running" {
                break;
            }
            let arg = lua.create_table().unwrap();
            arg.set("terminal_id", sid.as_str()).unwrap();
            arg.set("wait_ms", 5000).unwrap();
            last = call_proxy(&proxy, "read", Some(arg)).await.unwrap();
        }
        assert_eq!(last.get::<String>("status").unwrap(), "failed");
        assert_eq!(last.get::<Option<u32>>("exit_code").unwrap(), Some(7));
        assert_eq!(last.get::<Option<i32>>("exit_signal").unwrap(), None);

        // Line-offset pagination via VT page
        let page_arg = lua.create_table().unwrap();
        page_arg.set("terminal_id", sid.as_str()).unwrap();
        page_arg.set("offset", 1).unwrap();
        page_arg.set("count", 2).unwrap();
        let paged = call_proxy(&proxy, "read", Some(page_arg)).await.unwrap();
        assert_eq!(paged.get::<usize>("line_begin").unwrap(), 1);
        assert!(paged.get::<usize>("total_lines").unwrap() >= 5);

        // Byte cap truncation
        let cap_arg = lua.create_table().unwrap();
        cap_arg.set("terminal_id", sid.as_str()).unwrap();
        cap_arg.set("output_offset", 0).unwrap();
        cap_arg.set("max_bytes", 6).unwrap();
        let capped = call_proxy(&proxy, "read", Some(cap_arg)).await.unwrap();
        assert!(capped.get::<bool>("truncated").unwrap());
        let head = capped.get::<String>("text").unwrap();
        assert_eq!(head.len(), 6, "{head:?}");
        let next_output_offset = capped.get::<u64>("next_output_offset").unwrap();
        assert_eq!(next_output_offset, 6);

        let rest_arg = lua.create_table().unwrap();
        rest_arg.set("terminal_id", sid.as_str()).unwrap();
        rest_arg.set("output_offset", next_output_offset).unwrap();
        let rest = call_proxy(&proxy, "read", Some(rest_arg)).await.unwrap();
        assert!(rest.get::<String>("text").unwrap().contains("line5"));

        // Mutating operations on a finished terminal fail with terminal.not_running.
        let resize_arg = lua.create_table().unwrap();
        resize_arg.set("terminal_id", sid.as_str()).unwrap();
        resize_arg.set("cols", 90).unwrap();
        resize_arg.set("rows", 25).unwrap();
        let resize_err = call_proxy(&proxy, "resize", Some(resize_arg)).await.unwrap_err().to_string();
        assert!(resize_err.contains("not_running"), "{resize_err}");

        let close_arg = lua.create_table().unwrap();
        close_arg.set("terminal_id", sid.as_str()).unwrap();
        let closed = call_proxy(&proxy, "close", Some(close_arg)).await.unwrap();
        assert_eq!(closed.get::<String>("status").unwrap(), "failed");
        let close_again_arg = lua.create_table().unwrap();
        close_again_arg.set("terminal_id", sid.as_str()).unwrap();
        let closed_again = call_proxy(&proxy, "close", Some(close_again_arg)).await.unwrap();
        assert_eq!(closed_again.get::<String>("terminal_id").unwrap(), sid);
    }));
}

#[test]
fn lua_terminal_oversized_chunk_preserves_tail_without_dropping_whole_chunk() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;

        let opts = lua.create_table().unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("buffer_bytes", 1024).unwrap();
        opts.set("args", vec!["-c", "python3 -c 'import sys; sys.stdout.write(\"A\" * 2000 + \"TAIL_MARKER\\n\")'"])
            .unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let sid: String = opened.get("terminal_id").unwrap();

        let mut last = opened;
        for _ in 0..32 {
            if last.get::<String>("status").unwrap() != "running" {
                break;
            }
            let arg = lua.create_table().unwrap();
            arg.set("terminal_id", sid.as_str()).unwrap();
            arg.set("wait_ms", 5000).unwrap();
            last = call_proxy(&proxy, "read", Some(arg)).await.unwrap();
        }
        assert_eq!(last.get::<String>("status").unwrap(), "completed");

        let read_arg = lua.create_table().unwrap();
        read_arg.set("terminal_id", sid.as_str()).unwrap();
        read_arg.set("output_offset", 0).unwrap();
        let res = call_proxy(&proxy, "read", Some(read_arg)).await.unwrap();
        assert!(res.get::<bool>("truncated").unwrap());
        let text = res.get::<String>("text").unwrap();
        assert!(!text.is_empty(), "oversized chunk must not be dropped whole");
        assert!(text.len() <= 1024, "retained buffer must respect log_limit: {}", text.len());
        assert!(text.contains("TAIL_MARKER"), "expected tail in {text:?}");
    }));
}

#[test]
fn lua_terminal_multibyte_utf8_pagination_with_max_bytes_one_advances_cursor() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;

        let opts = lua.create_table().unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("emulate", false).unwrap();
        opts.set("args", vec!["-c", "printf '你好world\\n'"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let sid: String = opened.get("terminal_id").unwrap();

        let mut last = opened;
        for _ in 0..32 {
            if last.get::<String>("status").unwrap() != "running" {
                break;
            }
            let arg = lua.create_table().unwrap();
            arg.set("terminal_id", sid.as_str()).unwrap();
            arg.set("wait_ms", 5000).unwrap();
            last = call_proxy(&proxy, "read", Some(arg)).await.unwrap();
        }
        assert_eq!(last.get::<String>("status").unwrap(), "completed");
        let end_cursor = last.get::<u64>("output_offset").unwrap();
        assert!(end_cursor >= "你好world".len() as u64);

        let mut cursor = 0u64;
        let mut collected = String::new();
        for _ in 0..64 {
            if cursor >= end_cursor {
                break;
            }
            let arg = lua.create_table().unwrap();
            arg.set("terminal_id", sid.as_str()).unwrap();
            arg.set("output_offset", cursor).unwrap();
            arg.set("max_bytes", 1).unwrap();
            let page = call_proxy(&proxy, "read", Some(arg)).await.unwrap();
            let next_cursor = page.get::<u64>("next_output_offset").unwrap();
            assert!(next_cursor > cursor, "cursor stalled at {cursor}: {page:?}");
            collected.push_str(&page.get::<String>("text").unwrap());
            cursor = next_cursor;
        }
        assert_eq!(cursor, end_cursor);
        assert!(collected.contains("你好world"), "{collected:?}");
    }));
}

#[test]
fn rpc_terminal_lifecycle_offset_truncation_signal_resize_and_disconnect_cleanup() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--clean"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| J::Null);
    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let methods = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for required in [
        "terminal.create",
        "terminal.send",
        "terminal.read",
        "terminal.signal",
        "terminal.resize",
        "terminal.close",
        "terminal.list",
    ] {
        assert!(methods.iter().any(|m| m.as_str() == Some(required)), "missing {required}");
    }

    let open = rpc.call_raw(
        "terminal.create",
        json!({
            "session_id": session_id,
            "name": "rpc-shell",
            "cmd": "sh",
            "args": ["-c", "printf 'RPC_READY\\n'; while IFS= read -r line; do printf 'ECHO:%s\\n' \"$line\"; [ \"$line\" = quit ] && exit 42; done"],
            "cols": 80,
            "rows": 24
        }),
        |_| J::Null,
    );
    let sid = open["result"]["terminal_id"].as_str().unwrap_or_else(|| panic!("{open}")).to_string();
    assert_eq!(open["result"]["status"], "running");

    let list = rpc.call_raw("terminal.list", json!({ "session_id": session_id }), |_| J::Null);
    assert_eq!(list["result"]["data"].as_array().unwrap().len(), 1);

    let resized = rpc.call_raw(
        "terminal.resize",
        json!({ "session_id": session_id, "terminal_id": sid, "cols": 100, "rows": 30 }),
        |_| J::Null,
    );
    assert_eq!(resized["result"], json!({}));

    let mut sent = rpc.call_raw(
        "terminal.send",
        json!({ "session_id": session_id, "terminal_id": sid, "text": "hello", "submit": true, "wait_ms": 5000 }),
        |_| J::Null,
    );
    let mut seen = sent["result"]["output"].as_str().unwrap_or("").to_string();
    let mut cursor = sent["result"]["next_output_offset"].as_u64().unwrap();
    while !seen.contains("ECHO:hello") {
        let r = rpc.call_raw(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": sid, "output_offset": cursor, "wait_ms": 5000 }),
            |_| J::Null,
        );
        seen.push_str(r["result"]["text"].as_str().unwrap_or(""));
        cursor = r["result"]["next_output_offset"].as_u64().unwrap();
    }

    let capped = rpc.call_raw(
        "terminal.read",
        json!({ "session_id": session_id, "terminal_id": sid, "offset": 0, "count": 5, "max_bytes": 8, "screen": true }),
        |_| J::Null,
    );
    assert_eq!(capped["result"]["truncated"], true, "{capped}");
    assert!(capped["result"]["text"].as_str().unwrap().len() <= 8);
    assert!(capped["result"]["screen"].is_string());

    sent = rpc.call_raw(
        "terminal.send",
        json!({ "session_id": session_id, "terminal_id": sid, "text": "quit", "submit": true, "wait_ms": 5000 }),
        |_| J::Null,
    );
    while sent["result"]["status"] == "running" {
        sent = rpc.call_raw(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": sid, "wait_ms": 5000 }),
            |_| J::Null,
        );
    }
    assert_eq!(sent["result"]["status"], "failed", "{sent}");
    assert_eq!(sent["result"]["exit_code"], 42, "{sent}");

    let closed = rpc.call_raw("terminal.close", json!({ "session_id": session_id, "terminal_id": sid }), |_| J::Null);
    assert_eq!(closed["result"]["status"], "failed", "{closed}");

    // Signal foreground command & verify cleanup on disconnect/shutdown
    let open2 = rpc.call_raw(
        "terminal.create",
        json!({ "session_id": session_id, "cmd": "sh", "args": ["-c", "printf 'SIG_READY\\n'; exec tail -f /dev/null"] }),
        |_| J::Null,
    );
    let sid2 = open2["result"]["terminal_id"].as_str().unwrap().to_string();
    let pid2 = open2["result"]["pid"].as_u64().unwrap() as u32;
    let mut ready = String::new();
    let mut c2 = 0u64;
    while !ready.contains("SIG_READY") {
        let r = rpc.call_raw(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": sid2, "output_offset": c2, "wait_ms": 5000 }),
            |_| J::Null,
        );
        ready.push_str(r["result"]["text"].as_str().unwrap_or(""));
        c2 = r["result"]["next_output_offset"].as_u64().unwrap();
    }
    let sig = rpc.call_raw(
        "terminal.signal",
        json!({ "session_id": session_id, "terminal_id": sid2, "signal": "SIGTERM" }),
        |_| J::Null,
    );
    assert_eq!(sig["result"], json!({}), "{sig}");

    // Leave open3 running and verify shutdown disposes connection ctx and kills it
    let open3 = rpc.call_raw(
        "terminal.create",
        json!({ "session_id": session_id, "cmd": "tail", "args": ["-f", "/dev/null"] }),
        |_| J::Null,
    );
    let pid3 = open3["result"]["pid"].as_u64().unwrap() as u32;
    assert!(pid_alive(pid3));

    let _ = rpc.call_raw("shutdown", json!({}), |_| J::Null);
    rpc.wait_exit();
    assert!(!pid_alive(pid2));
    assert!(!pid_alive(pid3), "RPC shutdown/disconnect must stop owner-bound terminal processes");
}

#[test]
fn rpc_terminal_disconnect_closes_pty_without_orphan_process() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--clean"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| J::Null);
    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let open = rpc.call_raw(
        "terminal.create",
        json!({ "session_id": session_id, "cmd": "tail", "args": ["-f", "/dev/null"] }),
        |_| J::Null,
    );
    let tid =
        open["result"]["terminal_id"].as_str().unwrap_or_else(|| panic!("expected terminal_id: {open}")).to_string();
    assert!(tid.starts_with("term-"), "{open}");
    let pid = open["result"]["pid"].as_u64().unwrap() as u32;
    assert!(pid_alive(pid), "opened PTY process must be alive");

    // Disconnect immediately via stdin EOF without calling terminal.close or shutdown.
    rpc.wait_exit();
    assert!(!pid_alive(pid), "PTY process must be closed when connection disconnects without leaving an orphan");
}

#[test]
fn rpc_terminal_peer_cannot_operate_other_connections_terminal() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { peer = { allow = { 'terminal.list', 'terminal.read', 'terminal.create', 'terminal.send', 'terminal.signal', 'terminal.resize', 'terminal.close' } } }\n",
    )
    .unwrap();

    let env = std::collections::BTreeMap::from([("MARE_HOME".to_string(), home.path().display().to_string())]);
    let mut primary = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let init = primary.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let session_id = init["session"]["session_id"].as_str().unwrap().to_string();

    let open = primary.call(
        "terminal.create",
        json!({ "session_id": session_id, "cmd": "sh", "args": ["-c", "printf 'PRIMARY_READY\\n'; exec tail -f /dev/null"] }),
    );
    let tid = open["terminal_id"].as_str().unwrap_or_else(|| panic!("expected terminal_id: {open}")).to_string();

    let mut ready = String::new();
    let mut cursor = 0u64;
    while !ready.contains("PRIMARY_READY") {
        let r = primary.call(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": tid, "output_offset": cursor, "wait_ms": 5000 }),
        );
        ready.push_str(r["text"].as_str().unwrap_or(""));
        cursor = r["next_output_offset"].as_u64().unwrap();
    }

    let mut peer = crate::common::rpc::PeerClient::connect(home.path(), primary.pid());
    let _ = peer.call("initialize", json!({"protocol_version": "3.0"}));

    let peer_list = peer.call("terminal.list", json!({ "session_id": session_id }));
    assert_eq!(peer_list["result"]["data"], json!([]), "peer must not list primary's terminals: {peer_list}");

    let peer_read = peer.call("terminal.read", json!({ "session_id": session_id, "terminal_id": tid }));
    assert_eq!(
        peer_read["error"]["data"]["code"], "terminal.unknown_terminal",
        "peer must not read another connection's terminal: {peer_read}"
    );

    for (method, params) in [
        ("terminal.create", json!({ "session_id": session_id, "cmd": "true" })),
        ("terminal.send", json!({ "session_id": session_id, "terminal_id": tid, "text": "x" })),
        ("terminal.signal", json!({ "session_id": session_id, "terminal_id": tid, "signal": "SIGTERM" })),
        ("terminal.resize", json!({ "session_id": session_id, "terminal_id": tid, "cols": 80, "rows": 24 })),
        ("terminal.close", json!({ "session_id": session_id, "terminal_id": tid })),
    ] {
        let res = peer.call(method, params);
        assert_eq!(res["error"]["data"]["code"], "denied", "peer must be denied {method}: {res}");
    }

    drop(peer);
    let closed = primary.call("terminal.close", json!({ "session_id": session_id, "terminal_id": tid }));
    assert_eq!(closed["status"], "cancelled", "{closed}");
}

#[test]
fn rpc_terminal_truncates_output_by_max_bytes() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--clean"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| J::Null);
    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let open = rpc.call_raw(
        "terminal.create",
        json!({
            "session_id": session_id,
            "cmd": "sh",
            "emulate": false,
            "args": ["-c", "printf '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ\\n'"]
        }),
        |_| J::Null,
    );
    let tid =
        open["result"]["terminal_id"].as_str().unwrap_or_else(|| panic!("expected terminal_id: {open}")).to_string();

    let mut cur = open;
    while cur["result"]["status"] == "running" {
        cur = rpc.call_raw(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": tid, "wait_ms": 5000 }),
            |_| J::Null,
        );
    }
    assert_eq!(cur["result"]["status"], "completed", "{cur}");

    let first = rpc.call_raw(
        "terminal.read",
        json!({ "session_id": session_id, "terminal_id": tid, "output_offset": 0, "max_bytes": 10 }),
        |_| J::Null,
    );
    assert_eq!(first["result"]["truncated"], true, "{first}");
    let head = first["result"]["text"].as_str().unwrap();
    assert_eq!(head.len(), 10, "{first}");
    let next_cursor = first["result"]["next_output_offset"].as_u64().unwrap();
    assert_eq!(next_cursor, 10, "{first}");

    let rest = rpc.call_raw(
        "terminal.read",
        json!({ "session_id": session_id, "terminal_id": tid, "output_offset": next_cursor }),
        |_| J::Null,
    );
    let combined = format!("{head}{}", rest["result"]["text"].as_str().unwrap());
    assert!(combined.contains("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"), "{combined:?}");

    // Also verify terminal.send caps returned output by max_bytes.
    let open2 = rpc.call_raw(
        "terminal.create",
        json!({
            "session_id": session_id,
            "cmd": "sh",
            "emulate": false,
            "args": ["-c", "while IFS= read -r l; do printf '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ\\n'; done"]
        }),
        |_| J::Null,
    );
    let tid2 = open2["result"]["terminal_id"].as_str().unwrap().to_string();
    let sent = rpc.call_raw(
        "terminal.send",
        json!({ "session_id": session_id, "terminal_id": tid2, "text": "go", "submit": true, "wait_ms": 5000, "max_bytes": 8 }),
        |_| J::Null,
    );
    assert_eq!(sent["result"]["truncated"], true, "{sent}");
    let out = sent["result"]["output"].as_str().unwrap();
    assert!(out.len() <= 8 && !out.is_empty(), "{sent}");

    let _ = rpc.call_raw("terminal.close", json!({ "session_id": session_id, "terminal_id": tid2 }), |_| J::Null);
    let _ = rpc.call_raw("shutdown", json!({}), |_| J::Null);
    rpc.wait_exit();
}

#[test]
fn rpc_terminal_defaults_max_bytes_to_262144_without_param() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--clean"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| J::Null);
    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    // Emit 300,000 ASCII bytes (> 262144 default max_bytes, < 4 MiB default buffer_bytes).
    let open = rpc.call_raw(
        "terminal.create",
        json!({
            "session_id": session_id,
            "cmd": "sh",
            "emulate": false,
            "args": ["-c", "dd if=/dev/zero bs=1000 count=300 2>/dev/null | tr '\\0' 'A'"]
        }),
        |_| J::Null,
    );
    let tid = open["result"]["terminal_id"].as_str().unwrap().to_string();

    let mut cur = open;
    while cur["result"]["status"] == "running" {
        cur = rpc.call_raw(
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": tid, "output_offset": 1_000_000, "wait_ms": 5000 }),
            |_| J::Null,
        );
    }
    assert_eq!(cur["result"]["status"], "completed", "{cur}");

    // Without max_bytes, terminal.read must truncate at 262144 bytes and advance next_output_offset.
    let first = rpc.call_raw(
        "terminal.read",
        json!({ "session_id": session_id, "terminal_id": tid, "output_offset": 0 }),
        |_| J::Null,
    );
    assert_eq!(first["result"]["truncated"], true, "{first}");
    let head = first["result"]["text"].as_str().unwrap();
    assert_eq!(head.len(), 262_144, "default read max_bytes must be 262144");
    let next_cursor = first["result"]["next_output_offset"].as_u64().unwrap();
    assert_eq!(next_cursor, 262_144, "{first}");

    let rest = rpc.call_raw(
        "terminal.read",
        json!({ "session_id": session_id, "terminal_id": tid, "output_offset": next_cursor }),
        |_| J::Null,
    );
    let tail = rest["result"]["text"].as_str().unwrap();
    assert_eq!(head.len() + tail.len(), 300_000, "remaining bytes must be readable from next_output_offset");

    // Verify terminal.send without max_bytes also caps output at 262144.
    let open2 = rpc.call_raw(
        "terminal.create",
        json!({
            "session_id": session_id,
            "cmd": "sh",
            "emulate": false,
            "args": [
                "-c",
                "while IFS= read -r _; do dd if=/dev/zero bs=1000 count=300 2>/dev/null | tr '\\0' 'B'; printf '\\n'; done"
            ]
        }),
        |_| J::Null,
    );
    let tid2 = open2["result"]["terminal_id"].as_str().unwrap().to_string();
    let sent = rpc.call_raw(
        "terminal.send",
        json!({ "session_id": session_id, "terminal_id": tid2, "text": "go", "submit": true, "wait_ms": 5000 }),
        |_| J::Null,
    );
    assert_eq!(sent["result"]["truncated"], true, "{sent}");
    let out = sent["result"]["output"].as_str().unwrap();
    assert_eq!(out.len(), 262_144, "default send max_bytes must be 262144");

    let _ = rpc.call_raw("terminal.close", json!({ "session_id": session_id, "terminal_id": tid2 }), |_| J::Null);
    let _ = rpc.call_raw("shutdown", json!({}), |_| J::Null);
    rpc.wait_exit();
}

#[test]
fn kernel_reload_terminal_closes_running_pty() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;
        let opts = lua.create_table().unwrap();
        opts.set("cmd", "tail").unwrap();
        opts.set("args", vec!["-f", "/dev/null"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let pid: u32 = opened.get("pid").unwrap();
        assert!(pid_alive(pid), "opened PTY process must be running before reload");

        host.kernel.reload(Some("terminal")).await.unwrap();
        assert!(!pid_alive(pid), "kernel.reload('terminal') must close running PTYs");
    }));
}

#[test]
fn rpc_terminal_default_peer_without_allow_is_denied() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let env = std::collections::BTreeMap::from([("MARE_HOME".to_string(), home.path().display().to_string())]);
    let mut primary = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let init = primary.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let session_id = init["session"]["session_id"].as_str().unwrap().to_string();
    let open = primary
        .call("terminal.create", json!({ "session_id": session_id, "cmd": "tail", "args": ["-f", "/dev/null"] }));
    let tid = open["terminal_id"].as_str().unwrap().to_string();

    let mut peer = crate::common::rpc::PeerClient::connect(home.path(), primary.pid());
    let _ = peer.call("initialize", json!({"protocol_version": "3.0"}));

    for (method, params) in [
        ("terminal.list", json!({ "session_id": session_id })),
        ("terminal.read", json!({ "session_id": session_id, "terminal_id": tid })),
        ("terminal.create", json!({ "session_id": session_id, "cmd": "true" })),
        ("terminal.send", json!({ "session_id": session_id, "terminal_id": tid, "text": "x" })),
        ("terminal.signal", json!({ "session_id": session_id, "terminal_id": tid, "signal": "SIGTERM" })),
        ("terminal.resize", json!({ "session_id": session_id, "terminal_id": tid, "cols": 80, "rows": 24 })),
        ("terminal.close", json!({ "session_id": session_id, "terminal_id": tid })),
    ] {
        let res = peer.call(method, params);
        assert_eq!(res["error"]["data"]["code"], "denied", "unauthorized peer must be denied {method}: {res}");
    }

    drop(peer);
    let closed = primary.call("terminal.close", json!({ "session_id": session_id, "terminal_id": tid }));
    assert_eq!(closed["status"], "cancelled", "{closed}");
}

#[test]
fn rpc_terminal_rejects_negative_and_out_of_range_params_with_invalid_arguments() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--clean"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| J::Null);
    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    // cols=0 / rows=0 with emulate=true must fail contract validation with invalid_arguments, not panic.
    for (label, params) in [
        ("create cols=0", json!({ "session_id": session_id, "cmd": "true", "emulate": true, "cols": 0, "rows": 24 })),
        ("create rows=0", json!({ "session_id": session_id, "cmd": "true", "emulate": true, "cols": 80, "rows": 0 })),
        ("create cols=-1", json!({ "session_id": session_id, "cmd": "true", "cols": -1 })),
        ("create rows=201", json!({ "session_id": session_id, "cmd": "true", "rows": 201 })),
        ("create buffer_bytes=-1", json!({ "session_id": session_id, "cmd": "true", "buffer_bytes": -1 })),
        ("create scrollback_lines=-1", json!({ "session_id": session_id, "cmd": "true", "scrollback_lines": -1 })),
        ("create empty name", json!({ "session_id": session_id, "cmd": "true", "name": "" })),
    ] {
        let res = rpc.call_raw("terminal.create", params, |_| J::Null);
        assert_eq!(res["error"]["data"]["code"], "invalid_arguments", "{label} must return invalid_arguments: {res}");
    }

    let opened = rpc.call_raw(
        "terminal.create",
        json!({ "session_id": session_id, "cmd": "tail", "args": ["-f", "/dev/null"] }),
        |_| J::Null,
    );
    let tid = opened["result"]["terminal_id"].as_str().unwrap().to_string();

    for (label, method, params) in [
        ("send wait_ms=-1", "terminal.send", json!({ "session_id": session_id, "terminal_id": tid, "wait_ms": -1 })),
        (
            "send max_bytes=-1",
            "terminal.send",
            json!({ "session_id": session_id, "terminal_id": tid, "max_bytes": -1 }),
        ),
        ("read offset=-1", "terminal.read", json!({ "session_id": session_id, "terminal_id": tid, "offset": -1 })),
        ("read count=0", "terminal.read", json!({ "session_id": session_id, "terminal_id": tid, "count": 0 })),
        (
            "read output_offset=-1",
            "terminal.read",
            json!({ "session_id": session_id, "terminal_id": tid, "output_offset": -1 }),
        ),
        ("read max_bytes=0", "terminal.read", json!({ "session_id": session_id, "terminal_id": tid, "max_bytes": 0 })),
        ("read wait_ms=-1", "terminal.read", json!({ "session_id": session_id, "terminal_id": tid, "wait_ms": -1 })),
        (
            "resize cols=0",
            "terminal.resize",
            json!({ "session_id": session_id, "terminal_id": tid, "cols": 0, "rows": 24 }),
        ),
        (
            "resize cols=-5",
            "terminal.resize",
            json!({ "session_id": session_id, "terminal_id": tid, "cols": -5, "rows": 24 }),
        ),
        (
            "close grace_ms=-1",
            "terminal.close",
            json!({ "session_id": session_id, "terminal_id": tid, "grace_ms": -1 }),
        ),
        (
            "close grace_ms=30001",
            "terminal.close",
            json!({ "session_id": session_id, "terminal_id": tid, "grace_ms": 30001 }),
        ),
    ] {
        let res = rpc.call_raw(method, params, |_| J::Null);
        assert_eq!(res["error"]["data"]["code"], "invalid_arguments", "{label} must return invalid_arguments: {res}");
    }

    // Ensure the terminal remained listed and closable after invalid close grace_ms.
    let listed = rpc.call_raw("terminal.list", json!({ "session_id": session_id }), |_| J::Null);
    assert_eq!(listed["result"]["data"].as_array().unwrap().len(), 1, "{listed}");
    let closed = rpc.call_raw("terminal.close", json!({ "session_id": session_id, "terminal_id": tid }), |_| J::Null);
    assert_eq!(closed["result"]["status"], "cancelled", "{closed}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| J::Null);
    rpc.wait_exit();
}

#[test]
fn lua_terminal_close_failure_retains_session_in_bucket() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().no_session().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;
        let opts = lua.create_table().unwrap();
        opts.set("name", "keep-on-stop-err").unwrap();
        opts.set("cmd", "tail").unwrap();
        opts.set("args", vec!["-f", "/dev/null"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let tid: String = opened.get("terminal_id").unwrap();
        let pid: u32 = opened.get("pid").unwrap();

        let bad_close = lua.create_table().unwrap();
        bad_close.set("terminal_id", tid.as_str()).unwrap();
        bad_close.set("grace_ms", 30001).unwrap();
        let err = call_proxy(&proxy, "close", Some(bad_close)).await.unwrap_err().to_string();
        assert!(err.contains("invalid_arguments") || err.contains("PTY_STOP_LIMIT"), "{err}");

        assert!(pid_alive(pid), "PTY process must still be alive when stop fails");
        assert_eq!(
            call_proxy(&proxy, "list", None).await.unwrap().get::<mlua::Table>("data").unwrap().len().unwrap(),
            1,
            "failed close must not drop the session from bucket.sessions before pty:stop succeeds"
        );

        let good_close = lua.create_table().unwrap();
        good_close.set("terminal_id", tid.as_str()).unwrap();
        let closed = call_proxy(&proxy, "close", Some(good_close)).await.unwrap();
        assert_eq!(closed.get::<String>("status").unwrap(), "cancelled");
        assert!(!pid_alive(pid));
    }));
}

#[test]
fn session_set_sandbox_mode_pty_policy_busy_does_not_poison_session_and_keeps_terminal_usable() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env.host().builtins(&["terminal"]).user_plugins().build().await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;
        let opts = lua.create_table().unwrap();
        opts.set("name", "keep-alive-on-sandbox-busy").unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("emulate", false).unwrap();
        opts.set("args", vec!["-c", "while IFS= read -r l; do printf 'ECHO:%s\\n' \"$l\"; done"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let tid: String = opened.get("terminal_id").unwrap();

        let err: String = eval(
            &host,
            r#"
            local S = mare._pctx.a:get("session")
            local ok, err = pcall(function() return S:set_sandbox_mode("read-only") end)
            assert(not ok, "expected set_sandbox_mode to fail while persistent terminal is open")
            assert(mare.task.session_error() == nil, "session must not be poisoned: " .. tostring(mare.task.session_error()))
            local rec_ok, rec_err = pcall(function() return S:add_record("probe/after-busy", { ok = true }) end)
            assert(rec_ok, "session record must still succeed after PTY_POLICY_BUSY: " .. tostring(rec_err))
            return tostring(err)
            "#,
        )
        .await;
        assert!(err.contains("PTY_POLICY_BUSY"), "expected PTY_POLICY_BUSY error, got: {err}");

        let send_arg = lua.create_table().unwrap();
        send_arg.set("terminal_id", tid.as_str()).unwrap();
        send_arg.set("text", "still-alive").unwrap();
        send_arg.set("submit", true).unwrap();
        send_arg.set("wait_ms", 5000).unwrap();
        let sent = call_proxy(&proxy, "send", Some(send_arg)).await.unwrap();
        let out = sent.get::<String>("output").unwrap();
        assert!(out.contains("ECHO:still-alive"), "terminal_send must remain usable: {out:?}");

        let close_arg = lua.create_table().unwrap();
        close_arg.set("terminal_id", tid.as_str()).unwrap();
        let _ = call_proxy(&proxy, "close", Some(close_arg)).await.unwrap();
        host.shutdown().await;
    }));
}

#[test]
fn permission_set_pty_policy_busy_keeps_sandbox_ready_and_recovers_after_terminal_close() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env
            .host()
            .builtins(&["terminal"])
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .project_trusted(true)
            .user_plugins()
            .build()
            .await;
        let lua = host.lua();

        let proxy = terminal_of(&host, "a").await;
        let opts = lua.create_table().unwrap();
        opts.set("name", "busy-preset-check").unwrap();
        opts.set("cmd", "sh").unwrap();
        opts.set("emulate", false).unwrap();
        opts.set("args", vec!["-c", "IFS= read -r _; exit 0"]).unwrap();
        let opened = call_proxy(&proxy, "create", Some(opts)).await.unwrap();
        let tid: String = opened.get("terminal_id").unwrap();

        let (busy_err, preset_after_busy, ready_ok): (String, String, bool) = eval(
            &host,
            r#"
            local P = mare._pctx.a:get("permissions")
            local ok, err = pcall(function() return P:set("read-only") end)
            assert(not ok, "P:set('read-only') must fail with PTY_POLICY_BUSY while terminal is open")
            -- Request readiness gate must stay open (sandbox_ready == true, not permanently OFF).
            local verdict = mare._pctx.a:get("policy"):decide("request/ready", { source = "test" })
            local ready = (verdict == nil or verdict.decision ~= "deny")
            return tostring(err), tostring(P:preset()), ready
            "#,
        )
        .await;
        assert!(busy_err.contains("PTY_POLICY_BUSY"), "expected PTY_POLICY_BUSY, got: {busy_err}");
        assert_eq!(preset_after_busy, "auto", "failed switch must keep previous preset intact");
        assert!(ready_ok, "request/ready must stay allowed when previous sandbox policy is intact");

        // Closing the terminal allows switching preset cleanly without restarting the session.
        let close_arg = lua.create_table().unwrap();
        close_arg.set("terminal_id", tid.as_str()).unwrap();
        let _ = call_proxy(&proxy, "close", Some(close_arg)).await.unwrap();

        let switched: String = eval(
            &host,
            r#"
            local P = mare._pctx.a:get("permissions")
            assert(P:set("read-only"))
            return tostring(P:preset())
            "#,
        )
        .await;
        assert_eq!(switched, "read-only");
        host.shutdown().await;
    }));
}

/// Switching sessions (`S:switch({ kind = "new" })`) after changing sandbox preset while a
/// non-drained persistent PTY (`mare.pty.spawn`) is open must fail cleanly with `PTY_POLICY_BUSY`
/// BEFORE `publish` and must NEVER poison the VM (`mare.task.session_error() == nil`), recovering
/// once closed. (Managed `terminal` service sessions are drained automatically on switch.)
#[test]
fn session_new_with_open_terminal_after_preset_change_returns_busy_without_poisoning_vm() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env
            .host()
            .builtins(&["terminal"])
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .project_trusted(true)
            .user_plugins()
            .build()
            .await;

        // Change preset away from startup baseline ("auto") to "read-only" before opening a raw
        // persistent PTY (not registered with `on_session_drain`), so switching to a new session
        // needs to reset sandbox back to baseline and hits the pre-publish `check_set` fence.
        exec(
            &host,
            r#"
            assert(mare._pctx.a:get("permissions"):set("read-only"))
            mare._held_pty = mare.pty.spawn({
                cmd = "sh",
                args = { "-c", "IFS= read -r _; exit 0" },
                cwd = mare.cwd,
                emulate = false,
            })
            "#,
        )
        .await;

        let (err_msg, same_sid, rec_ok): (String, bool, bool) = eval(
            &host,
            r#"
            local S = mare._pctx.a:get("session")
            local before_sid = S:info().session_id
            local ok, err = pcall(function() return S:switch({ kind = "new" }) end)
            assert(not ok, "S:switch(new) must fail with PTY_POLICY_BUSY while raw PTY holds different sandbox")
            assert(mare.task.session_error() == nil, "session must NOT be poisoned: " .. tostring(mare.task.session_error()))
            local after_sid = S:info().session_id
            local rec_ok = pcall(function() return S:add_record("probe/after-new-busy", { ok = true }) end)
            return tostring(err), before_sid == after_sid, rec_ok
            "#,
        )
        .await;
        assert!(err_msg.contains("PTY_POLICY_BUSY"), "expected PTY_POLICY_BUSY error, got: {err_msg}");
        assert!(same_sid, "failed switch must leave previous session published and intact");
        assert!(rec_ok, "session must remain writable without VM poison after precheck busy error");

        // Closing the PTY allows S:switch({ kind = "new" }) to succeed cleanly.
        exec(
            &host,
            r#"
            local p = mare._held_pty
            mare._held_pty = nil
            assert(p):stop({ grace_ms = 0 })
            "#,
        )
        .await;

        let switched_new: bool = eval(
            &host,
            r#"
            local S = mare._pctx.a:get("session")
            local before_sid = S:info().session_id
            S:switch({ kind = "new" })
            return S:info().session_id ~= before_sid
            "#,
        )
        .await;
        assert!(switched_new, "S:switch(new) must succeed once persistent PTY is closed");
        host.shutdown().await;
    }));
}

#[test]
fn session_fork_with_open_terminal_after_preset_change_returns_busy_without_poisoning_vm() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["a"]);
        let host = env
            .host()
            .builtins(&["terminal"])
            .config(json!({ "plugins": { "permissions": { "enabled": true } } }))
            .project_trusted(true)
            .user_plugins()
            .build()
            .await;

        // Both prompts run under baseline preset ("auto"); then switch to "read-only" (anchored at
        // turn 2's leaf) and open a raw persistent PTY. Forking before turn 2 restores turn 1's
        // "auto" sandbox and hits the pre-publish `check_set` fence with `PTY_POLICY_BUSY`.
        assert!(host.prompt("turn 1", json!({})).await.is_ok());
        assert!(host.prompt("turn 2", json!({})).await.is_ok());
        exec(&host, r#"assert(mare._pctx.a:get("permissions"):set("read-only"))"#).await;
        let turn2_user_id: String = eval(
            &host,
            r#"
            local S = mare._pctx.a:get("session")
            local last_user
            for _, it in ipairs(S:list_items().data) do
                if it.type == "user_message" then last_user = it.item_id end
            end
            return assert(last_user)
            "#,
        )
        .await;
        exec(
            &host,
            r#"
            mare._held_pty = mare.pty.spawn({
                cmd = "sh",
                args = { "-c", "IFS= read -r _; exit 0" },
                cwd = mare.cwd,
                emulate = false,
            })
            "#,
        )
        .await;

        let (err_msg, same_sid, rec_ok): (String, bool, bool) = eval(
            &host,
            &format!(
                r#"
            local S = mare._pctx.a:get("session")
            local before_sid = S:info().session_id
            local ok, err = pcall(function() return S:fork({{ session_id = before_sid, before_item_id = {:?} }}) end)
            assert(not ok, "S:fork must fail with PTY_POLICY_BUSY while raw PTY holds different sandbox")
            assert(mare.task.session_error() == nil, "session must NOT be poisoned: " .. tostring(mare.task.session_error()))
            local after_sid = S:info().session_id
            local rec_ok = pcall(function() return S:add_record("probe/after-fork-busy", {{ ok = true }}) end)
            return tostring(err), before_sid == after_sid, rec_ok
            "#,
                turn2_user_id
            ),
        )
        .await;
        assert!(err_msg.contains("PTY_POLICY_BUSY"), "expected PTY_POLICY_BUSY error, got: {err_msg}");
        assert!(same_sid, "failed fork must leave previous session published and intact");
        assert!(rec_ok, "session must remain writable without VM poison after fork precheck busy error");

        exec(
            &host,
            r#"
            local p = mare._held_pty
            mare._held_pty = nil
            assert(p):stop({ grace_ms = 0 })
            "#,
        )
        .await;

        let forked_ok: bool = eval(
            &host,
            &format!(
                r#"
            local S = mare._pctx.a:get("session")
            local before_sid = S:info().session_id
            local forked = S:fork({{ session_id = before_sid, before_item_id = {:?} }})
            return forked ~= nil and forked.session_id ~= before_sid
            "#,
                turn2_user_id
            ),
        )
        .await;
        assert!(forked_ok, "S:fork must succeed once persistent PTY is closed");
        host.shutdown().await;
    }));
}

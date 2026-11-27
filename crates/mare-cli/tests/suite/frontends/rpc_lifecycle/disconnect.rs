use super::*;

#[test]
fn rpc_lost_output_aborts_an_active_primary_turn() {
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            {"text":"x".repeat(4000), "delay_ms":25,
             "tool_calls":[{"name":"bash", "arguments":{"command":"touch should-not-run"}}]},
            {"text":"done"}
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let output = child.0.stdout.take().unwrap();
    let mut c = RpcClient::new(BufReader::new(output), child.0.stdin.take().unwrap());
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({"scope": "all", "include":["item/delta"]}), |_| json!(null));
    c.request("agent.prompt", json!({"session_id":sid,"input":"go"}));
    loop {
        let m = c.read();
        if m["method"] == "event" && m["params"]["type"] == "item/delta" {
            break;
        }
    }
    c.close_reader(); // close the actual read end while keeping stdin open
    let end = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() >= end {
            drop(child);
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(c);
    assert!(status.is_some(), "RPC process continued after output failed");
    assert!(!home.path().join("should-not-run").exists());
}

#[test]
fn rpc_stdin_eof_closes_primary_and_cancels_an_active_turn() {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{"text":"must not finish after input EOF", "delay_ms":60_000}]).to_string())
        .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut c = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.request("agent.prompt", json!({"session_id":sid,"input":"go"}));
    loop {
        let message = c.read();
        if message["method"] == "event" && message["params"]["type"] == "turn/started" {
            break;
        }
    }
    let mut r = c.into_reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = String::new();
        let _ = r.read_to_string(&mut output);
        let _ = tx.send(output);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            drop(child);
            panic!("RPC process kept the active turn after stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = rx.recv_timeout(Duration::from_secs(1)).expect("RPC stdout did not close");
    let messages: Vec<Value> = output.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(status.success(), "{status}");
    assert!(
        !messages.iter().any(|message| message["method"] == "stream.failed"),
        "EOF is connection closure, not a stream failure: {messages:?}"
    );
    assert!(!messages.iter().any(|message| message.to_string().contains("must not finish after input EOF")));
}

fn run_pre_start_rpc_eof_barrier(
    script: Option<Value>,
    init_lua: Option<&str>,
    plugin_body: impl FnOnce(&str) -> String,
    method: &str,
    build_params: impl FnOnce(&str) -> Value,
) -> (tempfile::TempDir, Vec<Value>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let barrier_url = format!("http://127.0.0.1:{}/barrier", listener.local_addr().unwrap().port());
    let config_dir = home.path().join("config");
    let plugin_dir = config_dir.join("plugins/barrier");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    if let Some(init) = init_lua {
        std::fs::write(config_dir.join("init.lua"), init).unwrap();
    }
    std::fs::write(plugin_dir.join("init.lua"), plugin_body(&barrier_url)).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(script) = script {
        let script_path = home.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        cmd.env("MARE_FAUX_SCRIPT", script_path);
    }
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stderr = child.0.stderr.take().unwrap();

    let mut c = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().unwrap_or("default").to_string();
    c.request(method, build_params(&sid));

    // Block until the spawned process actually enters the pre-start async boundary.
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (mut stream, _) = match accept_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(res) => res.unwrap(),
        Err(_) => {
            drop(child);
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            panic!("barrier listener did not receive connection for {method}; stderr:\n{err}");
        }
    };

    let mut r = c.into_reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = String::new();
        let _ = r.read_to_string(&mut output);
        let _ = tx.send(output);
    });

    // Release the barrier only after the server has *observed* stdin EOF, not merely after the
    // pipe was closed: on EOF it cancels the connection's in-flight requests, which drops the
    // pending barrier request and closes this socket. Waiting for that close orders "EOF observed"
    // before "barrier released" (otherwise the listener could return and the turn start before
    // the server read the EOF — the flake).
    {
        use std::io::Read as _;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => panic!("the server did not observe stdin EOF and drop the pre-start {method}: {e}"),
            }
        }
    }
    // (a late answer is harmless: nobody is reading it any more)
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let _ = stream.flush();
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            drop(child);
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            panic!("RPC process hung after stdin EOF during pre-start {method}; stderr:\n{err}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = rx.recv_timeout(Duration::from_secs(1)).expect("RPC stdout did not close");
    let messages: Vec<Value> = output.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert!(status.success(), "{status}");
    assert!(
        !messages.iter().any(|message| message["method"] == "stream.failed"),
        "EOF is connection closure, not a stream failure: {messages:?}"
    );
    (home, messages)
}

#[test]
fn rpc_stdin_eof_cancels_pre_start_prompt_without_tool_side_effects() {
    let script = json!([
        {"tool_calls": [{"name":"bash", "arguments":{"command":"touch should-not-run"}}]},
        {"text":"must not run"}
    ]);
    let (home, messages) = run_pre_start_rpc_eof_barrier(
        Some(script),
        Some("mare.setup { plugins = { agent = { enabled = false } } }\n"),
        |url| {
            format!(
                r#"return {{
    name = "barrier",
    provide = {{ "agent" }},
    apply = function(ctx)
        local Agent = require("cordis").class("Agent", mare.Service)
        function Agent:constructor(c) Agent.super.constructor(self, c, "agent") end
        for _, m in ipairs({{
            "messages", "prepare_commit", "register_activity", "set_messages",
            "adopt_session", "session_revision", "check_session_revision", "set_tool_states",
            "set_pending_request", "pending_request", "append", "notify", "set_session_id", "set_queued", "baseline_cut",
        }}) do
            Agent[m] = function() return {{}} end
        end
        function Agent:activity() return {{ version = 1, busy = false, queued = 0, turn_cancelled = false }} end
        function Agent:prompt()
            mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
            mare.fs.write(mare.cwd .. "/should-not-run", "ran")
            return {{ status = "started", turn_id = "blocked" }}
        end
        function Agent:steer() return {{ input_id = "none" }} end
        function Agent:cancel() return {{ removed = mare.json.array() }} end
        function Agent:continue() return {{ turn_id = "none" }} end
        function Agent:wait() return {{ turn_id = "none", status = "completed", started_at_ms = 0 }} end
        function Agent:get() return {{ busy = false, queued = 0 }} end
        function Agent:list_queue() return {{ data = mare.json.array() }} end
        function Agent:update_queued() return {{}} end
        function Agent:delete_queued() return {{}} end
        function Agent:list_messages() return {{ data = mare.json.array() }} end
        function Agent:persist_error() return nil end
        function Agent:interrupted() return false end
        ctx:plugin(Agent)
    end,
}}"#
            )
        },
        "agent.prompt",
        |sid| json!({"session_id": sid, "input": "run tool"}),
    );
    assert!(
        !messages.iter().any(|m| m["method"] == "event" && m["params"]["type"] == "turn/started"),
        "pre-start prompt must not start a turn after stdin EOF: {messages:?}"
    );
    assert!(!home.path().join("should-not-run").exists(), "tool side effect ran after stdin EOF");
}

#[test]
fn rpc_stdin_eof_cancels_pre_start_command_without_side_effects() {
    let (home, messages) = run_pre_start_rpc_eof_barrier(
        None,
        Some("mare.setup { plugins = { commands = { enabled = false } } }\n"),
        |url| {
            format!(
                r#"return {{
    name = "barrier",
    provide = {{ "commands" }},
    apply = function(ctx)
        local Commands = require("cordis").class("Commands", mare.Service)
        function Commands:constructor(c) Commands.super.constructor(self, c, "commands") end
        function Commands:register() return function() end end
        function Commands:run(line)
            if line == "/sideeffect" then
                mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
                mare.fs.write(mare.cwd .. "/command-ran", "ran")
            end
            return {{}}
        end
        function Commands:list() return {{ data = {{}} }} end
        function Commands:get() return nil end
        ctx:plugin(Commands)
    end,
}}"#
            )
        },
        "commands.run",
        |_| json!({"line": "/sideeffect"}),
    );
    assert!(
        !messages.iter().any(|m| m["method"] == "event" && m["params"]["type"] == "turn/started"),
        "pre-start command must not start work after stdin EOF: {messages:?}"
    );
    assert!(!home.path().join("command-ran").exists(), "command side effect ran after stdin EOF");
}

#[test]
fn rpc_stdin_eof_disconnects_while_stdout_remains_open() {
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{"text":"x".repeat(4000), "delay_ms":25}]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({"scope": "all", "include":["item/delta"]}), |_| json!(null));
    c.request("agent.prompt", json!({"session_id":sid,"input":"go"}));
    loop {
        let m = c.read();
        if m["method"] == "event" && m["params"]["type"] == "item/delta" {
            break;
        }
    }
    c.close_stdin();
    // The initialize reply and first stream frame above establish that the connection
    // is running. Keep its output reader alive while awaiting input-EOF teardown.
    let end = Instant::now() + Duration::from_secs(5);
    let completed = loop {
        if c.child.as_mut().unwrap().0.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= end {
            c.kill_and_wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(completed, "stdin EOF must close the primary connection while stdout remains open");
    drop(c);
}

#[test]
fn rpc_output_failure_aborts_a_command_started_turn() {
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("start_turn.lua"),
        r#"return { name = "start-turn", inject = { "commands", "agent", "task" }, apply = function(ctx)
      ctx.commands:register("start-turn", { run = function()
        ctx.task:spawn(function() ctx.agent:prompt("from command") end)
      end })
    end }"#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            {"text":"x".repeat(4000), "delay_ms":25,
             "tool_calls":[{"name":"bash", "arguments":{"command":"touch should-not-run"}}]},
            {"text":"done"}
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let input = child.0.stdin.take().unwrap();
    let mut c = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), input);
    c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    c.call_raw("events.subscribe", json!({"scope": "all", "include":["item/delta"]}), |_| json!(null));
    c.request("commands.run", json!({"line":"/start-turn"}));
    loop {
        let m = c.read();
        if m["method"] == "event" && m["params"]["type"] == "item/delta" {
            break;
        }
    }
    c.close_reader(); // stdout has lost its reader; keep stdin writable
    let end = Instant::now() + Duration::from_secs(5);
    let completed = loop {
        if child.0.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= end {
            drop(child);
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(c);
    assert!(completed, "the command-started turn survived output failure");
    assert!(!home.path().join("should-not-run").exists());
}

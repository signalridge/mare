use super::*;

/// `session.resume` aligns with TUI `resolve_resume` (`session/browser.lua:98-160`, Codex
/// `main.rs:366-390`): resolves by exact id, unique id prefix, or case-insensitive session name,
/// rejects ambiguous matches, and does not match arbitrary path substrings.
#[test]
fn rpc_session_resume_resolves_by_id_prefix_and_name_without_path_substring() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let s1 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id1 = s1["result"]["session_id"].as_str().unwrap().to_string();
    let p1 = c.call_raw("agent.prompt", json!({ "session_id": id1, "input": "hi 1" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id1, "turn_id": p1["result"]["turn_id"] }), |_| json!(null));
    let r1 = c.call_raw("session.update", json!({ "session_id": id1, "name": "Alpha-Work" }), |_| json!(null));
    assert_eq!(r1["result"]["name"], "Alpha-Work", "{r1}");
    let path1 = r1["result"]["path"].as_str().unwrap().to_string();

    let s2 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id2 = s2["result"]["session_id"].as_str().unwrap().to_string();
    let p2 = c.call_raw("agent.prompt", json!({ "session_id": id2, "input": "hi 2" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id2, "turn_id": p2["result"]["turn_id"] }), |_| json!(null));
    let r2 = c.call_raw("session.update", json!({ "session_id": id2, "name": "Beta-Work" }), |_| json!(null));
    assert_eq!(r2["result"]["name"], "Beta-Work", "{r2}");

    // Path substring must NOT match (e.g., "sessions" or ".jsonl" or parent dir slug).
    let sub_err = c.call_raw("session.load", json!({ "session_id": "sessions" }), |_| json!(null));
    assert!(sub_err.get("error").is_some(), "path substring should not match: {sub_err}");

    // Existing file path still resolves directly.
    let by_path = c.call_raw("session.load", json!({ "path": path1 }), |_| json!(null));
    assert_eq!(by_path["result"]["session_id"], id1, "{by_path}");

    // Switch to s2, then load s1 by case-insensitive session name.
    let back_to_2 = c.call_raw("session.load", json!({ "session_id": id2 }), |_| json!(null));
    assert_eq!(back_to_2["result"]["session_id"], id2, "{back_to_2}");

    let by_name = c.call_raw("session.load", json!({ "session_id": "alpha-work" }), |_| json!(null));
    assert_eq!(by_name["result"]["session_id"], id1, "{by_name}");

    // Ambiguous name must be rejected with a clear error.
    let back_for_rename = c.call_raw("session.load", json!({ "session_id": id2 }), |_| json!(null));
    assert_eq!(back_for_rename["result"]["session_id"], id2, "{back_for_rename}");
    let r2_dup = c.call_raw("session.update", json!({ "session_id": id2, "name": "alpha-work" }), |_| json!(null));
    assert!(r2_dup.get("error").is_none(), "{r2_dup}");
    let amb_name = c.call_raw("session.load", json!({ "session_id": "alpha-work" }), |_| json!(null));
    let amb_msg = amb_name["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(amb_msg.contains("matches 2 sessions") || amb_msg.contains("ambiguous"), "{amb_name}");

    // Unique id prefix resolves s2.
    let prefix_len = id1.chars().zip(id2.chars()).take_while(|(a, b)| a == b).count() + 1;
    let unique_prefix: String = id2.chars().take(prefix_len).collect();
    let by_prefix = c.call_raw("session.load", json!({ "session_id": unique_prefix }), |_| json!(null));
    assert_eq!(by_prefix["result"]["session_id"], id2, "{by_prefix}");

    c.shutdown_and_wait();
}

/// Exact id match takes precedence over a colliding session name, and ambiguous id prefixes
/// are rejected rather than picking the first entry.
#[test]
fn rpc_session_resume_prioritizes_exact_id_and_rejects_ambiguous_prefix() {
    let home = tempfile::tempdir().unwrap();
    // Even when unrelated local sessions exist on disk, a replaced `session` service must still
    // resolve against its own `session.list` contract rather than the built-in index.
    let local_dir = home.path().join("state/sessions/proj");
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::write(
        local_dir.join("2026-01-01T00-00-00_local.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "local-disk-0001", "cwd": home.path()}),
            json!({"type": "message", "entry_id": "m1", "role": "user", "content": "local"}),
        ),
    )
    .unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("session.lua"),
        r#"return {
  name = "session", override = true, provide = { "session" },
  apply = function(ctx)
    local all = {
      { id = "11111111-0000-0000-0000-000000000001", path = "/stub/first.jsonl", title = "22222222-0000-0000-0000-000000000002" },
      { id = "22222222-0000-0000-0000-000000000002", path = "/stub/second.jsonl", title = "shared-name" },
      { id = "11111111-9999-0000-0000-000000000003", path = "/stub/third.jsonl", title = "other" },
    }
    local cur = all[1]
    if mare.args and type(mare.args.resume) == "string" then
      for _, s in ipairs(all) do
        if s.path == mare.args.resume or s.id == mare.args.resume then
          cur = s
          break
        end
      end
    end
    local impl = {
      commit = function() return true end,
      append_compaction = function() return true end,
      append_prune = function() return true end,
      set_sandbox_mode = function(_, spec) return mare.proc.sandbox.set_current(spec) end,
      info = function() return { session_id = cur.id, session_epoch = cur.id .. "-epoch-0", path = cur.path, count = 0 } end,
      available = function() return true end,
      switching = function() return false end,
      child = function() return nil end,
      children = function() return {} end,
      short_id = function(_, id) return id end,
      load_transcript = function() end,
      get = function(_, sid)
        for _, s in ipairs(all) do
          if s.id == sid then
            return { session_id = s.id, session_epoch = (s.id == cur.id) and (s.id .. "-epoch-0") or nil, name = s.title, cwd = mare.cwd, path = s.path, loaded = (s.id == cur.id), created_at_ms = 0, updated_at_ms = 0 }
          end
        end
        require("mare_lib").fail("unknown_session", "no session matches " .. tostring(sid))
      end,
      list = function()
        local out = {}
        for i, s in ipairs(all) do
          out[i] = {
            session_id = s.id,
            session_epoch = (s.id == cur.id) and (s.id .. "-epoch-0") or nil,
            name = s.title,
            cwd = mare.cwd,
            path = s.path,
            loaded = (s.id == cur.id),
            created_at_ms = 0,
            updated_at_ms = 0,
          }
        end
        return { data = out }
      end,
      entries = function() return {} end,
      list_items = function() return { data = {} } end,
      add_record = function() return { record_id = "r1", durable = false } end,
      list_records = function() return { data = {} } end,
      leaf = function() return nil end,
      entry_id = function() return nil end,
      file_changes = function() return {} end,
      path_to = function() return {} end,
      set_leaf = function() return true end,
      update = function(_, name)
        cur.title = name
        return { session_id = cur.id, session_epoch = cur.id .. "-epoch-0", name = cur.title, cwd = mare.cwd, path = cur.path, loaded = true, created_at_ms = 0, updated_at_ms = 0 }
      end,
      label_item = function() return { item_id = "e1" } end,
      archive = function() return {} end,
      unarchive = function() return {} end,
      delete = function() return {} end,
      rewind = function() return cur end,
      transition = function() return { status = "rejected", error = "unsupported" } end,
      persist_message = function() return "mock-msg" end,
      record_tool_state = function() return "mock-ts" end,
      record_request = function() return "mock-req" end,
      record_request_end = function() return "mock-req-end" end,
      record_request_aborted = function() return "mock-req-abort" end,
      record_run_end = function() return true end,
      baseline_cut = function() return { parts = {} } end,
      snapshot = function() return { session_id = cur.id, path = cur.path, leaf_entry_id = "e1" } end,
      snapshot_context = function() return { entries = {}, message_count = 0 } end,
      search = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
      search_items = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
      get_item = function() return {} end,
      trace = function() return {} end,
      trace_item = function() return {} end,
    }
    local Session = require("cordis").class("Session", mare.Service)
    function Session:constructor(c)
      Session.super.constructor(self, c, "session")
    end
    -- every contract method on the class, `new` included (cordis-lua constructs a class whose
    -- method is named `new`, b74525e9)
    for name, fn in pairs(impl) do
      Session[name] = fn
    end
    ctx:plugin(Session)
  end,
}"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    // Exact id match wins even though entry #1 has title == entry #2's id.
    let exact =
        c.call_raw("session.load", json!({ "session_id": "22222222-0000-0000-0000-000000000002" }), |_| json!(null));
    assert_eq!(exact["result"]["session_id"], "22222222-0000-0000-0000-000000000002", "{exact}");

    // Ambiguous id prefix "11111111-" matches two entries -> must be rejected.
    let amb = c.call_raw("session.load", json!({ "session_id": "11111111-" }), |_| json!(null));
    let amb_msg = amb["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(amb_msg.contains("matches 2 sessions") || amb_msg.contains("ambiguous"), "{amb}");

    // Path substring "first" must NOT match "/stub/first.jsonl".
    let path_sub = c.call_raw("session.load", json!({ "session_id": "first" }), |_| json!(null));
    assert!(path_sub.get("error").is_some(), "path substring should not match: {path_sub}");

    c.shutdown_and_wait();
}

/// Name matching uses the same whitespace normalization and 160-char truncation as the TUI
/// session index (`mare_store::index::scan::one_line`), and rejects archived sessions by name.
#[test]
fn rpc_session_resume_normalizes_whitespace_and_truncation_and_rejects_archived_names() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(
        &faux,
        json!([{ "text": "one" }, { "text": "two" }, { "text": "three" }, { "text": "four" }]).to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    // Two sessions whose names differ only by whitespace ("release  plan" vs "release plan").
    let s1 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id1 = s1["result"]["session_id"].as_str().unwrap().to_string();
    let p1 = c.call_raw("agent.prompt", json!({ "session_id": id1, "input": "hi 1" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id1, "turn_id": p1["result"]["turn_id"] }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id1, "name": "release  plan" }), |_| json!(null));

    let s2 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id2 = s2["result"]["session_id"].as_str().unwrap().to_string();
    let p2 = c.call_raw("agent.prompt", json!({ "session_id": id2, "input": "hi 2" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id2, "turn_id": p2["result"]["turn_id"] }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id2, "name": "release plan" }), |_| json!(null));

    let amb_ws = c.call_raw("session.load", json!({ "session_id": "release plan" }), |_| json!(null));
    let amb_ws_msg = amb_ws["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(amb_ws_msg.contains("matches 2 sessions"), "{amb_ws}");

    // Two sessions whose >160-char names share the first 160 chars collide after truncation.
    let prefix_160 = "x".repeat(160);
    let long_a = format!("{prefix_160}-one");
    let long_b = format!("{prefix_160}-two");
    c.call_raw("session.load", json!({ "session_id": id1 }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id1, "name": long_a }), |_| json!(null));
    c.call_raw("session.load", json!({ "session_id": id2 }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id2, "name": long_b }), |_| json!(null));

    let amb_trunc = c.call_raw("session.load", json!({ "session_id": long_a }), |_| json!(null));
    let amb_trunc_msg = amb_trunc["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(amb_trunc_msg.contains("matches 2 sessions"), "{amb_trunc}");

    // Archived session matched by name must be rejected.
    let s3 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id3 = s3["result"]["session_id"].as_str().unwrap().to_string();
    let p3 = c.call_raw("agent.prompt", json!({ "session_id": id3, "input": "hi 3" }), |_| json!(null));
    assert!(p3.get("error").is_none(), "{p3}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id3, "turn_id": p3["result"]["turn_id"] }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id3, "name": "archived-target" }), |_| json!(null));

    // Archived session whose name overlaps its own unique id prefix is still rejected.
    let s4 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id4 = s4["result"]["session_id"].as_str().unwrap().to_string();
    assert_eq!(c.call_raw("session.close", json!({ "session_id": id3 }), |_| json!(null))["result"], json!({}));
    let arch = c.call_raw("session.archive", json!({ "session_id": id3 }), |_| json!(null));
    assert_eq!(arch["result"], json!({}), "{arch}");

    let arch_err = c.call_raw("session.load", json!({ "session_id": "archived-target" }), |_| json!(null));
    let arch_msg = arch_err["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(arch_msg.contains("archived"), "{arch_err}");

    let max_shared = [id1.as_str(), id2.as_str(), id3.as_str()]
        .into_iter()
        .map(|other| id4.chars().zip(other.chars()).take_while(|(a, b)| a == b).count())
        .max()
        .unwrap_or(0);
    let prefix4: String = id4.chars().take(max_shared + 1).collect();
    let p4 = c.call_raw("agent.prompt", json!({ "session_id": id4, "input": "hi 4" }), |_| json!(null));
    assert!(p4.get("error").is_none(), "{p4}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id4, "turn_id": p4["result"]["turn_id"] }), |_| json!(null));
    let ren4 = c.call_raw("session.update", json!({ "session_id": id4, "name": prefix4 }), |_| json!(null));
    assert_eq!(ren4["result"]["name"], prefix4, "{ren4}");
    c.call_raw("session.create", json!({}), |_| json!(null));
    assert_eq!(c.call_raw("session.close", json!({ "session_id": id4 }), |_| json!(null))["result"], json!({}));
    let arch4 = c.call_raw("session.archive", json!({ "session_id": id4 }), |_| json!(null));
    assert_eq!(arch4["result"], json!({}), "{arch4}");
    let by_prefix4 = c.call_raw("session.load", json!({ "session_id": prefix4 }), |_| json!(null));
    let arch4_msg = by_prefix4["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(arch4_msg.contains("archived"), "{by_prefix4}");

    c.shutdown_and_wait();
}

/// Incomplete trailing JSONL lines and truncated UTF-8 bytes at EOF are ignored by the index
/// snapshot reader (`scan.rs:372`), preserving the last complete name and ambiguity detection.
#[test]
fn rpc_session_resume_ignores_incomplete_trailing_line_and_truncated_utf8() {
    use std::io::Write;

    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let s1 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id1 = s1["result"]["session_id"].as_str().unwrap().to_string();
    let p1 = c.call_raw("agent.prompt", json!({ "session_id": id1, "input": "hi 1" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id1, "turn_id": p1["result"]["turn_id"] }), |_| json!(null));
    let r1 = c.call_raw("session.update", json!({ "session_id": id1, "name": "beta" }), |_| json!(null));
    let path1 = r1["result"]["path"].as_str().unwrap().to_string();

    let s2 = c.call_raw("session.create", json!({}), |_| json!(null));
    let id2 = s2["result"]["session_id"].as_str().unwrap().to_string();
    let p2 = c.call_raw("agent.prompt", json!({ "session_id": id2, "input": "hi 2" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": id2, "turn_id": p2["result"]["turn_id"] }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id2, "name": "beta" }), |_| json!(null));

    // Switch away to a fresh session so session 1 is not locked, then append an unterminated
    // trailing line changing its name to "alpha" (without trailing newline).
    c.call_raw("session.create", json!({}), |_| json!(null));
    let mut f1 = std::fs::OpenOptions::new().append(true).open(&path1).unwrap();
    f1.write_all(br#"{"type":"name","name":"alpha"}"#).unwrap();
    f1.flush().unwrap();
    drop(f1);

    // "beta" must STILL see both sessions and reject ambiguity!
    let amb = c.call_raw("session.load", json!({ "session_id": "beta" }), |_| json!(null));
    let amb_msg = amb["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(amb_msg.contains("matches 2 sessions"), "{amb}");

    // Rename session 2 away, and append truncated UTF-8 bytes to session 1's incomplete tail.
    c.call_raw("session.load", json!({ "session_id": id2 }), |_| json!(null));
    c.call_raw("session.update", json!({ "session_id": id2, "name": "gamma" }), |_| json!(null));
    c.call_raw("session.create", json!({}), |_| json!(null));
    let mut f1 = std::fs::OpenOptions::new().append(true).open(&path1).unwrap();
    f1.write_all(b"\xe3\x81").unwrap();
    f1.flush().unwrap();
    drop(f1);

    // Session 1's last complete name ("beta") must still resolve cleanly.
    let res = c.call_raw("session.load", json!({ "session_id": "beta" }), |_| json!(null));
    assert_eq!(res["result"]["session_id"], id1, "{res}");

    c.shutdown_and_wait();
}

/// Resolving a session with a large session file on disk runs index reconciliation on a blocking
/// thread so concurrent RPC requests on the single-threaded runtime remain responsive.
#[test]
fn rpc_session_resume_does_not_block_concurrent_requests_on_large_session_file() {
    use std::io::{BufRead, Write};

    let home = tempfile::tempdir().unwrap();
    let sessions_dir = home.path().join("state/sessions/large-test");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let big_path = sessions_dir.join("2026-01-01T00-00-00_big.jsonl");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&big_path).unwrap());
    writeln!(f, r#"{{"type":"session","version":3,"session_id":"big-0001","cwd":"{}"}}"#, home.path().display())
        .unwrap();
    let pad = "a".repeat(4096);
    for i in 0..3000 {
        writeln!(f, r#"{{"type":"message","entry_id":"m{i}","role":"user","content":"{pad}"}}"#).unwrap();
    }
    writeln!(f, r#"{{"type":"name","name":"big-session"}}"#).unwrap();
    f.flush().unwrap();
    drop(f);

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());

    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        while stdout.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(v) = line.parse::<Value>() {
                let _ = tx.send(v);
            }
            line.clear();
        }
    });

    // Initialize first.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocol_version":"3.0","capabilities":{{}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let rpc_budget = mare_cli::testkit::scaled_test_timeout(std::time::Duration::from_secs(10));
    let init = rx.recv_timeout(rpc_budget).expect("timeout waiting for initialize");
    assert_eq!(init["id"], 1, "{init}");
    let init_sid = init["result"]["session"]["session_id"].as_str().expect("session_id");

    // Pipeline session.load (which reconciles the ~12 MB file on spawn_blocking) and a
    // concurrent options.get request; both must complete within bounded time with exact expected results.
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session.load","params":{{"session_id":"nonexistent"}}}}"#)
        .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"options.get","params":{{"session_id":"{init_sid}"}}}}"#)
        .unwrap();
    stdin.flush().unwrap();

    let mut seen = std::collections::HashMap::new();
    let deadline = std::time::Instant::now() + rpc_budget;
    while seen.len() < 2 {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        let v = rx.recv_timeout(rem).expect("timeout waiting for pipelined responses");
        if let Some(id) = v.get("id").and_then(|x| x.as_i64()) {
            seen.insert(id, v);
        }
    }
    let resume_msg = seen[&2]["error"]["message"].as_str().unwrap_or_default();
    assert!(resume_msg.contains("no session matches `nonexistent`"), "{}", seen[&2]);
    assert!(seen[&3].get("result").is_some(), "{}", seen[&3]);

    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"shutdown","params":{{}}}}"#).unwrap();
    stdin.flush().unwrap();
    let _ = child.wait_timeout(rpc_budget);
    let _ = reader.join();
}

/// A peer granted `session.resume` must not be able to probe host files or create `<path>.lock`
/// files by passing path-like targets; only session IDs/prefixes/names are resolved, while the
/// primary connection retains path-based resume.
#[cfg(unix)]
#[test]
fn rpc_peer_session_resume_rejects_path_targets_without_file_probe_or_lock() {
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup { peer = { allow = { \"session.load\" } } }\n").unwrap();

    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();

    let existing_file = home.path().join("secret.txt");
    std::fs::write(&existing_file, "not a session\n").unwrap();
    let existing_lock = home.path().join("secret.txt.lock");

    let rel_dir = work.join("sub");
    std::fs::create_dir_all(&rel_dir).unwrap();
    let rel_existing_file = rel_dir.join("note.txt");
    std::fs::write(&rel_existing_file, "relative non-session\n").unwrap();
    let rel_existing_lock = rel_dir.join("note.txt.lock");

    let missing_file = home.path().join("missing.txt");
    let missing_jsonl = home.path().join("missing.jsonl");

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--peer", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut primary = RpcClient::spawn_cmd(&mut cmd);

    let pid = primary.pid();
    let sock = home.path().join(format!("state/peers/{pid}.sock"));
    let t0 = Instant::now();
    while !sock.exists() {
        assert!(t0.elapsed() < Duration::from_secs(30), "peer socket never appeared");
        // sleep-ok: polls for the peer socket file the child creates (30 s bound); no event for it
        std::thread::sleep(Duration::from_millis(50));
    }

    primary.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    // Create two sessions so both peer name/id load and primary path load can be verified.
    let s1 = primary.call_raw("session.create", json!({}), |_| json!(null));
    let id1 = s1["result"]["session_id"].as_str().unwrap().to_string();
    let p1 = primary.call_raw("agent.prompt", json!({ "session_id": id1, "input": "hi 1" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let _ = primary
        .call_raw("agent.wait", json!({ "session_id": id1, "turn_id": p1["result"]["turn_id"] }), |_| json!(null));
    let r1 = primary.call_raw("session.update", json!({ "session_id": id1, "name": "Peer-Target" }), |_| json!(null));
    let path1 = r1["result"]["path"].as_str().unwrap().to_string();

    let s2 = primary.call_raw("session.create", json!({}), |_| json!(null));
    let id2 = s2["result"]["session_id"].as_str().unwrap().to_string();
    let p2 = primary.call_raw("agent.prompt", json!({ "session_id": id2, "input": "hi 2" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    let _ = primary
        .call_raw("agent.wait", json!({ "session_id": id2, "turn_id": p2["result"]["turn_id"] }), |_| json!(null));

    let stream = UnixStream::connect(&sock).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut peer = RpcClient::timed(BufReader::new(stream.try_clone().unwrap()), stream);
    let init = peer.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");

    // Existing non-session files and nonexistent paths must be rejected uniformly with -32602
    // and must not leave any `.lock` file on disk.
    for target in [
        existing_file.to_str().unwrap(),
        missing_file.to_str().unwrap(),
        missing_jsonl.to_str().unwrap(),
        "sub/note.txt",
        "bare.jsonl",
    ] {
        let resp = peer.call_raw("session.load", json!({ "session_id": target }), |_| json!(null));
        assert_eq!(
            resp["error"]["code"], -32602,
            "peer session.load({target}) must return uniform -32602 invalid params: {resp}"
        );
        let msg = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(&format!("no session matches `{target}`")),
            "unexpected error message for {target}: {resp}"
        );
    }
    let path_field_err = peer.call_raw("session.load", json!({ "path": existing_file }), |_| json!(null));
    assert_eq!(path_field_err["error"]["code"], -32602, "{path_field_err}");

    assert!(!existing_lock.exists(), "peer must not create lock file beside {}", existing_file.display());
    assert!(!rel_existing_lock.exists(), "peer must not create lock file beside {}", rel_existing_file.display());

    // Primary connection can still load by file path.
    let primary_by_path = primary.call_raw("session.load", json!({ "path": path1 }), |_| json!(null));
    assert_eq!(primary_by_path["result"]["session_id"], id1, "{primary_by_path}");

    drop(peer);
    primary.shutdown_and_wait();
}

//! Behavioral tests for session save failure degradation
//! (still output final answer, quarantine unsaved records, warn, exit code 5).

use std::process::Stdio;

use serde_json::{Value, json};

use crate::common::*;

#[test]
fn headless_plain_save_failure_outputs_final_answer_quarantines_warns_and_exits_5() {
    let home = tempfile::tempdir().unwrap();
    // A regular file where `state/sessions` belongs makes every session write fail (even as root).
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(home.path().join("state/sessions"), "").unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "final answer from model" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["-p", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("final answer from model"), "expected final answer on stdout, got:\n{stdout}");
    assert!(stderr.contains("mare: warning:"), "expected warning on stderr, got:\n{stderr}");
    assert!(stderr.contains("quarantined at "), "expected quarantine path in stderr warning, got:\n{stderr}");

    let qdir = home.path().join("state/quarantine");
    let qfiles: Vec<_> = std::fs::read_dir(&qdir)
        .expect("quarantine dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&qfiles[0]).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "quarantine file should be private 0600, got {mode:o}");
    }
    let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
    let reported = stderr
        .lines()
        .find_map(|line| line.split_once("(quarantined at ").and_then(|(_, path)| path.strip_suffix(')')))
        .unwrap_or_else(|| panic!("stderr should name quarantine file:\n{stderr}"));
    assert_eq!(std::path::Path::new(reported), qfiles[0], "stderr should name the quarantined file");
    let records: Vec<Value> = qtext
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL line ({e}): {l}")))
        .collect();
    assert!(!records.is_empty(), "quarantine file should not be empty");
    assert!(qtext.contains("final answer from model"), "quarantine should contain assistant reply:\n{qtext}");
}

#[test]
fn headless_json_save_failure_emits_settled_text_reason_save_failed_and_quarantines() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(home.path().join("state/sessions"), "").unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "json final answer" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let events: Vec<Value> =
        stdout.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))).collect();
    assert!(events.iter().any(|e| e["type"] == "turn.failed"), "{events:?}");
    let settled = events.last().expect("settled event");
    assert_eq!(settled["type"], "settled", "{settled}");
    assert_eq!(settled["data"]["exit_code"], 5, "{settled}");
    assert_eq!(settled["data"]["reason"], "save_failed", "{settled}");
    assert_eq!(settled["data"]["text"], "json final answer", "{settled}");
    assert!(
        settled["data"]["error"]["code"].is_string()
            && settled["data"]["error"]["message"].as_str().unwrap_or("").contains("quarantined at "),
        "settled.error should be structured and mention quarantine path: {settled}"
    );
    assert!(stderr.contains("mare: warning:"), "stderr:\n{stderr}");

    let qdir = home.path().join("state/quarantine");
    let qfiles: Vec<_> = std::fs::read_dir(&qdir)
        .expect("quarantine dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
    let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
    assert!(qtext.contains("json final answer"), "quarantine should contain assistant reply:\n{qtext}");
}

#[test]
fn run_end_save_failure_after_response_outputs_answer_quarantines_and_exits_5() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("fail_run_end.lua"),
        r#"return {
  name = "fail_run_end",
  apply = function()
    local orig_open = mare.store.open
    mare.store.open = function(path, opts)
      local f = orig_open(path, opts)
      if type(f) ~= "userdata" and type(f) ~= "table" then return f end
      local wrap = {}
      return setmetatable(wrap, {
        __index = function(_, k)
          if k == "append" then
            return function(_, entry, aopts)
              if type(entry) == "table" and entry.type == "custom" and entry.kind == "agent/run-end" then
                error("run-end disk full", 0)
              end
              return f:append(entry, aopts)
            end
          end
          if k == "append_batch" then
            return function(_, entries, aopts)
              for _, entry in ipairs(entries or {}) do
                if type(entry) == "table" and entry.type == "custom" and entry.kind == "agent/run-end" then
                  error("run-end disk full", 0)
                end
              end
              return f:append_batch(entries, aopts)
            end
          end
          local v = f[k]
          if type(v) == "function" then
            return function(_, ...) return v(f, ...) end
          end
          return v
        end,
      })
    end
  end
}"#,
    )
    .unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "answer before run-end fail" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["-p", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("answer before run-end fail"), "stdout:\n{stdout}");
    assert!(stderr.contains("mare: warning:"), "stderr:\n{stderr}");
    assert!(stderr.contains("quarantined at "), "stderr:\n{stderr}");

    let qdir = home.path().join("state/quarantine");
    let qfiles: Vec<_> = std::fs::read_dir(&qdir)
        .expect("quarantine dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
    let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
    assert!(qtext.contains("run-end"), "quarantine should contain run-end record:\n{qtext}");
}

#[test]
fn save_failure_does_not_override_max_turns_and_still_warns() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    // Fail persistence on the tool_result message so turn 1 completes its tool call,
    // persistence fault + quarantine are genuinely triggered, and turn 2 hits --max-turns 1.
    std::fs::write(
        plugins.join("fail_tool_result.lua"),
        r#"return {
  name = "fail_tool_result",
  apply = function()
    local orig_open = mare.store.open
    mare.store.open = function(path, opts)
      local f = orig_open(path, opts)
      if type(f) ~= "userdata" and type(f) ~= "table" then return f end
      local wrap = {}
      return setmetatable(wrap, {
        __index = function(_, k)
          if k == "append_batch" then
            return function(_, entries, aopts)
              for _, entry in ipairs(entries or {}) do
                if entry.type == "message" and entry.message and entry.message.role == "tool_result" then
                  error("tool_result disk full", 0)
                end
              end
              return f:append_batch(entries, aopts)
            end
          end
          local v = f[k]
          if type(v) == "function" then
            return function(_, ...) return v(f, ...) end
          end
          return v
        end,
      })
    end
  end
}"#,
    )
    .unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "faux.json" } }] },
            { "text": "never reached" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    if cfg!(windows) {
        cmd.arg("--yolo");
    }
    let out = cmd
        .args(["--json", "--model", "faux/echo", "--max-turns", "1", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "max_turns (4) must win over save_failed:\n{stdout}\n{stderr}");
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["data"]["reason"], "max_turns", "{last}");

    // Save warning must not be masked by max_turns error.
    assert!(stderr.contains("mare: warning:"), "expected save warning on stderr:\n{stderr}");
    assert!(stderr.contains("quarantined at "), "expected quarantine path in stderr warning:\n{stderr}");
    let qdir = home.path().join("state/quarantine");
    let qfiles: Vec<_> = std::fs::read_dir(&qdir)
        .expect("quarantine dir should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
    let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
    assert!(qtext.contains("tool_result"), "quarantine should contain tool_result:\n{qtext}");
}

#[test]
fn structured_output_preserved_on_save_failed() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(home.path().join("state/sessions"), "").unwrap();

    let schema = home.path().join("schema.json");
    std::fs::write(
        &schema,
        json!({
            "type": "object",
            "properties": { "answer": { "type": "integer" } },
            "required": ["answer"],
            "additionalProperties": false
        })
        .to_string(),
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "{\"answer\":42}" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--output-schema", schema.to_str().unwrap(), "compute"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let settled: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(settled["data"]["reason"], "save_failed", "{settled}");
    assert_eq!(settled["data"]["exit_code"], 5, "{settled}");
    assert_eq!(settled["data"]["text"], "{\"answer\":42}", "{settled}");
    assert_eq!(settled["data"]["structured"], json!({ "answer": 42 }), "{settled}");
}

#[tokio::test]
async fn quarantine_handles_non_utf8_bytes_with_safe_fallback() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(env.home.path().join("state/sessions"), "").unwrap();
        let h = env.host().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"local ok, err = pcall(S.add_record, S, "bad_bytes", { raw = "\xff\xfe\x00abc" }, { persist = true })
               return not ok and err ~= nil"#,
        )
        .await;
        assert!(ok, "session.record should fail when sessions dir is blocked");
        h.shutdown().await;

        let qdir = env.home.path().join("state/quarantine");
        let qfiles: Vec<_> = std::fs::read_dir(&qdir)
            .expect("quarantine dir should exist")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
        let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
        let lines: Vec<Value> = qtext
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL fallback ({e}): {l}")))
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["type"], "custom", "{}", lines[0]);
        assert_eq!(lines[0]["kind"], "bad_bytes", "{}", lines[0]);
        let raw = lines[0]["data"]["raw"].as_str().expect("raw field should be preserved as string");
        assert!(raw.contains("abc"), "valid text should be preserved: {raw:?}");
        assert!(raw.contains("\\xff\\xfe"), "invalid bytes should be escaped: {raw:?}");
    })
    .await
}

#[cfg(unix)]
#[test]
fn quarantine_rejects_symlink_outside_state_dir() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(home.path().join("state/sessions"), "").unwrap();
    std::os::unix::fs::symlink(outside.path(), home.path().join("state/quarantine")).unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "still answered" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["-p", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("still answered"), "stdout:\n{stdout}");
    assert!(stderr.contains("quarantine failed:"), "expected quarantine failure in stderr:\n{stderr}");
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        0,
        "nothing may be written through symlink outside state dir"
    );
}

#[tokio::test]
async fn save_failure_after_open_file_and_initial_model_failure_quarantine_reliably() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "reply after open" }])).build().await;
        expose(&h);
        // Wrap mare.store.open so the 1st append_batch (ensure_file) succeeds and opens S.file,
        // and subsequent append_batch calls (initial_model_entry inside call_sync) fail.
        exec(
            &h,
            r#"local real_open = mare.store.open
               mare.store.open = function(p, opts)
                 local f = real_open(p, opts)
                 local calls = 0
                 return setmetatable({
                   append_batch = function(_, batch, o)
                     calls = calls + 1
                     if calls >= 3 then
                       return nil, "injected append_batch failure after open"
                     end
                     return f:append_batch(batch, o)
                   end,
                 }, {
                   __index = function(_, k)
                     local v = f[k]
                     if type(v) == "function" then
                       return function(_, ...)
                         return v(f, ...)
                       end
                     end
                     return v
                   end,
                 })
               end"#,
        )
        .await;
        let opened: String = eval(
            &h,
            r#"local ok, err = pcall(S.add_record, S, "pre_open", { ok = true }, { persist = true })
               local info = S:info()
               if ok and info and info.path then return "ok" end
               return "ok=" .. tostring(ok) .. " err=" .. tostring(err) .. " path=" .. tostring(info and info.path)"#,
        )
        .await;
        assert_eq!(opened, "ok", "session file should be open");

        let res = h.prompt("hello after open", json!({})).await.unwrap();
        assert_eq!(res["error_kind"], "save_failed", "{res}");
        assert_eq!(res["text"], "reply after open", "{res}");
        h.shutdown().await;

        let qdir = env.home.path().join("state/quarantine");
        let qfiles: Vec<_> = std::fs::read_dir(&qdir)
            .expect("quarantine dir should exist")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
        let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
        assert!(qtext.contains("\"type\":\"model\""), "initial model entry should be quarantined:\n{qtext}");
        assert!(qtext.contains("hello after open"), "user prompt must still be quarantined:\n{qtext}");
        assert!(qtext.contains("reply after open"), "assistant reply must be quarantined:\n{qtext}");
    })
    .await
}

#[tokio::test]
async fn persistent_fault_does_not_duplicate_quarantined_records() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(env.home.path().join("state/sessions"), "").unwrap();
        let h = env.host().faux(json!([{ "text": "reply one" }, { "text": "reply two" }])).build().await;
        let res1 = h.prompt("first prompt", json!({})).await.unwrap();
        assert_eq!(res1["error_kind"], "save_failed", "{res1}");
        let res2 = h.prompt("second prompt", json!({})).await.unwrap();
        assert_eq!(res2["error_kind"], "save_failed", "{res2}");
        h.shutdown().await;

        let qdir = env.home.path().join("state/quarantine");
        let qfiles: Vec<_> = std::fs::read_dir(&qdir)
            .expect("quarantine dir should exist")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
        let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
        assert_eq!(qtext.matches("first prompt").count(), 1, "first prompt duplicated:\n{qtext}");
        assert_eq!(qtext.matches("reply one").count(), 1, "reply one duplicated:\n{qtext}");
        assert_eq!(qtext.matches("second prompt").count(), 1, "second prompt duplicated:\n{qtext}");
        assert_eq!(qtext.matches("reply two").count(), 1, "reply two duplicated:\n{qtext}");
    })
    .await
}

#[tokio::test]
async fn quarantine_rejects_invalid_object_keys_without_fallback() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let (p1, e1, p2, e2): (Option<String>, Option<String>, Option<String>, Option<String>) = eval(
            &h,
            r#"local lib = require("mare_lib")
               local qpath1 = lib.quarantine_path(mare.paths.state, "bad-key-utf8")
               local bad_utf8_key = { type = "message", [string.char(0xff, 0xfe)] = "val" }
               local r1, err1 = lib.quarantine_records(qpath1, { bad_utf8_key }, "disk full", mare.paths.state)
               local qpath2 = lib.quarantine_path(mare.paths.state, "bad-key-table")
               local bad_tbl_key = { type = "message", [{}] = "val" }
               local r2, err2 = lib.quarantine_records(qpath2, { bad_tbl_key }, "disk full", mare.paths.state)
               return r1, err1, r2, err2"#,
        )
        .await;
        h.shutdown().await;
        assert!(p1.is_none(), "non-UTF8 key must fail quarantine: {p1:?} {e1:?}");
        assert!(e1.as_deref().unwrap_or("").contains("invalid"), "{e1:?}");
        assert!(p2.is_none(), "non-string key must fail quarantine: {p2:?} {e2:?}");
        assert!(e2.as_deref().unwrap_or("").contains("invalid"), "{e2:?}");
        let qdir = env.home.path().join("state/quarantine");
        if qdir.exists() {
            assert_eq!(std::fs::read_dir(&qdir).unwrap().count(), 0, "no fallback file may be written");
        }
    })
    .await
}

#[tokio::test]
async fn partial_quarantine_failure_initial_model_fails_then_recovers_on_retry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "reply one" }, { "text": "reply two" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"local real_open = mare.store.open
               mare.store.open = function(p, opts)
                 local f = real_open(p, opts)
                 local calls = 0
                 return setmetatable({
                   append_batch = function(_, batch, o)
                     calls = calls + 1
                     if calls >= 3 then
                       return nil, "injected append_batch failure"
                     end
                     return f:append_batch(batch, o)
                   end,
                 }, {
                   __index = function(_, k)
                     local v = f[k]
                     if type(v) == "function" then
                       return function(_, ...)
                         return v(f, ...)
                       end
                     end
                     return v
                   end,
                 })
               end
               local real_write = mare.fs.write
               local failed_once = false
               mare.fs.write = function(p, data, opts)
                 if not failed_once and type(p) == "string" and p:find("/quarantine/", 1, true) and type(data) == "string" and data:find('"type":"model"', 1, true) then
                   failed_once = true
                   return nil, "injected quarantine write failure on initial model"
                 end
                 return real_write(p, data, opts)
               end"#,
        )
        .await;
        let opened: String = eval(
            &h,
            r#"local ok, err = pcall(S.add_record, S, "pre_open", { ok = true }, { persist = true })
               local info = S:info()
               if ok and info and info.path then return "ok" end
               return "ok=" .. tostring(ok) .. " err=" .. tostring(err)"#,
        )
        .await;
        assert_eq!(opened, "ok", "session file should be open");

        let res1 = h.prompt("first prompt", json!({})).await.unwrap();
        assert_eq!(res1["error_kind"], "save_failed", "{res1}");
        let err1 = res1["error"]["message"].as_str().unwrap_or("");
        assert!(err1.contains("quarantined at "), "current msg should be quarantined: {err1}");
        assert!(err1.contains("quarantine failed:"), "partial quarantine failure must be reported: {err1}");

        // Second prompt flushes pending without duplicating "first prompt", and quarantines initial model.
        let res2 = h.prompt("second prompt", json!({})).await.unwrap();
        assert_eq!(res2["error_kind"], "save_failed", "{res2}");
        h.shutdown().await;

        let qdir = env.home.path().join("state/quarantine");
        let qfiles: Vec<_> = std::fs::read_dir(&qdir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        assert!(!qfiles.is_empty(), "expected at least 1 quarantine file, got: {qfiles:?}");
        let qtext: String = qfiles.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
        assert_eq!(qtext.matches("\"type\":\"model\"").count(), 1, "initial model should recover once:\n{qtext}");
        let first_msg_count =
            qtext.lines().filter(|l| l.contains("\"type\":\"message\"") && l.contains("first prompt")).count();
        assert_eq!(first_msg_count, 1, "first prompt must not duplicate:\n{qtext}");
    })
    .await
}

#[tokio::test]
async fn partial_quarantine_failure_current_message_fails_then_recovers_on_retry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "reply one" }, { "text": "reply two" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"local real_open = mare.store.open
               mare.store.open = function(p, opts)
                 local f = real_open(p, opts)
                 local calls = 0
                 return setmetatable({
                   append_batch = function(_, batch, o)
                     calls = calls + 1
                     if calls >= 3 then
                       return nil, "injected append_batch failure"
                     end
                     return f:append_batch(batch, o)
                   end,
                 }, {
                   __index = function(_, k)
                     local v = f[k]
                     if type(v) == "function" then
                       return function(_, ...)
                         return v(f, ...)
                       end
                     end
                     return v
                   end,
                 })
               end
               local real_write = mare.fs.write
               local failed_once = false
               mare.fs.write = function(p, data, opts)
                 if not failed_once and type(p) == "string" and p:find("/quarantine/", 1, true) and type(data) == "string" and data:find('"type":"message"', 1, true) and data:find("first prompt", 1, true) then
                   failed_once = true
                   return nil, "injected quarantine write failure on current message"
                 end
                 return real_write(p, data, opts)
               end"#,
        )
        .await;
        let opened: String = eval(
            &h,
            r#"local ok, err = pcall(S.add_record, S, "pre_open", { ok = true }, { persist = true })
               local info = S:info()
               if ok and info and info.path then return "ok" end
               return "ok=" .. tostring(ok) .. " err=" .. tostring(err)"#,
        )
        .await;
        assert_eq!(opened, "ok", "session file should be open");

        let res1 = h.prompt("first prompt", json!({})).await.unwrap();
        assert_eq!(res1["error_kind"], "save_failed", "{res1}");
        let err1 = res1["error"]["message"].as_str().unwrap_or("");
        assert!(err1.contains("quarantined at "), "initial model should be quarantined: {err1}");
        assert!(err1.contains("quarantine failed:"), "partial quarantine failure must be reported: {err1}");

        // Second prompt flushes pending: retries current message without duplicating initial model.
        let res2 = h.prompt("second prompt", json!({})).await.unwrap();
        assert_eq!(res2["error_kind"], "save_failed", "{res2}");
        h.shutdown().await;

        let qdir = env.home.path().join("state/quarantine");
        let qfiles: Vec<_> = std::fs::read_dir(&qdir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        assert_eq!(qfiles.len(), 1, "expected 1 quarantine file, got: {qfiles:?}");
        let qtext = std::fs::read_to_string(&qfiles[0]).unwrap();
        assert_eq!(qtext.matches("\"type\":\"model\"").count(), 1, "initial model must not duplicate:\n{qtext}");
        let first_msg_count =
            qtext.lines().filter(|l| l.contains("\"type\":\"message\"") && l.contains("first prompt")).count();
        assert_eq!(first_msg_count, 1, "first prompt should recover once:\n{qtext}");
    })
    .await
}

#[tokio::test]
async fn retry_initial_model_does_not_overwrite_earlier_quarantine_path() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "reply one" }, { "text": "reply two" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"local real_open = mare.store.open
               mare.store.open = function(p, opts)
                 local f = real_open(p, opts)
                 local calls = 0
                 return setmetatable({
                   append_batch = function(_, batch, o)
                     calls = calls + 1
                     if calls >= 3 then
                       return nil, "injected append_batch failure"
                     end
                     return f:append_batch(batch, o)
                   end,
                 }, {
                   __index = function(_, k)
                     local v = f[k]
                     if type(v) == "function" then
                       return function(_, ...)
                         return v(f, ...)
                       end
                     end
                     return v
                   end,
                 })
               end
               local real_write = mare.fs.write
               local failed_once = false
               mare.fs.write = function(p, data, opts)
                 if not failed_once and type(p) == "string" and p:find("/quarantine/", 1, true) and type(data) == "string" and data:find('"type":"model"', 1, true) then
                   failed_once = true
                   return nil, "injected quarantine write failure on initial model"
                 end
                 return real_write(p, data, opts)
               end"#,
        )
        .await;
        let opened: String = eval(
            &h,
            r#"local ok, err = pcall(S.add_record, S, "pre_open", { ok = true }, { persist = true })
               local info = S:info()
               if ok and info and info.path then return "ok" end
               return "ok=" .. tostring(ok) .. " err=" .. tostring(err)"#,
        )
        .await;
        assert_eq!(opened, "ok", "session file should be open");

        let res1 = h.prompt("first prompt", json!({})).await.unwrap();
        assert_eq!(res1["error_kind"], "save_failed", "{res1}");
        let err1 = res1["error"]["message"].as_str().unwrap_or("");
        let marker = "quarantined at ";
        let start = err1.find(marker).expect("err1 should contain quarantine path") + marker.len();
        let rest = &err1[start..];
        let end = rest.find([';', ')']).unwrap_or(rest.len());
        let first_qpath = rest[..end].trim().to_string();
        assert!(!first_qpath.is_empty(), "empty first_qpath from: {err1}");

        // Second prompt retries initial_model into S.quarantine (a different path);
        // the earlier message's quarantine path must still be preserved in res2["error"].
        let res2 = h.prompt("second prompt", json!({})).await.unwrap();
        assert_eq!(res2["error_kind"], "save_failed", "{res2}");
        let err2 = res2["error"]["message"].as_str().unwrap_or("");
        assert!(
            err2.contains(&first_qpath),
            "earlier quarantine path {first_qpath} was overwritten in res2 error: {err2}"
        );
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[test]
fn quarantine_rejects_symlinks_when_mare_home_is_symlink_alias() {
    let base = tempfile::tempdir().unwrap();
    let real_home = base.path().join("real_home");
    let alias_home = base.path().join("alias_home");
    std::fs::create_dir_all(real_home.join("state/inside_target")).unwrap();
    std::fs::write(real_home.join("state/sessions"), "").unwrap();
    std::os::unix::fs::symlink(&real_home, &alias_home).unwrap();

    let script = base.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "still answered" }]).to_string()).unwrap();

    // 1) state/quarantine points to another dir inside state: must be rejected even via symlinked MARE_HOME.
    let q_link = real_home.join("state/quarantine");
    std::os::unix::fs::symlink(real_home.join("state/inside_target"), &q_link).unwrap();
    let out1 = mare_cmd()
        .args(["-p", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", &alias_home)
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(base.path())
        .stdin(Stdio::null())
        .run_output();
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    assert_eq!(out1.status.code(), Some(5), "stderr:\n{stderr1}");
    assert!(stderr1.contains("quarantine failed:"), "expected quarantine failure in stderr:\n{stderr1}");
    assert_eq!(
        std::fs::read_dir(real_home.join("state/inside_target")).unwrap().count(),
        0,
        "nothing may be written through symlink inside state dir"
    );

    // 2) state/quarantine points to /dev/shm: must be rejected (no /dev exception for explicit roots).
    if let Ok(shm_dir) = tempfile::tempdir_in("/dev/shm") {
        std::fs::remove_file(&q_link).unwrap();
        std::os::unix::fs::symlink(shm_dir.path(), &q_link).unwrap();
        let out2 = mare_cmd()
            .args(["-p", "--model", "faux/echo", "hello"])
            .env("MARE_HOME", &alias_home)
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(base.path())
            .stdin(Stdio::null())
            .run_output();
        let stderr2 = String::from_utf8_lossy(&out2.stderr);
        assert_eq!(out2.status.code(), Some(5), "stderr:\n{stderr2}");
        assert!(stderr2.contains("quarantine failed:"), "expected quarantine failure for /dev/shm:\n{stderr2}");
        assert_eq!(
            std::fs::read_dir(shm_dir.path()).unwrap().count(),
            0,
            "nothing may be written through symlink to /dev/shm"
        );
    }

    // 3) Normal state/quarantine under symlinked MARE_HOME must succeed.
    std::fs::remove_file(&q_link).unwrap();
    let out3 = mare_cmd()
        .args(["-p", "--model", "faux/echo", "hello"])
        .env("MARE_HOME", &alias_home)
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(base.path())
        .stdin(Stdio::null())
        .run_output();
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    assert_eq!(out3.status.code(), Some(5), "stderr:\n{stderr3}");
    assert!(stderr3.contains("quarantined at "), "expected quarantine success:\n{stderr3}");
    assert_eq!(std::fs::read_dir(&q_link).unwrap().count(), 1);
}

#[tokio::test]
async fn quarantine_reversible_bad_byte_escaping_distinguishes_literal_escapes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let qpath: String = eval(
            &h,
            r#"local lib = require("mare_lib")
               local qpath = lib.quarantine_path(mare.paths.state, "reversible-bytes")
               local entries = {
                 { type = "custom", kind = "bad_then_lit", data = { v = string.char(0xff) .. "\\xff" } },
                 { type = "custom", kind = "lit_then_bad", data = { v = "\\xff" .. string.char(0xff) } },
                 { type = "custom", kind = "lit_only",     data = { v = "\\xff\\xff" } },
                 { type = "custom", kind = "marker_lit",   data = { v = "[mare:escaped-utf8-v1]\\xff" } },
               }
               local p, err = lib.quarantine_records(qpath, entries, "disk full", mare.paths.state)
               assert(p, tostring(err))
               return p"#,
        )
        .await;
        let qtext = std::fs::read_to_string(&qpath).unwrap();
        let lines: Vec<Value> = qtext.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 4, "{lines:?}");
        let v_bad_then_lit = lines[0]["data"]["v"].as_str().unwrap();
        let v_lit_then_bad = lines[1]["data"]["v"].as_str().unwrap();
        let v_lit_only = lines[2]["data"]["v"].as_str().unwrap();
        let v_marker_lit = lines[3]["data"]["v"].as_str().unwrap();

        assert_ne!(v_bad_then_lit, v_lit_then_bad, "bad+lit must differ from lit+bad");
        assert_ne!(v_bad_then_lit, v_lit_only, "bad+lit must differ from lit_only");
        assert_ne!(v_lit_then_bad, v_lit_only, "lit+bad must differ from lit_only");
        assert!(v_bad_then_lit.starts_with("[mare:escaped-utf8-v1]"), "expected marker prefix: {v_bad_then_lit:?}");
        assert!(v_lit_then_bad.starts_with("[mare:escaped-utf8-v1]"), "expected marker prefix: {v_lit_then_bad:?}");
        assert!(!v_lit_only.starts_with("[mare:escaped-utf8-v1]"), "literal text must not be marked: {v_lit_only:?}");
        assert_ne!(v_marker_lit, v_bad_then_lit, "literal marker prefix must not collide");

        let roundtrip_ok: bool = eval(
            &h,
            &format!(
                r#"local lib = require("mare_lib")
                   local ok1 = lib.unescape_quarantine_utf8({s0}) == (string.char(0xff) .. "\\xff")
                   local ok2 = lib.unescape_quarantine_utf8({s1}) == ("\\xff" .. string.char(0xff))
                   local ok3 = lib.unescape_quarantine_utf8({s2}) == "\\xff\\xff"
                   local ok4 = lib.unescape_quarantine_utf8({s3}) == "[mare:escaped-utf8-v1]\\xff"
                   return ok1 and ok2 and ok3 and ok4"#,
                s0 = serde_json::to_string(v_bad_then_lit).unwrap(),
                s1 = serde_json::to_string(v_lit_then_bad).unwrap(),
                s2 = serde_json::to_string(v_lit_only).unwrap(),
                s3 = serde_json::to_string(v_marker_lit).unwrap(),
            ),
        )
        .await;
        assert!(roundtrip_ok, "reversible bad-byte roundtrip failed");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn quarantine_reversible_escaping_preserves_deep_markers_null_arrays_and_nonfinite() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let qpath: String = eval(
            &h,
            r#"local lib = require("mare_lib")
               local qpath = lib.quarantine_path(mare.paths.state, "deep-and-mixed")
               local deep = { marker = "[mare:escaped-utf8-v1]\\xff" }
               for _ = 1, 40 do
                 deep = { nested = deep }
               end
               local entries = {
                 { type = "custom", kind = "deep_marker", data = deep },
                 {
                   type = "custom",
                   kind = "mixed_values",
                   data = {
                     bad = string.char(0xff) .. "ok",
                     null_val = mare.json.null,
                     empty_arr = mare.json.array({}),
                     nan_val = 0 / 0,
                   },
                 },
               }
               local p, err = lib.quarantine_records(qpath, entries, "disk full", mare.paths.state)
               assert(p, tostring(err))
               return p"#,
        )
        .await;
        let qtext = std::fs::read_to_string(&qpath).unwrap();
        let lines: Vec<Value> = qtext.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");

        let mut cur = &lines[0]["data"];
        for _ in 0..40 {
            cur = &cur["nested"];
        }
        let deep_marker = cur["marker"].as_str().expect("deep marker must remain a string");
        let roundtrip_deep: String = eval(
            &h,
            &format!(
                r#"return require("mare_lib").unescape_quarantine_utf8({})"#,
                serde_json::to_string(deep_marker).unwrap()
            ),
        )
        .await;
        assert_eq!(roundtrip_deep, "[mare:escaped-utf8-v1]\\xff", "deep literal marker must roundtrip");

        let mixed = &lines[1]["data"];
        assert!(mixed["null_val"].is_null(), "mare.json.null must stay JSON null: {mixed}");
        assert_eq!(mixed["empty_arr"], json!([]), "empty array must stay JSON []: {mixed}");
        assert!(mixed["nan_val"].is_null(), "non-finite number must serialize as JSON null: {mixed}");
        h.shutdown().await;
    })
    .await
}

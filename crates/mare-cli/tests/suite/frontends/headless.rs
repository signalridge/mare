//! Real process regressions for headless.
use super::*;

#[test]
fn headless_json_emits_one_event_per_line_in_turn_order() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": "echo hi" } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    let mut mare = mare_cmd();
    // Windows' default sandbox tier has no network control, so the read-only preset of this
    // untrusted folder asks before every shell command (permissions/vote.lua `net_note`) and
    // headless denies it: the call runs there only without approvals
    if cfg!(windows) {
        mare.arg("--yolo");
    }
    let out = mare
        .args(["--json", "--model", "faux/echo", "--no-session", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let events: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not one JSON event per line ({e}): {l}")))
        .collect();
    assert!(events.iter().all(|e| e["type"].is_string() && e.get("data").is_some()), "every event is {{type, data}}");
    let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    let at = |n: &str| names.iter().position(|x| *x == n).unwrap_or_else(|| panic!("no {n} in {names:?}"));
    let order = ["session", "turn/started", "item/started", "turn/completed", "settled"];
    let idx: Vec<usize> = order.iter().map(|n| at(n)).collect();
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "event order {order:?} at {idx:?}");
    let step_idx = at("step/started");
    let tool_started_idx = events
        .iter()
        .position(|e| e["type"] == "item/started" && e["data"]["item"]["type"] == "tool_call")
        .unwrap_or_else(|| panic!("no tool_call item/started in {names:?}"));
    let tool_completed_idx = events
        .iter()
        .position(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call")
        .unwrap_or_else(|| panic!("no tool_call item/completed in {names:?}"));
    assert!(
        step_idx < tool_started_idx && tool_started_idx < tool_completed_idx && tool_completed_idx < idx[3],
        "expected step/started ({step_idx}) < tool item/started ({tool_started_idx}) < tool item/completed ({tool_completed_idx}) < turn/completed ({})",
        idx[3]
    );
    // the contract: session record first, settled last, only public events in between
    assert_eq!(names.first(), Some(&"session"));
    assert_eq!(events[0]["data"]["protocol_version"], mare_protocol::PROTOCOL_VERSION);
    assert_eq!(names.last(), Some(&"settled"));
    assert!(events[0]["data"]["session_id"].is_string() && events[0]["data"]["cwd"].is_string(), "{}", events[0]);
    let internal = ["app/shutdown", "context/transform", "request/build", "input/transform", "request/prepare"];
    assert!(!names.iter().any(|n| internal.contains(n)), "internal events leaked: {names:?}");
    let end = &events[at("turn/completed")]["data"];
    assert!(end["error"].is_null(), "{end}");
    let settled = &events[names.len() - 1]["data"];
    assert_eq!(settled["text"], "done", "{settled}");
    assert!(!names.contains(&"turn.failed"));
}

/// `--json` is item level (Codex `exec --json`): no per-chunk `agent/stream` events — the
/// finished message carries the text; `--stream-deltas` adds them back.
#[test]
fn headless_json_streams_deltas_only_when_asked() {
    let home = tempfile::tempdir().unwrap();
    let run = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["--json", "--model", "faux/echo", "--no-session"];
        args.extend_from_slice(extra);
        args.push("a long enough prompt to stream in more than one chunk");
        let out = mare_cmd()
            .args(&args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["type"].as_str().unwrap().to_string())
            .collect()
    };
    let items = run(&[]);
    assert!(!items.iter().any(|n| n == "item/delta"), "deltas in plain --json: {items:?}");
    assert!(items.iter().any(|n| n == "item/completed") && items.iter().any(|n| n == "usage/changed"), "{items:?}");
    let deltas = run(&["--stream-deltas"]);
    assert!(deltas.iter().any(|n| n == "item/delta"), "--stream-deltas: {deltas:?}");
}

#[test]
fn no_session_runs_are_marked_ephemeral_in_the_session_record() {
    let home = tempfile::tempdir().unwrap();
    let run = |extra: &[&str]| {
        let mut args = vec!["--json", "--model", "faux/echo"];
        args.extend_from_slice(extra);
        args.push("hi");
        let out = mare_cmd()
            .args(&args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let first = String::from_utf8(out.stdout).unwrap().lines().next().unwrap().to_string();
        serde_json::from_str::<Value>(&first).unwrap()
    };
    // --no-session: nothing persisted, no crash recovery — the caller is told up front
    let rec = run(&["--no-session"]);
    assert_eq!(rec["type"], "session");
    assert_eq!(rec["data"]["ephemeral"], true, "{rec}");
    assert!(rec["data"]["path"].is_null(), "{rec}");
    // a normal run (its file is created on the first write) carries no ephemeral mark
    let rec = run(&[]);
    assert!(rec["data"].get("ephemeral").is_none(), "{rec}");
}

/// `mare --json …` with a faux script: (exit code, the `settled` line's data).
fn headless_run(script: Value, extra: &[&str]) -> (Option<i32>, Value) {
    let (code, settled, _) = headless_run_with(script, extra, "");
    (code, settled)
}

/// [`headless_run`] with Lua as the config's `init.lua`, plus the run's stderr.
fn headless_run_with(script: Value, extra: &[&str], init_lua: &str) -> (Option<i32>, Value, String) {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("faux.json");
    std::fs::write(&path, script.to_string()).unwrap();
    if !init_lua.is_empty() {
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(home.path().join("config/init.lua"), init_lua).unwrap();
    }
    let mut args = vec!["--json", "--model", "faux/echo", "--no-session"];
    args.extend_from_slice(extra);
    let out = mare_cmd()
        .args(&args)
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &path)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let last = stdout.lines().last().unwrap_or_else(|| panic!("no output: {stderr}"));
    let ev: Value = serde_json::from_str(last).unwrap();
    assert_eq!(ev["type"], "settled", "{stdout}");
    (out.status.code(), ev["data"].clone(), stderr)
}

/// Headless exit codes: each outcome has its code, and `settled` says the same.
#[test]
fn headless_exit_codes_follow_the_outcome_and_are_in_settled() {
    let tool = |cmd: &str| json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": cmd } }] });
    // 0: completed
    let (code, s) = headless_run(json!([{ "text": "ok" }]), &["hi"]);
    assert_eq!((code, s["exit_code"].as_i64(), s["reason"].as_str()), (Some(0), Some(0), Some("completed")), "{s}");
    // the run's total cost rides on settled.usage (live tests and evals report it)
    assert!(s["usage"]["cost"].is_number(), "settled.usage.cost: {s}");
    // 4: --max-turns reached (the model keeps calling tools)
    let (code, s) = headless_run(
        json!([tool("true"), tool("true"), tool("true"), { "text": "never" }]),
        &["--yolo", "--max-turns", "1", "go"],
    );
    assert_eq!((code, s["reason"].as_str()), (Some(4), Some("max_turns")), "{s}");
    // 4: --timeout reached (a slow reply)
    let (code, s) = headless_run(json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]), &["--timeout", "300ms", "go"]);
    assert_eq!((code, s["reason"].as_str()), (Some(4), Some("timeout")), "{s}");
    // an approval nobody could answer is a denied call, not a failed run: the turn completes (0)
    let ask = json!({ "tool_calls": [{ "name": "bash", "arguments": {
        "command": "touch x", "sandbox_permissions": "all", "justification": "test" } }] });
    let (code, s) = headless_run(json!([ask, { "text": "gave up" }]), &["--permission", "read-only", "go"]);
    assert_eq!((code, s["reason"].as_str(), s["error"].is_null()), (Some(0), Some("completed"), true), "{s}");
    // 2: bad arguments (an unparseable --timeout) — still one settled line in --json
    let (code, s) = headless_run(json!([]), &["--timeout", "soon", "go"]);
    assert_eq!((code, s["reason"].as_str()), (Some(2), Some("usage")), "{s}");
}

/// Headless denials (Codex `exec/src/lib.rs:1795-1797`, pi `print-mode.ts:146-169`, reference implementation
/// `headless/README.md:96`): calls denied because nobody could answer are listed on stderr, and a
/// turn that completes after them still exits 0 with no `settled.error`.
#[test]
fn headless_denied_calls_are_listed_on_stderr_and_the_completed_turn_exits_0() {
    let ask = |cmd: &str| {
        json!({ "tool_calls": [{ "name": "bash", "arguments": {
            "command": cmd, "sandbox_permissions": "all", "justification": "test" } }] })
    };
    let (code, s, err) = headless_run_with(
        json!([ask("touch one"), ask("rm -rf /tmp/two"), { "text": "done anyway" }]),
        &["--permission", "read-only", "go"],
        "",
    );
    assert_eq!(
        (code, s["reason"].as_str(), s["text"].as_str()),
        (Some(0), Some("completed"), Some("done anyway")),
        "{s}\n{err}"
    );
    assert!(s["error"].is_null(), "a completed turn has no error: {s}");
    assert!(err.contains("mare: 2 approvals were needed and nobody could answer (denied)"), "{err}");
    let lines: Vec<&str> = err.lines().filter(|l| l.starts_with("mare:   denied bash: ")).collect();
    assert_eq!(lines.len(), 2, "one line per denied call: {err}");
    assert!(lines[0].contains("touch one") && lines[1].contains("rm -rf /tmp/two"), "{err}");
    assert!(lines.iter().all(|l| l.ends_with(')')), "each line ends with its reason: {err}");
}

/// A denial nobody could answer followed by an abort that has nothing to do with approvals (a
/// `tool/before` policy abort): exit 1, not 3, and the denial is still listed on stderr.
#[test]
fn headless_unrelated_abort_after_a_denial_exits_1_and_still_lists_the_denial() {
    let ask = |cmd: &str| {
        json!({ "tool_calls": [{ "name": "bash", "arguments": {
            "command": cmd, "sandbox_permissions": "all", "justification": "test" } }] })
    };
    let init = r#"mare.policy.voter("tool/before", function(call)
  if call.arguments.command == "touch two" then return { decision = "deny", abort = true, reason = "policy stop" } end
end)
"#;
    let (code, s, err) = headless_run_with(
        json!([ask("touch one"), ask("touch two"), { "text": "never reached" }]),
        &["--permission", "read-only", "go"],
        init,
    );
    assert_eq!((code, s["exit_code"].as_i64(), s["reason"].as_str()), (Some(1), Some(1), Some("error")), "{s}\n{err}");
    assert_eq!(s["error"]["code"], "aborted", "no approval sentence outside exit 3: {s}");
    assert_eq!(s["error"]["message"], "aborted", "no approval sentence outside exit 3: {s}");
    assert!(err.contains("mare: 1 approval was needed and nobody could answer (denied)"), "{err}");
    assert!(err.contains("mare:   denied bash: touch one"), "{err}");
}

/// A turn stopped by an approval (an `abort` answer) after a denial nobody could answer: exit 3,
/// `settled.error` says why, and the denied call is on stderr.
#[test]
fn headless_run_aborted_by_an_approval_exits_3_with_the_error_and_the_denial_lines() {
    let ask = |cmd: &str| {
        json!({ "tool_calls": [{ "name": "bash", "arguments": {
            "command": cmd, "sandbox_permissions": "all", "justification": "test" } }] })
    };
    let init = r#"local n = 0
mare.policy.voter("tool/approval", function()
  n = n + 1
  if n == 1 then return { decision = "deny", unavailable = true, reason = "nobody here" } end
  return { decision = "deny", abort = true, reason = "stop the turn" }
end)
"#;
    let (code, s, err) = headless_run_with(
        json!([ask("touch one"), ask("touch two"), { "text": "never reached" }]),
        &["--permission", "read-only", "go"],
        init,
    );
    assert_eq!(
        (code, s["exit_code"].as_i64(), s["reason"].as_str()),
        (Some(3), Some(3), Some("approval_unavailable")),
        "{s}\n{err}"
    );
    assert_eq!(s["error"]["code"], "unavailable", "{s}");
    assert_eq!(s["error"]["message"], "an approval was needed and nobody could answer (denied)", "{s}");
    assert!(err.contains("mare: 1 approval was needed and nobody could answer (denied)"), "{err}");
    assert!(err.contains("mare:   denied bash: touch one (policy)"), "{err}");
    assert!(err.contains("mare: an approval was needed and nobody could answer (denied)"), "{err}");
}

#[test]
fn headless_never_is_a_deterministic_refusal_with_normal_completion() {
    let (code, settled) = headless_run(
        json!([
            {"tool_calls":[{"name":"write","arguments":{"path":"blocked.txt","content":"blocked"}}]},
            {"text":"reported refusal"}
        ]),
        &["--permission", "read-only", "--approval-policy", "never", "go"],
    );
    assert_eq!(code, Some(0), "{settled}");
    assert_eq!(settled["reason"], "completed", "{settled}");
}

#[test]
fn headless_invalid_builtin_event_reports_failure() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("bad_event.lua"),
        r#"return {name="bad_event", inject = { "bus" }, apply=function(ctx)
        ctx:on("request/prepare", function() ctx.bus:emit("turn/started", {turn_id="bad-turn", text="missing trigger"}) end)
    end}"#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "finished" }]).to_string()).unwrap();
    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(!out.status.success());
    let lines: Vec<Value> =
        String::from_utf8(out.stdout).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines[0]["type"], "session");
    assert!(lines.iter().any(|line| line["type"] == "turn.failed"));
    assert_eq!(lines.last().unwrap()["type"], "settled");
    assert_ne!(lines.last().unwrap()["data"]["exit_code"], 0);
}

#[test]
fn headless_plugin_events_cannot_impersonate_lifecycle_records() {
    for name in ["session", "turn.failed", "settled", "lagged", "dropped"] {
        let home = tempfile::tempdir().unwrap();
        let plugins = home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("collision.lua"),
            format!(
                r#"return {{
                    name = "collision",
                    inject = {{ "bus" }},
                    contracts = {{
                        collision = {{
                            methods = {{}},
                            events = {{
                                event = {{
                                    doc = "test public event",
                                    mode = "emit",
                                    expose = "both",
                                    delivery = "required",
                                    ignorable = false,
                                    data = {{ type = "object" }},
                                    example = {{ value = "kept" }},
                                }},
                            }},
                        }},
                    }},
                    apply = function(ctx)
                        ctx:on("request/prepare", function()
                            ctx.bus:emit("collision/event", {{ value = "kept" }})
                            ctx.bus:emit("{name}", {{ value = "impostor" }})
                        end)
                    end
                }}"#
            ),
        )
        .unwrap();
        let script = home.path().join("script.json");
        std::fs::write(&script, json!([{ "text": "finished" }]).to_string()).unwrap();
        let out = mare_cmd()
            .args(["--json", "--model", "faux/echo", "--no-session", "go"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let lines: Vec<Value> =
            String::from_utf8(out.stdout).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(lines[0]["type"], "session", "{name}: {lines:?}");
        assert!(lines.iter().any(|line| line["type"] == "collision/event" && line["data"]["value"] == "kept"));
        assert!(!lines.iter().any(|line| line["data"]["value"] == "impostor"));
        assert_eq!(lines.iter().filter(|line| line["type"] == "session").count(), 1);
        assert_eq!(lines.iter().filter(|line| line["type"] == "settled").count(), 1);
        assert_eq!(lines.last().unwrap()["type"], "settled");
        assert!(!out.status.success(), "{name}");
        assert_eq!(lines.iter().filter(|line| line["type"] == "turn.failed").count(), 1);
        assert_ne!(lines.last().unwrap()["data"]["exit_code"], 0);
    }
}

/// `--timeout` with active streaming events must still emit both `turn.failed` and `settled`.
#[test]
fn headless_timeout_during_streaming_emits_turn_failed_and_settled() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]).to_string()).unwrap();
    let out = mare_cmd()
        .args(["--json", "--stream-deltas", "--model", "faux/echo", "--no-session", "--timeout", "300ms", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(out.status.code(), Some(4));
    let lines: Vec<Value> =
        String::from_utf8(out.stdout).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.iter().filter(|line| line["type"] == "turn.failed").count(), 1, "{lines:?}");
    assert_eq!(lines.iter().filter(|line| line["type"] == "settled").count(), 1, "{lines:?}");
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "settled", "{lines:?}");
    assert_eq!(last["data"]["reason"], "timeout", "{last}");
    assert_eq!(last["data"]["exit_code"], 4, "{last}");
}

/// On exit, headless prints `Resume this session with: mare -r <id>` on stderr only when a
/// session was persisted — never on stdout (to keep `-p` text and `--json` JSONL clean) and
/// never under `--no-session`.
/// Reference: `reference implementation/apps/reference implementation/src/cli/prompt-render.ts:390-410` and
/// `reference implementation/apps/reference implementation/test/cli/run-prompt.test.ts:18-37`.
#[test]
fn headless_exit_prints_resume_hint_on_stderr_only_when_persisted() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "hello back" }]).to_string()).unwrap();
    let script_resume = home.path().join("faux-resume.json");
    std::fs::write(&script_resume, json!([{ "text": "resumed ok" }]).to_string()).unwrap();

    // 1) Plain `-p` with persisted session: hint on stderr, stdout stays pure response text.
    let out = mare_cmd()
        .args(["--model", "faux/echo", "-p", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(stdout.trim(), "hello back", "stdout must only carry response text: {stdout}");
    let hint_line = stderr
        .lines()
        .find(|l| l.starts_with("Resume this session with: mare -r "))
        .unwrap_or_else(|| panic!("expected resume hint on stderr, got:\n{stderr}"));
    let session_id = hint_line.strip_prefix("Resume this session with: mare -r ").unwrap().trim();
    assert!(!session_id.is_empty(), "empty session id in hint: {hint_line}");

    // The hinted `mare -r <id>` actually resumes the persisted session.
    let resumed = mare_cmd()
        .args(["--model", "faux/echo", "-r", session_id, "-p", "again"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_resume)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    assert_eq!(String::from_utf8_lossy(&resumed.stdout).trim(), "resumed ok");

    // 2) `--no-session` in plain `-p`: no resume hint on stderr.
    let no_sess = mare_cmd()
        .args(["--model", "faux/echo", "--no-session", "-p", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(no_sess.status.success(), "{}", String::from_utf8_lossy(&no_sess.stderr));
    let no_sess_err = String::from_utf8_lossy(&no_sess.stderr);
    assert!(!no_sess_err.contains("Resume this session with:"), "unexpected hint under --no-session:\n{no_sess_err}");

    // 3) `--json` with persisted session: stdout stays valid JSONL, stderr carries the matching hint.
    let json_out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "-p", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(json_out.status.success(), "{}", String::from_utf8_lossy(&json_out.stderr));
    let events: Vec<Value> = String::from_utf8(json_out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("broken JSONL ({e}): {l}")))
        .collect();
    let json_id = events[0]["data"]["session_id"].as_str().expect("session.session_id");
    let json_err = String::from_utf8(json_out.stderr).unwrap();
    assert!(
        json_err.lines().any(|l| l == format!("Resume this session with: mare -r {json_id}")),
        "expected matching resume hint on stderr in --json mode, got:\n{json_err}"
    );

    // 4) `--json --no-session`: no resume hint on stderr.
    let json_no_sess = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "-p", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(json_no_sess.status.success(), "{}", String::from_utf8_lossy(&json_no_sess.stderr));
    let json_no_sess_err = String::from_utf8_lossy(&json_no_sess.stderr);
    assert!(
        !json_no_sess_err.contains("Resume this session with:"),
        "unexpected hint under --json --no-session:\n{json_no_sess_err}"
    );
}

#[test]
fn headless_contract_handles_piped_stdin_with_continue_json_errors_and_mode_conflicts() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "first" }, { "text": "continued" }]).to_string()).unwrap();

    // 1) Create a persisted session, then pipe a prompt into `-p -c`.
    let first = mare_cmd()
        .args(["-p", "--model", "faux/echo", "seed"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));

    let script2 = home.path().join("script2.json");
    std::fs::write(&script2, json!([{ "text": "continued" }]).to_string()).unwrap();
    let mut cont_child = mare_cmd()
        .args(["-p", "-c", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script2)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        cont_child.stdin.take().unwrap().write_all(b"piped follow up").unwrap();
    }
    let cont = cont_child.wait_with_output().unwrap();
    assert!(cont.status.success(), "{}", String::from_utf8_lossy(&cont.stderr));
    assert_eq!(String::from_utf8_lossy(&cont.stdout).trim(), "continued");

    // 2) Pre-run `-C /nonexistent --json` emits settled JSONL line with exit code 2.
    let missing_dir = home.path().join("no-such-dir");
    let cwd_err = mare_cmd()
        .args(["-C", missing_dir.to_str().unwrap(), "--json", "hi"])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(cwd_err.status.code(), Some(2));
    let cwd_lines: Vec<Value> = String::from_utf8(cwd_err.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("expected JSONL: {e}: {l}")))
        .collect();
    assert_eq!(cwd_lines.last().unwrap()["type"], "settled");
    assert_eq!(cwd_lines.last().unwrap()["data"]["exit_code"], 2);

    // 3) Conflicting mode combinations exit with code 2.
    let conflict1 =
        mare_cmd().args(["-p", "--mode", "rpc", "hi"]).env("MARE_HOME", home.path()).stdin(Stdio::null()).run_output();
    assert_eq!(conflict1.status.code(), Some(2));
    let conflict2 = mare_cmd()
        .args(["--mode", "text", "--web", "hi"])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(conflict2.status.code(), Some(2));

    // 4) Lua `print(...)` and `io.write(...)` in `--json` mode must NOT pollute protocol stdout.
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("noisy.lua"),
        r#"return { name = "noisy", apply = function() print("NOISY_LUA_PRINT"); io.write("NOISY_IO_WRITE\n") end }"#,
    )
    .unwrap();
    let noisy = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(noisy.status.success(), "{}", String::from_utf8_lossy(&noisy.stderr));
    let noisy_stdout = String::from_utf8(noisy.stdout).unwrap();
    assert!(!noisy_stdout.contains("NOISY_LUA_PRINT"), "stdout polluted: {noisy_stdout}");
    assert!(!noisy_stdout.contains("NOISY_IO_WRITE"), "stdout polluted: {noisy_stdout}");
    for line in noisy_stdout.lines() {
        serde_json::from_str::<Value>(line).unwrap_or_else(|e| panic!("invalid JSONL ({e}): {line}"));
    }
    let noisy_stderr = String::from_utf8(noisy.stderr).unwrap();
    assert!(noisy_stderr.contains("NOISY_LUA_PRINT") && noisy_stderr.contains("NOISY_IO_WRITE"));
}

#[test]
fn headless_turn_failed_and_settled_emit_structured_event_errors() {
    let parse_lines = |stdout: &[u8]| -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL ({e}): {l}")))
            .collect()
    };

    // 1) Usage error -> starts with session/protocol_version, ends with settled, and carries
    // structured { code: "invalid_arguments", message } on both turn.failed and settled.
    let out = mare_cmd().args(["--json", "--timeout", "invalid", "go"]).stdin(Stdio::null()).run_output();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8(out.stdout).expect("usage error stdout must be valid UTF-8");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL ({e}): {l}")))
        .collect();
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["data"]["protocol_version"], mare_protocol::PROTOCOL_VERSION);
    let tf = lines.iter().find(|l| l["type"] == "turn.failed").expect("turn.failed line");
    let settled = lines.last().expect("settled line");
    assert_eq!(settled["type"], "settled");
    let tf_parsed: mare_protocol::headless::TurnFailed =
        serde_json::from_value(tf["data"].clone()).expect("turn.failed must match TurnFailed schema");
    let settled_parsed: mare_protocol::headless::Settled =
        serde_json::from_value(settled["data"].clone()).expect("settled must match Settled schema");
    assert_eq!(tf_parsed.error.code, "invalid_arguments");
    assert!(!tf_parsed.error.message.is_empty());
    assert_eq!(settled_parsed.error, Some(tf_parsed.error));

    // 2) Timeout -> structured { code: "timed_out", message }.
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("slow.json");
    std::fs::write(&script, json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]).to_string()).unwrap();
    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "--timeout", "250ms", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(out.status.code(), Some(4));
    let lines = parse_lines(&out.stdout);
    let tf = lines.iter().find(|l| l["type"] == "turn.failed").expect("timeout turn.failed");
    let settled = lines.last().expect("timeout settled");
    let tf_parsed: mare_protocol::headless::TurnFailed = serde_json::from_value(tf["data"].clone()).unwrap();
    let settled_parsed: mare_protocol::headless::Settled = serde_json::from_value(settled["data"].clone()).unwrap();
    assert_eq!(tf_parsed.error.code, "timed_out");
    assert_eq!(settled_parsed.error, Some(tf_parsed.error));
}

/// `--max-turns 0` stops before the first turn: exit code 4, terminal state `max_turns`, and
/// zero model requests sent.
#[test]
fn headless_max_turns_zero_exits_4_without_sending_any_request() {
    let home = tempfile::tempdir().unwrap();
    let req_log = home.path().join("requests.log");
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("req_counter.lua"),
        format!(
            r#"return {{
                name = "req_counter",
                inject = {{ "agent" }},
                apply = function(ctx)
                    ctx:on("request/build", function(_, next)
                        local f = assert(io.open({:?}, "a"))
                        f:write("req\n")
                        f:close()
                        return next()
                    end)
                end,
            }}"#,
            req_log.to_string_lossy()
        ),
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "should never be requested" }]).to_string()).unwrap();

    let out = mare_cmd()
        .args(["-p", "--json", "--model", "faux/echo", "--no-session", "--max-turns", "0", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "expected exit 4 for --max-turns 0\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(!req_log.exists(), "provider must receive zero requests under --max-turns 0");

    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL ({e}): {l}")))
        .collect();
    assert!(!events.iter().any(|e| e["type"] == "step/started"), "no step may start under --max-turns 0: {events:?}");
    let settled = events.last().expect("settled record");
    assert_eq!(settled["type"], "settled", "{events:?}");
    assert_eq!(settled["data"]["exit_code"], 4, "{settled}");
    assert_eq!(settled["data"]["reason"], "max_turns", "{settled}");
    assert_eq!(settled["data"]["termination_reason"], "turn_limit", "{settled}");
}

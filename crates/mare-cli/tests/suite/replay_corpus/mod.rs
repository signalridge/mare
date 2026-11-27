//! Reference: three-layer replay corpus for real provider sessions.
//!
//! 1. Keyless replay (always runs): replays recorded `opencode-go` sessions (`deepseek-v4.1-flash`
//!    and `gpt-5.6-luna` on the Responses wire; `glm-5.3-flash` and `claude-haiku-5-5` on the
//!    Anthropic wire) through `mare -p --json` against `MockServer`, splitting SSE bytes mid-UTF-8
//!    character to exercise cross-byte-boundary Chinese text, thinking blocks, long tool outputs,
//!    and session save + `--resume` recovery (including verifying prior history & thinking enter
//!    the resumed provider request).
//!    Fixtures are HTTP cassettes (`common/cassette.rs`, opencode cassette v1) under
//!    `tests/fixtures/cassettes/mare-record/`; `metadata.phases` names the session phase
//!    (`turn_1`, `compact`, `turn_2`) of each recorded interaction.
//! 2. Re-record: see `just record` (the cassette is written by the recorder, then replayed here).
//! 3. Live check (`suite::live::replay_corpus_live_smoke_skips_without_key`, `#[ignore]`d): runs only via `just live`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::common::cassette::Cassette;
#[cfg(debug_assertions)]
use crate::common::cassette::MatchMode;
use crate::common::*;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};

mod harness;
mod runner;

use harness::*;
use runner::*;

#[tokio::test]
async fn replay_corpus_opencode_go_deepseek_v4_1_flash() {
    local(run_replay_case(
        "opencode-go-deepseek-v4.1-flash.json",
        "deepseek-v4.1-flash",
        "responses",
        "/openai/v1",
        true,
    ))
    .await;
}

#[tokio::test]
async fn replay_corpus_opencode_go_glm_5_3_flash() {
    local(run_replay_case("opencode-go-glm-5.3-flash.json", "glm-5.3-flash", "anthropic", "/anthropic", false)).await;
}

#[tokio::test]
async fn replay_corpus_opencode_go_claude_haiku_5_5() {
    local(run_replay_case("opencode-go-claude-haiku-5-5.json", "claude-haiku-5-5", "anthropic", "/anthropic", true))
        .await;
}

#[tokio::test]
async fn replay_corpus_opencode_go_gpt_5_6_luna() {
    local(run_replay_case("opencode-go-gpt-5.6-luna.json", "gpt-5.6-luna", "responses", "/openai/v1", true)).await;
}

fn route_prefix(route: &str) -> &'static str {
    match route {
        "anthropic" => "/anthropic",
        "responses" | "openai-completions" => "/openai/v1",
        _ => panic!("unexpected route"),
    }
}

/// Records the three phases the replay runs (turn 1 with a tool call, compaction, resume + turn 2)
/// against `upstream` into `cassette`, each `mare` child recording through the HTTP recorder
/// (`MARE_RECORD_CASSETTE`, mare-binding `http_record.rs`). The cassette is started with the
/// metadata the replay reads (`model`, `route`) before the first interaction is appended.
async fn record_corpus(upstream: &str, key: &str, model: &str, route: &str, cassette: &Path) {
    let name = cassette.file_stem().unwrap().to_string_lossy().into_owned();
    let head = serde_json::json!({
        "version": 1,
        "metadata": { "name": name, "source": "mare-record", "provider": "opencode-go", "model": model, "route": route },
        "interactions": [],
    });
    std::fs::write(cassette, format!("{head:#}\n")).unwrap();
    let mut env = TestEnv::new().await;
    prepare_workspace(&env.cwd);
    write_opencode_config(&env, upstream, route, true);
    env.set_var("MARE_TEST_ALLOW_LIVE", "1");
    env.set_var("MARE_RECORD_CASSETTE", cassette);
    let model_ref = format!("opencode-go/{model}");
    let (home, cwd, key) = (env.home.path().to_path_buf(), env.cwd.clone(), key.to_string());

    env.set_var("MARE_RECORD_PHASE", "turn_1");
    let prompt_turn1 =
        "请使用 read 工具读取相对路径 corpus.txt（不要传绝对路径），简短用中文总结其中包含的中文测试句和最后一行行号。";
    let (events, _) =
        run_headless_turn_async(home.clone(), cwd.clone(), model_ref.clone(), prompt_turn1.into(), None, key.clone())
            .await;
    let sid = events
        .iter()
        .find(|e| e["type"] == "session")
        .and_then(|e| e["data"]["session_id"].as_str())
        .unwrap_or_else(|| panic!("turn 1 started no session"))
        .to_string();
    let sess_path = find_session_file(&home, &sid).expect("saved session");

    env.set_var("MARE_RECORD_PHASE", "compact");
    run_rpc_compact_async(home.clone(), cwd.clone(), model_ref.clone(), sess_path.clone(), key.clone()).await;

    env.set_var("MARE_RECORD_PHASE", "turn_2");
    run_headless_turn_async(home, cwd, model_ref, "请用中文简短确认会话已恢复。".into(), Some(sess_path), key).await;
}

/// Re-record a corpus from the live opencode Go endpoint (`just record [model] [route]`); the new
/// cassette replaces the fixture only after it replays here. The key comes from
/// `OPENCODE_API_KEY` in the environment; it is never printed,
/// and the recorder refuses to write a cassette that contains it.
#[tokio::test]
#[ignore = "live: run through `just record` (needs OPENCODE_API_KEY)"]
async fn record_opencode_go_corpus() {
    const CHILD: &str = "MARE_RECORD_CORPUS_CHILD";
    let child = std::env::var_os(CHILD).is_some();
    if child {
        // A directly invoked child must not print panic payloads from the recorder or runner.
        std::panic::set_hook(Box::new(|_| eprintln!("recording failed; diagnostic details suppressed")));
    }
    let key = std::env::var("OPENCODE_API_KEY").ok().filter(|k| !k.trim().is_empty()).expect("OPENCODE_API_KEY");
    if !child {
        let mut command = tokio::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "suite::replay_corpus::record_opencode_go_corpus", "--ignored", "--nocapture"])
            .env(CHILD, "1");
        run_recording_child(command).await.unwrap_or_else(|e| panic!("{e}"));
        eprintln!("recorded cassette");
        return;
    }
    let var = |k: &str, d: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
    let (model, route) = (var("MARE_RECORD_MODEL", "glm-5.3-flash"), var("MARE_RECORD_ROUTE", "anthropic"));
    let upstream = var("MARE_RECORD_UPSTREAM", "https://opencode.ai/inference/go");
    let name = format!("opencode-go-{model}.json");
    let out = tempfile::tempdir().unwrap();
    let cassette = out.path().join(&name);
    local(record_corpus(&upstream, &key, &model, &route, &cassette)).await;

    let bytes = std::fs::read(&cassette).expect("read recorded cassette");
    validate_recorded_cassette(&bytes).unwrap_or_else(|e| panic!("{e}"));

    // replace the fixture only once the new cassette replays
    local(run_replay_case_from_path(&cassette, &model, &route, route_prefix(&route), false)).await;
    let target = fixture_path(&name);
    publish_recorded_cassette(&cassette, &target).unwrap();
}

/// (reference implementation suite.spec.ts:177/244 runs record mode and checks the
/// output): the whole record → replay chain without a live provider. Recording runs against a mock
/// upstream that serves the glm-5.3-flash fixture; the recorded cassette carries the model, the
/// route and every request, and replays with request matching (what `just record` does before it
/// replaces a fixture).
///
/// Debug-only by design: release builds compile out `MARE_RECORD_CASSETTE` HTTP recording in
/// `mare-binding/src/http_record.rs` (`!cfg!(any(debug_assertions, test))`).
#[cfg(debug_assertions)]
#[tokio::test]
async fn a_recorded_corpus_replays_with_request_matching() {
    local(async {
        let (model, route) = ("glm-5.3-flash", "anthropic");
        let server = MockServer::start().await;
        let upstream = server.cassette(
            route_prefix(route),
            Cassette::load(&fixture_path("opencode-go-glm-5.3-flash.json")),
            MatchMode::ResponseOnly,
        );
        let out = tempfile::tempdir().unwrap();
        let cassette = out.path().join(format!("opencode-go-{model}.json"));
        record_corpus(&server.base, "mock-opencode-key-0000", model, route, &cassette).await;
        upstream.assert_done();

        validate_recorded_cassette(&std::fs::read(&cassette).unwrap()).unwrap();
        let recorded = Cassette::load(&cassette);
        assert_eq!(
            (recorded.metadata["model"].as_str(), recorded.metadata["route"].as_str()),
            (Some(model), Some(route))
        );
        assert_eq!(recorded.mode(), MatchMode::RequestAndResponse, "every recorded interaction has its request");
        run_replay_case_from_path(&cassette, model, route, route_prefix(route), false).await;
    })
    .await
}

#[tokio::test]
async fn replay_corpus_from_env_path() {
    let Ok(raw_path) = std::env::var("MARE_REPLAY_CORPUS_PATH") else {
        return;
    };
    let path = PathBuf::from(raw_path);
    let cassette = Cassette::load(&path);
    let model = cassette.metadata["model"].as_str().expect("metadata.model").to_string();
    let route = cassette.metadata["route"].as_str().expect("metadata.route").to_string();
    let route_prefix = route_prefix(&route);
    local(run_replay_case_from_path(&path, &model, &route, route_prefix, false)).await;
}

/// Regression for `SESSION-WRITE`: a prompt whose multi-byte Chinese character crosses byte 2000
/// (`changes.lua:80`) must persist cleanly to the session `.jsonl` without UTF-8 byte-slice corruption.
#[tokio::test]
async fn replay_corpus_multibyte_prompt_crossing_2000_bytes() {
    local(async {
        let cassette = Cassette::load(&fixture_path("opencode-go-glm-5.3-flash.json"));
        let server = MockServer::start().await;
        server.route("/anthropic", phase(&cassette, "compact"));

        let env = TestEnv::new().await;
        write_opencode_config(&env, &server.base, "anthropic", false);

        let prompt = format!("{}中 请简短确认。", "a".repeat(1999));
        let (events, stdout) = run_headless_turn_async(
            env.home.path().to_path_buf(),
            env.cwd.clone(),
            "opencode-go/glm-5.3-flash".to_string(),
            prompt.clone(),
            None,
            "replay-dummy-key".to_string(),
        )
        .await;
        assert!(!stdout.contains('\u{FFFD}'), "UTF-8 replacement char found: {stdout}");
        let settled = events.iter().rfind(|e| e["type"] == "settled").expect("settled event");
        assert_ne!(settled["data"]["reason"], "error", "{settled}");

        let sid = events
            .iter()
            .find(|e| e["type"] == "session")
            .and_then(|e| e["data"]["session_id"].as_str())
            .expect("session event");
        let sess_path = find_session_file(env.home.path(), sid).expect("saved session .jsonl file");
        let sess_text = std::fs::read_to_string(&sess_path).unwrap();
        let entries: Vec<Value> = sess_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid session JSONL line: {e}: {l}")))
            .collect();

        let changes_turn = entries
            .iter()
            .find(|e| e["type"] == "custom" && e["kind"] == "changes/turn")
            .expect("saved session must contain changes/turn entry");
        let turn_text = changes_turn["data"]["text"].as_str().expect("changes/turn data.text string");
        assert!(!turn_text.contains('\u{FFFD}'), "UTF-8 replacement char found in changes/turn text: {turn_text}");
        assert_eq!(
            turn_text,
            "a".repeat(1999),
            "changes/turn text must truncate cleanly at byte 1999 before '中' (changes.lua:80 -> mare.text.cut(..., 2000))"
        );

        let user_text = entries
            .iter()
            .find(|e| e["type"] == "message" && e["message"]["role"] == "user")
            .and_then(|e| e["message"]["content"].as_array())
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str())
            .expect("saved session must contain user message text");
        assert!(!user_text.contains('\u{FFFD}'), "UTF-8 replacement char found in user message text: {user_text}");
        assert_eq!(user_text, prompt, "user message text must preserve full multibyte prompt");
    })
    .await;
}

#[test]
fn replay_resume_history_check_rejects_omitted_tool_history() {
    let p1 = "read corpus.txt";
    let p2 = "confirm resumed";
    let reply = "summarized corpus";

    // Anthropic: missing tool_use or mismatched tool_use_id must fail; complete history passes.
    let anthropic_ok = serde_json::json!({
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": p1 }] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "t", "signature": "sig-1" },
                { "type": "tool_use", "id": "call_1", "name": "read", "input": { "path": "corpus.txt" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "中文跨字节边界测试", "is_error": false }
            ]},
            { "role": "assistant", "content": [{ "type": "text", "text": reply }] },
            { "role": "user", "content": [{ "type": "text", "text": p2 }] }
        ]
    });
    assert!(verify_resume_request_history("anthropic", &anthropic_ok, p1, p2, reply, true).is_ok());

    let anthropic_no_tool = serde_json::json!({
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": p1 }] },
            { "role": "assistant", "content": [{ "type": "text", "text": reply }] },
            { "role": "user", "content": [{ "type": "text", "text": p2 }] }
        ]
    });
    assert!(verify_resume_request_history("anthropic", &anthropic_no_tool, p1, p2, reply, false).is_err());

    let anthropic_mismatched_id = serde_json::json!({
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": p1 }] },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "call_1", "name": "read", "input": { "path": "corpus.txt" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_other", "content": "中文跨字节边界测试" }
            ]},
            { "role": "assistant", "content": [{ "type": "text", "text": reply }] },
            { "role": "user", "content": [{ "type": "text", "text": p2 }] }
        ]
    });
    assert!(verify_resume_request_history("anthropic", &anthropic_mismatched_id, p1, p2, reply, false).is_err());

    // Responses: missing function_call_output or reasoning must fail; complete history passes.
    let responses_ok = serde_json::json!({
        "input": [
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": p1 }] },
            { "type": "reasoning", "id": "rs_1" },
            { "type": "function_call", "call_id": "fc_1", "name": "read", "arguments": "{\"path\":\"corpus.txt\"}" },
            { "type": "function_call_output", "call_id": "fc_1", "output": "中文跨字节边界测试" },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": reply }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": p2 }] }
        ]
    });
    assert!(verify_resume_request_history("responses", &responses_ok, p1, p2, reply, true).is_ok());

    let responses_no_output = serde_json::json!({
        "input": [
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": p1 }] },
            { "type": "function_call", "call_id": "fc_1", "name": "read", "arguments": "{\"path\":\"corpus.txt\"}" },
            { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": reply }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": p2 }] }
        ]
    });
    assert!(verify_resume_request_history("responses", &responses_no_output, p1, p2, reply, false).is_err());

    // OpenAI completions: missing tool result must fail; complete history passes.
    let completions_ok = serde_json::json!({
        "messages": [
            { "role": "user", "content": p1 },
            { "role": "assistant", "tool_calls": [{ "id": "tc_1", "function": { "name": "read", "arguments": "{\"path\":\"corpus.txt\"}" } }] },
            { "role": "tool", "tool_call_id": "tc_1", "content": "中文跨字节边界测试" },
            { "role": "assistant", "content": reply },
            { "role": "user", "content": p2 }
        ]
    });
    assert!(verify_resume_request_history("openai-completions", &completions_ok, p1, p2, reply, false).is_ok());

    let completions_no_tool = serde_json::json!({
        "messages": [
            { "role": "user", "content": p1 },
            { "role": "assistant", "content": reply },
            { "role": "user", "content": p2 }
        ]
    });
    assert!(verify_resume_request_history("openai-completions", &completions_no_tool, p1, p2, reply, false).is_err());
}

#[tokio::test]
async fn replay_rpc_reply_deadline_bounds_hung_and_chatty_streams() {
    local(async {
        let (_writer, reader_half) = tokio::io::duplex(256);
        let mut reader = BufReader::new(reader_half);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let err = read_rpc_reply_with_deadline(&mut reader, 1, deadline).await.unwrap_err();
        assert!(err.contains("timed out"), "expected timeout error, got: {err}");

        let (mut writer, reader_half) = tokio::io::duplex(1024);
        let writer_task = tokio::spawn(async move {
            loop {
                if writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{}}\n").await.is_err() {
                    break;
                }
                // sleep-ok: paces an endless event trickle so the read must hit its deadline, not EOF
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut reader = BufReader::new(reader_half);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(120);
        let err = read_rpc_reply_with_deadline(&mut reader, 42, deadline).await.unwrap_err();
        assert!(err.contains("timed out"), "expected timeout error on chatty stream, got: {err}");
        writer_task.abort();
    })
    .await;
}

#[tokio::test]
async fn replay_response_preserves_http_status_and_content_type() {
    local(async {
        // a cassette's recorded status and content-type reach the wire as recorded; a missing
        // content-type defaults by status (200 → SSE, else JSON)
        let json = r#"{ "version": 1, "metadata": { "name": "status-probe" }, "interactions": [
            { "transport": "http", "response": { "status": 429, "headers": { "content-type": "application/json" },
              "body": "{\"error\":{\"message\":\"rate limited\"}}" } },
            { "transport": "http", "response": { "status": 503, "body": "service unavailable" } },
            { "transport": "http", "response": { "status": 200,
              "body": "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n" } } ] }"#;
        let probe: Cassette = serde_json::from_str(json).unwrap();
        let resps: Vec<Resp> = probe.interactions.iter().map(|it| it.response.to_resp()).collect();
        let ct = |r: &Resp| r.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.clone()).unwrap();
        assert_eq!((resps[0].status, ct(&resps[0])), (429, "application/json".to_string()));
        assert_eq!((resps[1].status, ct(&resps[1])), (503, "application/json".to_string()));
        assert_eq!((resps[2].status, ct(&resps[2])), (200, "text/event-stream".to_string()));

        // Verify MockServer + mare retry end-to-end with a 429 before 200 SSE.
        let fixture = Cassette::load(&fixture_path("opencode-go-glm-5.3-flash.json"));
        let mut mock_resps = vec![resps[0].clone()];
        mock_resps.extend(phase(&fixture, "compact"));

        let server = MockServer::start().await;
        server.route("/anthropic", mock_resps);
        let env = TestEnv::new().await;
        write_opencode_config(&env, &server.base, "anthropic", false);

        let (events, _) = run_headless_turn_async(
            env.home.path().to_path_buf(),
            env.cwd.clone(),
            "opencode-go/glm-5.3-flash".to_string(),
            "请简短确认。".to_string(),
            None,
            "replay-dummy-key".to_string(),
        )
        .await;
        let settled = events.iter().rfind(|e| e["type"] == "settled").expect("settled event");
        assert_ne!(settled["data"]["reason"], "error", "{settled}");
        assert!(server.count("/anthropic") >= 2, "expected 429 retry + 200 SSE");
    })
    .await;
}

#[tokio::test]
async fn replay_rpc_concurrent_stderr_drain_does_not_deadlock() {
    local(async {
        let mut std_cmd = Command::new("python3");
        std_cmd
            .arg("-c")
            .arg(
                "import sys\n\
                 sys.stderr.write('x' * 262144)\n\
                 sys.stderr.flush()\n\
                 line = sys.stdin.readline()\n\
                 sys.stdout.write('{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n')\n\
                 sys.stdout.flush()\n",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut cmd = tokio::process::Command::from(std_cmd);
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let stderr_task = spawn_pipe_drain(child.stderr.take().unwrap(), Some(STDERR_DRAIN_TAIL_BYTES));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n").await.unwrap();
        stdin.flush().await.unwrap();
        let reply = read_rpc_reply_with_deadline(&mut reader, 1, deadline).await.unwrap();
        assert_eq!(reply["result"]["ok"], true);
        drop(stdin);
        drop(reader);

        let status = tokio::time::timeout_at(deadline, child.wait()).await.unwrap().unwrap();
        assert!(status.success());
        let stderr_tail = stderr_task.await.unwrap();
        assert_eq!(stderr_tail.len(), STDERR_DRAIN_TAIL_BYTES);
    })
    .await;
}

#[cfg(unix)]
#[test]
fn replay_gnu_tool_shims_match_cassette_od_and_wc_format() {
    let dir = tempfile::tempdir().unwrap();
    prepare_workspace(dir.path());
    let bin = install_gnu_tool_shims(dir.path());
    let corpus = std::fs::read(dir.path().join("corpus.txt")).unwrap();
    let tail = &corpus[corpus.len() - 200..];
    let mut od = Command::new(bin.join("od")).arg("-c").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    use std::io::Write as _;
    od.stdin.take().unwrap().write_all(tail).unwrap();
    let od_out = od.wait_with_output().unwrap();
    assert!(od_out.status.success());
    let od_text = String::from_utf8(od_out.stdout).unwrap();
    assert_eq!(
        od_text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
        vec![
            "0000220   i   n   e       1   4   0   :     350 276 271 347 225 214 346",
            "0000240 225 260 346 215 256   _   a   l   p   h   a   _   b   e   t   a",
            "0000260   _   g   a   m   m   a   _   0   1   2   3   4   5   6   7   8",
            "0000300   9   a   b   c   d   e   f  \\n",
            "0000310",
        ]
    );
    if Path::new("/usr/bin/wc").exists() {
        let wc_out = Command::new(bin.join("wc")).args(["-l", "corpus.txt"]).current_dir(dir.path()).output().unwrap();
        assert!(wc_out.status.success());
        assert_eq!(String::from_utf8(wc_out.stdout).unwrap(), "141 corpus.txt\n");
    }
}

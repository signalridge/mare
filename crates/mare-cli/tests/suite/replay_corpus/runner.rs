use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::common::cassette::{Cassette, Chunk};
use crate::common::*;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::harness::*;

pub(super) const HEADLESS_TURN_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const RPC_COMPACT_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const STDERR_DRAIN_TAIL_BYTES: usize = 64 * 1024;

/// Ambient provider credential/endpoint variables stripped from replay child processes so local
/// shell credentials cannot perturb model readiness or `subagent.models` disclosure state.
const AMBIENT_PROVIDER_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GEMINI_BASE_URL",
    "DEEPSEEK_API_KEY",
    "KIMI_API_KEY",
    "MOONSHOT_API_KEY",
    "OPENROUTER_API_KEY",
    "OLLAMA_HOST",
    "LMSTUDIO_BASE_URL",
];

/// Ambient locale, shell, and POSIX/tool override variables stripped from replay child processes
/// before pinning `LC_ALL=C` so host locale collation (`C.UTF-8` rejecting `[一-龥]` in GNU grep),
/// `GREP_OPTIONS`, or `_POSIX2_VERSION` cannot perturb deterministic tool outputs.
const AMBIENT_SHELL_AND_LOCALE_ENV_VARS: &[&str] = &[
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LANGUAGE",
    "LANG",
    "GREP_OPTIONS",
    "GREP_COLORS",
    "GREP_COLOR",
    "_POSIX2_VERSION",
    "POSIXLY_CORRECT",
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
];

fn configure_replay_env(cmd: &mut std::process::Command, home: &Path) {
    for key in AMBIENT_PROVIDER_ENV_VARS.iter().chain(AMBIENT_SHELL_AND_LOCALE_ENV_VARS.iter()) {
        cmd.env_remove(key);
    }
    cmd.env("LC_ALL", "C").env("LANG", "C").env("PYTHONUTF8", "1");
    #[cfg(unix)]
    {
        let bin = install_gnu_tool_shims(home);
        let path = match std::env::var_os("PATH") {
            Some(existing) if !existing.is_empty() => format!("{}:{}", bin.display(), existing.to_string_lossy()),
            _ => bin.display().to_string(),
        };
        cmd.env("PATH", path);
        if Path::new("/bin/bash").is_file() {
            cmd.env("SHELL", "/bin/bash");
        }
    }
    #[cfg(not(unix))]
    let _ = home;
}

pub(super) fn spawn_pipe_drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut pipe: R,
    max_bytes: Option<usize>,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match pipe.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(cap) = max_bytes
                        && buf.len() > cap
                    {
                        let excess = buf.len() - cap;
                        buf.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    })
}

pub(super) async fn run_headless_turn_async(
    home: PathBuf,
    cwd: PathBuf,
    model_ref: String,
    prompt: String,
    resume: Option<PathBuf>,
    api_key: String,
) -> (Vec<Value>, String) {
    let mut std_cmd = mare_cmd();
    std_cmd.args(["-p", "--json", "--permission", "auto", "--model", &model_ref, "--effort", "high"]);
    if let Some(p) = resume.as_deref() {
        std_cmd.args(["--resume", &p.to_string_lossy()]);
    }
    configure_replay_env(&mut std_cmd, &home);
    std_cmd
        .arg(&prompt)
        .current_dir(&cwd)
        .env("MARE_HOME", &home)
        .env("OPENCODE_API_KEY", &api_key)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let stdout_task = spawn_pipe_drain(child.stdout.take().unwrap(), None);
    let stderr_task = spawn_pipe_drain(child.stderr.take().unwrap(), Some(STDERR_DRAIN_TAIL_BYTES));
    let status = tokio::time::timeout(HEADLESS_TURN_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("mare -p timed out after {HEADLESS_TURN_TIMEOUT:?}"))
        .unwrap();

    let stdout = stdout_task.await.unwrap();
    let _ = stderr_task.await.unwrap();
    assert!(
        status.success(),
        "mare -p failed (status={status}):\nstderr:\n{SUPPRESSED_CHILD_STDERR}\nstdout:\n{stdout}"
    );
    let events: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSON line ({e}): {l}")))
        .collect();
    (events, stdout)
}

pub(super) async fn read_rpc_reply_with_deadline<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    expected_id: i64,
    deadline: tokio::time::Instant,
) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = tokio::time::timeout_at(deadline, reader.read_line(&mut line))
            .await
            .map_err(|_| format!("timed out waiting for RPC reply id={expected_id}"))?
            .map_err(|e| format!("error reading RPC stdout waiting for id={expected_id}: {e}"))?;
        if n == 0 {
            return Err(format!("mare --rpc closed stdout waiting for id={expected_id}"));
        }
        if line.trim().is_empty() {
            continue;
        }
        let frame: Value = serde_json::from_str(&line).map_err(|e| format!("invalid RPC JSON line ({e}): {line}"))?;
        if frame.get("id").and_then(|v| v.as_i64()) == Some(expected_id) {
            return Ok(frame);
        }
    }
}

pub(super) async fn run_rpc_compact_async(
    home: PathBuf,
    cwd: PathBuf,
    model_ref: String,
    sess_path: PathBuf,
    api_key: String,
) {
    let mut std_cmd = mare_cmd();
    configure_replay_env(&mut std_cmd, &home);
    std_cmd
        .args([
            "--rpc",
            "--permission",
            "auto",
            "--model",
            &model_ref,
            "--effort",
            "high",
            "--resume",
            &sess_path.to_string_lossy(),
        ])
        .current_dir(&cwd)
        .env("MARE_HOME", &home)
        .env("OPENCODE_API_KEY", &api_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let stderr_task = spawn_pipe_drain(child.stderr.take().unwrap(), Some(STDERR_DRAIN_TAIL_BYTES));
    let deadline = tokio::time::Instant::now() + RPC_COMPACT_TIMEOUT;

    let run_result: Result<std::process::ExitStatus, String> = async {
        stdin
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocol_version\":\"3.0\"}}\n",
            )
            .await
            .map_err(|e| format!("failed writing initialize: {e}"))?;
        stdin.flush().await.map_err(|e| format!("failed flushing initialize: {e}"))?;
        let init_reply = read_rpc_reply_with_deadline(&mut reader, 1, deadline).await?;
        if init_reply.get("error").is_some() {
            return Err(format!("RPC initialize returned error: {init_reply}"));
        }

        let session_id = init_reply
            .pointer("/result/session/session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("RPC initialize missing session.session_id: {init_reply}"))?
            .to_string();

        let compact_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "compact.run",
            "params": {
                "session_id": session_id,
                "instructions": "Preserve the summary of corpus.txt."
            }
        });
        let mut line = serde_json::to_vec(&compact_req).unwrap();
        line.push(b'\n');
        stdin.write_all(&line).await.map_err(|e| format!("failed writing compact.run: {e}"))?;
        stdin.flush().await.map_err(|e| format!("failed flushing compact.run: {e}"))?;
        let compact_reply = read_rpc_reply_with_deadline(&mut reader, 2, deadline).await?;
        if compact_reply.get("error").is_some() {
            return Err(format!("RPC compact.run returned error: {compact_reply}"));
        }
        if compact_reply.pointer("/result/status").and_then(|v| v.as_str()) != Some("compacted") {
            return Err(format!("RPC compact.run did not succeed: {compact_reply}"));
        }

        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"shutdown\",\"params\":{}}\n")
            .await
            .map_err(|e| format!("failed writing shutdown: {e}"))?;
        stdin.flush().await.map_err(|e| format!("failed flushing shutdown: {e}"))?;
        let _ = read_rpc_reply_with_deadline(&mut reader, 3, deadline).await?;
        drop(stdin);
        drop(reader);

        tokio::time::timeout_at(deadline, child.wait())
            .await
            .map_err(|_| format!("mare --rpc compact wait timed out after {RPC_COMPACT_TIMEOUT:?}"))?
            .map_err(|e| format!("failed waiting on mare --rpc compact: {e}"))
    }
    .await;

    let status = match run_result {
        Ok(status) => {
            let _ = stderr_task.await.unwrap_or_else(|e| panic!("failed joining stderr drain task: {e}"));
            status
        }
        Err(err) => {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
            panic!("{err}\nstderr:\n{SUPPRESSED_CHILD_STDERR}");
        }
    };
    assert!(status.success(), "mare --rpc compact failed (status={status}):\nstderr:\n{SUPPRESSED_CHILD_STDERR}");
}

pub(super) async fn run_replay_case(
    fixture_name: &str,
    model: &str,
    route: &str,
    route_prefix: &str,
    expect_thinking_on_resume: bool,
) {
    let path = fixture_path(fixture_name);
    run_replay_case_from_path(&path, model, route, route_prefix, expect_thinking_on_resume).await;
}

pub(super) async fn run_replay_case_from_path(
    path: &Path,
    model: &str,
    route: &str,
    route_prefix: &str,
    expect_thinking_on_resume: bool,
) {
    let env = TestEnv::new().await;
    prepare_workspace(&env.cwd);

    let prompt_turn1 =
        "请使用 read 工具读取相对路径 corpus.txt（不要传绝对路径），简短用中文总结其中包含的中文测试句和最后一行行号。";
    let prompt_turn2 = "请用中文简短确认会话已恢复。";

    let cassette = Cassette::load(path);
    assert_eq!(cassette.metadata["model"], model, "cassette {}", cassette.name());
    let phases: Vec<String> = cassette.metadata["phases"]
        .as_array()
        .expect("metadata.phases")
        .iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    let has_compact = phases.iter().any(|p| p == "compact");
    assert!(
        phases.iter().any(|p| p == "turn_1") && phases.iter().any(|p| p == "turn_2"),
        "cassette {}: phases {phases:?}",
        cassette.name()
    );

    let mut cassette = cassette;
    // Split the final text/thinking response of turn 1 mid-UTF-8 character to verify
    // cross-byte boundary streaming in mare's SSE parser.
    let last_turn_1 = phases.iter().rposition(|p| *p == "turn_1").unwrap();
    let resp = &mut cassette.interactions[last_turn_1].response;
    if resp.status == 200 && resp.chunks.is_none() {
        resp.chunks = Some(split_chunks(resp.bytes(), &mid_utf8_split_offset(&resp.body)));
    }
    let server = MockServer::start().await;
    let mode = cassette.mode();
    let replay = server.cassette(route_prefix, cassette, mode);
    write_opencode_config(&env, &server.base, route, has_compact);

    let model_ref = format!("opencode-go/{model}");
    let (events1, stdout1) = run_headless_turn_async(
        env.home.path().to_path_buf(),
        env.cwd.clone(),
        model_ref.clone(),
        prompt_turn1.to_string(),
        None,
        "replay-dummy-key".to_string(),
    )
    .await;
    assert!(!stdout1.contains('\u{FFFD}'), "UTF-8 replacement char found in turn 1 output: {stdout1}");

    let settled1 =
        events1.iter().rfind(|e| e["type"] == "settled").unwrap_or_else(|| panic!("missing settled event: {stdout1}"));
    assert_ne!(settled1["data"]["reason"], "error", "turn 1 errored: {settled1}");

    let text1 = settled1["data"]["text"].as_str().expect("settled text");
    assert!(!text1.is_empty() && !text1.is_ascii(), "expected Chinese text in turn 1 output: {text1}");

    // Verify tool execution happened (turn 1 had >1 provider request: tool call -> tool result -> final)
    assert!(
        server.count(route_prefix) >= 2,
        "expected tool call round-trip (>=2 requests), got {}",
        server.count(route_prefix)
    );
    let reqs = server.requests();
    assert!(
        reqs.iter().any(|r| r.body.contains("中文跨字节边界测试")),
        "expected long Chinese tool output from corpus.txt to be sent back to provider"
    );

    // Verify session was saved on disk with thinking + tool_call and can be resumed
    let sid = events1
        .iter()
        .find(|e| e["type"] == "session")
        .and_then(|e| e["data"]["session_id"].as_str())
        .expect("session event")
        .to_string();
    let sess_path = find_session_file(env.home.path(), &sid).expect("saved session .jsonl file");
    let sess_text = std::fs::read_to_string(&sess_path).unwrap();
    assert!(sess_text.contains("corpus.txt"), "saved session must persist the turn history");
    let sess_entries: Vec<Value> = sess_text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let has_tool_call = sess_entries.iter().any(|e| {
        e["message"]["role"] == "assistant"
            && e["message"]["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_call" && b["name"] == "read"))
    });
    assert!(has_tool_call, "expected read tool_call in saved session: {sess_text}");

    let turn1_req_count = server.count(route_prefix);
    let mut compact_summary = None;
    if has_compact {
        run_rpc_compact_async(
            env.home.path().to_path_buf(),
            env.cwd.clone(),
            model_ref.clone(),
            sess_path.clone(),
            "replay-dummy-key".to_string(),
        )
        .await;
        let after_compact_count = server.count(route_prefix);
        assert!(
            after_compact_count > turn1_req_count,
            "expected compaction to consume recorded response (before={turn1_req_count}, after={after_compact_count})"
        );
        let post_compact_sess = std::fs::read_to_string(&sess_path).unwrap();
        let compact_entry = post_compact_sess
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|e| e["type"] == "compaction")
            .unwrap_or_else(|| panic!("expected compaction entry in saved session: {post_compact_sess}"));
        compact_summary = compact_entry["summary"].as_str().map(str::to_string);
        assert!(
            compact_summary.as_deref().is_some_and(|s| !s.is_empty()),
            "expected non-empty compaction summary in saved session: {compact_entry}"
        );
    }
    let before_resume_req_count = server.count(route_prefix);

    let (events2, stdout2) = run_headless_turn_async(
        env.home.path().to_path_buf(),
        env.cwd.clone(),
        model_ref,
        prompt_turn2.to_string(),
        Some(sess_path.clone()),
        "replay-dummy-key".to_string(),
    )
    .await;
    assert!(!stdout2.contains('\u{FFFD}'), "UTF-8 replacement char found in resumed turn: {stdout2}");
    let settled2 = events2
        .iter()
        .rfind(|e| e["type"] == "settled")
        .unwrap_or_else(|| panic!("missing settled event on resume: {stdout2}"));
    assert_ne!(settled2["data"]["reason"], "error", "resumed turn errored: {settled2}");
    let text2 = settled2["data"]["text"].as_str().expect("resumed settled text");
    assert!(!text2.is_empty(), "expected non-empty resumed text: {settled2}");

    // Verify that --resume actually sent prior conversation history (either uncompacted turn 1
    // history or the compaction summary) + turn 2 prompt into the model request.
    let all_reqs = server.requests();
    let resume_reqs: Vec<_> =
        all_reqs.iter().filter(|r| r.path.starts_with(route_prefix)).skip(before_resume_req_count).collect();
    assert!(!resume_reqs.is_empty(), "expected at least one provider request on --resume");
    let resume_body_raw = &resume_reqs[0].body;
    let resume_body: Value = serde_json::from_str(resume_body_raw)
        .unwrap_or_else(|e| panic!("invalid JSON in resumed request ({e}): {resume_body_raw}"));
    if let Some(summary) = compact_summary.as_deref() {
        let body_text = value_text(&resume_body);
        assert!(body_text.contains(prompt_turn2), "resumed request missing turn 2 prompt: {resume_body_raw}");
        assert!(
            body_text.contains(summary),
            "resumed request missing compaction summary ({summary:?}): {resume_body_raw}"
        );
    } else {
        verify_resume_request_history(
            route,
            &resume_body,
            prompt_turn1,
            prompt_turn2,
            text1,
            expect_thinking_on_resume,
        )
        .unwrap_or_else(|err| panic!("resume request history check failed ({route}): {err}\n{resume_body_raw}"));
    }

    let full_sess_text = std::fs::read_to_string(&sess_path).unwrap();
    let full_entries: Vec<Value> = full_sess_text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let has_thinking = full_entries.iter().any(|e| {
        e["message"]["role"] == "assistant"
            && e["message"]["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "reasoning"))
    });
    assert!(has_thinking, "expected thinking block in saved session across turns: {full_sess_text}");
    replay.assert_done();
}

/// The body cut at byte offsets (possibly inside a UTF-8 character), as recorded chunks.
pub(super) fn split_chunks(bytes: Vec<u8>, at: &[usize]) -> Vec<Chunk> {
    use base64::Engine as _;
    let mut out = Vec::new();
    let mut from = 0;
    for &to in at.iter().chain(std::iter::once(&bytes.len())) {
        let to = to.clamp(from, bytes.len());
        out.push(Chunk {
            t_ms: 0,
            text: None,
            b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes[from..to])),
        });
        from = to;
    }
    out
}

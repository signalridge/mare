//! Live provider tests against a real model: two codexradar tasks (candy and pelican) on
//! opencode Go (`glm-5.3-flash` by default, falling back to `deepseek-v4.1-flash` when it is
//! unavailable; the reasoning task candy uses the same default, with `MARE_LIVE_CANDY_MODEL` /
//! `MARE_LIVE_CANDY_EFFORT` overrides). `MARE_LIVE_NO_FALLBACK` disables the fallback for a
//! glm-only run. Manual only — `just live` passes `MARE_LIVE_OPENCODE_KEY` from the environment
//! and runs the ignored tests; without a key they skip. The key is passed to the child as
//! `OPENCODE_API_KEY` and never printed, logged or written to a file here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::Value;

const KEY_ENV: &str = "MARE_LIVE_OPENCODE_KEY";
const DEFAULT_MODEL: &str = "glm-5.3-flash";
/// candy (reasoning) runs on the default model too; `MARE_LIVE_CANDY_MODEL` /
/// `MARE_LIVE_CANDY_EFFORT` override. When glm is unavailable candy falls back to DeepSeek at
/// effort `high` (max is more than it needs).
const CANDY_MODEL: &str = DEFAULT_MODEL;
const CANDY_FALLBACK_EFFORT: &str = "high";
/// When glm-5.3-flash is unavailable the live tests fall back to this model.
const FALLBACK_MODEL: &str = "deepseek-v4.1-flash";
/// Child-process timeout for live model runs (`[profile.live]` in `.config/nextest.toml` allows 16m).
const LIVE_CMD_TIMEOUT: Duration = Duration::from_secs(600);

/// The live configuration, or None (skip) without a key.
struct Live {
    key: String,
    model: String,
    /// `--effort` for the run (None: the configured default)
    effort: Option<String>,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|v| !v.trim().is_empty())
}

fn live_config(key: Option<String>, model: Option<String>) -> Option<Live> {
    let key = non_empty(key)?;
    let model = non_empty(model).unwrap_or_else(|| DEFAULT_MODEL.to_string());
    Some(Live { key, model, effort: None })
}

fn candy_config(key: Option<String>, model: Option<String>, effort: Option<String>) -> Option<Live> {
    let mut live = live_config(key, Some(non_empty(model).unwrap_or_else(|| CANDY_MODEL.to_string())))?;
    live.effort = non_empty(effort);
    Some(live)
}

fn resolve_live_key_with(env_key: Option<String>, fallback_env_key: Option<String>) -> Option<String> {
    non_empty(env_key).or_else(|| non_empty(fallback_env_key))
}

fn runtime_live_key() -> Option<String> {
    resolve_live_key_with(std::env::var(KEY_ENV).ok(), std::env::var("OPENCODE_API_KEY").ok())
}

fn require_live_key(task: &str, key: Option<String>) -> String {
    non_empty(key).unwrap_or_else(|| {
        panic!("LIVE {task}: missing API key (set {KEY_ENV} or OPENCODE_API_KEY; refusing to silently pass)")
    })
}

fn from_env() -> Option<Live> {
    live_config(runtime_live_key(), std::env::var("MARE_LIVE_MODEL").ok())
}

fn candy_from_env() -> Option<Live> {
    let var = |k: &str| std::env::var(k).ok();
    candy_config(runtime_live_key(), var("MARE_LIVE_CANDY_MODEL"), var("MARE_LIVE_CANDY_EFFORT"))
}

/// The reasoning effort as the Responses wire spells it (provider-openai EFFORT_MAP).
fn wire_effort(effort: &str) -> &str {
    if effort == "off" { "none" } else { effort }
}

fn prompt(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/live/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// What one headless run produced.
struct Run {
    lines: Vec<Value>,
    exit: i32,
    elapsed: Duration,
    /// one `x-opencode-session` value per model request (recorded by a `provider/request` hook)
    session_headers: Vec<String>,
    /// the reasoning effort in each request body (`reasoning.effort`, or `reasoning_effort` on a
    /// chat wire; empty when absent), from the same hook
    efforts: Vec<String>,
    /// stderr (key redacted)
    stderr: String,
    workspace: tempfile::TempDir,
    _env: TestEnv,
    /// the child's raw stdout, kept with the run when it fails
    stdout: String,
}

impl Run {
    /// Stop the run's MARE_HOME and workspace from being deleted when it drops, write the child's
    /// stdout and stderr next to the session files, and return a note naming both directories.
    fn keep_for_inspection(&mut self) -> String {
        let _ = std::fs::write(self._env.home.path().join("live-stdout.jsonl"), &self.stdout);
        let _ = std::fs::write(self._env.home.path().join("live-stderr.txt"), &self.stderr);
        self._env.preserve_home();
        self.workspace.disable_cleanup(true);
        format!(
            "kept for inspection: MARE_HOME={} workspace={} (live-stdout.jsonl and live-stderr.txt are in MARE_HOME)",
            self._env.home.path().display(),
            self.workspace.path().display()
        )
    }
    fn of(&self, kind: &str) -> Vec<&Value> {
        self.lines.iter().filter(|l| l["type"] == kind).collect()
    }
    fn settled(&self) -> &Value {
        &self.of("settled").last().expect("no settled line (stream never finished)")["data"]
    }
    /// The session file: the path from `session` or `status/changed` when already known, else the
    /// `.jsonl` under this run's MARE_HOME whose header has the session id (created lazily).
    fn session_path(&self) -> Option<PathBuf> {
        resolve_session_path(&self.lines, self._env.home.path())
    }
    /// usage totals from the settled line: (input, output, reasoning)
    fn tokens(&self) -> (i64, i64, i64) {
        let u = &self.settled()["usage"];
        let n = |k: &str| u[k].as_i64().unwrap_or(0);
        (n("input"), n("output"), n("reasoning"))
    }
    fn report(&self, task: &str, pass: bool) {
        let (i, o, r) = self.tokens();
        // the default model is billed against the Go subscription: show what each run cost
        let cost = self.settled()["usage"]["cost"].as_f64().unwrap_or(0.0);
        eprintln!(
            "LIVE {task}: {} in {:.1}s, tokens in={i} out={o} reasoning={r}, requests={}, usage.cost=${cost:.4}, effort={:?}",
            if pass { "PASS" } else { "FAIL" },
            self.elapsed.as_secs_f64(),
            self.session_headers.len(),
            self.efforts.first().map(String::as_str).unwrap_or_default()
        );
    }
}

/// Run `mare -p --json` on a task in a fresh workspace with a fresh MARE_HOME.
async fn run(live: &Live, task: &str) -> Run {
    let env = TestEnv::new().await;
    let workspace = tempfile::tempdir().unwrap();
    let headers_log = env.home.path().join("session-headers.log");
    std::fs::create_dir_all(env.config_dir()).unwrap();
    // record the session header and the reasoning effort of every model request (never the
    // authorization header): "<session>\t<effort>" per line
    std::fs::write(
        env.config_dir().join("init.lua"),
        format!(
            r#"-- a waterfall: a native listener passes the wire on with next
mare.on("provider/request", function(w, next)
  local b = type(w.body) == "table" and w.body or {{}}
  local effort = type(b.reasoning) == "table" and b.reasoning.effort or b.reasoning_effort or ""
  local f = io.open({log:?}, "a")
  f:write(tostring(w.headers and w.headers["x-opencode-session"] or ""), "\t", tostring(effort), "\n")
  f:close()
  return next(w)
end)
"#,
            log = headers_log.display().to_string()
        ),
    )
    .unwrap();
    let t0 = Instant::now();
    let mut cmd = mare_cmd();
    // Registration seam for the `live` group (opencode Go): explicitly opt out of the default offline test guard.
    cmd.env("MARE_TEST_ALLOW_LIVE", "1");
    cmd.args(["-p", "--json", "--permission", "auto", "--model"]).arg(format!("opencode-go/{}", live.model));
    if let Some(e) = &live.effort {
        cmd.args(["--effort", e]);
    }
    let out = cmd
        .arg(prompt(task))
        .current_dir(workspace.path())
        .env("MARE_HOME", env.home.path())
        .env("OPENCODE_API_KEY", &live.key)
        .env_remove(KEY_ENV)
        .stdin(std::process::Stdio::null())
        .run_output_timeout(LIVE_CMD_TIMEOUT);
    let elapsed = t0.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<Value> = stdout.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    // stderr may carry provider errors; it never contains the key (mare doesn't echo it)
    let stderr = redact(&String::from_utf8_lossy(&out.stderr), &live.key);
    if !out.status.success() {
        eprintln!("LIVE {task} stderr:\n{stderr}");
    }
    let (session_headers, efforts) = std::fs::read_to_string(&headers_log)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let (s, e) = l.split_once('\t').unwrap_or((l, ""));
            (s.to_string(), e.to_string())
        })
        .unzip();
    Run {
        lines,
        exit: out.status.code().unwrap_or(-1),
        elapsed,
        session_headers,
        efforts,
        stderr,
        workspace,
        _env: env,
        stdout: stdout.into_owned(),
    }
}

/// Run a live test's checks against `run`. Every live assertion goes through here: when one
/// fails, the run's MARE_HOME and workspace are kept (a passing run is deleted as before) and the
/// panic message names them, so a failed live run can be inspected instead of vanishing.
fn checked(mut run: Run, task: &str, checks: impl FnOnce(&Run)) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| checks(&run)));
    if let Err(payload) = outcome {
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "(non-string panic)".into());
        panic!("{msg}\nLIVE {task}: {}", run.keep_for_inspection());
    }
}

/// The provider said the model can't be used (gone, unknown, not served on this plan).
fn model_unavailable(diag: &str) -> bool {
    let d = diag.to_ascii_lowercase();
    d.contains("model_not_found")
        || d.contains("modelnotfound")
        || (d.contains("model")
            && ["unavailable", "not available", "not found", "not supported", "does not exist", "no such model"]
                .iter()
                .any(|p| d.contains(p)))
}

fn fallback_allowed(model: &str, failed: bool, disabled: bool, diag: &str) -> bool {
    !disabled && model == DEFAULT_MODEL && failed && model_unavailable(diag)
}

/// `run`, but when the default model (glm-5.3-flash) turns out to be unavailable the task is
/// run again on `FALLBACK_MODEL`, said on stderr; any other failure is returned as is.
async fn run_task(live: &Live, task: &str) -> Run {
    let first = run(live, task).await;
    let failed = first.exit != 0 || !first.of("turn.failed").is_empty();
    let mut diag = first.stderr.clone();
    for l in first.lines.iter().filter(|l| l["type"] == "turn.failed" || l["type"] == "settled") {
        diag.push_str(&l.to_string());
    }
    if !fallback_allowed(&live.model, failed, std::env::var_os("MARE_LIVE_NO_FALLBACK").is_some(), &diag) {
        return first;
    }
    eprintln!("LIVE {task}: {DEFAULT_MODEL} is unavailable, falling back to {FALLBACK_MODEL}");
    let effort = live.effort.clone().or_else(|| (task == "candy.txt").then(|| CANDY_FALLBACK_EFFORT.to_string()));
    let fallback = Live { key: live.key.clone(), model: FALLBACK_MODEL.to_string(), effort };
    run(&fallback, task).await
}

fn redact(s: &str, key: &str) -> String {
    if key.is_empty() { s.to_string() } else { s.replace(key, "[redacted]") }
}

/// Checks shared by both tasks: the session header on every request and constant; the stream
/// reached its terminal event; usage was recorded.
fn assert_common(run: &Run, task: &str) {
    assert!(!run.session_headers.is_empty(), "{task}: no model request was recorded");
    let first = &run.session_headers[0];
    assert!(!first.is_empty(), "{task}: x-opencode-session missing");
    assert!(run.session_headers.iter().all(|h| h == first), "{task}: x-opencode-session changed within the session");
    assert!(run.of("turn.failed").is_empty(), "{task}: turn failed: {:?}", run.of("turn.failed"));
    let settled = run.settled();
    assert!(settled["error"].is_null(), "{task}: {settled}");
    // `input` excludes cached tokens and may be 0 on a cache hit; output can't be
    let (_, o, _) = run.tokens();
    assert!(o > 0, "{task}: usage not recorded: {}", settled["usage"]);
}

/// Every tool call in the session log has a matching tool result.
fn assert_tool_calls_paired(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let (mut calls, mut results) = (Vec::new(), std::collections::HashSet::new());
    for line in text.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let m = &e["message"];
        match m["role"].as_str() {
            Some("assistant") => {
                for b in m["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_call" {
                        calls.push(b["id"].as_str().unwrap_or_default().to_string());
                    }
                }
            }
            Some("tool_result") => {
                let id = m["tool_call_id"].as_str().unwrap_or_default();
                results.insert(id.to_string());
            }
            _ => {}
        }
    }
    let missing: Vec<_> = calls.iter().filter(|c| !results.contains(*c)).collect();
    assert!(missing.is_empty(), "tool calls without results: {missing:?}");
}

/// The newest .html file anywhere in the workspace.
fn find_html(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("html")) {
                return Some(p);
            }
        }
    }
    None
}

#[tokio::test]
#[ignore = "real model: run with `just live`"]
async fn candy_reasoning_task_answers_15() {
    let live = candy_from_env().unwrap_or_else(|| {
        let _ = require_live_key("candy", None);
        unreachable!()
    });
    eprintln!("LIVE candy: opencode-go/{} effort {}", live.model, live.effort.as_deref().unwrap_or_default());
    let run = run_task(&live, "candy.txt").await;
    checked(run, "candy", |run| {
        let text = run.settled()["text"].as_str().unwrap_or_default().to_string();
        // the final answer: 15 is stated, and no other number is given as the answer (bold or "答案")
        let says_15 = text.split(|c: char| !c.is_ascii_digit()).any(|n| n == "15");
        // the answer is the model's ability, not mare's: reported as PASS/FAIL with time and cost,
        // never failing the run; mare's own behaviour stays asserted
        run.report("candy", run.exit == 0 && says_15);
        assert_common(run, "candy");
        assert_eq!(run.exit, 0);
        // every request (the turn and any auxiliary one) carried the chosen effort when one was
        // asked for (`MARE_LIVE_CANDY_EFFORT`); checked for the default model, whose declared
        // levels include it (another model may clamp it)
        if let (CANDY_MODEL, Some(effort)) = (live.model.as_str(), live.effort.as_deref()) {
            let want = wire_effort(effort);
            assert!(run.efforts.iter().all(|e| e == want), "candy: effort sent {:?}, want {want:?}", run.efforts);
        }
        if !says_15 {
            eprintln!("LIVE candy: the model's final answer is not 15 (reported, not asserted)");
        }
    });
}

#[tokio::test]
#[ignore = "real model: run with `just live`"]
async fn pelican_tool_task_writes_an_animated_svg_page() {
    let live = from_env().unwrap_or_else(|| {
        let _ = require_live_key("pelican", None);
        unreachable!()
    });
    let run = run_task(&live, "pelican.txt").await;
    checked(run, "pelican", |run| {
        let html = find_html(run.workspace.path());
        let body = html.as_ref().map(|p| std::fs::read_to_string(p).unwrap_or_default()).unwrap_or_default();
        let has_svg = body.contains("<svg");
        let animated = ["<animate", "@keyframes", "requestAnimationFrame"].iter().any(|a| body.contains(a));
        let pass = run.exit == 0 && html.is_some() && has_svg && animated;
        run.report("pelican", pass);
        assert_common(run, "pelican");
        assert_eq!(run.exit, 0);
        assert!(html.is_some(), "pelican: no .html file in the workspace");
        assert!(has_svg && animated, "pelican: {} lacks <svg or an animation", html.unwrap().display());
        assert_tool_calls_paired(&run.session_path().expect("no session file recorded"));
    });
}

/// Live provider smoke test, explicit `#[ignore]` (`just live` only).
#[test]
#[ignore = "real model: run with `just live`"]
fn replay_corpus_live_smoke_requires_key() {
    let key = require_live_key("replay smoke", runtime_live_key());
    let key = key.trim();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = mare_cmd()
        // the live group reaches the real provider: opt out of the offline guard explicitly
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .args(["-p", "--json", "--permission", "auto", "--model", "opencode-go/glm-5.3-flash", "只回复：ok"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .env("OPENCODE_API_KEY", key)
        .stdin(std::process::Stdio::null())
        .run_output_timeout(LIVE_CMD_TIMEOUT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = redact(&String::from_utf8_lossy(&out.stderr), key);
    assert!(out.status.success(), "mare -p failed (status={}):\nstderr:\n{stderr}\nstdout:\n{stdout}", out.status);
    let settled = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .rfind(|e| e["type"] == "settled")
        .unwrap_or_else(|| panic!("missing settled event: {stdout}"));
    assert_ne!(settled["data"]["reason"], "error", "{settled}");
}

/// Without a key the live tests skip instead of failing.
#[test]
fn live_tests_skip_without_a_key() {
    assert!(live_config(None, None).is_none());
    assert!(live_config(Some("  ".into()), Some("m".into())).is_none());
    let l = live_config(Some("k".into()), None).unwrap();
    assert_eq!(l.model, DEFAULT_MODEL);
    assert_eq!(live_config(Some("k".into()), Some("other".into())).unwrap().model, "other");
    assert_eq!(redact("Bearer sk-123 x", "sk-123"), "Bearer [redacted] x");
    // candy: the default model with the configured effort, both overridable
    assert!(candy_config(None, None, None).is_none());
    let c = candy_config(Some("k".into()), None, Some(" ".into())).unwrap();
    assert_eq!((c.model.as_str(), c.effort.as_deref()), (DEFAULT_MODEL, None));
    let c = candy_config(Some("k".into()), Some("deepseek-v4.1-flash".into()), Some("high".into())).unwrap();
    assert_eq!((c.model.as_str(), c.effort.as_deref()), ("deepseek-v4.1-flash", Some("high")));
    let c = candy_config(Some("k".into()), Some("glm-5.3-flash".into()), Some("max".into())).unwrap();
    assert_eq!((c.model.as_str(), c.effort.as_deref()), ("glm-5.3-flash", Some("max")));
    assert_eq!(l.effort, None);
    assert_eq!((wire_effort("high"), wire_effort("max"), wire_effort("off")), ("high", "max", "none"));
    // the fallback to deepseek-v4.1-flash triggers on "model unavailable", not on other errors
    assert!(model_unavailable(r#"{"error":{"type":"ModelError","message":"Model glm-5.3-flash is unavailable"}}"#));
    assert!(model_unavailable("400: model_not_found"));
    assert!(model_unavailable("The model `glm-5.3-flash` does not exist"));
    assert!(!model_unavailable("401 Unauthorized: invalid api key"));
    assert!(!model_unavailable("rate limited, retry after 3s"));
    assert!(fallback_allowed(DEFAULT_MODEL, true, false, "model_not_found"));
    assert!(!fallback_allowed(DEFAULT_MODEL, true, true, "model_not_found"), "glm-only runs never fall back");
    assert!(!fallback_allowed(DEFAULT_MODEL, false, false, "model_not_found"));
    assert!(!fallback_allowed("other", true, false, "model_not_found"));
    // Key resolution order (MARE_LIVE_OPENCODE_KEY > OPENCODE_API_KEY)
    assert_eq!(
        resolve_live_key_with(Some("from-live".into()), Some("from-opencode".into())).as_deref(),
        Some("from-live")
    );
    assert_eq!(
        resolve_live_key_with(Some("  ".into()), Some("from-opencode".into())).as_deref(),
        Some("from-opencode")
    );
    assert_eq!(resolve_live_key_with(None, Some("".into())), None);
    assert_eq!(resolve_live_key_with(None, None), None);
}

#[test]
#[should_panic(expected = "refusing to silently pass")]
fn live_tests_fail_loudly_without_a_key() {
    let _ = require_live_key("candy", None);
}

/// A failing live check keeps the run's MARE_HOME and workspace and names them in the panic; a
/// passing one leaves nothing behind (no real model: a hand-made `Run`).
#[tokio::test]
async fn a_failed_live_check_keeps_the_run_directories_and_a_passing_one_cleans_up() {
    let fake = || async {
        let env = TestEnv::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let paths = (env.home.path().to_path_buf(), workspace.path().to_path_buf());
        let run = Run {
            lines: vec![],
            exit: 1,
            elapsed: Duration::ZERO,
            session_headers: vec![],
            efforts: vec![],
            stderr: "boom".into(),
            workspace,
            _env: env,
            stdout: "{}".into(),
        };
        (run, paths)
    };
    let (run, (home, work)) = fake().await;
    checked(run, "ok", |_| {});
    assert!(!home.exists() && !work.exists(), "a passing run is deleted");
    let (run, (home, work)) = fake().await;
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| checked(run, "bad", |r| assert_eq!(r.exit, 0))))
        .expect_err("the failing check panics");
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains(&home.display().to_string()) && msg.contains(&work.display().to_string()), "{msg}");
    assert!(home.join("live-stderr.txt").exists() && work.exists(), "kept: {msg}");
    std::fs::remove_dir_all(&home).unwrap();
    std::fs::remove_dir_all(&work).unwrap();
}

fn resolve_session_path(lines: &[Value], home: &Path) -> Option<PathBuf> {
    let rec = lines.iter().find(|l| l["type"] == "session").map(|l| &l["data"]);
    if let Some(p) = rec.and_then(|r| r["path"].as_str()) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) =
        lines.iter().filter(|l| l["type"] == "status/changed").find_map(|l| l["data"]["session"]["path"].as_str())
    {
        return Some(PathBuf::from(p));
    }
    let id = rec.and_then(|r| r["session_id"].as_str().or_else(|| r["id"].as_str()))?;
    let mut stack = vec![home.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| t.lines().next().map(str::to_string))
                    .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                    .is_some_and(|h| h["type"] == "session" && h["id"] == id)
            {
                return Some(p);
            }
        }
    }
    None
}

/// Offline check that the restored live prompt fixtures and HTML discovery helper work.
#[test]
fn live_fixture_prompts_and_html_discovery_work() {
    let candy = prompt("candy.txt");
    assert!(candy.contains("糖果") && candy.contains("苹果味"), "unexpected candy.txt: {candy:?}");
    let pelican = prompt("pelican.txt");
    assert!(pelican.contains("鹈鹕") && pelican.contains("SVG"), "unexpected pelican.txt: {pelican:?}");
    let source = prompt("SOURCE.md");
    assert!(source.contains("candy.txt") && source.contains("pelican.txt"), "unexpected SOURCE.md: {source:?}");
    let dir = tempfile::tempdir().unwrap();
    assert!(find_html(dir.path()).is_none());
    let nested = dir.path().join("out");
    std::fs::create_dir_all(&nested).unwrap();
    let page = nested.join("pelican.html");
    std::fs::write(&page, "<svg></svg>").unwrap();
    assert_eq!(find_html(dir.path()), Some(page));
}

#[test]
fn live_session_path_reads_v3_session_id_and_status_path() {
    use serde_json::json;
    let home = tempfile::tempdir().unwrap();
    let sess_dir = home.path().join("state/sessions/proj");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let sess_file = sess_dir.join("2026-11-26_abc.jsonl");
    std::fs::write(&sess_file, r#"{"type":"session","id":"sess-v3-id"}"#).unwrap();

    // Protocol v3.0 `session` line uses `session_id` without `path` or `id`.
    let lines_id_only = vec![json!({
        "type": "session",
        "data": { "protocol_version": "3.0", "session_id": "sess-v3-id" }
    })];
    assert_eq!(resolve_session_path(&lines_id_only, home.path()), Some(sess_file.clone()));

    // `status/changed` carries `data.session.path` directly.
    let lines_with_status = vec![
        json!({ "type": "session", "data": { "protocol_version": "3.0", "session_id": "other" } }),
        json!({ "type": "status/changed", "data": { "session": { "path": sess_file } } }),
    ];
    assert_eq!(resolve_session_path(&lines_with_status, home.path()), Some(sess_file));
}

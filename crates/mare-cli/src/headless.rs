//! Headless mode: `mare -p "prompt"` prints the final text; `--json` streams the public event
//! contract (`mare_protocol::headless`): a `session` record first, the public agent events one
//! per line, `turn.failed` on failure and `settled` last.
//! Internal plugin events are not forwarded; streaming deltas only with `--stream-deltas`.

use std::sync::Arc;

use anyhow::Result;
use mare_protocol::headless;
use serde_json::{Value, json};

use mare_host::Host;

/// Set once a write to stdout failed (the reader went away, e.g. `mare --json … | head`): the
/// run stops instead of finishing the turn (and spending tokens) for nobody. Rust ignores
/// SIGPIPE, so a closed pipe shows up as EPIPE here; `println!` would panic on it instead.
static STDOUT_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn stdout_closed() -> bool {
    STDOUT_CLOSED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Runtime-owned records are serialized/flushed on the dedicated bounded writer.
async fn write_out(output: &crate::headless_output::Output, line: crate::headless_output::Line) {
    if stdout_closed() {
        return;
    }
    if output.line(line).await.is_err() {
        STDOUT_CLOSED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn write_line(output: &crate::headless_output::Output, kind: &str, data: Value) {
    write_out(output, crate::headless_output::Line::Json(json!({ "type": kind, "data": data }))).await;
}

// Argument parsing can run before any runtime exists. These few usage records have no
// running producer or --timeout; keep that synchronous entry point separate.
fn usage_line(kind: &str, data: Value) {
    let line = json!({ "type": kind, "data": data }).to_string();
    let _ = crate::write_protocol_stdout_line(&line);
}

fn schedule_abort(host: Arc<Host>) -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        let _ = host.abort().await;
        let _ = tx.send(());
    });
    rx
}

/// Unix: has the reader of stdout gone away? (`poll` reports POLLERR / POLLHUP on the write end
/// of a pipe whose read end is closed.) Text mode writes only at the end, so it asks this.
#[cfg(unix)]
fn stdout_reader_gone() -> bool {
    let mut p = libc::pollfd { fd: 1, events: 0, revents: 0 };
    // SAFETY: one valid pollfd, zero timeout
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLERR | libc::POLLHUP) != 0
}
#[cfg(not(unix))]
fn stdout_reader_gone() -> bool {
    false
}

/// Exit code when stdout closed under us (128 + SIGPIPE, what a shell reports for `… | head`).
pub const STDOUT_CLOSED_CODE: i32 = 141;

/// The `session` record: id and file of the session the run writes (what `mare -r` takes).
async fn session_record(host: &Host) -> headless::SessionRecord {
    let mut rec = headless::SessionRecord { cwd: host.cwd().to_string_lossy().into_owned(), ..Default::default() };
    if let Some(s) = host.service_table("session")
        && let Ok(info) = s.get::<mlua::Function>("info")
        && let Ok(mlua::Value::Table(t)) = info.call_async::<mlua::Value>(s).await
    {
        rec.session_id = t.get::<Option<String>>("session_id").ok().flatten();
        rec.path = t.get::<Option<String>>("path").ok().flatten();
        rec.ephemeral = t.get::<Option<bool>>("ephemeral").ok().flatten().unwrap_or(false);
    }
    rec.model = host.state().await.get("model").and_then(|m| m.as_str()).map(str::to_string);
    rec
}

/// How a headless run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Error,
    ApprovalUnavailable,
    MaxTurns,
    Timeout,
    SaveFailed,
    Interrupted,
}

impl Outcome {
    pub fn code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Error => 1,
            Self::ApprovalUnavailable => 3,
            Self::MaxTurns | Self::Timeout => 4,
            Self::SaveFailed => 5,
            Self::Interrupted => 130,
        }
    }
    pub fn reason(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::ApprovalUnavailable => "approval_unavailable",
            Self::MaxTurns => "max_turns",
            Self::Timeout => "timeout",
            Self::SaveFailed => "save_failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// An error as one clear line for stderr and the `settled` / `turn.failed` records: a Lua error
/// that crossed the plugin boundary carries mlua's `runtime error: ` prefix and a stack traceback
/// (a dozen lines of plugin internals for an unreachable proxy); both are dropped, the message
/// itself is kept.
pub fn clean_error(e: &str) -> String {
    let msg = match e.find("stack traceback:") {
        Some(i) => &e[..i],
        None => e,
    };
    let mut msg = msg.trim();
    while let Some(rest) = msg.strip_prefix("runtime error: ") {
        msg = rest.trim_start();
    }
    msg.to_string()
}

fn event_error(
    code: impl Into<String>,
    message: impl Into<String>,
    data: Option<Value>,
) -> mare_protocol::events::EventError {
    mare_protocol::events::EventError { code: code.into(), message: message.into(), data }
}

fn parse_agent_error(v: &Value) -> Option<mare_protocol::events::EventError> {
    let err = v.get("error")?;
    if err.is_null() {
        return None;
    }
    if let Some(obj) = err.as_object() {
        let message = obj
            .get("message")
            .and_then(Value::as_str)
            .map(clean_error)
            .unwrap_or_else(|| clean_error(&err.to_string()));
        let code = obj.get("code").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or("internal").to_string();
        let data = obj.get("data").filter(|d| !d.is_null()).cloned();
        return Some(event_error(code, message, data));
    }
    err.as_str().map(|s| event_error("internal", clean_error(s), None))
}

/// Settled line for a run that never started (usage error or `--timeout` while reading stdin).
fn pre_run_error(json: bool, exit_code: i32, reason: &str, error_code: &str, msg: &str) -> i32 {
    if json {
        let record = headless::SessionRecord {
            cwd: std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned(),
            ..Default::default()
        };
        usage_line("session", serde_json::to_value(record).unwrap_or_default());
        let err = event_error(error_code, msg, None);
        let s = headless::Settled { error: Some(err.clone()), exit_code, reason: reason.into(), ..Default::default() };
        usage_line("turn.failed", serde_json::to_value(headless::TurnFailed { error: err }).unwrap_or_default());
        usage_line("settled", serde_json::to_value(&s).unwrap_or_default());
    }
    eprintln!("mare: {msg}");
    exit_code
}

/// Settled line for a run that never started (bad arguments or configuration): exit code 2.
pub fn usage_error(json: bool, msg: &str) -> i32 {
    pre_run_error(json, 2, "usage", "invalid_arguments", msg)
}

/// Settled line when `--timeout` expires before a host starts (e.g. waiting on piped stdin): exit code 4.
pub fn timeout_error(json: bool, msg: &str) -> i32 {
    pre_run_error(json, Outcome::Timeout.code(), Outcome::Timeout.reason(), "timed_out", msg)
}

/// `--timeout` values: `90`, `90s`, `5m`, `1h`, `1500ms`.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, mul_ms) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (s, 1000)
    };
    let n: f64 = num.trim().parse().map_err(|_| format!("invalid duration {s:?} (e.g. 90s, 5m, 1h)"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("invalid duration {s:?}"));
    }
    Ok(std::time::Duration::from_millis((n * mul_ms as f64) as u64))
}

/// Why `-c` / `--resume` found no session (`info().resume_error` and `info().resume_error_code`).
pub(crate) async fn resume_error_details(host: &Host) -> Option<(String, Option<String>)> {
    let s = host.service_table("session")?;
    let info = s.get::<mlua::Function>("info").ok()?;
    match info.call_async::<mlua::Value>(s).await {
        Ok(mlua::Value::Table(t)) => {
            let msg = t.get::<Option<String>>("resume_error").ok().flatten()?;
            let code = t.get::<Option<String>>("resume_error_code").ok().flatten();
            Some((msg, code))
        }
        _ => None,
    }
}

/// Why `-c` / `--resume` found no session (the session plugin's `info().resume_error`).
pub(crate) async fn resume_error(host: &Host) -> Option<String> {
    resume_error_details(host).await.map(|(msg, _)| msg)
}

/// Whether a public event goes to the `--json` stream (`--stream-deltas` includes ignorable
/// events from the protocol or contract event registry; plain `--json` keeps only non-ignorable
/// records).
pub(crate) fn json_event(log: &mare_kernel::EventLog, name: &str, stream_deltas: bool) -> bool {
    log.is_external_event(name) && (stream_deltas || !log.event_ignorable(name))
}

/// These selected records define the transcript/result projection; delivery policy comes from the
/// protocol or contract event registry.
fn required_event(log: &mare_kernel::EventLog, name: &str) -> bool {
    log.event_delivery(name) == Some(mare_protocol::event_registry::EventDelivery::Required)
}

/// The deadline covers output acknowledgement and final draining as well as agent work.
/// A blocked reader cannot freeze Tokio or defeat --timeout. If it never resumes, the stream
/// ends explicitly incomplete (stderr/nonzero exit), rather than inventing a settled record.
pub async fn run(
    host: Arc<Host>,
    prompt: String,
    json: bool,
    stream_deltas: bool,
    timeout: Option<std::time::Duration>,
    output_schema: Option<serde_json::Value>,
) -> Result<i32> {
    let output = crate::headless_output::Output::new(mare_shared::wire_projection::hook(host.lua()))?;
    let run = run_inner(host.clone(), prompt, json, stream_deltas, output_schema, &output);
    if let Some(timeout) = timeout {
        match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => {
                crate::signals::begin_forceable_shutdown(Outcome::Timeout.code());
                output.stderr(
                    "mare: --timeout reached — cancelling the turn; output may be incomplete if the reader is blocked",
                );
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), host.abort()).await;
                crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::HostShutdown);
                host.shutdown().await;
                crate::signals::set_shutdown_stage(crate::signals::ShutdownStage::RuntimeDrain);
                if json {
                    let err = event_error("timed_out", "timeout; event delivery interrupted", None);
                    write_line(
                        &output,
                        "turn.failed",
                        serde_json::to_value(headless::TurnFailed { error: err.clone() })?,
                    )
                    .await;
                    write_line(
                        &output,
                        "settled",
                        serde_json::to_value(headless::Settled {
                            error: Some(err),
                            exit_code: 4,
                            reason: "timeout".into(),
                            ..Default::default()
                        })?,
                    )
                    .await;
                }
                Ok(4)
            }
        }
    } else {
        run.await
    }
}

struct HeadlessRunState {
    json: bool,
    approval_unavailable: bool,
    approval_abort: bool,
    denials: Vec<String>,
    // Only public model-authored calls supply display text. Never retain execution rewrites.
    requested_calls: std::collections::HashMap<String, serde_json::Value>,
    /// the run's cost so far (the usage plugin's running total): `settled.usage.cost`, like Claude
    /// Code's `total_cost_usd` — the agent result's usage has tokens only
    run_cost: Option<serde_json::Value>,
}

impl HeadlessRunState {
    fn new(json: bool) -> Self {
        Self {
            json,
            approval_unavailable: false,
            approval_abort: false,
            denials: Vec::new(),
            requested_calls: std::collections::HashMap::new(),
            run_cost: None,
        }
    }

    async fn handle_record(
        &mut self,
        output: &crate::headless_output::Output,
        record: mare_kernel::HeadlessEvent,
    ) -> Result<()> {
        let selected = record.selected;
        let ev = match record.record {
            mare_kernel::HeadlessRecord::Lagged { from_seq, to_seq } => {
                if self.json {
                    write_line(
                        output,
                        "lagged",
                        serde_json::to_value(headless::Lagged {
                            from_seq,
                            to_seq,
                            session_id: None,
                            session_epoch: None,
                        })?,
                    )
                    .await;
                }
                return Ok(());
            }
            mare_kernel::HeadlessRecord::Dropped { count, session_id, session_epoch } => {
                if self.json {
                    write_line(
                        output,
                        "dropped",
                        serde_json::to_value(headless::Dropped {
                            count,
                            session_id,
                            session_epoch: Some(session_epoch),
                        })?,
                    )
                    .await;
                }
                return Ok(());
            }
            mare_kernel::HeadlessRecord::Event(event) => event,
        };
        if ev.name == "usage/changed"
            && let Some(c) = ev.payload.get("cost")
        {
            self.run_cost = Some(c.clone());
        }
        if ev.name == "item/completed"
            && let Some(item) = ev.payload.get("item")
        {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("assistant_message") => {
                    if let Some(blocks) = item.pointer("/message/content").and_then(|c| c.as_array()) {
                        for call in blocks
                            .iter()
                            .filter(|block| block["type"] == "tool_call" && block["hosted"].as_bool() != Some(true))
                        {
                            if let Some(id) = call["id"].as_str() {
                                self.requested_calls.insert(id.into(), call.clone());
                            }
                        }
                    }
                }
                Some("tool_call") => {
                    if let Some(id) = item.get("tool_call_id").and_then(|v| v.as_str()) {
                        self.requested_calls.remove(id);
                    }
                }
                _ => {}
            }
        }
        if ev.name == "approval/resolved" {
            let result = ev.payload.get("result").unwrap_or(&ev.payload);
            let call_id = ev
                .payload
                .get("tool_call_id")
                .and_then(|id| id.as_str())
                .or_else(|| result.pointer("/call/tool_call_id").and_then(|id| id.as_str()));
            let requested = call_id.and_then(|id| self.requested_calls.remove(id));
            match result.get("decision").and_then(|d| d.as_str()) {
                Some("unavailable") => {
                    self.approval_unavailable = true;
                    self.denials.push(denial_line(result, requested.as_ref()));
                }
                Some("abort") => self.approval_abort = true,
                _ => {}
            }
        }
        if self.json && selected {
            if matches!(ev.name.as_str(), "session" | "turn.failed" | "settled" | "lagged" | "dropped") {
                anyhow::bail!("plugin event {} conflicts with a headless lifecycle record", ev.name);
            }
            mare_protocol::events::validate_protocol(&ev.name, &ev.payload)
                .map_err(|e| anyhow::anyhow!("invalid built-in event {}: {e}", ev.name))?;
            write_out(output, crate::headless_output::Line::Event(ev.wire_json.to_string())).await;
        }
        Ok(())
    }
}

struct TurnLoopResult {
    res: Result<Value>,
    forced: Option<Outcome>,
    event_failed: bool,
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
}

async fn handle_resume_error(host: &Host, json: bool, output: &crate::headless_output::Output) -> Result<Option<i32>> {
    // `--resume <unknown>` / `-c` with nothing to continue: an error (exit 1), never a silent
    // new session
    let Some(err) = resume_error(host).await else {
        return Ok(None);
    };
    if json {
        let session = serde_json::to_value(session_record(host).await)?;
        write_line(output, "session", session).await;
        let err_obj = event_error("internal", &err, None);
        let s = headless::Settled {
            error: Some(err_obj.clone()),
            exit_code: 1,
            reason: "error".into(),
            ..Default::default()
        };
        write_line(output, "turn.failed", serde_json::to_value(headless::TurnFailed { error: err_obj })?).await;
        write_line(output, "settled", serde_json::to_value(&s).unwrap_or_default()).await;
    }
    output.stderr(&format!("mare: {err}"));
    host.shutdown().await;
    Ok(Some(1))
}

fn setup_tap(
    host: &Host,
    json: bool,
    stream_deltas: bool,
) -> (mare_kernel::HeadlessTapHandle, mare_kernel::HeadlessCursor) {
    // Even text mode observes approvals and usage; JSON mode also selects its public events.
    let event_log = host.kernel.event_log().clone();
    host.kernel.open_headless_cursor(Arc::new(move |name| {
        let output = json && json_event(&event_log, name, stream_deltas);
        let tracking = matches!(name, "approval/resolved" | "usage/changed");
        let cache = name == "item/completed";
        (output || tracking || cache).then_some(mare_kernel::HeadlessSelection {
            selected: output,
            required: tracking || (output && required_event(&event_log, name)),
        })
    }))
}

async fn run_turn_loop(
    host: &Arc<Host>,
    prompt: String,
    output_schema: Option<serde_json::Value>,
    rx: &mut mare_kernel::HeadlessCursor,
    tap_failure: &mut tokio::sync::watch::Receiver<Option<String>>,
    state: &mut HeadlessRunState,
    output: &crate::headless_output::Output,
) -> TurnLoopResult {
    let mut forced: Option<Outcome> = None;
    let mut event_failed = false;
    let mut abort = None;
    // listening before the turn starts, for the whole loop (see `Interrupts`)
    let mut sigint = crate::signals::Interrupts::new();
    // empty prompt + -c: continue the interrupted turn instead of starting a new one
    let mut opts = serde_json::Map::new();
    if let Some(schema) = output_schema {
        opts.insert("output_schema".into(), schema);
    }
    let opts = serde_json::Value::Object(opts);
    let producer_host = host.clone();
    let mut fut = tokio::task::spawn_local(async move {
        if prompt.trim().is_empty() {
            producer_host.continue_turn_with(opts).await
        } else {
            producer_host.prompt(&prompt, opts).await
        }
    });
    // The producer keeps polling while the output consumer awaits an I/O acknowledgement.
    // Cancellation of the enclosing run must not leave that producer running detached.
    struct StopProducer(tokio::task::AbortHandle);
    impl Drop for StopProducer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop_producer = StopProducer(fut.abort_handle());
    let term = crate::signals::terminate();
    tokio::pin!(term);
    // SIGINT: the first cancels the turn (it then ends as aborted),
    // a second one exits right away
    let mut interrupts = 0u32;
    let mut pipe_check = tokio::time::interval(std::time::Duration::from_millis(250));
    pipe_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let res = loop {
        if stdout_closed() {
            // nobody reads the output any more: stop the turn and leave quietly
            rx.close();
            let _ = host.abort().await;
            break Err(anyhow::anyhow!("stdout closed"));
        }
        tokio::select! {
            biased;
            _ = tap_failure.changed() => {
                rx.close();
                event_failed = true;
                let _ = host.abort().await;
                break Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
            }
            Some(record) = rx.recv() => {
                if let Err(error) = state.handle_record(output, record).await {
                    rx.close();
                    event_failed = true;
                    let _ = host.abort().await;
                    break Err(error);
                }
            },
            _ = pipe_check.tick() => {
                if stdout_reader_gone() {
                    STDOUT_CLOSED.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            r = &mut fut => break r.map_err(anyhow::Error::from).and_then(|result| result),
            _ = sigint.recv() => {
                interrupts += 1;
                forced = Some(Outcome::Interrupted);
                if interrupts == 1 {
                    crate::signals::begin_forceable_shutdown(130);
                    output.stderr("mare: interrupted — cancelling the turn (interrupt again to exit)");
                    if abort.is_none() {
                        abort = Some(schedule_abort(host.clone()));
                    }
                } else {
                    crate::signals::force_exit_now(130);
                }
            }
            // SIGTERM/SIGHUP: stop the turn, then shut down normally (plugins get app/shutdown)
            _ = &mut term => {
                crate::signals::begin_forceable_shutdown(1);
                rx.close();
                let _ = host.abort().await;
                break Err(anyhow::anyhow!("terminated by signal"));
            }
        }
    };
    TurnLoopResult { res, forced, event_failed, abort }
}

async fn drain_abort(
    mut abort_done: tokio::sync::oneshot::Receiver<()>,
    rx: &mut mare_kernel::HeadlessCursor,
    tap_failure: &mut tokio::sync::watch::Receiver<Option<String>>,
    state: &mut HeadlessRunState,
    output: &crate::headless_output::Output,
    event_failed: &mut bool,
    res: &mut Result<Value>,
) {
    let mut abort_finished = false;
    loop {
        tokio::select! {
            _ = &mut abort_done => {
                abort_finished = true;
                break;
            }
            _ = tap_failure.changed() => {
                if tap_failure.borrow().is_some() {
                    *event_failed = true;
                    rx.close();
                    break;
                }
            }
            Some(record) = rx.recv() => {
                if let Err(error) = state.handle_record(output, record).await {
                    *event_failed = true;
                    *res = Err(error);
                    rx.close();
                    break;
                }
                if stdout_closed() {
                    rx.close();
                    break;
                }
            }
        }
    }
    if !abort_finished {
        let _ = abort_done.await;
    }
}

async fn drain_ready(
    rx: &mut mare_kernel::HeadlessCursor,
    state: &mut HeadlessRunState,
    output: &crate::headless_output::Output,
    res: &mut Result<Value>,
) {
    while let Some(record) = rx.try_recv() {
        if let Err(error) = state.handle_record(output, record).await {
            *res = Err(error);
            rx.close();
            break;
        }
    }
}

async fn drain_shutdown(
    host: Arc<Host>,
    rx: &mut mare_kernel::HeadlessCursor,
    tap_failure: &mut tokio::sync::watch::Receiver<Option<String>>,
    state: &mut HeadlessRunState,
    output: &crate::headless_output::Output,
    event_failed: &mut bool,
    res: &mut Result<Value>,
) {
    let mut shutdown = tokio::task::spawn_local(async move { host.shutdown().await });
    loop {
        if *event_failed || stdout_closed() {
            rx.close();
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                if let Err(error) = result {
                    tracing::warn!("headless host shutdown task failed: {error}");
                }
                break;
            }
            _ = tap_failure.changed() => {
                if tap_failure.borrow().is_some() {
                    *event_failed = true;
                    *res = Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
                    rx.close();
                    break;
                }
            }
            Some(record) = rx.recv() => {
                if let Err(error) = state.handle_record(output, record).await {
                    *event_failed = true;
                    *res = Err(error);
                    rx.close();
                    break;
                }
            }
        }
    }
    if !shutdown.is_finished() {
        let _ = shutdown.await;
    }
}

async fn drain_flush_lagged(
    tap: &mare_kernel::HeadlessTapHandle,
    rx: &mut mare_kernel::HeadlessCursor,
    tap_failure: &mut tokio::sync::watch::Receiver<Option<String>>,
    state: &mut HeadlessRunState,
    output: &crate::headless_output::Output,
    event_failed: &mut bool,
    res: &mut Result<Value>,
) {
    let flush = tap.flush_lagged();
    tokio::pin!(flush);
    loop {
        tokio::select! {
            result = &mut flush => {
                if let Err(error) = result {
                    *event_failed = true;
                    *res = Err(anyhow::anyhow!("event stream incomplete: {error}"));
                    rx.close();
                }
                break;
            }
            _ = tap_failure.changed() => {
                if tap_failure.borrow().is_some() {
                    *event_failed = true;
                    *res = Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
                    rx.close();
                    break;
                }
            }
            Some(record) = rx.recv() => {
                if let Err(error) = state.handle_record(output, record).await {
                    *event_failed = true;
                    *res = Err(error);
                    rx.close();
                    break;
                }
            }
        }
    }
}

fn build_settled(
    res: &Result<Value>,
    forced: Option<Outcome>,
    approval_abort: bool,
    run_cost: &mut Option<Value>,
) -> (headless::Settled, bool, Option<String>) {
    let mut invalid_output = false;
    let mut save_warning = None;
    let (mut settled, outcome) = match res {
        Ok(v) => {
            let error = parse_agent_error(v);
            let kind = v.get("error_kind").and_then(|e| e.as_str());
            save_warning = v.get("save_warning").and_then(|w| w.as_str()).map(clean_error);
            let termination_reason = v
                .get("termination_reason")
                .cloned()
                .and_then(|value| serde_json::from_value::<mare_protocol::AgentTerminationReason>(value).ok());
            invalid_output = kind == Some("invalid_output");
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let structured =
                if error.is_none() || kind == Some("save_failed") { v.get("structured").cloned() } else { None };
            let outcome = match (forced, termination_reason, kind, &error) {
                (Some(f), _, _, _) => f,
                (None, Some(mare_protocol::AgentTerminationReason::TurnLimit), _, _)
                | (None, _, Some("max_turns"), _) => Outcome::MaxTurns,
                // exit 3 only when the run *stopped* because of an approval: the turn was aborted
                // and an approval answered `abort` in it; a denial nobody could answer never
                // changes the exit code (a completed turn is 0, an unrelated abort is 1)
                (None, Some(mare_protocol::AgentTerminationReason::Interrupted), _, _)
                | (None, _, Some("aborted"), _)
                    if approval_abort =>
                {
                    Outcome::ApprovalUnavailable
                }
                (None, _, Some("save_failed"), _) => Outcome::SaveFailed,
                (None, _, _, Some(_)) => Outcome::Error,
                (None, _, _, None) => Outcome::Completed,
            };
            let mut usage = v.get("usage").cloned();
            if let (Some(serde_json::Value::Object(u)), Some(c)) = (usage.as_mut(), run_cost.take()) {
                u.insert("cost".into(), c);
            }
            (headless::Settled { text, structured, usage, error, termination_reason, ..Default::default() }, outcome)
        }
        Err(e) => (
            headless::Settled {
                error: Some(event_error("internal", clean_error(&format!("{e:#}")), None)),
                ..Default::default()
            },
            forced.unwrap_or(Outcome::Error),
        ),
    };
    if outcome == Outcome::ApprovalUnavailable
        && (settled.termination_reason == Some(mare_protocol::AgentTerminationReason::Interrupted)
            || settled.error.as_ref().is_none_or(|e| e.code == "aborted" || e.message == "aborted"))
    {
        settled.error =
            Some(event_error("unavailable", "an approval was needed and nobody could answer (denied)", None));
    }
    settled.exit_code = outcome.code();
    settled.reason = outcome.reason().into();
    (settled, invalid_output, save_warning)
}

/// Stderr notes printed after the settled result: a save-failure warning and the
/// resume hint for a persisted session (headless-resume-hint).
#[derive(Default, Clone, Copy)]
struct StderrNotes<'a> {
    save_warning: Option<&'a str>,
    resume_hint: Option<&'a str>,
}

async fn emit_settled_and_denials(
    output: &crate::headless_output::Output,
    json: bool,
    res_ok: bool,
    invalid_output: bool,
    settled: &headless::Settled,
    denials: &[String],
    notes: StderrNotes<'_>,
) -> Result<i32> {
    if json {
        if let Some(e) = &settled.error {
            write_line(output, "turn.failed", serde_json::to_value(headless::TurnFailed { error: e.clone() })?).await;
        }
        write_line(output, "settled", serde_json::to_value(settled)?).await;
    } else if !invalid_output
        && ((settled.exit_code == Outcome::SaveFailed.code() && !settled.text.is_empty()) || res_ok)
    {
        write_out(output, crate::headless_output::Line::Text(settled.text.clone())).await;
    }
    if stdout_closed() {
        return Ok(STDOUT_CLOSED_CODE);
    }
    // what was denied, whatever the exit code
    if !denials.is_empty() {
        let n = denials.len();
        let (s, were) = if n == 1 { ("", "was") } else { ("s", "were") };
        output.stderr(&format!("mare: {n} approval{s} {were} needed and nobody could answer (denied)"));
        for d in denials {
            output.stderr(d);
        }
    }
    if let Some(w) = notes.save_warning
        && settled.exit_code != Outcome::SaveFailed.code()
    {
        output.stderr(&format!("mare: warning: {w}"));
    }
    if let Some(e) = &settled.error {
        if settled.exit_code == Outcome::SaveFailed.code() {
            output.stderr(&format!("mare: warning: {}", e.message));
        } else {
            output.stderr(&format!("mare: {}", e.message));
        }
    }
    if let Some(hint) = notes.resume_hint {
        output.stderr(hint);
    }
    Ok(settled.exit_code)
}

async fn run_inner(
    host: Arc<Host>,
    prompt: String,
    json: bool,
    stream_deltas: bool,
    output_schema: Option<serde_json::Value>,
    output: &crate::headless_output::Output,
) -> Result<i32> {
    if let Some(code) = handle_resume_error(&host, json, output).await? {
        return Ok(code);
    }
    let (tap, mut rx) = setup_tap(&host, json, stream_deltas);
    if json {
        let rec = serde_json::to_value(session_record(&host).await)?;
        write_line(output, "session", rec).await;
    }
    let mut tap_failure = tap.failure.clone();
    let mut state = HeadlessRunState::new(json);
    let TurnLoopResult { mut res, forced, mut event_failed, mut abort } =
        run_turn_loop(&host, prompt, output_schema, &mut rx, &mut tap_failure, &mut state, output).await;

    if let Some(abort_done) = abort.take() {
        drain_abort(abort_done, &mut rx, &mut tap_failure, &mut state, output, &mut event_failed, &mut res).await;
    }
    if tap_failure.borrow().is_some() {
        event_failed = true;
        res = Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
        rx.close();
        let _ = host.abort().await;
    } else if !event_failed {
        drain_ready(&mut rx, &mut state, output, &mut res).await;
    }
    let resume_hint = crate::app::resume_hint(&host).await;
    drain_shutdown(host.clone(), &mut rx, &mut tap_failure, &mut state, output, &mut event_failed, &mut res).await;
    if tap_failure.borrow().is_some() {
        event_failed = true;
        res = Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
        rx.close();
    }
    if !event_failed && !stdout_closed() {
        drain_flush_lagged(&tap, &mut rx, &mut tap_failure, &mut state, output, &mut event_failed, &mut res).await;
    }
    if tap_failure.borrow().is_some() {
        event_failed = true;
        res = Err(anyhow::anyhow!("event stream incomplete: delivery failed"));
        rx.close();
    }
    if !event_failed && !stdout_closed() {
        drain_ready(&mut rx, &mut state, output, &mut res).await;
    }
    drop(host);
    if stdout_closed() {
        tracing::info!("headless: stdout closed — the turn was stopped");
        return Ok(STDOUT_CLOSED_CODE);
    }
    let (settled, invalid_output, save_warning) =
        build_settled(&res, forced, state.approval_abort, &mut state.run_cost);
    let notes = StderrNotes { save_warning: save_warning.as_deref(), resume_hint: resume_hint.as_deref() };
    emit_settled_and_denials(output, json, res.is_ok(), invalid_output, &settled, &state.denials, notes).await
}

/// Widest a denied call's summary goes on its stderr line.
const DENIAL_SUMMARY_WIDTH: usize = 100;

/// `mare: denied <tool>: <one-line summary> (<reason or source>)` for an `approval/resolved`
/// nobody answered (prints one declined line per command).
fn denial_line(payload: &serde_json::Value, requested: Option<&serde_json::Value>) -> String {
    let call = payload.get("call");
    let tool = call.and_then(|c| c.get("name")).and_then(|n| n.as_str()).unwrap_or("tool");
    let safe_request = requested.map(mare_shared::redact::redact_json);
    let args = safe_request.as_ref().and_then(|call| call.get("arguments"));
    let text = |k: &str| args.and_then(|a| a.get(k)).and_then(|v| v.as_str());
    let what = text("command")
        .or_else(|| text("path"))
        .or_else(|| text("file_path"))
        .map(str::to_string)
        .or_else(|| args.filter(|a| !a.is_null()).map(|a| a.to_string()))
        .unwrap_or_default();
    let mut one_line = what.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > DENIAL_SUMMARY_WIDTH {
        one_line = one_line.chars().take(DENIAL_SUMMARY_WIDTH - 1).collect::<String>() + "…";
    }
    let why = payload
        .get("reason")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty())
        .or_else(|| payload.get("source").and_then(|s| s.as_str()))
        .unwrap_or("no answer");
    let why = why.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("mare:   denied {tool}: {one_line} ({why})")
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_are_one_line_without_the_lua_traceback() {
        let raw = "runtime error: transport: request failed: error sending request for url (https://x/v1)\n\
                   stack traceback:\n\t[C]: in local 'poll'\n\t...llm.lua:1153: in function <...>";
        assert_eq!(super::clean_error(raw), "transport: request failed: error sending request for url (https://x/v1)");
        assert_eq!(super::clean_error("max turns reached"), "max turns reached");
        let log = mare_kernel::EventLog::new();
        assert!(super::json_event(&log, "turn/started", false));
        assert!(super::json_event(&log, "item/started", false));
        assert!(super::json_event(&log, "item/completed", false));
        assert!(!super::json_event(&log, "item/delta", false) && super::json_event(&log, "item/delta", true));
        // per-listener hook/start / hook/end are not public on the cordis tree (cke-interface.md, A)
        assert!(!super::json_event(&log, "hook/start", false));
        assert!(!super::json_event(&log, "hook/end", false));
    }
}

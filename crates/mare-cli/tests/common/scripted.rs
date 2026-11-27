//! Rust-native scripted model server (`ScriptedModel`) mounted on `common::MockServer`.
//! In-process HTTP/SSE renderer and capture-time request invariant checker
//! (Codex `validate_request_body_invariants` + Kimi `Step`/`Part` model).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use mare_host::Host;
use serde_json::{Value, json};

use super::{MockServer, Request, Resp, TestEnv, expose};

/// Supported wire protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Responses,
    Anthropic,
    Compat,
    Gemini,
}

impl Wire {
    pub const ALL: [Self; 4] = [Self::Responses, Self::Anthropic, Self::Compat, Self::Gemini];

    pub fn name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
            Self::Compat => "compat",
            Self::Gemini => "gemini",
        }
    }

    pub fn model(self) -> &'static str {
        match self {
            Self::Responses => "openai/gpt-6.1-terra",
            Self::Anthropic => "anthropic/claude-sonnet-5-5",
            Self::Compat => "deepseek/deepseek-chat",
            Self::Gemini => "google/gemini-2.5-pro",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::Responses => "/v1/responses",
            Self::Anthropic => "/v1/messages",
            Self::Compat => "/v1/chat/completions",
            Self::Gemini => ":streamGenerateContent",
        }
    }

    pub fn route_prefix(self) -> &'static str {
        match self {
            Self::Gemini => "/v1beta",
            _ => self.path(),
        }
    }
}

impl std::fmt::Display for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Finish reason for a scripted reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

/// Content/metadata part of a scripted model turn.
#[derive(Clone, Debug)]
pub enum Part {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    /// Tool call with an explicit provider signature (e.g. Gemini `thoughtSignature`).
    SignedToolCall {
        id: String,
        name: String,
        args: Value,
        signature: String,
    },
    /// Tool call whose arguments are still streaming (incomplete JSON, no terminal block stop).
    PartialToolCall {
        id: String,
        name: String,
        raw_args: String,
    },
    Finish(Finish),
    Usage {
        input: u64,
        output: u64,
        cached: u64,
    },
}

/// One scripted model response step.
#[derive(Clone, Debug)]
pub enum Step {
    Reply(Vec<Part>),
    HttpError {
        status: u16,
        retry_after: Option<String>,
        message: String,
    },
    StreamError {
        message: String,
        code: Option<String>,
    },
    CutAfter {
        parts: Vec<Part>,
        events: usize,
    },
    StallAfter {
        parts: Vec<Part>,
        events: usize,
        ms: u64,
    },
    Empty,
    /// A hand-written response (status, headers, body) for shapes the renderers don't produce:
    /// an upstream's exact error body, an error event with nested headers. Requests to it are
    /// still captured and checked like any other step.
    Raw(Resp),
}

/// Wire-agnostic view of a captured request's history.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub calls: Vec<(String, String, Value)>,
    pub results: Vec<(String, String)>,
    pub text: Vec<String>,
}

/// Render `parts` into individual SSE event frames for `wire`.
pub use super::scripted_render::{render, render_step};

// ---------------------------------------------------------------------------------------------
// Request parsing + capture-time invariant validation
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
enum HistStep {
    Turn,
    Call { key: String, name: String, args: Value, by_name: bool },
    Result { key: String, text: String },
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn extract_steps(wire: Wire, body: &Value) -> Vec<HistStep> {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let args = |v: &Value| match v {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::String(raw.clone())),
        other => other.clone(),
    };
    let mut out = Vec::new();
    match wire {
        Wire::Responses => {
            let mut after_result = true;
            for item in body["input"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("function_call" | "custom_tool_call") => {
                        if after_result {
                            out.push(HistStep::Turn);
                        }
                        after_result = false;
                        let a = if item["type"] == "function_call" {
                            args(&item["arguments"])
                        } else {
                            item["input"].clone()
                        };
                        out.push(HistStep::Call {
                            key: s(&item["call_id"]),
                            name: s(&item["name"]),
                            args: a,
                            by_name: false,
                        });
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        after_result = true;
                        out.push(HistStep::Result { key: s(&item["call_id"]), text: text_of(&item["output"]) });
                    }
                    Some("message") if item["role"] == "assistant" => {
                        out.push(HistStep::Turn);
                        after_result = false;
                    }
                    _ => {}
                }
            }
        }
        Wire::Anthropic => {
            for m in body["messages"].as_array().into_iter().flatten() {
                if m["role"] == "assistant" {
                    out.push(HistStep::Turn);
                }
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("tool_use") => out.push(HistStep::Call {
                            key: s(&b["id"]),
                            name: s(&b["name"]),
                            args: b["input"].clone(),
                            by_name: false,
                        }),
                        Some("tool_result") => {
                            out.push(HistStep::Result { key: s(&b["tool_use_id"]), text: text_of(&b["content"]) });
                        }
                        _ => {}
                    }
                }
            }
        }
        Wire::Compat => {
            for m in body["messages"].as_array().into_iter().flatten() {
                if m["role"] == "assistant" {
                    out.push(HistStep::Turn);
                }
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    out.push(HistStep::Call {
                        key: s(&c["id"]),
                        name: s(&c["function"]["name"]),
                        args: args(&c["function"]["arguments"]),
                        by_name: false,
                    });
                }
                if m["role"] == "tool" {
                    out.push(HistStep::Result { key: s(&m["tool_call_id"]), text: text_of(&m["content"]) });
                }
            }
        }
        Wire::Gemini => {
            for c in body["contents"].as_array().into_iter().flatten() {
                if c["role"] == "model" {
                    out.push(HistStep::Turn);
                }
                for p in c["parts"].as_array().into_iter().flatten() {
                    let key = |f: &Value| match f.get("id") {
                        None => (format!("name:{}", s(&f["name"])), true),
                        Some(id) => (s(id), false),
                    };
                    if let Some(f) = p.get("functionCall") {
                        let (key, by_name) = key(f);
                        out.push(HistStep::Call { key, name: s(&f["name"]), args: f["args"].clone(), by_name });
                    }
                    if let Some(f) = p.get("functionResponse") {
                        out.push(HistStep::Result { key: key(f).0, text: text_of(&f["response"]) });
                    }
                }
            }
        }
    }
    out
}

/// Checks wire path marker, JSON object body, wire array key, and Anthropic `max_tokens`.
pub fn assert_wire(wire: Wire, req: &Request) -> Result<(), String> {
    if !req.path.contains(wire.path()) {
        return Err(format!("{wire}: request to {}", req.path));
    }
    let body: Value =
        serde_json::from_str(&req.body).map_err(|e| format!("{wire}: non-JSON body ({e}): {}", req.body))?;
    if !body.is_object() {
        return Err(format!("{wire}: POST without a JSON object body: {body}"));
    }
    let key = match wire {
        Wire::Responses => "input",
        Wire::Anthropic | Wire::Compat => "messages",
        Wire::Gemini => "contents",
    };
    if !body[key].is_array() {
        return Err(format!("{wire}: body has no `{key}` array: {body}"));
    }
    if wire == Wire::Anthropic && !body["max_tokens"].is_number() {
        return Err(format!("anthropic: no max_tokens: {body}"));
    }
    Ok(())
}

/// Validate all request invariants (wire shape, tool call/result pairing, ids, Anthropic thinking signatures).
pub fn validate_request(
    wire: Wire,
    req: &Request,
    pending: &[String],
    expected_sigs: &HashMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    if let Err(e) = assert_wire(wire, req) {
        problems.push(e);
        return problems;
    }
    let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
    let mut open: Vec<(String, String)> = Vec::new();
    let mut ids = HashSet::new();
    for step in extract_steps(wire, &body) {
        match step {
            HistStep::Turn => {
                if !open.is_empty() {
                    problems.push(format!("{wire}: a model turn starts with calls unanswered: {open:?}"));
                }
            }
            HistStep::Call { key, name, by_name, .. } => {
                if key.is_empty() || key == "name:" || name.is_empty() {
                    problems.push(format!("{wire}: a call without id or name: {key:?} {name:?}"));
                }
                if !by_name && !ids.insert(key.clone()) {
                    problems.push(format!("{wire}: {key}: a call id used twice"));
                }
                open.push((key, name));
            }
            HistStep::Result { key, .. } if key.is_empty() => {
                problems.push(format!("{wire}: a result with an empty id"));
            }
            HistStep::Result { key, .. } => match open.iter().position(|(k, _)| *k == key) {
                Some(i) => {
                    open.remove(i);
                }
                None => {
                    problems.push(format!("{wire}: {key}: a result with no open call (orphan, duplicate or early)"))
                }
            },
        }
    }
    let unanswered: Vec<_> = open.iter().filter(|(k, _)| !pending.iter().any(|p| p == k)).collect();
    if !unanswered.is_empty() {
        problems.push(format!("{wire}: calls never answered: {unanswered:?}"));
    }
    if wire == Wire::Anthropic && !expected_sigs.is_empty() {
        for m in body["messages"].as_array().into_iter().flatten() {
            if m["role"] != "assistant" {
                continue;
            }
            for b in m["content"].as_array().into_iter().flatten() {
                if b["type"] == "thinking" {
                    let text = b["thinking"].as_str().unwrap_or("");
                    if let Some(want) = expected_sigs.get(text) {
                        let got = b["signature"].as_str().unwrap_or("");
                        if got != want {
                            problems
                                .push(format!("anthropic: thinking signature mismatch: got {got:?}, want {want:?}"));
                        }
                    }
                }
            }
        }
    }
    problems
}

/// Extract tool rounds `(name, args, result_text)` in order from a serialized request body.
pub fn tool_rounds(wire: Wire, body: &Value) -> Vec<(String, Value, String)> {
    let mut rounds: Vec<(String, String, Value, Option<String>)> = Vec::new();
    for step in extract_steps(wire, body) {
        match step {
            HistStep::Turn => {}
            HistStep::Call { key, name, args, .. } => rounds.push((key, name, args, None)),
            HistStep::Result { key, text } => {
                if let Some(r) = rounds.iter_mut().find(|r| r.0 == key && r.3.is_none()) {
                    r.3 = Some(text);
                }
            }
        }
    }
    rounds.into_iter().map(|(_, name, args, text)| (name, args, text.unwrap_or_default())).collect()
}

fn extract_turn(wire: Wire, req: &Request) -> Turn {
    let body = req.json();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for step in extract_steps(wire, &body) {
        match step {
            HistStep::Turn => {}
            HistStep::Call { key, name, args, .. } => calls.push((key, name, args)),
            HistStep::Result { key, text } => results.push((key, text)),
        }
    }
    let mut text = Vec::new();
    let push_text = |out: &mut Vec<String>, v: &Value| {
        if let Some(s) = v.as_str() {
            out.push(s.to_string());
        } else if let Some(arr) = v.as_array() {
            for item in arr {
                if let Some(t) = item["text"].as_str() {
                    out.push(t.to_string());
                }
            }
        }
    };
    match wire {
        Wire::Responses => {
            for item in body["input"].as_array().into_iter().flatten() {
                if item["type"] == "message" {
                    push_text(&mut text, &item["content"]);
                }
            }
        }
        Wire::Anthropic | Wire::Compat => {
            for m in body["messages"].as_array().into_iter().flatten() {
                if m["role"] != "tool" {
                    push_text(&mut text, &m["content"]);
                }
            }
        }
        Wire::Gemini => {
            for c in body["contents"].as_array().into_iter().flatten() {
                for p in c["parts"].as_array().into_iter().flatten() {
                    if let Some(t) = p["text"].as_str() {
                        text.push(t.to_string());
                    }
                }
            }
        }
    }
    Turn { calls, results, text }
}

/// In-process scripted model server with capture-time invariant checking.
pub struct ScriptedModel {
    wire: Wire,
    server: MockServer,
    steps_len: usize,
    pending: Arc<Mutex<Vec<String>>>,
    violations: Arc<Mutex<Vec<String>>>,
    finished: bool,
}

impl ScriptedModel {
    pub async fn start(wire: Wire, steps: Vec<Step>) -> Self {
        assert_ne!(
            std::env::var("MARE_TEST_ALLOW_LIVE").as_deref(),
            Ok("1"),
            "ScriptedModel tests never run with MARE_TEST_ALLOW_LIVE=1 (it turns the offline guard off)"
        );
        let server = MockServer::start().await;
        let steps_len = steps.len();
        let mut expected_sigs = HashMap::new();
        for s in &steps {
            let parts = match s {
                Step::Reply(p) | Step::CutAfter { parts: p, .. } | Step::StallAfter { parts: p, .. } => p.as_slice(),
                _ => &[],
            };
            for p in parts {
                if let Part::Reasoning { text, signature: Some(sig) } = p {
                    expected_sigs.insert(text.clone(), sig.clone());
                }
            }
        }
        let mut resps: Vec<Resp> = steps.iter().map(|s| render_step(wire, s)).collect();
        // Sentinel 500 for any request beyond the scripted steps.
        resps.push(Resp::status(500, r#"{"error":{"message":"unexpected extra request"}}"#));
        server.route(wire.route_prefix(), resps);

        let pending: Arc<Mutex<Vec<String>>> = Arc::default();
        let violations: Arc<Mutex<Vec<String>>> = Arc::default();
        let (p_ref, v_ref) = (pending.clone(), violations.clone());
        server.on_request(move |req| {
            let p = p_ref.lock().unwrap().clone();
            let mut errs = validate_request(wire, req, &p, &expected_sigs);
            if !errs.is_empty() {
                v_ref.lock().unwrap().append(&mut errs);
            }
        });

        Self { wire, server, steps_len, pending, violations, finished: false }
    }

    pub fn base(&self) -> &str {
        &self.server.base
    }

    pub fn host_config(&self) -> Value {
        let u = &self.server.base;
        json!({
            "model": self.wire.model(),
            "providers": { "deepseek": { "base_url": format!("{u}/v1") } },
            "plugins": {
                "provider-openai": { "base_url": format!("{u}/v1"), "transport": "http" },
                "provider-anthropic": { "base_url": u },
                "provider-google": { "base_url": format!("{u}/v1beta") },
                "provider-opencode-go": { "base_url": u },
                "llm": { "retry_base_ms": 1 },
            },
        })
    }

    /// Build a host configured for this scripted model's wire (with fake API keys and exposed test globals).
    pub async fn host(&self, env: &mut TestEnv, resume: bool, builtins: &[&str]) -> Arc<Host> {
        for (k, v) in [
            ("OPENAI_API_KEY", "test-openai"),
            ("ANTHROPIC_API_KEY", "test-anthropic"),
            ("DEEPSEEK_API_KEY", "test-deepseek"),
            ("GEMINI_API_KEY", "test-google"),
        ] {
            env.set_var(k, v);
        }
        let b = env.host().builtins(builtins).config(self.host_config());
        let h = if resume { b.resume() } else { b }.build().await;
        expose(&h);
        h
    }

    pub fn hold(&self, step: usize, release: Arc<tokio::sync::Notify>) {
        self.server.set_hold(self.wire.route_prefix(), step, release);
    }

    pub fn allow_pending(&self, ids: &[&str]) {
        *self.pending.lock().unwrap() = ids.iter().map(|s| (*s).to_string()).collect();
    }

    pub fn requests(&self) -> Vec<Request> {
        self.server.requests()
    }

    pub fn turns(&self) -> Vec<Turn> {
        self.requests().iter().map(|r| extract_turn(self.wire, r)).collect()
    }

    /// Check all accumulated invariant violations and exact step consumption without panicking.
    pub fn check_finish(&mut self) -> Result<(), String> {
        self.finished = true;
        let mut errs = self.violations.lock().unwrap().clone();
        let got = self.server.requests().len();
        if got != self.steps_len {
            errs.push(format!("{}: expected {} request(s), got {got}", self.wire, self.steps_len));
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(format!("ScriptedModel({}) violations:\n- {}", self.wire, errs.join("\n- ")))
        }
    }

    /// Verify all invariants and that all scripted steps were consumed; panics on any violation.
    pub fn finish(mut self) {
        if let Err(msg) = self.check_finish() {
            panic!("{msg}");
        }
    }
}

impl Drop for ScriptedModel {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            panic!("ScriptedModel({}) dropped without calling finish()", self.wire);
        }
    }
}

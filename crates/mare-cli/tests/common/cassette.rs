//! HTTP cassettes: recorded provider exchanges replayed through `MockServer`.
//!
//! Format: opencode `packages/http-recorder` cassette v1 (`src/cassette/model.ts`,
//! `src/http/model.ts`; MIT, anomalyco/opencode@367d24b), read as is, plus two optional
//! response fields of our own: `chunks` (the body as recorded, piece by piece: chunk boundaries
//! and relative time) and `end` (`close` | `reset` | `truncate`: how the connection ended).
//! A reader that does not know them ignores them.
//!
//! Matching (opencode `src/http/matching.ts`): a request takes the first *unused* interaction that
//! matches; the same request again takes the next recorded one (retries, polls), different
//! requests may come in any order. A request no interaction matches gets a JSON-path diff
//! against the closest candidate, and every interaction must be used by the end of the test.
//! Bodies are compared as canonical JSON after the volatile-field table below (docker-agent
//! `pkg/fake/proxy.go:384-470`: call ids, token limits, tool choice, thinking/reasoning config,
//! absolute paths).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Request, Resp};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cassette {
    pub version: u32,
    #[serde(default)]
    pub metadata: Value,
    pub interactions: Vec<Interaction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Interaction {
    pub transport: String,
    /// absent for response-only fixtures (public chunk recordings): served in order
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RecordedRequest>,
    pub response: RecordedResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
}

impl RecordedRequest {
    fn canonical_body(&self) -> Value {
        let content_type = self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case("content-type"));
        canonical_request_body(&self.body, content_type.map(|(_, value)| value.as_str()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    /// `base64` for a binary body (opencode `captureResponseBody`)
    #[serde(rename = "bodyEncoding", default, skip_serializing_if = "Option::is_none")]
    pub body_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<Chunk>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<End>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub t_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum End {
    Close,
    Reset,
    Truncate,
}

/// How requests pick interactions: public recordings come from another client (its own system
/// prompt and tools), so only their responses are meaningful; mare's own recordings also pin
/// the request mare sends, so a request-shape change fails the match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    ResponseOnly,
    RequestAndResponse,
}

impl Cassette {
    pub fn load(path: &std::path::Path) -> Self {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|_| panic!("cassette: failed to read"));
        let c: Self = serde_json::from_str(&raw).unwrap_or_else(|_| panic!("cassette: not a v1 cassette"));
        assert_eq!(c.version, 1, "cassette: unsupported version");
        for (i, it) in c.interactions.iter().enumerate() {
            assert!(it.transport == "http", "cassette interaction {i}: only http is replayed");
        }
        c
    }

    pub fn name(&self) -> &str {
        self.metadata["name"].as_str().unwrap_or("<unnamed>")
    }

    fn assert_nonempty(&self) {
        assert!(!self.interactions.is_empty(), "cassette has no interactions");
    }

    /// How to replay it: request+response when every interaction has its request (mare's own
    /// recordings), response-only when none has (public response recordings, a mare extension to
    /// opencode's schema, where `request` is required). A mix is refused.
    pub fn mode(&self) -> MatchMode {
        self.assert_nonempty();
        let with = self.interactions.iter().filter(|it| it.request.is_some()).count();
        match with {
            0 => MatchMode::ResponseOnly,
            n if n == self.interactions.len() => MatchMode::RequestAndResponse,
            n => panic!("cassette: {n} of {} interactions have a request: all or none", self.interactions.len()),
        }
    }
}

impl RecordedResponse {
    /// The body bytes: decoded base64, or the chunks joined, or the text body.
    pub fn bytes(&self) -> Vec<u8> {
        if let Some(chunks) = &self.chunks {
            return chunks.iter().flat_map(chunk_bytes).collect();
        }
        if self.body_encoding.as_deref() == Some("base64") {
            return base64::engine::general_purpose::STANDARD
                .decode(self.body.trim())
                .unwrap_or_else(|e| panic!("bodyEncoding base64: {e}"));
        }
        self.body.clone().into_bytes()
    }

    /// The response as `MockServer` serves it: recorded headers (content-type only, as
    /// recorded), the body split at the recorded chunk boundaries, and the recorded end.
    pub fn to_resp(&self) -> Resp {
        let bytes = self.bytes();
        let mut r = Resp::status(self.status, String::new());
        r.headers.clear();
        let ct = self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.clone());
        let ct = ct
            .unwrap_or_else(|| if self.status == 200 { "text/event-stream".into() } else { "application/json".into() });
        r.headers.push(("content-type".into(), ct));
        if let Some(chunks) = &self.chunks {
            let mut at = Vec::new();
            let mut off = 0;
            for c in &chunks[..chunks.len().saturating_sub(1)] {
                off += chunk_bytes(c).len();
                at.push(off);
            }
            r.chunks = Some((at, 0));
        }
        match self.end {
            Some(End::Reset) => r.reset = true,
            Some(End::Truncate) => r.cut_after = Some(bytes.len()),
            Some(End::Close) | None => {}
        }
        r.raw = Some(bytes);
        r
    }
}

fn chunk_bytes(c: &Chunk) -> Vec<u8> {
    match (&c.text, &c.b64) {
        (Some(t), _) => t.clone().into_bytes(),
        (None, Some(b)) => base64::engine::general_purpose::STANDARD.decode(b).expect("chunk b64"),
        (None, None) => Vec::new(),
    }
}

// --- volatile-field table and canonical form --------------------------------------------------

/// Top-level request keys whose values vary run to run or by configuration and are left out of the
/// comparison. Only at the top: the same names inside the history (an Anthropic `thinking` block
/// with its text and signature, a Responses `reasoning` item) are content and are compared.
const VOLATILE_KEYS: &[&str] = &[
    "max_tokens",
    "max_output_tokens",
    "maxOutputTokens",
    "max_completion_tokens",
    "thinkingConfig",
    "thinking",
    "reasoning",
    "reasoning_effort",
    "tool_choice",
    "prompt_cache_key",
    "metadata",
    "user",
    "stream_options",
];

fn volatile_strings() -> &'static [(regex::Regex, &'static str)] {
    static RE: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        [
            (r"\b(call|toolu|fc|tool)_[A-Za-z0-9_-]{6,}", "$1_ID"),
            (r"<cwd>\n[^\n]+", "<cwd>\n<PATH>"),
            (r"(/run|/tmp|/mnt|/home|/Users|/private|/var/folders)/[^\s\x22]*", "<PATH>"),
            (r"[A-Za-z]:\\[^\s\x22]*", "<PATH>"),
            (
                r"(?s)<environment>\n.*?\n</environment>",
                "<environment>\nOS: <OS>\nShell for commands: <SHELL>\n</environment>",
            ),
            (r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "<UUID>"),
            // session entry ids, only where a compaction summary names the range it replaced
            // ("summarized: entries <id> … <id>"); any other 16-hex value (a checksum in a message
            // or a tool argument) is compared as is
            (r"(summarized: entries )[0-9a-f]{16}( … )[0-9a-f]{16}", "${1}<ID16>${2}<ID16>"),
        ]
        .into_iter()
        .map(|(p, r)| (regex::Regex::new(p).unwrap(), r))
        .collect()
    })
}

fn normalize(v: &Value) -> Value {
    normalize_at(v, true)
}

fn normalize_at(v: &Value, top: bool) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            let mut keys: Vec<_> = m.keys().filter(|k| !(top && VOLATILE_KEYS.contains(&k.as_str()))).collect();
            keys.sort();
            for k in keys {
                out.insert(k.clone(), normalize_at(&m[k], false));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize_at(x, false)).collect()),
        Value::String(s) => {
            let mut s = s.clone();
            for (re, rep) in volatile_strings() {
                s = re.replace_all(&s, *rep).into_owned();
            }
            Value::String(s)
        }
        other => other.clone(),
    }
}

/// A body as compared: normalized JSON when it parses, else the normalized text.
pub fn canonical_body(body: &str) -> Value {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => normalize(&mare_shared::redact::redact_json(&v)),
        Err(_) => normalize(&Value::String(mare_shared::redact::redact_str(body))),
    }
}

fn canonical_request_body(body: &str, content_type: Option<&str>) -> Value {
    if !mare_shared::redact::is_form_content_type(content_type) {
        return canonical_body(body);
    }
    let fields = body
        .as_bytes()
        .split(|byte| *byte == b'&')
        .filter_map(mare_shared::redact::redact_form_field)
        .map(|(key, value, _)| Value::Array(vec![Value::String(key), Value::String(value)]))
        .collect();
    normalize(&Value::Array(fields))
}

/// JSON-path differences, at most `limit` lines, without request values or arbitrary field names.
pub fn json_diff(expected: &Value, actual: &Value, limit: usize) -> Vec<String> {
    fn field_name(key: &str) -> &str {
        match key {
            "model" | "messages" | "role" | "content" | "type" | "text" | "input" | "output" | "tools" | "name"
            | "arguments" | "function" | "tool_calls" | "tool_call_id" | "id" | "parameters" | "properties"
            | "required" | "description" | "system" | "instructions" | "stream" | "temperature" | "top_p"
            | "api_key" | "access_token" | "refresh_token" => key,
            _ => "<field>",
        }
    }

    fn walk(path: &str, e: &Value, a: &Value, out: &mut Vec<String>, limit: usize) {
        if out.len() >= limit || e == a {
            return;
        }
        match (e, a) {
            (Value::Object(em), Value::Object(am)) => {
                let mut keys: Vec<&String> = em.keys().chain(am.keys()).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    if out.len() >= limit {
                        break;
                    }
                    let p = format!("{path}.{}", field_name(k));
                    match (em.get(k), am.get(k)) {
                        (Some(x), Some(y)) => walk(&p, x, y, out, limit),
                        (Some(_), None) => out.push(format!("{p} missing")),
                        (None, Some(_)) => out.push(format!("{p} unexpected")),
                        (None, None) => {}
                    }
                }
            }
            (Value::Array(ea), Value::Array(aa)) => {
                for i in 0..ea.len().max(aa.len()) {
                    if out.len() >= limit {
                        break;
                    }
                    let p = format!("{path}[{i}]");
                    match (ea.get(i), aa.get(i)) {
                        (Some(x), Some(y)) => walk(&p, x, y, out, limit),
                        (Some(_), None) => out.push(format!("{p} missing")),
                        (None, Some(_)) => out.push(format!("{p} unexpected")),
                        (None, None) => {}
                    }
                }
            }
            _ => out.push(format!("{path} differs")),
        }
    }
    let mut out = Vec::new();
    walk("$", expected, actual, &mut out, limit);
    out
}

// --- replay -----------------------------------------------------------------------------------

/// One cassette being served: which interactions were used, and what did not match.
pub struct Replay {
    cassette: Cassette,
    mode: MatchMode,
    /// the route prefix the server strips before comparing paths
    prefix: String,
    used: Mutex<Vec<bool>>,
    misses: Mutex<Vec<String>>,
}

impl Replay {
    pub fn new(cassette: Cassette, mode: MatchMode, prefix: &str) -> Arc<Self> {
        cassette.assert_nonempty();
        let n = cassette.interactions.len();
        Arc::new(Self {
            cassette,
            mode,
            prefix: prefix.to_string(),
            used: Mutex::new(vec![false; n]),
            misses: Mutex::default(),
        })
    }

    pub fn cassette(&self) -> &Cassette {
        &self.cassette
    }

    fn path_matches(&self, recorded: &RecordedRequest, req: &Request) -> bool {
        let req_path = req.path.strip_prefix(self.prefix.as_str()).unwrap_or(&req.path);
        let req_path = req_path.split('?').next().unwrap_or(req_path);
        let rec_path =
            recorded.url.split("://").nth(1).and_then(|r| r.find('/').map(|i| &r[i..])).unwrap_or(&recorded.url);
        let rec_path = rec_path.split('?').next().unwrap_or(rec_path);
        recorded.method.eq_ignore_ascii_case(&req.method) && (req_path.is_empty() || rec_path.ends_with(req_path))
    }

    /// The response for `req` (marks its interaction used), or a 599 naming what did not match.
    pub fn respond(&self, req: &Request) -> Resp {
        let mut used = self.used.lock().unwrap();
        let pick = match self.mode {
            MatchMode::ResponseOnly => used.iter().position(|u| !u),
            MatchMode::RequestAndResponse => {
                let actual = canonical_request_body(&req.body, req.header("content-type"));
                self.cassette.interactions.iter().enumerate().position(|(i, it)| {
                    !used[i]
                        && it
                            .request
                            .as_ref()
                            .is_some_and(|r| self.path_matches(r, req) && r.canonical_body() == actual)
                })
            }
        };
        if let Some(i) = pick {
            used[i] = true;
            return self.cassette.interactions[i].response.to_resp();
        }
        let detail = self.miss_detail(req, &used);
        self.misses.lock().unwrap().push(detail.clone());
        Resp::status(599, serde_json::json!({ "error": { "message": detail } }).to_string())
    }

    fn miss_detail(&self, req: &Request, used: &[bool]) -> String {
        let candidate = self
            .cassette
            .interactions
            .iter()
            .enumerate()
            .find(|(i, it)| !used[*i] && it.request.as_ref().is_some_and(|r| self.path_matches(r, req)));
        match candidate {
            None if self.mode == MatchMode::ResponseOnly => "cassette: no unused interaction left".to_string(),
            None => "cassette: no unused interaction matches the request route".to_string(),
            Some((i, it)) => {
                let recorded = it.request.as_ref().unwrap();
                let expected = recorded.canonical_body();
                let actual = canonical_request_body(&req.body, req.header("content-type"));
                let diff = json_diff(&expected, &actual, 8);
                format!("cassette: request does not match interaction {i}:\n  {}", diff.join("\n  "))
            }
        }
    }

    /// What did not match so far (each with its JSON-path differences).
    pub fn unmatched(&self) -> Vec<String> {
        self.misses.lock().unwrap().clone()
    }

    /// Every interaction was used and every request matched.
    pub fn assert_done(&self) {
        let misses = self.misses.lock().unwrap();
        assert!(misses.is_empty(), "cassette: unmatched requests:\n{}", misses.join("\n"));
        let unused: Vec<usize> =
            self.used.lock().unwrap().iter().enumerate().filter(|(_, u)| !**u).map(|(i, _)| i).collect();
        assert!(unused.is_empty(), "cassette: unused recorded interactions {unused:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volatile_fields_and_ids_do_not_break_a_match() {
        let a =
            r#"{"model":"m","max_tokens":10,"messages":[{"tool_call_id":"call_abcdef123","content":"/tmp/x/a.txt"}]}"#;
        let b = r#"{"messages":[{"content":"/mnt/y/z/a.txt","tool_call_id":"call_zzzzzz999"}],"model":"m","max_tokens":99}"#;
        assert_eq!(canonical_body(a), canonical_body(b));
        let c = r#"{"model":"m","messages":[{"tool_call_id":"call_abcdef123","content":"other"}]}"#;
        let d = json_diff(&canonical_body(a), &canonical_body(c), 8);
        assert_eq!(d, vec!["$.messages[0].content differs".to_string()]);
    }

    /// `thinking` is dropped only as the top-level request option; a history block's
    /// thinking text (and signature) still has to match.
    #[test]
    fn thinking_in_the_history_is_compared() {
        let with = |sig: &str| {
            format!(
                r#"{{"thinking":{{"type":"enabled"}},"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{sig}"}}]}}]}}"#
            )
        };
        let a = canonical_body(&with("s1"));
        assert_eq!(a, canonical_body(&with("s1").replace(r#""type":"enabled""#, r#""type":"adaptive""#)));
        assert_ne!(a, canonical_body(&with("s2")), "a changed history signature must not match");
    }

    /// Only the entry ids a compaction summary names are
    /// normalized; two requests that differ only in a checksum elsewhere do not match.
    #[test]
    fn a_checksum_is_compared_and_only_summary_entry_ids_vary() {
        let summary =
            |a: &str, b: &str| format!(r#"{{"messages":[{{"content":"summarized: entries {a} … {b}\nmore"}}]}}"#);
        assert_eq!(
            canonical_body(&summary("4d3665a36c876b95", "6e2a7356834cb1aa")),
            canonical_body(&summary("9cb10682a9126d4e", "26454fa2535f41da"))
        );
        let sum = |c: &str| format!(r#"{{"messages":[{{"content":"sha {c}"}}],"tools":[{{"args":{{"sum":"{c}"}}}}]}}"#);
        assert_ne!(canonical_body(&sum("0123456789abcdef")), canonical_body(&sum("fedcba9876543210")));
    }

    fn replay_with_body(body: Value) -> Arc<Replay> {
        let cassette: Cassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "metadata": { "name": "offline-fixture" },
            "interactions": [{
                "transport": "http",
                "request": { "method": "POST", "url": "https://example.invalid/v1/messages", "body": body.to_string() },
                "response": { "status": 200, "body": "ok" }
            }]
        }))
        .unwrap();
        Replay::new(cassette, MatchMode::RequestAndResponse, "")
    }

    fn request(path: &str, body: Value) -> Request {
        Request { method: "POST".into(), path: path.into(), body: body.to_string(), headers: vec![] }
    }

    #[test]
    fn cassette_matches_redacted_recording_against_actual_secret_fields() {
        // Recorder redaction: mare-binding/src/http_record.rs:145-146,257-261.
        let recorded = serde_json::json!({
            "model": "offline-model",
            "api_key": "[REDACTED]",
            "messages": [{ "content": "business text", "access_token": "[REDACTED]" }]
        });
        let actual = serde_json::json!({
            "model": "offline-model",
            "api_key": "fake-cassette-api-secret-123",
            "messages": [{ "content": "business text", "access_token": "fake-nested-token-456" }]
        });
        let replay = replay_with_body(recorded);
        assert_eq!(replay.respond(&request("/v1/messages", actual)).status, 200);
        replay.assert_done();
    }

    #[test]
    fn cassette_matches_redacted_form_fields_without_ignoring_business_values() {
        let content_type = "application/x-www-form-urlencoded; charset=utf-8";
        let recorded = "grant_type=authorization_code&code=%5BREDACTED%5D&scope=read";
        let cassette: Cassette = serde_json::from_value(serde_json::json!({
            "version": 1,
            "interactions": [{
                "transport": "http",
                "request": { "method": "POST", "url": "https://example.invalid/oauth", "headers": { "content-type": content_type }, "body": recorded },
                "response": { "status": 200, "body": "ok" }
            }]
        })).unwrap();
        let request = |scope: &str| Request {
            method: "POST".into(),
            path: "/oauth".into(),
            headers: vec![("content-type".into(), content_type.into())],
            body: format!("grant_type=authorization_code&%63ode=fake-form-secret-123&scope={scope}"),
        };
        let replay = Replay::new(cassette.clone(), MatchMode::RequestAndResponse, "");
        assert_eq!(replay.respond(&request("read")).status, 200, "redacted form secret must match");
        let replay = Replay::new(cassette, MatchMode::RequestAndResponse, "");
        let mismatch = replay.respond(&request("write"));
        assert_eq!(mismatch.status, 599, "a changed form business field must not match");
        assert!(!mismatch.body.contains("fake-form-secret-123"), "form mismatch leaked a credential");
    }

    #[test]
    fn cassette_mismatch_exposes_only_safe_field_paths() {
        let secret = "fake-secret-as-object-key-123";
        let replay = replay_with_body(serde_json::json!({
            "model": "offline-model", "messages": [{ "content": "expected-business-value" }]
        }));
        let mut actual = serde_json::json!({
            "model": "offline-model", "messages": [{ "content": "different-business-value" }]
        });
        actual.as_object_mut().unwrap().insert(secret.into(), serde_json::json!("fake-secret-value-456"));
        let response = replay.respond(&request("/v1/messages", actual));
        assert_eq!(response.status, 599, "a changed business field must not match");
        let diagnostic = response.body;
        assert!(diagnostic.contains("$.messages[0].content"), "missing useful safe field path");
        for forbidden in [secret, "fake-secret-value-456", "expected-business-value", "different-business-value"] {
            assert!(!diagnostic.contains(forbidden), "mismatch diagnostic exposed a request value or unsafe key");
        }
    }

    #[test]
    fn cassette_unmatched_route_does_not_echo_request_path_or_credentials() {
        let replay = replay_with_body(serde_json::json!({"model": "offline-model"}));
        let response = replay.respond(&request("/other?api_key=fake-query-secret-123", serde_json::json!({})));
        assert_eq!(response.status, 599);
        assert!(!response.body.contains("fake-query-secret-123"), "unmatched route leaked a credential");
    }

    #[test]
    fn cassette_empty_interactions_cannot_choose_response_only_mode() {
        let cassette: Cassette =
            serde_json::from_value(serde_json::json!({ "version": 1, "interactions": [] })).unwrap();
        assert!(std::panic::catch_unwind(|| cassette.mode()).is_err(), "empty cassettes must be rejected");
        assert!(
            std::panic::catch_unwind(|| Replay::new(cassette, MatchMode::ResponseOnly, "")).is_err(),
            "an explicitly selected response-only replay must also reject an empty cassette"
        );
    }

    #[test]
    fn cassette_invalid_transport_diagnostic_does_not_echo_values_or_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-cassette-path-secret-123.json");
        let cassette = serde_json::json!({
            "version": 1,
            "interactions": [{ "transport": "fake-transport-secret-456", "response": { "status": 200 } }]
        });
        std::fs::write(&path, cassette.to_string()).unwrap();
        let error = std::panic::catch_unwind(|| Cassette::load(&path)).unwrap_err();
        let diagnostic = error
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| error.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            diagnostic.contains("only http is replayed"),
            "expected the transport validation to reject the fixture"
        );
        assert!(!diagnostic.contains("fake-cassette-path-secret-123"), "cassette path leaked");
        assert!(!diagnostic.contains("fake-transport-secret-456"), "invalid transport value leaked");
    }

    #[test]
    fn cassette_normalizes_cwd_paths_under_arbitrary_temp_roots() {
        let recorded = serde_json::json!({
            "model": "offline-model",
            "instructions": "prefix\n<cwd>\n/tmp/mare-record/mn-__cu205_/tmp/.tmpZATx2m\n</cwd>\nsuffix",
            "messages": [{ "role": "user", "content": "read /tmp/mare-record/work/file.txt" }]
        });
        for actual_cwd in [
            "/tmp/mare-record/mn-other/tmp/.tmpABC",
            "/run/user/1001/mn-cc_dt5nd/tmp/.tmpBt6or1",
            "/custom/scratch/workspace-123",
        ] {
            let actual = serde_json::json!({
                "model": "offline-model",
                "instructions": format!("prefix\n<cwd>\n{actual_cwd}\n</cwd>\nsuffix"),
                "messages": [{ "role": "user", "content": "read /run/user/1001/work/file.txt" }]
            });
            let replay = replay_with_body(recorded.clone());
            assert_eq!(
                replay.respond(&request("/v1/messages", actual)).status,
                200,
                "cwd layout {actual_cwd} should normalize to match recorded <cwd>"
            );
            replay.assert_done();
        }
        let with_extra = canonical_body(
            r#"{"input":"<cwd>\n/tmp/mare-record/mn-other/tmp/.tmpABC\n\nAdditional workspace directories:\n/opt/extra-workspace\n</cwd>"}"#,
        );
        let without_extra = canonical_body(r#"{"input":"<cwd>\n/tmp/mare-record/mn-other/tmp/.tmpABC\n</cwd>"}"#);
        assert_ne!(without_extra, with_extra, "Additional workspace directories must stay distinct");
    }
}

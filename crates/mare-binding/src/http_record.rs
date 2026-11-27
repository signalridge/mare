//! HTTP cassette recorder (debug/test builds only): with `MARE_RECORD_CASSETTE=<path>` set, every
//! `mare.http.request` exchange is appended to that cassette as it completes.
//!
//! The format is cassette v1 (the test-side reader is `crates/mare-cli/tests/common/cassette.rs`)
//! with our optional `response.chunks` (each body piece as received, with its time from the
//! response headers) and `response.end` (`close` | `reset` | `truncate`). The response stream is
//! tee'd, not buffered: the caller sees every piece as it arrives, exactly as without the
//! recorder.
//!
//! Redaction: request headers are an allowlist (content-type, accept, anthropic-version,
//! anthropic-beta, openai-beta), response headers keep content-type only, URL userinfo and
//! credential-like query parameters are replaced,
//! headers keep content-type only, URL userinfo and credential-like query parameters are replaced,
//! and bodies go through mare's own redaction (`mare_shared::redact`: credential-named JSON and
//! form fields, token shapes in text). A body that redaction changed keeps no raw `chunks` (they
//! would hold the original bytes, possibly split mid-secret). The whole cassette is then scanned
//! for secrets (known key shapes, JWTs and the value of every credential-like environment
//! variable) before it is written: a hit refuses the write and leaves the existing file untouched. `MARE_RECORD_PHASE=<label>` tags interactions
//! with a session phase (`metadata.phases`, one label per interaction).

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Instant;

use base64::Engine as _;
use bytes::Bytes;
use futures_util::Stream;
use mare_shared::redact::REDACTED;
use serde_json::{Value, json};

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

const REQUEST_HEADERS: &[&str] = &["content-type", "accept", "anthropic-version", "anthropic-beta", "openai-beta"];
/// The cassette path, when recording is on.
fn target() -> Option<std::path::PathBuf> {
    if !cfg!(any(debug_assertions, test)) {
        return None;
    }
    std::env::var_os("MARE_RECORD_CASSETTE").filter(|v| !v.is_empty()).map(Into::into)
}

/// What was sent, captured before the response arrives.
pub(crate) struct RequestSnapshot {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

/// A snapshot of the request when recording is on.
pub(crate) fn snapshot(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Option<RequestSnapshot> {
    target()?;
    Some(RequestSnapshot {
        method: method.to_string(),
        url: url.to_string(),
        headers: headers.to_vec(),
        body: body.map(<[u8]>::to_vec),
    })
}

/// `inner` with every piece passed through and recorded; the interaction is written when the
/// stream ends, fails, or is dropped (a caller that stops reading: `truncate`).
pub(crate) fn tee(req: RequestSnapshot, status: u16, headers: &[(String, String)], inner: ByteStream) -> ByteStream {
    let content_type = headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.clone());
    Box::pin(Tee { inner, req: Some(req), status, content_type, start: Instant::now(), chunks: Vec::new(), end: None })
}

struct Tee {
    inner: ByteStream,
    req: Option<RequestSnapshot>,
    status: u16,
    content_type: Option<String>,
    start: Instant,
    chunks: Vec<(u64, Bytes)>,
    end: Option<&'static str>,
}

impl Stream for Tee {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = self.inner.as_mut().poll_next(cx);
        match &next {
            Poll::Ready(Some(Ok(b))) => {
                let t = self.start.elapsed().as_millis() as u64;
                let b = b.clone();
                self.chunks.push((t, b));
            }
            Poll::Ready(Some(Err(_))) => {
                self.end = Some("reset");
                self.finish();
            }
            Poll::Ready(None) => {
                self.end.get_or_insert("close");
                self.finish();
            }
            Poll::Pending => {}
        }
        next
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        if self.req.is_some() {
            self.end = Some("truncate");
            self.finish();
        }
    }
}

impl Tee {
    fn finish(&mut self) {
        let Some(req) = self.req.take() else { return };
        let Some(path) = target() else { return };
        let phase = std::env::var("MARE_RECORD_PHASE").ok().filter(|p| !p.is_empty());
        let written = interaction(&req, self.status, self.content_type.as_deref(), &self.chunks, self.end)
            .and_then(|it| append(&path, it, phase));
        if let Err(e) = written {
            eprintln!("mare: cassette {} not written: {e}", path.display());
        }
    }
}

fn interaction(
    req: &RequestSnapshot,
    status: u16,
    content_type: Option<&str>,
    chunks: &[(u64, Bytes)],
    end: Option<&str>,
) -> Result<Value, String> {
    let mut headers = serde_json::Map::new();
    for (k, v) in &req.headers {
        let k = k.to_ascii_lowercase();
        if REQUEST_HEADERS.contains(&k.as_str()) {
            headers.insert(k, Value::String(v.clone()));
        }
    }
    let request_content_type = headers.get("content-type").and_then(Value::as_str);
    let body = req.body.as_deref().map(|b| redact_body(b, request_content_type).0).unwrap_or_default();
    let all: Vec<u8> = chunks.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    let mut response = json!({ "status": status, "headers": {} });
    if let Some(ct) = content_type {
        response["headers"]["content-type"] = Value::String(ct.to_string());
    }
    let mut redacted = false;
    match String::from_utf8(all) {
        Ok(text) => {
            let (body, changed) = redact_body(text.as_bytes(), content_type);
            redacted = changed;
            response["body"] = Value::String(body);
        }
        Err(e) => {
            // not UTF-8 (binary, text cut mid-character, or UTF-16 text): stored as base64, where the
            // write-time scan cannot see a secret — its text form is checked first (what a BOM says
            // the text is, else lossy UTF-8), and a credential refuses it
            let text = decode_utf8_or_bom_lossy(e.as_bytes());
            let mut hits = vec![];
            decoded_secrets(&Value::String(text.clone()), 0, &mut hits);
            if redact_body(text.as_bytes(), content_type).1 {
                hits.push("a credential-named field".into());
            }
            if !hits.is_empty() {
                hits.sort();
                hits.dedup();
                return Err(format!(
                    "refused: a response body that is not UTF-8 holds a credential ({})",
                    hits.join(", ")
                ));
            }
            response["body"] = Value::String(base64::engine::general_purpose::STANDARD.encode(e.as_bytes()));
            response["bodyEncoding"] = Value::String("base64".into());
        }
    }
    // the raw pieces would carry what redaction removed from the body
    if chunks.len() > 1 && !redacted {
        response["chunks"] = Value::Array(
            chunks
                .iter()
                .map(|(t, b)| match std::str::from_utf8(b) {
                    Ok(s) => json!({ "t_ms": t, "text": s }),
                    Err(_) => json!({ "t_ms": t, "b64": base64::engine::general_purpose::STANDARD.encode(b) }),
                })
                .collect(),
        );
    }
    if let Some(end) = end.filter(|e| *e != "close") {
        response["end"] = Value::String(end.to_string());
    }
    Ok(json!({
        "transport": "http",
        "request": { "method": req.method, "url": redact_url(&req.url), "headers": headers, "body": body },
        "response": response,
    }))
}

fn redact_url(url: &str) -> String {
    let Ok(mut u) = url::Url::parse(url) else { return mare_shared::redact::redact_str(url) };
    if !u.username().is_empty() || u.password().is_some() {
        let _ = u.set_username(REDACTED);
        let _ = u.set_password(None);
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                let secret = mare_shared::redact::sensitive_query_key(&k);
                (k.into_owned(), if secret { REDACTED.to_string() } else { v.into_owned() })
            })
            .collect();
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.to_string()
}

/// A body with credentials removed (`mare_shared::redact`): credential-named JSON and form fields,
/// and token shapes in strings or other text.
/// Keeps unredacted formatting, escaping raw invalid UTF-8 bytes for JSON, and reports redactions.
fn redact_body(body: &[u8], content_type: Option<&str>) -> (String, bool) {
    use mare_shared::redact::{is_form_content_type, redact_form_field, redact_json, redact_str};
    let text = String::from_utf8_lossy(body).into_owned();
    if is_form_content_type(content_type) {
        let mut changed = false;
        let mut form = String::with_capacity(body.len());
        for (index, field) in body.split(|byte| *byte == b'&').enumerate() {
            if index > 0 {
                form.push('&');
            }
            let Some((redacted_key, redacted_value, field_changed)) = redact_form_field(field) else { continue };
            if field_changed {
                changed = true;
                let mut field = url::form_urlencoded::Serializer::new(String::new());
                field.append_pair(&redacted_key, &redacted_value);
                form.push_str(&field.finish());
            } else {
                append_form_segment(&mut form, field);
            }
        }
        if changed || std::str::from_utf8(body).is_err() {
            return (form, changed);
        }
    }
    if let Ok(v) = serde_json::from_str::<Value>(&text)
        && (v.is_object() || v.is_array())
    {
        let r = redact_json(&v);
        return if r == v { (text, false) } else { (r.to_string(), true) };
    }
    let r = redact_str(&text);
    let changed = r != text;
    (r, changed)
}

/// Keep valid original bytes verbatim; escape invalid UTF-8 so JSON can store the form losslessly.
fn append_form_segment(out: &mut String, mut segment: &[u8]) {
    while !segment.is_empty() {
        match std::str::from_utf8(segment) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                out.push_str(std::str::from_utf8(&segment[..valid_len]).expect("valid UTF-8 prefix"));
                segment = &segment[valid_len..];
                let invalid_len = error.error_len().unwrap_or(segment.len());
                out.extend(url::form_urlencoded::byte_serialize(&segment[..invalid_len]));
                segment = &segment[invalid_len..];
            }
        }
    }
}

/// Secrets left in `text`: known key shapes and credential-like environment values.
pub(crate) fn find_secrets(text: &str) -> Vec<String> {
    static PATTERNS: std::sync::OnceLock<Vec<(regex::Regex, &str)>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            // the shared redactor's rule (mare-shared redact.rs): spaces or tabs, never a line break
            (r"(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*", "bearer token"),
            (r"\bsk-(?:ant-)?[A-Za-z0-9_-]{16,}", "sk- key"),
            (r"\bAIza[0-9A-Za-z_-]{30,}", "Google API key"),
            (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "AWS access key"),
            (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "JWT"),
            (r"\bgh[pousr]_[A-Za-z0-9]{30,}", "GitHub token"),
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "private key"),
        ]
        .into_iter()
        .map(|(p, what)| (regex::Regex::new(p).unwrap(), what))
        .collect()
    });
    let mut hits: Vec<String> =
        patterns.iter().filter(|(re, _)| re.is_match(text)).map(|(_, what)| (*what).to_string()).collect();
    for (k, v) in std::env::vars() {
        let ku = k.to_ascii_uppercase();
        let credential = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"].iter().any(|w| ku.contains(w));
        if credential && v.len() >= 8 && text.contains(&v) {
            hits.push(format!("value of ${k}"));
        }
    }
    hits
}

/// How many decoding layers one string goes through (a form / percent layer, a string that is JSON
/// itself): the encodings the recorder writes and the ones its inputs carry, each a layer.
const DECODE_DEPTH: usize = 4;

/// One form-urlencoded component (no `&`): `+` is a space, `%XX` a byte.
fn form_decode(raw: &str) -> String {
    url::form_urlencoded::parse(format!("k={raw}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

fn decode_utf8_or_bom_lossy(bytes: &[u8]) -> String {
    match encoding_rs::Encoding::for_bom(bytes) {
        Some((encoding, bom)) => encoding.decode_without_bom_handling(&bytes[bom..]).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Decode a standalone base64 (or `"Basic <b64>"`) token if it yields UTF-8 or BOM-tagged text.
fn decode_base64_text(raw: &str) -> Option<String> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let trimmed = raw.trim();
    let candidate = trimmed
        .as_bytes()
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case(b"basic "))
        .map_or(trimmed, |_| trimmed[6..].trim_start());
    // 8-byte minimum credential value encodes to >= 11 unpadded base64 characters.
    if candidate.len() < 11 {
        return None;
    }
    let mut has_url_safe = false;
    let mut has_std_only = false;
    for b in candidate.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'=' => {}
            b'+' | b'/' => has_std_only = true,
            b'-' | b'_' => has_url_safe = true,
            _ => return None,
        }
    }
    if has_std_only && has_url_safe {
        return None;
    }
    let engine = match (has_url_safe, candidate.ends_with('=')) {
        (true, true) => &URL_SAFE,
        (true, false) => &URL_SAFE_NO_PAD,
        (false, true) => &STANDARD,
        (false, false) => &STANDARD_NO_PAD,
    };
    let decoded = engine.decode(candidate).ok()?;
    if decoded.len() < 8 {
        return None;
    }
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(&decoded) {
        return Some(encoding.decode_without_bom_handling(&decoded[bom..]).0.into_owned());
    }
    String::from_utf8(decoded).ok()
}

fn embedded_basic_auths(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if bytes[i..i + 6].eq_ignore_ascii_case(b"basic ") && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            let rest = raw[i + 6..].trim_start();
            let token = rest
                .split(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')'))
                .next()
                .unwrap_or("");
            if let Some(decoded) = decode_base64_text(token) {
                out.push(decoded);
            }
            i += 6;
        } else {
            i += 1;
        }
    }
    out
}

/// Reassemble JSON SSE string deltas (`data: {...}`) by stream slot & field path so credentials in
/// single-event or split streamed chunks cannot evade deep decoding and secret scanning.
fn sse_assembled_streams(raw: &str) -> Vec<String> {
    let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    if !raw.contains("data:") {
        return Vec::new();
    }
    let mut streams: BTreeMap<String, String> = BTreeMap::new();
    let mut path = String::new();
    for payload in raw
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| !payload.is_empty() && *payload != "[DONE]")
    {
        if let Ok(event) = serde_json::from_str::<Value>(payload) {
            path.clear();
            collect_sse_strings(&event, &mut path, &mut streams);
        }
    }
    streams.into_values().collect()
}

fn collect_sse_strings(value: &Value, path: &mut String, out: &mut BTreeMap<String, String>) {
    match value {
        Value::String(s) if !s.is_empty() => {
            out.entry(path.clone()).or_default().push_str(s);
        }
        Value::Array(items) => {
            let base = path.len();
            for (i, item) in items.iter().enumerate() {
                let slot = item.get("index").and_then(Value::as_u64).unwrap_or(i as u64);
                path.push('[');
                path.push_str(&slot.to_string());
                path.push(']');
                collect_sse_strings(item, path, out);
                path.truncate(base);
            }
        }
        Value::Object(map) => {
            let base = path.len();
            for key in ["output_index", "content_index", "index"] {
                if let Some(idx) = map.get(key).and_then(Value::as_u64) {
                    path.push('#');
                    path.push_str(&idx.to_string());
                }
            }
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "id" | "model" | "object" | "role" | "type" | "finish_reason" | "stop_reason" | "signature"
                ) {
                    continue;
                }
                let before_field = path.len();
                path.push('.');
                path.push_str(k);
                collect_sse_strings(v, path, out);
                path.truncate(before_field);
            }
            path.truncate(base);
        }
        _ => {}
    }
}

/// Secrets in what `value` decodes to: every JSON string as it is (no JSON escapes); each form /
/// percent-decoded component that decoding changed (a form body re-encoded after a field was
/// redacted, or kept in the client's encoding; a re-encoded URL query; a value that is itself
/// encoded, as a whole URL in one query value), scanned again as a string; standalone base64 (or
/// `Basic <b64>`) payloads yielding UTF-8 / BOM text; reassembled SSE `data:` JSON string streams;
/// and a string that is JSON itself (an object, an array or a string literal). The serialized
/// cassette holds these only in encoded form, where a known credential is not found verbatim.
fn decoded_secrets(value: &Value, depth: usize, hits: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            hits.extend(find_secrets(s));
            if depth >= DECODE_DEPTH {
                return;
            }
            // `&` and `=` alone decode nothing: the whole string was scanned already
            if s.contains(['%', '+']) {
                for pair in s.split('&') {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    for raw in [key, value] {
                        let decoded = form_decode(raw);
                        if decoded != raw {
                            decoded_secrets(&Value::String(decoded), depth + 1, hits);
                        }
                    }
                }
            }
            if let Ok(inner @ (Value::Object(_) | Value::Array(_) | Value::String(_))) =
                serde_json::from_str::<Value>(s)
            {
                decoded_secrets(&inner, depth + 1, hits);
            }
            if let Some(decoded) = decode_base64_text(s) {
                decoded_secrets(&Value::String(decoded), depth + 1, hits);
            } else if s.as_bytes().windows(6).any(|w| w.eq_ignore_ascii_case(b"basic ")) {
                for decoded in embedded_basic_auths(s) {
                    decoded_secrets(&Value::String(decoded), depth + 1, hits);
                }
            }
            for assembled in sse_assembled_streams(s) {
                decoded_secrets(&Value::String(assembled), depth + 1, hits);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| decoded_secrets(item, depth, hits)),
        Value::Object(fields) => {
            for (key, item) in fields {
                hits.extend(find_secrets(key));
                decoded_secrets(item, depth, hits);
            }
        }
        _ => {}
    }
}

static WRITE: Mutex<()> = Mutex::new(());

/// Append one interaction (and its phase label) to the cassette at `path`, refusing a write that
/// would contain a secret. Written to a sibling temporary file, then renamed over the cassette.
fn append(path: &std::path::Path, interaction: Value, phase: Option<String>) -> Result<(), String> {
    let _w = WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut cassette = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<Value>(&raw).map_err(|e| format!("existing cassette is not JSON: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let recorded_at = jiff::Timestamp::now().to_string();
            json!({ "version": 1, "metadata": { "name": name, "recordedAt": recorded_at, "source": "mare-record" }, "interactions": [] })
        }
        Err(e) => return Err(e.to_string()),
    };
    // the new interaction's decoded values (the interactions already on disk were checked when they
    // were written)
    let mut hits = vec![];
    decoded_secrets(&interaction, 0, &mut hits);
    let interactions = cassette["interactions"].as_array_mut().ok_or("cassette has no interactions array")?;
    interactions.push(interaction);
    let n = interactions.len();
    if let Some(phase) = phase {
        let phases = cassette["metadata"]
            .as_object_mut()
            .ok_or("cassette metadata is not an object")?
            .entry("phases")
            .or_insert_with(|| Value::Array(Vec::new()));
        let phases = phases.as_array_mut().ok_or("metadata.phases is not an array")?;
        // interactions recorded without a phase before this one keep an empty label
        phases.resize(n - 1, Value::String(String::new()));
        phases.push(Value::String(phase));
    }
    let text = serde_json::to_string_pretty(&cassette).map_err(|e| e.to_string())? + "\n";
    hits.extend(find_secrets(&text));
    hits.sort();
    hits.dedup();
    if !hits.is_empty() {
        return Err(format!("refused: it would contain a secret ({})", hits.join(", ")));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "http_record_tests.rs"]
mod tests;

#[cfg(test)]
mod followup_tests {
    use super::*;

    #[test]
    fn form_redaction_preserves_unmodified_encoded_fields() {
        let form = b"name=Andr%E9&spelling=%41+%42&flag&&code=SECRET";
        let (redacted, changed) = redact_body(form, Some("application/x-www-form-urlencoded; charset=iso-8859-1"));
        assert!(changed);
        assert_eq!(redacted, "name=Andr%E9&spelling=%41+%42&flag&&code=%5BREDACTED%5D");
    }

    #[test]
    fn form_redaction_preserves_non_utf8_bytes_as_form_escapes() {
        let (redacted, changed) = redact_body(b"name=Andr\xe9&code=SECRET", Some("application/x-www-form-urlencoded"));
        assert!(changed);
        assert_eq!(redacted, "name=Andr%E9&code=%5BREDACTED%5D");
        assert_eq!(
            redact_body(b"name=Andr\xe9", Some("application/x-www-form-urlencoded")),
            ("name=Andr%E9".into(), false)
        );
    }

    #[test]
    fn form_redaction_hides_credentials_but_preserves_oauth_type_fields() {
        // Exercise form redaction on an OAuth token-exchange payload carrying all three fields.
        let form = concat!(
            "subject_token=SECRET&subject_token_type=urn%3Aietf%3Arefresh_token",
            "&requested_token_type=urn%3Aietf%3Aid-jag&x-amz-Credential=CRED&sig=SIG&signature=SIGNATURE"
        );
        let (redacted, changed) = redact_body(form.as_bytes(), Some("application/x-www-form-urlencoded"));
        assert!(changed);
        let fields = url::form_urlencoded::parse(redacted.as_bytes()).into_owned().collect::<BTreeMap<_, _>>();
        for key in ["subject_token", "x-amz-Credential", "sig", "signature"] {
            assert_eq!(fields[key], REDACTED, "{key}: {redacted}");
        }
        assert_eq!(fields["subject_token_type"], "urn:ietf:refresh_token");
        assert_eq!(fields["requested_token_type"], "urn:ietf:id-jag");
    }

    #[test]
    fn query_redaction_hides_credentials_but_preserves_oauth_type_fields() {
        let url = redact_url(concat!(
            "https://example.test/?subject_token=SECRET&subject_token_type=urn%3Aietf%3Arefresh_token",
            "&requested_token_type=urn%3Aietf%3Aid-jag&x-amz-Credential=CRED&sig=SIG&signature=SIGNATURE"
        ));
        let parsed = url::Url::parse(&url).unwrap();
        let fields = parsed.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        for key in ["subject_token", "x-amz-Credential", "sig", "signature"] {
            assert_eq!(fields[key], REDACTED, "{key}: {url}");
        }
        assert_eq!(fields["subject_token_type"], "urn:ietf:refresh_token");
        assert_eq!(fields["requested_token_type"], "urn:ietf:id-jag");
    }
}

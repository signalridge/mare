use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::cassette;

// ---------------------------------------------------------------------------------------------
// Scripted HTTP server (no external crates): routes by path prefix to a queue of responses;
// the last response of a queue repeats. Records every request.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// close the connection after this many body bytes (a stream cut mid-message)
    pub cut_after: Option<usize>,
    /// after this many body bytes, go silent for the given time (an idle stream)
    pub stall: Option<(usize, u64)>,
    /// send the body in pieces split at these byte offsets, pausing this many ms between them
    /// (frames cut mid-line, characters split across chunks, heartbeats between gaps)
    pub chunks: Option<(Vec<usize>, u64)>,
    /// Explicit test barrier after request capture and before writing the response.
    pub release: Option<Arc<tokio::sync::Notify>>,
    /// the body as bytes (a binary or replayed body); `body` is ignored when set
    pub raw: Option<Vec<u8>>,
    /// end the connection with a TCP reset instead of a close
    pub reset: bool,
}

impl Resp {
    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into(),
            cut_after: None,
            stall: None,
            chunks: None,
            release: None,
            raw: None,
            reset: false,
        }
    }
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
            cut_after: None,
            stall: None,
            chunks: None,
            release: None,
            raw: None,
            reset: false,
        }
    }
    pub fn hold(mut self, release: Arc<tokio::sync::Notify>) -> Self {
        self.release = Some(release);
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn cut(mut self, after: usize) -> Self {
        self.cut_after = Some(after);
        self
    }
    pub fn stall(mut self, after: usize, ms: u64) -> Self {
        self.stall = Some((after, ms));
        self
    }
    pub fn chunked(mut self, at: Vec<usize>, pause_ms: u64) -> Self {
        self.chunks = Some((at, pause_ms));
        self
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
    /// request header lines, names lowercased
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

type Routes = Arc<Mutex<Vec<(String, Vec<Resp>)>>>;
type Cassettes = Arc<Mutex<Vec<(String, Arc<cassette::Replay>)>>>;
type OnRequest = Arc<dyn Fn(&Request) + Send + Sync>;

pub struct MockServer {
    pub base: String,
    routes: Routes,
    cassettes: Cassettes,
    requests: Arc<Mutex<Vec<Request>>>,
    on_request: Arc<Mutex<Option<OnRequest>>>,
}

impl MockServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let routes: Routes = Arc::default();
        let cassettes: Cassettes = Arc::default();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let on_request: Arc<Mutex<Option<OnRequest>>> = Arc::default();
        let (r, c, q, h) = (routes.clone(), cassettes.clone(), requests.clone(), on_request.clone());
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle(sock, r.clone(), c.clone(), q.clone(), h.clone()));
            }
        });
        Self { base, routes, cassettes, requests, on_request }
    }

    /// Register a capture-time hook invoked for each incoming request before responding.
    pub fn on_request(&self, f: impl Fn(&Request) + Send + Sync + 'static) {
        *self.on_request.lock().unwrap() = Some(Arc::new(f));
    }

    /// Attach a `Notify` barrier to `step` of the route registered under `prefix`.
    pub fn set_hold(&self, prefix: &str, step: usize, release: Arc<tokio::sync::Notify>) {
        let mut r = self.routes.lock().unwrap();
        if let Some((_, q)) = r.iter_mut().find(|(p, _)| p == prefix)
            && let Some(resp) = q.get_mut(step)
        {
            resp.release = Some(release);
        }
    }

    /// Responses for requests whose path starts with `prefix`, served in order.
    pub fn route(&self, prefix: &str, responses: Vec<Resp>) {
        self.routes.lock().unwrap().push((prefix.to_string(), responses));
    }

    /// Serve requests under `prefix` from a cassette (see `cassette.rs`); call
    /// `assert_done` on the returned replay at the end of the test.
    pub fn cassette(
        &self,
        prefix: &str,
        cassette: cassette::Cassette,
        mode: cassette::MatchMode,
    ) -> Arc<cassette::Replay> {
        let replay = cassette::Replay::new(cassette, mode, prefix);
        self.cassettes.lock().unwrap().push((prefix.to_string(), replay.clone()));
        replay
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.requests().iter().filter(|r| r.path.starts_with(prefix)).count()
    }

    /// A handle that outlives a borrow of the server (a Lua test hook can wait on it): how many
    /// requests under `prefix` have arrived so far.
    pub fn counter(&self, prefix: &str) -> impl Fn() -> usize + 'static {
        let (requests, prefix) = (self.requests.clone(), prefix.to_string());
        move || requests.lock().unwrap().iter().filter(|r| r.path.starts_with(&prefix)).count()
    }
}

async fn handle(
    mut sock: tokio::net::TcpStream,
    routes: Routes,
    cassettes: Cassettes,
    requests: Arc<Mutex<Vec<Request>>>,
    on_request: Arc<Mutex<Option<OnRequest>>>,
) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let (head_end, len) = loop {
        let Ok(n) = sock.read(&mut tmp).await else { return };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + len {
        let Ok(n) = sock.read(&mut tmp).await else { break };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let method = head.split_whitespace().next().unwrap_or("GET").to_string();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())))
        .collect();
    let req = Request { method, path: path.clone(), body, headers };
    if let Some(hook) = on_request.lock().unwrap().as_ref().cloned() {
        hook(&req);
    }
    let replay = cassettes.lock().unwrap().iter().find(|(p, _)| path.starts_with(p.as_str())).map(|(_, r)| r.clone());
    let from_cassette = replay.map(|r| r.respond(&req));
    requests.lock().unwrap().push(req);
    let resp = if let Some(resp) = from_cassette {
        resp
    } else {
        let mut r = routes.lock().unwrap();
        match r.iter_mut().find(|(p, _)| path.starts_with(p.as_str())) {
            Some((_, q)) if q.len() > 1 => q.remove(0),
            Some((_, q)) if !q.is_empty() => q[0].clone(),
            _ => Resp::status(404, "{}"),
        }
    };
    if let Some(release) = &resp.release {
        release.notified().await;
    }
    let mut out = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", resp.status);
    for (k, v) in &resp.headers {
        let _ = write!(out, "{k}: {v}\r\n");
    }
    let full: &[u8] = resp.raw.as_deref().unwrap_or(resp.body.as_bytes());
    if resp.cut_after.is_none() && resp.status != 200 {
        let _ = write!(out, "content-length: {}\r\n", full.len());
    }
    out.push_str("\r\n");
    let body = match resp.cut_after {
        Some(n) => &full[..n.min(full.len())],
        None => full,
    };
    if let Some((after, ms)) = resp.stall {
        let after = after.min(body.len());
        let _ = sock.write_all(out.as_bytes()).await;
        let _ = sock.write_all(&body[..after]).await;
        let _ = sock.flush().await;
        // sleep-ok: the mock's scripted stall between the response head and the rest of the body (the behaviour under test)
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        let _ = sock.write_all(&body[after..]).await;
        let _ = sock.shutdown().await;
        return;
    }
    if let Some((at, ms)) = &resp.chunks {
        let _ = sock.write_all(out.as_bytes()).await;
        let bytes = body;
        let mut from = 0;
        for &to in at.iter().chain(std::iter::once(&bytes.len())) {
            let to = to.clamp(from, bytes.len());
            let _ = sock.write_all(&bytes[from..to]).await;
            let _ = sock.flush().await;
            from = to;
            if to < bytes.len() {
                // sleep-ok: the mock's scripted pause between body chunks (the behaviour under test)
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            }
        }
        finish(sock, resp.reset).await;
        return;
    }
    let _ = sock.write_all(out.as_bytes()).await;
    let _ = sock.write_all(body).await;
    finish(sock, resp.reset).await;
}

/// Close the connection, or reset it (an abrupt end: RST instead of FIN).
async fn finish(mut sock: tokio::net::TcpStream, reset: bool) {
    if reset {
        let _ = sock.flush().await;
        let _ = sock.set_zero_linger();
        drop(sock);
    } else {
        let _ = sock.shutdown().await;
    }
}

/// SSE body of raw JSON events, each named after its `type`.
pub fn sse_events(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
}

/// Format newline-delimited JSON chunk lines into `data: ...` SSE frames.
pub fn chunks_to_sse(raw: &str, with_done: bool) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push_str("data: ");
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }
    if with_done {
        out.push_str("data: [DONE]\n\n");
    }
    out
}

/// Path helper for tests that build paths under the workspace.
pub fn p(base: &Path, rel: &str) -> String {
    base.join(rel).to_string_lossy().to_string()
}

//! Cross-platform JSON-RPC 2.0 test client for `mare --rpc`, `mare acp`, and peer sockets/pipes.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{ChildGroup, ensure_test_offline, mare_bin};

const RPC_TIMEOUT: Duration = Duration::from_secs(45);

/// Bound messages retained while a test waits for something else, as in the admission inbox.
pub const RPC_MAX_KEPT: usize = 100_000;

#[cfg(unix)]
type PeerStream = std::os::unix::net::UnixStream;

fn spawn_line_reader<R: Read + Send + 'static>(reader: R) -> Receiver<Result<Option<String>, String>> {
    let (tx, rx) = mpsc::sync_channel(RPC_MAX_KEPT);
    std::thread::spawn(move || {
        let mut buf = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match buf.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Ok(None));
                    break;
                }
                Ok(_) => {
                    if tx.send(Ok(Some(line))).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("read error: {e}")));
                    break;
                }
            }
        }
    });
    rx
}

/// Bounded queue of JSON-RPC messages already read while waiting for another frame.
#[derive(Default)]
pub struct RpcMessageBuffer {
    messages: VecDeque<Value>,
}

impl RpcMessageBuffer {
    pub fn ensure_capacity(&self, what: &str) {
        assert!(
            self.messages.len() < RPC_MAX_KEPT,
            "{what} never arrived; {} other messages kept",
            self.messages.len()
        );
    }

    pub fn push(&mut self, message: Value, what: &str) {
        self.ensure_capacity(what);
        self.messages.push_back(message);
    }

    pub fn pop(&mut self) -> Option<Value> {
        self.messages.pop_front()
    }

    pub fn take_matching(&mut self, predicate: impl FnMut(&Value) -> bool) -> Option<Value> {
        let position = self.messages.iter().position(predicate)?;
        self.messages.remove(position)
    }

    pub fn retain(&mut self, predicate: impl FnMut(&Value) -> bool) {
        self.messages.retain(predicate);
    }
}

/// Line-delimited JSON-RPC 2.0 connection over a peer socket (Unix) or named pipe (Windows).
pub struct PeerClient {
    inner: RpcClient,
    #[cfg(unix)]
    sock_writer: PeerStream,
}

impl PeerClient {
    /// Connect to `home/state/peers/<pid>.sock` (or the Windows named pipe resolved from it).
    pub fn connect(home: &Path, pid: u32) -> Self {
        let sock = home.join("state/peers").join(format!("{pid}.sock"));
        #[cfg(unix)]
        {
            let stream = connect_peer_stream(&sock);
            let writer = stream.try_clone().expect("clone peer stream");
            let sock_writer = stream.try_clone().expect("clone peer stream for shutdown");
            let mut inner = RpcClient::timed(stream, writer);
            inner.next_id = 100;
            Self { inner, sock_writer }
        }
        #[cfg(windows)]
        {
            let mut inner = connect_peer_client(&sock);
            inner.next_id = 100;
            Self { inner }
        }
    }

    /// Read one JSON-RPC message sent by the server over this peer socket.
    pub fn read(&mut self) -> Value {
        self.inner.read_deadline(Instant::now() + RPC_TIMEOUT).unwrap_or_else(|e| panic!("peer read failed: {e}"))
    }

    /// Send a JSON-RPC frame (adding `"jsonrpc": "2.0"` if absent) over this peer socket.
    pub fn send(&mut self, mut msg: Value) {
        if let Some(obj) = msg.as_object_mut() {
            obj.entry("jsonrpc").or_insert_with(|| json!("2.0"));
        }
        self.inner.write_json(&msg);
    }

    /// Send a JSON-RPC request and read until its matching response (ignoring notifications).
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.inner.request(method, params);
        loop {
            let reply = self.read();
            if reply.get("id").and_then(Value::as_u64) == Some(id) {
                return reply;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for PeerClient {
    fn drop(&mut self) {
        let _ = self.sock_writer.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(unix)]
fn connect_peer_stream(sock: &Path) -> PeerStream {
    PeerStream::connect(sock).unwrap_or_else(|e| panic!("connecting to peer socket {}: {e}", sock.display()))
}

#[cfg(windows)]
type PipeWrite = (Vec<u8>, mpsc::SyncSender<std::io::Result<()>>);

#[cfg(windows)]
struct PipeWriter(tokio::sync::mpsc::UnboundedSender<PipeWrite>);

#[cfg(windows)]
impl Write for PipeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.0.send((bytes.to_vec(), tx)).map_err(|_| std::io::ErrorKind::BrokenPipe)?;
        match rx.recv_timeout(RPC_TIMEOUT) {
            Ok(result) => result.map(|()| bytes.len()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(std::io::ErrorKind::TimedOut.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn connect_peer_client(sock: &Path) -> RpcClient {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let path = sock.to_string_lossy().into_owned();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (read_tx, read_rx) = mpsc::sync_channel(RPC_MAX_KEPT);
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<PipeWrite>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("pipe runtime");
        runtime.block_on(async move {
            let started = Instant::now();
            let stream = loop {
                match mare_binding::platform::pipe::connect(&path).await {
                    Ok(stream) => break stream,
                    // The production connector already backs off while all instances are busy.
                    Err(error) if error.raw_os_error() == Some(231) && started.elapsed() < Duration::from_secs(10) => {}
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let (reader, mut writer) = tokio::io::split(stream);
            let reading = async move {
                let mut reader = tokio::io::BufReader::new(reader);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            let _ = read_tx.send(Ok(None));
                            break;
                        }
                        Ok(_) if read_tx.send(Ok(Some(line))).is_err() => break,
                        Ok(_) => {}
                        Err(error) => {
                            let _ = read_tx.send(Err(format!("read error: {error}")));
                            break;
                        }
                    }
                }
            };
            let writing = async move {
                while let Some((bytes, done)) = write_rx.recv().await {
                    let result = match tokio::time::timeout(RPC_TIMEOUT, writer.write_all(&bytes)).await {
                        Ok(result) => result,
                        Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
                    };
                    let failed = result.is_err();
                    let _ = done.send(result);
                    if failed {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = reading => {}
                _ = writing => {}
            }
        });
    });
    ready_rx
        .recv_timeout(RPC_TIMEOUT)
        .expect("peer pipe connection timed out")
        .unwrap_or_else(|e| panic!("connecting to peer pipe {}: {e}", sock.display()));
    RpcClient::with_transport(None, WireReader::Threaded(read_rx), Box::new(PipeWriter(write_tx)), false)
}

#[test]
fn peer_client_writes_while_reading_and_disconnects() {
    use mare_binding::platform::{Ipc, IpcBackend};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let home = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    let endpoint = home.path().join("state/peers").join(format!("{pid}.sock"));
    std::fs::create_dir_all(endpoint.parent().unwrap()).unwrap();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let listener = <Ipc as IpcBackend>::bind(&endpoint, 0o600).unwrap();
            ready_tx.send(()).unwrap();
            let stream = tokio::time::timeout(Duration::from_secs(5), <Ipc as IpcBackend>::accept(&listener))
                .await
                .unwrap()
                .unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            stream.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"ready\"}\n").await.unwrap();
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line)).await.unwrap().unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "probe");
            stream.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":101,\"result\":{}}\n").await.unwrap();
            line.clear();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line)).await.unwrap().unwrap(),
                0,
                "dropping the client must close its peer connection"
            );
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let path = home.path().to_owned();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let client = std::thread::spawn(move || {
        let mut peer = PeerClient::connect(&path, pid);
        assert_eq!(peer.read()["method"], "ready");
        let response = peer.call("probe", json!({}));
        assert_eq!(response["id"], 101);
        drop(peer);
        done_tx.send(()).unwrap();
    });
    done_rx.recv_timeout(Duration::from_secs(10)).expect("peer request must not deadlock behind its reader");
    client.join().unwrap();
    server.join().unwrap();
}

/// Custom handler for server-initiated JSON-RPC notifications/requests.
/// Return `true` if handled (so default dispatch is skipped).
pub type MethodHandler = Box<dyn FnMut(&mut RpcClient, &Value) -> bool + Send>;
pub type ToolHandler = Box<dyn FnMut(&Value) -> Option<Value> + Send>;
pub type HookHandler = Box<dyn FnMut(&Value) -> Value + Send>;
pub type ApprovalHandler = Box<dyn FnMut(&Value) -> Value + Send>;

enum WireReader {
    Sync(Box<dyn BufRead + Send>),
    Threaded(Receiver<Result<Option<String>, String>>),
    Closed,
}

impl WireReader {
    fn read_optional(&mut self, enforce_jsonrpc: bool) -> std::io::Result<Option<Value>> {
        let raw = match self {
            Self::Sync(r) => {
                let mut line = String::new();
                if r.read_line(&mut line)? == 0 {
                    return Ok(None);
                }
                line
            }
            Self::Threaded(rx) => match rx.recv() {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) | Err(_) => return Ok(None),
                Ok(Err(e)) => return Err(std::io::Error::other(e)),
            },
            Self::Closed => panic!("RPC reader already closed"),
        };
        Ok(Some(parse_rpc_line(&raw, enforce_jsonrpc)))
    }
}

fn parse_rpc_line(line: &str, enforce_jsonrpc: bool) -> Value {
    let msg: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
    if enforce_jsonrpc {
        assert_eq!(msg.get("jsonrpc").and_then(Value::as_str), Some("2.0"), "{msg}");
    }
    msg
}

/// Unified JSON-RPC 2.0 test client supporting subprocesses, raw streams (sync or threaded with
/// real deadlines), auto-dispatched handlers, and raw envelope/buffer inspection.
pub struct RpcClient {
    pub child: Option<ChildGroup>,
    reader: WireReader,
    writer: Option<Box<dyn Write + Send>>,
    enforce_jsonrpc: bool,
    pub next_id: u64,
    pub session_id: Option<String>,
    kept: RpcMessageBuffer,
    /// Server requests answered automatically by [`call_raw`]/[`wait_raw`], paired with their responses.
    pub answered: Vec<(Value, Value)>,
    pub events: Vec<Value>,
    pub tool_calls: Vec<Value>,
    pub hook_calls: Vec<Value>,
    pub statuses: Vec<Value>,
    pub approvals: Vec<Value>,
    pub notices: Vec<Value>,
    pub on_tool: Option<ToolHandler>,
    pub on_hook: Option<HookHandler>,
    pub on_approval: Option<ApprovalHandler>,
    pub on_method: Option<MethodHandler>,
}

impl RpcClient {
    fn with_transport(
        child: Option<ChildGroup>,
        reader: WireReader,
        writer: Box<dyn Write + Send>,
        enforce_jsonrpc: bool,
    ) -> Self {
        Self {
            child,
            reader,
            writer: Some(writer),
            enforce_jsonrpc,
            next_id: 0,
            session_id: None,
            kept: RpcMessageBuffer::default(),
            answered: Vec::new(),
            events: Vec::new(),
            tool_calls: Vec::new(),
            hook_calls: Vec::new(),
            statuses: Vec::new(),
            approvals: Vec::new(),
            notices: Vec::new(),
            on_tool: None,
            on_hook: None,
            on_approval: None,
            on_method: None,
        }
    }

    /// Synchronous line-delimited JSON-RPC client over `BufRead` + `Write` (dropping the reader or
    /// calling [`close_reader`] closes the underlying FD immediately without a background thread).
    pub fn new(r: impl BufRead + Send + 'static, w: impl Write + Send + 'static) -> Self {
        Self::with_transport(None, WireReader::Sync(Box::new(r)), Box::new(w), false)
    }

    /// Background-thread line-delimited JSON-RPC client so [`read_deadline`], [`read_timeout`],
    /// and [`wait_timeout`] enforce real wall-clock deadlines even when the peer stalls.
    pub fn timed(r: impl Read + Send + 'static, w: impl Write + Send + 'static) -> Self {
        Self::with_transport(None, WireReader::Threaded(spawn_line_reader(r)), Box::new(w), false)
    }

    /// Spawn `mare_bin` with `args`, `env` overrides, and `cwd`.
    pub fn spawn(args: &[&str], env: &BTreeMap<String, String>, cwd: &Path) -> Self {
        Self::spawn_with_env_removed(args, env, cwd, &[])
    }

    /// Spawn with explicit removals for variables that must not be inherited.
    pub fn spawn_with_env_removed(args: &[&str], env: &BTreeMap<String, String>, cwd: &Path, removed: &[&str]) -> Self {
        ensure_test_offline();
        let mut cmd = Command::new(mare_bin());
        cmd.args(args).current_dir(cwd).env("BROWSER", "true");
        for (k, v) in env {
            cmd.env(k, v);
        }
        for name in removed {
            cmd.env_remove(name);
        }
        if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() != Ok("1") {
            cmd.env("MARE_TEST_OFFLINE", "1");
        }
        Self::spawn_cmd(&mut cmd)
    }

    /// Spawn `cmd` in an isolated [`ChildGroup`] with piped stdin/stdout and a timed reader.
    pub fn spawn_cmd(cmd: &mut Command) -> Self {
        ensure_test_offline();
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() != Ok("1") {
            cmd.env("MARE_TEST_OFFLINE", "1");
        }
        Self::from_child(ChildGroup::spawn(cmd).expect("spawn child process"))
    }

    pub fn from_child(mut child: ChildGroup) -> Self {
        let stdin = child.0.stdin.take().expect("take stdin");
        let stdout = child.0.stdout.take().expect("take stdout");
        Self::with_transport(Some(child), WireReader::Threaded(spawn_line_reader(stdout)), Box::new(stdin), true)
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("RpcClient has no Child").id()
    }

    fn write_json(&mut self, value: &Value) {
        let line = serde_json::to_string(value).expect("serialize RPC message");
        let writer = self.writer.as_mut().expect("RPC writer already closed");
        writeln!(writer, "{line}").expect("write to RPC stream");
        writer.flush().expect("flush RPC stream");
    }

    /// Write a raw pre-formatted line and flush immediately (for testing invalid JSON / literal tokens).
    pub fn write_raw_line(&mut self, line: &str) {
        let writer = self.writer.as_mut().expect("RPC writer already closed");
        writeln!(writer, "{line}").expect("write raw line to RPC stream");
        writer.flush().expect("flush RPC stream");
    }

    pub fn send(&mut self, mut msg: Value) {
        let obj = msg.as_object_mut().expect("RPC message must be a JSON object");
        obj.insert("jsonrpc".to_string(), json!("2.0"));
        self.write_json(&msg);
    }

    pub fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "id": id, "method": method, "params": params }));
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "method": method, "params": params }));
    }

    pub fn tool_update(&mut self, request_id: &Value, partial: Value) {
        self.notify("callback.update", json!({ "id": request_id, "partial": partial }));
    }

    pub fn read_optional(&mut self) -> std::io::Result<Option<Value>> {
        if let Some(message) = self.kept.pop() {
            return Ok(Some(message));
        }
        self.reader.read_optional(self.enforce_jsonrpc)
    }

    pub fn read(&mut self) -> Value {
        if self.child.is_some() {
            return self.read_timeout(RPC_TIMEOUT).unwrap_or_else(|| {
                self.kill_and_wait();
                panic!("timed out after {}s waiting for mare --rpc stdout line", RPC_TIMEOUT.as_secs());
            });
        }
        match self.reader {
            WireReader::Sync(_) => self.read_optional().unwrap().expect("connection closed"),
            WireReader::Threaded(_) => {
                self.read_deadline(Instant::now() + RPC_TIMEOUT).unwrap_or_else(|e| panic!("{e}"))
            }
            WireReader::Closed => panic!("RPC reader already closed"),
        }
    }

    pub fn read_timeout(&mut self, timeout: Duration) -> Option<Value> {
        if let Some(message) = self.kept.pop() {
            return Some(message);
        }
        let line = match &self.reader {
            WireReader::Threaded(rx) => match rx.recv_timeout(timeout) {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => {
                    self.kill_and_wait();
                    panic!("mare closed stdout");
                }
                Ok(Err(e)) => {
                    self.kill_and_wait();
                    panic!("read mare stdout: {e}");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.kill_and_wait();
                    panic!("mare stdout reader disconnected");
                }
            },
            WireReader::Sync(_) => panic!("read_timeout requires RpcClient::timed or RpcClient::spawn"),
            WireReader::Closed => panic!("RPC reader already closed"),
        };
        Some(parse_rpc_line(&line, self.enforce_jsonrpc))
    }

    fn read_stream_deadline(&mut self, deadline: Instant) -> Result<Value, String> {
        let rem = deadline.saturating_duration_since(Instant::now());
        let line = match &self.reader {
            WireReader::Threaded(rx) => match rx.recv_timeout(rem) {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("connection closed (EOF)".to_string());
                }
                Ok(Err(e)) => return Err(e),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("timed out waiting for RPC line".to_string()),
            },
            WireReader::Sync(_) => {
                return Err("read_deadline requires RpcClient::timed or RpcClient::spawn".to_string());
            }
            WireReader::Closed => return Err("RPC reader already closed".to_string()),
        };
        let msg = serde_json::from_str::<Value>(&line).map_err(|e| format!("not JSON ({e}): {line}"))?;
        if self.enforce_jsonrpc && msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(format!("missing jsonrpc 2.0: {msg}"));
        }
        Ok(msg)
    }

    /// Read next buffered or wire frame, returning `Err` if `deadline` elapses, EOF occurs, or
    /// invalid JSON is received.
    pub fn read_deadline(&mut self, deadline: Instant) -> Result<Value, String> {
        if let Some(message) = self.kept.pop() {
            return Ok(message);
        }
        self.read_stream_deadline(deadline)
    }

    /// Drop the read side immediately (for output-loss disconnect tests).
    pub fn close_reader(&mut self) {
        self.reader = WireReader::Closed;
    }

    /// Close stdin/writer without sending `shutdown` (for disconnect-mid-callback tests).
    pub fn close_stdin(&mut self) {
        self.writer.take();
    }

    /// Close writer and return the synchronous reader, if backed by [`RpcClient::new`].
    pub fn into_reader(mut self) -> Option<Box<dyn BufRead + Send>> {
        self.writer.take();
        match std::mem::replace(&mut self.reader, WireReader::Closed) {
            WireReader::Sync(r) => Some(r),
            WireReader::Threaded(_) | WireReader::Closed => None,
        }
    }

    /// Close writer and drain all buffered and remaining wire messages until EOF.
    pub fn into_messages(mut self) -> impl Iterator<Item = Value> {
        self.writer.take();
        let mut kept = std::mem::take(&mut self.kept);
        let mut reader = std::mem::replace(&mut self.reader, WireReader::Closed);
        let _child = self.child.take();
        let enforce = self.enforce_jsonrpc;
        std::iter::from_fn(move || {
            let _keep_alive = &_child;
            if let Some(msg) = kept.pop() {
                return Some(msg);
            }
            reader.read_optional(enforce).unwrap()
        })
    }

    /// Close writer, drain remaining wire messages up to [`RPC_TIMEOUT`], and verify clean child exit.
    pub fn drain_and_wait(mut self) -> Vec<Value> {
        self.writer.take();
        let mut out = Vec::new();
        while let Some(msg) = self.kept.pop() {
            out.push(msg);
        }
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            match &self.reader {
                WireReader::Threaded(_) => match self.read_stream_deadline(deadline) {
                    Ok(msg) => out.push(msg),
                    Err(e) if e == "connection closed (EOF)" => break,
                    Err(e) => {
                        self.kill_and_wait();
                        panic!("drain_and_wait failed: {e}");
                    }
                },
                WireReader::Sync(_) => match self.reader.read_optional(self.enforce_jsonrpc).unwrap() {
                    Some(msg) => out.push(msg),
                    None => break,
                },
                WireReader::Closed => break,
            }
        }
        self.wait_exit();
        out
    }

    pub fn dispatch(&mut self, msg: Value) -> Option<Value> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Some(msg);
        };
        if let Some(mut handler) = self.on_method.take() {
            let handled = handler(self, &msg);
            if self.on_method.is_none() {
                self.on_method = Some(handler);
            }
            if handled {
                return None;
            }
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "event" => {
                if params.get("type").and_then(Value::as_str) == Some("status/changed")
                    && let Some(data) = params.get("data")
                {
                    self.statuses.push(data.clone());
                }
                self.events.push(params);
            }
            "ui.notify" => self.notices.push(params),
            "callback.call" => {
                if params.get("callback").and_then(Value::as_str) == Some("execute") {
                    let arguments = params["arguments"].clone();
                    self.tool_calls.push(arguments.clone());
                    let synthetic = json!({
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "method": "callback.call",
                        "params": arguments,
                    });
                    let result = if let Some(cb) = self.on_tool.as_mut() {
                        cb(&synthetic)
                    } else {
                        Some(json!({ "content": "ok" }))
                    };
                    if let Some(result) = result {
                        self.send(json!({ "id": msg["id"], "result": result }));
                    }
                } else {
                    let arguments = params["arguments"].clone();
                    self.hook_calls.push(arguments.clone());
                    let call = &arguments["args"][0];
                    let value = if let Some(cb) = self.on_hook.as_mut() {
                        cb(&arguments)
                    } else if call.get("name").and_then(Value::as_str) == Some("bash") {
                        json!({ "decision": "deny", "reason": "denied by host" })
                    } else {
                        Value::Null
                    };
                    self.send(json!({ "id": msg["id"], "result": { "value": value } }));
                }
            }
            "approval.request" => {
                self.approvals.push(params.clone());
                let decision = if let Some(cb) = self.on_approval.as_mut() { cb(&params) } else { json!("deny") };
                let result = if decision.is_object() { decision } else { json!({ "decision": decision }) };
                self.send(json!({ "id": msg["id"], "result": result }));
            }
            "ui.select" | "ui.input" | "ui.confirm" => {
                self.send(json!({ "id": msg["id"], "result": {} }));
            }
            _ => {}
        }
        None
    }

    fn read_before(&mut self, deadline: Instant, what: &str) -> Value {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            self.kill_and_wait();
            panic!("timed out after {}s waiting for {what}", RPC_TIMEOUT.as_secs());
        }
        self.read_timeout(rem).unwrap_or_else(|| {
            self.kill_and_wait();
            panic!("timed out after {}s waiting for {what}", RPC_TIMEOUT.as_secs());
        })
    }

    pub fn wait(&mut self, rid: u64) -> Result<Value, Value> {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let msg = if let Some(m) = self.kept.pop() {
                m
            } else {
                self.read_before(deadline, &format!("rpc response id={rid}"))
            };
            if let Some(resp) = self.dispatch(msg)
                && resp.get("id").and_then(Value::as_u64) == Some(rid)
            {
                if let Some(err) = resp.get("error") {
                    assert!(resp.get("result").is_none(), "error response must not contain result: {resp}");
                    return Err(err.clone());
                }
                assert!(resp.get("result").is_some(), "response must contain result or error: {resp}");
                return Ok(resp["result"].clone());
            }
        }
    }

    pub fn session_id(&self) -> String {
        self.session_id.clone().expect("initialize must return session_id before session-scoped calls")
    }

    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let rid = self.request(method, params);
        let res = self.wait(rid).unwrap_or_else(|err| panic!("rpc error on {method}: {err}"));
        if method == "initialize"
            && let Some(sid) = res.get("session").and_then(|s| s.get("session_id")).and_then(Value::as_str)
        {
            self.session_id = Some(sid.to_owned());
        }
        res
    }

    pub fn call_err(&mut self, method: &str, params: Value) -> Value {
        let rid = self.request(method, params);
        self.wait(rid).expect_err(&format!("expected rpc error on {method}"))
    }

    pub fn wait_status(&mut self, state: &str) {
        let deadline = Instant::now() + RPC_TIMEOUT;
        while self.statuses.last().and_then(|s| s.get("state")).and_then(Value::as_str) != Some(state) {
            let msg = self.read_before(deadline, &format!("status {state}"));
            self.dispatch(msg);
        }
    }

    pub fn wait_event(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            if let Some(pos) = self.events.iter().position(|e| e.get("type").and_then(Value::as_str) == Some(kind)) {
                return self.events.remove(pos);
            }
            let msg = self.read_before(deadline, &format!("event {kind}"));
            self.dispatch(msg);
        }
    }

    fn dispatch_waiting(
        &mut self,
        message: Value,
        id: u64,
        on_call: &mut Option<&mut dyn FnMut(&Value) -> Value>,
    ) -> Option<Value> {
        if message.get("method").is_none() && message["id"] == id {
            return Some(message);
        }
        if message.get("method").is_some()
            && let Some(callback_id) = message.get("id")
            && let Some(on_call) = on_call.as_mut()
        {
            assert!(self.answered.len() < RPC_MAX_KEPT, "the reply to {id} never arrived; too many requests answered");
            let result = on_call(&message);
            let response = json!({ "jsonrpc": "2.0", "id": callback_id, "result": result });
            self.write_json(&response);
            self.answered.push((message, response));
        } else {
            self.kept.push(message, &format!("the reply to {id}"));
        }
        None
    }

    fn wait_raw_inner(&mut self, id: u64, mut on_call: Option<&mut dyn FnMut(&Value) -> Value>) -> Value {
        if let Some(reply) = self.kept.take_matching(|m| m.get("method").is_none() && m["id"] == id) {
            return reply;
        }
        loop {
            let message = match &self.reader {
                WireReader::Sync(_) => {
                    self.reader.read_optional(self.enforce_jsonrpc).unwrap().expect("connection closed")
                }
                WireReader::Threaded(_) => {
                    let dl = Instant::now() + RPC_TIMEOUT;
                    self.read_stream_deadline(dl).unwrap_or_else(|e| panic!("waiting for reply {id}: {e}"))
                }
                WireReader::Closed => panic!("RPC reader already closed"),
            };
            if let Some(reply) = self.dispatch_waiting(message, id, &mut on_call) {
                return reply;
            }
        }
    }

    /// Wait until response `id` arrives and return the full JSON-RPC envelope; `on_call` answers
    /// server-initiated requests meanwhile while non-matching messages remain buffered.
    pub fn wait_raw(&mut self, id: u64, mut on_call: impl FnMut(&Value) -> Value) -> Value {
        self.wait_raw_inner(id, Some(&mut on_call))
    }

    /// Send `method` and wait for its full JSON-RPC envelope, answering server requests via `on_call`.
    pub fn call_raw(&mut self, method: &str, params: Value, on_call: impl FnMut(&Value) -> Value) -> Value {
        let id = self.request(method, params);
        let reply = self.wait_raw(id, on_call);
        if method == "initialize"
            && let Some(sid) = reply
                .get("result")
                .and_then(|r| r.get("session"))
                .and_then(|s| s.get("session_id"))
                .and_then(Value::as_str)
        {
            self.session_id = Some(sid.to_owned());
        }
        reply
    }

    /// Wait until response `id` arrives while holding any server requests in the message buffer.
    pub fn wait_holding(&mut self, id: u64) -> Value {
        self.wait_raw_inner(id, None)
    }

    /// Send `method` and wait for its full JSON-RPC envelope while holding server requests in the buffer.
    pub fn call_holding(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params);
        self.wait_holding(id)
    }

    fn wait_timeout_inner(
        &mut self,
        id: u64,
        timeout: Duration,
        mut on_call: Option<&mut dyn FnMut(&Value) -> Value>,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        if let Some(reply) = self.kept.take_matching(|m| m.get("method").is_none() && m["id"] == id) {
            return Ok(reply);
        }
        loop {
            let message = self.read_stream_deadline(deadline)?;
            if let Some(reply) = self.dispatch_waiting(message, id, &mut on_call) {
                return Ok(reply);
            }
        }
    }

    pub fn wait_timeout(
        &mut self,
        id: u64,
        timeout: Duration,
        mut on_call: impl FnMut(&Value) -> Value,
    ) -> Result<Value, String> {
        self.wait_timeout_inner(id, timeout, Some(&mut on_call))
    }

    pub fn wait_holding_timeout(&mut self, id: u64, timeout: Duration) -> Result<Value, String> {
        self.wait_timeout_inner(id, timeout, None)
    }

    /// Close stdin and wait up to [`RPC_TIMEOUT`] for the process to exit, returning its status.
    pub fn wait_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.writer.take();
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let status = {
                let child = self.child.as_mut()?;
                child.0.try_wait().expect("try_wait child process")
            };
            match status {
                Some(status) => {
                    drop(self.child.take());
                    return Some(status);
                }
                None if Instant::now() < deadline => {
                    // sleep-ok: bounded poll waiting for child exit after RPC shutdown
                    std::thread::sleep(Duration::from_millis(20));
                }
                None => {
                    self.kill_and_wait();
                    panic!("timed out after {}s waiting for mare --rpc to exit", RPC_TIMEOUT.as_secs());
                }
            }
        }
    }

    /// Close stdin and wait up to [`RPC_TIMEOUT`] for the process to exit cleanly with status 0.
    pub fn wait_exit(&mut self) {
        if let Some(status) = self.wait_exit_status() {
            assert!(status.success(), "mare exited with {status}");
        }
    }

    /// Send `shutdown`, close stdin, and wait up to [`RPC_TIMEOUT`] for exit status 0.
    pub fn shutdown_and_wait(&mut self) {
        self.call("shutdown", json!({}));
        self.wait_exit();
    }

    /// Close stdin/stdout and kill the process tree (for crash-recovery and timeout cleanup).
    pub fn kill_and_wait(&mut self) {
        drop(self.child.take());
        self.writer.take();
        self.reader = WireReader::Closed;
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        drop(self.child.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn written(&self) -> Vec<Value> {
            let guard = self.0.lock().unwrap();
            std::str::from_utf8(&guard).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
        }
    }

    fn scripted(messages: &[Value]) -> Cursor<Vec<u8>> {
        let lines = messages.iter().map(|message| format!("{message}\n")).collect::<String>();
        Cursor::new(lines.into_bytes())
    }

    #[test]
    fn rpc_client_retains_notification_before_reply() {
        let notification = json!({"jsonrpc":"2.0", "method":"event", "params":{"type":"early"}});
        let next = json!({"jsonrpc":"2.0", "method":"event", "params":{"type":"next"}});
        let reply = json!({"jsonrpc":"2.0", "id":1, "result":"done"});
        for mut rpc in [
            RpcClient::new(scripted(&[notification.clone(), next.clone(), reply.clone()]), SharedBuf::default()),
            RpcClient::timed(scripted(&[notification.clone(), next.clone(), reply.clone()]), SharedBuf::default()),
        ] {
            assert_eq!(rpc.call_raw("go", json!({}), |_| panic!("unexpected request")), reply);
            assert_eq!(rpc.read(), notification);
            assert_eq!(rpc.read(), next);
        }
    }

    #[test]
    fn rpc_client_retains_other_reply_before_awaited_reply() {
        let other = json!({"jsonrpc":"2.0", "id":2, "result":"other"});
        let awaited = json!({"jsonrpc":"2.0", "id":1, "result":"done"});
        let mut sync_rpc = RpcClient::new(scripted(&[other.clone(), awaited.clone()]), SharedBuf::default());
        assert_eq!(sync_rpc.wait_raw(1, |_| panic!("unexpected request")), awaited);
        assert_eq!(sync_rpc.wait_raw(2, |_| panic!("unexpected request")), other);

        let mut timed_rpc = RpcClient::timed(scripted(&[other.clone(), awaited.clone()]), SharedBuf::default());
        assert_eq!(
            timed_rpc.wait_timeout(1, Duration::from_secs(1), |_| panic!("unexpected request")).unwrap(),
            awaited
        );
        assert_eq!(timed_rpc.wait_timeout(2, Duration::from_secs(1), |_| panic!("unexpected request")).unwrap(), other);
    }

    #[test]
    fn rpc_client_records_the_request_and_automatic_answer() {
        let request = json!({"jsonrpc":"2.0", "id":"callback", "method":"hook.call"});
        let response = json!({"jsonrpc":"2.0", "id":"callback", "result":{"decision":"allow"}});
        let reply = json!({"jsonrpc":"2.0", "id":1, "result":"done"});
        for timed in [false, true] {
            let out = SharedBuf::default();
            let src = scripted(&[request.clone(), reply.clone()]);
            let mut rpc = if timed { RpcClient::timed(src, out.clone()) } else { RpcClient::new(src, out.clone()) };
            assert_eq!(rpc.call_raw("go", json!({}), |_| json!({"decision":"allow"})), reply);
            assert_eq!(out.written()[1], response);
            assert_eq!(rpc.answered, vec![(request.clone(), response.clone())]);
        }
    }

    #[test]
    fn rpc_client_holding_preserves_the_request_without_answering() {
        let request = json!({"jsonrpc":"2.0", "id":"callback", "method":"hook.call"});
        let reply = json!({"jsonrpc":"2.0", "id":1, "result":"done"});
        let later_reply = json!({"jsonrpc":"2.0", "id":2, "result":"later"});

        let out = SharedBuf::default();
        let mut sync_rpc =
            RpcClient::new(scripted(&[request.clone(), reply.clone(), later_reply.clone()]), out.clone());
        assert_eq!(sync_rpc.call_holding("go", json!({})), reply);
        assert_eq!(sync_rpc.wait_raw(2, |_| panic!("held request must stay unanswered")), later_reply);
        assert_eq!(out.written(), vec![json!({"jsonrpc":"2.0", "id":1, "method":"go", "params":{}})]);
        assert!(sync_rpc.answered.is_empty());
        assert_eq!(sync_rpc.read(), request);
        let response = json!({"jsonrpc":"2.0", "id":"callback", "result":"manual"});
        sync_rpc.send(response.clone());
        assert_eq!(out.written()[1], response);

        let out2 = SharedBuf::default();
        let mut timed_rpc = RpcClient::timed(scripted(&[request.clone(), reply, later_reply.clone()]), out2.clone());
        assert_eq!(timed_rpc.call_holding("go", json!({})), json!({"jsonrpc":"2.0", "id":1, "result":"done"}));
        assert_eq!(
            timed_rpc.wait_timeout(2, Duration::from_secs(1), |_| panic!("held request must stay unanswered")).unwrap(),
            later_reply
        );
        assert_eq!(timed_rpc.read_deadline(Instant::now() + Duration::from_secs(1)).unwrap(), request);
        timed_rpc.send(response.clone());
        assert_eq!(out2.written()[1], response);
    }

    #[test]
    fn rpc_client_drain_reads_kept_messages_before_the_stream() {
        let early = json!({"jsonrpc":"2.0", "method":"event", "params":{"type":"early"}});
        let reply = json!({"jsonrpc":"2.0", "id":1, "result":"done"});
        let late = json!({"jsonrpc":"2.0", "method":"event", "params":{"type":"late"}});
        let mut rpc = RpcClient::new(scripted(&[early.clone(), reply, late.clone()]), Vec::new());
        rpc.call_raw("go", json!({}), |_| panic!("unexpected request"));
        assert_eq!(rpc.into_messages().collect::<Vec<_>>(), vec![early, late]);
    }

    #[test]
    #[should_panic(expected = "the reply to 1")]
    fn rpc_client_sync_bound_names_the_reply_being_awaited() {
        let chatter = format!("{}\n", json!({"method":"event"})).repeat(100_001);
        let mut rpc = RpcClient::new(Cursor::new(chatter.into_bytes()), Vec::new());
        rpc.wait_raw(1, |_| panic!("unexpected request"));
    }

    #[test]
    #[should_panic(expected = "the reply to 1")]
    fn rpc_client_timed_bound_names_the_reply_being_awaited() {
        let chatter = format!("{}\n", json!({"method":"event"})).repeat(100_001);
        let mut rpc = RpcClient::timed(Cursor::new(chatter.into_bytes()), Vec::new());
        rpc.wait_timeout(1, Duration::from_secs(30), |_| panic!("unexpected request")).unwrap();
    }

    #[cfg(unix)]
    fn event_and_reply_in_order(event_first: bool) {
        let event = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"item/started"}}"#;
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"status":"started"}}"#;
        let (first, second) = if event_first { (event, reply) } else { (reply, event) };
        let script = format!("IFS= read -r request; printf '%s\\n' '{first}' '{second}'");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]).stdin(Stdio::piped()).stdout(Stdio::piped());
        let child = ChildGroup::spawn(&mut cmd).unwrap();
        let mut client = RpcClient::from_child(child);
        assert_eq!(client.call("agent.prompt", json!({}))["status"], "started");
        assert_eq!(client.wait_event("item/started")["type"], "item/started");
        client.wait_exit();
    }

    #[cfg(unix)]
    #[test]
    fn web_rpc_event_before_prompt_reply_is_retained() {
        event_and_reply_in_order(true);
    }

    #[cfg(unix)]
    #[test]
    fn web_rpc_event_after_prompt_reply_is_read() {
        event_and_reply_in_order(false);
    }
}

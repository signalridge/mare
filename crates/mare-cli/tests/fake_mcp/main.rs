//! `mare-test-mcp`: the scripted MCP servers the integration tests run (`tests/suite/mcp*`,
//! `tests/suite/plugins.rs`), in Rust so they run wherever the tests do (no Python). Test-only:
//! the release packages ship `mare` alone.
//!//! `mare-test-mcp scripted <json config>` — the configurable stdio server (`scripted.rs`)
//! `mare-test-mcp simple` — a stdio server with one `echo` tool
//! `mare-test-mcp tiered <log> <delay secs> [release file]` — logs each start, waits `delay` (and
//! until the release file exists, 60 s cap) before answering `initialize`; `echo` and `shout`
//! `mare-test-mcp http <headers file> <port file>` — Streamable HTTP answering JSON (`http.rs`)
//!//! Also the stdin sinks the hook tests run as their external program (`suite::status`), so they
//! need no POSIX shell:
//! `mare-test-mcp record <file>` — appends `$MARE_STATE`, a tab, stdin and a newline
//! `mare-test-mcp sink <file> [delay ms]` — waits, then writes stdin to the file
//!//! And a small process tree for the shutdown tests (`suite::shutdown_trees`):
//! `mare-test-mcp tree <pid file>` — starts a `sleep` grandchild that inherits its stdio, writes
//! the grandchild's pid to the file, then waits for it
//! `mare-test-mcp sleep` — sleeps ten minutes

mod http;
mod scripted;

use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scripted") => scripted::run(args.get(1).map_or("{}", String::as_str)),
        Some("simple") => simple(),
        Some("tiered") => tiered(&args[1..]),
        Some("http") => http::run(&args[1], &args[2]),
        Some("record") => record(&args[1]),
        Some("tree") => {
            let mut c = grandchild(&args[1]);
            let _ = c.wait();
        }
        Some("sleep") => std::thread::sleep(std::time::Duration::from_secs(600)),
        Some("sink") => sink(&args[1], args.get(2).map_or(Ok(0), |d| d.parse()).unwrap_or_else(|_| die("delay"))),
        _ => die(&format!("usage: mare-test-mcp scripted|simple|tiered|http|record|sink|tree|sleep …, got {args:?}")),
    }
}

/// An uncaught error: what a Python server does on a malformed message (traceback, exit 1).
pub fn die(msg: &str) -> ! {
    eprintln!("mare-test-mcp: {msg}");
    std::process::exit(1)
}

/// One JSON-RPC message per stdin line, until EOF.
pub fn messages() -> impl Iterator<Item = Value> {
    std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .map(|l| serde_json::from_str::<Value>(&l).unwrap_or_else(|e| die(&format!("bad message {l:?}: {e}"))))
}

/// Requests carry an `id` (even a null one); notifications don't.
pub fn is_request(m: &Value) -> bool {
    m.as_object().is_some_and(|o| o.contains_key("id"))
}

pub fn method(m: &Value) -> &str {
    m.get("method").and_then(Value::as_str).unwrap_or_else(|| die("a request without a method"))
}

/// Start `mare-test-mcp sleep` with this process's stdio (it holds the output pipes too) and
/// write its pid to `pid_file` (complete once the file ends in a newline).
pub fn grandchild(pid_file: &str) -> std::process::Child {
    let exe = std::env::current_exe().unwrap_or_else(|e| die(&format!("current_exe: {e}")));
    let c = std::process::Command::new(exe).arg("sleep").spawn().unwrap_or_else(|e| die(&format!("grandchild: {e}")));
    std::fs::write(pid_file, format!("{}\n", c.id())).unwrap_or_else(|e| die(&format!("writing {pid_file}: {e}")));
    c
}

/// Write one line to stdout and flush it.
pub fn print_line(m: &Value) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{m}").and_then(|()| out.flush()).is_err() {
        die("stdout closed");
    }
}

/// The JSON-RPC answer: `result`, or method-not-found when there is none.
pub fn reply(id: &Value, r: Option<Value>) -> Value {
    match r {
        Some(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        None => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } }),
    }
}

pub fn append(path: &str, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| die(&format!("opening {path}: {e}")));
    let _ = f.write_all(format!("{line}\n").as_bytes());
}

/// `arguments["text"]` as a string: missing is `default` (or an error when there is none), any
/// other type is an error (Python's string concatenation would raise).
fn text_arg(m: &Value, default: Option<&str>) -> String {
    match (m["params"]["arguments"].get("text"), default) {
        (Some(Value::String(s)), _) => s.clone(),
        (None, Some(d)) => d.to_string(),
        (other, _) => die(&format!("text argument {other:?}")),
    }
}

fn echo_schema() -> Value {
    json!({ "type": "object", "properties": { "text": { "type": "string" } } })
}

fn simple() {
    for m in messages().filter(is_request) {
        let r = match method(&m) {
            "initialize" => Some(json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} },
                                         "serverInfo": { "name": "fake", "version": "1" } })),
            "tools/list" => {
                Some(json!({ "tools": [{ "name": "echo", "description": "echo back", "inputSchema": echo_schema() }] }))
            }
            "tools/call" => {
                Some(json!({ "content": [{ "type": "text", "text": format!("echo: {}", text_arg(&m, None)) }] }))
            }
            _ => None,
        };
        print_line(&reply(&m["id"], r));
    }
}

fn tiered(args: &[String]) {
    let log = &args[0];
    append(log, "start");
    let delay: f64 = args[1].parse().unwrap_or_else(|_| die("delay"));
    let release = args.get(2).map(std::path::PathBuf::from);
    for m in messages().filter(is_request) {
        let r = match method(&m) {
            "initialize" => {
                std::thread::sleep(std::time::Duration::from_secs_f64(delay));
                // parked until the test creates the release file (60 s cap, so a hang ends)
                for _ in 0..3000 {
                    if release.as_ref().is_none_or(|r| r.exists()) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Some(json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} },
                             "serverInfo": { "name": "fake", "version": "1" } }))
            }
            "tools/list" => Some(json!({ "tools": [
                { "name": "echo", "description": "echo text back", "inputSchema": echo_schema() },
                { "name": "shout", "description": "upper-case text", "inputSchema": { "type": "object" } }] })),
            "tools/call" => {
                Some(json!({ "content": [{ "type": "text", "text": format!("echo: {}", text_arg(&m, Some(""))) }] }))
            }
            _ => None,
        };
        print_line(&reply(&m["id"], r));
    }
}

fn stdin_text() -> String {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).unwrap_or_else(|e| die(&format!("stdin: {e}")));
    s
}

/// `printf "%s\t" "$MARE_STATE" >> file; cat >> file; echo >> file`
fn record(path: &str) {
    let state = std::env::var("MARE_STATE").unwrap_or_default();
    let text = stdin_text();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| die(&format!("opening {path}: {e}")));
    // one write: a run is one line even if another run appends concurrently
    f.write_all(format!("{state}\t{text}\n").as_bytes()).unwrap_or_else(|e| die(&format!("writing {path}: {e}")));
}

/// `sleep <delay>; cat > file`
fn sink(path: &str, delay_ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    let text = stdin_text();
    std::fs::write(path, text).unwrap_or_else(|e| die(&format!("writing {path}: {e}")));
}

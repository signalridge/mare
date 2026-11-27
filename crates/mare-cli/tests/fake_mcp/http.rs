//! A Streamable HTTP MCP server answering JSON (no SSE), one HTTP/1.0 exchange per connection:
//! appends each POST's `x-team` header to the headers file, writes its port to the port file,
//! and has one `add` tool.

use crate::{append, die};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};

pub fn run(headers_file: &str, port_file: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| die(&format!("bind: {e}")));
    let port = listener.local_addr().unwrap_or_else(|e| die(&format!("local_addr: {e}"))).port();
    std::fs::write(port_file, port.to_string()).unwrap_or_else(|e| die(&format!("port file: {e}")));
    for stream in listener.incoming().map_while(Result::ok) {
        let headers_file = headers_file.to_string();
        std::thread::spawn(move || {
            let _ = handle(stream, &headers_file);
        });
    }
}

fn respond(mut s: &TcpStream, status: &str, headers: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.0 {status}\r\n");
    for (k, v) in headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()?;
    s.shutdown(Shutdown::Write)
}

fn handle(stream: TcpStream, headers_file: &str) -> std::io::Result<()> {
    let mut r = BufReader::new(&stream);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let verb = line.split_whitespace().next().unwrap_or_default().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    match verb.as_str() {
        "GET" => return respond(&stream, "405 Method Not Allowed", &[], b""),
        "DELETE" => return respond(&stream, "200 OK", &[], b""),
        "POST" => {}
        _ => return respond(&stream, "501 Unsupported method", &[], b""),
    }
    append(headers_file, &header("x-team").unwrap_or_default());
    let len: usize = header("content-length").and_then(|l| l.parse().ok()).unwrap_or_else(|| die("content-length"));
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    let m: Value = serde_json::from_slice(&body).unwrap_or_else(|e| die(&format!("body: {e}")));
    if !crate::is_request(&m) {
        return respond(&stream, "202 Accepted", &[], b"");
    }
    let p = &m["params"];
    let result = match crate::method(&m) {
        "initialize" => json!({ "protocolVersion": p["protocolVersion"], "capabilities": { "tools": {} },
                                "serverInfo": { "name": "http-fake", "version": "1" } }),
        "tools/list" => json!({ "tools": [{ "name": "add", "description": "add numbers", "inputSchema": {
            "type": "object", "properties": { "a": { "type": "number" }, "b": { "type": "number" } } } }] }),
        "tools/call" => {
            let a = &p["arguments"];
            json!({ "content": [{ "type": "text", "text": py_sum(&a["a"], &a["b"]) }] })
        }
        _ => json!({}),
    };
    let body = json!({ "jsonrpc": "2.0", "id": m["id"], "result": result }).to_string();
    respond(
        &stream,
        "200 OK",
        &[
            ("content-type", "application/json".into()),
            ("mcp-session-id", "s1".into()),
            ("content-length", body.len().to_string()),
        ],
        body.as_bytes(),
    )
}

/// `str(a + b)` in Python: integers stay integers, a float sum prints with a decimal point.
fn py_sum(a: &Value, b: &Value) -> String {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return (x + y).to_string();
    }
    let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) else { die(&format!("add {a} {b}")) };
    let s = x + y;
    if s.fract() == 0.0 && s.is_finite() { format!("{s:.1}") } else { s.to_string() }
}

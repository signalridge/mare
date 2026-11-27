//! `mare.net.unix` (client and server) and the public `ui/wait-start` / `ui/wait-end` events.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use crate::common::*;

/// HTTP server on 127.0.0.1: `/redirect` answers 302 to `location`, anything else 200 "ok";
/// records each request's Host header.
pub(super) fn http_server(location: String) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::Read;
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let hosts = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let seen = hosts.clone();
    std::thread::spawn(move || {
        for mut s in srv.incoming().flatten() {
            let mut b = [0u8; 4096];
            let n = s.read(&mut b).unwrap_or(0);
            let req = String::from_utf8_lossy(&b[..n]).to_string();
            let host = req.lines().find_map(|l| l.strip_prefix("host: ").or(l.strip_prefix("Host: "))).unwrap_or("");
            seen.lock().unwrap().push(host.to_string());
            let resp = if req.starts_with("GET /redirect") {
                format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            } else {
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_string()
            };
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (port, hosts)
}

pub(super) fn fake_dns(names: &[&str]) {
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    mare_binding::netauth::set_resolver_override(Some(names.iter().map(|n| (n.to_string(), vec![ip])).collect()));
}

/// One CONNECT through the sandbox proxy (blocking, in a thread): returns the proxy's status line
/// and, on success, what the upstream answered after `inner` was sent through the tunnel.
pub(super) fn connect_through(proxy: u16, target: &str, inner: &str) -> (String, String) {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", proxy)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    write!(s, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut status = String::new();
    r.read_line(&mut status).unwrap();
    if !status.contains(" 200 ") {
        return (status, String::new());
    }
    loop {
        let mut l = String::new();
        if r.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
            break;
        }
    }
    s.write_all(inner.as_bytes()).unwrap();
    let mut body = String::new();
    let _ = r.read_to_string(&mut body);
    (status, body)
}

/// A minimal upstream HTTP proxy: accepts one CONNECT, records its request line, tunnels to
/// 127.0.0.1:<the requested port>.
pub(super) fn upstream_proxy() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for c in srv.incoming() {
            let Ok(mut c) = c else { break };
            let s2 = s2.clone();
            std::thread::spawn(move || {
                let mut head = vec![];
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let line = text.lines().next().unwrap_or("").to_string();
                s2.lock().unwrap().push(line.clone());
                let target_port: u16 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|t| t.rsplit(':').next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                let Ok(mut t) = std::net::TcpStream::connect(("127.0.0.1", target_port)) else { return };
                c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
                let (mut c2, mut t2) = (c.try_clone().unwrap(), t.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut c2, &mut t2);
                });
                let _ = std::io::copy(&mut t, &mut c);
                let _ = c.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    (port, seen)
}

/// A minimal local HTTP forward proxy: records the first request line, replies `200 OK` ("ok").
pub(super) fn recording_http_proxy() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for c in srv.incoming() {
            let Ok(mut c) = c else { break };
            let s3 = s2.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) > 0 {
                    s3.lock().unwrap().push(line.trim().to_string());
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            });
        }
    });
    (port, seen)
}

mod http;
mod offline;
mod proxy;
mod proxy_options;
mod unix;

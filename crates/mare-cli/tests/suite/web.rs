//! Public web provider registry: selection, disposal, inflight cancellation, tools, and RPC parity.
use std::process::Stdio;

use serde_json::json;

use crate::common::*;

const LUA_SELECTED_ID: &str = "local function selected_id(res) for _, p in ipairs(res.data) do if p.selected then return p.provider_id end end return res.reason end";

async fn call_service_confined(
    h: &mare_host::Host,
    service: &str,
    method: &str,
    args: mlua::MultiValue,
) -> anyhow::Result<mlua::MultiValue> {
    let mlua::Value::Table(t) =
        h.kernel.service(service).ok_or_else(|| anyhow::anyhow!("missing service {service}"))?
    else {
        anyhow::bail!("service {service} is not a table");
    };
    let f: mlua::Function = t.get(method)?;
    let mut a = mlua::MultiValue::new();
    a.push_back(mlua::Value::Table(t));
    a.extend(args);
    Ok(mare_binding::exec_ctx::run_host_query(h.lua(), &h.cwd, f, a).await?)
}

fn resp_with(status: u16, content_type: &str, body: impl Into<String>) -> Resp {
    let mut r = Resp::status(status, body);
    r.headers = vec![("content-type".into(), content_type.into())];
    r
}

struct ResolverGuard;
impl Drop for ResolverGuard {
    fn drop(&mut self) {
        mare_binding::netauth::set_resolver_override(None);
        mare_binding::netauth::set_resolver_delay(None);
    }
}

struct EnvVarGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvVarGuard {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for &(k, v) in vars {
            saved.push((k, std::env::var(k).ok()));
            unsafe { std::env::set_var(k, v) };
        }
        Self { saved }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

/// Minimal upstream HTTP CONNECT proxy: records request lines and tunnels `CONNECT <host>:<port>`
/// to `127.0.0.1:<port>`. Non-CONNECT requests are recorded and closed with 400.
fn upstream_connect_proxy() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
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
                if !line.starts_with("CONNECT ") {
                    let _ = c.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
                    return;
                }
                let target_port: u16 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|t| t.rsplit(':').next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                let Ok(mut t) = std::net::TcpStream::connect(("127.0.0.1", target_port)) else {
                    let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n");
                    return;
                };
                if c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").is_err() {
                    return;
                }
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

fn redirect_resp(location: &str) -> Resp {
    let mut r = Resp::status(302, "");
    r.headers = vec![("location".into(), location.into())];
    r
}

fn host_port(base: &str) -> u16 {
    base.rsplit(':').next().unwrap().parse().unwrap()
}

async fn read_http_request_head(s: &mut tokio::net::TcpStream) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(());
        }
        if buf.len() > 64 * 1024 {
            return Err("request headers exceeded 64 KiB".into());
        }
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), s.read(&mut tmp))
            .await
            .map_err(|_| "timed out reading request headers".to_string())?
            .map_err(|e| format!("read error in request headers: {e}"))?;
        if n == 0 {
            return Err("client closed before finishing request headers".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn assert_peer_closed(s: &mut tokio::net::TcpStream) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut sink = [0u8; 64];
    match tokio::time::timeout(std::time::Duration::from_secs(2), s.read(&mut sink)).await {
        Ok(Ok(0)) => Ok(()),
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(())
        }
        Ok(Ok(n)) => Err(format!("expected peer close (EOF), got {n} extra bytes")),
        Ok(Err(e)) => Err(format!("unexpected read error waiting for close: {e}")),
        Err(_) => Err("timed out waiting for client to close socket".into()),
    }
}

mod fetch;
mod fetch_bounds_and_reload;
mod search_and_http_text;

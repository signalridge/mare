// exec-ctx: not a tool entry (split from http.rs; gated by exec_ctx::check at http.rs:686)
use std::io;

use super::*;

const OK_HTML: &str = "<!doctype html><html><body style=\"font-family:system-ui;padding:3em\"><h2>Login complete</h2><p>You can close this window and return to mare.</p></body></html>";
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_CONNS: usize = 16;

fn valid_loopback_host_header(headers: &[httparse::Header<'_>]) -> bool {
    let mut host_vals = headers.iter().filter(|h| h.name.eq_ignore_ascii_case("host"));
    let Some(first) = host_vals.next() else {
        return false;
    };
    if host_vals.next().is_some() {
        return false;
    }
    let Ok(raw) = std::str::from_utf8(first.value) else {
        return false;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let host_only = if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((h, _)) = rest.split_once(']') else {
            return false;
        };
        h
    } else if trimmed.matches(':').count() == 1 {
        trimmed.split_once(':').map_or(trimmed, |(h, _)| h)
    } else {
        trimmed
    };
    crate::netauth::is_local_proxy_host(host_only)
}

async fn read_http_head(sock: &mut tokio::net::TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + HEAD_TIMEOUT;
    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP header read timed out"));
        }
        let n = tokio::time::timeout(rem, sock.read(&mut tmp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HTTP header read timed out"))??;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF before end of headers"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "HTTP headers too large"));
        }
    }
}

async fn handle_conn(
    mut sock: tokio::net::TcpStream,
    want_path: Option<String>,
    tx: tokio::sync::mpsc::Sender<(String, Vec<(String, String)>)>,
) {
    let Ok(buf) = read_http_head(&mut sock).await else {
        return;
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let Ok(httparse::Status::Complete(_)) = req.parse(&buf) else {
        let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    };
    if !valid_loopback_host_header(req.headers) {
        let _ = sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    }
    let target = req.path.unwrap_or("/");
    let parsed = url::Url::parse(&format!("http://localhost{target}")).ok();
    let path = parsed.as_ref().map(|u| u.path().to_string()).unwrap_or_default();
    if let Some(w) = &want_path
        && &path != w
    {
        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        OK_HTML.len(),
        OK_HTML
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let query: Vec<(String, String)> =
        parsed.map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()).unwrap_or_default();
    let _ = tx.send((path, query)).await;
}

/// listen_once{port | ports = {p1, p2, …}, path="/", host="127.0.0.1", timeout_ms, on_bound}
/// -> {path, query={...}, port} | nil, err. With `ports` the first free one is used (an OAuth
/// callback with a fallback port); `on_bound(port)` runs once it is
/// listening, before the first connection (so the caller can build the redirect URL).
pub(super) async fn listen_once(lua: Lua, opts: Table) -> mlua::Result<mlua::MultiValue> {
    let ports: Vec<u16> = match opts.get::<Option<Vec<u16>>>("ports")? {
        Some(p) if !p.is_empty() => p,
        _ => vec![opts.get("port")?],
    };
    let host: String = opts.get::<Option<String>>("host")?.unwrap_or_else(|| "127.0.0.1".into());
    if !crate::netauth::is_local_proxy_host(&host) {
        return mare_shared::lua::nil_err(&lua, format!("listen_once refuses non-loopback host {host:?}"));
    }
    let want_path: Option<String> = opts.get("path")?;
    let timeout = opts.get::<Option<f64>>("timeout_ms")?.unwrap_or(300_000.0);
    let on_bound: Option<mlua::Function> = opts.get("on_bound")?;
    let tok = opt_token(&Some(opts.clone()));
    let mut bound = None;
    let mut errors = vec![];
    for port in &ports {
        match tokio::net::TcpListener::bind((host.as_str(), *port)).await {
            Ok(l) => {
                let addr = l.local_addr()?;
                if !addr.ip().is_loopback() {
                    return mare_shared::lua::nil_err(
                        &lua,
                        format!("listen_once refused non-loopback bound address {}", addr.ip()),
                    );
                }
                bound = Some((l, addr.port()));
                break;
            }
            Err(e) => errors.push(format!("{host}:{port}: {e}")),
        }
    }
    let Some((listener, port)) = bound else {
        return mare_shared::lua::nil_err(&lua, format!("cannot listen on {}", errors.join("; ")));
    };
    if let Some(f) = on_bound {
        f.call_async::<()>(port).await?;
    }
    let fut = async {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNS));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Vec<(String, String)>)>(1);
        loop {
            tokio::select! {
                matched = rx.recv() => {
                    return matched.ok_or_else(|| io::Error::other("listener closed"));
                }
                accepted = listener.accept() => {
                    let (sock, _) = accepted?;
                    let Ok(permit) = sem.clone().try_acquire_owned() else {
                        drop(sock);
                        continue;
                    };
                    let want = want_path.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        handle_conn(sock, want, tx).await;
                    });
                }
            }
        }
    };
    let res = tokio::time::timeout(Duration::from_millis(timeout as u64), cancellable(&tok, fut)).await;
    match res {
        Ok(Ok(Ok((path, query)))) => {
            let t = lua.create_table()?;
            t.set("path", path)?;
            t.set("port", port)?;
            let q = lua.create_table()?;
            for (k, v) in query {
                q.set(k, v)?;
            }
            t.set("query", q)?;
            Ok(mlua::MultiValue::from_vec(vec![Value::Table(t)]))
        }
        Ok(Ok(Err(e))) => mare_shared::lua::nil_err(&lua, e),
        Ok(Err(e)) => mare_shared::lua::nil_err(&lua, e),
        Err(_) => mare_shared::lua::nil_err(&lua, "timed out"),
    }
}

//! Host HTTP/WebSocket RPC integration tests (contracts, listener security, static assets
//! `web = { dir }`, and RPC over WebSocket).

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

use crate::common::*;

fn parse_contracts() -> std::collections::BTreeMap<String, mare_kernel::ServiceContract> {
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    mare_kernel::contracts::parse_contracts(&lua, value).unwrap()
}

async fn raw_http_request(addr: std::net::SocketAddr, req: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

#[test]
fn webserver_contract_exists_with_api_unification_shapes() {
    let contracts = parse_contracts();
    let ws = contracts.get("webserver").expect("`webserver` service contract must exist");
    for name in ["get", "list_routes", "list_assets", "register_route", "register_assets"] {
        assert!(ws.methods.contains_key(name), "webserver.{name} missing from contract");
    }
    assert_eq!(ws.methods["get"].access, mare_kernel::contracts::Access::Peer);
    assert_eq!(ws.methods["list_routes"].access, mare_kernel::contracts::Access::Peer);
    assert_eq!(ws.methods["list_assets"].access, mare_kernel::contracts::Access::Peer);
    assert_eq!(ws.methods["register_route"].access, mare_kernel::contracts::Access::Primary);
    assert!(ws.methods["register_route"].registration);
    assert_eq!(ws.methods["register_assets"].access, mare_kernel::contracts::Access::Primary);
    assert!(ws.methods["register_assets"].registration);

    for (mname, spec) in &ws.methods {
        let text = serde_json::to_string(&spec.result).unwrap();
        assert!(!text.contains("\"ok\""), "webserver.{mname} result must not use ok boolean");
        assert!(!text.contains("\"handle\""), "webserver.{mname} result must not use handle");
    }
}

#[tokio::test]
async fn cli_listener_auth_assets_and_ws_rpc_lifecycle() {
    let env = TestEnv::new().await;

    // 1. Refuse wildcard / unencrypted non-loopback / disabled token at CLI startup.
    let bad_wildcard = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["--rpc", "--clean", "--web-listen", "0.0.0.0:0"])
        .current_dir(&env.cwd)
        .output()
        .unwrap();
    assert!(!bad_wildcard.status.success(), "wildcard bind must be refused");

    let bad_plain_remote = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["--rpc", "--clean", "--web-listen", "192.0.2.10:0"])
        .current_dir(&env.cwd)
        .output()
        .unwrap();
    assert!(!bad_plain_remote.status.success(), "non-loopback bind without TLS/plain_http must be refused");

    // A backend that cannot protect the runtime token must fail closed without explicit acceptance.
    let no_protection = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args(["--rpc", "--clean", "--web-listen", "127.0.0.1:0"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output();
    let diagnostic = String::from_utf8_lossy(&no_protection.stderr);
    assert!(!no_protection.status.success(), "missing host token protection must refuse startup");
    assert!(diagnostic.contains("SANDBOX_INSUFFICIENT"), "unexpected refusal: {diagnostic}");
    assert!(!env.home.path().join("state/host/token").exists(), "refused startup must not create a token");

    // 2. Write a plugin package with `web = { dir = "public" }`.
    let pkg_dir = env.work.path().join(".mare/plugins/demo-web");
    std::fs::create_dir_all(pkg_dir.join("public")).unwrap();
    std::fs::write(pkg_dir.join("public/index.html"), "<html>ok</html>").unwrap();
    std::fs::write(
        pkg_dir.join("init.lua"),
        r#"
return {
  name = "demo-web",
  web = { dir = "public", prefix = "/ui" },
  apply = function(_ctx) end,
}
"#,
    )
    .unwrap();

    // 3. Start `mare --rpc --trust --web-listen 127.0.0.1:0` with faux model.
    let mut command = mare_cmd();
    command
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args([
            "--rpc",
            "--trust",
            "--no-session",
            "--model",
            "faux/echo",
            "--web-listen",
            "127.0.0.1:0",
            "--set",
            r#"webserver.accept_partial=["fs_host_protect"]"#,
        ])
        .current_dir(&env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut command).unwrap();
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut startup = String::new();
        let found = loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) => break Err(startup),
                Err(error) => break Err(format!("{startup}\nstderr read failed: {error}")),
                Ok(_) => {
                    assert!(!line.contains("?token="), "webserver stderr must not print raw token: {line}");
                    if let Some(pos) = line.find("http://127.0.0.1:") {
                        let candidate = line[pos..].trim().to_string();
                        break Ok(candidate);
                    }
                    startup.push_str(&line);
                }
            }
        };
        let _ = url_tx.send(found);
        // Keep draining stderr so the child can never block behind a full pipe.
        let mut line = String::new();
        while stderr.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let _stdout = child.0.stdout.take();
    let url = tokio::time::timeout(Duration::from_secs(15), url_rx)
        .await
        .expect("timed out waiting for webserver URL")
        .expect("webserver stderr reader exited")
        .unwrap_or_else(|diagnostic| panic!("webserver exited before printing its URL:\n{diagnostic}"));
    let base = url.trim_end_matches('/');
    let token = std::fs::read_to_string(env.home.path().join("state/host/token"))
        .expect("host token file written")
        .trim()
        .to_string();
    let host_port = base.strip_prefix("http://").unwrap();
    let addr: std::net::SocketAddr = host_port.parse().unwrap();

    // /readyz works without token
    let readyz =
        raw_http_request(addr, &format!("GET /readyz HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"))
            .await;
    assert!(readyz.starts_with("HTTP/1.1 200"), "readyz failed: {readyz}");

    // Unauthenticated asset GET fails 401
    let unauth = raw_http_request(
        addr,
        &format!("GET /ui/index.html HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(unauth.starts_with("HTTP/1.1 401"), "unauthenticated GET must fail 401: {unauth}");

    // Cross-site Origin, Origin: null, Sec-Fetch-Site: cross-site, and DNS-rebinding Host rejected 403
    let bad_host = raw_http_request(
        addr,
        &format!("GET /readyz HTTP/1.1\r\nHost: evil.example:{}\r\nConnection: close\r\n\r\n", addr.port()),
    )
    .await;
    assert!(bad_host.starts_with("HTTP/1.1 403"), "DNS-rebinding Host must fail 403: {bad_host}");

    let bad_origin = raw_http_request(
        addr,
        &format!(
            "GET /ui/index.html HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(bad_origin.starts_with("HTTP/1.1 403"), "cross-origin must fail 403: {bad_origin}");

    let null_origin = raw_http_request(
        addr,
        &format!(
            "GET /ui/index.html HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nOrigin: null\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(null_origin.starts_with("HTTP/1.1 403"), "Origin: null must fail 403: {null_origin}");

    let cross_site = raw_http_request(
        addr,
        &format!(
            "GET /ui/index.html HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nSec-Fetch-Site: cross-site\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(cross_site.starts_with("HTTP/1.1 403"), "Sec-Fetch-Site: cross-site must fail 403: {cross_site}");

    // ?token= exchanges for HttpOnly SameSite=Strict cookie + 303 strip
    let exchange = raw_http_request(
        addr,
        &format!("GET /ui/index.html?token={token} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(exchange.starts_with("HTTP/1.1 303"), "token exchange must 303: {exchange}");
    assert!(exchange.contains("HttpOnly") && exchange.contains("SameSite=Strict"), "cookie flags missing: {exchange}");

    // Path traversal rejected
    let traversal = raw_http_request(
        addr,
        &format!(
            "GET /ui/%2e%2e/%2e%2e/etc/passwd HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(
        traversal.starts_with("HTTP/1.1 400") || traversal.starts_with("HTTP/1.1 404"),
        "traversal must be blocked: {traversal}"
    );

    // Authorized asset served
    let asset = raw_http_request(
        addr,
        &format!("GET /ui/index.html HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(asset.starts_with("HTTP/1.1 200") && asset.contains("<html>ok</html>"), "asset failed: {asset}");

    // WebSocket RPC over /rpc with bearer token
    let ws_url = format!("ws://{host_port}/rpc?token={token}");
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.expect("WS connect failed");
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let init_frame = ws.next().await.unwrap().unwrap();
    let init_json: Value = serde_json::from_str(init_frame.to_text().unwrap()).unwrap();
    assert_eq!(init_json["id"], 1);
    assert!(init_json["result"]["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "webserver.get"));

    // Binary frame must close with 1003
    ws.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
    let close_msg = tokio::time::timeout(Duration::from_secs(3), ws.next()).await.unwrap().unwrap().unwrap();
    match close_msg {
        Message::Close(Some(cf)) => assert_eq!(u16::from(cf.code), 1003, "binary frame must close with 1003"),
        other => panic!("expected Close(1003), got {other:?}"),
    }

    // Disconnecting WS client must NOT kill the host process!
    assert!(child.0.try_wait().unwrap().is_none(), "WS disconnect must not kill host process");

    // Non-13 Sec-WebSocket-Version and invalid Sec-WebSocket-Key rejected with 400.
    let bad_ver = raw_http_request(
        addr,
        &format!(
            "GET /rpc HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 12\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        ),
    )
    .await;
    assert!(bad_ver.starts_with("HTTP/1.1 400"), "non-13 WS version must fail 400: {bad_ver}");

    let bad_key = raw_http_request(
        addr,
        &format!(
            "GET /rpc HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {token}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: invalid\r\n\r\n"
        ),
    )
    .await;
    assert!(bad_key.starts_with("HTTP/1.1 400"), "invalid WS key must fail 400: {bad_key}");

    // Cleanly shut down host process via stdin close.
    drop(ws);
    drop(_stdout);
    drop(child.0.stdin.take());
    let status = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::task::spawn_blocking(move || child.wait_timeout(Duration::from_secs(15))),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success(), "child exited cleanly: {status:?}");
}

/// The webserver startup banner must never print the raw token or a `?token=` URL; instead it
/// must show the host token file path and point the user to `mare web url`.
#[tokio::test]
async fn startup_banner_shows_token_file_path_and_web_url_command_without_token() {
    let env = TestEnv::new().await;
    let mut command = mare_cmd();
    command
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args([
            "--rpc",
            "--no-session",
            "--model",
            "faux/echo",
            "--web-listen",
            "127.0.0.1:0",
            "--set",
            r#"webserver.accept_partial=["fs_host_protect"]"#,
        ])
        .current_dir(&env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut command).unwrap();
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let (banner_tx, banner_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut banner = String::new();
        loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    banner.push_str(&line);
                    if line.contains("auth:") {
                        break;
                    }
                }
            }
        }
        let _ = banner_tx.send(banner);
        let mut line = String::new();
        while stderr.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let _stdout = child.0.stdout.take();
    let banner = tokio::time::timeout(Duration::from_secs(15), banner_rx)
        .await
        .expect("timed out waiting for webserver banner")
        .expect("stderr reader exited");

    let token_path = env.home.path().join("state").join("host").join("token");
    let token = std::fs::read_to_string(&token_path).expect("host token file written").trim().to_string();

    drop(_stdout);
    drop(child.0.stdin.take());
    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || child.wait_timeout(Duration::from_secs(10))),
    )
    .await;

    assert!(!banner.contains("?token="), "startup banner must never print ?token= URL:\n{banner}");
    assert!(!banner.contains(&token), "startup banner must never print raw token value:\n{banner}");
    assert!(
        banner.contains(&token_path.display().to_string())
            || banner.contains("state/host/token")
            || banner.contains(r"state\host\token"),
        "startup banner must display the host token file path ({}):\n{banner}",
        token_path.display()
    );
    assert!(banner.contains("mare web url"), "startup banner must point to `mare web url`:\n{banner}");
}

/// `mare web url` prints the authenticated `?token=` login URL when a host webserver listener is
/// active, and exits non-zero when no listener is running.
#[tokio::test]
async fn web_url_subcommand_prints_login_link_or_exits_nonzero_when_stopped() {
    let env = TestEnv::new().await;

    // 1. With no running webserver, `mare web url` must be a recognized subcommand and exit non-zero.
    let stopped = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output();
    let stopped_err = String::from_utf8_lossy(&stopped.stderr);
    assert!(!stopped.status.success(), "mare web url must exit non-zero when no listener is running");
    assert!(
        !stopped_err.contains("unrecognized subcommand") && !stopped_err.contains("no model configured"),
        "`mare web url` must be parsed as a subcommand (not a prompt) and report no running listener, got stderr:\n{stopped_err}"
    );

    // 2. With a running webserver, `mare web url` prints the login URL containing `?token=<token>` on stdout.
    let mut command = mare_cmd();
    command
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args([
            "--rpc",
            "--no-session",
            "--model",
            "faux/echo",
            "--web-listen",
            "127.0.0.1:0",
            "--set",
            r#"webserver.accept_partial=["fs_host_protect"]"#,
        ])
        .current_dir(&env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut command).unwrap();
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut startup = String::new();
        let found = loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) => break Err(startup),
                Err(e) => break Err(format!("{startup}\n{e}")),
                Ok(_) => {
                    if let Some(pos) = line.find("http://127.0.0.1:") {
                        break Ok(line[pos..].trim().to_string());
                    }
                    startup.push_str(&line);
                }
            }
        };
        let _ = url_tx.send(found);
        let mut line = String::new();
        while stderr.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let _stdout = child.0.stdout.take();
    let base_url = tokio::time::timeout(Duration::from_secs(15), url_rx)
        .await
        .expect("timed out waiting for webserver URL")
        .expect("stderr reader exited")
        .unwrap();
    let token = std::fs::read_to_string(env.home.path().join("state/host/token"))
        .expect("host token file written")
        .trim()
        .to_string();

    let running = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output();
    let out = String::from_utf8_lossy(&running.stdout);
    let err = String::from_utf8_lossy(&running.stderr);

    drop(_stdout);
    drop(child.0.stdin.take());
    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || child.wait_timeout(Duration::from_secs(10))),
    )
    .await;

    assert!(running.status.success(), "mare web url must succeed while listener is running, stderr:\n{err}");
    assert!(
        out.contains(base_url.trim_end_matches('/')) && out.contains(&format!("?token={token}")),
        "mare web url must print login URL with ?token=, got stdout: {out:?}"
    );
}

/// `mare web url` rejects forged/tampered listener metadata (never appending the real host token to
/// another address or reading an attacker-chosen `token_path`), cleans up stale dead-PID records,
/// refuses static-token listeners without reading cwd config, and follows `mare token rotate`.
#[tokio::test]
async fn web_url_rejects_forged_records_cleans_stale_state_refuses_static_cwd_fallback_and_follows_rotation() {
    let cmd_timeout = Duration::from_secs(15);
    let env = TestEnv::new().await;
    let web_dir = env.home.path().join("state/web");
    mare_shared::private::create_dir_all(&web_dir).unwrap();

    // 1. Stale dead-PID record is removed and `mare web url` exits non-zero.
    let stale_file = web_dir.join("999999-1.json");
    let stale_json = json!({
        "pid": 999_999u32,
        "start_time": 1u64,
        "instance_id": 1u64,
        "url": "http://127.0.0.1:49999",
        "ws_url": "ws://127.0.0.1:49999/rpc",
        "tls": false,
        "token_source": "runtime_file"
    })
    .to_string();
    mare_shared::paths::write_atomic(&stale_file, stale_json.as_bytes(), mare_shared::paths::WriteMode::Private)
        .unwrap();
    let stale_out = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    assert!(
        !stale_out.status.success(),
        "stale listener must fail mare web url; stdout={}, stderr={}",
        String::from_utf8_lossy(&stale_out.stdout),
        String::from_utf8_lossy(&stale_out.stderr)
    );
    assert!(!stale_file.exists(), "mare web url must remove stale listener record");

    // 2. Start a genuine runtime-file webserver, then plant a forged record using a live PID pointing
    //    to an attacker URL and an arbitrary token_path.
    let mut command = mare_cmd();
    command
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args([
            "--rpc",
            "--no-session",
            "--model",
            "faux/echo",
            "--web-listen",
            "127.0.0.1:0",
            "--set",
            r#"webserver.accept_partial=["fs_host_protect"]"#,
        ])
        .current_dir(&env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut command).unwrap();
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut startup = String::new();
        let found = loop {
            let mut line = String::new();
            match stderr.read_line(&mut line) {
                Ok(0) => break Err(startup),
                Err(e) => break Err(format!("{startup}\n{e}")),
                Ok(_) => {
                    if let Some(pos) = line.find("http://127.0.0.1:") {
                        break Ok(line[pos..].trim().to_string());
                    }
                    startup.push_str(&line);
                }
            }
        };
        let _ = url_tx.send(found);
        let mut line = String::new();
        while stderr.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let _stdout = child.0.stdout.take();
    let genuine_base = tokio::time::timeout(Duration::from_secs(15), url_rx)
        .await
        .expect("timed out waiting for webserver URL")
        .expect("stderr reader exited")
        .unwrap();
    let initial_token = std::fs::read_to_string(env.home.path().join("state/host/token"))
        .expect("host token file written")
        .trim()
        .to_string();

    // Plant a forged listener record claiming a live PID (our own test process!) and attacker URL.
    let forged_file = web_dir.join(format!("{}-9999.json", std::process::id()));
    std::fs::write(
        &forged_file,
        json!({
            "pid": std::process::id(),
            "instance_id": 9999u64,
            "url": "http://198.51.100.77:8080",
            "ws_url": "ws://198.51.100.77:8080/rpc",
            "tls": false,
            "token_source": "runtime_file",
            "token_path": "/etc/hosts"
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&forged_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mixed_out = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let mixed_stdout = String::from_utf8_lossy(&mixed_out.stdout);
    let mixed_stderr = String::from_utf8_lossy(&mixed_out.stderr);
    assert!(
        mixed_out.status.success(),
        "genuine listener must still succeed; stdout={mixed_stdout:?}, stderr={mixed_stderr:?}"
    );
    assert!(
        mixed_stdout.contains(genuine_base.trim_end_matches('/'))
            && mixed_stdout.contains(&format!("?token={initial_token}")),
        "genuine URL missing; stdout={mixed_stdout:?}, stderr={mixed_stderr:?}"
    );
    assert!(
        !mixed_stdout.contains("198.51.100.77"),
        "forged listener record must NEVER receive real host token; stdout={mixed_stdout:?}, stderr={mixed_stderr:?}"
    );

    // Loose-mode (0644) record on Unix must be ignored even if it points to 127.0.0.1.
    #[cfg(unix)]
    let loose_file = {
        use std::os::unix::fs::PermissionsExt;
        let p = web_dir.join(format!("{}-9998.json", std::process::id()));
        std::fs::write(
            &p,
            json!({
                "pid": std::process::id(),
                "instance_id": 9998u64,
                "url": "http://127.0.0.1:49998",
                "ws_url": "ws://127.0.0.1:49998/rpc",
                "tls": false,
                "token_source": "runtime_file"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out_loose = mare_cmd()
            .env("MARE_HOME", env.home.path())
            .args(["web", "url"])
            .current_dir(&env.cwd)
            .stdin(Stdio::null())
            .run_output_timeout(cmd_timeout);
        let loose_stdout = String::from_utf8_lossy(&out_loose.stdout);
        let loose_stderr = String::from_utf8_lossy(&out_loose.stderr);
        assert!(
            !loose_stdout.contains("49998"),
            "world/group-readable listener record must be rejected; stdout={loose_stdout:?}, stderr={loose_stderr:?}"
        );
        p
    };
    #[cfg(unix)]
    let _ = std::fs::remove_file(&loose_file);

    // 3. Rotating the host token (and even replacing host/token directly) updates `mare web url`
    //    to print the new token for the genuine listener without stale-MAC rejection.
    let rot = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args(["token", "rotate", "--accept-partial", "fs_host_protect"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    assert!(
        rot.status.success(),
        "token rotate failed; stdout={}, stderr={}",
        String::from_utf8_lossy(&rot.stdout),
        String::from_utf8_lossy(&rot.stderr)
    );
    let rotated_token = std::fs::read_to_string(env.home.path().join("state/host/token")).unwrap().trim().to_string();
    assert_ne!(rotated_token, initial_token);

    // Simulate out-of-band token regeneration (e.g. token file removed & recreated or concurrent rotations).
    let regenerated_token = "regenerated-out-of-band-token-0123456789abc";
    std::fs::write(env.home.path().join("state/host/token"), regenerated_token).unwrap();

    let after_rot = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let rot_stdout = String::from_utf8_lossy(&after_rot.stdout);
    let rot_stderr = String::from_utf8_lossy(&after_rot.stderr);
    assert!(
        after_rot.status.success(),
        "mare web url must still succeed after out-of-band token regeneration; stdout={rot_stdout:?}, stderr={rot_stderr:?}"
    );
    assert!(
        rot_stdout.contains(genuine_base.trim_end_matches('/'))
            && rot_stdout.contains(&format!("?token={regenerated_token}")),
        "mare web url must print live host token; stdout={rot_stdout:?}, stderr={rot_stderr:?}"
    );
    assert!(
        !rot_stdout.contains("198.51.100.77"),
        "forged record must stay rejected; stdout={rot_stdout:?}, stderr={rot_stderr:?}"
    );

    // Stop the genuine listener; now ONLY the forged record remains -> `mare web url` MUST fail and leak nothing.
    drop(_stdout);
    drop(child.0.stdin.take());
    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || child.wait_timeout(Duration::from_secs(10))),
    )
    .await;

    let forged_only = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let forged_stdout = String::from_utf8_lossy(&forged_only.stdout);
    let forged_stderr = String::from_utf8_lossy(&forged_only.stderr);
    assert!(
        !forged_only.status.success(),
        "forged-only listener must fail mare web url; stdout={forged_stdout:?}, stderr={forged_stderr:?}"
    );
    assert!(
        !forged_stdout.contains(regenerated_token),
        "forged record must never leak token; stdout={forged_stdout:?}, stderr={forged_stderr:?}"
    );
    let _ = std::fs::remove_file(&forged_file);

    // Zone-scoped IPv6 record must give an explicit diagnostic (standard URLs cannot carry %zone).
    let zone_file = web_dir.join(format!("{}-9997.json", std::process::id()));
    let zone_json = json!({
        "pid": std::process::id(),
        "instance_id": 9997u64,
        "url": "http://[fe80::1%eth0]:7432",
        "ws_url": "ws://[fe80::1%eth0]:7432/rpc",
        "tls": false,
        "token_source": "runtime_file"
    })
    .to_string();
    mare_shared::paths::write_atomic(&zone_file, zone_json.as_bytes(), mare_shared::paths::WriteMode::Private).unwrap();
    let zone_out = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let zone_stdout = String::from_utf8_lossy(&zone_out.stdout);
    let zone_stderr = String::from_utf8_lossy(&zone_out.stderr);
    assert!(
        !zone_out.status.success(),
        "zone-scoped IPv6 record must exit non-zero; stdout={zone_stdout:?}, stderr={zone_stderr:?}"
    );
    assert!(
        zone_stderr.contains("zone") || zone_stderr.contains('%'),
        "zone-scoped IPv6 record must explain %zone URL limitation; stdout={zone_stdout:?}, stderr={zone_stderr:?}"
    );
    let _ = std::fs::remove_file(&zone_file);

    // Static-token record with an invalid URL (hostname instead of IP literal) must also be rejected by URL validation.
    let bad_static_file = web_dir.join(format!("{}-9996.json", std::process::id()));
    let bad_static_json = json!({
        "pid": std::process::id(),
        "instance_id": 9996u64,
        "url": "http://evil.example/steal",
        "ws_url": "ws://evil.example/rpc",
        "tls": false,
        "token_source": "static"
    })
    .to_string();
    mare_shared::paths::write_atomic(
        &bad_static_file,
        bad_static_json.as_bytes(),
        mare_shared::paths::WriteMode::Private,
    )
    .unwrap();
    let bad_static_out = mare_cmd()
        .env("MARE_HOME", env.home.path())
        .args(["web", "url"])
        .current_dir(&env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let bad_static_stdout = String::from_utf8_lossy(&bad_static_out.stdout);
    let bad_static_stderr = String::from_utf8_lossy(&bad_static_out.stderr);
    assert!(
        !bad_static_out.status.success(),
        "static record with invalid URL must fail; stdout={bad_static_stdout:?}, stderr={bad_static_stderr:?}"
    );
    assert!(
        !bad_static_stderr.contains("evil.example/steal"),
        "static record must validate URL before echoing it; stdout={bad_static_stdout:?}, stderr={bad_static_stderr:?}"
    );
    let _ = std::fs::remove_file(&bad_static_file);
    drop(env);

    // 4. Static-token listener: `mare web url` checks recorded `token_source = "static"` and refuses
    //    WITHOUT falling back to reading cwd/home `init.lua`.
    let static_env = TestEnv::new().await;
    let static_secret = "static-cwd-token-0123456789abcdef";
    std::fs::create_dir_all(static_env.home.path().join("config")).unwrap();
    std::fs::write(
        static_env.home.path().join("config/init.lua"),
        format!("mare.setup {{ webserver = {{ token = {static_secret:?} }} }}\n"),
    )
    .unwrap();
    let mut static_cmd = mare_cmd();
    static_cmd
        .env("MARE_HOME", static_env.home.path())
        .env("MARE_SANDBOX_BACKEND", "none")
        .args(["--rpc", "--no-session", "--model", "faux/echo", "--web-listen", "127.0.0.1:0"])
        .current_dir(&static_env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut static_child = ChildGroup::spawn(&mut static_cmd).unwrap();
    let mut static_stderr = BufReader::new(static_child.0.stderr.take().unwrap());
    let (s_tx, s_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut startup = String::new();
        let found = loop {
            let mut line = String::new();
            match static_stderr.read_line(&mut line) {
                Ok(0) => break Err(startup),
                Err(e) => break Err(format!("{startup}\n{e}")),
                Ok(_) => {
                    if line.contains("auth:") {
                        break Ok(startup);
                    }
                    startup.push_str(&line);
                }
            }
        };
        let _ = s_tx.send(found);
        let mut line = String::new();
        while static_stderr.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let _s_stdout = static_child.0.stdout.take();
    tokio::time::timeout(Duration::from_secs(15), s_rx).await.unwrap().unwrap().unwrap();

    // Static-token startup must NOT have created `<state>/host/`.
    assert!(!static_env.home.path().join("state/host").exists(), "static-token webserver must not create state/host");

    let static_url = mare_cmd()
        .env("MARE_HOME", static_env.home.path())
        .args(["web", "url"])
        .current_dir(&static_env.cwd)
        .stdin(Stdio::null())
        .run_output_timeout(cmd_timeout);
    let static_stdout = String::from_utf8_lossy(&static_url.stdout);
    let static_stderr_text = String::from_utf8_lossy(&static_url.stderr);

    drop(_s_stdout);
    drop(static_child.0.stdin.take());
    let _ = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || static_child.wait_timeout(Duration::from_secs(10))),
    )
    .await;

    assert!(!static_url.status.success(), "static-token listener must not fall back to cwd/config token");
    assert!(
        !static_stdout.contains(static_secret),
        "mare web url must not read static token from config: {static_stdout:?}"
    );
    assert!(
        static_stderr_text.contains("static"),
        "mare web url stderr must explain static token source: {static_stderr_text}"
    );
}

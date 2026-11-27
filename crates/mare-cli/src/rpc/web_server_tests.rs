//! Unit and in-process integration tests for host HTTP/WebSocket serving (`web/mod.rs`, `web/tls.rs`,
//! `web/ws_rpc.rs`, `mare_host::web_registry`, `remote_admin`, TLS, WS keepalive/1013/4001).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mare_binding::http_serve::rotate_host_token_at;
use mare_host::HostOptions;
use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::serve::serve_manager;
use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;
use crate::web::start_host_webserver;

const TEST_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIBmzCCAUGgAwIBAgIUY2XHHrDJOCqlk2plJQUotxQ2iQswCgYIKoZIzj0EAwIw\n\
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTEyMDA2MDc1MloYDzIxMjYxMDI3\n\
MDYwNzUyWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\n\
PQMBBwNCAAT+wpm6j63AKDedpV/dsGUcH2rxqmbrr8PrhtZzsNvH0LEFdpNZ2EJU\n\
rl31IZpEyp+vsQ6SDxGexBQ+HlUJb4a8o28wbTAdBgNVHQ4EFgQUa8a4CjyIzehc\n\
wydT+fI6E7KuGPIwHwYDVR0jBBgwFoAUa8a4CjyIzehcwydT+fI6E7KuGPIwDwYD\n\
VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI\n\
zj0EAwIDSAAwRQIhAMVMiH8sCxdCwgzL0zfG2wnN2FHqE6zje2npK1aD90/eAiAN\n\
KOd8+uWby0guUH0h+9bNr1M++lbUpnnG/SRCE6xETg==\n\
-----END CERTIFICATE-----\n";

const TEST_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgt0wVXsS5Wlfji/RQ\n\
64q2c4W7w4lhQelZ1XaR67ULhuShRANCAAT+wpm6j63AKDedpV/dsGUcH2rxqmbr\n\
r8PrhtZzsNvH0LEFdpNZ2EJUrl31IZpEyp+vsQ6SDxGexBQ+HlUJb4a8\n\
-----END PRIVATE KEY-----\n";

#[derive(Debug)]
struct AcceptAnyTestCert;

impl ServerCertVerifier for AcceptAnyTestCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ECDSA_NISTP256_SHA256, rustls::SignatureScheme::RSA_PSS_SHA256]
    }
}

async fn setup_web_rpc(
    config: J,
) -> (
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
    Rc<RpcRuntime>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    let guard = HOST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let opts = HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Rpc,
        config,
        load_init: false,
        project_trusted: true,
        args: json!({ "no_session": true }),
        no_user_plugins: true,
    };
    let manager = mare_host::HostManager::new();
    let rt = manager.create_session(opts.clone()).await.unwrap();
    let sid = rt.session_id.clone();
    let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

    let (client, server) = tokio::io::duplex(131072);
    let (sr, sw) = tokio::io::split(server);
    let (cr, cw) = tokio::io::split(client);
    let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
    (dir, guard, rpc, BufReader::new(cr), cw, task, sid)
}

async fn read_reply(cr: &mut (impl AsyncBufReadExt + Unpin), id: i64) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg["id"] == id {
            return msg;
        }
    }
}

async fn raw_http(addr: SocketAddr, req: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

async fn ws_next_text<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return t.to_string(),
            Message::Ping(p) => ws.send(Message::Pong(p)).await.unwrap(),
            other => panic!("unexpected ws frame: {other:?}"),
        }
    }
}

async fn ws_next_text_matching<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, needle: &str) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) if t.contains(needle) => return t.to_string(),
            Message::Text(_) => {}
            Message::Ping(p) => ws.send(Message::Pong(p)).await.unwrap(),
            other => panic!("unexpected ws frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn web_route_register_dispatch_conflict_and_session_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, sid1) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
                "webserver": {
                    "enabled": true,
                    "host": "127.0.0.1",
                    "port": 0,
                    "token": "test-static-token-0123456789abcdef"
                }
            }))
            .await;

            let cfg = rpc.anchor_host().unwrap().config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": { "callbacks": true } }
                }),
            )
            .await;
            let init_r = read_reply(&mut cr, 1).await;
            assert!(init_r["error"].is_null(), "{init_r}");

            // Reserved & invalid route rejection
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "webserver.register_route",
                    "params": { "path": "/rpc", "callbacks": ["handle"] }
                }),
            )
            .await;
            let r2 = read_reply(&mut cr, 2).await;
            assert_eq!(r2["error"]["data"]["code"], "webserver.invalid_route", "{r2}");

            // Register POST /api/echo via contract + callback.call
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "webserver.register_route",
                    "params": { "path": "/api/echo", "method": "POST", "callbacks": ["handle"] }
                }),
            )
            .await;
            let r3 = read_reply(&mut cr, 3).await;
            let reg_id = r3["result"]["registration_id"].as_str().expect("registration_id").to_string();

            // Duplicate method+path returns webserver.route_conflict
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "webserver.register_route",
                    "params": { "path": "/api/echo", "method": "POST", "callbacks": ["handle"] }
                }),
            )
            .await;
            let r4 = read_reply(&mut cr, 4).await;
            assert_eq!(r4["error"]["data"]["code"], "webserver.route_conflict", "{r4}");

            // Inspect list_routes and register_assets/list_assets via webserver contract.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 41, "method": "webserver.list_routes", "params": {} })).await;
            let routes_rep = read_reply(&mut cr, 41).await;
            assert!(
                routes_rep["result"]["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["path"] == "/api/echo"),
                "{routes_rep}"
            );

            let asset_dir = rpc.anchor_host().unwrap().cwd.join("pub-assets");
            std::fs::create_dir_all(&asset_dir).unwrap();
            std::fs::write(asset_dir.join("hello.txt"), "asset-ok").unwrap();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 42,
                    "method": "webserver.register_assets",
                    "params": { "dir": "pub-assets", "prefix": "/pub" }
                }),
            )
            .await;
            let reg_asset_rep = read_reply(&mut cr, 42).await;
            let asset_reg_id = reg_asset_rep["result"]["registration_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 43, "method": "webserver.list_assets", "params": {} })).await;
            let list_asset_rep = read_reply(&mut cr, 43).await;
            assert!(
                list_asset_rep["result"]["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["prefix"] == "/pub"),
                "{list_asset_rep}"
            );
            let asset_hit = raw_http(
                addr,
                &format!(
                    "GET /pub/hello.txt HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-static-token-0123456789abcdef\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(asset_hit.starts_with("HTTP/1.1 200") && asset_hit.contains("asset-ok"), "{asset_hit}");
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 44, "method": "registrations.release", "params": { "registration_id": asset_reg_id } }),
            )
            .await;
            assert!(read_reply(&mut cr, 44).await["error"].is_null());

            // Dispatch live HTTP request and answer callback.call on RPC client
            let req_text = format!(
                "POST /api/echo?x=1 HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-static-token-0123456789abcdef\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
            );
            let http_fut = raw_http(addr, &req_text);
            let rpc_fut = async {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" {
                        let req_arg = &msg["params"]["arguments"]["req"];
                        assert_eq!(req_arg["method"], "POST");
                        assert_eq!(req_arg["path"], "/api/echo");
                        assert_eq!(req_arg["query"]["x"], "1");
                        assert_eq!(req_arg["body"], "hello");
                        rpc_send(
                            &mut cw,
                            json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": { "status": 201, "headers": { "x-custom": "ok" }, "body": "echo:hello" }
                            }),
                        )
                        .await;
                        break;
                    }
                }
            };
            let (http_resp, ()) = tokio::join!(http_fut, rpc_fut);
            assert!(http_resp.starts_with("HTTP/1.1 201"), "{http_resp}");
            assert!(http_resp.contains("x-custom: ok") && http_resp.contains("echo:hello"), "{http_resp}");

            // Release via registrations.release
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 5, "method": "registrations.release", "params": { "registration_id": reg_id } }),
            )
            .await;
            let r5 = read_reply(&mut cr, 5).await;
            assert!(r5["error"].is_null(), "{r5}");
            let after_release = raw_http(
                addr,
                &format!(
                    "POST /api/echo HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-static-token-0123456789abcdef\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(after_release.starts_with("HTTP/1.1 404"), "{after_release}");

            // Register a Lua route on session 1, create session 2, close session 1 -> session 1 route removed, listener stays alive!
            let rt1 = rpc.manager.session(&sid1).unwrap();
            let _disposer: mlua::Function = rt1
                .host
                .lua()
                .load(r#"return mare.webserver.register_route("test-lua", "/lua-hi", { method = "GET", handle = function(req) return { status = 200, body = "hi-lua:" .. req.path } end })"#)
                .eval()
                .unwrap();
            let hit = raw_http(
                addr,
                &format!(
                    "GET /lua-hi HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-static-token-0123456789abcdef\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(hit.starts_with("HTTP/1.1 200") && hit.contains("hi-lua:/lua-hi"), "{hit}");

            let _rt2 = rpc
                .manager
                .create_session(HostOptions {
                    cwd: rt1.host.cwd.clone(),
                    mode: mare_plugin::Mode::Rpc,
                    config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                    load_init: false,
                    project_trusted: true,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(15), rpc.manager.close_session(&sid1))
                .await
                .expect("close_session timed out")
                .unwrap();
            let gone = raw_http(
                addr,
                &format!(
                    "GET /lua-hi HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-static-token-0123456789abcdef\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(gone.starts_with("HTTP/1.1 404"), "session close must drop its route: {gone}");
            let ready = raw_http(addr, &format!("GET /readyz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")).await;
            assert!(ready.starts_with("HTTP/1.1 200"), "host listener stays up after session 1 closes: {ready}");

            tokio::time::timeout(Duration::from_secs(10), srv.shutdown())
                .await
                .expect("srv.shutdown timed out");
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("serve_manager task timed out")
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), rpc.manager.shutdown_all())
                .await
                .expect("shutdown_all timed out");
        })
        .await;
}

#[tokio::test]
async fn web_remote_admin_false_and_tls_and_ws_keepalive_rotation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let cert_path = dir.path().join("cert.pem");
            let key_path = dir.path().join("key.pem");
            let tok_path = dir.path().join("host-token");
            std::fs::write(&cert_path, TEST_CERT_PEM).unwrap();
            std::fs::write(&key_path, TEST_KEY_PEM).unwrap();
            let initial_tok = rotate_host_token_at(&tok_path).unwrap();

            // 1. Validate config rejections (`token = false`, mismatched TLS, wildcard, invalid trusted_hosts)
            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": initial_tok,
                        "trusted_hosts": ["remote.example.test"],
                        "remote_admin": false,
                        "tls_cert": cert_path,
                        "tls_key": key_path,
                        "ws_ping_interval_ms": 50,
                        "ws_pong_timeout_ms": 80
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);

            assert!(start_host_webserver(&rpc, &json!({ "webserver": { "token": false } }), None).await.is_err());
            assert!(start_host_webserver(&rpc, &json!({ "webserver": { "tls_cert": cert_path } }), None).await.is_err());
            assert!(start_host_webserver(&rpc, &json!({ "webserver": { "trusted_hosts": ["bad/host"] } }), None).await.is_err());
            assert!(
                start_host_webserver(&rpc, &json!({ "webserver": { "host": "::ffff:0.0.0.0", "plain_http": true } }), None)
                    .await
                    .is_err(),
                "IPv4-mapped wildcard must be rejected"
            );
            let bad_pem = dir.path().join("bad.pem");
            std::fs::write(&bad_pem, "not-pem").unwrap();
            assert!(
                start_host_webserver(
                    &rpc,
                    &json!({ "webserver": { "tls_cert": bad_pem, "tls_key": key_path } }),
                    None,
                )
                .await
                .is_err()
            );

            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            assert!(srv.tls);
            let addr = srv.addr;

            // 2. Stage B TLS + `; Secure` cookie check
            let client_cfg = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyTestCert))
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(client_cfg));
            let tcp = TcpStream::connect(addr).await.unwrap();
            let sni = ServerName::try_from("localhost").unwrap();
            let mut tls_stream = connector.connect(sni, tcp).await.unwrap();
            let req = format!("GET /readyz?token={initial_tok} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n", addr.port());
            tls_stream.write_all(req.as_bytes()).await.unwrap();
            let mut resp_bytes = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), tls_stream.read_to_end(&mut resp_bytes)).await;
            let resp_str = String::from_utf8_lossy(&resp_bytes);
            assert!(resp_str.starts_with("HTTP/1.1 200"), "{resp_str}");

            // TLS mode must reject http:// Origin against https:// host authority (SF#5)
            let tcp_bad_scheme = TcpStream::connect(addr).await.unwrap();
            let mut tls_bad_scheme = connector.connect(ServerName::try_from("localhost").unwrap(), tcp_bad_scheme).await.unwrap();
            let bad_origin_req = format!(
                "GET /readyz HTTP/1.1\r\nHost: localhost:{}\r\nOrigin: http://localhost:{}\r\nConnection: close\r\n\r\n",
                addr.port(),
                addr.port()
            );
            tls_bad_scheme.write_all(bad_origin_req.as_bytes()).await.unwrap();
            let mut bad_scheme_bytes = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), tls_bad_scheme.read_to_end(&mut bad_scheme_bytes)).await;
            let bad_scheme_str = String::from_utf8_lossy(&bad_scheme_bytes);
            assert!(bad_scheme_str.starts_with("HTTP/1.1 403"), "http:// Origin on TLS must be 403: {bad_scheme_str}");

            // Check `; Secure` cookie on non-readyz token exchange over TLS
            let tcp2 = TcpStream::connect(addr).await.unwrap();
            let mut tls_stream2 = connector.connect(ServerName::try_from("localhost").unwrap(), tcp2).await.unwrap();
            let req2 = format!("GET /missing?token={initial_tok} HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n", addr.port());
            tls_stream2.write_all(req2.as_bytes()).await.unwrap();
            let mut r2_bytes = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), tls_stream2.read_to_end(&mut r2_bytes)).await;
            let r2_str = String::from_utf8_lossy(&r2_bytes);
            assert!(r2_str.starts_with("HTTP/1.1 303") && r2_str.contains("; Secure"), "{r2_str}");

            // 3. wss:// with `Host: remote.example.test:<port>` -> `remote_admin = false`
            let mut ws_req = format!("wss://localhost:{}/rpc?token={initial_tok}", addr.port())
                .into_client_request()
                .unwrap();
            ws_req.headers_mut().insert(
                "host",
                http::HeaderValue::from_str(&format!("remote.example.test:{}", addr.port())).unwrap(),
            );
            let tcp3 = TcpStream::connect(addr).await.unwrap();
            let tls3 = connector.connect(ServerName::try_from("localhost").unwrap(), tcp3).await.unwrap();
            let (mut wss, _) = tokio_tungstenite::client_async(ws_req, tls3).await.unwrap();
            wss.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let init_txt = ws_next_text(&mut wss).await;
            let init_v: J = serde_json::from_str(&init_txt).unwrap();
            let advertised = init_v["result"]["capabilities"]["methods"].as_array().unwrap();
            assert!(!advertised.iter().any(|m| m == "workspace.set_trust"), "must hide admin methods");
            assert!(!advertised.iter().any(|m| m == "webserver.register_route"), "must hide admin contract ops");
            assert!(advertised.iter().any(|m| m == "webserver.get"));

            // Admin calls rejected with denied
            wss.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 2, "method": "workspace.set_trust", "params": { "cwd": dir.path(), "trusted": true } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let denied_msg = ws_next_text(&mut wss).await;
            let dv: J = serde_json::from_str(&denied_msg).unwrap();
            assert_eq!(dv["error"]["data"]["code"], "denied", "{dv}");

            // Non-admin read succeeds
            wss.send(Message::Text(json!({ "jsonrpc": "2.0", "id": 3, "method": "webserver.get", "params": {} }).to_string().into()))
                .await
                .unwrap();
            let get_msg = ws_next_text(&mut wss).await;
            let gv: J = serde_json::from_str(&get_msg).unwrap();
            assert_eq!(gv["result"]["running"], true);
            assert_eq!(gv["result"]["tls"], true);

            // `services.list` hides withheld contract methods, and `services.call` denies invoking them with `denied`.
            wss.send(Message::Text(json!({ "jsonrpc": "2.0", "id": 4, "method": "services.list", "params": {} }).to_string().into()))
                .await
                .unwrap();
            let svc_list_txt = ws_next_text(&mut wss).await;
            let svc_list: J = serde_json::from_str(&svc_list_txt).unwrap();
            let svcs = svc_list["result"]["data"].as_array().unwrap();
            let has_op = |svc: &str, m: &str| {
                svcs.iter()
                    .find(|s| s["name"] == svc)
                    .and_then(|s| s["methods"].get(m))
                    .is_some()
            };
            assert!(has_op("webserver", "get"));
            assert!(!has_op("webserver", "register_route"), "services.list must hide webserver.register_route");
            assert!(!has_op("plugins", "install"), "services.list must hide plugins.install");
            assert!(!has_op("fs", "write"), "services.list must hide fs.write");
            assert!(!has_op("ui", "send_editor"), "services.list must hide ui.send_editor");
            assert!(!has_op("ui", "update_editor"), "services.list must hide ui.update_editor");
            assert!(!has_op("ui", "call_keymap"), "services.list must hide ui.call_keymap");
            assert!(!has_op("ui", "set_external_editor"), "services.list must hide ui.set_external_editor");
            assert!(!has_op("ui", "set_keymap"), "services.list must hide ui.set_keymap");
            assert!(!has_op("ui", "delete_keymap"), "services.list must hide ui.delete_keymap");
            assert!(!has_op("commands", "register"), "services.list must hide commands.register");
            assert!(!has_op("mcp", "connect"), "services.list must hide mcp.connect");
            assert!(!has_op("mcp", "reconnect"), "services.list must hide mcp.reconnect");
            assert!(!has_op("web", "register_fetch"), "services.list must hide web.register_fetch");
            assert!(!has_op("web", "register_search"), "services.list must hide web.register_search");

            for (req_id, method, params) in [
                (5, "plugins.install", json!({})),
                (6, "webserver.register_route", json!({ "path": "/blocked" })),
                (7, "ui.send_editor", json!({ "text": "! echo pwn" })),
                (8, "ui.update_editor", json!({ "text": "! echo pwn" })),
                (9, "ui.call_keymap", json!({ "context": "editor", "action": "editor.submit" })),
                (10, "ui.set_external_editor", json!({ "action": "start" })),
                (11, "commands.register", json!({ "name": "evil", "description": "x" })),
                (12, "mcp.connect", json!({ "name": "evil" })),
                (13, "web.register_fetch", json!({ "id": "evil" })),
            ] {
                wss.send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": req_id, "method": method, "params": params })
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
                let resp_txt = ws_next_text(&mut wss).await;
                let resp: J = serde_json::from_str(&resp_txt).unwrap();
                assert_eq!(resp["error"]["data"]["code"], "denied", "{method} must return denied: {resp}");
            }

            // Ping / Pong keepalive timeout test: stop reading/replying -> server closes dead peer with 1008!
            tokio::time::sleep(Duration::from_millis(250)).await; // sleep-ok: verify server-initiated WS pong keepalive timeout fires
            let timed_out = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match wss.next().await {
                        Some(Ok(Message::Ping(_))) => {}
                        Some(Ok(Message::Close(cf))) => break cf.map(|c| u16::from(c.code)),
                        Some(Err(e)) => panic!("expected clean Close(1008), got err: {e}"),
                        None => panic!("expected Close(1008) before EOF"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(timed_out, Some(1008), "server must close unresponsive WS client with 1008 Policy");

            srv.shutdown().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn web_static_assets_fallback_traversal_symlink_and_pkg_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    let outside = dir.path().join("secret.txt");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(&outside, "secret-data").unwrap();
    std::fs::write(site.join("index.html"), "<h1>app</h1>").unwrap();
    std::fs::write(site.join("main.js"), "console.log(1);").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, site.join("escape.txt")).unwrap();

    // Test mare-plugin-manager [web] manifest parsing
    std::fs::write(
        dir.path().join("mare-plugin.toml"),
        "name = \"ui-pkg\"\n[web]\ndir = \"site\"\nprefix = \"/plugins\"\nfallback = \"index.html\"\n",
    )
    .unwrap();
    let manifest = mare_plugin_manager::manifest(dir.path()).unwrap().unwrap();
    let web_m = manifest.web.unwrap();
    assert_eq!(web_m.dir, "site");
    assert_eq!(web_m.prefix.as_deref(), Some("/plugins"));
    assert_eq!(web_m.fallback.as_deref(), Some("index.html"));

    let reg = mare_host::WebRegistry::new();
    let sid = std::rc::Rc::new(std::cell::RefCell::new(None));
    let asset_id = reg
        .register_assets(mare_host::web_registry::RegisterAssetsArgs {
            prefix: "/app",
            dir: &site,
            root_guard: None,
            index: Some("index.html"),
            fallback: Some("index.html"),
            host_id: 1,
            owner_session_id: sid,
            plugin: None,
            declarative: false,
        })
        .unwrap();

    match reg.resolve_asset("GET", "/app/main.js") {
        mare_host::AssetResolve::File { content_type, bytes, head_only } => {
            assert_eq!(content_type, "application/javascript; charset=utf-8");
            assert_eq!(bytes, b"console.log(1);");
            assert!(!head_only);
        }
        other => panic!("unexpected resolve: {other:?}"),
    }
    match reg.resolve_asset("HEAD", "/app/") {
        mare_host::AssetResolve::File { content_type, head_only, .. } => {
            assert_eq!(content_type, "text/html; charset=utf-8");
            assert!(head_only);
        }
        other => panic!("unexpected HEAD resolve: {other:?}"),
    }
    assert!(matches!(reg.resolve_asset("POST", "/app/main.js"), mare_host::AssetResolve::MethodNotAllowed));
    assert!(matches!(reg.resolve_asset("GET", "/app/%2e%2e/secret.txt"), mare_host::AssetResolve::BadRequest(_)));
    #[cfg(unix)]
    assert!(matches!(reg.resolve_asset("GET", "/app/escape.txt"), mare_host::AssetResolve::NotFound));
    match reg.resolve_asset("GET", "/app/dashboard") {
        mare_host::AssetResolve::File { content_type, bytes, .. } => {
            assert_eq!(content_type, "text/html; charset=utf-8");
            assert_eq!(bytes, b"<h1>app</h1>");
        }
        other => panic!("SPA fallback failed: {other:?}"),
    }
    match reg.resolve_asset("GET", "/app/v1.0/dashboard") {
        mare_host::AssetResolve::File { content_type, bytes, .. } => {
            assert_eq!(content_type, "text/html; charset=utf-8");
            assert_eq!(bytes, b"<h1>app</h1>");
        }
        other => panic!("SPA fallback with dotted parent segment failed: {other:?}"),
    }
    assert!(matches!(reg.resolve_asset("GET", "/app/v1.0/missing.js"), mare_host::AssetResolve::NotFound));
    assert!(reg.unregister_asset(&asset_id));
}

struct WebHomeEnvGuard(Option<std::ffi::OsString>);

impl WebHomeEnvGuard {
    fn set(val: &std::path::Path) -> Self {
        let prev = std::env::var_os("MARE_HOME");
        // SAFETY: callers hold `HOST_LOCK`.
        unsafe { std::env::set_var("MARE_HOME", val) };
        Self(prev)
    }
}

impl Drop for WebHomeEnvGuard {
    fn drop(&mut self) {
        // SAFETY: paired with `WebHomeEnvGuard::set` under `HOST_LOCK`.
        unsafe {
            match &self.0 {
                Some(v) => std::env::set_var("MARE_HOME", v),
                None => std::env::remove_var("MARE_HOME"),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WsCloseOutcome {
    Closed(Option<u16>),
    Error(String),
    Eof,
}

async fn read_ws_close<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, reply_ping: bool) -> WsCloseOutcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await {
            Some(Ok(Message::Ping(p))) if reply_ping => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Some(Ok(Message::Close(cf))) => return WsCloseOutcome::Closed(cf.map(|c| u16::from(c.code))),
            Some(Err(e)) => return WsCloseOutcome::Error(e.to_string()),
            None => return WsCloseOutcome::Eof,
            _ => {}
        }
    }
}

#[test]
fn ws_overload_drain_wait_covers_inflight_send_and_close_flush() {
    let required = crate::web::WS_SEND_TIMEOUT + crate::web::OVERLOAD_CLOSE_TIMEOUT + crate::web::WS_SHUTDOWN_TIMEOUT;
    assert!(
        WRITER_OVERLOAD_DRAIN_WAIT >= required,
        "WRITER_OVERLOAD_DRAIN_WAIT ({WRITER_OVERLOAD_DRAIN_WAIT:?}) must cover WS_SEND_TIMEOUT + OVERLOAD_CLOSE_TIMEOUT + WS_SHUTDOWN_TIMEOUT ({required:?}) so serve_with_io does not abort ws_pump before Close(1013) flushes"
    );
}

#[tokio::test]
async fn ws_close_4001_on_token_rotation_and_1013_on_overload() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = WebHomeEnvGuard::set(home.path());
            let tok_path = mare_shared::paths::host_token_file();
            let initial_tok = rotate_host_token_at(&tok_path).unwrap();

            let work = tempfile::tempdir().unwrap();
            let opts = HostOptions {
                cwd: work.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "ws_ping_interval_ms": 50,
                        "ws_pong_timeout_ms": 5000,
                        "accept_partial": ["fs_host_protect"]
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            // 1. Token rotation on RuntimeFile closes active WS client with 4001.
            let ws_url1 = format!("ws://{addr}/rpc?token={initial_tok}");
            let (mut ws1, _) = tokio_tungstenite::connect_async(ws_url1).await.unwrap();
            ws1.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws1).await;
            let rotated_tok = rotate_host_token_at(&tok_path).unwrap();
            assert_ne!(rotated_tok, initial_tok);
            let close_code_4001 = tokio::time::timeout(Duration::from_secs(3), read_ws_close(&mut ws1, true))
                .await
                .expect("ws1 close after token rotation timed out");
            assert_eq!(close_code_4001, WsCloseOutcome::Closed(Some(4001)), "token rotation must close live WS with 4001");

            // 2. Slow-consumer output overflow closes WS client with 1013 (TryAgainLater).
            let ws_url2 = format!("ws://{addr}/rpc?token={rotated_tok}");
            let (mut ws2, _) = tokio_tungstenite::connect_async(ws_url2).await.unwrap();
            ws2.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws2).await;
            ws2.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 2, "method": "events.subscribe", "params": { "scope": "all", "include": ["custom/flood"] } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text_matching(&mut ws2, "\"id\":2").await;

            // Emit one real subscribed event, then saturate OUT_BYTES (64 MiB) directly on the live
            // WS connection's outbound queue without reading ws2 so TCP backpressures ws_pump and
            // out_budget overflows -> 1013 well within WS_SEND_TIMEOUT (5s) even on loaded CI runners.
            rt.host.emit_json("custom/flood", json!({ "probe": true })).await;
            tokio::task::yield_now().await;
            let ws2_conn = rpc
                .connections
                .borrow()
                .values()
                .filter_map(|w| w.upgrade())
                .find(|c| !c.overflowed.get())
                .expect("live ws2 connection");
            let pad = "x".repeat(896 * 1024);
            let flood_params = json!({ "event": "custom/flood", "data": { "pad": pad } }).to_string();
            for _ in 0..128 {
                if ws2_conn.overflowed.get() {
                    break;
                }
                ws2_conn.send_raw_event(&flood_params);
            }
            assert!(ws2_conn.overflowed.get(), "ws2 connection must overflow out_budget during flood");
            drop(ws2_conn);
            let close_code_1013 = tokio::time::timeout(Duration::from_secs(10), read_ws_close(&mut ws2, false))
                .await
                .expect("ws2 close after overload timed out");
            assert_eq!(
                close_code_1013,
                WsCloseOutcome::Closed(Some(1013)),
                "overloaded WS consumer must close with 1013"
            );

            // 3. Binary WebSocket frame is refused with close code 1003 (Unsupported).
            let ws_url3 = format!("ws://{addr}/rpc?token={rotated_tok}");
            let (mut ws3, _) = tokio_tungstenite::connect_async(ws_url3).await.unwrap();
            ws3.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
            let close_code_1003 = tokio::time::timeout(Duration::from_secs(3), read_ws_close(&mut ws3, false))
                .await
                .expect("ws3 binary close timed out");
            assert_eq!(close_code_1003, WsCloseOutcome::Closed(Some(1003)), "binary WS frame must close with 1003");

            srv.shutdown().await;
            let _ = rpc.manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn token_rotation_rejects_inbound_rpc_immediately_under_default_ping_interval() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = WebHomeEnvGuard::set(home.path());
            let tok_path = mare_shared::paths::host_token_file();
            let initial_tok = rotate_host_token_at(&tok_path).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "ws_ping_interval_ms": 15_000,
                        "ws_pong_timeout_ms": 10_000,
                        "accept_partial": ["fs_host_protect"]
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();

            let ws_url = format!("ws://{}/rpc?token={initial_tok}", srv.addr);
            let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws).await;

            // Rotate token under default 15s ping interval, then immediately send inbound RPC.
            let _rotated = rotate_host_token_at(&tok_path).unwrap();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 2, "method": "webserver.get", "params": {} })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

            let outcome = tokio::time::timeout(Duration::from_millis(800), async {
                loop {
                    match ws.next().await {
                        Some(Ok(Message::Text(t))) => panic!("revoked token must not execute inbound RPC: {t}"),
                        Some(Ok(Message::Close(cf))) => break cf.map(|c| u16::from(c.code)),
                        Some(Err(e)) => panic!("unexpected ws err after rotation: {e}"),
                        None => panic!("expected Close(4001), got EOF"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("inbound RPC on rotated token must immediately trigger Close(4001) without waiting 15s ping");
            assert_eq!(outcome, Some(4001));

            srv.shutdown().await;
            let _ = rpc.manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn webserver_plugin_reload_reclaims_routes_and_assets_via_lib_owned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let plugins_dir = dir.path().join(".mare/plugins");
            let assets_dir = dir.path().join("plug-ui");
            std::fs::create_dir_all(&plugins_dir).unwrap();
            std::fs::create_dir_all(&assets_dir).unwrap();
            std::fs::write(assets_dir.join("index.html"), "<h1>plugin-v1</h1>").unwrap();
            let plugin_file = plugins_dir.join("testweb.lua");
            std::fs::write(
                &plugin_file,
                r#"return {
    name = "testweb",
    inject = { "webserver" },
    apply = function(ctx)
        ctx.webserver:register_route("/plug-route", {
            method = "GET",
            handle = function()
                return { status = 200, body = "plug-v1" }
            end,
        })
        ctx.webserver:register_assets("plug-ui", { prefix = "/plug-ui" })
    end,
}"#,
            )
            .unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-plug-token-0123456789abcdef"
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();

            let routes = rpc.manager.web().list_routes();
            assert_eq!(routes.len(), 1, "expected 1 plugin route: {routes:?}");
            assert_eq!(routes[0]["plugin"], "testweb", "plugin attribution missing: {routes:?}");
            let assets = rpc.manager.web().list_assets();
            assert!(assets.iter().any(|a| a["prefix"] == "/plug-ui" && a["plugin"] == "testweb"), "{assets:?}");

            // Reload plugin -> old route/asset reclaimed so re-registration does not conflict!
            std::fs::write(
                &plugin_file,
                r#"return {
    name = "testweb",
    inject = { "webserver" },
    apply = function(ctx)
        ctx.webserver:register_route("/plug-route", {
            method = "GET",
            handle = function()
                return { status = 200, body = "plug-v2" }
            end,
        })
        ctx.webserver:register_assets("plug-ui", { prefix = "/plug-ui" })
    end,
}"#,
            )
            .unwrap();
            rt.host.kernel.reload(Some("testweb")).await.unwrap();
            let r2 = raw_http(
                srv.addr,
                &format!(
                    "GET /plug-route HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer test-plug-token-0123456789abcdef\r\nConnection: close\r\n\r\n",
                    srv.addr
                ),
            )
            .await;
            assert!(r2.starts_with("HTTP/1.1 200") && r2.contains("plug-v2"), "reload must replace route cleanly: {r2}");

            // Disable/reload empty -> route and assets reclaimed (404)
            std::fs::write(&plugin_file, "return { name = \"testweb\", apply = function() end }\n").unwrap();
            rt.host.kernel.reload(Some("testweb")).await.unwrap();
            assert!(rpc.manager.web().list_routes().is_empty(), "routes leaked after reload");
            let a_after = rpc.manager.web().list_assets();
            assert!(!a_after.iter().any(|a| a["prefix"] == "/plug-ui"), "assets leaked after reload: {a_after:?}");

            srv.shutdown().await;
            let _ = rpc.manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn declarative_web_assets_refcounted_across_multi_session_hosts() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let plugins_dir = dir.path().join(".mare/plugins/declweb");
            let ui_dir = plugins_dir.join("dist");
            std::fs::create_dir_all(&ui_dir).unwrap();
            std::fs::write(ui_dir.join("index.html"), "<h1>shared-spa</h1>").unwrap();
            std::fs::write(
                plugins_dir.join("init.lua"),
                r#"return {
    name = "declweb",
    web = { dir = "dist", fallback = "index.html" },
    apply = function() end,
}"#,
            )
            .unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: false,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(opts.clone()).await.unwrap();
            let sid1 = rt1.session_id.clone();
            let rt2 = manager.create_session(opts.clone()).await.unwrap();
            let sid2 = rt2.session_id.clone();

            let problems2 = rt2.host.kernel.problems();
            assert!(
                !problems2.iter().any(|p| p.contains("asset_conflict")),
                "second session must not hit asset_conflict: {problems2:?}"
            );
            let assets = manager.web().list_assets();
            assert_eq!(
                assets.iter().filter(|a| a["prefix"] == "/plugins/declweb").count(),
                1,
                "shared declarative mount should be deduplicated: {assets:?}"
            );

            // Closing session 1 must keep the mount alive while session 2 is open.
            manager.close_session(&sid1).await.unwrap();
            match manager.web().resolve_asset("GET", "/plugins/declweb/index.html") {
                mare_host::AssetResolve::File { bytes, .. } => {
                    assert_eq!(String::from_utf8_lossy(&bytes), "<h1>shared-spa</h1>");
                }
                other => panic!("expected File while session 2 still owns mount, got {other:?}"),
            }

            // Closing session 2 (last owner) removes the mount.
            manager.close_session(&sid2).await.unwrap();
            assert!(
                matches!(
                    manager.web().resolve_asset("GET", "/plugins/declweb/index.html"),
                    mare_host::AssetResolve::NotMatched
                ),
                "mount must be removed after last owning session closes"
            );

            let _ = manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn ws_slow_send_shutdown_and_max_connections_and_oversized_1009() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-conn-token-0123456789abcdef",
                        "max_connections": 1,
                        "ws_max_message_bytes": 1024
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            // 1. Open WS 1 occupies the single max_connections = 1 permit even after 101 upgrade.
            let ws_url = format!("ws://{addr}/rpc?token=test-conn-token-0123456789abcdef");
            let (mut ws1, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
            ws1.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws1).await;

            // 2nd HTTP /readyz must stall while ws1 holds the permit.
            let addr_probe = addr;
            let probe = tokio::task::spawn_local(async move {
                raw_http(addr_probe, &format!("GET /readyz HTTP/1.1\r\nHost: {addr_probe}\r\nConnection: close\r\n\r\n")).await
            });
            tokio::time::sleep(Duration::from_millis(80)).await; // sleep-ok: verify max_connections=1 blocks 2nd connection while WS1 is open
            assert!(!probe.is_finished(), "open WS must hold max_connections permit until closed");

            // 2. Sending oversized WS frame > 1024 bytes triggers Close(1009) and releases permit.
            let big = "a".repeat(2048);
            let _ = ws1.send(Message::Text(big.into())).await;
            let close_1009 = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match ws1.next().await {
                        Some(Ok(Message::Close(cf))) => break cf.map(|c| u16::from(c.code)),
                        Some(Err(_)) | None => break None,
                        _ => {}
                    }
                }
            })
            .await
            .expect("oversized frame close timed out");
            assert_eq!(close_1009, Some(1009), "oversized WS frame must close with 1009 Size");
            drop(ws1);

            let ready2 = tokio::time::timeout(Duration::from_secs(3), probe)
                .await
                .expect("probe did not unblock after WS1 close")
                .unwrap();
            assert!(ready2.starts_with("HTTP/1.1 200"), "{ready2}");

            // 3. Stalled outbound sink.send + server shutdown terminates promptly without hanging.
            let (mut ws_stall, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
            ws_stall.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws_stall).await;
            ws_stall.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 2, "method": "events.subscribe", "params": { "scope": "all", "include": ["custom/flood"] } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text_matching(&mut ws_stall, "\"id\":2").await;

            // Flood without reading from ws_stall so sink.send blocks on TCP buffer, then shut down!
            let flood_payload = json!({ "pad": "y".repeat(256 * 1024) });
            for _ in 0..40 {
                rt.host.emit_json("custom/flood", flood_payload.clone()).await;
            }
            tokio::time::timeout(Duration::from_secs(3), srv.shutdown())
                .await
                .expect("srv.shutdown must not hang on stalled WS sink.send");
            tokio::time::timeout(Duration::from_secs(2), async {
                while !rpc.connections.borrow().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("stalled WS connection must exit and leave rpc.connections empty after srv.shutdown");
            tokio::time::timeout(Duration::from_secs(3), rpc.manager.shutdown_all())
                .await
                .expect("shutdown_all timed out");
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn tls_handshake_timeout_and_http_keepalive_shutdown() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let cert_path = dir.path().join("cert.pem");
            let key_path = dir.path().join("key.pem");
            std::fs::write(&cert_path, TEST_CERT_PEM).unwrap();
            std::fs::write(&key_path, TEST_KEY_PEM).unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-tls-timeout-token-01234567",
                        "max_connections": 1,
                        "header_read_timeout_ms": 80,
                        "tls_cert": cert_path,
                        "tls_key": key_path
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();

            // 1. Stalled TCP client sends no TLS ClientHello -> timed out after ~80ms, freeing max_connections=1 permit.
            let mut silent_tcp = TcpStream::connect(srv.addr).await.unwrap();
            let mut eof_buf = [0u8; 16];
            let n = tokio::time::timeout(Duration::from_secs(2), silent_tcp.read(&mut eof_buf))
                .await
                .expect("stalled TLS handshake must time out cleanly")
                .unwrap();
            assert_eq!(n, 0, "stalled TLS socket should close cleanly");

            // 2. Keep-alive TLS HTTP connection remains open after 200; srv.shutdown() closes it cleanly.
            let client_cfg = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyTestCert))
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(client_cfg));
            let tcp2 = TcpStream::connect(srv.addr).await.unwrap();
            let mut tls2 = connector.connect(ServerName::try_from("localhost").unwrap(), tcp2).await.unwrap();
            let req = format!(
                "GET /readyz HTTP/1.1\r\nHost: localhost:{}\r\nConnection: keep-alive\r\n\r\n",
                srv.addr.port()
            );
            tls2.write_all(req.as_bytes()).await.unwrap();
            let mut head = vec![0u8; 256];
            let read_n = tokio::time::timeout(Duration::from_secs(2), tls2.read(&mut head)).await.unwrap().unwrap();
            let head_str = String::from_utf8_lossy(&head[..read_n]);
            assert!(head_str.starts_with("HTTP/1.1 200"), "{head_str}");

            tokio::time::timeout(Duration::from_secs(3), srv.shutdown())
                .await
                .expect("srv.shutdown must interrupt idle keep-alive TLS/HTTP connections");
            let mut keepalive_eof = [0u8; 16];
            let eof_n = tokio::time::timeout(Duration::from_secs(2), tls2.read(&mut keepalive_eof))
                .await
                .expect("idle keep-alive TLS/HTTP connection must close within 2s after srv.shutdown")
                .unwrap();
            assert_eq!(eof_n, 0, "expected clean EOF on idle keep-alive TLS socket after shutdown");
            tokio::time::timeout(Duration::from_secs(3), rpc.manager.shutdown_all())
                .await
                .expect("shutdown_all timed out");
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn plaintext_listener_behind_https_reverse_proxy_allows_same_authority_origin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-proxy-token-0123456789abcdef",
                        "trusted_hosts": ["mare.example.test"]
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            assert!(!srv.tls);

            // Stage A HTTPS reverse proxy / Tailscale Serve forwarding to plaintext loopback listener:
            // Host: mare.example.test + Origin: https://mare.example.test must succeed (200), not 403!
            let proxy_ready = raw_http(
                srv.addr,
                "GET /readyz HTTP/1.1\r\nHost: mare.example.test\r\nOrigin: https://mare.example.test\r\nConnection: close\r\n\r\n",
            )
            .await;
            assert!(proxy_ready.starts_with("HTTP/1.1 200"), "HTTPS reverse proxy Origin must be accepted: {proxy_ready}");

            // Same-authority WS upgrade behind HTTPS reverse proxy must succeed too.
            let mut ws_req = format!("ws://{}/rpc?token=test-proxy-token-0123456789abcdef", srv.addr)
                .into_client_request()
                .unwrap();
            ws_req.headers_mut().insert("host", http::HeaderValue::from_static("mare.example.test"));
            ws_req.headers_mut().insert("origin", http::HeaderValue::from_static("https://mare.example.test"));
            let (mut ws, _) = tokio_tungstenite::connect_async(ws_req).await.expect("HTTPS proxy WS upgrade must succeed");
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let init_txt = ws_next_text(&mut ws).await;
            assert!(init_txt.contains("\"id\":1"), "{init_txt}");
            drop(ws);

            // Cross-authority Origin must still be rejected 403.
            let cross_origin = raw_http(
                srv.addr,
                "GET /readyz HTTP/1.1\r\nHost: mare.example.test\r\nOrigin: https://evil.example\r\nConnection: close\r\n\r\n",
            )
            .await;
            assert!(cross_origin.starts_with("HTTP/1.1 403"), "cross-authority Origin must remain 403: {cross_origin}");

            tokio::time::timeout(Duration::from_secs(3), srv.shutdown()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), rpc.manager.shutdown_all()).await.unwrap();
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn head_asset_validates_without_reading_body_and_get_serves_off_localset() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let site = dir.path().join("site");
            std::fs::create_dir_all(&site).unwrap();
            let large_payload = vec![b'x'; 512 * 1024];
            std::fs::write(site.join("large.bin"), &large_payload).unwrap();
            // Sparse 33 MiB file exceeding MAX_STATIC_FILE_BYTES (32 MiB): HEAD must reject 413 without reading.
            let oversized = std::fs::File::create(site.join("oversized.bin")).unwrap();
            oversized.set_len(33 * 1024 * 1024).unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-head-token-0123456789abcdef"
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = Rc::new(std::cell::RefCell::new(Some(rt.session_id.clone())));
            manager
                .web()
                .register_assets(mare_host::web_registry::RegisterAssetsArgs {
                    prefix: "/assets",
                    dir: &site,
                    root_guard: None,
                    index: None,
                    fallback: None,
                    host_id: rt.host.instance_id,
                    owner_session_id: sid,
                    plugin: None,
                    declarative: false,
                })
                .unwrap();

            match manager.web().resolve_asset("HEAD", "/assets/large.bin") {
                mare_host::AssetResolve::File { content_type, bytes, head_only } => {
                    assert_eq!(content_type, "application/octet-stream");
                    assert!(head_only);
                    assert!(bytes.is_empty(), "HEAD must not read body bytes into memory");
                }
                other => panic!("unexpected HEAD resolve: {other:?}"),
            }
            assert!(matches!(
                manager.web().resolve_asset("HEAD", "/assets/oversized.bin"),
                mare_host::AssetResolve::TooLarge
            ));

            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let token = "test-head-token-0123456789abcdef";

            let head_res = raw_http(
                srv.addr,
                &format!(
                    "HEAD /assets/large.bin HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n",
                    srv.addr
                ),
            )
            .await;
            assert!(head_res.starts_with("HTTP/1.1 200"), "{head_res}");
            assert!(head_res.ends_with("\r\n\r\n"), "HEAD must return empty body: {head_res:?}");

            let oversized_res = raw_http(
                srv.addr,
                &format!(
                    "HEAD /assets/oversized.bin HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n",
                    srv.addr
                ),
            )
            .await;
            assert!(oversized_res.starts_with("HTTP/1.1 413"), "{oversized_res}");

            let get_res = raw_http(
                srv.addr,
                &format!(
                    "GET /assets/large.bin HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n",
                    srv.addr
                ),
            )
            .await;
            assert!(get_res.starts_with("HTTP/1.1 200"), "{get_res}");
            let (_, body) = get_res.split_once("\r\n\r\n").unwrap();
            assert_eq!(body.len(), large_payload.len());

            tokio::time::timeout(Duration::from_secs(3), srv.shutdown()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), rpc.manager.shutdown_all()).await.unwrap();
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn queued_asset_lookup_returns_404_when_mount_is_unregistered_or_reregistered_mid_read() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let site_a = dir.path().join("site_a");
            let site_b = dir.path().join("site_b");
            std::fs::create_dir_all(&site_a).unwrap();
            std::fs::create_dir_all(&site_b).unwrap();
            std::fs::write(site_a.join("data.txt"), b"from-mount-a").unwrap();
            std::fs::write(site_b.join("data.txt"), b"from-mount-b").unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "token": "test-race-token-0123456789abcdef"
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = Rc::new(std::cell::RefCell::new(Some(rt.session_id.clone())));
            let id_a = manager
                .web()
                .register_assets(mare_host::web_registry::RegisterAssetsArgs {
                    prefix: "/app",
                    dir: &site_a,
                    root_guard: None,
                    index: None,
                    fallback: None,
                    host_id: rt.host.instance_id,
                    owner_session_id: sid.clone(),
                    plugin: None,
                    declarative: false,
                })
                .unwrap();

            let rpc = RpcRuntime::new(manager.clone(), opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let token = "test-race-token-0123456789abcdef";

            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            crate::web::before_next_asset_read(move || {
                let _ = entered_tx.send(());
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            });

            let addr = srv.addr;
            let raw_req = format!(
                "GET /app/data.txt HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            );
            let req_fut = tokio::task::spawn_local(async move { raw_http(addr, &raw_req).await });

            // Wait until the blocking read task has entered, then unregister mount A and replace /app with mount B.
            tokio::time::timeout(Duration::from_secs(3), entered_rx).await.unwrap().unwrap();
            assert!(manager.web().unregister_asset(&id_a));
            manager
                .web()
                .register_assets(mare_host::web_registry::RegisterAssetsArgs {
                    prefix: "/app",
                    dir: &site_b,
                    root_guard: None,
                    index: None,
                    fallback: None,
                    host_id: rt.host.instance_id,
                    owner_session_id: sid,
                    plugin: None,
                    declarative: false,
                })
                .unwrap();
            release_tx.send(()).unwrap();

            let raced_res = tokio::time::timeout(Duration::from_secs(3), req_fut).await.unwrap().unwrap();
            assert!(
                raced_res.starts_with("HTTP/1.1 404"),
                "stale queued lookup after mount replacement must return 404, got: {raced_res}"
            );

            // A fresh request sees the newly registered mount B.
            let fresh_res = raw_http(
                addr,
                &format!(
                    "GET /app/data.txt HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
                ),
            )
            .await;
            assert!(fresh_res.starts_with("HTTP/1.1 200"), "{fresh_res}");
            assert!(fresh_res.ends_with("from-mount-b"), "{fresh_res}");

            tokio::time::timeout(Duration::from_secs(3), srv.shutdown()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), rpc.manager.shutdown_all()).await.unwrap();
            drop(guard);
        })
        .await;
}

/// Receiving a malformed WebSocket frame (protocol error, e.g. unmasked client frame per RFC 6455)
/// must elicit a best-effort WebSocket Close(1002 Protocol Error) frame before closing.
#[tokio::test]
async fn ws_protocol_error_closes_with_1002() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let token = "test-ws1002-token-0123456789abcdef";
            let (_dir, _guard, rpc, _cr, _cw, task, _sid) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
                "webserver": {
                    "enabled": true,
                    "host": "127.0.0.1",
                    "port": 0,
                    "token": token
                }
            }))
            .await;

            let cfg = rpc.anchor_host().unwrap().config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            // Perform raw HTTP/1.1 WebSocket upgrade so we can inject a raw unmasked client frame.
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let upgrade = format!(
                "GET /rpc HTTP/1.1\r\n\
                 Host: {addr}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 Authorization: Bearer {token}\r\n\r\n"
            );
            stream.write_all(upgrade.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();

            // Read HTTP 101 response headers.
            let mut hdr = Vec::new();
            let mut tmp = [0u8; 1];
            tokio::time::timeout(Duration::from_secs(3), async {
                while !hdr.ends_with(b"\r\n\r\n") {
                    let n = stream.read(&mut tmp).await.unwrap();
                    assert!(n > 0, "EOF during WS handshake");
                    hdr.push(tmp[0]);
                }
            })
            .await
            .expect("WS handshake timeout");
            let hdr_str = String::from_utf8_lossy(&hdr);
            assert!(hdr_str.starts_with("HTTP/1.1 101"), "expected 101 upgrade, got: {hdr_str}");

            // Send an unmasked client text frame (FIN=1, opcode=1, MASK=0, len=2, "hi") -> RFC 6455 protocol error.
            stream.write_all(&[0x81, 0x02, b'h', b'i']).await.unwrap();
            stream.flush().await.unwrap();

            // Read server's close frame and verify status code 1002.
            let mut raw = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut raw)).await;
            let close_code = if raw.len() >= 4 && (raw[0] & 0x0f) == 0x08 && (raw[1] & 0x7f) >= 2 {
                Some(u16::from_be_bytes([raw[2], raw[3]]))
            } else {
                None
            };
            assert_eq!(
                close_code,
                Some(1002),
                "WebSocket protocol error must close with 1002 (Protocol Error), raw server bytes: {raw:?}"
            );

            srv.shutdown().await;
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Without an explicit `ws_max_frame_bytes` override, the WebSocket single-frame size limit must
/// default to the message size limit (`MAX_LINE` = 64 MiB), accepting a 17 MiB single frame.
#[tokio::test]
async fn ws_frame_limit_defaults_to_message_limit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let token = "test-wsframe-token-0123456789abcdef";
            let (_dir, _guard, rpc, _cr, _cw, task, _sid) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
                "webserver": {
                    "enabled": true,
                    "host": "127.0.0.1",
                    "port": 0,
                    "token": token
                }
            }))
            .await;

            let cfg = rpc.anchor_host().unwrap().config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            let ws_url = format!("ws://{addr}/rpc?token={token}");
            let mut client_cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
            client_cfg.max_message_size = Some(crate::rpc::serve::MAX_LINE);
            client_cfg.max_frame_size = Some(crate::rpc::serve::MAX_LINE);
            let (mut ws, _) =
                tokio_tungstenite::connect_async_with_config(&ws_url, Some(client_cfg), false).await.unwrap();

            ws.send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let init_txt = ws_next_text(&mut ws).await;
            let init_val: J = serde_json::from_str(&init_txt).unwrap();
            assert!(init_val["error"].is_null(), "{init_val}");

            // Send a single 17 MiB JSON-RPC text frame (> old 16 MiB frame cap, < 64 MiB message limit).
            let pad = 17 * 1024 * 1024;
            let mut big_req = String::with_capacity(pad + 96);
            big_req.push_str(r#"{"jsonrpc":"2.0","id":2,"method":"peer.info","params":{}"#);
            big_req.extend(std::iter::repeat_n(' ', pad));
            big_req.push('}');
            let send_res = ws.send(Message::Text(big_req.into())).await;

            let reply = if let Err(send_err) = send_res {
                Err(format!("sending 17 MiB single frame failed (server closed early): {send_err}"))
            } else {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        match ws.next().await {
                            Some(Ok(Message::Text(t))) => {
                                let v: J = serde_json::from_str(&t).unwrap();
                                if v["id"] == 2 {
                                    break Ok(v);
                                }
                            }
                            Some(Ok(Message::Ping(p))) => {
                                ws.send(Message::Pong(p)).await.unwrap();
                            }
                            Some(Ok(Message::Close(cf))) => {
                                break Err(format!(
                                    "server closed connection instead of accepting 17 MiB frame: {cf:?}"
                                ));
                            }
                            other => break Err(format!("unexpected ws outcome on 17 MiB frame: {other:?}")),
                        }
                    }
                })
                .await
                .expect("timed out waiting for 17 MiB frame reply")
            };

            let val =
                reply.expect("default ws_max_frame_bytes must follow message limit and accept 17 MiB single frame");
            assert!(val["error"].is_null(), "17 MiB peer.info reply must succeed: {val}");

            let _ = ws.close(None).await;
            srv.shutdown().await;
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Plain HTTP is allowed when binding one explicit concrete address (including a concrete LAN IP)
/// with `plain_http = true`, and wildcard binds (`0.0.0.0`, `::`, empty host) are always refused
/// regardless of `plain_http` or TLS.
#[tokio::test]
async fn plain_http_binds_one_explicit_address_and_refuses_wildcards() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let token = "test-plainhttp-token-0123456789abcdef";
            let (dir, _guard, rpc, _cr, _cw, task, _sid) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
            }))
            .await;

            let cert_path = dir.path().join("cert.pem");
            let key_path = dir.path().join("key.pem");
            std::fs::write(&cert_path, TEST_CERT_PEM).unwrap();
            std::fs::write(&key_path, TEST_KEY_PEM).unwrap();

            // 1. Wildcard binds (including IPv4-mapped unspecified, zone-qualified unspecified, and `*`) are ALWAYS refused (with plain_http=true and even with TLS).
            for wildcard in ["0.0.0.0", "::", "::ffff:0.0.0.0", "::%lo", "*", ""] {
                let err_plain = start_host_webserver(
                    &rpc,
                    &json!({
                        "webserver": {
                            "enabled": true,
                            "host": wildcard,
                            "port": 0,
                            "token": token,
                            "plain_http": true
                        }
                    }),
                    None,
                )
                .await
                .err()
                .unwrap_or_else(|| panic!("wildcard `{wildcard}` with plain_http=true must be refused"));
                assert!(err_plain.contains("wildcard"), "unexpected wildcard error for `{wildcard}`: {err_plain}");

                let err_tls = start_host_webserver(
                    &rpc,
                    &json!({
                        "webserver": {
                            "enabled": true,
                            "host": wildcard,
                            "port": 0,
                            "token": token,
                            "tls_cert": cert_path,
                            "tls_key": key_path
                        }
                    }),
                    None,
                )
                .await
                .err()
                .unwrap_or_else(|| panic!("wildcard `{wildcard}` with TLS must be refused"));
                assert!(err_tls.contains("wildcard"), "unexpected TLS wildcard error for `{wildcard}`: {err_tls}");
            }

            // 2. Non-loopback concrete address WITHOUT plain_http/TLS is refused with explicit single-address + plain_http guidance.
            let err_no_plain = start_host_webserver(
                &rpc,
                &json!({
                    "webserver": {
                        "enabled": true,
                        "host": "192.0.2.10",
                        "port": 0,
                        "token": token,
                        "plain_http": false
                    }
                }),
                None,
            )
            .await
            .err()
            .expect("non-loopback bind without TLS or plain_http must be refused");
            assert!(
                err_no_plain.contains("plain_http") && !err_no_plain.contains("encrypted tunnel"),
                "refusal message must state explicit single-address + plain_http policy rather than tunnel-only wording: {err_no_plain}"
            );

            // 3. Concrete non-loopback address WITH plain_http = true is permitted by policy (not loopback-only):
            //    if the host has a concrete non-loopback IPv4, actually bind it; otherwise verify policy passes to TcpListener::bind.
            let local_lan_ip = std::net::UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| {
                    s.connect("192.0.2.1:80")?;
                    s.local_addr()
                })
                .ok()
                .map(|a| a.ip())
                .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());

            if let Some(lan_ip) = local_lan_ip {
                let mut srv = start_host_webserver(
                    &rpc,
                    &json!({
                        "webserver": {
                            "enabled": true,
                            "host": lan_ip.to_string(),
                            "port": 0,
                            "token": token,
                            "plain_http": true
                        }
                    }),
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("plain_http=true on concrete LAN IP {lan_ip} must succeed: {e}"));
                assert_eq!(srv.addr.ip(), lan_ip);
                assert!(!srv.tls);
                srv.shutdown().await;
            } else {
                let err_bind = start_host_webserver(
                    &rpc,
                    &json!({
                        "webserver": {
                            "enabled": true,
                            "host": "192.0.2.10",
                            "port": 0,
                            "token": token,
                            "plain_http": true
                        }
                    }),
                    None,
                )
                .await
                .err()
                .expect("unassigned TEST-NET IP should fail at OS bind, not policy");
                assert!(
                    err_bind.starts_with("cannot listen on 192.0.2.10:0"),
                    "plain_http=true on concrete non-loopback IP must pass policy to bind: {err_bind}"
                );
            }

            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Non-loopback `plain_http = true` requires a concrete IPv4/IPv6 literal (with optional `%zone`),
/// rejecting hostnames, while `localhost` stays on the loopback path.
#[tokio::test]
async fn plain_http_non_loopback_requires_ip_literal_and_allows_localhost_and_zone() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let token = "test-ipliteral-token-0123456789abcdef";
            let (_dir, _guard, rpc, _cr, _cw, task, _sid) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
            }))
            .await;

            // 1. Non-loopback hostname with plain_http = true must be refused by policy (never reaching OS bind/DNS).
            let err_hostname = start_host_webserver(
                &rpc,
                &json!({
                    "webserver": {
                        "enabled": true,
                        "host": "mybox.invalid",
                        "port": 0,
                        "token": token,
                        "plain_http": true
                    }
                }),
                None,
            )
            .await
            .err()
            .expect("non-loopback hostname with plain_http=true must be refused");
            assert!(
                !err_hostname.starts_with("cannot listen on")
                    && (err_hostname.contains("IP") || err_hostname.contains("literal")),
                "non-loopback hostname must fail IP-literal policy check before bind: {err_hostname}"
            );

            // 2. `localhost` without TLS or plain_http still takes the loopback branch and starts cleanly.
            let mut srv_local = start_host_webserver(
                &rpc,
                &json!({
                    "webserver": {
                        "enabled": true,
                        "host": "localhost",
                        "port": 0,
                        "token": token,
                        "plain_http": false
                    }
                }),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("localhost must remain allowed on loopback branch: {e}"));
            assert!(srv_local.addr.ip().is_loopback());
            srv_local.shutdown().await;

            // 3. Concrete IPv6 literal with `%zone` passes the IP-literal policy check and reaches OS bind.
            let err_zone = start_host_webserver(
                &rpc,
                &json!({
                    "webserver": {
                        "enabled": true,
                        "host": "fe80::1%nonexistentzone0",
                        "port": 0,
                        "token": token,
                        "plain_http": true
                    }
                }),
                None,
            )
            .await
            .err()
            .expect("non-existent zone literal should fail at OS bind, not IP-literal policy");
            assert!(
                err_zone.starts_with("cannot listen on fe80::1%nonexistentzone0:0"),
                "zone-qualified IPv6 literal must pass policy to bind: {err_zone}"
            );

            // 4. IPv4 literal with `%zone` is NOT valid (only IPv6 allows `%zone`) and must fail policy check.
            let err_v4_zone = start_host_webserver(
                &rpc,
                &json!({
                    "webserver": {
                        "enabled": true,
                        "host": "192.0.2.10%eth0",
                        "port": 0,
                        "token": token,
                        "plain_http": true
                    }
                }),
                None,
            )
            .await
            .err()
            .expect("IPv4 literal with %zone must be refused");
            assert!(
                !err_v4_zone.starts_with("cannot listen on")
                    && (err_v4_zone.contains("IP") || err_v4_zone.contains("literal")),
                "IPv4 with %zone must fail IP-literal policy check before bind: {err_v4_zone}"
            );

            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Invalid UTF-8 in a WebSocket text frame closes with 1007 (Invalid), while an abrupt peer EOF
/// without a closing handshake (`ResetWithoutClosingHandshake`) is treated as clean EOF rather than
/// sending a 1002 protocol-error close frame.
#[tokio::test]
async fn ws_invalid_utf8_closes_with_1007_and_abrupt_reset_is_clean_eof() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let token = "test-ws1007-token-0123456789abcdef";
            let (_dir, _guard, rpc, _cr, _cw, task, _sid) = setup_web_rpc(json!({
                "model": "faux/echo",
                "plugins": { "permissions": { "enabled": false } },
                "webserver": {
                    "enabled": true,
                    "host": "127.0.0.1",
                    "port": 0,
                    "token": token
                }
            }))
            .await;

            let cfg = rpc.anchor_host().unwrap().config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let addr = srv.addr;

            async fn raw_ws_upgrade(addr: std::net::SocketAddr, token: &str) -> TcpStream {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                let upgrade = format!(
                    "GET /rpc HTTP/1.1\r\n\
                     Host: {addr}\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n\
                     Authorization: Bearer {token}\r\n\r\n"
                );
                stream.write_all(upgrade.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
                let mut hdr = Vec::new();
                let mut tmp = [0u8; 1];
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !hdr.ends_with(b"\r\n\r\n") {
                        let n = stream.read(&mut tmp).await.unwrap();
                        assert!(n > 0, "EOF during WS handshake");
                        hdr.push(tmp[0]);
                    }
                })
                .await
                .expect("WS handshake timeout");
                assert!(String::from_utf8_lossy(&hdr).starts_with("HTTP/1.1 101"));
                stream
            }

            // 1. Masked client text frame containing invalid UTF-8 (`0xff 0xfe`) -> Close(1007).
            let mut stream = raw_ws_upgrade(addr, token).await;
            stream.write_all(&[0x81, 0x82, 0x00, 0x00, 0x00, 0x00, 0xff, 0xfe]).await.unwrap();
            stream.flush().await.unwrap();

            let mut raw = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut raw)).await;
            let close_code = if raw.len() >= 4 && (raw[0] & 0x0f) == 0x08 && (raw[1] & 0x7f) >= 2 {
                Some(u16::from_be_bytes([raw[2], raw[3]]))
            } else {
                None
            };
            assert_eq!(
                close_code,
                Some(1007),
                "invalid UTF-8 text frame must close with 1007 (Invalid), raw server bytes: {raw:?}"
            );

            // 2. TCP half-close (FIN) without a WebSocket Close frame triggers
            //    `ResetWithoutClosingHandshake`; server must treat it as clean EOF and NOT send 1002.
            let mut reset_stream = raw_ws_upgrade(addr, token).await;
            reset_stream.shutdown().await.unwrap();
            let mut reset_raw = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(3), reset_stream.read_to_end(&mut reset_raw)).await;
            let reset_close_code =
                if reset_raw.len() >= 4 && (reset_raw[0] & 0x0f) == 0x08 && (reset_raw[1] & 0x7f) >= 2 {
                    Some(u16::from_be_bytes([reset_raw[2], reset_raw[3]]))
                } else {
                    None
                };
            assert_ne!(
                reset_close_code,
                Some(1002),
                "ResetWithoutClosingHandshake must be handled as EOF, not Close(1002): {reset_raw:?}"
            );

            srv.shutdown().await;
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Static-token webserver metadata lives outside `<state>/host/` (leaving `FsHostProtect` unneeded
/// for sandboxed commands), has `0600` private permissions, never stores the token or `token_path`,
/// records `token_source`, and isolates concurrent listener instances per file.
#[tokio::test]
async fn static_token_webserver_keeps_protected_host_dir_absent_and_isolates_concurrent_records() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home_guard = WebHomeEnvGuard::set(home.path());
            let dir = tempfile::tempdir().unwrap();
            let token = "static-secret-token-0123456789abcdef";

            let opts = HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);

            let cfg = json!({
                "webserver": {
                    "enabled": true,
                    "host": "127.0.0.1",
                    "port": 0,
                    "token": token
                }
            });
            let mut srv1 = start_host_webserver(&rpc, &cfg, None).await.unwrap();
            let mut srv2 = start_host_webserver(&rpc, &cfg, None).await.unwrap();

            // 1. `<state>/host/` must remain completely absent so backends without FsHostProtect
            //    (WinAcl, Landlock without restrict_read, custom Seatbelt) do not refuse commands.
            let protected = mare_binding::ossandbox::protected::host_protected();
            for entry in &protected {
                assert_eq!(
                    mare_binding::ossandbox::protected::secret_status(entry),
                    mare_binding::ossandbox::protected::SecretStatus::Absent,
                    "static-token webserver must not create anything in protected host dir {}",
                    entry.path.display()
                );
            }
            let lua = mlua::Lua::new();
            let policy = mare_binding::ossandbox::Policy::from_lua(
                &lua,
                &mlua::Value::String(lua.create_string("workspace-write").unwrap()),
                dir.path(),
            )
            .unwrap()
            .unwrap();
            assert!(
                !policy
                    .missing(mare_binding::ossandbox::Backend::WinAcl)
                    .contains(&mare_binding::ossandbox::Cap::FsHostProtect),
                "static-token webserver must not cause WinAcl to require FsHostProtect"
            );
            assert!(
                !policy
                    .missing(mare_binding::ossandbox::Backend::Landlock { partial: false })
                    .contains(&mare_binding::ossandbox::Cap::FsHostProtect),
                "static-token webserver must not cause Landlock to require FsHostProtect"
            );
            let mut seatbelt_custom = policy.clone();
            seatbelt_custom.profile = Some("(version 1)(allow default)".into());
            assert!(
                !seatbelt_custom
                    .missing(mare_binding::ossandbox::Backend::Seatbelt)
                    .contains(&mare_binding::ossandbox::Cap::FsHostProtect),
                "static-token webserver must not cause custom Seatbelt profile to require FsHostProtect"
            );

            // 2. Both concurrent listeners write separate 0600 files under `<state>/web/`, without token or token_path.
            let web_dir = home.path().join("state").join("web");
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&web_dir)
                .expect("listener metadata must be written under <state>/web/")
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            files.sort();
            assert_eq!(files.len(), 2, "two concurrent listeners must not overwrite each other: {files:?}");

            for f in &files {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "listener metadata {} must be 0600, got {:o}", f.display(), mode);
                }
                #[cfg(windows)]
                {
                    assert_eq!(mare_shared::private::check(f), Ok(()), "listener metadata must have private DACL");
                }
                let raw = std::fs::read_to_string(f).unwrap();
                assert!(!raw.contains(token), "listener metadata must never contain raw token: {raw}");
                let parsed: J = serde_json::from_str(&raw).unwrap();
                assert_eq!(parsed["token_source"], "static", "record must declare token_source: {parsed}");
                assert!(parsed.get("token_path").is_none(), "record must not store token_path: {parsed}");
            }

            // 3. Shutting down srv1 removes only its own record; srv2's record stays until srv2 shuts down.
            srv1.shutdown().await;
            let remaining: Vec<std::path::PathBuf> = std::fs::read_dir(&web_dir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            assert_eq!(remaining.len(), 1, "shutting down srv1 must keep srv2 record alive: {remaining:?}");

            srv2.shutdown().await;
            let after_both: Vec<std::path::PathBuf> = std::fs::read_dir(&web_dir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            assert!(after_both.is_empty(), "shutting down srv2 must remove its record: {after_both:?}");

            let _ = rpc.manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

#[tokio::test]
async fn idle_ws_connection_closes_immediately_on_token_rotation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let _home = WebHomeEnvGuard::set(home.path());
            let tok_path = mare_shared::paths::host_token_file();
            let initial_tok = rotate_host_token_at(&tok_path).unwrap();
            let dir = tempfile::tempdir().unwrap();

            let opts = HostOptions {
                cwd: dir.path().to_path_buf(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": {
                        "enabled": true,
                        "host": "127.0.0.1",
                        "port": 0,
                        "ws_ping_interval_ms": 15_000,
                        "ws_pong_timeout_ms": 10_000,
                        "accept_partial": ["fs_host_protect"]
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);
            let cfg = rt.host.config();
            let mut srv = start_host_webserver(&rpc, &cfg, None).await.unwrap();

            let ws_url = format!("ws://{}/rpc?token={initial_tok}", srv.addr);
            let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
            ws.send(Message::Text(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let _ = ws_next_text(&mut ws).await;

            // Rotate token while connection is completely idle under 15s ping interval.
            let _rotated = rotate_host_token_at(&tok_path).unwrap();

            let outcome = tokio::time::timeout(Duration::from_millis(800), async {
                loop {
                    match ws.next().await {
                        Some(Ok(Message::Close(cf))) => break cf.map(|c| u16::from(c.code)),
                        Some(Err(e)) => panic!("unexpected ws err after idle rotation: {e}"),
                        None => panic!("expected Close(4001), got EOF"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("idle WS connection must close immediately with 4001 on token rotation without waiting 15s ping");
            assert_eq!(outcome, Some(4001));

            srv.shutdown().await;
            let _ = rpc.manager.shutdown_all().await;
            drop(guard);
        })
        .await;
}

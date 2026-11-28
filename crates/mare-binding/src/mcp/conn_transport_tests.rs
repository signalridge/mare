use super::*;

/// Answer every request with `403 Forbidden` and the given `WWW-Authenticate` header.
async fn forbidden_server(www_authenticate: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 403 Forbidden\r\nWWW-Authenticate: {www_authenticate}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (port, task)
}

async fn post_ping(port: u16) -> StreamableHttpError<reqwest::Error> {
    let guard = ReplyGuard::new(crate::http::build_client(crate::http::base_builder().unwrap()).unwrap());
    let msg: rmcp::model::ClientJsonRpcMessage =
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"})).unwrap();
    match guard.post_message(format!("http://127.0.0.1:{port}/mcp").into(), msg, None, None, HashMap::new()).await {
        Ok(_) => panic!("a 403 must fail the request"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn forbidden_with_a_non_scope_challenge_is_not_insufficient_scope() {
    for header in [r#"Bearer error="invalid_token""#, r#"Bearer realm="mcp""#, "Basic realm=\"x\""] {
        let (port, task) = forbidden_server(header).await;
        let err = post_ping(port).await;
        task.abort();
        assert!(!matches!(err, StreamableHttpError::InsufficientScope(_)), "{header}: {err:?}");
        assert!(err.to_string().contains("403"), "{header}: {err}");
    }
}

#[tokio::test]
async fn forbidden_with_insufficient_scope_stays_insufficient_scope() {
    for header in [
        r#"Bearer error="insufficient_scope", scope="files:write""#,
        r#"Bearer realm="mcp", error=insufficient_scope, scope="files:write""#,
    ] {
        let (port, task) = forbidden_server(header).await;
        let err = post_ping(port).await;
        task.abort();
        match err {
            StreamableHttpError::InsufficientScope(e) => {
                assert_eq!(e.required_scope.as_deref(), Some("files:write"), "{header}")
            }
            other => panic!("{header}: expected InsufficientScope, got {other:?}"),
        }
    }
}

/// Answer every request with the given HTTP status line.
async fn status_server(status: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let resp = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (port, task)
}

#[tokio::test]
async fn reply_post_404_with_session_id_maps_to_session_expired() {
    let (port, task) = status_server("404 Not Found").await;
    let guard = ReplyGuard::new(crate::http::build_client(crate::http::base_builder().unwrap()).unwrap());
    let reply_msg: rmcp::model::ClientJsonRpcMessage =
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":7,"result":{"action":"accept"}})).unwrap();
    let uri: Arc<str> = format!("http://127.0.0.1:{port}/mcp").into();

    let with_session = guard
        .post_message(uri.clone(), reply_msg.clone(), Some(Arc::from("sess-1")), None, HashMap::new())
        .await
        .unwrap_err();
    assert!(
        matches!(with_session, StreamableHttpError::SessionExpired),
        "404 with session_id must map to SessionExpired, got: {with_session:?}"
    );

    let without_session = guard.post_message(uri, reply_msg, None, None, HashMap::new()).await.unwrap_err();
    assert!(
        matches!(without_session, StreamableHttpError::UnexpectedServerResponse(_)),
        "404 without session_id must stay UnexpectedServerResponse, got: {without_session:?}"
    );
    task.abort();
}

#[tokio::test]
async fn http_client_keeps_custom_headers_on_same_origin_redirect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let seen_srv = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
            let resp = if path == "/start" {
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                seen_srv.lock().unwrap().push(req);
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let spec = Spec::parse(serde_json::json!({
        "name": "srv-same-origin",
        "url": format!("http://127.0.0.1:{port}/start"),
        "headers": { "X-API-Key": "secret-107" }
    }))
    .unwrap();
    let client = http_client(&spec).await.unwrap();
    let resp = client.post(format!("http://127.0.0.1:{port}/start")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "expected one request on same-origin target");
    assert!(
        captured[0].to_ascii_lowercase().contains("x-api-key: secret-107"),
        "same-origin redirect must keep custom credential header, got:\n{}",
        captured[0]
    );
    task.abort();
}

#[tokio::test]
async fn http_client_blocks_cross_origin_redirect_without_leaking_custom_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let target_port = target_listener.local_addr().unwrap().port();
    let leaked = Arc::new(StdMutex::new(Vec::<String>::new()));
    let leaked_srv = leaked.clone();
    let target_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = target_listener.accept().await else { break };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            leaked_srv.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        }
    });

    let origin_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let origin_port = origin_listener.local_addr().unwrap().port();
    let origin_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = origin_listener.accept().await else { break };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
            let resp = if path == "/chain" {
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: /hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            } else {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let spec = Spec::parse(serde_json::json!({
        "name": "srv-cross-origin",
        "url": format!("http://127.0.0.1:{origin_port}/mcp"),
        "headers": { "X-API-Key": "secret-107" }
    }))
    .unwrap();
    let client = http_client(&spec).await.unwrap();

    for path in ["/mcp", "/chain"] {
        let err = client
            .post(format!("http://127.0.0.1:{origin_port}{path}"))
            .send()
            .await
            .expect_err("cross-origin redirect must be blocked");
        assert!(
            format!("{err:?}").contains("cross-origin redirect blocked"),
            "expected cross-origin redirect error for {path}, got: {err:?}"
        );
    }
    assert!(
        leaked.lock().unwrap().is_empty(),
        "cross-origin server must never receive custom credential headers: {:?}",
        leaked.lock().unwrap()
    );
    origin_task.abort();
    target_task.abort();
}

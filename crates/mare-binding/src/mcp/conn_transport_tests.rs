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

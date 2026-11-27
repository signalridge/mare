//! Discovery hardening: protected resource metadata and authorization server metadata come from
//! the remote server and are treated as untrusted input.

use super::*;

/// One canned answer of [`Fixture`]: status line, extra headers, body.
#[derive(Clone)]
struct Answer {
    status: &'static str,
    headers: Vec<String>,
    body: String,
}

fn ok_json(body: String) -> Answer {
    Answer { status: "200 OK", headers: vec!["Content-Type: application/json".into()], body }
}

fn unauthorized(www_authenticate: String) -> Answer {
    Answer {
        status: "401 Unauthorized",
        headers: vec![format!("WWW-Authenticate: {www_authenticate}")],
        body: String::new(),
    }
}

/// A loopback HTTP server answering by exact request path; any other path is a 404. Records the
/// request line of every request.
struct Fixture {
    port: u16,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(host: &str, routes: impl Fn(u16) -> Vec<(String, Answer)>) -> Self {
        Self::serve(tokio::net::TcpListener::bind((host, 0)).await.unwrap(), routes)
    }

    fn serve(listener: tokio::net::TcpListener, routes: impl Fn(u16) -> Vec<(String, Answer)>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let port = listener.local_addr().unwrap().port();
        let routes: HashMap<String, Answer> = routes(port).into_iter().collect();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_srv = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = head.lines().next().unwrap_or("").to_string();
                seen_srv.lock().unwrap().push(line.clone());
                let path = line.split_whitespace().nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
                let answer = routes.get(&path).cloned().unwrap_or(Answer {
                    status: "404 Not Found",
                    headers: vec![],
                    body: String::new(),
                });
                let headers: String = answer.headers.iter().map(|h| format!("{h}\r\n")).collect();
                let resp = format!(
                    "HTTP/1.1 {}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.status,
                    answer.body.len(),
                    answer.body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        Self { port, seen, task }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn as_metadata(issuer: &str, extra: &str) -> String {
    format!(
        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","response_types_supported":["code"]{extra}}}"#
    )
}

const S256: &str = r#","code_challenge_methods_supported":["S256"]"#;

fn preregistered() -> OAuthOpts {
    OAuthOpts { client_id: Some("test-client".into()), ..Default::default() }
}

async fn begin_err(server_url: String, opts: OAuthOpts) -> String {
    match begin(server_url, "srv".into(), opts).await {
        Ok(login) => panic!("expected discovery to be refused, got authorization URL {}", login.url),
        Err(e) => e,
    }
}

#[tokio::test]
async fn oauth_discovery_follows_a_same_origin_resource_metadata_pointer() {
    let srv = Fixture::start("127.0.0.1", |port| {
        let origin = format!("http://127.0.0.1:{port}");
        vec![
            ("/mcp".into(), unauthorized(format!(r#"Bearer resource_metadata="{origin}/prm""#))),
            (
                "/prm".into(),
                ok_json(format!(r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}/as"]}}"#)),
            ),
            ("/.well-known/oauth-authorization-server/as".into(), ok_json(as_metadata(&format!("{origin}/as"), S256))),
        ]
    })
    .await;
    let login = begin(format!("http://127.0.0.1:{}/mcp", srv.port), "srv".into(), preregistered()).await.unwrap();
    assert!(login.url.starts_with(&format!("http://127.0.0.1:{}/as/authorize", srv.port)), "{}", login.url);
}

#[tokio::test]
async fn oauth_discovery_refuses_a_cross_origin_resource_metadata_pointer() {
    // The pointer names another origin (same loopback host, another port) whose document would
    // validate: it names the server as its resource. It is never fetched.
    let ours_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let ours_port = ours_listener.local_addr().unwrap().port();
    let other = Fixture::start("127.0.0.1", move |port| {
        let theirs = format!("http://127.0.0.1:{port}");
        vec![
            (
                "/prm".into(),
                ok_json(format!(
                    r#"{{"resource":"http://127.0.0.1:{ours_port}/mcp","authorization_servers":["{theirs}"]}}"#
                )),
            ),
            ("/.well-known/oauth-authorization-server".into(), ok_json(as_metadata(&theirs, S256))),
        ]
    })
    .await;
    let other_port = other.port;
    let srv = Fixture::serve(ours_listener, move |port| {
        let theirs = format!("http://127.0.0.1:{other_port}");
        let ours = format!("http://127.0.0.1:{port}");
        vec![
            ("/mcp".into(), unauthorized(format!(r#"Bearer resource_metadata="{theirs}/prm""#))),
            ("/.well-known/oauth-authorization-server".into(), ok_json(as_metadata(&ours, S256))),
        ]
    });
    let err = begin_err(format!("http://127.0.0.1:{}/mcp", srv.port), preregistered()).await;
    assert!(err.contains("resource metadata") && err.contains("origin"), "unexpected error: {err}");
    assert!(other.requests().is_empty(), "the cross-origin pointer must not be fetched: {:?}", other.requests());
}

#[tokio::test]
async fn oauth_discovery_refuses_synthesized_endpoints_when_the_server_publishes_no_metadata() {
    let srv = Fixture::start("127.0.0.1", |_| vec![]).await;
    let err = begin_err(format!("http://127.0.0.1:{}/mcp", srv.port), preregistered()).await;
    assert!(err.contains("no OAuth metadata"), "unexpected error: {err}");
}

#[tokio::test]
async fn oauth_discovery_refuses_metadata_that_does_not_advertise_pkce_s256() {
    for extra in ["", r#","code_challenge_methods_supported":["plain"]"#] {
        let srv = Fixture::start("127.0.0.1", move |port| {
            vec![(
                "/.well-known/oauth-authorization-server".into(),
                ok_json(as_metadata(&format!("http://127.0.0.1:{port}"), extra)),
            )]
        })
        .await;
        let err = begin_err(format!("http://127.0.0.1:{}/mcp", srv.port), preregistered()).await;
        assert!(err.contains("S256"), "metadata {extra:?}: unexpected error: {err}");
    }
}

#[tokio::test]
async fn oauth_configured_metadata_without_pkce_s256_is_refused() {
    let srv = Fixture::start("127.0.0.1", |port| {
        vec![("/custom-meta".into(), ok_json(as_metadata(&format!("http://127.0.0.1:{port}"), "")))]
    })
    .await;
    let opts = OAuthOpts {
        auth_server_metadata_url: Some(format!("http://127.0.0.1:{}/custom-meta", srv.port)),
        ..preregistered()
    };
    let err = begin_err(format!("http://127.0.0.1:{}/mcp", srv.port), opts).await;
    assert!(err.contains("S256"), "unexpected error: {err}");
}

#[tokio::test]
async fn oauth_discovery_refuses_plain_http_endpoints_off_loopback() {
    // Each endpoint in turn is plain http on a non-loopback host; nothing ever connects to it.
    for field in ["authorization_endpoint", "token_endpoint", "registration_endpoint", "revocation_endpoint"] {
        let srv = Fixture::start("127.0.0.1", move |port| {
            let origin = format!("http://127.0.0.1:{port}");
            let mut meta: serde_json::Value = serde_json::from_str(&as_metadata(&origin, S256)).unwrap();
            meta[field] = serde_json::Value::String("http://auth.example.test/endpoint".into());
            vec![("/.well-known/oauth-authorization-server".into(), ok_json(meta.to_string()))]
        })
        .await;
        let err = begin_err(format!("http://127.0.0.1:{}/mcp", srv.port), preregistered()).await;
        assert!(err.contains(field) && err.contains("https"), "{field}: unexpected error: {err}");
    }
}

#[test]
fn endpoint_scheme_rule_allows_https_and_loopback_http_only() {
    for ok in
        ["https://auth.example.test/token", "http://127.0.0.1:9/token", "http://localhost:9/t", "http://[::1]:9/t"]
    {
        assert!(check_endpoint("token_endpoint", ok).is_ok(), "{ok}");
    }
    for bad in ["http://auth.example.test/token", "ftp://auth.example.test/token", "not a url", "http://10.0.0.1/t"] {
        assert!(check_endpoint("token_endpoint", bad).is_err(), "{bad}");
    }
}

/// Login (authorization URL + code exchange) and subsequent token refresh must send the exact same
/// `resource` indicator: the canonical `resource` published by protected resource metadata rather
/// than the configured endpoint URL with transport query parameters.
#[tokio::test]
async fn oauth_login_and_refresh_send_the_same_server_published_resource() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let published_resource = format!("{origin}/mcp");
    let token_resources: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let token_resources_srv = token_resources.clone();
    let origin_srv = origin.clone();
    let published_srv = published_resource.clone();

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let first = req.lines().next().unwrap_or("");
            let path = first.split_whitespace().nth(1).unwrap_or("").split('?').next().unwrap_or("");
            let body = req.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");

            let (status, headers, resp_body) = match path {
                "/mcp" => (
                    "401 Unauthorized",
                    format!("WWW-Authenticate: Bearer resource_metadata=\"{origin_srv}/prm\"\r\n"),
                    String::new(),
                ),
                "/prm" => (
                    "200 OK",
                    "Content-Type: application/json\r\n".into(),
                    format!(r#"{{"resource":"{published_srv}","authorization_servers":["{origin_srv}/as"]}}"#),
                ),
                "/.well-known/oauth-authorization-server/as" => (
                    "200 OK",
                    "Content-Type: application/json\r\n".into(),
                    as_metadata(&format!("{origin_srv}/as"), S256),
                ),
                "/as/token" => {
                    let params: HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    let grant = params.get("grant_type").cloned().unwrap_or_default();
                    let res = params.get("resource").cloned().unwrap_or_default();
                    token_resources_srv.lock().unwrap().push((grant.clone(), res));
                    let tok = if grant == "refresh_token" { "acc-refreshed" } else { "acc-initial" };
                    (
                        "200 OK",
                        "Content-Type: application/json\r\n".into(),
                        format!(
                            r#"{{"access_token":"{tok}","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-1"}}"#
                        ),
                    )
                }
                _ => ("404 Not Found", String::new(), String::new()),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    // Server URL carries transport query parameters; PRM publishes the canonical resource without them.
    let server_url = format!("{origin}/mcp?transport=http&version=2");
    let opts = preregistered();
    let dir = tempfile::tempdir().unwrap();
    let cred_path = dir.path().join("mcp-oauth").join("res-test.json");

    // 1. Login path (authorize URL + code exchange)
    let mut login_mgr = auth_manager(&server_url, "res-test").await.unwrap();
    login_mgr.set_credential_store(FileCredentialStore(cred_path.clone()));
    discover(&mut login_mgr, &server_url, &opts).await.unwrap();
    let req = AuthorizationRequest::new("http://127.0.0.1:1/callback")
        .with_client_name("mare")
        .with_preregistered_client("test-client");
    let session = AuthorizationSession::new(login_mgr, req).await.map_err(|(_, e)| e).unwrap();
    let auth_url = url::Url::parse(session.get_authorization_url()).unwrap();
    let auth_resource = auth_url.query_pairs().find(|(k, _)| k == "resource").map(|(_, v)| v.into_owned());
    let state = auth_url.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()).unwrap();
    assert_eq!(auth_resource.as_deref(), Some(published_resource.as_str()), "authorize URL resource mismatch");
    session.handle_callback_url(&format!("http://127.0.0.1:1/callback?code=code-1&state={state}")).await.unwrap();

    // 2. Reconnect + refresh path (fresh AuthorizationManager, as in connect_http)
    let mut refresh_mgr = auth_manager(&server_url, "res-test").await.unwrap();
    refresh_mgr.set_credential_store(FileCredentialStore(cred_path));
    discover(&mut refresh_mgr, &server_url, &opts).await.unwrap();
    assert!(refresh_mgr.initialize_from_store().await.unwrap());
    refresh_mgr.refresh_token().await.unwrap();

    server.abort();
    let seen = token_resources.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("authorization_code".to_string(), published_resource.clone()),
            ("refresh_token".to_string(), published_resource),
        ],
        "login token exchange and refresh must both send the PRM-published resource"
    );
}

/// Initial login preserves scope-only `WWW-Authenticate` challenges (without `resource_metadata`),
/// enables rmcp initial scope selection so challenge scopes take precedence over wider metadata
/// `scopes_supported`, and keeps explicit configured scopes winning.
#[tokio::test]
async fn oauth_initial_login_preserves_scope_only_challenge_and_initial_scope_selection() {
    let scope_of = |auth_url: &str| {
        url::Url::parse(auth_url).unwrap().query_pairs().find(|(k, _)| k == "scope").map(|(_, v)| v.into_owned())
    };

    // 1. Scope-only WWW-Authenticate challenge (no resource_metadata) narrows scope to `read`
    // instead of requesting all scopes advertised in AS metadata (`read`, `write`).
    let scope_only_srv = Fixture::start("127.0.0.1", |port| {
        let origin = format!("http://127.0.0.1:{port}");
        vec![
            ("/mcp".into(), unauthorized(r#"Bearer scope="read""#.into())),
            (
                "/.well-known/oauth-authorization-server".into(),
                ok_json(as_metadata(&origin, &format!(r#"{S256},"scopes_supported":["read","write"]"#))),
            ),
        ]
    })
    .await;
    let login1 =
        begin(format!("http://127.0.0.1:{}/mcp", scope_only_srv.port), "srv1".into(), preregistered()).await.unwrap();
    assert_eq!(
        scope_of(&login1.url).as_deref(),
        Some("read"),
        "scope-only challenge must constrain initial login scope: {}",
        login1.url
    );

    // 2. Challenge carrying both same-origin resource_metadata and scope prefers challenge scope
    // over wider PRM scopes_supported (`read`, `write`).
    let prm_srv = Fixture::start("127.0.0.1", |port| {
        let origin = format!("http://127.0.0.1:{port}");
        vec![
            ("/mcp".into(), unauthorized(format!(r#"Bearer resource_metadata="{origin}/prm", scope="read""#))),
            (
                "/prm".into(),
                ok_json(format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}/as"],"scopes_supported":["read","write"]}}"#
                )),
            ),
            (
                "/.well-known/oauth-authorization-server/as".into(),
                ok_json(as_metadata(&format!("{origin}/as"), &format!(r#"{S256},"scopes_supported":["read","write"]"#))),
            ),
        ]
    })
    .await;
    let login2 = begin(format!("http://127.0.0.1:{}/mcp", prm_srv.port), "srv2".into(), preregistered()).await.unwrap();
    assert_eq!(
        scope_of(&login2.url).as_deref(),
        Some("read"),
        "initial challenge scope must take precedence over wider PRM scopes: {}",
        login2.url
    );

    // 3. Explicit configured scopes continue to take precedence over initial challenge selection.
    let explicit_opts = OAuthOpts { scopes: vec!["custom:scope".into()], ..preregistered() };
    let login3 = begin(format!("http://127.0.0.1:{}/mcp", prm_srv.port), "srv3".into(), explicit_opts).await.unwrap();
    assert_eq!(
        scope_of(&login3.url).as_deref(),
        Some("custom:scope"),
        "explicit configured scopes must win over challenge/metadata scopes: {}",
        login3.url
    );
}

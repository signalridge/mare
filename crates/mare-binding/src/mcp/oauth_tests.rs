use super::*;

// ---- shared by the production-path OAuth protection tests ------------------------------

fn seed_store(dir: &tempfile::TempDir, name: &str) -> (FileCredentialStore, rmcp::transport::auth::OAuthTokenResponse) {
    let store = FileCredentialStore(dir.path().join("mcp-oauth").join(name));
    let token_resp: rmcp::transport::auth::OAuthTokenResponse =
        serde_json::from_str(r#"{"access_token":"old","token_type":"Bearer","refresh_token":"rt-1"}"#).unwrap();
    (store, token_resp)
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out.to_ascii_lowercase()
}

/// A local OAuth endpoint (also used as an HTTP proxy: it answers by path): `/register` is
/// accepted, anything else gets a token. Counts every connection.
async fn token_fixture() -> (u16, Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let local_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let local_port = local_listener.local_addr().unwrap().port();
    let local_hits = Arc::new(AtomicUsize::new(0));
    let local_hits_srv = local_hits.clone();
    let local_srv = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = local_listener.accept().await else { break };
            local_hits_srv.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let (status, body) = if first_line.contains("/register") {
                ("201 Created", r#"{"client_id":"bypassed-client","redirect_uris":["http://127.0.0.1/cb"]}"#)
            } else {
                ("200 OK", r#"{"access_token":"bypassed","token_type":"Bearer"}"#)
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    (local_port, local_hits, local_srv)
}

#[derive(Clone, Default)]
struct WarnCapture(Arc<std::sync::Mutex<Vec<String>>>);
struct MsgVisitor(String);
impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}
impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut v = MsgVisitor(String::new());
        event.record(&mut v);
        if !v.0.is_empty() {
            self.0.lock().unwrap().push(v.0);
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[tokio::test]
async fn oauth_credentials_round_trip_through_a_private_file() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());
    let store = FileCredentialStore(dir.path().join("mcp-oauth").join("srv.json"));
    assert!(store.load().await.unwrap().is_none());
    store.save(StoredCredentials::new("client-1".into(), None, vec!["read".into()], Some(42))).await.unwrap();
    let back = store.load().await.unwrap().unwrap();
    assert_eq!(
        (back.client_id.as_str(), back.granted_scopes.as_slice()),
        ("client-1", ["read".to_string()].as_slice())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.0), 0o600);
        assert_eq!(mode(store.0.parent().unwrap()), 0o700);
    }
    #[cfg(windows)]
    for p in [&store.0, store.0.parent().unwrap()] {
        assert_eq!(mare_shared::private::check(p), Ok(()));
    }
    store.clear().await.unwrap();
    assert!(store.load().await.unwrap().is_none());
}

#[test]
fn mcp_oauth_file_names_are_safe_and_keyed_by_url() {
    let p = oauth_file("../evil/name", "https://a.example/mcp");
    let name = p.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("___evil_name-") && name.ends_with(".json"), "{name}");
    // a different URL → a different file: the old token is never sent to the new address
    assert_ne!(oauth_file("srv", "https://a.example/mcp"), oauth_file("srv", "https://b.example/mcp"));
    // a trailing slash isn't a different server
    assert_eq!(oauth_file("srv", "https://a.example/mcp/"), oauth_file("srv", "https://a.example/mcp"));
}

#[test]
fn logout_only_removes_url_keyed_oauth_file() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");

    let url_path = oauth_file_in(&state_dir, "srv", "https://a.example/mcp");
    let old_name_only = state_dir.join("mcp-oauth").join("srv.json");
    std::fs::create_dir_all(url_path.parent().unwrap()).unwrap();
    std::fs::write(&url_path, "{}").unwrap();
    std::fs::write(&old_name_only, "{}").unwrap();

    assert!(logout_in(&state_dir, "srv", Some("https://a.example/mcp")).unwrap());
    assert!(!url_path.exists());
    assert!(old_name_only.exists(), "logout must not touch unhashed <key>.json files");
}

#[test]
fn mcp_oauth_callback_surfaces_error_and_description() {
    let base = "http://127.0.0.1:5555";
    assert_eq!(
        parse_callback("GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n", base, "/callback"),
        Callback::Url(format!("{base}/callback?code=abc&state=xyz"))
    );
    match parse_callback(
        "GET /callback?error=access_denied&error_description=User%20said%20no&state=s HTTP/1.1\r\n\r\n",
        base,
        "/callback",
    ) {
        Callback::Error { message: e, state } => {
            assert!(e.contains("access_denied") && e.contains("User said no"), "{e}");
            assert_eq!(state.as_deref(), Some("s"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1\r\n\r\n", base, "/callback"), Callback::Other);
    assert_eq!(parse_callback("garbage", base, "/callback"), Callback::Other);
}

/// The real listener: a browser redirect carrying `error=` ends the login with that error
/// (and the page says so) instead of waiting for the timeout.
#[tokio::test]
async fn mcp_oauth_callback_listener_reports_error_param() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = tokio::spawn(async move {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        // the request line arrives in two writes: the reader must not stop at the first
        s.write_all(b"GET /callback?error=access_denied&error_descr").await.unwrap();
        // sleep-ok: moved unchanged from mcp/oauth.rs; the gap splits the request line across two reads
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.write_all(b"iption=nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    });
    let (mut sock, _) = listener.accept().await.unwrap();
    let head = read_head(&mut sock).await.unwrap();
    let cb = parse_callback(&head, &format!("http://127.0.0.1:{port}"), "/callback");
    let Callback::Error { message: e, .. } = cb else { panic!("{cb:?}") };
    respond(&mut sock, "400 Bad Request", &format!("mare: login failed — {e}")).await;
    drop(sock);
    let page = client.await.unwrap();
    assert!(page.starts_with("HTTP/1.1 400") && page.contains("access_denied") && page.contains("nope"), "{page}");
}

#[tokio::test]
async fn oauth_begin_enforces_offline_guard_and_allows_loopback_discovery() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("MARE_HOME");
    // Safety: test-only env setup restored before returning, or use isolated check.
    // Note: oauth_file uses state_dir(), which doesn't affect network check in begin().
    let _ = (dir, prev_home);

    // 1. Non-loopback server URL must be rejected before any network I/O.
    let err = match begin("https://example.com/mcp".into(), "srv".into(), OAuthOpts::default()).await {
        Ok(_) => panic!("expected offline guard to reject non-loopback OAuth server URL"),
        Err(e) => e,
    };
    assert!(err.contains("offline test guard"), "expected offline test guard error, got: {err}");

    // 2. Loopback server redirecting OAuth discovery to a non-loopback URL must be rejected
    //    (proves rmcp discovery uses mare_binding::http's redirect/offline guard; rmcp wraps the
    //    redirect-policy error as "OAuth metadata discovery failed for...").
    let redir_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let redir_port = redir_listener.local_addr().unwrap().port();
    let redir_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let redir_hits_srv = redir_hits.clone();
    let redir_srv = tokio::spawn(async move {
        for _ in 0..4 {
            let Ok((mut s, _)) = redir_listener.accept().await else { break };
            redir_hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: https://example.com/.well-known/oauth-authorization-server\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let redir_err = match begin(format!("http://127.0.0.1:{redir_port}/mcp"), "srv".into(), OAuthOpts::default()).await
    {
        Ok(_) => panic!("expected offline guard to block OAuth discovery redirect to example.com"),
        Err(e) => e,
    };
    redir_srv.abort();
    assert!(
        redir_hits.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "expected discovery request to hit loopback server before redirect block"
    );
    assert!(
        redir_err.contains("offline test guard") || redir_err.contains("OAuth metadata discovery failed"),
        "expected offline guard or wrapped discovery failure on redirect, got: {redir_err}"
    );

    // 3. Positive loopback case: loopback server with metadata + preregistered client succeeds.
    let ok_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let ok_port = ok_listener.local_addr().unwrap().port();
    let meta_body = format!(
        r#"{{"issuer":"http://127.0.0.1:{ok_port}/mcp","authorization_endpoint":"http://127.0.0.1:{ok_port}/authorize","token_endpoint":"http://127.0.0.1:{ok_port}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"]}}"#
    );
    let ok_srv = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = ok_listener.accept().await else { break };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let resp = if req.contains("/.well-known/oauth-authorization-server") {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{meta_body}",
                    meta_body.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            };
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let opts = OAuthOpts { client_id: Some("test-client".into()), ..Default::default() };
    let login = begin(format!("http://127.0.0.1:{ok_port}/mcp"), "srv".into(), opts).await.unwrap();
    ok_srv.abort();
    assert!(
        login.url.starts_with(&format!("http://127.0.0.1:{ok_port}/authorize")),
        "unexpected auth url: {}",
        login.url
    );
}

/// Marker set by [`run_link_local_child`]; the child test refuses to run without it.
const LINK_LOCAL_CHILD: &str = "MARE_TEST_OAUTH_LINK_LOCAL_CHILD";

/// Runs `link_local_oauth_endpoints_are_refused_before_any_connection` in a child test process
/// whose environment is cleared, so the only proxy setting is the local fixture the child sets
/// as `HTTP_PROXY` (no lower-case twin to remove — Windows names are case-insensitive — and no
/// `REQUEST_METHOD`, which makes hyper-util ignore proxy variables: hyper-util
/// client/proxy/matcher.rs:303). Every reqwest client in the child, rmcp's filtered one and
/// mare's own, therefore routes `http://169.254.169.254/...` to the local fixture should the
/// address filter ever be lost: the test fails locally and never dials the metadata address.
fn run_link_local_child() {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--exact", "mcp::oauth::tests::link_local_oauth_endpoints_are_refused_before_any_connection"])
        .args(["--ignored", "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(LINK_LOCAL_CHILD, "1");
    // what a process needs to start and make temp files on each platform; no proxy variables
    for k in [
        "PATH",
        "TMPDIR",
        "MARE_TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
        "SYSTEMROOT",
        "windir",
        "USERPROFILE",
        "HOME",
        "MARE_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "link-local child failed ({}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The production OAuth path keeps rmcp's address policy, so link-local metadata endpoints
/// (169.254.169.254 and its IPv4-mapped IPv6 form) are refused with DisallowedAddress before any
/// connection, for registration and for refresh. Child of [`run_link_local_child`] only.
#[tokio::test]
#[ignore = "runs as a child of oauth_production_path_preserves_rmcp_network_and_redirect_protections"]
async fn link_local_oauth_endpoints_are_refused_before_any_connection() {
    use std::sync::atomic::Ordering;
    assert_eq!(std::env::var(LINK_LOCAL_CHILD).as_deref(), Ok("1"), "only runs in its cleared-environment child");
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("isolated HOME"));
    let root = home.parent().expect("isolated home root");
    for (name, child) in
        [("MARE_HOME", "mare"), ("USERPROFILE", "home"), ("APPDATA", "appdata"), ("LOCALAPPDATA", "local-appdata")]
    {
        assert_eq!(std::env::var_os(name).map(std::path::PathBuf::from), Some(root.join(child)), "{name}");
    }
    crate::test_home::assert_xdg(root);
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "REQUEST_METHOD"] {
        assert!(std::env::var_os(name).is_none(), "{name} must be cleared in the child");
    }
    let (local_port, local_hits, local_srv) = token_fixture().await;
    // SAFETY: this child process runs this one test, before any client is built.
    unsafe { std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{local_port}")) };
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());

    let (store_ll, tok_ll) = seed_store(&dir, "linklocal.json");
    store_ll.save(StoredCredentials::new("test-client".into(), Some(tok_ll), vec![], None)).await.unwrap();
    let mut ll_mgr = auth_manager_with_mode(
        &format!("http://127.0.0.1:{local_port}/mcp"),
        "srv-ll",
        AuthManagerMode::ProductionPathNoOfflineGuard,
    )
    .await
    .unwrap();
    let ll_meta: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
        "issuer": format!("http://127.0.0.1:{local_port}/mcp"),
        "authorization_endpoint": format!("http://127.0.0.1:{local_port}/authorize"),
        "token_endpoint": "http://169.254.169.254/token",
        "registration_endpoint": "http://169.254.169.254/register",
        "response_types_supported": ["code"]
    }))
    .unwrap();
    ll_mgr.set_metadata(ll_meta.clone());
    ll_mgr.configure_client_id("test-client").unwrap();
    ll_mgr.set_credential_store(store_ll);

    let ll_reg_err =
        tokio::time::timeout(Duration::from_secs(10), ll_mgr.register_client("mare", "http://127.0.0.1/cb", &[]))
            .await
            .expect("rmcp MetadataAddressResolver must reject 169.254.169.254 registration immediately")
            .expect_err("link-local 169.254.169.254 registration must be rejected by rmcp");
    let ll_reg_msg = error_chain(&ll_reg_err);
    // rmcp's OAuthHttpError::DisallowedAddress for exactly this URL (rmcp transport/auth.rs:91),
    // not any connection failure
    assert!(
        ll_reg_msg.contains(
            "oauth request to `http://169.254.169.254/register` rejected: address not allowed for this resource"
        ),
        "expected rmcp DisallowedAddress rejection for 169.254.169.254/register, got: {ll_reg_msg}"
    );

    let warn_capture = WarnCapture::default();
    let warn_dispatch = tracing::Dispatch::new(warn_capture.clone());
    let ll_err = {
        use tracing::instrument::WithSubscriber;
        tokio::time::timeout(Duration::from_secs(10), ll_mgr.refresh_token())
            .with_subscriber(warn_dispatch)
            .await
            .expect("rmcp MetadataAddressResolver must reject 169.254.169.254 immediately")
            .expect_err("link-local 169.254.169.254 must be rejected by rmcp")
    };
    let ll_msg = error_chain(&ll_err);
    assert!(
        ll_msg.contains("oauth token refresh failed"),
        "expected rmcp token refresh rejection for 169.254.169.254, got: {ll_msg}"
    );
    let captured_warns = warn_capture.0.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(
        captured_warns.contains("address not allowed for this resource")
            && captured_warns.contains("169.254.169.254/token"),
        "expected rmcp DisallowedAddress policy warning for 169.254.169.254/token, got: {captured_warns}"
    );
    assert_eq!(local_hits.load(Ordering::SeqCst), 0, "filtered 169.254.169.254 must not even reach the local proxy");

    // Negative control: mare's own client (what a production path that dropped rmcp's client
    // would use) takes the same environment proxy, so register and refresh both reach the
    // local fixture and succeed. The filter is what refused them above, and a regression
    // fails here locally instead of dialing 169.254.169.254.
    let unfiltered_http = crate::http::build_client(crate::http::base_builder().unwrap()).unwrap();
    let (store_ll_unfilt, tok_ll_unfilt) = seed_store(&dir, "linklocal-unfilt.json");
    store_ll_unfilt
        .save(StoredCredentials::new("test-client".into(), Some(tok_ll_unfilt), vec![], None))
        .await
        .unwrap();
    let mut ll_unfilt_mgr = auth_manager_with_mode(
        &format!("http://127.0.0.1:{local_port}/mcp"),
        "srv-ll-unfilt",
        AuthManagerMode::ProductionPathNoOfflineGuard,
    )
    .await
    .unwrap();
    ll_unfilt_mgr.with_client(unfiltered_http).unwrap();
    ll_unfilt_mgr.set_metadata(ll_meta);
    ll_unfilt_mgr.configure_client_id("test-client").unwrap();
    ll_unfilt_mgr.set_credential_store(store_ll_unfilt);
    ll_unfilt_mgr.register_client("mare", "http://127.0.0.1/cb", &[]).await.expect(
        "removing MetadataAddressResolver filter must allow 169.254.169.254 registration through local proxy fixture",
    );
    let (store_ll_unfilt_tok, tok_ll_unfilt_tok) = seed_store(&dir, "linklocal-unfilt-tok.json");
    store_ll_unfilt_tok
        .save(StoredCredentials::new("bypassed-client".into(), Some(tok_ll_unfilt_tok), vec![], None))
        .await
        .unwrap();
    ll_unfilt_mgr.set_credential_store(store_ll_unfilt_tok);
    ll_unfilt_mgr
        .refresh_token()
        .await
        .expect("removing MetadataAddressResolver filter must allow 169.254.169.254 token through local proxy fixture");
    assert_eq!(
        local_hits.load(Ordering::SeqCst),
        2,
        "without the filter both requests reach the local proxy, not 169.254.169.254"
    );

    // IPv4-mapped IPv6 link-local ([::ffff:169.254.169.254]) must also be rejected by ll_mgr's
    // MetadataAddressResolver without hitting local_srv.
    let v6_ll_meta: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
        "issuer": format!("http://127.0.0.1:{local_port}/mcp"),
        "authorization_endpoint": format!("http://127.0.0.1:{local_port}/authorize"),
        "token_endpoint": "http://[::ffff:169.254.169.254]/token",
        "registration_endpoint": "http://[::ffff:169.254.169.254]/register",
        "response_types_supported": ["code"]
    }))
    .unwrap();
    ll_mgr.set_metadata(v6_ll_meta);
    let v6_reg_err = ll_mgr
        .register_client("mare", "http://127.0.0.1/cb", &[])
        .await
        .expect_err("IPv4-mapped IPv6 link-local [::ffff:169.254.169.254] registration must be rejected");
    let v6_reg_msg = error_chain(&v6_reg_err);
    assert!(
        v6_reg_msg.contains("address not allowed for this resource"),
        "expected rmcp DisallowedAddress rejection for [::ffff:169.254.169.254], got: {v6_reg_msg}"
    );
    assert_eq!(local_hits.load(Ordering::SeqCst), 2, "filtered [::ffff:169.254.169.254] must not hit local fixture");

    local_srv.abort();
}

#[tokio::test]
async fn oauth_production_path_preserves_rmcp_network_and_redirect_protections() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Per-call ProductionPathNoOfflineGuard must NOT disable the process-wide offline guard
    // for concurrent tests.
    assert!(crate::http::offline_guard_active(), "process-wide offline guard must remain active");
    assert!(
        crate::http::check_offline_url("https://example.com").is_err(),
        "concurrent offline guard must continue blocking public URLs"
    );

    // 1a. Prove MetadataAddressResolver actually takes effect: for a non-loopback base URL,
    //     rmcp must reject a real local loopback token_endpoint BEFORE dialing it. If
    //     with_client() were called, MetadataAddressResolver would be replaced and this
    //     listener would be hit and return 200 OK.
    let (local_port, local_hits, local_srv) = token_fixture().await;

    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());

    let (store_nonpub, tok_nonpub) = seed_store(&dir, "nonpub.json");
    store_nonpub.save(StoredCredentials::new("test-client".into(), Some(tok_nonpub), vec![], None)).await.unwrap();
    let mut nonpub_mgr =
        auth_manager_with_mode("https://example.com/mcp", "srv-nonpub", AuthManagerMode::ProductionPathNoOfflineGuard)
            .await
            .unwrap();
    let nonpub_meta: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
        "issuer": "https://example.com/mcp",
        "authorization_endpoint": "https://example.com/authorize",
        "token_endpoint": format!("http://127.0.0.1:{local_port}/token"),
        "registration_endpoint": format!("http://127.0.0.1:{local_port}/register"),
        "response_types_supported": ["code"]
    }))
    .unwrap();
    nonpub_mgr.set_metadata(nonpub_meta.clone());
    nonpub_mgr.configure_client_id("test-client").unwrap();
    nonpub_mgr.set_credential_store(store_nonpub);

    let nonpub_reg_err = nonpub_mgr
        .register_client("mare", "http://127.0.0.1/cb", &[])
        .await
        .expect_err("MetadataAddressResolver must reject loopback registration_endpoint for public base_url");
    let nonpub_reg_msg = error_chain(&nonpub_reg_err);
    assert!(
        nonpub_reg_msg.contains("address not allowed for this resource"),
        "expected rmcp DisallowedAddress rejection for loopback registration_endpoint, got: {nonpub_reg_msg}"
    );
    let nonpub_warn_capture = WarnCapture::default();
    let nonpub_warn_dispatch = tracing::Dispatch::new(nonpub_warn_capture.clone());
    let nonpub_err = {
        use tracing::instrument::WithSubscriber;
        nonpub_mgr
            .refresh_token()
            .with_subscriber(nonpub_warn_dispatch)
            .await
            .expect_err("MetadataAddressResolver must reject loopback token_endpoint for public base_url")
    };
    let nonpub_msg = error_chain(&nonpub_err);
    assert!(
        nonpub_msg.contains("oauth token refresh failed"),
        "expected rmcp token refresh rejection, got: {nonpub_msg}"
    );
    let nonpub_warns = nonpub_warn_capture.0.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(
        nonpub_warns.contains("address not allowed for this resource")
            && nonpub_warns.contains(&format!("127.0.0.1:{local_port}/token")),
        "expected rmcp DisallowedAddress policy warning for loopback token_endpoint, got: {nonpub_warns}"
    );
    assert_eq!(local_hits.load(Ordering::SeqCst), 0, "local listener must never be dialed when resolver rejects");

    // Negative control for 1a: removing MetadataAddressResolver via with_client() hits local_srv and succeeds.
    let unfiltered_http = crate::http::build_client(
        crate::http::base_builder()
            .unwrap()
            .proxy(reqwest::Proxy::http(format!("http://127.0.0.1:{local_port}")).unwrap()),
    )
    .unwrap();
    let (store_nonpub_unfilt, tok_nonpub_unfilt) = seed_store(&dir, "nonpub-unfilt.json");
    store_nonpub_unfilt
        .save(StoredCredentials::new("test-client".into(), Some(tok_nonpub_unfilt), vec![], None))
        .await
        .unwrap();
    let mut nonpub_unfilt_mgr = auth_manager_with_mode(
        "https://example.com/mcp",
        "srv-nonpub-unfilt",
        AuthManagerMode::ProductionPathNoOfflineGuard,
    )
    .await
    .unwrap();
    nonpub_unfilt_mgr.with_client(unfiltered_http.clone()).unwrap();
    nonpub_unfilt_mgr.set_metadata(nonpub_meta);
    nonpub_unfilt_mgr.configure_client_id("test-client").unwrap();
    nonpub_unfilt_mgr.set_credential_store(store_nonpub_unfilt);
    nonpub_unfilt_mgr
        .register_client("mare", "http://127.0.0.1/cb", &[])
        .await
        .expect("removing MetadataAddressResolver filter must allow registration_endpoint through to local fixture");
    assert_eq!(local_hits.load(Ordering::SeqCst), 1, "unfiltered client must hit local fixture");

    local_srv.abort();

    // 1b. link-local endpoints run in a child process with a cleared environment
    //     (`link_local_oauth_endpoints_are_refused_before_any_connection`).
    run_link_local_child();

    // 2. Token endpoint 307 and 308 redirects must BOTH be refused individually in production
    //    (proves with_client() did not override OAuthHttpRedirectPolicy::Stop).
    for status_line in ["307 Temporary Redirect", "308 Permanent Redirect"] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token_hits = Arc::new(AtomicUsize::new(0));
        let redirected_hits = Arc::new(AtomicUsize::new(0));
        let token_hits_srv = token_hits.clone();
        let redirected_hits_srv = redirected_hits.clone();
        let status_owned = status_line.to_string();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let resp = if first_line.starts_with("POST /token ") {
                    token_hits_srv.fetch_add(1, Ordering::SeqCst);
                    format!(
                        "HTTP/1.1 {status_owned}\r\nLocation: http://127.0.0.1:{port}/redirected-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else if first_line.contains("/redirected-token") {
                    redirected_hits_srv.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"access_token":"leaked","token_type":"Bearer"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });

        let (store, token_resp) = seed_store(&dir, &format!("redir-{}.json", &status_line[..3]));
        store.save(StoredCredentials::new("test-client".into(), Some(token_resp), vec![], None)).await.unwrap();

        let mut mgr = auth_manager_with_mode(
            &format!("http://127.0.0.1:{port}/mcp"),
            "srv-redir",
            AuthManagerMode::ProductionPathNoOfflineGuard,
        )
        .await
        .unwrap();
        let meta: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "issuer": format!("http://127.0.0.1:{port}/mcp"),
            "authorization_endpoint": format!("http://127.0.0.1:{port}/authorize"),
            "token_endpoint": format!("http://127.0.0.1:{port}/token"),
            "response_types_supported": ["code"]
        }))
        .unwrap();
        mgr.set_metadata(meta);
        mgr.configure_client_id("test-client").unwrap();
        mgr.set_credential_store(store);

        let err = mgr.refresh_token().await.unwrap_err();
        srv.abort();
        assert_eq!(token_hits.load(Ordering::SeqCst), 1, "expected /token to be called once for {status_line}: {err}");
        assert_eq!(
            redirected_hits.load(Ordering::SeqCst),
            0,
            "redirected token endpoint must never be hit for {status_line}: {err}"
        );
    }

    // Offline guard must still be active after production-path checks complete.
    assert!(crate::http::offline_guard_active(), "process-wide offline guard must still be active");
}

#[tokio::test]
async fn two_processes_refreshing_oauth_concurrently_do_not_overwrite_with_stale_token() {
    const CHILD_URL: &str = "MARE_TEST_OAUTH_RACE_URL";
    if let Ok(url) = std::env::var(CHILD_URL) {
        let mut mgr = auth_manager(&url, "srv-race").await.unwrap();
        let meta: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "issuer": url,
            "authorization_endpoint": format!("{url}/authorize"),
            "token_endpoint": format!("{url}/token"),
            "response_types_supported": ["code"]
        }))
        .unwrap();
        mgr.set_metadata(meta);
        assert!(mgr.initialize_from_store().await.unwrap(), "seeded credentials must initialize");
        {
            use std::io::Write;
            println!("READY");
            let _ = std::io::stdout().flush();
        }
        mgr.refresh_token().await.unwrap();
        std::process::exit(0);
    }

    use std::io::BufRead;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}/mcp");
    let cred_path = {
        let h = sha2::Sha256::digest(base_url.trim_end_matches('/').as_bytes());
        let hex: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
        dir.path().join("state").join("mcp-oauth").join(format!("srv-race-{hex}.json"))
    };
    let seeded: StoredCredentials = serde_json::from_value(serde_json::json!({
        "client_id": "test-client",
        "token_response": {
            "access_token": "acc-1",
            "token_type": "Bearer",
            "expires_in": 1,
            "refresh_token": "rt-1"
        },
        "granted_scopes": [],
        "token_received_at": 0,
        "issuer": base_url
    }))
    .unwrap();
    write_private(&cred_path, &serde_json::to_vec_pretty(&seeded).unwrap()).unwrap();

    let req1_seen = Arc::new(tokio::sync::Notify::new());
    let req2_seen = Arc::new(tokio::sync::Notify::new());
    let release_req1 = Arc::new(tokio::sync::Notify::new());
    let seen_refresh_tokens = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let srv_req1 = req1_seen.clone();
    let srv_req2 = req2_seen.clone();
    let srv_rel1 = release_req1.clone();
    let srv_tokens = seen_refresh_tokens.clone();
    let srv = tokio::spawn(async move {
        let mut seq = 0usize;
        let mut handles = Vec::new();
        while seq < 2 {
            let Ok((mut s, _)) = listener.accept().await else { break };
            seq += 1;
            let idx = seq;
            let req1 = srv_req1.clone();
            let req2 = srv_req2.clone();
            let rel1 = srv_rel1.clone();
            let tokens = srv_tokens.clone();
            handles.push(tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..pos]);
                        let content_len = headers
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok()).flatten()
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + content_len {
                            let body = &buf[pos + 4..pos + 4 + content_len];
                            let rt = url::form_urlencoded::parse(body)
                                .find_map(|(k, v)| (k == "refresh_token").then(|| v.into_owned()))
                                .unwrap_or_default();
                            tokens.lock().unwrap().push(rt.clone());
                            let resp_body = if idx == 1 {
                                req1.notify_one();
                                rel1.notified().await;
                                r#"{"access_token":"acc-2","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-2"}"#
                            } else {
                                req2.notify_one();
                                if rt == "rt-2" {
                                    r#"{"access_token":"acc-3","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-3"}"#
                                } else {
                                    // Uncoordinated second caller still held old refresh token `rt-1`.
                                    r#"{"access_token":"acc-stale","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-stale"}"#
                                }
                            };
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                                resp_body.len()
                            );
                            let _ = s.write_all(resp.as_bytes()).await;
                            break;
                        }
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });

    struct SpawnedChild {
        ready_rx: tokio::sync::oneshot::Receiver<()>,
        done: tokio::task::JoinHandle<(std::process::ExitStatus, String, String)>,
    }

    let spawn_child = |url: &str, home: &std::path::Path| -> SpawnedChild {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mcp::oauth::tests::two_processes_refreshing_oauth_concurrently_do_not_overwrite_with_stale_token",
                "--nocapture",
            ])
            .env(CHILD_URL, url)
            .env("MARE_HOME", home)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let done = tokio::task::spawn_blocking(move || {
            let err_handle = std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default());
            let mut out = String::new();
            let mut ready_tx = Some(ready_tx);
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line.trim_end() == "READY"
                    && let Some(tx) = ready_tx.take()
                {
                    let _ = tx.send(());
                }
                out.push_str(&line);
                line.clear();
            }
            let status = child.wait().unwrap();
            let err = err_handle.join().unwrap_or_default();
            (status, out, err)
        });
        SpawnedChild { ready_rx, done }
    };

    let c1 = spawn_child(&base_url, dir.path());
    if tokio::time::timeout(Duration::from_secs(10), req1_seen.notified()).await.is_err() {
        srv.abort();
        let (st, out, err) = c1.done.await.unwrap();
        panic!("child 1 never hit /token (status={st}):\nstdout:\n{out}\nstderr:\n{err}");
    }

    let mut c2 = spawn_child(&base_url, dir.path());
    if tokio::time::timeout(Duration::from_secs(10), &mut c2.ready_rx).await.is_err() {
        release_req1.notify_one();
        srv.abort();
        let (st2, out2, err2) = c2.done.await.unwrap();
        panic!("child 2 never reached refresh_token() ready barrier (status={st2}):\nstdout:\n{out2}\nstderr:\n{err2}");
    }
    let concurrent_overlap = tokio::time::timeout(Duration::from_millis(250), req2_seen.notified()).await.is_ok();

    release_req1.notify_one();
    let (st1, out1, err1) = c1.done.await.unwrap();
    let (st2, out2, err2) = c2.done.await.unwrap();
    if !st1.success() || !st2.success() {
        srv.abort();
        panic!("child process failed:\nc1 ({st1}): {out1}\n{err1}\nc2 ({st2}): {out2}\n{err2}");
    }
    srv.await.unwrap();

    assert!(
        !concurrent_overlap,
        "second process reached /token while first process still held the refresh critical section"
    );

    let tokens = seen_refresh_tokens.lock().unwrap().clone();
    assert_eq!(
        tokens,
        vec!["rt-1".to_string(), "rt-2".to_string()],
        "expected serialized refresh tokens, got: {tokens:?}"
    );

    let final_bytes = std::fs::read(&cred_path).unwrap();
    let final_json: serde_json::Value = serde_json::from_slice(&final_bytes).unwrap();
    assert_eq!(final_json["token_response"]["access_token"], "acc-3", "{final_json}");
    assert_eq!(final_json["token_response"]["refresh_token"], "rt-3", "{final_json}");
}

#[tokio::test]
async fn refresh_guard_times_out_with_credential_store_error_when_lock_file_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = seed_store(&dir, "srv-timeout.json");
    let lock_path = oauth_lock_path(&store.0);
    mare_shared::private::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let held =
        mare_shared::private::open(std::fs::OpenOptions::new().create(true).truncate(false).write(true), &lock_path)
            .unwrap();
    held.lock().unwrap();

    let err = match store.acquire_refresh_guard_with(Duration::from_millis(40)).await {
        Ok(_) => panic!("contended OAuth lock must time out"),
        Err(err) => err,
    };
    assert!(
        matches!(err, AuthError::CredentialStoreError(ref msg) if msg.contains("timed out waiting for OAuth refresh lock")),
        "unexpected lock timeout error: {err:?}"
    );

    // Releasing the held lock allows immediate re-acquisition with private permissions intact.
    drop(held);
    let guard = store.acquire_refresh_guard_with(Duration::from_millis(250)).await.expect("re-acquire after release");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&lock_path), 0o600);
        assert_eq!(mode(lock_path.parent().unwrap()), 0o700);
    }
    #[cfg(windows)]
    {
        assert_eq!(mare_shared::private::check(&lock_path), Ok(()));
        assert_eq!(mare_shared::private::check(lock_path.parent().unwrap()), Ok(()));
    }
    drop(guard);
}

#[tokio::test]
async fn in_process_refresh_lock_serializes_and_times_out_for_missing_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(dir.path());
    let missing_parent = dir.path().join("fresh-oauth-dir");
    let cred_path = missing_parent.join("srv-inproc.json");
    let store = FileCredentialStore(cred_path.clone());

    let guard1 = store.acquire_refresh_guard_with(Duration::from_millis(250)).await.unwrap();
    let err = match store.acquire_refresh_guard_with(Duration::from_millis(40)).await {
        Ok(_) => panic!("contended in-process mutex must time out"),
        Err(err) => err,
    };
    assert!(
        matches!(err, AuthError::CredentialStoreError(ref msg) if msg.contains("timed out waiting for in-process OAuth refresh lock")),
        "unexpected in-process lock timeout error: {err:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&oauth_lock_path(&cred_path)), 0o600);
        assert_eq!(mode(&missing_parent), 0o700);
    }
    drop(guard1);

    let guard2 = store.acquire_refresh_guard_with(Duration::from_millis(250)).await.expect("re-acquire after drop");
    drop(guard2);
}

#[tokio::test]
async fn refresh_guard_recovers_after_lock_holder_process_exits() {
    const CHILD_LOCK: &str = "MARE_TEST_OAUTH_HOLD_EXIT_PATH";
    if let Ok(path) = std::env::var(CHILD_LOCK) {
        let p = std::path::PathBuf::from(path);
        if let Some(dir) = p.parent() {
            mare_shared::private::create_dir_all(dir).unwrap();
        }
        let f = mare_shared::private::open(std::fs::OpenOptions::new().create(true).truncate(false).write(true), &p)
            .unwrap();
        f.lock().unwrap();
        {
            use std::io::Write;
            println!("HELD");
            let _ = std::io::stdout().flush();
        }
        // Hold lock until parent observes contention and closes stdin.
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
        std::process::exit(0);
    }

    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let cred_path = dir.path().join("state").join("mcp-oauth").join("srv-exit.json");
    let lock_path = oauth_lock_path(&cred_path);
    let store = FileCredentialStore(cred_path);

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mcp::oauth::tests::refresh_guard_recovers_after_lock_holder_process_exits", "--nocapture"])
        .env(CHILD_LOCK, &lock_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let child_stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::task::spawn_blocking(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let mut out = String::new();
        let mut tx = Some(tx);
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line.trim_end() == "HELD"
                && let Some(tx) = tx.take()
            {
                let _ = tx.send(());
            }
            out.push_str(&line);
            line.clear();
        }
        let status = child.wait().unwrap();
        assert!(tx.is_none(), "child exited before holding lock (status={status}):\n{out}");
        status
    });
    tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
    let contended = mare_shared::private::testing::watch_contended(&lock_path);
    let acquire_fut = store.acquire_refresh_guard_with(Duration::from_secs(5));
    tokio::pin!(acquire_fut);
    tokio::select! {
        _ = contended.wait() => {}
        res = &mut acquire_fut => panic!("expected lock contention while child holds lock: {}", res.is_ok()),
    }
    drop(child_stdin);
    let guard = acquire_fut.await.expect("must recover once holder exits");
    drop(guard);
    assert!(waiter.await.unwrap().success());
}

#[test]
fn oauth_opts_from_json_accepts_snake_case_only() {
    let snake = serde_json::json!({
        "scopes": ["read", "write"],
        "client_id": "cid",
        "client_secret": "csec",
        "client_name": "cname",
        "callback_port": 8765,
        "auth_server_metadata_url": "https://auth.example.com/.well-known/oauth-authorization-server"
    });
    let parsed = OAuthOpts::from_json(Some(&snake));
    assert_eq!(parsed.scopes, vec!["read", "write"]);
    assert_eq!(parsed.client_id.as_deref(), Some("cid"));
    assert_eq!(parsed.client_secret.as_deref(), Some("csec"));
    assert_eq!(parsed.client_name.as_deref(), Some("cname"));
    assert_eq!(parsed.callback_port, Some(8765));
    assert_eq!(
        parsed.auth_server_metadata_url.as_deref(),
        Some("https://auth.example.com/.well-known/oauth-authorization-server")
    );

    let camel = serde_json::json!({
        "scope": "read write",
        "clientId": "cid",
        "clientSecret": "csec",
        "clientName": "cname",
        "callbackPort": 8765,
        "authServerMetadataUrl": "https://auth.example.com/.well-known/oauth-authorization-server"
    });
    let ignored = OAuthOpts::from_json(Some(&camel));
    assert!(ignored.scopes.is_empty(), "legacy `scope` alias must be ignored");
    assert_eq!(ignored.client_id, None);
    assert_eq!(ignored.client_secret, None);
    assert_eq!(ignored.client_name, None);
    assert_eq!(ignored.callback_port, None);
    assert_eq!(ignored.auth_server_metadata_url, None);
}

/// `OAuthLogin` honors explicit `scopes`, `client_secret`,
/// `callback_port`, `auth_server_metadata_url` override, and accepts callbacks on `::1` when IPv6
/// loopback is available.
#[tokio::test]
async fn oauth_begin_and_finish_honor_scopes_client_secret_callback_port_metadata_override_and_ipv6() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let srv = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let srv_port = srv.local_addr().unwrap().port();
    let seen_reqs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_srv = seen_reqs.clone();

    let meta_body = format!(
        r#"{{"issuer":"http://127.0.0.1:{srv_port}","authorization_endpoint":"http://127.0.0.1:{srv_port}/authorize","token_endpoint":"http://127.0.0.1:{srv_port}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"]}}"#
    );
    let token_body = r#"{"access_token":"tok-opts","token_type":"Bearer","expires_in":3600}"#;

    let server_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = srv.accept().await else { break };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end = None;
            let mut content_len = 0usize;
            while let Ok(n) = sock.read(&mut tmp).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none()
                    && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    header_end = Some(pos + 4);
                    let head = String::from_utf8_lossy(&buf[..pos]);
                    for line in head.lines() {
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                        {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                if let Some(end) = header_end
                    && buf.len() >= end + content_len
                {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            seen_srv.lock().unwrap().push(req.clone());
            let first = req.lines().next().unwrap_or("");
            let (status, body) = if first.starts_with("GET /custom-meta") {
                ("200 OK", meta_body.as_str())
            } else if first.starts_with("POST /token") {
                ("200 OK", token_body)
            } else {
                ("404 Not Found", "")
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    // Pick a free callback port for `callback_port`.
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let cb_port = probe.local_addr().unwrap().port();
    drop(probe);

    let opts = OAuthOpts {
        scopes: vec!["mcp:read".into(), "mcp:write".into()],
        client_id: Some("pre-client".into()),
        client_secret: Some("super-secret-xyz".into()),
        client_name: Some("mare-test".into()),
        callback_port: Some(cb_port),
        auth_server_metadata_url: Some(format!("http://127.0.0.1:{srv_port}/custom-meta")),
    };

    let cred_dir = tempfile::tempdir().unwrap();
    let server_url = format!("http://127.0.0.1:{srv_port}/mcp");
    let mut login = begin(server_url.clone(), "opts-test".into(), opts).await.unwrap();
    let default_cred_path = login.cred_path.clone();
    login.cred_path = cred_dir.path().join("opts-test.json");
    assert_eq!(login.redirect, format!("http://127.0.0.1:{cb_port}/callback"));
    let parsed = reqwest::Url::parse(&login.url).unwrap();
    let params: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    let scope = params.get("scope").cloned().unwrap_or_default();
    assert!(scope.contains("mcp:read") && scope.contains("mcp:write"), "scopes missing in auth URL: {}", login.url);
    assert_eq!(params.get("client_id").map(String::as_str), Some("pre-client"));
    let state = params.get("state").cloned().expect("state parameter");

    // Verify ::1 listener was also bound whenever IPv6 loopback is available on the host, and
    // deliver the callback over ::1 if bound (otherwise 127.0.0.1).
    let ipv6_bound = {
        let guard = login.state.lock().await;
        guard.as_ref().map(|(_, ls)| ls.len()).unwrap_or(0) >= 2
    };
    let cb_path = format!("/callback?code=code-42&state={state}");
    let cb_task = tokio::spawn(async move {
        let mut stream = if ipv6_bound {
            tokio::net::TcpStream::connect(("::1", cb_port)).await.unwrap()
        } else {
            tokio::net::TcpStream::connect(("127.0.0.1", cb_port)).await.unwrap()
        };
        let req = format!("GET {cb_path} HTTP/1.1\r\nHost: localhost:{cb_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp).await;
    });

    login.finish(Duration::from_secs(5)).await.unwrap();
    cb_task.await.unwrap();
    server_task.abort();
    let _ = std::fs::remove_file(&default_cred_path);
    let _ = std::fs::remove_file(oauth_lock_path(&default_cred_path));

    let reqs = seen_reqs.lock().unwrap().clone();
    assert!(reqs.iter().any(|r| r.starts_with("GET /custom-meta")), "metadata URL override must be fetched: {reqs:?}");
    assert!(
        !reqs.iter().any(|r| r.contains("/.well-known/oauth-authorization-server")),
        "well-known metadata must not be queried when auth_server_metadata_url is set: {reqs:?}"
    );
    let token_req = reqs.iter().find(|r| r.starts_with("POST /token")).expect("token exchange request");
    assert!(
        token_req.contains("super-secret-xyz") || token_req.contains("cHJlLWNsaWVudDpzdXBlci1zZWNyZXQteHl6"),
        "client_secret must be transmitted to token endpoint: {token_req}"
    );
}

#[tokio::test]
async fn oauth_callback_listener_ignores_mismatched_state_error_and_completes_valid_callback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let srv_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let srv_port = srv_listener.local_addr().unwrap().port();
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = srv_listener.accept().await else { break };
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let first = req.lines().next().unwrap_or("");
            let (status, body) = if first.starts_with("GET /meta") {
                (
                    "200 OK",
                    format!(
                        r#"{{"issuer":"http://127.0.0.1:{srv_port}","authorization_endpoint":"http://127.0.0.1:{srv_port}/authorize","token_endpoint":"http://127.0.0.1:{srv_port}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"]}}"#
                    ),
                )
            } else if first.starts_with("POST /token") {
                ("200 OK", r#"{"access_token":"tok-good","token_type":"Bearer","expires_in":3600}"#.into())
            } else {
                ("404 Not Found", String::new())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let cb_port = probe.local_addr().unwrap().port();
    drop(probe);

    let opts = OAuthOpts {
        scopes: vec!["mcp:read".into()],
        client_id: Some("state-check-client".into()),
        client_secret: None,
        client_name: Some("mare-test".into()),
        callback_port: Some(cb_port),
        auth_server_metadata_url: Some(format!("http://127.0.0.1:{srv_port}/meta")),
    };
    let cred_dir = tempfile::tempdir().unwrap();
    let mut login = begin(format!("http://127.0.0.1:{srv_port}/mcp"), "state-check".into(), opts).await.unwrap();
    let default_cred_path = login.cred_path.clone();
    login.cred_path = cred_dir.path().join("state-check.json");
    let expected_state = url_state_param(&login.url).expect("authorization url must include state");

    let cb_task = tokio::spawn(async move {
        // 1. Spoofed error callback with wrong state must be rejected (400) without ending the login.
        let mut s1 = tokio::net::TcpStream::connect(("127.0.0.1", cb_port)).await.unwrap();
        s1.write_all(b"GET /callback?error=access_denied&state=wrong-state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut resp1 = String::new();
        let _ = s1.read_to_string(&mut resp1).await;

        // 2. Spoofed error callback with missing state must also be rejected without ending the login.
        let mut s2 = tokio::net::TcpStream::connect(("127.0.0.1", cb_port)).await.unwrap();
        s2.write_all(b"GET /callback?error=access_denied HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut resp2 = String::new();
        let _ = s2.read_to_string(&mut resp2).await;

        // 3. Legitimate callback with matching state succeeds.
        let mut s3 = tokio::net::TcpStream::connect(("127.0.0.1", cb_port)).await.unwrap();
        let req3 = format!(
            "GET /callback?code=ok-code&state={expected_state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        s3.write_all(req3.as_bytes()).await.unwrap();
        let mut resp3 = String::new();
        let _ = s3.read_to_string(&mut resp3).await;
        (resp1, resp2, resp3)
    });

    login.finish(Duration::from_secs(5)).await.expect("valid callback after spoofed errors must succeed");
    let (r1, r2, r3) = cb_task.await.unwrap();
    server_task.abort();
    let _ = std::fs::remove_file(&default_cred_path);
    let _ = std::fs::remove_file(oauth_lock_path(&default_cred_path));
    assert!(r1.starts_with("HTTP/1.1 400") && r1.contains("invalid OAuth state"), "{r1}");
    assert!(r2.starts_with("HTTP/1.1 400") && r2.contains("invalid OAuth state"), "{r2}");
    assert!(r3.starts_with("HTTP/1.1 200"), "{r3}");
}

#[tokio::test]
async fn oauth_discovery_normalizes_challenge_and_ignores_lookalike_resource_metadata() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Decoy "other-origin" server that must NEVER be contacted during discovery.
    let (evil_port, evil_hits, evil_srv) = token_fixture().await;

    // 1. Scope-only challenge with a cross-origin lookalike `x_resource_metadata` parameter must
    //    be refused before rmcp's fallback probe can contact `evil_port`.
    let srv1 = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port1 = srv1.local_addr().unwrap().port();
    let task1 = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = srv1.accept().await else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first = req.lines().next().unwrap_or("");
            let resp = if first.starts_with("GET /mcp ") {
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer x_resource_metadata=\"http://127.0.0.1:{evil_port}/evil\", scope=\"mcp:read\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            };
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let opts1 = OAuthOpts { client_id: Some("norm-client-1".into()), ..Default::default() };
    let err1 = begin(format!("http://127.0.0.1:{port1}/mcp"), "norm1".into(), opts1)
        .await
        .err()
        .expect("cross-origin lookalike x_resource_metadata must be refused");
    task1.abort();
    assert!(err1.contains("another origin"), "expected cross-origin refusal, got: {err1}");
    assert_eq!(evil_hits.load(Ordering::SeqCst), 0, "lookalike x_resource_metadata must not be fetched");

    // 2. Same-origin `resource_metadata` preceded by a same-origin lookalike `x_resource_metadata`:
    //    normalization strips the lookalike so rmcp fetches `/prm` and NEVER `/decoy`.
    let prm_hits = Arc::new(AtomicUsize::new(0));
    let decoy_hits = Arc::new(AtomicUsize::new(0));
    let prm_hits_srv = prm_hits.clone();
    let decoy_hits_srv = decoy_hits.clone();
    let srv2 = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port2 = srv2.local_addr().unwrap().port();
    let task2 = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = srv2.accept().await else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first = req.lines().next().unwrap_or("");
            let resp = if first.starts_with("GET /mcp ") {
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer x_resource_metadata=\"http://127.0.0.1:{port2}/decoy\", resource_metadata=\"http://127.0.0.1:{port2}/prm\", scope=\"mcp:read\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else if first.starts_with("GET /decoy ") {
                decoy_hits_srv.fetch_add(1, Ordering::SeqCst);
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else if first.starts_with("GET /prm ") {
                prm_hits_srv.fetch_add(1, Ordering::SeqCst);
                let body = format!(
                    r#"{{"resource":"http://127.0.0.1:{port2}/mcp","authorization_servers":["http://127.0.0.1:{port2}"]}}"#
                );
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else if first.starts_with("GET /.well-known/oauth-authorization-server") {
                let body = format!(
                    r#"{{"issuer":"http://127.0.0.1:{port2}","authorization_endpoint":"http://127.0.0.1:{port2}/authorize","token_endpoint":"http://127.0.0.1:{port2}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"]}}"#
                );
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            };
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let opts2 = OAuthOpts { client_id: Some("norm-client-2".into()), ..Default::default() };
    let login2 = begin(format!("http://127.0.0.1:{port2}/mcp"), "norm2".into(), opts2).await.unwrap();
    task2.abort();
    evil_srv.abort();
    assert_eq!(evil_hits.load(Ordering::SeqCst), 0, "other-origin decoy must never be fetched");
    assert_eq!(decoy_hits.load(Ordering::SeqCst), 0, "lookalike x_resource_metadata must be stripped before rmcp");
    assert_eq!(prm_hits.load(Ordering::SeqCst), 1, "validated same-origin PRM URL must be fetched");
    assert!(login2.url.starts_with(&format!("http://127.0.0.1:{port2}/authorize")), "{}", login2.url);
    assert!(login2.url.contains("scope=mcp%3Aread"), "scope must be preserved: {}", login2.url);

    // 3. Quote injection in challenge parameters is refused.
    assert!(
        format_normalized_challenge(None, Some("mcp:read\", resource_metadata=\"https://other.example/prm")).is_err(),
        "parameter containing double quote must be rejected"
    );
}

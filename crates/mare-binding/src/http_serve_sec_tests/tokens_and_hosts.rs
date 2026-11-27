//! Runtime tokens, rotation, corruption handling, and trusted host rules.

use super::*;

#[tokio::test]
async fn runtime_token_persisted_0600_cookie_exchange_and_immediate_rotation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let token_path = dir.path().join("host").join("token");
            let lua = setup_lua();

            // Empty string or boolean true token must be rejected.
            let empty_res: mlua::Value = lua
                .load(r#"local s, e = mare.http.serve { token = "", handler = function() return nil end }; return e"#)
                .eval_async()
                .await
                .unwrap();
            assert!(empty_res.as_string().is_some());
            let true_res: mlua::Value = lua
                .load(r#"local s, e = mare.http.serve { token = true, handler = function() return nil end }; return e"#)
                .eval_async()
                .await
                .unwrap();
            assert!(true_res.as_string().is_some());

            // Start server with omitted `token`: creates 0600 32-byte base64url runtime token file.
            // On backends without fs_host_protect (e.g. Landlock-only, Windows default tier),
            // explicitly accept the gap here; strict refusal is tested separately in
            // `http_serve_enforces_fs_host_protect_strictly_unless_accepted`.
            let opts_expr = if crate::ossandbox::protected::host_protect_available() {
                r#"{ handler = function(req) return { status = 200, body = req.path .. ":" .. (req.query.k or "") } end }"#
            } else {
                r#"{ accept_partial = { "fs_host_protect" }, handler = function(req) return { status = 200, body = req.path .. ":" .. (req.query.k or "") } end }"#
            };
            let srv = start_with_token_path(&lua, opts_expr, token_path.clone()).await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            let tok1 = std::fs::read_to_string(&token_path).unwrap();
            assert_eq!(tok1.len(), 43, "32 bytes base64url unpadded should be 43 chars");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
                let dir_mode = std::fs::metadata(token_path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
                assert_eq!(dir_mode, 0o700);
            }

            let no_redirect = crate::http::test_client_no_redirect();

            // Unauthenticated request gets 401.
            let resp = no_redirect.get(format!("http://{addr}/app")).send().await.unwrap();
            assert_eq!(resp.status(), 401);

            // Browser ?token= exchange returns 303, strips token from Location, sets HttpOnly SameSite=Strict cookie.
            let resp =
                no_redirect.get(format!("http://{addr}/app?k=v&token={tok1}")).send().await.unwrap();
            assert_eq!(resp.status(), 303);
            assert_eq!(resp.headers().get("location").unwrap().to_str().unwrap(), "/app?k=v");
            assert_eq!(resp.headers().get("cache-control").unwrap().to_str().unwrap(), "no-store");
            assert_eq!(resp.headers().get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");
            let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
            let port = addr.rsplit(':').next().unwrap();
            let expected_prefix = format!("mare_session_{port}_");
            assert!(set_cookie.starts_with(&expected_prefix), "{set_cookie}");
            assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
            assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
            let cookie_pair = set_cookie.split(';').next().unwrap().to_string();

            // Cookie authenticates subsequent requests.
            let resp = no_redirect
                .get(format!("http://{addr}/app?k=v"))
                .header("cookie", &cookie_pair)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "/app:v");

            // Valid cookie + ?token= in GET URL still redirects (303) to strip ?token= while preserving other params.
            let resp = no_redirect
                .get(format!("http://{addr}/app?k=v&token={tok1}"))
                .header("cookie", &cookie_pair)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 303);
            assert_eq!(resp.headers().get("location").unwrap().to_str().unwrap(), "/app?k=v");
            assert_eq!(resp.headers().get("cache-control").unwrap().to_str().unwrap(), "no-store");
            assert_eq!(resp.headers().get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");
            assert!(resp.headers().get("set-cookie").is_none());

            // Valid cookie + ?token= alone strips query string completely.
            let resp = no_redirect
                .get(format!("http://{addr}/app?token={tok1}"))
                .header("cookie", &cookie_pair)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 303);
            assert_eq!(resp.headers().get("location").unwrap().to_str().unwrap(), "/app");
            assert_eq!(resp.headers().get("cache-control").unwrap().to_str().unwrap(), "no-store");
            assert_eq!(resp.headers().get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");

            // Restarting server reuses the same persisted token.
            srv.call_async_method::<()>("close", ()).await.unwrap();
            let reuse_opts = if crate::ossandbox::protected::host_protect_available() {
                r#"{ handler = function(req) return { status = 200, body = "reused" } end }"#
            } else {
                r#"{ accept_partial = { "fs_host_protect" }, handler = function(req) return { status = 200, body = "reused" } end }"#
            };
            let srv2 = start_with_token_path(
                &lua,
                reuse_opts,
                token_path.clone(),
            )
            .await;
            let addr2: String = srv2.call_method("addr", ()).unwrap();
            let resp = no_redirect
                .get(format!("http://{addr2}/"))
                .header("authorization", format!("Bearer {tok1}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);

            // Obtain a session cookie on srv2, then rotate the token on disk.
            let resp = no_redirect.get(format!("http://{addr2}/?token={tok1}")).send().await.unwrap();
            assert_eq!(resp.status(), 303);
            let cookie2 = resp.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            let tok2 = rotate_host_token_at(&token_path).unwrap();
            assert_ne!(tok1, tok2);

            // Old token AND old session cookie are immediately invalid on srv2!
            let resp = no_redirect
                .get(format!("http://{addr2}/"))
                .header("authorization", format!("Bearer {tok1}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            let resp = no_redirect
                .get(format!("http://{addr2}/"))
                .header("cookie", &cookie2)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401);

            // New token works immediately.
            let resp = no_redirect
                .get(format!("http://{addr2}/"))
                .header("authorization", format!("Bearer {tok2}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "reused");

            srv2.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn runtime_token_rejects_empty_truncated_and_inflight_corruption() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let token_path = dir.path().join("host").join("token");
            std::fs::create_dir_all(token_path.parent().unwrap()).unwrap();

            // 1. Empty token file must be rejected.
            std::fs::write(&token_path, "").unwrap();
            assert!(ensure_host_token_at(&token_path).is_err(), "empty token file must fail");

            // 2. Truncated / non-canonical token file must be rejected.
            std::fs::write(&token_path, "too-short").unwrap();
            assert!(ensure_host_token_at(&token_path).is_err(), "truncated token file must fail");

            // Remove corrupt file and start real server with valid runtime token.
            std::fs::remove_file(&token_path).unwrap();
            let lua = setup_lua();
            let opts_expr = if crate::ossandbox::protected::host_protect_available() {
                r#"{ handler = function() return { status = 200, body = "ok" } end }"#
            } else {
                r#"{ accept_partial = { "fs_host_protect" }, handler = function() return { status = 200, body = "ok" } end }"#
            };
            let srv = start_with_token_path(&lua, opts_expr, token_path.clone()).await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            let tok1 = std::fs::read_to_string(&token_path).unwrap();
            let no_redirect = crate::http::test_client_no_redirect();

            let r_cookie = no_redirect.get(format!("http://{addr}/?token={tok1}")).send().await.unwrap();
            assert_eq!(r_cookie.status(), 303);
            let cookie =
                r_cookie.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            // 3. In-flight corruption (truncated / empty): old token, corrupt token, and old cookie all fail closed.
            std::fs::write(&token_path, "corrupt").unwrap();
            assert_eq!(
                no_redirect
                    .get(format!("http://{addr}/"))
                    .header("authorization", format!("Bearer {tok1}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
            assert_eq!(
                no_redirect
                    .get(format!("http://{addr}/"))
                    .header("authorization", "Bearer corrupt")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
            assert_eq!(
                no_redirect
                    .get(format!("http://{addr}/"))
                    .header("cookie", &cookie)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );

            // 4. Restoring a valid rotated token recovers auth while keeping pre-corruption cookie invalid.
            let tok2 = rotate_host_token_at(&token_path).unwrap();
            assert_eq!(
                no_redirect
                    .get(format!("http://{addr}/"))
                    .header("cookie", &cookie)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
            assert_eq!(
                no_redirect
                    .get(format!("http://{addr}/"))
                    .header("authorization", format!("Bearer {tok2}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[test]
fn concurrent_first_time_token_creation_agrees_on_winner() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("host/token");
    let threads = 8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
    let results: Vec<String> = std::thread::scope(|sc| {
        (0..threads)
            .map(|_| {
                let b = barrier.clone();
                let p = token_path.clone();
                sc.spawn(move || {
                    b.wait();
                    ensure_host_token_at(&p).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    assert!(results.iter().all(|t| t == &results[0]), "all creators must agree on one token: {results:?}");
    let disk = std::fs::read_to_string(&token_path).unwrap();
    assert_eq!(disk.trim(), results[0]);
    let leftover_temps: Vec<_> = std::fs::read_dir(token_path.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftover_temps.is_empty(), "leftover temp files: {leftover_temps:?}");
}

#[tokio::test]
async fn trusted_hosts_allows_normalized_proxy_host_and_rejects_untrusted() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = Lua::new();
            let srv = start_server(
                &lua,
                r#"{
                    token = false,
                    trusted_hosts = { "MARE.EXAMPLE", "api.internal:8443" },
                    handler = function(req) return { status = 200, body = req.path } end
                }"#,
            )
            .await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            let client = crate::http::test_client();

            // Trusted host without port matches both bare and ported Host headers (case-insensitive).
            let resp = client.get(format!("http://{addr}/ok")).header("host", "mare.example").send().await.unwrap();
            assert_eq!(resp.status(), 200);
            let resp =
                client.get(format!("http://{addr}/ok")).header("host", "mare.example:9000").send().await.unwrap();
            assert_eq!(resp.status(), 200);

            // Trusted host with explicit port matches only that port.
            let resp =
                client.get(format!("http://{addr}/ok")).header("host", "api.internal:8443").send().await.unwrap();
            assert_eq!(resp.status(), 200);
            let resp =
                client.get(format!("http://{addr}/ok")).header("host", "api.internal:9999").send().await.unwrap();
            assert_eq!(resp.status(), 403);

            // Untrusted host is still rejected.
            let resp = client.get(format!("http://{addr}/no")).header("host", "evil.example").send().await.unwrap();
            assert_eq!(resp.status(), 403);

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[test]
fn http_serve_times_out_when_token_lock_held_by_another_process() {
    const CHILD: &str = "MARE_TEST_HOLD_TOKEN_LOCK";
    if let Ok(lock_path_str) = std::env::var(CHILD) {
        use std::io::{Read, Write};
        let lock_path = std::path::PathBuf::from(lock_path_str);
        mare_shared::private::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let file = mare_shared::private::open(
            std::fs::OpenOptions::new().create(true).truncate(false).write(true),
            &lock_path,
        )
        .unwrap();
        file.lock().unwrap();
        let mut out = std::io::stdout().lock();
        writeln!(out, "LOCKED").unwrap();
        out.flush().unwrap();
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        drop(file);
        std::process::exit(0);
    }

    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("host").join("token");
    let lock_path = token_path.with_file_name(".token.lock");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "http_serve::sec_tests::tokens_and_hosts::http_serve_times_out_when_token_lock_held_by_another_process",
            "--nocapture",
        ])
        .env(CHILD, &lock_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let child_stdin = child.stdin.take().unwrap();
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let mut locked = false;
    loop {
        line.clear();
        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap() == 0 {
            break;
        }
        if line.trim() == "LOCKED" {
            locked = true;
            break;
        }
    }
    if !locked {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(locked, "child process did not signal .token.lock acquisition");

    let (tx, rx) = std::sync::mpsc::channel();
    let tp = token_path.clone();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        let res = local.block_on(&rt, async move {
            let lua = setup_lua();
            let opts: mlua::Table = lua
                .load(r#"{ accept_partial = { "fs_host_protect" }, handler = function() return { status = 200 } end }"#)
                .eval()
                .unwrap();
            let mv = super::serve_with_token_path(lua.clone(), opts, tp).await.unwrap();
            extract_serve_result(mv).map(|_| ())
        });
        let _ = tx.send(res);
    });

    let result = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(res) => res,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mare.http.serve hung waiting on .token.lock instead of timing out");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("worker thread panicked unexpectedly");
        }
    };
    drop(child_stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "lock holder child failed: {status}");
    worker.join().unwrap();

    let err = result.expect_err("mare.http.serve must fail when .token.lock stays held");
    assert!(err.contains("timed out") || err.contains("timeout"), "expected lock timeout error, got: {err}");
    assert!(!token_path.exists(), "timed out serve must not create token file");
}

#[tokio::test]
async fn http_serve_cancel_aborts_token_lock_wait_and_rejects_invalid_cancel() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let token_path = dir.path().join("host").join("token");
            let lock_path = token_path.with_file_name(".token.lock");
            mare_shared::private::create_dir_all(lock_path.parent().unwrap()).unwrap();
            let held = mare_shared::private::open(
                std::fs::OpenOptions::new().create(true).truncate(false).write(true),
                &lock_path,
            )
            .unwrap();
            held.lock().unwrap();

            let lua = setup_lua();

            // Invalid `cancel` types must be rejected without touching the token file.
            for bad_expr in [
                r#"{ accept_partial = { "fs_host_protect" }, cancel = 123, handler = function() return { status = 200 } end }"#,
                r#"{ accept_partial = { "fs_host_protect" }, cancel = "nope", handler = function() return { status = 200 } end }"#,
                r#"{ accept_partial = { "fs_host_protect" }, cancel = {}, handler = function() return { status = 200 } end }"#,
            ] {
                let opts: mlua::Table = lua.load(bad_expr).eval().unwrap();
                let err = super::serve_with_token_path(lua.clone(), opts, token_path.clone()).await.unwrap_err();
                let msg = err.to_string();
                assert!(
                    msg.contains("userdata") || msg.contains("mare.Token"),
                    "expected invalid cancel error for {bad_expr}: {msg}"
                );
            }
            let (lua_ok, lua_err): (bool, String) = lua
                .load(r#"local ok, err = pcall(mare.http.serve, { cancel = 123, handler = function() return { status = 200 } end }); return ok, tostring(err)"#)
                .eval_async()
                .await
                .unwrap();
            assert!(!lua_ok && (lua_err.contains("userdata") || lua_err.contains("mare.Token")), "{lua_err}");
            assert!(!token_path.exists(), "invalid cancel must not create token file");

            let contended = mare_shared::private::testing::watch_contended(&lock_path);
            let lua_wait = lua.clone();
            let tp_wait = token_path.clone();
            let serve_handle = tokio::task::spawn_local(async move {
                lua_wait.load("TOK = mare.task.token()").exec().unwrap();
                let opts: mlua::Table = lua_wait
                    .load(
                        r#"{
                            accept_partial = { "fs_host_protect" },
                            cancel = TOK,
                            handler = function() return { status = 200 } end
                        }"#,
                    )
                    .eval()
                    .unwrap();
                let mv = super::serve_with_token_path(lua_wait, opts, tp_wait).await.unwrap();
                extract_serve_result(mv).map(|_| ())
            });

            tokio::time::timeout(Duration::from_secs(5), contended.wait())
                .await
                .expect("mare.http.serve never reached contended .token.lock wait");
            lua.load("TOK:cancel()").exec().unwrap();

            let err = serve_handle.await.unwrap().expect_err("cancelled serve must return error");
            assert!(err.contains("cancelled waiting for token lock"), "unexpected cancel error: {err}");
            assert!(!token_path.exists(), "cancelled serve must not create token file");

            // Holder lock must still be intact after cancelling the waiter.
            let probe = mare_shared::private::open(
                std::fs::OpenOptions::new().create(true).truncate(false).write(true),
                &lock_path,
            )
            .unwrap();
            assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
            drop(probe);

            // Once holder drops, mare.http.serve acquires the lock and starts immediately.
            drop(held);
            let srv = start_with_token_path(
                &lua,
                r#"{ accept_partial = { "fs_host_protect" }, handler = function() return { status = 200 } end }"#,
                token_path.clone(),
            )
            .await;
            srv.call_async_method::<()>("close", ()).await.unwrap();
            assert!(token_path.exists(), "post-release serve should persist runtime token");
        })
        .await;
}

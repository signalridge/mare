//! Only fake OAuth endpoints and TestEnv auth.json; exercise remote dispatch before cancellation.
use super::*;

#[tokio::test]
async fn dispatched_oauth_refresh_persists_rotation_after_prepare_waiter_cancels() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = Arc::new(tokio::sync::Notify::new());
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(
                    200,
                    r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
                )
                .hold(release.clone()),
            ],
        );
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        exec(
            &h,
            r#"
            LOGIN('expired','initial-refresh',0,'fixture-account')
            CANCEL=mare.task.token()
            PREPARE=mare.task.spawn(function()
                OK,WHY=LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'),{cancel=CANCEL})
            end)
        "#,
        )
        .await;
        wait_until(5, "fake OAuth refresh dispatch", || async { server.count("/auth/oauth/token") == 1 }).await;
        exec(&h, "CANCEL:cancel(); PREPARE:wait(); assert(not OK and WHY:find('cancelled',1,true))").await;
        release.notify_one();
        wait_until(5, "rotation saved after waiter cancellation", || {
            let h = h.clone();
            async move {
                eval::<bool>(&h, "local a=LLM:auth_get('openai-codex'); return a and a.refresh=='rotated-refresh'")
                    .await
            }
        })
        .await;
        assert_eq!(server.count("/auth/oauth/token"), 1);
        exec(&h, "local g=mare.lock.file(mare.paths.auth..'.lock',{timeout_ms=100}); g:release()").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_274_graceful_shutdown_drains_dispatched_oauth_refresh() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = Arc::new(tokio::sync::Notify::new());
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(
                    200,
                    r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
                )
                .hold(release.clone()),
            ],
        );
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        let auth_path = std::path::PathBuf::from(eval::<String>(&h, "return mare.paths.auth").await);
        exec(
            &h,
            r#"
            LOGIN('expired','initial-refresh',0,'fixture-account')
            CANCEL = mare.task.token()
            PREPARE = mare.task.spawn(function()
                OK, WHY = LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'), { cancel = CANCEL })
            end)
        "#,
        )
        .await;
        wait_until(5, "OAuth refresh dispatched to token endpoint", || async {
            server.count("/auth/oauth/token") == 1
        })
        .await;

        // Release the delayed token response 600ms after graceful shutdown begins (well within shutdown_grace_ms = 4000ms).
        let rel = release.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            rel.notify_one();
        });

        let t0 = std::time::Instant::now();
        h.shutdown().await;
        let elapsed = t0.elapsed();

        let raw = std::fs::read_to_string(&auth_path).unwrap();
        let saved: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            saved["openai-codex"]["refresh"], "rotated-refresh",
            "graceful shutdown returned after {:?} without draining dispatched OAuth refresh rotation: {saved}",
            elapsed
        );
    })
    .await
}

#[tokio::test]
async fn new_oauth_refresh_is_rejected_once_shutdown_starts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(
                200,
                r#"{"access_token":"late-access","refresh_token":"late-refresh","expires_in":3600}"#,
            )],
        );
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        exec(&h, "LOGIN('expired','initial-refresh',0,'fixture-account')").await;
        h.emit_json("app/shutdown", json!({})).await;
        let (ok, why): (bool, String) = eval(
            &h,
            r#"
            local ok, why = LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'))
            return ok == true, tostring(why)
            "#,
        )
        .await;
        h.shutdown().await;
        assert!(!ok, "OAuth refresh must be rejected once shutdown starts");
        assert!(why.contains("shutting down"), "expected shutdown rejection reason, got: {why}");
        assert_eq!(server.count("/auth/oauth/token"), 0, "must not dispatch token exchange after shutdown starts");
    })
    .await
}

#[tokio::test]
async fn old_oauth_response_cannot_overwrite_logout_or_a_new_credential_generation() {
    local(async {
        for replacement in ["logout", "same-account", "other-account"] {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let release = Arc::new(tokio::sync::Notify::new());
            server.route(
                "/auth/oauth/token",
                vec![
                    Resp::status(
                        200,
                        r#"{"access_token":"stale-response","refresh_token":"stale-rotation","expires_in":3600}"#,
                    )
                    .hold(release.clone()),
                ],
            );
            let h = codex_host(&mut env, &server).await;
            exec(&h, WRITE_LOGIN).await;
            exec(
                &h,
                r#"
                LOGIN('expired','initial-refresh',0,'fixture-account')
                CANCEL=mare.task.token()
                PREPARE=mare.task.spawn(function()
                    OK,WHY=LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'),{cancel=CANCEL})
                end)
            "#,
            )
            .await;
            wait_until(5, "refresh held after dispatch", || async { server.count("/auth/oauth/token") == 1 }).await;
            exec(&h, "CANCEL:cancel(); PREPARE:wait(); assert(not OK)").await;
            // Simulate another credential writer changing the file while this HTTP response is
            // held. The CAS must re-read persisted data, including a same-account new login.
            global(&h, "REPLACEMENT", replacement);
            exec(
                &h,
                r#"
                if REPLACEMENT=='logout' then
                    mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1 }),{mode=384})
                else
                    LOGIN('replacement-access','replacement-refresh',mare.task.now()+3600000,
                        REPLACEMENT=='same-account' and 'fixture-account' or 'different-account')
                end
            "#,
            )
            .await;
            release.notify_one();
            exec(
                &h,
                r#"
                local g=mare.lock.file(mare.paths.auth..'.lock',{timeout_ms=5000}); g:release()
                local a=LLM:auth_get('openai-codex')
                if REPLACEMENT=='logout' then assert(a==nil)
                else assert(a.access=='replacement-access' and a.refresh=='replacement-refresh') end
            "#,
            )
            .await;
            assert_eq!(server.count("/auth/oauth/token"), 1);
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn oauth_owner_preserves_permanent_and_temporary_refresh_classification() {
    local(async {
        for permanent in [true,false] {
            let mut env=TestEnv::new().await;
            let server=MockServer::start().await;
            server.route("/auth/oauth/token",if permanent {
                vec![Resp::status(400,r#"{"error":"refresh_token_reused"}"#)]
            } else {
                vec![Resp::status(503,r#"{"error":"temporary"}"#).header("retry-after","0"),
                    Resp::status(200,r#"{"access_token":"recovered-access","refresh_token":"recovered-refresh","expires_in":3600}"#)]
            });
            let h=codex_host(&mut env,&server).await;
            exec(&h,WRITE_LOGIN).await;
            exec(&h,"LOGIN('expired','initial-refresh',0,'fixture-account')").await;
            let (ready,reason):(bool,Option<String>)=eval(&h,
                "return LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'))").await;
            if permanent {
                assert!(!ready && reason.as_deref().is_some_and(|r|r.contains("refresh token was already used")),"{reason:?}");
                assert_eq!(server.count("/auth/oauth/token"),1);
                assert_eq!(eval::<String>(&h,"return LLM:auth_get('openai-codex').refresh").await,"initial-refresh");
            } else {
                assert!(ready,"{reason:?}");
                assert_eq!(server.count("/auth/oauth/token"),2);
                assert_eq!(eval::<String>(&h,"return LLM:auth_get('openai-codex').refresh").await,"recovered-refresh");
            }
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn ordinary_auth_writers_serialize_after_an_inflight_refresh_owner() {
    local(async {
        for logout in [true,false] {
            let mut env=TestEnv::new().await;
            let server=MockServer::start().await;
            let release=Arc::new(tokio::sync::Notify::new());
            server.route("/auth/oauth/token",vec![Resp::status(200,
                r#"{"access_token":"rotation","refresh_token":"rotation-refresh","expires_in":3600}"#)
                .hold(release.clone())]);
            server.route("/auth/oauth/revoke",vec![Resp::status(200,"{}")]);
            let h=codex_host(&mut env,&server).await;
            exec(&h,WRITE_LOGIN).await;
            exec(&h,r#"
                LOGIN('expired','initial-refresh',0,'fixture-account')
                CANCEL=mare.task.token()
                PREPARE=mare.task.spawn(function()
                    OK=LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'),{cancel=CANCEL})
                end)
            "#).await;
            wait_until(5,"inflight owner holding auth lock",||async {server.count("/auth/oauth/token")==1}).await;
            exec(&h,"CANCEL:cancel(); PREPARE:wait(); assert(not OK)").await;
            global(&h,"LOGOUT",logout);
            exec(&h,r#"
                WRITER=mare.task.spawn(function()
                    if LOGOUT then LLM:logout('openai')
                    else LLM:auth_set('openai-codex',{
                        type='oauth',access='new-login',refresh='new-login-refresh',
                        expires=mare.task.now()+3600000,account_id='new-account'}) end
                end)
            "#).await;
            release.notify_one();
            exec(&h,r#"
                WRITER:wait()
                local a=LLM:auth_get('openai-codex')
                if LOGOUT then assert(a==nil)
                else assert(a.access=='new-login' and a.refresh=='new-login-refresh' and a.account_id=='new-account') end
            "#).await;
            assert_eq!(server.count("/auth/oauth/token"),1);
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn cancelled_oauth_lock_waiter_never_dispatches_a_refresh() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(
                200,
                r#"{"access_token":"must-not-send","refresh_token":"must-not-send","expires_in":3600}"#,
            )],
        );
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        let lock_path = eval::<String>(&h, "return mare.paths.auth..'.lock'").await;
        let contended = mare_shared::private::testing::watch_contended(std::path::Path::new(&lock_path));
        exec(
            &h,
            r#"
            LOGIN('expired','initial-refresh',0,'fixture-account')
            LOCK_FILE=mare.lock.file
            GUARD=LOCK_FILE(mare.paths.auth..'.lock',{timeout_ms=1000})
            local finished_tx,finished_rx=mare.task.channel()
            FINISHED_RX=finished_rx
            mare.lock.file=function(...)
                local ok,result=pcall(LOCK_FILE,...)
                finished_tx:send({ok=ok,result=tostring(result)})
                if not ok then error(result,0) end
                return result
            end
            CANCEL=mare.task.token()
            PREPARE=mare.task.spawn(function()
                READY,REASON=LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'),{cancel=CANCEL})
            end)
        "#,
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), contended.wait())
            .await
            .expect("second caller never reached contended filelock wait before timeout");
        exec(
            &h,
            r#"
            assert(FINISHED_RX:try_recv()==nil,'lock_file returned before cancel')
            CANCEL:cancel(); PREPARE:wait()
            local lock_res=FINISHED_RX:recv() -- the independent owner actually left its lock wait
            assert(not lock_res.ok and (lock_res.result:find('aborted',1,true) or lock_res.result:find('cancelled',1,true)),lock_res.result)
            assert(not READY and REASON:find('cancelled',1,true))
            mare.lock.file=LOCK_FILE
            GUARD:release()
        "#,
        )
        .await;
        assert_eq!(server.count("/auth/oauth/token"), 0);
        assert_eq!(eval::<String>(&h, "return LLM:auth_get('openai-codex').refresh").await, "initial-refresh");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn lock_waiter_released_during_agent_tree_drain_never_dispatches_oauth_refresh() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(
                200,
                r#"{"access_token":"late-drain-access","refresh_token":"late-drain-refresh","expires_in":3600}"#,
            )],
        );
        // A child-only plugin that stalls agent_tree drain in Host::shutdown_report() until the
        // parent writes `release-child`, holding shutdown in the window after host cancel and
        // before parent `app/shutdown`.
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let child_started = env.cwd.join("child-started");
        let child_in_shutdown = env.cwd.join("child-in-shutdown");
        let release_child = env.cwd.join("release-child");
        std::fs::write(
            pdir.join("hold-child-drain.lua"),
            r#"
            return {
                name = "hold-child-drain",
                apply = function(ctx)
                    if mare.mode ~= "child" then
                        return
                    end
                    ctx:on("turn/started", function()
                        assert(mare.fs.write(mare.cwd .. "/child-started", "yes"))
                    end)
                    ctx:on("app/shutdown", function()
                        assert(mare.fs.write(mare.cwd .. "/child-in-shutdown", "yes"))
                        for _ = 1, 200 do
                            if mare.fs.stat(mare.cwd .. "/release-child") then
                                break
                            end
                            mare.task.sleep(10)
                        end
                    end)
                end,
            }
            "#,
        )
        .unwrap();

        let base = server.base.clone();
        let h = env
            .host()
            .user_plugins()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "done", "delay_ms": 60000 }]))
            .config(json!({
                "shutdown_grace_ms": 5000,
                "plugins": {
                    "permissions": { "enabled": false },
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth")
                    },
                    "llm": { "retry_base_ms": 1 }
                }
            }))
            .build()
            .await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        exec(&h, WRITE_LOGIN).await;

        let lock_path = eval::<String>(&h, "return mare.paths.auth..'.lock'").await;
        let contended = mare_shared::private::testing::watch_contended(std::path::Path::new(&lock_path));
        exec(
            &h,
            r#"
            LOGIN('expired', 'initial-refresh', 0, 'fixture-account')
            APP_SHUTDOWN_SEEN = false
            C.cordis:on("app/shutdown", function()
                APP_SHUTDOWN_SEEN = true
            end)
            GUARD = mare.lock.file(mare.paths.auth .. ".lock", { timeout_ms = 5000 })
            PREPARE = mare.task.spawn(function()
                READY, WHY = LLM:prepare(LLM:resolve("openai/gpt-6.1-terra"))
            end)
            CHILD = mare.subagent.start({ prompt = "hold during shutdown", no_session = true })
            "#,
        )
        .await;

        tokio::time::timeout(std::time::Duration::from_secs(5), contended.wait())
            .await
            .expect("refresh owner never reached contended auth lock wait");
        wait_until(5, "child subagent started", || {
            let child_started = child_started.clone();
            async move { child_started.exists() }
        })
        .await;

        // Begin graceful shutdown; it cancels the host token immediately and blocks in
        // agent_tree::Scope::drain() before parent `app/shutdown` is emitted.
        let h_shut = h.clone();
        let shut = tokio::task::spawn_local(async move { h_shut.shutdown().await });

        wait_until(5, "shutdown entered agent_tree drain", || {
            let child_in_shutdown = child_in_shutdown.clone();
            async move { child_in_shutdown.exists() }
        })
        .await;
        let (host_shutting_down, app_shutdown_seen): (bool, bool) =
            eval(&h, "return mare.is_shutting_down(), APP_SHUTDOWN_SEEN").await;
        assert!(host_shutting_down, "mare.is_shutting_down() must be true during agent_tree drain");
        assert!(!app_shutdown_seen, "parent app/shutdown must not have fired while agent_tree is draining");

        // Release the credential lock during the agent-tree drain window; the waiting refresh
        // owner must see host shutdown before dispatching and avoid any OAuth token exchange.
        exec(&h, "GUARD:release(); PREPARE:wait()").await;

        let (ready, why, app_shutdown_still_false): (bool, String, bool) =
            eval(&h, "return READY == true, tostring(WHY), not APP_SHUTDOWN_SEEN").await;
        assert!(!ready, "lock waiter released during tree drain must be rejected");
        assert!(why.contains("shutting down"), "expected shutdown rejection reason, got: {why}");
        assert!(app_shutdown_still_false, "rejection must occur before parent app/shutdown fires");
        assert_eq!(server.count("/auth/oauth/token"), 0, "must not dispatch token exchange during tree drain");

        std::fs::write(&release_child, "go").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), shut).await.expect("shutdown timed out").unwrap();
        assert_eq!(server.count("/auth/oauth/token"), 0);
    })
    .await
}

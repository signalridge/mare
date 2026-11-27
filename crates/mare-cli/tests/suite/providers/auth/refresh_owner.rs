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

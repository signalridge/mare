//! A side-model provider prepares through the same public owner before primary model I/O.
use crate::common::*;
use serde_json::json;

async fn fixture(env: &TestEnv) -> std::sync::Arc<mare_host::Host> {
    let h = env
        .host()
        .no_session()
        .config(json!({"plugins":{"approvals":{
            "reviewer":"prepare-fixture", "review_all":true, "review_timeout_ms":2000
        }}}))
        .build()
        .await;
    expose(&h);
    global(&h, "L", h.service_table("llm").unwrap());
    exec(&h, r#"
        MODE='normal'; PREPARES=0; MAIN_SENT=0; REVIEW_SENT=0; BUILDS=0; CLOSED=0
        ENTER_TX,ENTER_RX=mare.task.channel(); HOLD_TX,HOLD_RX=mare.task.channel()
        L:register_provider({provider='prepare-primary', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
            models={{model_id='main',context_window=100000,max_output=1024}},
            prepare=function() PRIMARY_PREPARED=true end,
            stream=function()
                assert(PRIMARY_PREPARED and PREPARES > 0)
                MAIN_SENT=MAIN_SENT+1
                return {role='assistant',stop_reason='stop',content={{type='text',text='primary response'}}}
            end})
        L:register_provider({provider='prepare-review', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
            models={{model_id='review',context_window=100000,max_output=1024}},
            prepare=function(model)
                assert(model.ref == 'prepare-review/review')
                PREPARES=PREPARES+1
                if MODE=='permanent' then error({kind='auth',message='review credential revoked'},0) end
                if MODE=='transient' and PREPARES==1 or MODE=='exhausted' then
                    error({kind='server',message='review refresh temporarily unavailable',retryable=true,retry_after_ms=1},0)
                end
                if MODE=='hang' then
                    local cleanup <close> = setmetatable({}, {__close=function() CLOSED=CLOSED+1 end})
                    ENTER_TX:send(true)
                    HOLD_RX:recv() -- deliberately ignores the provider cancel argument
                    RESUMED=true
                end
            end,
            stream=function() REVIEW_SENT=REVIEW_SENT+1; error('admission must not send reviewer requests') end})
        L:set_model('prepare-primary/main',{save=false})
        C.cordis:on('request/build',function(req, next) BUILDS=BUILDS+1; return next() end)
        C.cordis:get('approvals'):register_reviewer('prepare-fixture',function() return {decision='deny'} end,{
            admit=function(cancel)
                return L:prepare(L:resolve('prepare-review/review'),{cancel=cancel})
            end,
        })
        assert(L:ready(L:resolve('prepare-review/review')))
    "#).await;
    h
}

#[tokio::test]
async fn selected_prepare_rejects_permanent_auth_before_primary_and_rechecks_next_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let h = fixture(&env).await;
        exec(&h, "MODE='permanent'").await;
        let failed = h.prompt("must not send", json!({})).await.unwrap();
        assert_eq!(failed["not_sent"], true, "{failed}");
        assert!(failed["error"]["message"].as_str().unwrap().contains("review credential revoked"), "{failed}");
        let counters: (i64, i64, i64, i64) = eval(&h, "return PREPARES, MAIN_SENT, REVIEW_SENT, BUILDS").await;
        assert_eq!(counters, (1, 0, 0, 0));
        exec(&h, "MODE='normal'").await;
        let normal = h.prompt("retry after fixing credentials", json!({})).await.unwrap();
        assert_eq!(normal["text"], "primary response", "{normal}");
        assert_eq!(eval::<i64>(&h, "return PREPARES").await, 2);
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "prepare-primary/main");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn selected_prepare_uses_existing_transient_backoff_and_cached_token_budget() {
    local(async {
        let env = TestEnv::new().await;
        let h = fixture(&env).await;
        for (mode, expected) in [("transient", 2), ("exhausted", 3)] {
            global(&h, "NEXT_MODE", mode);
            exec(&h, "MODE=NEXT_MODE; PREPARES=0").await;
            let result = h.prompt("prepare side provider first", json!({})).await.unwrap();
            assert_eq!(result["text"], "primary response", "{result}");
            assert_eq!(eval::<i64>(&h, "return PREPARES").await, expected);
        }
        assert_eq!(eval::<i64>(&h, "return MAIN_SENT").await, 2);
        assert_eq!(eval::<i64>(&h, "return REVIEW_SENT").await, 0);
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "prepare-primary/main");
        h.shutdown().await;
    })
    .await
}

/// The aborted prepare's scope closes once. The close runs as the cancellation unwinds, which may
/// finish after the aborted prompt has returned: wait for it (bounded), then check it ran once.
async fn closed_once(h: &std::sync::Arc<mare_host::Host>) {
    wait_until(WAIT_SECS, "the aborted prepare's scope to close", || {
        let h = h.clone();
        async move { eval::<i64>(&h, "return CLOSED").await > 0 }
    })
    .await;
    assert_eq!(eval::<i64>(h, "return CLOSED").await, 1);
}

#[tokio::test]
async fn selected_prepare_cancel_and_deadline_close_uncooperative_provider_scope() {
    local(async {
        let env = TestEnv::new().await;
        let h = fixture(&env).await;
        exec(&h, "MODE='hang'").await;
        let host = h.clone();
        let prompt = tokio::task::spawn_local(async move { host.prompt("cancel admission", json!({})).await });
        exec(&h, "ENTER_RX:recv()").await;
        h.abort().await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), prompt).await.unwrap().unwrap().unwrap();
        assert!(result["error"]["message"].as_str().is_some_and(|e| e.contains("abort")), "{result}");
        assert_eq!(eval::<i64>(&h, "return MAIN_SENT+REVIEW_SENT+BUILDS").await, 0);
        closed_once(&h).await;
        let timed_out = h.prompt("admission deadline", json!({})).await.unwrap();
        assert_eq!(timed_out["not_sent"], true, "{timed_out}");
        assert!(timed_out["error"]["message"].as_str().is_some_and(|e| e.contains("admission timeout")), "{timed_out}");
        assert_eq!(eval::<i64>(&h, "return MAIN_SENT+REVIEW_SENT+BUILDS").await, 0);
        exec(&h, "ENTER_RX:recv(); assert(CLOSED==2 and not RESUMED)").await;
        exec(
            &h,
            r#"
            local ok,reason=L:prepare(L:resolve('prepare-review/review'),{timeout_ms=20})
            assert(not ok and reason:find('timed out',1,true),tostring(reason))
            ENTER_RX:recv()
            assert(CLOSED==3 and not RESUMED)
            HOLD_TX:send(true)
            assert(not RESUMED)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

async fn builtin_reviewer_fixture(env: &TestEnv, timeout_ms: u64) -> std::sync::Arc<mare_host::Host> {
    let h = env
        .host()
        .no_session()
        .config(json!({"plugins":{
            "approvals":{"reviewer":"model", "review_all":true, "review_timeout_ms":timeout_ms},
            "approval-review-model":{"model":"prepare-review/review"}
        }}))
        .build()
        .await;
    expose(&h);
    global(&h, "L", h.service_table("llm").unwrap());
    exec(
        &h,
        r#"
        MODE='normal'; PREPARES=0; MAIN_SENT=0; REVIEW_SENT=0; BUILDS=0; CLOSED=0
        ENTER_TX,ENTER_RX=mare.task.channel(); HOLD_TX,HOLD_RX=mare.task.channel()
        L:register_provider({provider='prepare-primary', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
            models={{model_id='main',context_window=100000,max_output=1024}},
            prepare=function() PRIMARY_PREPARED=true end,
            stream=function()
                assert(PRIMARY_PREPARED and PREPARES > 0)
                MAIN_SENT=MAIN_SENT+1
                return {role='assistant',stop_reason='stop',content={{type='text',text='primary response'}}}
            end})
        L:register_provider({provider='prepare-review', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
            models={{model_id='review',context_window=100000,max_output=1024}},
            prepare=function(model)
                assert(model.ref == 'prepare-review/review')
                PREPARES=PREPARES+1
                if MODE=='permanent' then error({kind='auth',message='builtin review credential revoked'},0) end
                if MODE=='hang' then
                    local cleanup <close> = setmetatable({}, {__close=function() CLOSED=CLOSED+1 end})
                    ENTER_TX:send(true)
                    HOLD_RX:recv()
                    RESUMED=true
                end
            end,
            stream=function() REVIEW_SENT=REVIEW_SENT+1; error('admission must not send reviewer requests') end})
        L:set_model('prepare-primary/main',{save=false})
        C.cordis:on('request/build',function(req, next) BUILDS=BUILDS+1; return next() end)
    "#,
    )
    .await;
    h
}

#[tokio::test]
async fn builtin_model_reviewer_prepares_selected_provider_and_blocks_permanent_failure() {
    local(async {
        let env = TestEnv::new().await;
        let h = builtin_reviewer_fixture(&env, 2000).await;
        exec(&h, "MODE='permanent'").await;
        let failed = h.prompt("must not send", json!({})).await.unwrap();
        assert_eq!(failed["not_sent"], true, "{failed}");
        assert!(failed["error"]["message"].as_str().unwrap().contains("builtin review credential revoked"), "{failed}");
        let counters: (i64, i64, i64, i64) = eval(&h, "return PREPARES, MAIN_SENT, REVIEW_SENT, BUILDS").await;
        assert_eq!(counters, (1, 0, 0, 0));

        exec(&h, "MODE='normal'").await;
        let ok = h.prompt("proceed when prepared", json!({})).await.unwrap();
        assert_eq!(ok["text"], "primary response", "{ok}");
        let counters: (i64, i64, i64) = eval(&h, "return PREPARES, MAIN_SENT, REVIEW_SENT").await;
        assert_eq!(counters, (2, 1, 0));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn builtin_model_reviewer_cancel_and_deadline_close_provider_prepare() {
    local(async {
        let env = TestEnv::new().await;
        let h = builtin_reviewer_fixture(&env, 150).await;
        exec(&h, "MODE='hang'").await;
        let host = h.clone();
        let prompt = tokio::task::spawn_local(async move { host.prompt("cancel builtin admission", json!({})).await });
        exec(&h, "ENTER_RX:recv()").await;
        h.abort().await.unwrap();
        let aborted = tokio::time::timeout(std::time::Duration::from_secs(5), prompt).await.unwrap().unwrap().unwrap();
        assert!(aborted["error"]["message"].as_str().is_some_and(|e| e.contains("abort")), "{aborted}");
        assert_eq!(eval::<i64>(&h, "return MAIN_SENT+REVIEW_SENT+BUILDS").await, 0);
        closed_once(&h).await;

        let timed_out = h.prompt("builtin admission deadline", json!({})).await.unwrap();
        assert_eq!(timed_out["not_sent"], true, "{timed_out}");
        assert!(timed_out["error"]["message"].as_str().is_some_and(|e| e.contains("admission timeout")), "{timed_out}");
        assert_eq!(eval::<i64>(&h, "return MAIN_SENT+REVIEW_SENT+BUILDS").await, 0);
        exec(&h, "ENTER_RX:recv(); assert(CLOSED==2 and not RESUMED)").await;
        exec(&h, "HOLD_TX:send(true); HOLD_TX:send(true); assert(not RESUMED)").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn side_model_prepare_success_does_not_clear_primary_model_prepare_failure() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            PRIMARY_FAIL = true; SIDE_PREPARED = 0; MAIN_SENT = 0; BUILDS = 0
            L:register_provider({provider='primary-auth', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
                models={{model_id='main',context_window=100000,max_output=1024}},
                prepare=function()
                    if PRIMARY_FAIL then error({kind='auth', message='primary credential revoked'}, 0) end
                end,
                stream=function()
                    MAIN_SENT = MAIN_SENT + 1
                    return {role='assistant',stop_reason='stop',content={{type='text',text='primary response'}}}
                end})
            L:register_provider({provider='side-auth', login={methods={'api_key'},run=function() end}, is_configured=function() return true end,
                models={{model_id='side',context_window=100000,max_output=1024}},
                prepare=function() SIDE_PREPARED = SIDE_PREPARED + 1 end,
                stream=function() error('side model must not stream') end})
            L:set_model('primary-auth/main', {save=false})
            C.cordis:on('request/build', function(req, next) BUILDS = BUILDS + 1; return next() end)
            C.cordis:get('llm'):readiness('side-prepare', { order = 5, prepare = function()
                local ok, reason = L:prepare(L:resolve('side-auth/side'))
                assert(ok, tostring(reason))
            end })
        "#,
        )
        .await;

        let refused = h.prompt("primary failed", json!({})).await.unwrap();
        assert_eq!(refused["not_sent"], true, "{refused}");
        assert!(refused["error"]["message"].as_str().unwrap().contains("primary credential revoked"), "{refused}");
        let counters: (i64, i64, i64) = eval(&h, "return SIDE_PREPARED, MAIN_SENT, BUILDS").await;
        assert_eq!(counters, (1, 0, 0));

        exec(&h, "PRIMARY_FAIL = false").await;
        let ok = h.prompt("primary fixed", json!({})).await.unwrap();
        assert_eq!(ok["text"], "primary response", "{ok}");
        let counters: (i64, i64) = eval(&h, "return SIDE_PREPARED, MAIN_SENT").await;
        assert_eq!(counters, (2, 1));
        h.shutdown().await;
    })
    .await
}

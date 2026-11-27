//! Tool execution lifecycle tests: truncation budget, cancellation, timeouts, and idempotency.

use super::{call, host};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn structured_results_share_one_truncation_budget() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (total, truncated): (usize, bool) = eval(
            &h,
            r#"
            T:register("big", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    return { content = { { type = "text", text = string.rep("line\n", 10000) },
                                         { type = "text", text = string.rep("more\n", 10000) } } }
                end,
            })
            local r = T:execute({ id = "1", name = "big", arguments = {} }, {})
            local n = 0
            for _, b in ipairs(r.content) do n = n + #b.text end
            return n, r.truncated == true
        "#,
        )
        .await;
        assert!(truncated && total <= 52 * 1024, "structured results bypassed truncation ({total} bytes)");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn disposing_a_tools_owner_cancels_its_calls_in_flight() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        exec(
            &h,
            r#"
            -- the owner is a child fiber of the test ctx: the tool is an effect of it
            OWNER = C.cordis:plugin({ name = "stubborn-owner", apply = function(ctx) OWNER_CTX = ctx end })
            while not OWNER_CTX do mare.task.sleep(5) end
            -- ignores its cancel token: cut off after the grace period
            OWNER_CTX:get("tools"):register("stubborn", { description = "x", parameters = { type = "object", properties = {} },
                side_effect = "read", idempotent = true,
                execute = function() STARTED = true; while true do mare.task.sleep(50) end end })
            STARTED, RESULT = false, nil
            mare.task.spawn(function() RESULT = T:execute({ id = "s1", name = "stubborn", arguments = {} }, {}) end)
        "#,
        )
        .await;
        wait_until(WAIT_SECS, "the call to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STARTED").await }
        })
        .await;
        exec(&h, "OWNER:dispose()").await;
        wait_until(WAIT_SECS, "in-flight call to be cancelled", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return RESULT ~= nil and RESULT.cancelled == true").await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_timeout_cancels_the_call_and_reports_tool_timeout() {
    local(async {
        let env = TestEnv::new().await;
        let h =
            env.host().no_session().config(json!({ "plugins": { "tools": { "cancel_grace_ms": 100 } } })).build().await;
        expose(&h);
        // a cooperative tool stops when its token is cancelled; a stubborn one is cut off after
        // the grace period — both come back as tool_timeout
        exec(
            &h,
            r#"local params = { type = "object", properties = {} }
               T:register("polite", { description = "x", parameters = params, timeout_ms = 100,
                 side_effect = "read", idempotent = true,
                 execute = function(_, t) t.cancel:wait(); POLITE_SAW_CANCEL = true; return "stopped" end })
               T:register("stubborn", { description = "x", parameters = params, timeout_ms = 100,
                 side_effect = "read", idempotent = true,
                 execute = function() while true do mare.task.sleep(20) end end })"#,
        )
        .await;
        let polite = call(&h, "polite", json!({})).await;
        let stubborn = call(&h, "stubborn", json!({})).await;
        for r in [&polite, &stubborn] {
            assert_eq!(r["error_code"], "tool_timeout", "{r}");
            assert_eq!(r["is_error"], true, "{r}");
        }
        assert!(eval::<bool>(&h, "return POLITE_SAW_CANCEL == true").await, "the token was not cancelled on timeout");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tools_get_the_call_id_as_an_idempotency_key() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (id, key): (String, String) = eval(
            &h,
            r#"
            T:register("keyed", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function(_, tctx) return tctx.tool_call_id .. "|" .. tctx.idempotency_key end,
            })
            local r = T:execute({ id = "call-42", name = "keyed", arguments = {} }, {})
            local text = type(r.content) == "string" and r.content or r.content[1].text
            return text:match("^(.-)|"), text:match("|(.*)$")
        "#,
        )
        .await;
        // --no-session has no crash recovery: tools dedupe external effects on this key
        assert_eq!((id.as_str(), key.as_str()), ("call-42", "call-42"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn managed_tool_timeout_preserves_forced_cancellation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({"plugins":{"tools":{
                "timeout_ms": 10, "cancel_grace_ms": 20
            }}}))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            TOKEN = mare.task.token()
            T:register("managed", {
                description = "owns its timeout", parameters = { type = "object" },
                side_effect = "read", timeout_policy = "managed",
                execute = function()
                    ENTERED = true
                    while true do mare.task.sleep(20) end
                end,
            })
            CALL = mare.task.spawn(function()
                RESULT = T:execute({id="managed-call",name="managed",arguments={}}, {cancel=TOKEN})
            end)
        "#,
        )
        .await;
        wait_until(5, "managed tool execution", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return ENTERED == true").await }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(eval::<bool>(&h, "return RESULT == nil").await, "managed execution used the pipeline wall clock");
        exec(&h, "TOKEN:cancel(); CALL:wait()").await;
        assert!(eval::<bool>(&h, "return RESULT.cancelled == true and RESULT.is_error == true").await);
        h.shutdown().await;
    })
    .await
}

//! Usage across resume and fork after compaction/prune (provider usage audit): provider usage that
//! predates a compaction or prune is stale, a later reply makes it current again.

use super::*;

/// reference implementation: replay keeps the reported usage baseline.
#[tokio::test]
async fn resumed_provider_usage_triggers_auto_compaction_and_stays_with_its_model() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": {
            "threshold_tokens": base + 2000, "keep_recent_tokens": 1,
            "recent_user_tokens": 0, "stages": ["local"]
        } } });
        let mut initial_cfg = cfg.clone();
        initial_cfg["plugins"]["compact"]["auto"] = json!(false);
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(initial_cfg).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"
            L:provider('faux').stream = function(req, sink)
                sink({type = 'text_delta', delta = 'initial answer'})
                return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = 'initial answer'}},
                    usage = {input = 60000, output = 10, cache_read = 0, cache_write = 0}}
            end
        "#).await;
        for i in 0..4 {
            let first = h.prompt(&format!("initial question {i}: {}", "detail ".repeat(70)), json!({})).await.unwrap();
            assert!(first["error"].is_null(), "{first}");
        }
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;

        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).args(json!({ "resume": path })).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        exec(&h, r#"
            COMPACTED, STARTED = 0, 0
            C.cordis:on('compact/started', function(ev)
                if ev.reason == 'auto' then STARTED = STARTED + 1 end
            end)
            C.cordis:on('compact/completed', function(ev)
                if ev.reason == 'auto' and ev.status == 'completed' then COMPACTED = COMPACTED + 1 end
            end)
            L:provider('faux').stream = function(req, sink)
                if req.purpose == 'compact' then
                    return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = '## Summary\nRESUME-SUMMARY'}}}
                end
                sink({type = 'text_delta', delta = 'resumed answer'})
                return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = 'resumed answer'}},
                    usage = {input = 2, output = 1, cache_read = 0, cache_write = 0}}
            end
        "#).await;
        let usage: i64 = eval(&h, "return U:context_tokens()").await;
        assert_eq!(usage, 60010, "provider usage must be restored by resume");
        let result = h.prompt("next question", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (compacted, history): (i64, String) =
            eval(&h, "return COMPACTED, mare.json.encode(A:list_messages().data)").await;
        assert_eq!(compacted, 1, "resumed provider usage must trigger the first prepared request");
        assert!(history.contains("RESUME-SUMMARY"), "{history}");

        let path_a: String = eval(&h, "return S:info().path").await;
        exec(&h, r#"
            S:create()
            L:set_model('faux/echo2', {save = false})
            L:provider('faux').stream = function(req, sink)
                sink({type = 'text_delta', delta = 'other answer'})
                return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = 'other answer'}},
                    usage = {input = 60000, output = 10, cache_read = 0, cache_write = 0}}
            end
        "#).await;
        let other = h.prompt("other session", json!({})).await.unwrap();
        assert!(other["error"].is_null(), "{other}");
        let path_b: String = eval(&h, "return S:info().path").await;
        assert_ne!(path_a, path_b);
        global(&h, "PATH_A", path_a);
        global(&h, "PATH_B", path_b);
        exec(&h, "S:load({path = PATH_A})").await;
        let primed = h.prompt("prime first model", json!({})).await.unwrap();
        assert!(primed["error"].is_null(), "{primed}");
        let extra: i64 = eval(&h, r#"
            S:load({path = PATH_B})
            assert(U:context_tokens() == 60010)
            local history = C.cordis:get('agent'):messages()
            local before = STARTED
            K:prepare_request({model = L:resolve('faux/echo'), messages = history, system = '', tools = {}}, history)
            return STARTED - before
        "#).await;
        assert_eq!(extra, 0, "another session's different model must not trigger compaction");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn resume_after_compaction_invalidates_stale_pre_compaction_usage() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 200,
                    "recent_user_tokens": 0
                }
            }
        });
        let script = json!([
            { "text": format!("large first answer {}", "detail ".repeat(2500)) },
            { "text": format!("kept recent answer {}", "kept ".repeat(200)) },
            { "text": "## Summary\nCOMPACTED-SUMMARY" },
            { "text": "post-resume answer" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());

        h.prompt("turn 1", json!({})).await.unwrap();
        h.prompt("turn 2", json!({})).await.unwrap();
        let pre_compact_context: i64 = eval(&h, "return U:get().context_tokens").await;

        let (ok, err): (bool, String) =
            eval(&h, "local ok, r = pcall(K.run, K, nil, { reason = 'manual' }); return ok and r.status == 'compacted', tostring(r)").await;
        assert!(ok, "manual compaction failed: {err}");

        let (post_compact_usage, post_compact_measure, has_kept_assistant): (i64, i64, bool) = eval(
            &h,
            r#"local kept = false
               for _, m in ipairs(A:list_messages().data) do
                   if m.role == "assistant" and m.usage then
                       kept = true
                   end
               end
               return U:get().context_tokens, K:measure(), kept"#,
        )
        .await;
        assert!(has_kept_assistant, "test requires a kept recent assistant message with pre-compaction usage");
        assert!(
            post_compact_usage < pre_compact_context,
            "compaction must shrink context_tokens: {pre_compact_context} -> {post_compact_usage}"
        );
        assert_eq!(post_compact_usage, post_compact_measure);
        h.shutdown().await;

        // Resume before any new model request: usage.get must NOT report the stale pre-compaction context size.
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "post-resume answer" }])).config(cfg).resume().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());

        let (resumed_usage, resumed_measure): (i64, i64) =
            eval(&h, "return U:get().context_tokens, K:measure()").await;
        assert_eq!(
            resumed_usage, resumed_measure,
            "resumed usage.get().context_tokens must report post-compaction estimate, not stale pre-compaction usage ({pre_compact_context})"
        );
        assert!(
            resumed_usage < pre_compact_context,
            "resumed context_tokens ({resumed_usage}) must stay below pre-compaction ({pre_compact_context})"
        );

        // On the next model response, usage.get updates normally from the fresh provider usage.
        let res = h.prompt("turn 3 after resume", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let (after_prompt_usage, fresh_last_usage): (i64, i64) = eval(
            &h,
            r#"local msgs = A:list_messages().data
               local last = msgs[#msgs]
               local u = last.usage
               return U:get().context_tokens, (u.input or 0) + (u.cache_read or 0) + (u.cache_write or 0) + (u.output or 0)"#,
        )
        .await;
        assert_eq!(after_prompt_usage, fresh_last_usage);
        assert!(after_prompt_usage > 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_after_prune_invalidates_stale_pre_prune_usage() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let cfg = prune_cfg();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "done reading" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());

        h.prompt("read the big file", json!({})).await.unwrap();
        let pre_prune_context: i64 = eval(&h, "return U:get().context_tokens").await;
        let (ok, err): (bool, String) =
            eval(&h, "local reply = C.cordis:get('bus'):waterfall('compact/overflow', {}); local ok, r = reply.succeeded == true, reply.res; return ok and r.status == 'compacted', tostring(r)").await;
        assert!(ok, "prune compaction failed: {err}");
        let (post_prune_usage, post_prune_measure): (i64, i64) =
            eval(&h, "return U:get().context_tokens, K:measure()").await;
        assert_eq!(post_prune_usage, post_prune_measure);
        assert!(post_prune_usage < pre_prune_context, "{post_prune_usage} < {pre_prune_context}");
        h.shutdown().await;

        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "after prune resume" }])).config(cfg).resume().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());

        let (resumed_usage, resumed_measure): (i64, i64) = eval(&h, "return U:get().context_tokens, K:measure()").await;
        assert_eq!(resumed_usage, resumed_measure);
        assert!(
            resumed_usage < pre_prune_context,
            "resumed context_tokens ({resumed_usage}) must not revert to stale pre-prune usage ({pre_prune_context})"
        );
        h.shutdown().await;
    })
    .await
}

/// Fork copies the source's prune state; it must keep the prune *before* a later reply so the
/// fork's last provider usage stays current (`mare_lib.path_usage_stale` reads log order).
/// Before the fix the fork appended one aggregated prune after every copied message, so
/// "prune → new reply → fork" dropped the reply's provider context/cache figures.
#[tokio::test]
async fn fork_after_prune_then_reply_keeps_the_reply_usage() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let cfg = prune_cfg();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "done reading" },
            { "text": "reply after the prune" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());

        h.prompt("read the big file", json!({})).await.unwrap();
        let (ok, err): (bool, String) =
            eval(&h, "local reply = C.cordis:get('bus'):waterfall('compact/overflow', {}); local ok, r = reply.succeeded == true, reply.res; return ok and r.status == 'compacted', tostring(r)")
                .await;
        assert!(ok, "prune failed: {err}");
        h.prompt("one more turn", json!({})).await.unwrap();
        let (reply_context, reply_usage): (i64, i64) = eval(
            &h,
            r#"local msgs = A:list_messages().data
               local u = msgs[#msgs].usage
               return U:get().context_tokens, (u.input or 0) + (u.cache_read or 0) + (u.cache_write or 0) + (u.output or 0)"#,
        )
        .await;
        assert_eq!(reply_context, reply_usage, "precondition: the new reply's provider usage is current");
        assert_ne!(reply_usage, eval::<i64>(&h, "return K:measure()").await, "the estimate must be distinguishable");

        let forked: i64 = eval(&h, "S:fork({ session_id = S:info().session_id }); return U:get().context_tokens").await;
        assert_eq!(forked, reply_usage, "fork must keep the post-prune reply's provider usage");
        h.shutdown().await;

        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).resume().build().await;
        global(&h, "U", h.service_table("usage").unwrap());
        let resumed: i64 = eval(&h, "return U:get().context_tokens").await;
        assert_eq!(resumed, reply_usage, "resumed fork must keep the post-prune reply's provider usage");
        h.shutdown().await;
    })
    .await
}

/// Resuming a compacted session with the compact plugin turned off: the stale pre-compaction
/// usage is dropped, and with no context-size owner left the usage service must still report
/// a local estimate of the restored context, never 0 for a non-empty conversation.
#[tokio::test]
async fn resume_after_compaction_without_compact_plugin_estimates_context() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": { "compact": { "auto": false, "keep_recent_tokens": 200, "recent_user_tokens": 0 } }
        });
        let script = json!([
            { "text": format!("large first answer {}", "detail ".repeat(2500)) },
            { "text": format!("kept recent answer {}", "kept ".repeat(200)) },
            { "text": "## Summary\nCOMPACTED-SUMMARY" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        h.prompt("turn 1", json!({})).await.unwrap();
        h.prompt("turn 2", json!({})).await.unwrap();
        let pre_compact_context: i64 = eval(&h, "return U:get().context_tokens").await;
        let (ok, err): (bool, String) =
            eval(&h, "local ok, r = pcall(K.run, K, nil, { reason = 'manual' }); return ok and r.status == 'compacted', tostring(r)").await;
        assert!(ok, "manual compaction failed: {err}");
        h.shutdown().await;

        let off = json!({ "plugins": { "compact": { "enabled": false } } });
        let h = env.host().builtins(&["usage"]).faux(json!([])).config(off).resume().build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        assert!(h.service_table("compact").is_none(), "precondition: compact plugin is off");
        let (resumed, n_msgs): (i64, i64) = eval(&h, "return U:get().context_tokens, #A:list_messages().data").await;
        assert!(n_msgs > 0, "precondition: the resumed context is not empty");
        assert!(resumed > 0, "non-empty resumed context must not report 0 context tokens");
        assert!(
            resumed < pre_compact_context,
            "must not fall back to stale usage ({resumed} vs {pre_compact_context})"
        );
        h.shutdown().await;
    })
    .await
}

fn prune_cfg() -> serde_json::Value {
    json!({
        "plugins": {
            "compact": {
                "auto": false,
                "prune_tool_result_chars": 2000,
                "prune_head_chars": 100,
                "prune_tail_chars": 100,
                "prune_protect_tokens": 100,
                "prune_min_saving_tokens": 1000,
                "stages": ["prune"]
            }
        }
    })
}

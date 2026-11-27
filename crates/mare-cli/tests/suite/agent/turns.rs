//! Core agent turn loop: tool execution, batching, retries, overflow, abort, steer, follow-up.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn tool_call_edits_file_and_session_resumes_with_same_context() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("hello.txt"), "hello\nworld\n").unwrap();
        let script = json!([
            { "text": "Reading.", "tool_calls": [{ "name": "read", "arguments": { "path": "hello.txt" } }] },
            { "tool_calls": [{ "name": "edit", "arguments": { "path": "hello.txt",
                "old_string": "world", "new_string": "mare" } }] },
            { "text": "Changed world to mare." }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script.clone()).build().await;
        let res = h.prompt("rename world", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "Changed world to mare.");
        assert_eq!(std::fs::read_to_string(env.cwd.join("hello.txt")).unwrap(), "hello\nmare\n");
        // user, assistant(read), result, assistant(edit), result, assistant(final)
        assert_eq!(res["messages"].as_array().unwrap().len(), 6);
        h.shutdown().await;

        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).resume().build().await;
        assert_eq!(h.state().await["message_count"], 6);
        h.shutdown().await;
        assert_registered(env.home.path());
    })
    .await
}

#[tokio::test]
async fn readonly_calls_batch_and_turn_stopping_can_continue() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "alpha\n").unwrap();
        std::fs::create_dir(env.cwd.join("d")).unwrap();
        let script = json!([
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "a.txt" } },
                { "name": "ls", "arguments": { "path": "d" } } ] },
            { "text": "first" },
            { "text": "second" }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"local n = 0
            C.cordis:on("agent/turn-stopping", function() n = n + 1; if n == 1 then return { message = "continue" } end end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "second", "turn-stopping kept the loop going");
        let msgs = res["messages"].as_array().unwrap();
        // user, assistant(2 calls), result(read), result(ls), assistant(first), user(continue), assistant(second)
        assert_eq!(msgs.len(), 7, "{res}");
        assert_eq!((msgs[2]["tool_name"].as_str(), msgs[3]["tool_name"].as_str()), (Some("read"), Some("ls")));
        assert!(msgs[2]["content"][0]["text"].as_str().unwrap().contains("alpha"));
        assert_eq!(msgs[5]["content"][0]["text"], "continue");
        h.shutdown().await;
    })
    .await
}

/// agent/turn-stopping carries the run's identity and cancellation (a Stop hook keys its state by
/// `turn_id` and refuses to decide without `cancel`, so without them it kept every run going to the
/// turn limit): the same `turn_id` as step/started, and the run's live token.
#[tokio::test]
async fn turn_stopping_carries_the_runs_turn_id_and_cancel_token() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"STARTED, STOPPING, CANCEL_TYPE, CANCELLED = nil, nil, nil, nil
            C.cordis:on("step/started", function(ev) STARTED = ev.turn_id end)
            C.cordis:on("agent/turn-stopping", function(ev)
                STOPPING = ev.turn_id
                CANCEL_TYPE = ev.cancel and type(ev.cancel.is_cancelled) or "missing"
                CANCELLED = ev.cancel and ev.cancel:is_cancelled()
            end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "done", "{res}");
        let (started, stopping): (Option<String>, Option<String>) = eval(&h, "return STARTED, STOPPING").await;
        assert!(stopping.is_some(), "agent/turn-stopping carried no turn_id");
        assert_eq!(stopping, started, "turn-stopping's turn_id is the run's (agent/turn-start)");
        let (cancel_type, cancelled): (String, bool) = eval(&h, "return CANCEL_TYPE, CANCELLED").await;
        assert_eq!(cancel_type, "function", "agent/turn-stopping carried no cancel token");
        assert!(!cancelled, "a live run's token is not cancelled");
        h.shutdown().await;
    })
    .await
}

/// A run aborted while an agent/turn-stopping listener decides (a Stop hook that stops it, or the
/// user) ends interrupted, like an abort anywhere else in the turn: termination `interrupted` and
/// the `turn_aborted` marker, not an error.
#[tokio::test]
async fn an_abort_during_turn_stopping_ends_the_run_interrupted() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&h);
        exec(&h, r#"C.cordis:on("agent/turn-stopping", function() A:cancel() end)"#).await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["error"]["code"], "aborted", "{res}");
        assert_eq!(res["termination_reason"], "interrupted", "{res}");
        let msgs = res["messages"].as_array().unwrap();
        assert_eq!(msgs.last().unwrap()["turn_aborted"], true, "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn transient_model_error_is_retried() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "error": "overloaded", "retryable": true }, { "text": "recovered" }]);
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script)
            .config(json!({ "plugins": { "llm": { "retry_base_ms": 5 } } }))
            .no_session()
            .build()
            .await;
        let res = h.prompt("hi", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "recovered");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn non_retryable_model_error_is_surfaced() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "error": "invalid api key", "retryable": false }, { "text": "never" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let res = h.prompt("hi", json!({})).await.unwrap();
        assert!(res["error"]["message"].as_str().unwrap_or("").contains("invalid api key"), "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn context_overflow_compacts_once_and_retries() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            // A real overflow leaves enough history for the wrapped summary to be smaller.
            // The old seven-byte answer was shorter than the summary wrapper itself.
            { "text": "looking ".repeat(300), "tool_calls": [{ "name": "ls", "arguments": {} }] },
            { "error": "prompt is too long", "kind": "context_overflow" },
            { "text": "## Original Request\nlook around" },
            { "text": "after compaction" }
        ]);
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script)
            .config(json!({ "plugins": { "compact": { "keep_recent_tokens": 1 } } }))
            .no_session()
            .build()
            .await;
        let res = h.prompt("look", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "after compaction");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn context_overflow_stops_if_even_the_summary_cannot_shrink_history() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "text": "looking", "tool_calls": [{ "name": "ls", "arguments": {} }] },
            { "error": "prompt is too long", "kind": "context_overflow" },
            { "text": "## Original Request\nlook around" },
            { "text": "## Original Request\nlook around" },
            { "text": "## Original Request\nlook around" },
            { "text": "must not retry with a larger context" }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        exec(&h, "COMPACT_ATTEMPTS = 0; C.cordis:on('compact/started', function() COMPACT_ATTEMPTS = COMPACT_ATTEMPTS + 1 end)").await;
        let res = h.prompt("look", json!({})).await.unwrap();
        assert!(res["error"]["message"].as_str().unwrap_or("").contains("prompt is too long"), "{res}");
        assert_ne!(res["text"], "must not retry with a larger context");
        let (attempts, committed): (i64, i64) = eval(
            &h,
            "local n = 0; for _, m in ipairs(A:list_messages().data) do if m.compaction then n = n + 1 end end; return COMPACT_ATTEMPTS, n",
        )
        .await;
        assert_eq!(attempts, 1, "a nonshrinking full-range summary must stop immediately");
        assert_eq!(committed, 0, "a larger summary must not replace the original history");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn abort_mid_stream_ends_the_turn() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "x".repeat(4000), "delay_ms": 10 }, { "text": "next" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        exec(&h, "STREAM_STARTED = false; C.cordis:on('item/delta', function(e) if e.kind == 'text' then STREAM_STARTED = true end end)").await;
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("long answer please", json!({})).await.unwrap() });
        wait_until(60, "stream to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STREAM_STARTED").await }
        })
        .await;
        // the 60 s ceiling only guards a hang; the stop itself must be prompt even under load
        let aborted_at = std::time::Instant::now();
        h.abort().await.unwrap();
        let res =
            tokio::time::timeout(std::time::Duration::from_secs(60), run).await.expect("abort was ignored").unwrap();
        let stop = aborted_at.elapsed();
        assert!(stop < std::time::Duration::from_secs(5), "the turn took {stop:?} to stop after the abort");
        assert_eq!(res["error"]["code"], "aborted", "{res}");
        assert!(!h.is_busy().await);
        assert!(
            eval::<bool>(
                &h,
                "local lease = C.cordis:get('kernel'):request_lease(); if not lease then return false end; lease:release(); return true",
            )
            .await,
            "abort must release the request lease"
        );
        // what was streamed is kept (marked interrupted) and the model sees an abort marker
        let msgs = res["messages"].as_array().unwrap();
        let partial = msgs.iter().find(|m| m["role"] == "assistant").expect("partial assistant kept");
        assert_eq!(partial["interrupted"], true, "{partial}");
        assert!(partial["content"][0]["text"].as_str().unwrap().starts_with("xxxxxxxx"), "{partial}");
        assert_eq!(msgs.last().unwrap()["turn_aborted"], true, "{res}");
        // the next prompt starts cleanly
        let res = h.prompt("again", json!({})).await.unwrap();
        assert_eq!(res["text"], "next", "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_lua_request_scope_releases_its_lease() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).user_plugins().no_session().build().await;
        expose(&h);
        let task = h
            .lua()
            .load("local lease <close> = assert(C.cordis:get('kernel'):request_lease()); mare.task.sleep(5000)")
            .exec_async();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), task).await.is_err());
        assert!(
            eval::<bool>(
                &h,
                "local lease = C.cordis:get('kernel'):request_lease(); if not lease then return false end; lease:release(); return true",
            )
            .await
        );
        h.shutdown().await;
    })
    .await
}

/// `mare.reload_version()` stays public while `request_lease` lives on `kernel` (not on global
/// `mare`) and is exclusive one lease at a time.
#[tokio::test]
async fn reload_version_is_public_and_request_lease_is_host_method() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["copy"]).user_plugins().no_session().build().await;
        expose(&h);
        let (busy, again, has_public): (String, bool, bool) = eval(
            &h,
            r#"local k = C.cordis:get("kernel")
               local a = assert(k:request_lease())
               local b, why = k:request_lease()
               assert(b == nil)
               a:release()
               local c = k:request_lease()
               if c then c:release() end
               return why, c ~= nil, (mare.request_lease ~= nil or mare.set_session_identity ~= nil)"#,
        )
        .await;
        assert!(busy.contains("another request is in progress"), "{busy}");
        assert!(again, "a released lease can be taken again");
        assert!(!has_public, "request_lease and set_session_identity must not be on global mare");
        let v1: String = eval(&h, "return mare.reload_version()").await;
        h.kernel.reload(Some("copy")).await.unwrap();
        assert_ne!(eval::<String>(&h, "return mare.reload_version()").await, v1, "a reload changes the version");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn user_plugin_holds_the_request_lease_and_blocks_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/lease_holder.lua"),
            r#"return {
                name = "lease-holder",
                inject = { "kernel", "task" },
                apply = function(ctx)
                    local rel_tx, rel_rx, done_tx, done_rx
                    mare._lease_acquire = function()
                        local ready_tx, ready_rx = mare.task.channel()
                        rel_tx, rel_rx = mare.task.channel()
                        done_tx, done_rx = mare.task.channel()
                        ctx:spawn(function()
                            do
                                local lease <close> = assert(ctx.kernel:request_lease())
                                ready_tx:send(true)
                                rel_rx:recv()
                            end
                            done_tx:send(true)
                        end)
                        return assert(ready_rx:recv())
                    end
                    mare._lease_release = function()
                        if rel_tx then
                            rel_tx:send(true)
                            rel_tx = nil
                            done_rx:recv()
                            done_rx = nil
                        end
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().builtins(&["copy"]).user_plugins().no_session().build().await;
        assert!(eval::<bool>(&h, "return mare._lease_acquire()").await, "user plugin must acquire request lease");

        let err = h.reload_all().await.expect_err("reload_all must be refused while a user plugin holds the lease");
        assert!(
            err.to_string().contains("model request is active") || err.to_string().contains("already in progress"),
            "unexpected reload error while lease held: {err:#}"
        );
        exec(&h, "mare._lease_release()").await;
        let reloaded = h.reload_all().await;
        assert!(reloaded.is_ok(), "reload_all failed after lease release: {reloaded:?}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn second_lease_is_refused_while_one_is_held() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/lease_exclusive.lua"),
            r#"return {
                name = "lease-exclusive",
                inject = { "kernel" },
                apply = function(ctx)
                    mare._test_exclusive = function()
                        local a = assert(ctx.kernel:request_lease())
                        local b, why = ctx.kernel:request_lease()
                        assert(b == nil, "second lease must fail while first is held")
                        a:release()
                        local c = assert(ctx.kernel:request_lease())
                        c:release()
                        return tostring(why)
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let why: String = eval(&h, "return mare._test_exclusive()").await;
        assert!(why.contains("another request is in progress"), "{why}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn lease_dies_with_its_holder_plugin() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        let holder_path = env.config_dir().join("plugins/lease_leaker.lua");
        std::fs::write(
            &holder_path,
            r#"return {
                name = "lease-leaker",
                inject = { "kernel" },
                apply = function(ctx)
                    mare._leaked_lease_tok = mare.task.token()
                    mare._leaked_lease = assert(ctx.kernel:request_lease(mare._leaked_lease_tok))
                    mare._dispose_leaker = function()
                        ctx.fiber:dispose():await()
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        // While lease-leaker is alive, a second request_lease fails.
        let busy: bool = eval(
            &h,
            r#"
            local b = C.cordis:get("kernel"):request_lease()
            return b == nil
            "#,
        )
        .await;
        assert!(busy, "lease held by lease-leaker must block a second caller");

        // Disposing lease-leaker cancels its owner token, automatically releasing the lease.
        exec(&h, "mare._dispose_leaker()").await;
        let recovered: bool = eval(
            &h,
            r#"
            local c = C.cordis:get("kernel"):request_lease()
            if not c then return false end
            c:release()
            return true
            "#,
        )
        .await;
        assert!(recovered, "disposing holder plugin must release its request lease");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn steer_while_busy_reaches_the_next_request() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "text": "y".repeat(1600), "delay_ms": 10, "tool_calls": [{ "name": "ls", "arguments": {} }] },
            { "text": "steered" }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"REQS = {} C.cordis:on("request/build", function(req, next) REQS[#REQS + 1] = mare.json.encode(req.messages) return next() end)"#,
        )
        .await;
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("start", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "turn to start", || {
            let h = h.clone();
            async move { h.is_busy().await }
        })
        .await;
        h.steer("use the src dir").await.unwrap();
        let res = run.await.unwrap();
        assert_eq!(res["text"], "steered", "{res}");
        let second: String = eval(&h, "return REQS[2] or ''").await;
        assert!(second.contains("use the src dir"), "steer missing from the next request: {second}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn follow_up_runs_after_the_current_turn() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "z".repeat(1600), "delay_ms": 5 }, { "text": "follow answer" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("first", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "turn to start", || {
            let h = h.clone();
            async move { h.is_busy().await }
        })
        .await;
        h.follow_up("and then this").await.unwrap();
        run.await.unwrap();
        wait_until(WAIT_SECS, "follow-up turn", || {
            let h = h.clone();
            async move {
                let last: String = eval(&h, "local m = A:list_messages().data; local l = m[#m]; return l.role .. ':' .. ((l.content[1] or {}).text or '')").await;
                last == "assistant:follow answer" && !h.is_busy().await
            }
        })
        .await;
        let order: String = eval(
            &h,
            r#"local t = {} for _, m in ipairs(A:list_messages().data) do if m.role == "user" then t[#t + 1] = m.content[1].text end end
               return table.concat(t, "|")"#,
        )
        .await;
        assert_eq!(order, "first|and then this");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_error_is_reported_to_the_model() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "missing.txt" } }] },
            { "tool_calls": [{ "name": "no_such_tool", "arguments": {} }] },
            { "text": "handled" }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h.prompt("go", json!({})).await.unwrap();
        let mut terminal = Vec::new();
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                && ev.name == "item/completed"
                && ev.payload["item"]["type"] == "tool_call"
            {
                terminal.push(ev.payload["item"].clone());
            }
        }
        assert_eq!(terminal.len(), 2, "{terminal:?}");
        for event in &terminal {
            assert_eq!(event["result_status"], "error");
            assert_eq!(event["execution_status"], "unknown");
            assert_eq!(event["tool_call_id"], event["message"]["tool_call_id"]);
            assert_eq!(event["message"]["is_error"], true);
        }
        assert_eq!(res["text"], "handled");
        let msgs = res["messages"].as_array().unwrap();
        let results: Vec<_> = msgs.iter().filter(|m| m["role"] == "tool_result").collect();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r["is_error"] == true), "{results:?}");
        assert!(results[1]["content"].to_string().contains("Unknown tool"), "{:?}", results[1]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn truncated_response_fails_its_tool_calls_without_running_them() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "stop": "length", "tool_calls": [{ "name": "write", "arguments": { "path": "cut.txt", "content": "half" } }] },
            { "text": "retrying smaller" }
        ]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        expose(&h);
        let res = h.prompt("write it", json!({})).await.unwrap();
        assert!(!env.cwd.join("cut.txt").exists(), "a call from a truncated response ran");
        let results = tool_results(&h).await;
        assert!(results.contains("output token limit"), "{results}");
        assert_eq!(res["text"], "retrying smaller", "the model gets another turn");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parallel_safe_calls_respect_max_parallel_and_keep_model_order() {
    local(async {
        let env = TestEnv::new().await;
        let calls: Vec<_> = (1..=5).map(|i| json!({ "name": "probe", "arguments": { "i": i } })).collect();
        let script = json!([{ "tool_calls": calls }, { "text": "done" }]);
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script)
            .no_session()
            .config(json!({ "plugins": { "agent": { "max_parallel": 2 } } }))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"RUNNING, PEAK, ORDER = 0, 0, {}
            T:register("probe", { description = "x", parameters = { type = "object", properties = {} },
                concurrency_safe = function(args) return args.i ~= 99 end,
                execute = function(args)
                    RUNNING = RUNNING + 1; PEAK = math.max(PEAK, RUNNING)
                    mare.task.sleep(60 - args.i * 10) -- later calls finish first
                    RUNNING = RUNNING - 1
                    return "r" .. args.i
                end })
            C.cordis:on("item/completed", function(ev)
                local m = ev.item and ev.item.message
                if m and m.role == "tool_result" then ORDER[#ORDER + 1] = m.content[1].text end
            end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "done", "{res}");
        let (peak, order): (i64, String) = eval(&h, "return PEAK, table.concat(ORDER, ',')").await;
        assert_eq!(peak, 2, "max_parallel caps concurrency");
        assert_eq!(order, "r1,r2,r3,r4,r5", "results commit in the model's order");
        h.shutdown().await;
    })
    .await
}

/// An abort in the middle of a parallel batch (Kimi agent-core-v2
/// toolExecutor.test.ts:848 "every tool.call.started still has a matching tool.result when aborted
/// mid-batch", :807 "does not start a queued conflicting tool after abort"): two concurrency-safe
/// calls run together, a third (not concurrency-safe) waits behind them. Aborting while both run
/// gives every call exactly one result: the two running ones cancelled, the queued one never
/// started and aborted.
#[tokio::test]
async fn abort_mid_parallel_batch_answers_every_call_once_and_never_starts_the_queued_one() {
    local(async {
        let env = TestEnv::new().await;
        let calls: Vec<_> = (1..=3).map(|i| json!({ "name": "probe", "arguments": { "i": i } })).collect();
        let script = json!([{ "tool_calls": calls }]);
        let h = env
            .host()
            .faux(script)
            .no_session()
            .config(json!({ "plugins": { "agent": { "max_parallel": 2 } } }))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"RUNNING, STARTED, RESULTS = 0, {}, {}
            T:register("probe", { description = "x", parameters = { type = "object", properties = {} },
                concurrency_safe = function(args) return args.i ~= 3 end,
                execute = function(args)
                    STARTED[#STARTED + 1] = args.i
                    RUNNING = RUNNING + 1
                    mare.task.sleep(60000)
                    return "r" .. args.i
                end })
            C.cordis:on("item/completed", function(ev)
                local m = ev.item and ev.item.message
                if m and m.role == "tool_result" then
                    RESULTS[#RESULTS + 1] = m.tool_call_id .. "=" .. ((m.content[1] or {}).text or "")
                end
            end)"#,
        )
        .await;
        let run = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.prompt("go", json!({})).await.unwrap() })
        };
        wait_until(WAIT_SECS, "both safe calls to run", || {
            let h = h.clone();
            async move { eval::<i64>(&h, "return RUNNING").await == 2 }
        })
        .await;
        h.abort().await.unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
        assert_eq!(res["error"]["code"], "aborted", "{res}");
        let (started, results): (Vec<i64>, Vec<String>) = eval(&h, "return STARTED, RESULTS").await;
        h.shutdown().await;
        let mut started = started;
        started.sort();
        assert_eq!(started, [1, 2], "the queued call must never start: {started:?}");
        assert_eq!(results.len(), 3, "one result per call: {results:?}");
        let ids: std::collections::BTreeSet<&str> = results.iter().map(|r| r.split('=').next().unwrap()).collect();
        assert_eq!(ids.len(), 3, "a call answered twice or one missing: {results:?}");
        assert_eq!(results.iter().filter(|r| r.contains("cancelled")).count(), 2, "{results:?}");
        assert_eq!(results.iter().filter(|r| r.contains("Aborted")).count(), 1, "{results:?}");
    })
    .await
}

#[tokio::test]
async fn a_paused_turn_continues_without_a_user_message() {
    local(async {
        let env = TestEnv::new().await;
        // Anthropic pause_turn: the paused reply goes back as it is and the server continues; no
        // user message is added in between (Kimi anthropic.ts:62-91)
        let script = json!([{ "text": "searching", "stop": "paused" }, { "text": "found it" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let res = h.prompt("look it up", json!({})).await.unwrap();
        assert_eq!(res["text"], "found it", "{res}");
        let roles: Vec<&str> = res["messages"].as_array().unwrap().iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "assistant"], "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_same_tool_call_repeated_gets_a_reminder_but_still_runs() {
    local(async {
        let env = TestEnv::new().await;
        let ls = json!({ "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] });
        let script = json!([ls, ls, ls, { "text": "done" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let res = h.prompt("look around", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        // reference implementation: advisory at the 3rd identical call, never a refusal
        assert!(msgs.contains("You are repeating the exact same tool call"), "{msgs}");
        let results = res["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool_result").count();
        assert_eq!(results, 3, "every call still ran");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_repeat_reminder_preview_cuts_cjk_and_emoji_arguments_on_character_boundaries() {
    local(async {
        let env = TestEnv::new().await;
        let ls = json!({ "tool_calls": [{ "name": "ls", "arguments": { "path": "日本語のパス🙂🙂" } }] });
        let script = json!([ls, ls, ls, { "text": "done" }]);
        let cfg = json!({ "plugins": { "repeat-guard": { "thresholds": [2, 3], "arguments_preview_chars": 13 } } });
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script)
            .config(cfg)
            .no_session()
            .build()
            .await;
        // the detailed reminder fires at the 3rd call; its preview keeps 13 code points of
        // `{"path":"日本語のパス🙂🙂"}` (19 in all) — a byte cut would split 本 in half
        let res = h.prompt("look around", json!({})).await.unwrap();
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        assert!(msgs.contains(r#"- arguments: {\"path\":\"日本語の… (+6 more chars)"#), "{msgs}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repeat_guard_resets_on_user_message_or_other_call_hits_default_5_and_8_and_honors_filters() {
    local(async {
        let ls = json!({ "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] });
        let rd = json!({ "tool_calls": [{ "name": "read", "arguments": { "path": "note.txt" } }] });

        // 1. Default thresholds (3, 5, 8): gentle at 3, detailed with consecutive_calls 5 and 8.
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("note.txt"), "hi\n").unwrap();
        let script = json!([ls, ls, ls, ls, ls, ls, ls, ls, { "text": "done" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let res = h.prompt("loop it", json!({})).await.unwrap();
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        assert!(msgs.contains("You are repeating the exact same tool call"), "{msgs}");
        assert!(msgs.contains("consecutive_calls: 5"), "expected 5th-call reminder: {msgs}");
        assert!(msgs.contains("consecutive_calls: 8"), "expected 8th-call reminder: {msgs}");
        h.shutdown().await;
        drop(env);

        // 2. Intervening different tool call resets the chain (2 ls + 1 read + 2 ls -> no reminder).
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("note.txt"), "hi\n").unwrap();
        let script = json!([ls, ls, rd, ls, ls, { "text": "done" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let res = h.prompt("interleaved", json!({})).await.unwrap();
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        assert!(!msgs.contains("repeating the exact same tool call"), "other call must reset chain: {msgs}");
        h.shutdown().await;
        drop(env);

        // 3. New user prompt resets the chain across turns (2 ls in turn 1 + 2 ls in turn 2 -> no reminder).
        let env = TestEnv::new().await;
        let script = json!([ls, ls, { "text": "t1" }, ls, ls, { "text": "t2" }]);
        let h = env.host().builtins(&["compact", "repeat-guard", "copy"]).faux(script).no_session().build().await;
        let r1 = h.prompt("turn 1", json!({})).await.unwrap();
        let r2 = h.prompt("turn 2", json!({})).await.unwrap();
        let msgs = format!(
            "{}{}",
            serde_json::to_string(&r1["messages"]).unwrap(),
            serde_json::to_string(&r2["messages"]).unwrap()
        );
        assert!(!msgs.contains("repeating the exact same tool call"), "user prompt must reset chain: {msgs}");
        h.shutdown().await;
        drop(env);

        // 4. include/exclude filters: excluded tool is never guarded; include restricts to matching tools.
        let env = TestEnv::new().await;
        let script = json!([ls, ls, ls, { "text": "done" }]);
        let cfg = json!({ "plugins": { "repeat-guard": { "exclude": ["l*"] } } });
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script.clone())
            .config(cfg)
            .no_session()
            .build()
            .await;
        let res = h.prompt("excluded", json!({})).await.unwrap();
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        assert!(!msgs.contains("repeating the exact same tool call"), "excluded tool must not trigger: {msgs}");
        h.shutdown().await;
        drop(env);

        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "repeat-guard": { "include": ["read*"] } } });
        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(script)
            .config(cfg)
            .no_session()
            .build()
            .await;
        let res = h.prompt("not included", json!({})).await.unwrap();
        let msgs = serde_json::to_string(&res["messages"]).unwrap();
        assert!(!msgs.contains("repeating the exact same tool call"), "non-included tool must not trigger: {msgs}");
        h.shutdown().await;
    })
    .await
}

/// a real `mare` killed while a model request is in flight; the resumed
/// retry reuses the stable request id, so the crash guard counts the crash (the old fingerprint
/// from the history tail changed once `aborted` was appended, and never accumulated).
#[tokio::test]
async fn a_request_interrupted_by_a_crash_is_retried_with_the_same_id_and_counted() {
    local(async {
        let env = TestEnv::new().await;
        let slow = env.cwd.join("slow.json");
        std::fs::write(&slow, json!([{ "text": "x".repeat(40_000), "delay_ms": 10 }]).to_string()).unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--yolo", "--model", "faux/echo", "-p", "go"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &slow)
            .current_dir(&env.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = ChildGroup::spawn(&mut cmd).unwrap();
        // wait until the request marker is on disk and the crash guard marks it in flight
        let guard = env.home.path().join("state/crash-guard.json");
        let t0 = std::time::Instant::now();
        let id = loop {
            if let Ok(g) = std::fs::read_to_string(&guard)
                && let Some(i) = g.find("\"req:")
            {
                break g[i + 5..].split('"').next().unwrap().to_string();
            }
            assert!(child.0.try_wait().unwrap().is_none(), "mare exited before the request was in flight");
            // a ceiling only (a debug build starting under a loaded machine takes tens of seconds);
            // the marker is the condition
            assert!(t0.elapsed() < std::time::Duration::from_secs(100), "the request never got in flight");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        drop(child); // SIGKILL mid-request: no cleanup

        let h = env
            .host()
            .builtins(&["compact", "repeat-guard", "copy"])
            .faux(json!([{ "text": "recovered" }]))
            .resume()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"local orig = mare.crash.begin
               mare.crash.begin = function(fp) local n = orig(fp) FP, CRASHES = fp, n return n end"#,
        )
        .await;
        let pending: String = eval(&h, "local p = A:pending_request() return p and p.request_id or ''").await;
        assert_eq!(pending, id, "resume picks up the interrupted request");
        let res: String = eval(
            &h,
            "local s = A:continue(); local r = s.turn_id and A:wait(s.turn_id) or s; return tostring(r.error)",
        )
        .await;
        assert_eq!(res, "nil", "the retry succeeded");
        let (fp, crashes): (String, i64) = eval(&h, "return FP, CRASHES").await;
        assert_eq!(fp, format!("req:{id}"), "the retry reuses the request id");
        assert_eq!(crashes, 1, "the crash counts toward the limit");
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains(r#""kind":"agent/request-aborted""#), "the interrupted attempt is recorded");
        assert!(raw.contains(r#""attempt":1"#), "the attempt count is persisted before the retry");
        assert!(!raw.contains("xxxxxxxx"), "no streamed partial reply was written");
    })
    .await
}

/// agent:abort_turn({turn_id}) (Codex `turn/interrupt { turnId }`, app-server
/// turn_interrupt.rs: turn_interrupt_aborts_running_turn, turn_interrupt_rejects_completed_turn):
/// it cancels the active turn only when the id is that turn's — a stale id (another turn, a
/// finished one, none running) is refused with the reason and changes nothing; queued input stays.
#[tokio::test]
async fn abort_turn_cancels_only_the_named_active_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "first turn" }, { "text": "should not run" }])).build().await;
        expose(&h);
        let r: serde_json::Value = eval::<String>(
            &h,
            r#"
            local out = {}
            local idle_cancel = A:cancel({ turn_id = "t-none", clear_queue = false })
            out.idle_has_turn_id = idle_cancel.turn_id ~= nil
            out.bad_args = not pcall(A.cancel, A, { turn_id = "" })
            local at_commit_tx, at_commit_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local delivered = 0
            C.cordis:on("step/started", function() delivered = delivered + 1 end)
            local start_tx, start_rx = mare.task.channel()
            local go_tx, go_rx = mare.task.channel()
            local gated = false
            C.cordis:on("turn/started", function()
                if not gated then
                    gated = true
                    start_tx:send(true)
                    go_rx:recv()
                end
            end)
            C.cordis:get("policy"):voter("session/commit-ready", function(ev)
                for _, m in ipairs(ev.messages or {}) do
                    if m._input_id then
                        at_commit_tx:send(true)
                        release_rx:recv()
                        break
                    end
                end
            end)
            local started = A:prompt("go")
            assert(start_rx:recv())
            assert(A:prompt("keep me", { when_busy = "queue" }).status == "queued")
            go_tx:send(true)
            assert(at_commit_rx:recv())
            local turn_id = A:get().turn_id
            local wrong_cancel = A:cancel({ turn_id = "t-other", clear_queue = false })
            out.wrong_has_turn_id = wrong_cancel.turn_id ~= nil
            out.busy_after_wrong = A:get().busy
            local right_cancel = A:cancel({ turn_id = turn_id, clear_queue = false })
            out.right_turn_id = right_cancel.turn_id
            out.expected_turn_id = turn_id
            release_tx:send(true)
            out.result_error = A:wait(started.turn_id).error
            local stale_cancel = A:cancel({ turn_id = turn_id, clear_queue = false })
            out.stale_has_turn_id = stale_cancel.turn_id ~= nil
            local q = A:list_queue().data
            out.follow = q[1] and q[1].text or ""
            out.delivered = delivered
            return mare.json.encode(out)
        "#,
        )
        .await
        .parse()
        .unwrap();
        h.shutdown().await;
        assert_eq!(r["idle_has_turn_id"], false, "{r}");
        assert_eq!(r["bad_args"], true, "{r}");
        assert_eq!(r["wrong_has_turn_id"], false, "{r}");
        assert_eq!(r["busy_after_wrong"], true, "a stale id leaves the turn running: {r}");
        assert_eq!(r["right_turn_id"], r["expected_turn_id"], "{r}");
        assert_eq!(r["result_error"]["code"], "aborted", "{r}");
        assert_eq!(r["stale_has_turn_id"], false, "a finished turn returns empty turn_id: {r}");
        assert_eq!(r["follow"], "keep me", "queued input stays: {r}");
        assert_eq!(r["delivered"], 1, "the queued follow-up did not run: {r}");
    })
    .await
}

#[tokio::test]
async fn duplicate_provider_call_ids_are_rewritten_and_kept_as_provider_call_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let report: serde_json::Value = eval::<String>(
            &h,
            r#"
            T:register("probe_tool", {
                description = "probe tool",
                side_effect = "read",
                idempotent = true,
                parameters = { type = "object", properties = { step = { type = "integer" } } },
                execute = function(args)
                    return { content = { { type = "text", text = "ok-" .. tostring(args.step) } } }
                end,
            })
            local llm = C.cordis:get("llm")
            local turn = 0
            llm:register_provider({
                provider = "dup-call-id-probe",
                is_configured = function() return true end,
                models = { { model_id = "m1", name = "m1", context_window = 128000, max_output = 4096 } },
                stream = function(req, sink)
                    turn = turn + 1
                    if turn == 1 then
                        return {
                            role = "assistant",
                            content = { { type = "tool_call", id = "dup_call", name = "probe_tool", arguments = { step = 1 } } },
                            stop_reason = "tool_use",
                            usage = { input = 5, output = 2 },
                        }
                    elseif turn == 2 then
                        return {
                            role = "assistant",
                            content = {
                                { type = "tool_call", id = "dup_call", name = "probe_tool", arguments = { step = 2 } },
                                { type = "tool_call", id = "dup_call", name = "probe_tool", arguments = { step = 3 } },
                            },
                            stop_reason = "tool_use",
                            usage = { input = 5, output = 2 },
                        }
                    else
                        return {
                            role = "assistant",
                            content = { { type = "text", text = "done" } },
                            stop_reason = "stop",
                            usage = { input = 5, output = 2 },
                        }
                    end
                end,
            })
            llm:set_model("dup-call-id-probe/m1")
            local started = A:prompt("go")
            local res = A:wait(started.turn_id)
            assert(res and not res.error, tostring(res and res.error))
            return mare.json.encode({
                messages = A:list_messages().data,
                path = C.cordis:get("session"):info().path,
            })
        "#,
        )
        .await
        .parse()
        .unwrap();
        h.shutdown().await;

        let msgs = report["messages"].as_array().unwrap();
        let calls: Vec<_> = msgs
            .iter()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().unwrap().iter())
            .filter(|b| b["type"] == "tool_call")
            .collect();
        assert_eq!(calls.len(), 3, "{calls:?}");
        let id1 = calls[0]["id"].as_str().unwrap();
        let id2 = calls[1]["id"].as_str().unwrap();
        let id3 = calls[2]["id"].as_str().unwrap();
        assert_eq!(id1, "dup_call");
        assert!(calls[0].get("provider_call_id").is_none() || calls[0]["provider_call_id"].is_null());
        assert_ne!(id2, "dup_call");
        assert_ne!(id3, "dup_call");
        assert_ne!(id2, id3);
        assert_eq!(calls[1]["provider_call_id"], "dup_call");
        assert_eq!(calls[2]["provider_call_id"], "dup_call");

        let results: Vec<_> = msgs
            .iter()
            .filter(|m| m["role"] == "tool_result")
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        assert_eq!(results, vec![id1, id2, id3], "tool_results match rewritten call_ids");

        let entries = mare_store::read_entries(std::path::Path::new(report["path"].as_str().unwrap())).unwrap();
        let disk_calls: Vec<_> = entries
            .iter()
            .filter(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
            .flat_map(|e| e["message"]["content"].as_array().unwrap().iter())
            .filter(|b| b["type"] == "tool_call")
            .collect();
        assert_eq!(disk_calls[1]["provider_call_id"], "dup_call");
        assert_eq!(disk_calls[2]["provider_call_id"], "dup_call");
    })
    .await
}

#[tokio::test]
async fn third_party_agent_owner_acquires_bounded_request_lease_and_marks_committed() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("custom_agent.lua"),
            r#"
            return {
                name = "custom-agent",
                provide = { "agent" },
                inject = { "kernel", "task", "bus" },
                apply = function(ctx)
                    local svc = {}
                    for _, m in ipairs({
                        "prompt", "steer", "cancel", "continue", "wait", "get", "list_queue",
                        "update_queued", "delete_queued", "list_messages", "messages",
                        "prepare_commit", "activity", "register_activity", "set_messages",
                        "adopt_session", "session_revision", "check_session_revision", "set_tool_states",
                        "set_pending_request", "pending_request", "append", "persist_error", "notify",
                        "interrupted", "set_session_id", "set_queued", "baseline_cut",
                    }) do
                        svc[m] = function() return {} end
                    end
                    svc.get = function() return { busy = false, queued = 0 } end
                    svc.activity = function() return { version = 1, busy = false, queued = 0, turn_cancelled = false } end
                    svc.list_queue = function() return { data = mare.json.array() } end
                    svc.list_messages = function() return { data = mare.json.array() } end
                    svc.interrupted = function() return false end
                    svc.persist_error = function() return nil end

                    local turns = {}
                    local seq = 0
                    local agent_fiber = ctx:inject({}, function(sub)
                        local function finish(turn_id, res)
                            local entry = turns[turn_id]
                            if entry then
                                res.turn_id = turn_id
                                res.status = res.status or "completed"
                                res.started_at_ms = res.started_at_ms or 0
                                entry.result = res
                                entry.tx:send(true)
                            end
                        end

                        local function handle_prompt(_, _, input)
                            if input == "leak" then
                                local tok = mare.task.token()
                                local ready_tx, ready_rx = mare.task.channel()
                                sub:spawn(function()
                                    local lease, reason = sub.kernel:request_lease(tok)
                                    assert(lease ~= nil, "third-party active agent must acquire lease: " .. tostring(reason))
                                    mare._leaked_lease = lease
                                    mare._leaked_lease_token = tok
                                    ready_tx:send(true)
                                    tok:wait()
                                end)
                                assert(mare.task.timeout(5000, function() return ready_rx:recv() end))
                                return { status = "started", turn_id = "leak-0" }
                            end
                            if input == "leak-no-arg-active" then
                                local ready_tx, ready_rx = mare.task.channel()
                                local rel_tx, rel_rx = mare.task.channel()
                                mare._no_arg_release_tx = rel_tx
                                sub:spawn(function()
                                    local lease, reason = sub.kernel:request_lease()
                                    assert(lease ~= nil, "third-party active agent must acquire lease: " .. tostring(reason))
                                    mare._leaked_provider_lease = lease
                                    ready_tx:send(true)
                                    rel_rx:recv()
                                    sub.kernel:mark_call_committed()
                                end)
                                assert(mare.task.timeout(5000, function() return ready_rx:recv() end))
                                return { status = "started", turn_id = "leak-no-arg-0" }
                            end
                            if input == "leak-unbounded-cancel" then
                                local lease, reason = sub.kernel:request_lease()
                                assert(lease ~= nil, "third-party active agent must acquire lease: " .. tostring(reason))
                                mare._leaked_provider_lease = lease
                                return { status = "started", turn_id = "leak-provider-0" }
                            end
                            if input == "retain-released" then
                                mare._retained_leases = mare._retained_leases or {}
                                local lease, reason = sub.kernel:request_lease()
                                assert(lease ~= nil, tostring(reason))
                                lease:release()
                                mare._retained_leases[#mare._retained_leases + 1] = lease
                                return { status = "started", turn_id = "retained-0" }
                            end
                            if input == "error-unwind" then
                                local ok_err, err = pcall(function()
                                    local lease <close>, reason = sub.kernel:request_lease()
                                    assert(lease ~= nil, tostring(reason))
                                    error("boom inside turn")
                                end)
                                assert(not ok_err and tostring(err):find("boom inside turn", 1, true))
                                return { status = "started", turn_id = "err-0" }
                            end
                            seq = seq + 1
                            local turn_id = "custom-" .. seq
                            local done_tx, done_rx = mare.task.channel()
                            turns[turn_id] = { rx = done_rx, tx = done_tx }
                            sub:spawn(function()
                                local lease <close>, reason = sub.kernel:request_lease()
                                assert(lease ~= nil, "third-party active agent must acquire lease: " .. tostring(reason))
                                if mare._custom_lease_ready_tx then
                                    mare._custom_lease_ready_tx:send(true)
                                end
                                if mare._release_custom_turn_rx then
                                    mare._release_custom_turn_rx:recv()
                                end
                                sub.kernel:mark_call_committed()
                                mare._custom_committed = true
                                finish(turn_id, { text = "custom:" .. tostring(input) })
                            end)
                            return { status = "started", turn_id = turn_id }
                        end
                        svc.prompt = function(self, caller, input, opts)
                            local ok, res = pcall(handle_prompt, self, caller, input, opts)
                            if not ok then
                                local msg = type(res) == "table" and mare.json.encode(res) or tostring(res)
                                error("custom prompt error: " .. msg, 0)
                            end
                            return res
                        end

                        svc.wait = function(_, _, turn_id)
                            local entry = turns[turn_id]
                            if entry then
                                entry.rx:recv()
                                return entry.result
                            end
                            return { status = "completed", turn_id = tostring(turn_id or ""), started_at_ms = 0, text = "" }
                        end

                        require("mare_lib").service(sub, "agent", svc)
                    end)
                    agent_fiber:await()
                    mare._withdraw_custom_agent = function()
                        local d = agent_fiber:dispose()
                        if d and type(d) == "table" and d.await then
                            d:await()
                        end
                    end
                end,
            }
            "#,
        )
        .unwrap();

        std::fs::write(env.config_dir().join("init.lua"), "mare.o.plugins = { agent = { enabled = false } }\n").unwrap();
        let h = env
            .host()
            .user_config(false)
            .no_session()
            .build()
            .await;
        expose(&h);

        // 1. Complete a full turn via prompt() while reload waits cleanly until lease closes.
        exec(
            &h,
            "mare._custom_lease_ready_tx, mare._custom_lease_ready_rx = mare.task.channel(); mare._release_custom_turn_tx, mare._release_custom_turn_rx = mare.task.channel()",
        )
        .await;
        let started = h.prompt("hello", json!({ "wait": false })).await.unwrap();
        let turn_id = started["turn_id"].as_str().expect("missing turn_id in prompt reply").to_string();
        let ready = eval::<bool>(
            &h,
            "return mare.task.timeout(5000, function() return mare._custom_lease_ready_rx:recv() end)",
        )
        .await;
        assert!(ready, "timed out waiting for custom lease ready; problems={:?}", h.kernel.problems());
        let reload_err = h.reload_config().await.unwrap_err().to_string();
        assert!(
            reload_err.contains("model request is active"),
            "reload must wait/refuse while third-party agent holds lease: {reload_err}"
        );
        exec(&h, "mare._release_custom_turn_tx:send(true)").await;
        global(&h, "CUSTOM_TURN_ID", turn_id);
        let prompt_res_str: String = eval(&h, "return mare.json.encode(A:wait(CUSTOM_TURN_ID))").await;
        let prompt_res: serde_json::Value = serde_json::from_str(&prompt_res_str).unwrap();
        exec(&h, "mare._custom_lease_ready_tx = nil; mare._custom_lease_ready_rx = nil; mare._release_custom_turn_tx = nil; mare._release_custom_turn_rx = nil").await;
        assert_eq!(prompt_res["text"], "custom:hello", "{prompt_res}");
        assert!(eval::<bool>(&h, "return mare._custom_committed == true").await);

        // Error inside `<close>` scope also releases the lease immediately.
        h.prompt("error-unwind", json!({})).await.unwrap();
        h.reload_config().await.unwrap();

        // 2. Cancelling the turn cancellation token frees the lease immediately even while the
        // custom agent provider remains installed and the userdata remains live/reachable; both
        // single-plugin Kernel reload and host reload succeed cleanly.
        h.prompt("leak", json!({})).await.unwrap();
        assert!(h.reload_config().await.is_err(), "leaked lease still blocks reload while turn token is live");
        exec(&h, "mare._leaked_lease_token:cancel()").await;
        assert!(h.kernel.service("agent").is_some(), "custom agent provider remains installed");
        assert!(eval::<bool>(&h, "return mare._leaked_lease ~= nil").await, "userdata remains reachable");
        h.kernel.reload(Some("custom-agent")).await.expect("single-plugin reload must succeed once turn token cancels");
        expose(&h);
        h.reload_config().await.expect("config reload must also succeed after turn cancellation");

        // 3. Released handles retained by the provider must not grow or block future acquisitions/reloads.
        for _ in 0..3 {
            h.prompt("retain-released", json!({})).await.unwrap();
        }
        h.reload_config().await.expect("retained released handles must not block reload");

        // 4. No-argument `request_lease()` (no cancel arg) holds reload while the turn coroutine is
        // active, then automatically frees reload once the turn coroutine completes even while
        // the provider keeps `mare._leaked_provider_lease` reachable and stays installed!
        h.prompt("leak-no-arg-active", json!({})).await.unwrap();
        assert!(h.reload_config().await.is_err(), "active no-arg request lease must block reload during turn");
        exec(&h, "mare._no_arg_release_tx:send(true)").await;
        assert!(h.kernel.service("agent").is_some(), "custom agent provider remains installed");
        assert!(eval::<bool>(&h, "return mare._leaked_provider_lease ~= nil").await);
        wait_until(
            WAIT_SECS,
            "completed no-arg request coroutine releases reload_gate even with retained userdata",
            || async { h.reload_config().await.is_ok() },
        )
        .await;
        assert!(h.kernel.service("agent").is_some(), "custom agent provider remains installed");
        assert!(eval::<bool>(&h, "return mare._leaked_provider_lease ~= nil").await);

        // 5. No-arg `request_lease()` inside `prompt()` directly also releases as soon as the call finishes,
        // including under TrustSession revocation (`set_workspace_trust(false)`) with live provider & retained userdata.
        h.set_workspace_trust(true, false).await.unwrap();
        h.prompt("leak-unbounded-cancel", json!({})).await.unwrap();
        assert!(eval::<bool>(&h, "return mare._leaked_provider_lease ~= nil").await);
        tokio::time::timeout(
            std::time::Duration::from_secs(WAIT_SECS),
            h.set_workspace_trust(false, false),
        )
        .await
        .expect("trust revocation must not stall on completed no-arg request_lease coroutine")
        .unwrap();
        expose(&h);
        h.reload_config().await.expect("finished no-arg prompt call must release reload_gate");

        // 6. If a live suspended no-arg turn still holds a lease and the provider unloads,
        // ServiceEntry withdrawal also automatically frees reload_gate.
        h.prompt("leak-no-arg-active", json!({})).await.unwrap();
        assert!(h.reload_config().await.is_err(), "active no-arg turn blocks reload before withdrawal");
        exec(&h, "mare._withdraw_custom_agent()").await;
        assert!(h.kernel.service("agent").is_none(), "custom agent service must be withdrawn");
        h.reload_config().await.expect("withdrawing/unloading agent must release its active lease");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn post_stream_abort_strips_unexecuted_tool_calls_before_turn_aborted() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact"]).faux(json!([])).no_session().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local L = C.cordis:get("llm")
            L:provider("faux").stream = function(_req, sink)
                sink({ type = "text_delta", id = "t1", delta = "partial before abort" })
                A:cancel()
                return {
                    role = "assistant",
                    stop_reason = "aborted",
                    content = {
                        { type = "text", text = "partial before abort" },
                        { type = "tool_call", id = "c_unexec", name = "ls", arguments = { path = "." } },
                    },
                }
            end
            local started = A:prompt("trigger abort")
            A:wait(started.turn_id)
            local msgs = A:list_messages().data
            for _, m in ipairs(msgs) do
                if m.role == "assistant" then
                    for _, b in ipairs(m.content or {}) do
                        if b.type == "tool_call" then
                            return false
                        end
                    end
                end
            end
            return msgs[#msgs].turn_aborted == true
            "#,
        )
        .await;
        assert!(ok, "aborted stream must strip unexecuted tool_call blocks before <turn_aborted>");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reset_true_retry_emits_step_completed_before_item_reset() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({ "plugins": { "llm": { "retry_base_ms": 1, "retries": 1 } } }))
            .faux(json!([]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let order: Vec<String> = eval(
            &h,
            r#"
            local L = C.cordis:get("llm")
            local n = 0
            L:provider("faux").stream = function(_req, sink)
                n = n + 1
                if n == 1 then
                    sink({ type = "text_delta", id = "t1", delta = "partial-attempt-1" })
                    error({ message = "stream reset mid-flight", kind = "stream_closed", retryable = true })
                end
                sink({ type = "text_delta", id = "t2", delta = "clean-attempt-2" })
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "clean-attempt-2" } } }
            end
            local seq = {}
            C.cordis:on("step/completed", function(ev)
                if ev.will_retry then
                    seq[#seq + 1] = "step/completed:reset=" .. tostring(ev.reset)
                end
            end)
            C.cordis:on("item/completed", function(ev)
                if ev.status == "failed" then
                    seq[#seq + 1] = "item/completed:failed:" .. tostring(ev.item_id)
                end
            end)
            local started = A:prompt("retry me")
            local done = A:wait(started.turn_id)
            assert(done.text == "clean-attempt-2", tostring(done.text))
            return seq
            "#,
        )
        .await;
        assert!(order.len() >= 2, "{order:?}");
        assert_eq!(order[0], "step/completed:reset=true", "step/completed(reset=true) must emit before reset_id's item/completed(failed): {order:?}");
        assert!(order[1].starts_with("item/completed:failed:"), "{order:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn usage_reset_countdown_uses_epoch_seconds_and_export_redacts_0600() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage", "commands", "export"]).faux(json!([])).no_session().build().await;
        expose(&h);
        let export_path = env.cwd.join("exported.md");
        let export_str = export_path.to_string_lossy().replace('\\', "/");
        global(&h, "EXPORT_PATH", export_str);
        let (quota_text, exported): (String, String) = eval(
            &h,
            r#"
            local U = C.cordis:get("usage")
            local future_ms = math.floor(mare.task.now()) + 7200 * 1000
            C.cordis:emit("llm/rate-limits", {
                provider = "openai",
                limits = {
                    received_at_ms = math.floor(mare.task.now()),
                    primary = { window_minutes = 300, used_percent = 25, reset_at_ms = future_ms },
                },
            })
            local lines = U:quota_lines()
            local text = type(lines) == "table" and lines[1] and lines[1].text or ""
            local status = C.cordis:get("commands")
            A:set_messages({
                { role = "user", content = { { type = "text", text = "secret sk-ABCDEFGHIJKLMNOPQRSTUVWX AWS_SECRET_ACCESS_KEY=mysecretvalue123" } } },
            })
            status:run("/export " .. EXPORT_PATH)
            for _ = 1, 50 do
                if mare.fs.exists(EXPORT_PATH) then break end
                mare.task.sleep(10)
            end
            local raw = mare.fs.read(EXPORT_PATH)
            return text, raw
            "#,
        )
        .await;
        assert!(quota_text.contains("resets in"), "expected 'resets in' countdown, got: {quota_text}");
        assert!(!quota_text.contains("resets now"), "must not subtract ms from sec: {quota_text}");
        assert!(!exported.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWX"), "secret leaked in export: {exported}");
        assert!(!exported.contains("mysecretvalue123"), "env secret leaked in export: {exported}");
        assert!(exported.contains("[REDACTED]"), "{exported}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&export_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "exported file must have 0600 permissions, got {:o}", mode);
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn enqueued_content_array_message_stamps_epoch_ms() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        let ts: i64 = eval(
            &h,
            r#"
            local info = S:info()
            local res = A:enqueue({ { type = "text", text = "scheduled item" } }, { session_id = info.session_id, session_epoch = info.session_epoch })
            assert(res and (res.status == "started" or res.status == "queued"), "expected enqueue acceptance, got: " .. mare.json.encode(res))
            if res.status == "queued" then
                A:continue()
            end
            for _ = 1, 50 do
                local msgs = A:list_messages().data
                if #msgs >= 2 then
                    return math.floor(msgs[1].timestamp or 0)
                end
                mare.task.sleep(10)
            end
            return 0
            "#,
        )
        .await;
        assert!(ts > 1_000_000_000_000, "expected epoch ms timestamp (>1e12), got {ts}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn prompt_accepts_lua_only_fields_in_process() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().faux(json!([{ "text": "custom turn reply" }])).build().await;
        expose(&h);
        let (turn_id, text): (String, String) = eval(
            &h,
            r#"
            local started = A:prompt("hello", { turn_id = "k2-custom-turn-1" })
            assert(started.turn_id == "k2-custom-turn-1", "expected custom turn_id, got: " .. tostring(started.turn_id))
            local done = A:wait(started.turn_id)
            return done.turn_id, done.text
            "#,
        )
        .await;
        assert_eq!(turn_id, "k2-custom-turn-1");
        assert_eq!(text, "custom turn reply");
        h.shutdown().await;
    })
    .await;
}

/// Cancelling a run when acquiring the next turn lease must report `cancelled` / `interrupted`
/// rather than `failed` / `error`.
#[tokio::test]
async fn cancellation_during_lease_acquisition_reports_interrupted() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("hello.txt"), "hello\n").unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "hello.txt" } }] },
            { "text": "unreachable second step" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        let (status, reason, error_kind): (String, String, String) = eval(
            &h,
            r#"
            local kernel = C.cordis:get("kernel")
            local orig_req = kernel.request_lease
            local calls = 0
            kernel.request_lease = function(self, token)
                calls = calls + 1
                if calls == 2 then
                    token:cancel()
                    return nil, "request was cancelled"
                end
                return orig_req(self, token)
            end
            local started = A:prompt("read hello.txt")
            local done = A:wait(started.turn_id)
            kernel.request_lease = orig_req
            return tostring(done.status), tostring(done.termination_reason), tostring(done.error_kind)
            "#,
        )
        .await;
        assert_eq!(status, "cancelled", "cancelled lease acquisition must report cancelled status");
        assert_eq!(reason, "interrupted", "cancelled lease acquisition must report interrupted termination_reason");
        assert_eq!(error_kind, "aborted", "cancelled lease acquisition must report aborted error_kind");
        h.shutdown().await;
    })
    .await;
}

/// Unexpected Lua errors thrown during request assembly (`request/build`) or model streaming
/// (`llm:stream`) must still emit matching `step/completed` and `item/completed` events.
#[tokio::test]
async fn throwing_request_build_and_stream_close_step_and_item_events() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "unused" }])).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());

        // 1. Throwing inside request/build closes step/started with step/completed(failed).
        let (steps_started, steps_completed, last_step_status): (i64, i64, String) = eval(
            &h,
            r#"
            local ss, sc, last_st = 0, 0, ""
            local off_s = C.cordis:on("step/started", function() ss = ss + 1 end)
            local off_c = C.cordis:on("step/completed", function(ev) sc = sc + 1; last_st = tostring(ev.status) end)
            local off_b = C.cordis:on("request/build", function() error("boom-request-build") end)
            local started = A:prompt("trigger build error")
            local done = A:wait(started.turn_id)
            off_b(); off_c(); off_s()
            assert(done.status == "failed", "expected failed turn, got " .. tostring(done.status))
            return ss, sc, last_st
            "#,
        )
        .await;
        assert_eq!(steps_started, 1);
        assert_eq!(steps_completed, 1, "throwing request/build must emit step/completed");
        assert_eq!(last_step_status, "failed");

        // 2. Throwing inside llm:stream after emitting a delta closes both item/started and step/started.
        let (is_cnt, ic_cnt, item_st, ss2, sc2, step_st2): (i64, i64, String, i64, i64, String) = eval(
            &h,
            r#"
            local is_c, ic_c, ist = 0, 0, ""
            local ss, sc, sst = 0, 0, ""
            local off_is = C.cordis:on("item/started", function(ev)
                if ev.item and ev.item.type == "assistant_message" then is_c = is_c + 1 end
            end)
            local off_ic = C.cordis:on("item/completed", function(ev)
                if ev.item and ev.item.type == "assistant_message" then
                    ic_c = ic_c + 1
                    ist = tostring(ev.status)
                end
            end)
            local off_ss = C.cordis:on("step/started", function() ss = ss + 1 end)
            local off_sc = C.cordis:on("step/completed", function(ev) sc = sc + 1; sst = tostring(ev.status) end)
            local prov = L:provider("faux")
            local orig_stream = prov.stream
            prov.stream = function(_req, sink)
                sink({ type = "text_delta", delta = "partial " })
                sink({ type = "custom_flush_trigger" })
                error("boom-stream-crash")
            end
            local started = A:prompt("trigger stream error")
            local done = A:wait(started.turn_id)
            prov.stream = orig_stream
            off_sc(); off_ss(); off_ic(); off_is()
            assert(done.status == "failed", "expected failed turn, got " .. tostring(done.status))
            return is_c, ic_c, ist, ss, sc, sst
            "#,
        )
        .await;
        assert_eq!(is_cnt, 1, "expected assistant_message item/started before stream error");
        assert_eq!(ic_cnt, 1, "throwing llm:stream must emit item/completed for started stream item");
        assert_eq!(item_st, "failed");
        assert_eq!(ss2, 1);
        assert_eq!(sc2, 1, "throwing llm:stream must emit step/completed");
        assert_eq!(step_st2, "failed");
        h.shutdown().await;
    })
    .await;
}

/// Reloading plugins while a turn is active must refuse during an in-flight model request (holding
/// `request_lease`) and cancel cleanly when reloading mid-tool execution, closing active step/item
/// events and allowing subsequent turns on the reloaded agent.
#[tokio::test]
async fn reload_during_active_turn_cancels_cleanly_and_allows_subsequent_turn() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "text": "stream-phase-ok" },
            { "tool_calls": [{ "name": "slow_reload_probe", "arguments": {} }] },
            { "text": "after-reload-ok" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());

        // Phase A: while llm:stream is active (request_lease held), reload is refused and the turn finishes.
        let (stream_started_ok, stream_turn_id): (bool, String) = eval(
            &h,
            r#"
            local enter_tx, enter_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            mare._stream_release = release_tx
            local prov = L:provider("faux")
            local orig_stream = prov.stream
            local intercepted = false
            prov.stream = function(req, sink)
                if not intercepted then
                    intercepted = true
                    enter_tx:send(true)
                    release_rx:recv()
                end
                return orig_stream(req, sink)
            end
            local started = A:prompt("phase a")
            local entered = enter_rx:recv()
            return entered == true, started.turn_id
            "#,
        )
        .await;
        assert!(stream_started_ok, "expected stream hook to enter");
        let mid_stream_reload = h.kernel.reload(Some("agent")).await;
        assert!(
            mid_stream_reload.is_err(),
            "reloading agent while a model request holds the lease must be refused: {mid_stream_reload:?}"
        );
        let phase_a_status: String = eval(
            &h,
            &format!(
                r#"
                mare._stream_release:send(true)
                local done = A:wait("{stream_turn_id}")
                return tostring(done.status)
                "#
            ),
        )
        .await;
        assert_eq!(phase_a_status, "completed");

        // Phase B: during tool execution (after request_lease is released), reloading `agent`
        // cancels the in-flight tool and turn cleanly, closes step/item events, and leaves the
        // reloaded agent ready for a fresh prompt.
        let entered_tool: bool = eval(
            &h,
            r#"
            local enter_tx, enter_rx = mare.task.channel()
            mare._reload_probe = {
                tool_cancelled = false,
                tool_finished = false,
                step_started = 0,
                step_completed = 0,
                item_started = 0,
                item_completed = 0,
                turn_completed = nil,
            }
            C.cordis:on("step/started", function() mare._reload_probe.step_started = mare._reload_probe.step_started + 1 end)
            C.cordis:on("step/completed", function() mare._reload_probe.step_completed = mare._reload_probe.step_completed + 1 end)
            C.cordis:on("item/started", function(ev)
                if ev.item and ev.item.type == "tool_call" then
                    mare._reload_probe.item_started = mare._reload_probe.item_started + 1
                end
            end)
            C.cordis:on("item/completed", function(ev)
                if ev.item and ev.item.type == "tool_call" then
                    mare._reload_probe.item_completed = mare._reload_probe.item_completed + 1
                end
            end)
            C.cordis:on("turn/completed", function(ev)
                mare._reload_probe.turn_completed = {
                    status = ev.status,
                    termination_reason = ev.termination_reason,
                    cancellation_cause = ev.cancellation_cause,
                }
            end)
            local tools = C.cordis:get("tools")
            tools:register("slow_reload_probe", {
                description = "blocks until cancelled",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function(_args, ctx)
                    enter_tx:send(true)
                    for _ = 1, 200 do
                        if ctx.cancel and ctx.cancel:is_cancelled() then
                            mare._reload_probe.tool_cancelled = true
                            break
                        end
                        mare.task.sleep(10)
                    end
                    mare._reload_probe.tool_finished = true
                    return { content = "stopped" }
                end,
            })
            A:prompt("phase b")
            return enter_rx:recv() == true
            "#,
        )
        .await;
        assert!(entered_tool, "expected slow_reload_probe tool to enter");

        h.kernel.reload(Some("agent")).await.unwrap();

        let (probe_json, next_status, next_text): (String, String, String) = eval(
            &h,
            r#"
            for _ = 1, 100 do
                if mare._reload_probe.tool_finished and mare._reload_probe.turn_completed then
                    break
                end
                mare.task.sleep(10)
            end
            local snapshot = mare.json.encode(mare._reload_probe)
            local agent2 = C.cordis:get("agent")
            local started = agent2:prompt("phase c")
            local done = agent2:wait(started.turn_id)
            return snapshot, tostring(done.status), tostring(done.text)
            "#,
        )
        .await;
        let probe: serde_json::Value = serde_json::from_str(&probe_json).unwrap();
        assert_eq!(probe["tool_cancelled"], true, "in-flight tool must observe cancellation on reload: {probe}");
        assert_eq!(probe["tool_finished"], true, "in-flight tool must unwind cleanly: {probe}");
        assert!(probe["turn_completed"].is_object(), "cancelled turn must emit turn/completed: {probe}");
        assert_eq!(probe["turn_completed"]["status"], "cancelled", "{probe}");
        assert_eq!(probe["turn_completed"]["termination_reason"], "interrupted", "{probe}");
        assert_eq!(probe["turn_completed"]["cancellation_cause"], "disposed", "{probe}");
        assert_eq!(
            probe["step_started"], probe["step_completed"],
            "active step/started must be closed by step/completed on mid-turn reload: {probe}"
        );
        assert_eq!(
            probe["item_started"], probe["item_completed"],
            "active tool_call item/started must be closed by item/completed on mid-turn reload: {probe}"
        );
        assert_eq!(next_status, "completed", "reloaded agent must accept a fresh prompt cleanly");
        assert_eq!(next_text, "after-reload-ok");
        h.shutdown().await;
    })
    .await;
}

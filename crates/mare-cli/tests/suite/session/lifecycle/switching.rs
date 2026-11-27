//! Session switching, reloads, turn/side-request/compaction cancellation and drain.

use super::*;

#[tokio::test]
async fn new_during_stream_keeps_the_aborted_turn_in_the_old_session() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "text": format!("OLD-STREAM-{}", "old ".repeat(1500)), "delay_ms": 10 },
            { "text": "NEW-REPLY" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"STREAM_STARTED = false
            C.cordis:on("item/delta", function(e) if e.kind == "text" then STREAM_STARTED = true end end)"#,
        )
        .await;
        let old = h.clone();
        let run = tokio::task::spawn_local(async move { old.prompt("OLD-REQUEST", json!({})).await.unwrap() });
        wait_until(10, "old stream to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return STREAM_STARTED").await }
        })
        .await;
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        exec(&h, r#"CM:run("/new")"#).await;
        run.await.unwrap();
        global(&h, "OLD_ID", old_id.clone());
        wait_until(10, "new session after old turn ends", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return S:info().session_id ~= OLD_ID and not A:get().busy").await }
        })
        .await;
        let res = h.prompt("NEW-REQUEST", json!({})).await.unwrap();
        assert_eq!(res["text"], "NEW-REPLY", "{res}");
        let new_path: String = eval(&h, "return S:info().path").await;
        assert_ne!(old_path, new_path);
        h.shutdown().await;
        let old_log = std::fs::read_to_string(&old_path).unwrap();
        let new_log = std::fs::read_to_string(&new_path).unwrap();
        assert!(new_log.contains("NEW-REQUEST") && new_log.contains("NEW-REPLY"), "{new_log}");
        assert!(
            !new_log.contains("OLD-REQUEST") && !new_log.contains("OLD-STREAM") && !new_log.contains("turn_aborted"),
            "old turn leaked into new session: {new_log}"
        );
        assert!(old_log.contains("OLD-REQUEST") && old_log.contains("turn_aborted"), "{old_log}");
        let completed_old_runs = old_log
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| entry["kind"] == "agent/run-end")
            .count();
        assert_eq!(completed_old_runs, 0, "a cancelled run cannot become a fork boundary: {old_log}");
    })
    .await
}

/// Every public session switch must refuse an active turn, not only the /new command.
#[tokio::test]
async fn direct_session_switches_refuse_a_running_turn_without_cross_session_writes() {
    local(async {
        for method in ["new", "load", "fork"] {
            let env = TestEnv::new().await;
            let h = env
                .host()
                .builtins(&["compact", "usage"])
                .faux(json!([{ "text": format!("OLD-{}", "slow ".repeat(500)), "delay_ms": 10 }]))
                .build()
                .await;
            expose(&h);
            exec(
                &h,
                r#"STREAM_STARTED = false
                C.cordis:on("item/delta", function(e) if e.kind == "text" then STREAM_STARTED = true end end)"#,
            )
            .await;
            let running = h.clone();
            let turn = tokio::task::spawn_local(async move { running.prompt("OLD-REQUEST", json!({})).await.unwrap() });
            wait_until(5, "the old stream to start", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return STREAM_STARTED").await }
            })
            .await;
            let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
            global(&h, "SWITCH", method);
            global(&h, "OLD_PATH", old_path.clone());
            let (ok, err, after): (bool, String, String) = eval(
                &h,
                r#"
                local ok, err = pcall(function()
                    if SWITCH == "new" then return S:create() end
                    if SWITCH == "load" then return S:load({ path = OLD_PATH }) end
                    return S:fork({ session_id = S:info().session_id })
                end)
                return ok, tostring(err), S:info().session_id"#,
            )
            .await;
            turn.await.unwrap();
            assert!(!ok && err.contains("turn"), "{method}: {err}");
            assert_eq!(after, old_id, "{method} switched the active session");
            h.shutdown().await;
            let log = std::fs::read_to_string(old_path).unwrap();
            assert!(log.contains("OLD-REQUEST") && log.contains("OLD-"), "{method}: {log}");
        }
    })
    .await
}

/// A turn is not settled until its turn/completed listeners finish, even if agent.busy is false.
#[tokio::test]
async fn session_switch_waits_for_agent_end_listeners_to_finish() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"ENDING = false
            C.cordis:on("turn/completed", function() ENDING = true; mare.task.sleep(300) end)"#,
        )
        .await;
        let running = h.clone();
        let turn = tokio::task::spawn_local(async move { running.prompt("old turn", json!({})).await.unwrap() });
        wait_until(5, "the end listener to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return ENDING").await }
        })
        .await;
        let (busy, switched, error): (bool, bool, String) = eval(
            &h,
            r#"
            local busy = A:get().busy
            local ok, err = pcall(S.create, S)
            return busy, ok, tostring(err)"#,
        )
        .await;
        turn.await.unwrap();
        assert!(!busy, "the regression requires the interval after busy becomes false");
        assert!(!switched && error.contains("turn"), "the previous turn is still ending: {error}");
        h.shutdown().await;
    })
    .await
}

/// Reloading the current session must not close its writer while a new turn can still start.
#[tokio::test]
async fn reloading_current_session_rejects_a_concurrent_turn_and_allows_retry() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "seed" }, { "text": "concurrent reply" }]))
            .build()
            .await;
        expose(&h);
        h.prompt("seed prompt", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        exec(
            &h,
            r#"LOADING = false
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...)
                LOADING = true; mare.task.sleep(300); return original(...)
            end"#,
        )
        .await;
        global(&h, "LOAD_PATH", path.clone());
        let load_host = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(
                &load_host,
                r#"local ok, err = pcall(S.load, S, { path = LOAD_PATH }) return ok, tostring(err)"#,
            )
            .await
        });
        wait_until(5, "session reload to pause", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return LOADING").await }
        })
        .await;
        let err = h.prompt("concurrent prompt", json!({})).await.unwrap_err();
        let (loaded, reason) = loading.await.unwrap();
        assert!(loaded, "reload should finish after rejecting the concurrent turn: {reason}");
        assert!(err.to_string().contains("switch") && err.to_string().contains("retry"), "{err}");
        let retried = h.prompt("concurrent prompt", json!({})).await.unwrap();
        assert_eq!(retried["text"], "concurrent reply", "retry must retain the session writer: {retried}");
        h.shutdown().await;
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("concurrent prompt") && text.contains("concurrent reply"), "{text}");
    })
    .await
}

#[tokio::test]
async fn new_refuses_to_switch_if_the_aborted_turn_never_finishes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "old reply" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("old request", json!({})).await.unwrap();
        let old_id: String = eval(&h, "return S:info().session_id").await;
        exec(
            &h,
            r#"
            BUSY, ABORTS = true, 0
            A.activity = function() return { busy = BUSY, queued = 0 } end
            A.cancel = function() ABORTS = ABORTS + 1; BUSY = false; return { removed = mare.json.array() } end
            CM:run("/new")
            CM:run("/new")"#,
        )
        .await;
        wait_until(5, "the cancellation timeout warning", || {
            let h = h.clone();
            async move { notices(&h).await.contains("previous turn did not stop in time") }
        })
        .await;
        let (id, aborts, warnings): (String, i64, String) =
            eval(&h, "return S:info().session_id, ABORTS, table.concat(N, '\\n')").await;
        assert_eq!(id, old_id, "a missing turn/completed must never switch sessions");
        assert_eq!(aborts, 1, "a repeated /new must not launch another cancellation");
        assert!(warnings.contains("already waiting") && warnings.contains("did not stop in time"), "{warnings}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn new_does_not_reset_a_different_session_while_waiting() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"
            BUSY = true
            A.activity = function() return { busy = BUSY, queued = 0 } end
            A.cancel = function() BUSY = false; return { removed = mare.json.array() } end
            CM:run("/new")
            S:create()
            CHANGED_ID = S:info().session_id
            C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" })"#,
        )
        .await;
        wait_until(5, "the session-change warning", || {
            let h = h.clone();
            async move { notices(&h).await.contains("active session changed") }
        })
        .await;
        assert!(eval::<bool>(&h, "return S:info().session_id == CHANGED_ID").await);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn new_cancels_and_drains_a_running_side_request_before_switching() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "initial reply" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        h.prompt("old session", json!({})).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        exec(&h, r#"
            SIDE_STARTED, SIDE_DONE, SIDE_CANCELLED = false, false, false
            L:register_provider({
                provider = "sideprobe", is_configured = function() return true end,
                models = { { model_id = "m", name = "Side Probe", context_window = 100000, max_output = 4096 } },
                stream = function(req)
                    SIDE_STARTED = true
                    mare.task.sleep(150)
                    SIDE_CANCELLED = req.cancel and req.cancel:is_cancelled() or false
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "side answer" } },
                        usage = { input = 50, output = 5, cache_read = 0, cache_write = 0 } }
                end,
            })
            mare.task.spawn(function()
                L:complete({ model = L:resolve("sideprobe/m"), purpose = "btw",
                    messages = { { role = "user", content = { { type = "text", text = "side question" } } } }, tools = {} })
                SIDE_DONE = true
            end)"#).await;
        wait_until(5, "side request starts", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SIDE_STARTED").await }
        }).await;
        exec(&h, r#"CM:run("/new")"#).await;
        let id_during: String = eval(&h, "return S:info().session_id").await;
        assert_eq!(id_during, old_id, "the old session must remain until the side request is drained");
        global(&h, "OLD_ID", old_id);
        wait_until(5, "side request drains and session changes", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SIDE_DONE and S:info().session_id ~= OLD_ID").await }
        }).await;
        let (cancelled, side): (bool, i64) = eval(&h, "return SIDE_CANCELLED, U:totals().side_requests").await;
        assert!(cancelled, "the old session's side request must receive cancellation");
        assert_eq!(side, 0, "late old-session usage must not enter new session totals");
        h.prompt("new session", json!({})).await.unwrap();
        let new_path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let old_log = std::fs::read_to_string(old_path).unwrap();
        let new_log = std::fs::read_to_string(new_path).unwrap();
        assert!(old_log.contains("usage/side-v1") && old_log.contains("sideprobe"), "{old_log}");
        assert!(!new_log.contains("usage/side-v1") && !new_log.contains("sideprobe"), "{new_log}");
    }).await
}

#[tokio::test]
async fn direct_session_switches_drain_side_requests_before_closing_the_old_log() {
    local(async {
        for action in ["new", "load", "fork"] {
            let env = TestEnv::new().await;
            let target = env.write_session("target.jsonl", &[]);
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "initial reply" }])).build().await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "U", h.service_table("usage").unwrap());
            global(&h, "TARGET", target.to_string_lossy().to_string());
            h.prompt("old session", json!({})).await.unwrap();
            let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
            exec(&h, r#"
                SIDE_STARTED, SIDE_DONE, SIDE_CANCELLED = false, false, false
                L:register_provider({
                    provider = "sideprobe", is_configured = function() return true end,
                    models = { { model_id = "m", name = "Side Probe", context_window = 100000, max_output = 4096 } },
                    stream = function(req)
                        SIDE_STARTED = true
                        mare.task.sleep(150)
                        SIDE_CANCELLED = req.cancel and req.cancel:is_cancelled() or false
                        return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "side" } },
                            usage = { input = 51, output = 5, cache_read = 0, cache_write = 0 } }
                    end,
                })
                mare.task.spawn(function()
                    L:complete({ model = L:resolve("sideprobe/m"), purpose = "btw",
                        messages = { { role = "user", content = { { type = "text", text = "side question" } } } }, tools = {} })
                    SIDE_DONE = true
                end)"#).await;
            wait_until(5, "side request starts", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return SIDE_STARTED").await }
            }).await;
            let command = match action {
                "new" => "S:create()",
                "load" => "S:load({ path = TARGET })",
                "fork" => "S:fork({ session_id = S:info().session_id })",
                _ => unreachable!(),
            };
            exec(&h, command).await;
            let (id, done, cancelled, side): (String, bool, bool, i64) =
                eval(&h, "return S:info().session_id, SIDE_DONE, SIDE_CANCELLED, U:totals().side_requests").await;
            assert_ne!(id, old_id, "{action}");
            assert!(done && cancelled, "{action}: side request must be cancelled and drained");
            assert_eq!(side, if action == "fork" { 1 } else { 0 }, "{action}");
            h.shutdown().await;
            let old_log = std::fs::read_to_string(old_path).unwrap();
            assert!(old_log.contains("usage/side-v1") && old_log.contains("sideprobe"), "{action}: {old_log}");
        }
    }).await
}

#[tokio::test]
async fn peer_session_create_can_cancel_in_process_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let old_id: String = eval(&h, "return S:info().session_id").await;
        exec(
            &h,
            r#"
            BUSY_CALLS, CANCEL_CALLS, COMPACT_RUNNING = 0, 0, true
            K.busy = function()
                BUSY_CALLS = BUSY_CALLS + 1
                return COMPACT_RUNNING
            end
            K.cancel = function()
                CANCEL_CALLS = CANCEL_CALLS + 1
                COMPACT_RUNNING = false
            end
        "#,
        )
        .await;

        let caller = h.kernel.open_owner("compact-peer-test", "root").unwrap();
        h.kernel.set_caller_access(
            caller,
            Some(mare_kernel::CallerAccess {
                is_peer: true,
                allows: std::rc::Rc::new(|service, method, access| {
                    service == "session" && method == "create" && access == "peer"
                }),
                allows_session_path: std::rc::Rc::new(|| false),
            }),
        );
        let permitted = (
            h.kernel.permitted_for_caller(Some(caller), "compact", "busy"),
            h.kernel.permitted_for_caller(Some(caller), "compact", "cancel"),
        );
        assert_eq!(permitted, (true, true), "peer work must reach in-process compact methods");
        let result = h
            .kernel
            .call_service_export(caller, None, "session", "create", json!({}), Default::default())
            .await
            .expect("peer-granted session.create must drain compact through its in-process methods");
        assert_ne!(result["session_id"].as_str().unwrap(), old_id);
        let (busy_calls, cancel_calls, running): (i64, i64, bool) =
            eval(&h, "return BUSY_CALLS, CANCEL_CALLS, COMPACT_RUNNING").await;
        assert!(busy_calls > 0, "session switch must inspect compaction");
        assert_eq!(cancel_calls, 1, "session switch must cancel the running compaction");
        assert!(!running);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn new_cancels_running_compaction_before_switching() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "text": format!("first answer {}", "work ".repeat(1000)) },
            { "text": format!("second answer {}", "work ".repeat(1000)) },
            { "text": "slow summary ".repeat(1000), "delay_ms": 10 }
        ]);
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("first question", json!({})).await.unwrap();
        h.prompt("second question", json!({})).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "OLD_ID", old_id.clone());
        exec(&h, r#"CM:run("/compact")"#).await;
        wait_until(5, "compaction side request", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return K:busy() and L:side_requests_pending(OLD_ID)").await }
        })
        .await;
        exec(&h, r#"CM:run("/new")"#).await;
        wait_until(5, "compaction stops before the session switch", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not K:busy() and S:info().session_id ~= OLD_ID").await }
        })
        .await;
        let old_log = std::fs::read_to_string(old_path).unwrap();
        assert!(!old_log.contains("\"type\":\"compaction\""), "cancelled compaction must not commit: {old_log}");
        assert_eq!(eval::<i64>(&h, "return #A:list_messages().data").await, 0, "new session starts empty");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn queued_side_commands_do_not_start_in_a_replaced_session() {
    local(async {
        for command in ["/btw question", "/compact"] {
            let env = TestEnv::new().await;
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "CM", h.service_table("commands").unwrap());
            global(&h, "K", h.service_table("compact").unwrap());
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "COMMAND", command.to_string());
            exec(
                &h,
                r#"
                CALLS = 0
                K.run = function() CALLS = CALLS + 1; return { status = "compacted", tokens_before = 0, tokens_after = 0 } end
                L.complete = function()
                    CALLS = CALLS + 1
                    return { stop_reason = "stop", content = { { type = "text", text = "answer" } } }
                end
                CM:run(COMMAND)
                CM:run("/new")"#,
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(eval::<i64>(&h, "return CALLS").await, 0, "{command} must not start in the new session");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn new_refuses_to_switch_when_a_side_request_ignores_cancellation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "initial reply" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        h.prompt("old session", json!({})).await.unwrap();
        let old_id: String = eval(&h, "return S:info().session_id").await;
        exec(&h, r#"
            SIDE_STARTED, SIDE_DONE = false, false
            L:register_provider({
                provider = "slow-side", is_configured = function() return true end,
                models = { { model_id = "m", name = "Slow Side", context_window = 100000, max_output = 4096 } },
                stream = function()
                    SIDE_STARTED = true
                    mare.task.sleep(1300)
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "side" } },
                        usage = { input = 51, output = 5, cache_read = 0, cache_write = 0 } }
                end,
            })
            mare.task.spawn(function()
                L:complete({ model = L:resolve("slow-side/m"), purpose = "btw",
                    messages = { { role = "user", content = { { type = "text", text = "side question" } } } }, tools = {} })
                SIDE_DONE = true
            end)"#).await;
        wait_until(5, "side request starts", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SIDE_STARTED").await }
        }).await;
        exec(&h, r#"CM:run("/new")"#).await;
        wait_until(5, "side cancellation timeout", || {
            let h = h.clone();
            async move { notices(&h).await.contains("side requests did not stop in time") }
        }).await;
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, old_id);
        wait_until(5, "the late side result", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SIDE_DONE").await }
        }).await;
        assert_eq!(eval::<i64>(&h, "return U:totals().side_requests").await, 1);
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, old_id);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn reloading_current_session_accepts_an_equivalent_path_spelling() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "answer" }])).build().await;
        expose(&h);
        h.prompt("question", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        let path = std::path::Path::new(&path);
        let alias = format!("{}/./{}", path.parent().unwrap().display(), path.file_name().unwrap().to_string_lossy());
        global(&h, "ALIAS_PATH", alias);
        let (ok, error, count): (bool, String, i64) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = ALIAS_PATH })
            return ok, tostring(err), #A:list_messages().data
        "#,
        )
        .await;
        assert!(ok, "equivalent path must reuse the current writer: {error}");
        assert_eq!(count, 2, "the loaded conversation remains intact");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_new_during_side_request_drain_releases_switch_lease() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "SEED" }, { "text": "NEXT" }]))
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("BEFORE", json!({})).await.unwrap();
        let (id, path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        let (finished, switching, after): (bool, bool, String) = eval(
            &h,
            r#"
            local pending, cancel = L.side_requests_pending, L.cancel_side_requests
            L.side_requests_pending = function() return true end
            L.cancel_side_requests = function() end
            local finished = mare.task.timeout(40, function() S:create() end)
            L.side_requests_pending, L.cancel_side_requests = pending, cancel
            return finished, S:switching(), S:info().session_id
        "#,
        )
        .await;
        assert!(!finished && !switching);
        assert_eq!(after, id);
        assert_eq!(h.prompt("AFTER", json!({})).await.unwrap()["text"], "NEXT");
        h.shutdown().await;
        assert!(std::fs::read_to_string(path).unwrap().contains("AFTER"));
    })
    .await
}

#[tokio::test]
async fn session_ended_emits_once_per_retired_root_session_across_lifecycle() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("ended_observer.lua"),
            r#"
            return {
                name = "ended-observer",
                apply = function(ctx)
                    mare.g.ended_observer_loaded = true
                    mare.g.ended_log = mare.g.ended_log or {}
                    ctx:on("session/closed", function(snap)
                        local closed_on_disk = false
                        local reopened = false
                        if snap.path then
                            local raw = mare.fs.read(snap.path)
                            closed_on_disk = type(raw) == "string" and raw:find('"state":"closed"', 1, true) ~= nil
                            local ok_open, handle = pcall(mare.store.open, snap.path)
                            if ok_open and handle then
                                reopened = true
                                pcall(handle.close, handle)
                            end
                        end
                        mare.g.ended_seen = (mare.g.ended_seen or 0) + 1
                        local log = mare.g.ended_log or {}
                        log[#log + 1] = {
                            id = snap.session_id,
                            cwd = snap.cwd,
                            path = snap.path,
                            reason = snap.reason,
                            closed_on_disk = closed_on_disk,
                            reopened = reopened,
                        }
                        mare.g.ended_log = log
                    end)
                end,
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("ended_session_dep.lua"),
            r#"
            return {
                name = "ended-session-dep",
                inject = { "session" },
                apply = function(ctx)
                    mare.g.dep_ended_log = mare.g.dep_ended_log or {}
                    ctx:on("session/closed", function(snap)
                        local log = mare.g.dep_ended_log or {}
                        log[#log + 1] = { id = snap.session_id, reason = snap.reason }
                        mare.g.dep_ended_log = log
                    end)
                end,
            }
            "#,
        )
        .unwrap();

        let h = env
            .host()
            .user_config(true)
            .user_plugins()
            .faux(json!([{ "text": "one" }, { "text": "two" }, { "text": "three" }, { "text": "four" }]))
            .build()
            .await;
        expose(&h);

        // 1. Switch via session:create() (`reason == "new"`).
        h.prompt("msg-1", json!({})).await.unwrap();
        let s1: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();
        exec(&h, "S:create()").await;

        // 2. Switch via session:load() (`reason == "switch"`).
        h.prompt("msg-2", json!({})).await.unwrap();
        let s2: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();
        global(&h, "S1_PATH", s1["path"].as_str().unwrap().to_string());
        exec(&h, "S:load({ path = S1_PATH })").await;

        // 3. Switch via session:fork() (`reason == "fork"`).
        h.prompt("msg-3", json!({})).await.unwrap();
        exec(&h, "S:fork({ session_id = S:info().session_id })").await;
        let _s3: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();

        // 4. Config-only reload of `session` (`innerFiber:update`, unchanged source fingerprint)
        // must still retire session before tearing down `session`-dependent `session/ended` observers.
        h.prompt("msg-3b", json!({})).await.unwrap();
        let s3b: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.o.plugins = { session = { branch_summary = false } }\n")
            .unwrap();
        h.reload_config().await.unwrap();
        expose(&h);

        // 5. Explicit single-plugin reload `session` (`reason == "reload"`).
        h.prompt("msg-3c", json!({})).await.unwrap();
        let s3: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();
        h.kernel.reload(Some("session")).await.unwrap();
        expose(&h);

        // 6. Re-expose/create a final active root session and shut down (`reason == "shutdown"`, once).
        h.prompt("msg-4", json!({})).await.unwrap();
        let s4: serde_json::Value = eval::<String>(&h, "return mare.json.encode(S:info())").await.parse().unwrap();
        h.shutdown().await;

        let (loaded, seen): (bool, i64) =
            eval(&h, "return mare.g.ended_observer_loaded == true, mare.g.ended_seen or 0").await;
        let log_val: serde_json::Value =
            eval::<String>(&h, "return mare.json.encode(mare.json.array(mare.g.ended_log or {}))")
                .await
                .parse()
                .unwrap();
        let problems: Vec<String> = h.kernel.problems();
        assert!(problems.is_empty(), "kernel problems: {problems:?}; loaded={loaded} seen={seen} ended_log={log_val}");
        assert!(loaded, "ended_observer plugin did not load! seen={seen}");
        assert_eq!(seen, 6, "seen={seen} ended_log={log_val}");
        let log = log_val.as_array().unwrap();
        assert_eq!(log.len(), 6, "expected exactly 6 session/ended events, got: {log:?}");

        assert_eq!(log[0]["id"], s1["session_id"]);
        assert_eq!(log[0]["path"], s1["path"]);
        assert_eq!(log[0]["reason"], "new");
        assert_eq!(log[0]["closed_on_disk"], true, "writer must be closed inside session/ended callback");
        assert_eq!(log[0]["reopened"], true, "writer lock must be released inside session/ended callback");

        assert_eq!(log[1]["id"], s2["session_id"]);
        assert_eq!(log[1]["path"], s2["path"]);
        assert_eq!(log[1]["reason"], "switch");
        assert_eq!(log[1]["closed_on_disk"], true);
        assert_eq!(log[1]["reopened"], true);

        assert_eq!(log[2]["id"], s1["session_id"]);
        assert_eq!(log[2]["reason"], "fork");
        assert_eq!(log[2]["closed_on_disk"], true);

        assert_eq!(log[3]["id"], s3b["session_id"]);
        assert_eq!(log[3]["reason"], "reload");
        assert_eq!(log[3]["closed_on_disk"], true);

        assert_eq!(log[4]["id"], s3["session_id"]);
        assert_eq!(log[4]["reason"], "reload");
        assert_eq!(log[4]["closed_on_disk"], true);
        assert_eq!(log[4]["reopened"], true);

        assert_eq!(log[5]["id"], s4["session_id"]);
        assert_eq!(log[5]["path"], s4["path"]);
        assert_eq!(log[5]["reason"], "shutdown");
        assert_eq!(log[5]["closed_on_disk"], true);

        let dep_log_val: serde_json::Value =
            eval::<String>(&h, "return mare.json.encode(mare.json.array(mare.g.dep_ended_log or {}))")
                .await
                .parse()
                .unwrap();
        let dep_log = dep_log_val.as_array().unwrap();
        assert_eq!(
            dep_log.len(),
            6,
            "session-injecting observer must also receive all 6 session/ended events (including config-only reload, reload & shutdown): {dep_log:?}"
        );
        assert_eq!(dep_log[3]["reason"], "reload");
        assert_eq!(dep_log[4]["reason"], "reload");
        assert_eq!(dep_log[5]["reason"], "shutdown");
    })
    .await
}

#[tokio::test]
async fn pre_retirement_queued_tool_write_completes_before_session_ended() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "seed" }])).build().await;
        expose(&h);
        h.prompt("seed", json!({})).await.unwrap();
        let report_json: String = eval(
            &h,
            r#"
            local info = S:info()
            local path = info.path
            local ended_seen = false
            local write_done = false
            local order_ok = false
            local closed_before_release = false
            C.cordis:on("session/closed", function()
                ended_seen = true
                local raw = mare.fs.read(path)
                order_ok = write_done and type(raw) == "string" and raw:find('"kind":"pre-retire-probe"', 1, true) ~= nil
            end)
            assert(type(S:record_tool_state({ tool_call_id = "drain-probe", name = "probe", state = "started", durable = true })) == "string")
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local w_tx, w_rx = mare.task.channel()
            local r_tx, r_rx = mare.task.channel()
            -- Hold the accepted tool-context write inside the background writer worker so
            -- kernel:reload("session") deterministically enters writer_state = "draining" and
            -- blocks in drain_accepted_writes() before the write is released to finish.
            local orig_project = mare.projection.project
            mare.projection.project = function(val, opts)
                if type(val) == "table" and val.kind == "pre-retire-probe" then
                    mare.projection.project = orig_project
                    entered_tx:send(true)
                    release_rx:recv()
                end
                return orig_project(val, opts)
            end
            local off_before
            off_before = C.cordis:on("kernel/before-reload", function()
                if off_before then
                    off_before()
                    off_before = nil
                end
                -- Runs right before session's kernel/before-reload listener sets writer_state = "draining"
                -- and suspends on drain_accepted_writes(); the spawned task runs once reload suspends there.
                mare.task.spawn(function()
                    closed_before_release = ended_seen
                    release_tx:send(true)
                end)
            end, { prepend = true })
            mare.task.spawn(function()
                mare.exec.run({ tool = "probe", tool_call_id = "drain-probe" }, function()
                    local ok, err = pcall(S.add_record, S, "pre-retire-probe", { v = 42 }, { sync = true, persist = true })
                    write_done = ok
                    w_tx:send({ ok = ok, err = tostring(err or "") })
                end)
            end)
            assert(entered_rx:recv() == true, "tool-context write must be accepted and enter writer worker before reload")
            mare.task.spawn(function()
                local ok, err = pcall(function() C.cordis:get("kernel"):reload("session") end)
                r_tx:send({ ok = ok, err = tostring(err or "") })
            end)
            local wr = w_rx:recv()
            local rr = r_rx:recv()
            return mare.json.encode({
                wr = wr.ok == true,
                w_err = wr.err,
                rr = rr.ok == true,
                r_err = rr.err,
                ended_seen = ended_seen,
                write_done = write_done,
                order_ok = order_ok,
                closed_before_release = closed_before_release,
            })
        "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&report_json).unwrap();
        assert!(
            report["wr"] == true
                && report["rr"] == true
                && report["ended_seen"] == true
                && report["order_ok"] == true
                && report["closed_before_release"] == false,
            "accepted tool-context write before retirement must finish before session/ended: {report}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_ended_not_emitted_on_failed_close_or_child_mode() {
    #[cfg(debug_assertions)]
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "seed" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            mare._ended_count = 0
            C.cordis:on("session/closed", function()
                mare._ended_count = mare._ended_count + 1
            end)
        "#,
        )
        .await;
        h.prompt("hello", json!({})).await.unwrap();
        let fail = h
            .lua()
            .create_function(|lua, ()| {
                mare_binding::exec_gate::fail_session_journal_ops(lua, 0, false, true);
                Ok(())
            })
            .unwrap();
        global(&h, "FAIL_CLOSE", fail);
        let ok: bool = eval(
            &h,
            r#"
            local check = A.check_session_revision
            A.check_session_revision = function(...) FAIL_CLOSE(); return check(...) end
            local ok = pcall(S.fork, S, { session_id = S:info().session_id })
            A.check_session_revision = check
            return ok
        "#,
        )
        .await;
        assert!(!ok, "fork must fail when closing old session fails");
        assert_eq!(eval::<i64>(&h, "return mare._ended_count").await, 0, "failed close must not emit session/ended");
        h.shutdown().await;
        assert_eq!(
            eval::<i64>(&h, "return mare._ended_count").await,
            0,
            "prior close failure must not emit session/ended on subsequent teardown"
        );
    })
    .await;

    // 2. Loading the already-active path reuses the open writer without emitting session/ended,
    // and writes issued after shutdown retirement are refused without reopening the journal.
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "active" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            mare._ended_count = 0
            mare._late_write_result = nil
            mare._late_load_result = nil
            C.cordis:on("session/closed", function(snap)
                mare._ended_count = mare._ended_count + 1
                local call_ok, res_ok = pcall(S.record, S, "late/after-ended", { late = true }, { persist = true, sync = true })
                mare._late_write_result = (call_ok and res_ok == true)
                local load_ok = pcall(S.load, S, { path = snap and snap.path or ACTIVE_PATH })
                mare._late_load_result = load_ok
            end)
        "#,
        )
        .await;
        h.prompt("seed", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        global(&h, "ACTIVE_PATH", path.clone());
        exec(&h, "S:load({ path = ACTIVE_PATH })").await;
        assert_eq!(
            eval::<i64>(&h, "return mare._ended_count").await,
            0,
            "reusing already-active writer on self-load must not emit session/ended"
        );
        h.shutdown().await;
        assert_eq!(eval::<i64>(&h, "return mare._ended_count").await, 1);
        assert!(
            eval::<bool>(&h, "return mare._late_write_result == false").await,
            "write requested after writer retirement must be refused"
        );
        assert!(
            eval::<bool>(&h, "return mare._late_load_result == false").await,
            "session:load inside session/ended must not reopen or acquire a writer after retirement"
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("\"type\":\"session\"").count(), 1, "journal must not reopen: {raw}");
        assert!(!raw.contains("late/after-ended"), "late retired write must not persist: {raw}");
    })
    .await;

    // 3. Real child host mode (`mare.mode == "child"`) never emits session/ended across reload or shutdown.
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("child_ended_probe.lua"),
            r#"
            return {
                name = "child-ended-probe",
                inject = { "session" },
                apply = function(ctx)
                    if mare.mode ~= "child" then
                        return
                    end
                    ctx:on("session/closed", function(snap)
                        mare.fs.write(mare.cwd .. "/child-ended.txt", tostring(snap.reason))
                    end)
                    ctx:on("app/shutdown", function()
                        mare.fs.write(mare.cwd .. "/child-shutdown-ok.txt", "yes")
                    end)
                end,
            }
            "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "child done" }]))
            .build()
            .await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(&h, "local res = mare.subagent.start({ prompt = 'run child', cwd = CWD }):wait()").await;
        h.shutdown().await;
        assert!(env.cwd.join("child-shutdown-ok.txt").exists(), "child host did not reach shutdown");
        assert!(!env.cwd.join("child-ended.txt").exists(), "child host must never emit session/ended");
    })
    .await;
}

#[tokio::test]
async fn user_plugin_switches_identity_through_the_handle() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/switch_probe.lua"),
            r#"return {
                name = "switch-probe",
                inject = { "kernel" },
                apply = function(ctx)
                    mare._do_switch = function(target)
                        local before = ctx.kernel:session_epoch()
                        local sw = ctx.kernel:begin_session_switch({ session_id = target })
                        local committed = sw:commit(target)
                        local after = ctx.kernel:session_epoch()
                        return before, committed.session_epoch, after
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().build().await;
        let (before, committed, after): (String, String, String) =
            eval(&h, r#"return mare._do_switch("user-plugin-sid-1")"#).await;
        assert_ne!(before, committed, "commit must advance session_epoch");
        assert_eq!(committed, after, "kernel:session_epoch must reflect committed epoch");
        assert!(committed.starts_with("user-plugin-sid-1#"), "{committed}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn closed_switch_cannot_commit() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build_exposed().await;
        let (ok_after_abort, ok_after_close): (bool, bool) = eval(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local sw1 = k:begin_session_switch({ session_id = "aborted-sid" })
            sw1:abort()
            local ok1 = pcall(function() sw1:commit("aborted-sid") end)
            local escaped
            do
                local sw2 <close> = k:begin_session_switch({ session_id = "scoped-sid" })
                escaped = sw2
            end
            local ok2 = pcall(function() escaped:commit("scoped-sid") end)
            return ok1, ok2
            "#,
        )
        .await;
        assert!(!ok_after_abort, "aborted switch handle must refuse commit");
        assert!(!ok_after_close, "<close>-closed switch handle must refuse commit");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn duplicate_target_fails_before_publish() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let opts = || mare_host::HostOptions {
            cwd: env.cwd.clone(),
            mode: mare_shared::ui::Mode::Print,
            config: json!({
                "model": "faux/echo",
                "shutdown_grace_ms": 50,
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "script_json": "[]" }
                }
            }),
            load_init: false,
            project_trusted: true,
            args: json!({ "no_session": true }),
            no_user_plugins: true,
        };
        let s1 = mgr.create_session(opts()).await.unwrap();
        let s2 = mgr.create_session(opts()).await.unwrap();
        expose(&s2.host);
        global(&s2.host, "TARGET_SID", s1.session_id.clone());
        let (ok, code): (bool, String) = eval(
            &s2.host,
            r#"
            local k = C.cordis:get("kernel")
            local ok, err = pcall(function()
                return k:begin_session_switch({ session_id = TARGET_SID })
            end)
            if ok then return true, "" end
            return false, type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert!(!ok, "duplicate target session_id must fail during begin_session_switch");
        assert_eq!(code, "session.already_loaded", "must raise session.already_loaded: {code}");
        mgr.shutdown_all().await;
    })
    .await;
}

#[tokio::test]
async fn abort_only_releases_the_reservation() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let opts = || mare_host::HostOptions {
            cwd: env.cwd.clone(),
            mode: mare_shared::ui::Mode::Print,
            config: json!({
                "model": "faux/echo",
                "shutdown_grace_ms": 50,
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "script_json": "[]" }
                }
            }),
            load_init: false,
            project_trusted: true,
            args: json!({ "no_session": true }),
            no_user_plugins: true,
        };
        let s1 = mgr.create_session(opts()).await.unwrap();
        let s2 = mgr.create_session(opts()).await.unwrap();
        expose(&s1.host);
        expose(&s2.host);

        // s1 reserves a fresh target id in a live coroutine; while reserved, s2 sees session.already_loaded.
        exec(
            &s1.host,
            r#"
            PROVIDER_STATE = "mutated-before-abort"
            local ready_tx, ready_rx = mare.task.channel()
            SW_REL_TX, SW_REL_RX = mare.task.channel()
            SW_DONE_TX, SW_DONE_RX = mare.task.channel()
            mare.task.spawn(function()
                local sw <close> = C.cordis:get("kernel"):begin_session_switch({ session_id = "reserved-target-1" })
                ready_tx:send(true)
                SW_REL_RX:recv()
                sw:abort()
                SW_DONE_TX:send(true)
            end)
            assert(mare.task.timeout(5000, function() return ready_rx:recv() end))
            "#,
        )
        .await;
        let blocked_code: String = eval(
            &s2.host,
            r#"
            local ok, err = pcall(function()
                return C.cordis:get("kernel"):begin_session_switch({ session_id = "reserved-target-1" })
            end)
            assert(not ok, "active reservation must block sibling runtime")
            return type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert_eq!(blocked_code, "session.already_loaded");

        // Aborting on s1 releases the reservation without touching provider state.
        let state_after_abort: String = eval(
            &s1.host,
            r#"
            SW_REL_TX:send(true)
            assert(mare.task.timeout(5000, function() return SW_DONE_RX:recv() end))
            return PROVIDER_STATE
            "#,
        )
        .await;
        assert_eq!(state_after_abort, "mutated-before-abort");

        // Also verify a switch leaked by a finished coroutine on s1 is automatically reclaimed
        // when s2 attempts to reserve the same target id.
        exec(
            &s1.host,
            r#"
            local done_tx, done_rx = mare.task.channel()
            mare.task.spawn(function()
                LEAKED_S1_SW = C.cordis:get("kernel"):begin_session_switch({ session_id = "reserved-target-1" })
                done_tx:send(true)
            end)
            assert(mare.task.timeout(5000, function() return done_rx:recv() end))
            mare.task.sleep(10)
            "#,
        )
        .await;

        let s2_epoch: String = eval(
            &s2.host,
            r#"
            local sw <close> = C.cordis:get("kernel"):begin_session_switch({ session_id = "reserved-target-1" })
            return sw:commit("reserved-target-1").session_epoch
            "#,
        )
        .await;
        assert!(s2_epoch.starts_with("reserved-target-1#"), "{s2_epoch}");
        mgr.shutdown_all().await;
    })
    .await;
}

/// Cancelling an in-place session switch AFTER `publish` + `finish_switch` hands off to the
/// independent completion task (paused before `sw:commit` / `session/changed`) drops the caller
/// coroutine; the completion task must still hold the `SessionSwitchHandle` reservation (blocking
/// sibling runtimes with `session.already_loaded`), commit session identity across EventLog /
/// HostManager, and emit `session/changed`.
#[tokio::test]
async fn cancelled_session_switch_export_still_commits_identity_and_keeps_reservation() {
    local(async {
        let env = TestEnv::new().await;
        let target_file = env.write_session("resume-target.jsonl", &[msg("m1", None, "user", "hello resume")]);
        let target_sid = {
            let first_line = std::fs::read_to_string(&target_file).unwrap().lines().next().unwrap().to_string();
            let header: Value = serde_json::from_str(&first_line).unwrap();
            header["session_id"].as_str().unwrap().to_string()
        };

        let mgr = mare_host::HostManager::new();
        let opts = || mare_host::HostOptions {
            cwd: env.cwd.clone(),
            mode: mare_shared::ui::Mode::Print,
            config: json!({
                "model": "faux/echo",
                "shutdown_grace_ms": 50,
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "script_json": "[]" }
                }
            }),
            load_init: false,
            project_trusted: true,
            args: json!({}),
            no_user_plugins: true,
        };
        let s1 = mgr.create_session(opts()).await.unwrap();
        let s2 = mgr.create_session(opts()).await.unwrap();
        expose(&s1.host);
        expose(&s2.host);

        // Pause s1's completion task inside `agent/messages-replaced` (after publish + transferred,
        // right before `changed("resume")` calls `sw.commit_identity(S.id)` and `session/changed`).
        exec(
            &s1.host,
            r#"
            PAUSED_TX, PAUSED_RX = mare.task.channel()
            RESUME_TX, RESUME_RX = mare.task.channel()
            CHANGED_TX, CHANGED_RX = mare.task.channel()
            C.cordis:on("agent/messages-replaced", function()
                if not PAUSED_ONCE then
                    PAUSED_ONCE = true
                    PAUSED_TX:send(true)
                    RESUME_RX:recv()
                end
            end)
            C.cordis:on("session/changed", function(ev)
                CHANGED_TX:send(ev and ev.session_id or "")
            end)
            "#,
        )
        .await;

        // In multi-runtime mode, `session.load` creates a sibling runtime while in-place
        // switching goes through `session.switch`. Drop the caller coroutine via `select!`
        // (the same drop path `call_service_export` takes on cancel) while the independent
        // completion task is paused before `sw:commit`.
        global(&s1.host, "TARGET_PATH", target_file.to_string_lossy().to_string());
        let cancel = tokio_util::sync::CancellationToken::new();
        let h1 = s1.host.clone();
        let cancel_call = cancel.clone();
        let export_task = tokio::task::spawn_local(async move {
            let fut = h1.lua().load("S:switch({ kind = 'load', path = TARGET_PATH })").exec_async();
            tokio::select! {
                _ = cancel_call.cancelled() => Err(mare_kernel::ServiceCallError::Cancelled),
                res = fut => res.map_err(mare_kernel::ServiceCallError::from),
            }
        });

        // Wait until the completion task is paused between `publish` and `sw:commit`.
        let paused: bool =
            eval(&s1.host, "return mare.task.timeout(5000, function() return PAUSED_RX:recv() end)").await;
        assert!(paused, "completion task must pause before sw:commit");

        // Cancel the RPC export call so `call_fut` drops and its caller coroutine ends.
        cancel.cancel();
        let export_res = tokio::time::timeout(std::time::Duration::from_secs(3), export_task)
            .await
            .expect("cancelled export must return promptly")
            .unwrap();
        assert!(
            matches!(export_res, Err(mare_kernel::ServiceCallError::Cancelled)),
            "export call must cancel: {export_res:?}"
        );

        // Force Lua GC + yield so the dropped caller coroutine is finished/collected, then probe
        // from sibling runtime s2: the reservation MUST STILL BE HELD (never purged!).
        exec(&s1.host, "collectgarbage('collect'); mare.task.sleep(10)").await;
        global(&s2.host, "TARGET_SID", target_sid.clone());
        let blocked_code: String = eval(
            &s2.host,
            r#"
            local ok, err = pcall(function()
                return C.cordis:get("kernel"):begin_session_switch({ session_id = TARGET_SID })
            end)
            assert(not ok, "in-flight completion task reservation must still block sibling runtime after caller cancel")
            return type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert_eq!(blocked_code, "session.already_loaded");

        // Resume the completion task: it must commit identity and emit `session/changed` cleanly.
        let changed_id: String = eval(
            &s1.host,
            r#"
            RESUME_TX:send(true)
            local ok, val = mare.task.timeout(5000, function() return CHANGED_RX:recv() end)
            return ok and tostring(val) or ""
            "#,
        )
        .await;
        assert_eq!(changed_id, target_sid, "completion task must emit session/changed with target id");
        assert_eq!(
            s1.host.kernel.event_log().session_identity().0.as_deref(),
            Some(target_sid.as_str()),
            "EventLog session identity must be committed to target_sid"
        );
        // After commit, s2 must STILL see session.already_loaded because s1 now owns target_sid.
        let owned_code: String = eval(
            &s2.host,
            r#"
            local ok, err = pcall(function()
                return C.cordis:get("kernel"):begin_session_switch({ session_id = TARGET_SID })
            end)
            assert(not ok, "committed session on s1 must still block sibling runtime s2")
            return type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert_eq!(owned_code, "session.already_loaded");

        mgr.shutdown_all().await;
    })
    .await;
}

/// TUI slash commands (/new, /fork, /clone, /resume, /delete) must route through the
/// `session` service (and therefore the orchestrator) rather than calling internal
/// `D.lifecycle.new_session` / `D.fork.fork_at` / `D.lifecycle.load_file` directly.
#[tokio::test]
async fn tui_new_fork_close_commands_go_through_the_orchestrator() {
    local(async {
        let env = TestEnv::new().await;
        let h =
            env.host().builtins(&["changes"]).faux(json!([{ "text": "reply-1" }, { "text": "reply-2" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"
            ORCH_CALLS = {}
            for _, method in ipairs({ "create", "load", "fork", "close" }) do
                local orig = S[method]
                assert(type(orig) == "function", "missing session." .. method)
                S[method] = function(self, ...)
                    ORCH_CALLS[#ORCH_CALLS + 1] = method
                    return orig(self, ...)
                end
            end
            mare.ui.confirm = function() return true end
            mare.ui.set_editor = function() end
            mare.ui.clear = function() end
            "#,
        )
        .await;

        // 1. /new -> session.create
        let before_new: String = eval(&h, "return S:info().session_id").await;
        exec(&h, r#"CM:run("/new")"#).await;
        let after_new: String = eval(&h, "return S:info().session_id").await;
        assert_ne!(after_new, before_new, "/new must switch to a fresh session");

        // Record two completed turns so /fork (which forks before the latest user message)
        // has a recorded completed turn to keep.
        h.prompt("first turn for commands", json!({})).await.unwrap();
        h.prompt("second turn for commands", json!({})).await.unwrap();
        let seed_id: String = eval(&h, "return S:info().session_id").await;
        global(&h, "SEED_ID", seed_id.clone());

        // 2. /fork -> session.fork
        exec(&h, r#"CM:run("/fork")"#).await;
        let forked_id: String = eval(&h, "return S:info().session_id").await;
        assert_ne!(forked_id, seed_id, "/fork must switch to a forked session");

        // 3. /resume <seed_id> -> session.load
        exec(&h, r#"CM:run("/resume " .. SEED_ID)"#).await;
        wait_until(5, "/resume switches back to seed session", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return S:info().session_id == SEED_ID").await }
        })
        .await;

        // 4. /clone -> session.fork
        exec(&h, r#"CM:run("/clone")"#).await;
        let cloned_id: String = eval(&h, "return S:info().session_id").await;
        assert_ne!(cloned_id, seed_id, "/clone must switch to a cloned session");

        // 5. /delete (on persisted cloned session) -> session.close
        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "/delete finishes", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Deleted session") }
        })
        .await;

        let calls: Vec<String> = eval(&h, "return ORCH_CALLS").await;
        assert_eq!(
            calls,
            vec!["create", "fork", "load", "fork", "close"],
            "TUI commands bypassed the session service / orchestrator: {calls:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// If the session is already poisoned when `store_switch`'s completion task enters its body
/// (before `finish <close>` is declared), the early-return branch must explicitly clear
/// `lease.completion_token`, abort `lease.identity_switch` (releasing any cross-runtime
/// reservation), call `release_switch(st, lease)`, and unblock the caller.
#[tokio::test]
async fn poisoned_completion_early_return_aborts_switch_and_releases_reservation() {
    local(async {
        let env = TestEnv::new().await;
        let target_file = env.write_session("poison-early-target.jsonl", &[msg("m1", None, "user", "hello")]);
        let target_sid = {
            let first_line = std::fs::read_to_string(&target_file).unwrap().lines().next().unwrap().to_string();
            let header: Value = serde_json::from_str(&first_line).unwrap();
            header["session_id"].as_str().unwrap().to_string()
        };

        let mgr = mare_host::HostManager::new();
        let opts = || mare_host::HostOptions {
            cwd: env.cwd.clone(),
            mode: mare_shared::ui::Mode::Print,
            config: json!({
                "model": "faux/echo",
                "shutdown_grace_ms": 50,
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "script_json": "[]" }
                }
            }),
            load_init: false,
            project_trusted: true,
            args: json!({}),
            no_user_plugins: true,
        };
        let s1 = mgr.create_session(opts()).await.unwrap();
        let s2 = mgr.create_session(opts()).await.unwrap();
        expose(&s1.host);
        expose(&s2.host);

        // Wrap `mare.task.spawn_completion` and reload `session` so `session.store_switch`
        // captures the wrapper. When `finish_switch` spawns the completion task, poison the
        // session right before the task body runs, signal once the body returns early, and then
        // keep the completion coroutine alive until `KEEP_ALIVE_TX` fires so origin-thread
        // liveness purge cannot mask a missing explicit `sw:abort()`.
        exec(
            &s1.host,
            r#"
            EARLY_DONE_TX, EARLY_DONE_RX = mare.task.channel()
            KEEP_ALIVE_TX, KEEP_ALIVE_RX = mare.task.channel()
            local orig_spawn = mare.task.spawn_completion
            local intercepted = false
            mare.task.spawn_completion = function(ctx, fn)
                if not intercepted then
                    intercepted = true
                    return orig_spawn(ctx, function()
                        mare.task.fail_session("injected poison before completion body")
                        fn()
                        EARLY_DONE_TX:send(true)
                        KEEP_ALIVE_RX:recv()
                    end)
                end
                return orig_spawn(ctx, fn)
            end
            package.loaded["session.store_switch"] = nil
            C.cordis:get("kernel"):reload("session")
            S = C.cordis:get("session")
            "#,
        )
        .await;

        global(&s1.host, "TARGET_PATH", target_file.to_string_lossy().to_string());
        let h1 = s1.host.clone();
        let load_task = tokio::task::spawn_local(async move {
            h1.lua()
                .load(
                    r#"
                    local ok, err = pcall(function() return S:switch({ kind = "load", path = TARGET_PATH }) end)
                    return ok, tostring(err)
                    "#,
                )
                .eval_async::<(bool, String)>()
                .await
                .unwrap()
        });

        let early_done: bool =
            eval(&s1.host, "return mare.task.timeout(5000, function() return EARLY_DONE_RX:recv() end)").await;
        assert!(early_done, "completion task must reach early return when session is poisoned");

        // While the completion coroutine is STILL alive (blocked on KEEP_ALIVE_RX), s1 must have
        // aborted its active host switch and released the cross-runtime reservation for target_sid.
        assert!(
            !s1.host.is_session_switching(),
            "poison_reason() early-return branch must explicitly abort lease.identity_switch"
        );
        global(&s2.host, "TARGET_SID", target_sid.clone());
        let s2_epoch: String = eval(
            &s2.host,
            r#"
            local sw <close> = C.cordis:get("kernel"):begin_session_switch({ session_id = TARGET_SID })
            return sw:commit(TARGET_SID).session_epoch
            "#,
        )
        .await;
        assert!(
            s2_epoch.starts_with(&format!("{target_sid}#")),
            "sibling runtime must acquire target_sid reservation immediately after early return: {s2_epoch}"
        );

        // `S:switch` must fail promptly without waiting for KEEP_ALIVE_TX.
        let (load_ok, load_err) = tokio::time::timeout(std::time::Duration::from_secs(3), load_task)
            .await
            .expect("S:switch must not hang waiting on completion channel after poisoned early return")
            .unwrap();
        assert!(!load_ok, "S:switch must fail when completion returns early due to poison: {load_err}");

        exec(&s1.host, "KEEP_ALIVE_TX:send(true)").await;
        mgr.shutdown_all().await;
    })
    .await;
}

/// A plugin listener on `app/ready` must be able to call `ctx.session:create({})` on a standalone
/// host because the single-runtime orchestrator is attached before `app/ready` fires.
#[tokio::test]
async fn session_create_works_inside_app_ready_listener() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("ready-creator.lua"),
            r#"
            return {
                name = "ready-creator",
                inject = { "session" },
                apply = function(ctx)
                    ctx:on("app/ready", function()
                        local ok, res = pcall(function() return ctx.session:create({}) end)
                        mare.g.ready_create_ok = ok
                        mare.g.ready_create_res = ok and (res and res.session_id) or tostring(res)
                    end)
                end,
            }
            "#,
        )
        .unwrap();
        let h = env.host().user_plugins().faux(json!([])).build().await;
        let (ok, res): (bool, String) =
            eval(&h, "return mare.g.ready_create_ok == true, tostring(mare.g.ready_create_res or '')").await;
        assert!(ok, "ctx.session:create must succeed during app/ready, got: {res}");
        assert!(!res.is_empty(), "expected non-empty session_id from app/ready create");
        h.shutdown().await;
    })
    .await;
}

/// A custom session provider that omits optional `switch` must return `unavailable` (not `internal`)
/// when host-provided lifecycle methods run on a single-runtime host.
#[tokio::test]
async fn custom_session_provider_without_switch_reports_unavailable() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let no_switch = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session_replacement.lua"))
            .replace("switch = function", "unused_switch = function");
        std::fs::write(plugins.join("session.lua"), no_switch).unwrap();
        let h = env.host().user_plugins().faux(json!([])).build().await;
        expose(&h);
        let (create_code, load_code, fork_code, close_code): (String, String, String, String) = eval(
            &h,
            r#"
            local function call_code(fn)
                local ok, err = pcall(fn)
                assert(not ok, "expected missing switch to fail")
                return type(err) == "table" and tostring(err.code) or tostring(err)
            end
            local sid = S:info().session_id
            return
                call_code(function() return S:create({}) end),
                call_code(function() return S:load({ session_id = sid }) end),
                call_code(function() return S:fork({ session_id = sid }) end),
                call_code(function() return S:close(sid) end)
            "#,
        )
        .await;
        assert_eq!(create_code, "unavailable", "create without switch must return unavailable");
        assert_eq!(load_code, "unavailable", "load without switch must return unavailable");
        assert_eq!(fork_code, "unavailable", "fork without switch must return unavailable");
        assert_eq!(close_code, "unavailable", "close without switch must return unavailable");
        h.shutdown().await;
    })
    .await;
}

/// In single-runtime mode, `session.close` with an inactive selector and an explicit
/// `expected_epoch` must fail with `stale_session` rather than succeeding as a no-op.
#[tokio::test]
async fn single_close_inactive_session_with_expected_epoch_returns_stale_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "saved-1" }])).build().await;
        expose(&h);
        // Persist an initial session, then switch away so it is a known, indexed, inactive session.
        h.prompt("persist-inactive", json!({})).await.unwrap();
        let other_sid: String = eval(&h, "return S:info().session_id").await;
        exec(&h, "assert(S:create({}))").await;
        global(&h, "OTHER_SID", other_sid);
        let (ok, code): (bool, String) = eval(
            &h,
            r##"
            local ok, err = pcall(function()
                return S:close(OTHER_SID, { expected_epoch = OTHER_SID .. "#1" })
            end)
            return ok, type(err) == "table" and tostring(err.code) or tostring(err)
            "##,
        )
        .await;
        assert!(!ok, "closing an inactive session with expected_epoch must fail");
        assert_eq!(code, "stale_session", "expected stale_session, got: {code}");
        h.shutdown().await;
    })
    .await;
}

/// In multi-runtime mode, `/delete` on the active session must delete the journal files while
/// the VM is still alive so delete failures roll back cleanly without destroying the runtime,
/// and only close the runtime after deletion succeeds.
#[tokio::test]
async fn multi_runtime_delete_command_rolls_back_on_failure_and_closes_after_success() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let h = env
            .host()
            .builtins(&["changes"])
            .faux(json!([{ "text": "saved-turn" }, { "text": "after-rollback" }]))
            .build()
            .await;
        let rt = mgr.register_host(h.clone(), None, false).await.unwrap();
        let sid = rt.session_id.clone();

        // Persist a turn so the session file exists on disk.
        h.prompt("persist session for multi delete", json!({})).await.unwrap();

        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let session_path: String = eval(&h, "return S:info().path").await;
        assert!(std::path::Path::new(&session_path).exists(), "session file must exist before /delete");

        let deleted_ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        {
            let deleted_ids = std::sync::Arc::clone(&deleted_ids);
            let record_deleted = h
                .lua()
                .create_function(move |_, sid: String| {
                    deleted_ids.lock().unwrap().push(sid);
                    Ok(())
                })
                .unwrap();
            h.lua().globals().set("RECORD_DELETED", record_deleted).unwrap();
        }

        exec(
            &h,
            r#"
            mare.ui.confirm = function() return true end
            mare.ui.clear = function() end
            ORIG_STORE_DELETE = mare.store.delete
            mare.store.delete = function()
                return nil, "injected delete failure"
            end
            C.cordis:on("session/deleted", function(ev)
                if type(ev) == "table" and type(ev.session_id) == "string" then
                    RECORD_DELETED(ev.session_id)
                end
            end)
            "#,
        )
        .await;

        // 1. Delete failure: VM must stay alive, session stays registered in HostManager,
        //    file remains on disk, and rollback reopens the journal cleanly.
        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "/delete failure notifies rollback in live VM", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Session not fully deleted") }
        })
        .await;

        assert!(
            mgr.session(&sid).is_ok(),
            "failed /delete in multi-runtime mode must not close or remove the active session runtime"
        );
        assert!(std::path::Path::new(&session_path).exists(), "failed /delete must leave the session file on disk");
        let rollback_ok: bool =
            eval(&h, "local ok, res = pcall(S.update, S, 'survived-rollback'); return ok and type(res) == 'table'")
                .await;
        assert!(rollback_ok, "rolled-back session must reopen its journal writer cleanly");

        // 2. Delete success: file is deleted, session/deleted is emitted, and the runtime closes
        //    cleanly without self-deadlocking.
        exec(&h, "mare.store.delete = ORIG_STORE_DELETE").await;
        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "multi-runtime /delete closes session after deleting files", || {
            let mgr = mgr.clone();
            let sid = sid.clone();
            async move { mgr.session(&sid).is_err() && !mgr.is_closing(&sid) }
        })
        .await;

        assert!(
            !std::path::Path::new(&session_path).exists(),
            "successful /delete must remove the session file from disk"
        );
        assert_eq!(
            *deleted_ids.lock().unwrap(),
            vec![sid.clone()],
            "successful /delete must emit session/deleted before closing the runtime"
        );

        mgr.shutdown_all().await;
    })
    .await;
}

/// In multi-runtime mode, `/delete` must refuse while a turn is busy (matching single-runtime
/// `require_idle`), must rely on the public `kernel:session_capacity()` signal rather than a
/// spoofable `mare.g` key, and must keep a delete barrier armed so late writes cannot reopen
/// `S.planned` and resurrect the deleted session file.
#[tokio::test]
async fn multi_runtime_delete_refuses_busy_turn_and_blocks_late_write_resurrection() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let h = env
            .host()
            .builtins(&["changes"])
            .faux(json!([
                { "text": "saved-turn" },
                { "tool_calls": [{ "id": "tc1", "name": "slow_block", "arguments": {} }] },
                { "text": "after-tool" }
            ]))
            .build()
            .await;

        expose(&h);
        let single_cap: String = eval(&h, r#"return C.cordis:get("kernel"):session_capacity()"#).await;
        assert_eq!(single_cap, "single", "standalone host must report single capacity");

        let rt = mgr.register_host(h.clone(), None, false).await.unwrap();
        let sid = rt.session_id.clone();

        let multi_cap: String = eval(&h, r#"return C.cordis:get("kernel"):session_capacity()"#).await;
        assert_eq!(multi_cap, "multi", "HostManager host must report multi capacity");
        let no_private_global: bool = eval(&h, r#"return mare.g["session.multi_runtime"] == nil"#).await;
        assert!(no_private_global, "host must not publish undocumented mare.g[\"session.multi_runtime\"]");

        // Persist a turn so the session file exists on disk.
        h.prompt("persist session before busy delete", json!({})).await.unwrap();

        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let session_path: String = eval(&h, "return S:info().path").await;
        assert!(std::path::Path::new(&session_path).exists());

        // Register a blocking tool so a turn stays genuinely busy across `/delete`, and also
        // spoof `mare.g["session.multi_runtime"] = false` to verify `/delete` ignores `mare.g`.
        exec(
            &h,
            r#"
            mare.ui.confirm = function() return true end
            mare.ui.clear = function() end
            mare.g["session.multi_runtime"] = false
            BUSY_ENTERED_TX, BUSY_ENTERED_RX = mare.task.channel()
            BUSY_RELEASE_TX, BUSY_RELEASE_RX = mare.task.channel()
            C.cordis:get("tools"):register("slow_block", {
                description = "blocks turn for busy delete test",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                idempotent = true,
                execute = function()
                    BUSY_ENTERED_TX:send(true)
                    BUSY_RELEASE_RX:recv()
                    return { content = "done" }
                end,
            })
            "#,
        )
        .await;

        // 1. Start a busy turn and attempt `/delete` while it is in flight.
        let prompt_host = h.clone();
        let turn_task =
            tokio::task::spawn_local(async move { prompt_host.prompt("start busy tool turn", json!({})).await });
        let entered: bool =
            eval(&h, "return mare.task.timeout(5000, function() return BUSY_ENTERED_RX:recv() end) == true").await;
        assert!(entered, "busy turn must enter blocking tool");

        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "busy multi-runtime /delete notifies refusal", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Session not deleted") }
        })
        .await;

        assert!(mgr.session(&sid).is_ok(), "busy /delete must leave runtime active");
        assert!(std::path::Path::new(&session_path).exists(), "busy /delete must NOT delete the session file on disk");

        // Let the cancelled/in-flight turn finish cleanly before testing late-write resurrection.
        exec(&h, "BUSY_RELEASE_TX:send(true)").await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), turn_task).await.unwrap();
        wait_until(5, "turn becomes idle", || {
            let h = h.clone();
            async move { eval::<bool>(&h, r#"return not C.cordis:get("agent"):activity().busy"#).await }
        })
        .await;

        // 2. Register a `session/deleted` listener that attempts late writes after deletion
        //    (both `set_name` and `record`). The delete barrier must reject them and the deleted
        //    session file must never reappear on disk.
        let late_write_blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let flag = std::sync::Arc::clone(&late_write_blocked);
            let record = h
                .lua()
                .create_function(move |_, blocked: bool| {
                    flag.store(blocked, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            h.lua().globals().set("RECORD_LATE_BLOCKED", record).unwrap();
        }
        exec(
            &h,
            r#"
            C.cordis:on("session/deleted", function()
                local ok1, ok2 = pcall(function() return S:update("resurrected") end)
                local name_failed = (not ok1) or (ok2 == false) or (ok2 == nil)
                local rec_ok, rec_res = pcall(function() return S:add_record("late/write", { resurrected = true }) end)
                local rec_failed = (not rec_ok) or (rec_res == false) or (rec_res == nil)
                RECORD_LATE_BLOCKED(name_failed and rec_failed)
            end)
            "#,
        )
        .await;

        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "idle multi-runtime /delete closes runtime after deleting files", || {
            let mgr = mgr.clone();
            let sid = sid.clone();
            async move { mgr.session(&sid).is_err() && !mgr.is_closing(&sid) }
        })
        .await;

        assert!(
            late_write_blocked.load(std::sync::atomic::Ordering::SeqCst),
            "late writes during/after multi-runtime /delete must be rejected by the delete barrier"
        );
        assert!(
            !std::path::Path::new(&session_path).exists(),
            "late writes after multi-runtime /delete must never resurrect the deleted session file"
        );

        mgr.shutdown_all().await;
    })
    .await;
}

/// Multi-runtime sibling hosts created by `create_session` or `load_session` must already have
/// the Multi orchestrator attached before `app/ready` fires, so plugins in `app/ready` observe
/// `multi` capacity and calling `ctx.session:create({})` spawns a separate runtime instead of
/// switching the booting host in-place (which would cause `SessionIdMismatch`).
#[tokio::test]
async fn multi_runtime_sibling_hosts_see_multi_orchestrator_during_app_ready() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let sentinel = env.cwd.join("ready-create-once.flag");
        let sentinel_str = sentinel.to_string_lossy().replace('\\', "/");
        std::fs::write(
            plugins.join("ready-multi-check.lua"),
            format!(
                r#"
                return {{
                    name = "ready-multi-check",
                    inject = {{ "session", "kernel" }},
                    apply = function(ctx)
                        ctx:on("app/ready", function()
                            mare.g.ready_capacity = ctx.kernel:session_capacity()
                            if mare.fs.exists("{sentinel_str}") then
                                mare.fs.remove("{sentinel_str}")
                                local ok, res = pcall(function() return ctx.session:create({{}}) end)
                                mare.g.ready_spawn_ok = ok
                                mare.g.ready_spawn_sid = ok and (type(res) == "table" and res.session_id) or tostring(res)
                            end
                        end)
                    end,
                }}
                "#
            ),
        )
        .unwrap();

        struct TestExt {
            base_opts: mare_host::HostOptions,
        }
        impl mare_host::OrchestratorExt for TestExt {
            fn clean_opts(
                &self,
                _cwd_override: Option<&str>,
                no_session_override: Option<bool>,
            ) -> Result<mare_host::HostOptions, mare_host::OrchestratorError> {
                let mut opts = self.base_opts.clone();
                let mut args = opts.args.as_object().cloned().unwrap_or_default();
                for k in ["resume", "continue", "session_id", "fork_session"] {
                    args.remove(k);
                }
                if let Some(ns) = no_session_override {
                    args.insert("no_session".into(), json!(ns));
                }
                opts.args = serde_json::Value::Object(args);
                Ok(opts)
            }
        }

        let base_opts = mare_host::HostOptions {
            cwd: env.cwd.clone(),
            mode: mare_plugin::Mode::Print,
            config: json!({
                "model": "faux/echo",
                "plugins": {
                    "permissions": { "enabled": false },
                    "provider-faux": { "script_json": r#"[{"text":"ok"}]"# }
                }
            }),
            load_init: false,
            project_trusted: true,
            args: json!({}),
            no_user_plugins: false,
        };

        let mgr = mare_host::HostManager::new();
        mgr.orchestrator().set_ext(std::rc::Rc::new(TestExt { base_opts: base_opts.clone() }));

        // 1. `create_session`: sibling host observes `multi` in `app/ready`, and calling
        //    `ctx.session:create({})` inside `app/ready` spawns a distinct runtime.
        std::fs::write(&sentinel, "1").unwrap();
        let rt1 = mgr.create_session(base_opts.clone()).await.expect("create_session must succeed");
        let cap1: String = eval(&rt1.host, "return tostring(mare.g.ready_capacity)").await;
        let spawn_ok1: bool = eval(&rt1.host, "return mare.g.ready_spawn_ok == true").await;
        let spawned_sid1: String = eval(&rt1.host, "return tostring(mare.g.ready_spawn_sid)").await;
        assert_eq!(cap1, "multi", "create_session sibling must see multi capacity during app/ready");
        assert!(spawn_ok1, "app/ready ctx.session:create must succeed: {spawned_sid1}");
        assert_ne!(spawned_sid1, rt1.session_id, "app/ready create must spawn a separate runtime, not switch in place");
        assert!(mgr.session(&spawned_sid1).is_ok(), "spawned runtime must be registered in HostManager");

        // Persist rt1, close it, then reload via `load_session(Some(sid), ...)` while armed to
        // call `ctx.session:create({})` during `app/ready`: must NOT fail with SessionIdMismatch.
        rt1.host.prompt("persist for reload", json!({})).await.unwrap();
        let saved_sid = rt1.session_id.clone();
        expose(&rt1.host);
        let saved_path: String = eval(&rt1.host, "return S:info().path").await;
        let _ = mgr.close_session(&saved_sid).await.unwrap();

        std::fs::write(&sentinel, "1").unwrap();
        let mut load_opts = base_opts.clone();
        load_opts.args = json!({ "resume": saved_path });
        let rt_reloaded = mgr
            .load_session(Some(&saved_sid), load_opts)
            .await
            .expect("load_session must not fail with SessionIdMismatch when app/ready calls session:create");
        let cap2: String = eval(&rt_reloaded.host, "return tostring(mare.g.ready_capacity)").await;
        let spawn_ok2: bool = eval(&rt_reloaded.host, "return mare.g.ready_spawn_ok == true").await;
        let spawned_sid2: String = eval(&rt_reloaded.host, "return tostring(mare.g.ready_spawn_sid)").await;
        assert_eq!(cap2, "multi", "load_session sibling must see multi capacity during app/ready");
        assert!(spawn_ok2, "app/ready ctx.session:create on load_session must succeed: {spawned_sid2}");
        assert_eq!(rt_reloaded.session_id, saved_sid, "reloaded runtime must keep its expected session id");
        assert_ne!(spawned_sid2, saved_sid);
        assert!(mgr.session(&spawned_sid2).is_ok());

        mgr.shutdown_all().await;
    })
    .await;
}

/// When multi-runtime `/delete` encounters a thrown error during `drain_side` or during
/// `delete_and_emit`, the delete barrier must be reset so subsequent writes succeed instead
/// of failing silently.
#[tokio::test]
async fn multi_runtime_delete_resets_barrier_when_drain_or_delete_throws() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let h = env.host().builtins(&["changes", "compact"]).faux(json!([{ "text": "saved-turn" }])).build().await;
        expose(&h);
        let rt = mgr.register_host(h.clone(), None, false).await.unwrap();
        let sid = rt.session_id.clone();

        h.prompt("persist before throwing delete", json!({})).await.unwrap();
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let session_path: String = eval(&h, "return S:info().path").await;
        assert!(std::path::Path::new(&session_path).exists());

        // 1. Throw inside `drain_side` (via `compact:busy()`): `/delete` must report failure,
        //    keep the session open, and leave the writer unblocked for subsequent writes.
        exec(
            &h,
            r#"
            mare.ui.confirm = function() return true end
            mare.ui.clear = function() end
            local compact = C.cordis:get("compact")
            ORIG_COMPACT_BUSY = compact.busy
            compact.busy = function()
                error("injected drain throw")
            end
            "#,
        )
        .await;

        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "drain throw notifies delete refusal", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Session not deleted") }
        })
        .await;

        exec(&h, r#"C.cordis:get("compact").busy = ORIG_COMPACT_BUSY"#).await;
        assert!(mgr.session(&sid).is_ok(), "session must remain open after drain throw");
        assert!(std::path::Path::new(&session_path).exists());
        let after_drain_ok: bool = eval(
            &h,
            r#"
            local ok1 = S:update("after-drain-throw")
            local ok2 = S:add_record("probe/after-drain", { ok = true })
            return type(ok1) == "table" and ok2 ~= nil and ok2 ~= false
            "#,
        )
        .await;
        assert!(after_drain_ok, "writer must stay open after drain_side throws");

        // 2. Throw inside `mare.store.delete` (inside `delete_and_emit`): `/delete` must catch
        //    the error, reset the delete barrier, roll back the journal, and allow subsequent writes.
        exec(
            &h,
            r#"
            ORIG_STORE_DELETE = mare.store.delete
            mare.store.delete = function()
                error("injected delete throw")
            end
            "#,
        )
        .await;

        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "delete throw notifies rollback", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Session not fully deleted") }
        })
        .await;

        exec(&h, "mare.store.delete = ORIG_STORE_DELETE").await;
        assert!(mgr.session(&sid).is_ok(), "session must remain open after delete throw");
        assert!(std::path::Path::new(&session_path).exists());
        let after_delete_ok: bool = eval(
            &h,
            r#"
            local ok1 = S:update("after-delete-throw")
            local ok2 = S:add_record("probe/after-delete", { ok = true })
            return type(ok1) == "table" and ok2 ~= nil and ok2 ~= false
            "#,
        )
        .await;
        assert!(after_delete_ok, "writer must recover cleanly after delete_and_emit throws");

        mgr.shutdown_all().await;
    })
    .await;
}

/// Multi-runtime `/delete` must drain side requests BEFORE arming the `deleting` barrier so
/// writes occurring during an abandoned/failed drain are not silently dropped.
#[tokio::test]
async fn multi_runtime_delete_keeps_writes_during_failed_drain() {
    local(async {
        let env = TestEnv::new().await;
        let mgr = mare_host::HostManager::new();
        let h = env.host().builtins(&["changes", "compact"]).faux(json!([{ "text": "saved-turn" }])).build().await;
        expose(&h);
        let rt = mgr.register_host(h.clone(), None, false).await.unwrap();
        let sid = rt.session_id.clone();

        h.prompt("persist before drain write test", json!({})).await.unwrap();
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());

        // Simulate a side request that writes a record while draining and then fails to drain
        // (making `begin_delete` abandon deletion). Because `deleting` must not be armed until
        // after `drain_side` completes, the write during drain must succeed.
        exec(
            &h,
            r#"
            mare.ui.confirm = function() return true end
            mare.ui.clear = function() end
            DRAIN_WRITE_OK = false
            local compact = C.cordis:get("compact")
            ORIG_COMPACT_BUSY = compact.busy
            ORIG_COMPACT_CANCEL = compact.cancel
            compact.busy = function()
                if not DRAIN_WRITE_ATTEMPTED then
                    DRAIN_WRITE_ATTEMPTED = true
                    local ok = S:add_record("probe/during-drain", { kept = true })
                    DRAIN_WRITE_OK = (ok ~= nil and ok ~= false)
                end
                return true
            end
            compact.cancel = nil
            "#,
        )
        .await;

        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(5, "failed drain notifies refusal", || {
            let h = h.clone();
            async move { notices(&h).await.contains("Session not deleted") }
        })
        .await;

        exec(&h, r#"local c = C.cordis:get("compact"); c.busy = ORIG_COMPACT_BUSY; c.cancel = ORIG_COMPACT_CANCEL"#)
            .await;

        let drain_write_ok: bool = eval(&h, "return DRAIN_WRITE_OK == true").await;
        assert!(drain_write_ok, "writes during drain_side must succeed before the deleting barrier is armed");
        assert!(mgr.session(&sid).is_ok());

        mgr.shutdown_all().await;
    })
    .await;
}

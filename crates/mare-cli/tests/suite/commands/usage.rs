//! Side-request usage persistence, isolation, and recovery.

use super::*;

#[tokio::test]
async fn side_request_usage_is_restored_from_the_session_log() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["usage"])
            .faux(json!([{ "text": "initial reply" }, { "text": "side reply" }]))
            .build()
            .await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("initial request", json!({})).await.unwrap();
        let (path, before): (String, String) = eval(
            &h,
            r#"
            local result = L:complete({ purpose = "btw", messages = {
                { role = "user", content = { { type = "text", text = "side request" } } } }, tools = {} })
            assert(result.stop_reason == "stop")
            return S:info().path, mare.json.encode(U:totals())"#,
        )
        .await;
        h.shutdown().await;
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| entry["type"] == "custom" && entry["kind"] == "usage/side-v1")
            .collect();
        assert_eq!(records.len(), 1, "side usage must be stored once: {records:?}");
        let data = records[0]["data"].as_object().unwrap();
        let mut fields: Vec<_> = data.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["id", "leaf_entry_id", "model_id", "provider_id", "purpose", "subscription", "usage"]);
        assert!(data["leaf_entry_id"].as_str().is_some() && data["id"].as_str().is_some(), "{data:?}");
        assert_eq!(
            (data["purpose"].as_str(), data["provider_id"].as_str(), data["model_id"].as_str()),
            (Some("btw"), Some("faux"), Some("echo"))
        );
        assert!(records[0].get("id").is_none(), "a side record must not become a tree node");
        let h = env.host().builtins(&["usage"]).faux(json!([])).resume().build().await;
        global(&h, "U", h.service_table("usage").unwrap());
        let after: String = eval(&h, "return mare.json.encode(U:totals())").await;
        let before: Value = serde_json::from_str(&before).unwrap();
        let after: Value = serde_json::from_str(&after).unwrap();
        for field in ["input", "output", "cache_read", "cache_write", "requests", "side_requests", "cost"] {
            assert_eq!(before[field], after[field], "{field}: before={before} after={after}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn side_usage_follows_its_anchor_when_forking_and_survives_resume() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).faux(json!([{ "text": "first answer" }, { "text": "second answer" }])).build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        exec(
            &h,
            r#"local info = S:info()
            assert(info.leaf_entry_id == nil)
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 77, output = 3 } })"#,
        )
        .await;
        h.prompt("first question", json!({})).await.unwrap();
        exec(
            &h,
            r#"local info = S:info()
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, leaf_entry_id = info.leaf_entry_id, purpose = "btw",
                provider_id = "faux", model_ref = "faux/echo", usage = { input = 111, output = 1 } })"#,
        )
        .await;
        h.prompt("second question", json!({})).await.unwrap();
        let (original_path, fork_path, before, navigated, after): (String, String, i64, i64, i64) = eval(
            &h,
            r#"
            local info = S:info()
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, leaf_entry_id = info.leaf_entry_id, purpose = "compact",
                provider_id = "faux", model_ref = "faux/echo", usage = { input = 222, output = 2 } })
            local before = U:totals().side_requests
            local before_id, first_answer
            for _, e in ipairs(S:entries()) do
                local m = e.message
                if m and m.role == "user" and m.content[1].text == "second question" then before_id = e.entry_id end
                if m and m.role == "assistant" and m.content[1].text == "first answer" then first_answer = e.entry_id end
            end
            assert(before_id and first_answer)
            S:set_leaf(first_answer)
            local navigated = U:totals().side_requests
            S:set_leaf(info.leaf_entry_id)
            S:fork({ session_id = info.session_id, before_item_id = before_id })
            return info.path, S:info().path, before, navigated, U:totals().side_requests"#,
        )
        .await;
        assert_eq!((before, navigated, after), (3, 3, 2));
        h.shutdown().await;
        let original = std::fs::read_to_string(original_path).unwrap();
        let forked = std::fs::read_to_string(&fork_path).unwrap();
        assert!(
            original.contains("\"input\":77")
                && original.contains("\"input\":111")
                && original.contains("\"input\":222")
        );
        assert!(
            forked.contains("\"input\":77") && forked.contains("\"input\":111") && !forked.contains("\"input\":222"),
            "{forked}"
        );
        let entries: Vec<Value> = forked.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
        let root = entries.iter().find(|e| e["kind"] == "usage/side-v1" && e["data"]["usage"]["input"] == 77).unwrap();
        assert!(root["data"]["leaf_entry_id"].is_null(), "root-anchored usage remains rooted");
        let h = env.host().builtins(&["usage"]).faux(json!([])).resume().build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        global(&h, "FORK_PATH", fork_path);
        exec(&h, "S:load({ path = FORK_PATH })").await;
        assert_eq!(eval::<i64>(&h, "return U:totals().side_requests").await, 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_late_side_usage_event_is_not_attributed_to_the_new_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).faux(json!([{ "text": "old answer" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "U", h.service_table("usage").unwrap());
        h.prompt("old request", json!({})).await.unwrap();
        exec(
            &h,
            r#"
            local old_id = S:info().session_id
            S:create()
            C.cordis:get("bus"):emit("llm/usage", { session_id = old_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 123, output = 4 } })"#,
        )
        .await;
        let (requests, side, path, warning): (i64, i64, String, String) = eval(
            &h,
            r#"
            local info, totals = S:info(), U:totals()
            return totals.requests, totals.side_requests, tostring(info.path), table.concat(N, "\n")"#,
        )
        .await;
        assert_eq!((requests, side), (0, 0), "old usage must not pollute the new session");
        assert_eq!(path, "nil", "late usage must not create a new session log");
        assert!(warning.contains("Usage was not saved and will be lost"), "{warning}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_side_usage_write_retries_warns_and_allows_session_exit() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "U", h.service_table("usage").unwrap());
        exec(
            &h,
            r#"CALLS = 0
            local record = S.add_record
            S.add_record = function(...) CALLS = CALLS + 1; return record(...) end"#,
        )
        .await;
        let planned: String = eval(&h, "return S:info().planned").await;
        let path = std::path::Path::new(&planned);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(path))
            .unwrap();
        lock.try_lock().unwrap();
        exec(
            &h,
            r#"local info = S:info()
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 77, output = 3 } })"#,
        )
        .await;
        let (side, warning, id): (i64, String, String) =
            eval(&h, "return U:totals().side_requests, table.concat(N, '\\n'), S:info().session_id").await;
        assert_eq!(side, 1, "usage remains visible in memory");
        assert!(warning.contains("Usage was not saved and will be lost"), "{warning}");
        assert_eq!(eval::<i64>(&h, "return CALLS").await, 2, "one retry follows the failed append");
        assert!(!path.exists(), "a failed write must not create a session file");
        exec(&h, "S:create()").await;
        assert_ne!(eval::<String>(&h, "return S:info().session_id").await, id, "the warning must not block exit");
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn transient_side_usage_write_failure_retries_once_without_duplicate() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "U", h.service_table("usage").unwrap());
        let (calls, side, path, warning): (i64, i64, String, String) = eval(
            &h,
            r#"
            CALLS = 0
            local record = S.add_record
            S.add_record = function(...)
                CALLS = CALLS + 1
                if CALLS == 1 then error("temporary write failure") end
                return record(...)
            end
            local info = S:info()
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 77, output = 3 } })
            return CALLS, U:totals().side_requests, S:info().path, table.concat(N, "\n")"#,
        )
        .await;
        assert_eq!((calls, side), (2, 1));
        assert!(!warning.contains("Usage was not saved"), "{warning}");
        h.shutdown().await;
        let text = std::fs::read_to_string(path).unwrap();
        let entries: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        assert_eq!(entries.iter().filter(|e| e["kind"] == "usage/side-v1").count(), 1, "{text}");
    })
    .await
}

#[tokio::test]
async fn ephemeral_side_usage_stays_in_memory_without_a_session_file() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        exec(
            &h,
            r#"local info = S:info()
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 77, output = 3 } })"#,
        )
        .await;
        let (side, path, records): (i64, String, i64) = eval(
            &h,
            r#"
            return U:totals().side_requests, tostring(S:info().path), #S:list_records("usage/side-v1").data"#,
        )
        .await;
        assert_eq!((side, path.as_str(), records), (1, "nil", 0));
        h.shutdown().await;
        fn jsonl_files(dir: &std::path::Path) -> usize {
            std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    if path.is_dir() {
                        jsonl_files(&path)
                    } else {
                        usize::from(path.extension().is_some_and(|ext| ext == "jsonl"))
                    }
                })
                .sum()
        }
        assert_eq!(jsonl_files(&env.home.path().join("state/sessions")), 0);
    })
    .await
}

#[tokio::test]
async fn read_only_side_usage_does_not_write_to_the_locked_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("locked.jsonl", &[msg("m1", None, "user", "hello")]);
        let before = std::fs::read(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().builtins(&["usage"]).faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "U", h.service_table("usage").unwrap());
        global(&h, "PATH", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"mare.ui.select = function() return 2 end
            S:load({ path = PATH })
            local info = S:info()
            assert(info.readonly)
            C.cordis:get("bus"):emit("llm/usage", { session_id = info.session_id, purpose = "btw", provider_id = "faux", model_ref = "faux/echo",
                usage = { input = 77, output = 3 } })"#,
        )
        .await;
        assert_eq!(eval::<i64>(&h, "return U:totals().side_requests").await, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn side_usage_counts_returned_failed_attempt_and_fallback_once_each() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "fallback_models": ["side-fallback/m"] });
        let h = env.host().builtins(&["usage"]).faux(json!([{ "text": "initial reply" }])).config(cfg).build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("initial request", json!({})).await.unwrap();
        let (path, side, input): (String, i64, i64) = eval(
            &h,
            r#"
            L:register_provider({ provider = "side-first", is_configured = function() return true end,
                models = { { model_id = "m", name = "First", context_window = 100000, max_output = 4096 } },
                stream = function()
                    error({ message = "quota exhausted", kind = "quota", retryable = false,
                        usage = { input = 101, output = 1, cache_read = 0, cache_write = 0 } })
                end })
            L:register_provider({ provider = "side-fallback", is_configured = function() return true end,
                models = { { model_id = "m", name = "Fallback", context_window = 100000, max_output = 4096 } },
                stream = function()
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } },
                        usage = { input = 202, output = 2, cache_read = 0, cache_write = 0 } }
                end })
            local before = U:totals().input
            local result = L:complete({ model = L:resolve("side-first/m"), purpose = "btw",
                messages = { { role = "user", content = { { type = "text", text = "question" } } } }, tools = {} })
            assert(result.stop_reason == "stop")
            return S:info().path, U:totals().side_requests, U:totals().input - before"#,
        )
        .await;
        assert_eq!((side, input), (2, 303));
        h.shutdown().await;
        let entries: Vec<Value> =
            std::fs::read_to_string(path).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let records: Vec<_> = entries.iter().filter(|e| e["kind"] == "usage/side-v1").collect();
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(records[0]["data"]["provider_id"], "side-first");
        assert_eq!(records[1]["data"]["provider_id"], "side-fallback");
        assert_ne!(records[0]["data"]["id"], records[1]["data"]["id"]);
    })
    .await
}

#[tokio::test]
async fn side_usage_keeps_reported_usage_from_an_empty_response_retry() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 1 } } });
        let h = env.host().builtins(&["usage"]).faux(json!([{ "text": "initial reply" }])).config(cfg).build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("initial request", json!({})).await.unwrap();
        let (calls, side, input): (i64, i64, i64) = eval(
            &h,
            r#"
            CALLS = 0
            L:register_provider({ provider = "retry-side", is_configured = function() return true end,
                models = { { model_id = "m", name = "Retry", context_window = 100000, max_output = 4096 } },
                stream = function()
                    CALLS = CALLS + 1
                    if CALLS == 1 then return { role = "assistant", stop_reason = "stop", content = {},
                        usage = { input = 11, output = 1, cache_read = 0, cache_write = 0 } } end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } },
                        usage = { input = 22, output = 2, cache_read = 0, cache_write = 0 } }
                end })
            local before = U:totals().input
            local result = L:complete({ model = L:resolve("retry-side/m"), purpose = "btw",
                messages = { { role = "user", content = { { type = "text", text = "question" } } } }, tools = {} })
            assert(result.stop_reason == "stop")
            return CALLS, U:totals().side_requests, U:totals().input - before"#,
        )
        .await;
        assert_eq!((calls, side, input), (2, 2, 33));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn rate_limit_reset_at_seconds_converts_to_epoch_ms() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage", "provider-openai"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "U", h.service_table("usage").unwrap());
        let report_json: String = eval(
            &h,
            r#"
            local wire = require("provider-openai.wire")
            local parsed = wire.parse_rate_limits({
                ["x-codex-primary-used-percent"] = "42",
                ["x-codex-primary-window-minutes"] = "300",
                ["x-codex-primary-reset-at"] = "1700000123",
                ["x-codex-secondary-used-percent"] = "10",
                ["x-codex-secondary-window-minutes"] = "10080",
                ["x-codex-secondary-reset-at"] = "1700600000.5",
            })
            assert(parsed and parsed.primary and parsed.secondary, "expected parsed codex rate limits")
            C.cordis:get("bus"):emit("llm/rate-limits", {
                provider = "openai",
                limits = parsed,
            })
            local snap = U:get()
            local rl = snap.rate_limits and snap.rate_limits.openai
            assert(type(rl) == "table", "expected openai rate_limits entry")
            return mare.json.encode({
                primary_reset_at_ms = rl.primary and rl.primary.reset_at_ms,
                primary_legacy_reset_at = rl.primary and rl.primary.reset_at,
                secondary_reset_at_ms = rl.secondary and rl.secondary.reset_at_ms,
                received_at_ms = rl.received_at_ms,
                legacy_at = rl.at,
            })
            "#,
        )
        .await;
        let report: Value = serde_json::from_str(&report_json).unwrap();
        assert_eq!(report["primary_reset_at_ms"], 1_700_000_123_000_i64, "{report}");
        assert!(report["primary_legacy_reset_at"].is_null(), "{report}");
        assert_eq!(report["secondary_reset_at_ms"], 1_700_600_000_500_i64, "{report}");
        assert!(report["received_at_ms"].as_i64().is_some_and(|v| v > 1_000_000_000_000), "{report}");
        assert!(report["legacy_at"].is_null(), "{report}");
        h.shutdown().await;
    })
    .await
}

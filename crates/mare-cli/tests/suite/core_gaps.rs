//! Regression tests for core capabilities required by first-party plugins:
//! - idle `agent:prompt` `input_context`, committed user `turn_id`, and `/retry` preservation
//! - `llm.list_auth_keys` RPC and `llm:provider` projecting/generation-bound accessor

use std::collections::BTreeMap;

use serde_json::json;

use crate::common::*;

#[tokio::test]
async fn idle_prompt_preserves_input_context_and_committed_user_messages_carry_turn_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "first ok" }, { "text": "second ok" }])).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            SEEN = {}
            A:step_participant("ic_probe", function(_prev, step)
                local users = {}
                for _, m in ipairs(step.messages or {}) do
                    if m.role == "user" and not m.notification then
                        users[#users + 1] = {
                            turn_id = m.turn_id,
                            tz = m.input_context and m.input_context.timezone or nil,
                        }
                    end
                end
                SEEN[#SEEN + 1] = { step_turn_id = step.turn_id, users = users }
                return { state = { count = #SEEN } }
            end)
            "#,
        )
        .await;

        let (t1_ok, t1_turn, t1_msg_turn, t1_tz): (bool, String, String, String) = eval(
            &h,
            r#"
            local r = A:prompt("hello idle", { input_context = { timezone = "Asia/Tokyo" } })
            A:wait(r.turn_id)
            local rec = SEEN[1]
            local u = rec and rec.users[1] or {}
            return rec ~= nil and u.turn_id == r.turn_id,
                tostring(r.turn_id or ""),
                tostring(u.turn_id or ""),
                tostring(u.tz or "")
            "#,
        )
        .await;
        assert!(t1_ok, "idle prompt user message missing turn_id (turn={t1_turn}, msg_turn={t1_msg_turn})");
        assert_eq!(t1_tz, "Asia/Tokyo", "idle prompt dropped opts.input_context");

        let (t2_ok, t2_msg_turn, t2_tz): (bool, String, String) = eval(
            &h,
            r#"
            local info = S:info()
            local q = A:enqueue(
                "queued msg",
                { session_id = info.session_id, session_epoch = info.session_epoch },
                { delivery = "follow_up", input_context = { timezone = "Europe/London" } }
            )
            assert(q and q.status == "queued", "enqueue failed: " .. mare.json.encode_strict(q))
            local r = A:prompt("", { queued = true })
            A:wait(r.turn_id)
            local rec = SEEN[#SEEN]
            local u = rec and rec.users[#rec.users] or {}
            return q ~= nil and u.turn_id == r.turn_id,
                tostring(u.turn_id or ""),
                tostring(u.tz or "")
            "#,
        )
        .await;
        assert!(t2_ok, "queued user message missing turn_id (msg_turn={t2_msg_turn})");
        assert_eq!(t2_tz, "Europe/London", "queued prompt dropped input_context");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn retry_command_preserves_mentions_and_input_context_and_honors_command_override() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("README.md"), "hello readme\n").unwrap();
        let h = env.host().faux(json!([{ "text": "ans1" }, { "text": "ans2" }, { "text": "ans3" }])).build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());

        let (has_mention_1, tz_1, has_mention_2, tz_2): (bool, String, bool, String) = eval(
            &h,
            r#"
            local function last_user()
                local msgs = A:list_messages().data
                for i = #msgs, 1, -1 do
                    if msgs[i].role == "user" then return msgs[i] end
                end
            end
            local function has_mention(m, path)
                for _, b in ipairs((m and m.content) or {}) do
                    if b.type == "mention" and b.path == path then return true end
                end
                return false
            end

            local tx, rx = mare.task.channel()
            C.cordis:on("turn/completed", function(ev)
                tx:send(ev.turn_id)
            end)

            local r1 = A:prompt(
                { { type = "text", text = "inspect" }, { type = "file", path = "README.md" } },
                { input_context = { timezone = "Asia/Tokyo" } }
            )
            A:wait(r1.turn_id)
            rx:recv()

            CM:run("/retry")
            rx:recv()
            local m1 = last_user()

            CM:run("/retry", { input_context = { timezone = "America/New_York" } })
            rx:recv()
            local m2 = last_user()

            return has_mention(m1, "README.md"),
                tostring(m1 and m1.input_context and m1.input_context.timezone or ""),
                has_mention(m2, "README.md"),
                tostring(m2 and m2.input_context and m2.input_context.timezone or "")
            "#,
        )
        .await;
        assert!(has_mention_1, "/retry dropped mention block");
        assert_eq!(tz_1, "Asia/Tokyo", "/retry dropped prior input_context");
        assert!(has_mention_2, "/retry with override dropped mention block");
        assert_eq!(tz_2, "America/New_York", "/retry ignored command input.input_context override");
        h.shutdown().await;
    })
    .await
}

#[test]
fn llm_list_auth_keys_rpc_enumerates_sorted_keys_with_prefix_and_no_secrets() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    let mut rpc = RpcClient::spawn(&["--rpc", "--no-session"], &env, work.path());
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");

    let empty = rpc.call_raw("llm.list_auth_keys", json!({}), |_| json!(null));
    assert_eq!(empty["result"], json!({ "data": [] }), "{empty}");

    let auth_path = home.path().join("config/auth.json");
    std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
    std::fs::write(
        &auth_path,
        serde_json::to_string(&json!({
            "version": 1,
            "mare-plugin/secrets/STORE": { "version": 1, "kind": "explicit-secret", "value": "RAW_SECRET_VALUE" },
            "mare-plugin/secrets/OTHER": { "version": 1, "kind": "explicit-secret", "value": "RAW_OTHER_VALUE" },
            "openai": { "type": "api_key", "key": "sk-raw-openai" }
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let insecure = rpc.call_raw("llm.list_auth_keys", json!({}), |_| json!(null));
        assert_eq!(
            insecure["error"]["data"]["code"], "io",
            "llm.list_auth_keys must refuse group/world-readable auth.json: {insecure}"
        );
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let all = rpc.call_raw("llm.list_auth_keys", json!({}), |_| json!(null));
    assert_eq!(
        all["result"],
        json!({ "data": ["mare-plugin/secrets/OTHER", "mare-plugin/secrets/STORE", "openai"] }),
        "{all}"
    );
    assert!(!all.to_string().contains("RAW_SECRET_VALUE"), "must never return secret values");

    let filtered = rpc.call_raw("llm.list_auth_keys", json!({ "prefix": "mare-plugin/secrets/" }), |_| json!(null));
    assert_eq!(
        filtered["result"],
        json!({ "data": ["mare-plugin/secrets/OTHER", "mare-plugin/secrets/STORE"] }),
        "{filtered}"
    );

    let shutdown = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    assert!(shutdown.get("error").is_none(), "{shutdown}");
    rpc.wait_exit();
}

#[tokio::test]
async fn llm_provider_accessor_projects_scopes_usage_and_invalidates_replaced_generation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());

        let ok: bool = eval(
            &h,
            r#"
            local secret = "TOP_SECRET_PROVIDER_ACCESSOR_42"
            local lease = mare.projection.register(secret)
            local reference = lease.reference
            local usage_count = 0
            C.cordis:on("llm/usage", function(ev)
                if ev.provider_id == "accessor-probe" then
                    usage_count = usage_count + 1
                end
            end)

            local function assert_safe(v)
                local enc = mare.json.encode_strict(v)
                assert(not enc:find(secret, 1, true), "secret leaked into raw provider: " .. enc)
            end

            local function make_def(replace_midflight, allow_replace)
                return {
                    provider = "accessor-probe",
                    replace = allow_replace or nil,
                    models = { { model_id = "m1", context_window = 32000, max_output = 4096 } },
                    is_configured = function() return true end,
                    stream = function(req, sink)
                        assert_safe({ req.system, req.messages, req.tools, req.params })
                        assert(L:side_requests_pending(S:info().session_id), "direct provider call missing side_requests_pending")
                        if replace_midflight then
                            assert(L:register_provider(make_def(false, true)))
                            assert(req.cancel and req.cancel:is_cancelled(), "provider replacement must cancel in-flight req.cancel")
                        end
                        if sink then
                            sink({ type = "text_delta", id = "d1", delta = secret:sub(1, 12) })
                            sink({ type = "text_delta", id = "d1", delta = secret:sub(13) })
                        end
                        return {
                            role = "assistant",
                            content = { { type = "text", text = secret } },
                            stop_reason = "stop",
                            usage = { input = 1, output = 1 },
                        }
                    end,
                    compact = function(req)
                        assert_safe({ req.system, req.messages, req.tools, req.params })
                        assert(L:side_requests_pending(S:info().session_id), "direct compact call missing side_requests_pending")
                        return { items = { { type = "text", text = secret } }, usage = { input = 1, output = 1 } }
                    end,
                }
            end

            local raw_def = make_def(false, false)
            assert(L:register_provider(raw_def))
            local direct = assert(L:provider("accessor-probe"))
            assert(not rawequal(direct.stream, raw_def.stream), "llm:provider must wrap stream instead of returning raw table")

            local base_req = {
                model = direct.models[1],
                system = secret,
                messages = { { role = "user", content = { { type = "text", text = secret } } } },
                purpose = "accessor",
            }
            local streamed = {}
            local reply = direct.stream(base_req, function(ev)
                assert_safe(ev)
                if ev.type == "text_delta" then streamed[#streamed + 1] = ev.delta end
            end)
            assert(reply.content[1].text == reference, "stream reply not projected")
            assert(table.concat(streamed) == reference, "stream deltas not projected")

            local compacted = direct.compact(base_req)
            assert(compacted.items[1].text == reference, "compact reply not projected")
            assert(usage_count == 2, "expected 2 usage events, got " .. tostring(usage_count))

            -- Transparent delegation through accessor assignment must not duplicate usage scope.
            local original = direct.stream
            direct.stream = function(req, sink)
                return original(req, sink)
            end
            assert(L:provider("accessor-probe").stream ~= original, "reassigned accessor.stream must be visible")
            local delegated = {}
            local reply2 = L:provider("accessor-probe").stream(base_req, function(ev)
                assert_safe(ev)
                if ev.type == "text_delta" then delegated[#delegated + 1] = ev.delta end
            end)
            assert(reply2.content[1].text == reference and table.concat(delegated) == reference)
            assert(usage_count == 3, "transparent delegate duplicated usage scope: " .. tostring(usage_count))

            -- Replacing the provider invalidates the old accessor generation and cancels in-flight calls
            -- without cancelling the caller's own cancel token.
            assert(L:register_provider(make_def(true, true)))
            assert(not pcall(direct.stream, base_req), "old provider generation must be rejected")

            local replacement = assert(L:provider("accessor-probe"))
            local caller_cancel = mare.task.token()
            local req3 = table.copy(base_req)
            req3.cancel = caller_cancel
            req3.purpose = "agent"
            assert(not pcall(replacement.stream, req3), "withdrawn in-flight provider must fail")
            assert(not caller_cancel:is_cancelled(), "provider withdrawal must not cancel caller token")
            assert(not L:side_requests_pending(S:info().session_id), "side_requests_pending leaked after withdrawal")

            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn llm_guards_accessor_schema_validate_params_log_and_wire_history() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local L = C.cordis:get("llm")
            local A = C.cordis:get("agent")
            local lib = require("mare_lib")

            -- Dead helpers removed from mare_lib; session drains live on the session service.
            assert(lib.flag_guard == nil, "mare_lib.flag_guard must be removed")
            assert(lib.on_session_drain == nil, "mare_lib.on_session_drain must be removed")
            assert(lib.run_session_drains == nil, "mare_lib.run_session_drains must be removed")
            assert(lib.mutex().with_spawned == nil, "mutex:with_spawned must be removed")
            assert(type(S.on_drain) == "function", "session:on_drain service method must exist")

            -- Agent session revision tracks pending notes mutations.
            local rev = A:session_revision()
            assert(A:check_session_revision(rev) == true, "unchanged session revision must match")
            A:notify("mid-switch notice", { wake = false })
            assert(A:check_session_revision(rev) == false, "A.notes mutation must invalidate session_revision")

            -- tool_update = false is accepted by provider/model validation.
            local entered_tx, entered_rx = mare.task.channel()
            local block_tx, block_rx = mare.task.channel()
            local abort_mode = true
            local usage_events = {}
            C.cordis:on("llm/usage", function(ev)
                usage_events[#usage_events + 1] = ev
            end)
            assert(L:register_provider({
                provider = "probe-guard",
                label = "Guard Probe",
                no_key = true,
                tool_update = false,
                models = {
                    {
                        model_id = "m1",
                        name = "M1",
                        context_window = 16000,
                        max_output = 4096,
                        reasoning = true,
                        tool_update = false,
                        efforts = { "low", "medium" },
                        default_effort = "medium",
                    },
                },
                stream = function()
                    if abort_mode then
                        entered_tx:send(true)
                        block_rx:recv()
                    end
                    return {
                        role = "assistant",
                        content = mare.json.array({ { type = "text", text = "ok-after-aborts" } }),
                        stop_reason = "stop",
                    }
                end,
                compact = function()
                    return nil, "compact-boom", "invalid", { input = 11, output = 4 }, true
                end,
            }))
            local m = assert(L:resolve("probe-guard/m1"))

            -- Invalid output_schema returns a JSON array content + model_id/provider_id.
            local bad_schema = L:stream({
                model = m,
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                output_schema = { type = 12345 },
            }, function() end)
            assert(bad_schema.stop_reason == "error" and bad_schema.error_kind == "invalid", "expected invalid output_schema error")
            assert(mare.json.is_array(bad_schema.content), "invalid output_schema content must be a JSON array")
            assert(bad_schema.model_id == "m1" and bad_schema.provider_id == "probe-guard", "must use model_id/provider_id")
            assert(bad_schema.model == nil and bad_schema.provider == nil, "must not emit legacy model/provider keys")

            -- provider_accessor.compact preserves all return values and emits failed usage.
            local direct = assert(L:provider("probe-guard"))
            local before_usage = #usage_events
            local cres, cerr, ckind, cusage, csub = direct.compact({
                model = m,
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            })
            assert(cres == nil and cerr == "compact-boom" and ckind == "invalid", "compact must forward nil, err, kind")
            assert(type(cusage) == "table" and cusage.input == 11 and cusage.output == 4, "compact must forward failed usage")
            assert(csub == true, "compact must forward subscription flag")
            assert(#usage_events == before_usage + 1, "failed compact usage must be emitted via llm/usage")

            -- Aborting llm:stream via select 4 times must release crash guard and avoid false crash_limit.
            local base_req = {
                model = m,
                messages = { { role = "user", content = { { type = "text", text = "abort-me" } } } },
            }
            for _ = 1, 4 do
                local cancel_tx, cancel_rx = mare.task.channel()
                local done_tx, done_rx = mare.task.channel()
                C.cordis:spawn(function()
                    mare.task.select(
                        function() return L:stream(base_req, function() end) end,
                        function() return cancel_rx:recv() end
                    )
                    done_tx:send(true)
                end)
                assert(mare.task.timeout(5000, function() return entered_rx:recv() end), "stream did not start")
                cancel_tx:send(true)
                assert(mare.task.timeout(5000, function() return done_rx:recv() end), "aborted stream did not exit")
            end
            abort_mode = false
            local after_aborts = L:stream(base_req, function() end)
            assert(after_aborts.stop_reason == "stop", "aborted streams must not trigger crash_limit: " .. tostring(after_aborts.error))

            -- params_log fast_unloaded must return adjustments already recorded before params_api loads.
            assert(L:set_model("probe-guard/m1", { save = false }))
            assert(L:set_effort("high", { save = false }))
            local clamped_req = {
                model = m,
                messages = { { role = "user", content = { { type = "text", text = "clamp-me" } } } },
            }
            local clamped_res = L:stream(clamped_req, function() end)
            assert(clamped_res.stop_reason == "stop")
            local log_entries = L:params_log()
            assert(type(log_entries) == "table" and #log_entries > 0, "params_log must expose adjustments recorded before params_api loads")

            -- transform_messages downgrades dotted extension blocks on same-provider wire switch.
            local hist = {
                {
                    role = "assistant",
                    provider_id = "probe-guard",
                    model_id = "m1",
                    content = {
                        { type = "wire-a.card", text = "fallback-text" },
                        { type = "wire-a.opaque" },
                    },
                },
            }
            local same_wire = L:transform_messages(hist, m, nil, nil, nil, nil, "wire-a")
            assert(#same_wire[1].content == 2 and same_wire[1].content[1].type == "wire-a.card", "same wire keeps dotted blocks")
            local diff_wire = L:transform_messages(hist, m, nil, nil, nil, nil, "wire-b")
            assert(#diff_wire[1].content == 1 and diff_wire[1].content[1].type == "text" and diff_wire[1].content[1].text == "fallback-text",
                "wire switch must downgrade dotted extension blocks")

            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

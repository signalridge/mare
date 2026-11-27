//! Regression test: `tool/result` listener exceptions must not leak raw internal
//! error text or stack traces into model-visible tool failure content.

use crate::common::*;

#[tokio::test]
async fn tool_result_listener_error_does_not_leak_raw_exception_text() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (is_error, error_code, content): (bool, String, String) = eval(
            &h,
            r#"
            C.cordis:on("tool/result", function(data, ev)
                if data.call.name == "sanitize-probe" then
                    error("INTERNAL_LISTENER_SECRET: token=secret-123\nstack traceback:\n\tinternal.lua:42")
                end
                return ev.next()
            end)
            C.cordis:get("tools"):register("sanitize-probe", {
                description = "test",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                idempotent = true,
                execute = function() return { content = "ok" } end,
            })
            local r = C.cordis:get("tools"):execute({ id = "sanitize-1", name = "sanitize-probe", arguments = {} }, {})
            return r.is_error == true, tostring(r.error_code), tostring(r.content)
        "#,
        )
        .await;
        assert!(is_error, "throwing in tool/result must fail closed");
        assert_eq!(error_code, "tool_result_failed");
        assert_eq!(content, "Tool result post-processing failed.");
        assert!(
            !content.contains("INTERNAL_LISTENER_SECRET")
                && !content.contains("secret-123")
                && !content.contains("stack traceback"),
            "raw listener exception text leaked into tool/result content: {content}"
        );
        h.shutdown().await;
    })
    .await
}

// Like reference implementation post-execute around listener: packages/core/agent-loop/tests/interception.spec.ts:761.
#[tokio::test]
async fn native_user_tool_after_observes_and_rewrites_a_faux_tool_result() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("after-probe.txt"), "original tool result").unwrap();
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("native-probe.lua"),
            r#"return { name = "native-probe", inject = { "agent" }, apply = function(ctx)
  mare._after_count = 0
  mare._request_count = 0
  ctx:on("request/build", function(req, ev)
    mare._request_count = mare._request_count + 1
    return ev.next(req)
  end)
  ctx:on("tool/after", function(data, ev)
    mare._after_count = mare._after_count + 1
    mare._after_original = data.result.content
    mare._after_tool = data.call.name
    mare._after_op_type = type(data.operation)
    data.result = "rewritten tool result"
    return ev.next(data)
  end)
end }"#,
        )
        .unwrap();
        let script = serde_json::json!([
            { "tool_calls": [{ "id": "after-1", "name": "read", "arguments": { "path": "after-probe.txt" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .faux(script)
            .config(serde_json::json!({ "plugins": { "permissions": { "enabled": true } } }))
            .user_plugins()
            .no_session()
            .build()
            .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let turn = h.prompt("go", serde_json::json!({})).await.unwrap();
        assert!(turn["error"].is_null(), "{turn}");
        let mut tool_result = None;
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event, .. } = rec
                && event.name == "item/completed"
                && event.payload["item"]["type"] == "tool_call"
            {
                assert!(tool_result.is_none(), "expected one tool result");
                tool_result = Some(event.payload["item"].clone());
            }
        }
        let tool_result = tool_result.expect("the faux turn must execute the tool");
        assert_eq!(tool_result["name"], "read", "{tool_result}");
        assert_eq!(tool_result["result_status"], "success", "{tool_result}");
        let (requests, count, original, name, op_type): (i64, i64, String, String, String) = eval(
            &h,
            r#"return mare._request_count or 0, mare._after_count or 0, tostring(mare._after_original),
                     tostring(mare._after_tool), tostring(mare._after_op_type)"#,
        )
        .await;
        assert_eq!(requests, 2, "the active native plugin must receive request/build");
        assert_eq!(count, 1, "the native tool/after listener must run exactly once");
        assert!(original.contains("original tool result"), "{original}");
        assert_eq!(name, "read");
        assert_eq!(op_type, "table");
        assert!(tool_result["message"]["content"].to_string().contains("rewritten tool result"), "{tool_result}");
        h.shutdown().await;
    })
    .await;
}

/// The permissions and context listeners pass `tool/after`'s full arguments (result, call, operation)
/// on to a later listener, which gets the rewritten result and the same call and operation.
#[tokio::test]
async fn native_tool_after_listeners_pass_the_full_arguments_on() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(serde_json::json!({ "plugins": {
                "permissions": { "enabled": true }, "context": { "enabled": true }
            } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let (calls, call_id, op_budget, content): (i64, String, i64, String) = eval(
            &h,
            r#"
            local calls, seen_id, seen_budget = 0, "", -1
            C.cordis:on("tool/after", function(data, ev)
                calls = calls + 1
                seen_id = data.call.id
                seen_budget = data.operation.max_bytes
                assert(data.result.content == "original")
                data.result = { content = "rewritten result" }
                return ev.next(data)
            end)
            local out = C.cordis:get("bus"):waterfall("tool/after", {
                result = { content = "original" },
                call = { id = "full-1", name = "read" },
                operation = { max_bytes = 4096 },
            })
            return calls, seen_id, seen_budget, out.result.content
            "#,
        )
        .await;
        assert_eq!(calls, 1, "the later listener must be reached through the built-in listeners");
        assert_eq!(call_id, "full-1");
        assert_eq!(op_budget, 4096, "the operation must be passed on unchanged");
        assert_eq!(content, "rewritten result");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn tools_capabilities_and_result_pipeline_hooks_dispatch() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("cap-probe.txt"), "hello-pipeline").unwrap();
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("caps-probe.lua"),
            r#"return { name = "caps-probe", inject = { "tools", "agent" }, apply = function(ctx)
                mare._tools_caps = ctx.tools:capabilities()
                mare._program_results = {}
                mare._after_results = {}
                ctx:on("tool/program-result", function(data, ev)
                    mare._program_results[#mare._program_results + 1] = {
                        name = data.call.name,
                        content = data.result.content,
                    }
                    data.result.content = tostring(data.result.content) .. "+prog"
                    return ev.next(data)
                end)
                ctx:on("tool/after", function(data, ev)
                    data.result.content = tostring(data.result.content) .. "+after"
                    mare._after_results[#mare._after_results + 1] = {
                        name = data.call.name,
                        content = data.result.content,
                    }
                    return ev.next(data)
                end)
            end }"#,
        )
        .unwrap();
        let script = serde_json::json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "cap-probe.txt" } }] },
            { "text": "done" }
        ]);
        let h = env.host().user_plugins().faux(script).no_session().build().await;
        expose(&h);
        let cap_ver: i64 = eval(&h, "return mare._tools_caps and mare._tools_caps.result_pipeline or 0").await;
        assert_eq!(cap_ver, 1, "ctx.tools:capabilities().result_pipeline must equal 1");

        let res = h.prompt("run tool", serde_json::json!({})).await.unwrap();
        assert_eq!(res["text"], "done", "{res}");

        let (prog_count, after_count, prog_name, after_name, after_content): (i64, i64, String, String, String) =
            eval(
                &h,
                r#"
                local p = mare._program_results[1] or {}
                local a = mare._after_results[1] or {}
                return #mare._program_results, #mare._after_results, tostring(p.name), tostring(a.name), tostring(a.content)
                "#,
            )
            .await;
        assert_eq!(prog_count, 1);
        assert_eq!(after_count, 1);
        assert_eq!(prog_name, "read");
        assert_eq!(after_name, "read");
        assert!(after_content.ends_with("+prog+after"), "expected pipeline order: {after_content}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn legacy_tools_provider_without_capabilities_registers_cleanly() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("legacy-tools.lua"),
            r#"
            return {
                name = "legacy-tools",
                override = true,
                provide = { "tools" },
                apply = function(ctx)
                    local svc = {
                        register = function() return function() end end,
                        snippets = function() return {} end,
                        shadow = function() end,
                        command = function() end,
                        get = function() return nil end,
                        list = function() return {} end,
                        definitions = function() return {} end,
                        bind_declarations = function() end,
                        execute = function() return { content = "ok" } end,
                        edit_mode = function() return "full" end,
                        observe = function() return function() end end,
                        observation = function() return nil end,
                        fs_guard = function() return nil end,
                        fs_commit = function() end,
                        with_file_lock = function(_, _, _, fn) return fn() end,
                        readonly = function() return false end,
                        side_effect = function() return "read" end,
                        disable = function() end,
                        enable = function() end,
                        shell_command = function() return nil end,
                        shell = function() return {} end,
                        process_env = function() return {} end,
                        register_executor = function() return function() end end,
                        touch = function() end,
                        concurrency_safe = function() return true end,
                        cwd = function() return mare.cwd end,
                        revision = function() return 1 end,
                        has_dynamic_admission = function() return false end,
                        invalidate = function() end,
                        is_disabled = function() return false end,
                    }
                    require("mare_lib").service(ctx, "tools", svc)
                end,
            }
            "#,
        )
        .unwrap();
        std::fs::write(env.home.path().join("config/init.lua"), "mare.o.plugins = { tools = { enabled = false } }\n")
            .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        assert!(
            h.kernel.problems().is_empty(),
            "legacy tools provider must register cleanly: {:?}",
            h.kernel.problems()
        );
        assert!(h.kernel.service("tools").is_some());
        let has_caps: bool = eval(&h, "return C.cordis:get('tools').capabilities ~= nil").await;
        assert!(!has_caps, "omitted optional capabilities method must remain nil");
        h.shutdown().await;
    })
    .await;
}

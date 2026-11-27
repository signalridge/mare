use super::*;

#[tokio::test]
async fn never_precedes_custom_answerers_and_ask_still_executes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs", "tools-apply-patch"])
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "full-access" },
                    "approvals": { "policy": "never" }
                }
            }))
            .project_trusted(true)
            .build_exposed()
            .await;
        exec(&h, SETUP).await;
        let (refused, before, after, prompts, audit): (bool, i64, i64, i64, i64) = eval(
            &h,
            r#"
            local blocked = RUN()
            local before = ran
            Q:set("ask")
            local allowed = RUN()
            assert(allowed.content == "ran")
            return blocked.refused == true, before, ran, answered, #S:list_records("approval/resolved").data
        "#,
        )
        .await;
        assert!(refused);
        assert_eq!((before, after, prompts, audit), (0, 1, 1, 2));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reviewer_is_per_call_detached_and_denial_never_falls_back() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({"plugins": {
                "permissions":{"preset":"full-access"},
                "approvals":{"reviewer":"fixture","fallback":true}
            }}))
            .build()
            .await;
        expose(&h);
        exec(&h, SETUP).await;
        let (ran, answered, reviewed): (i64, i64, i64) = eval(
            &h,
            r#"
            Q:register_reviewer("fixture", function(action)
                reviewed = reviewed + 1
                assert(action.call.snapshot and action.schema.type == "object")
                action.call.name = "tampered"
                return {decision=reviewed == 1 and "allow" or "deny", reason="specific review"}
            end)
            assert(RUN().content == "ran")
            assert(RUN().refused)
            return ran, answered, reviewed
        "#,
        )
        .await;
        assert_eq!((ran, answered, reviewed), (1, 0, 2));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reviewer_timeout_invalid_result_unload_and_cancel_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().config(json!({"plugins": {
            "permissions":{"preset":"full-access"},
            "approvals":{"reviewer":"fixture","review_timeout_ms":20}
        }})).build().await;
        expose(&h);
        exec(&h, SETUP).await;
        let (ran, answered, outcomes): (i64, i64, i64) = eval(&h, r#"
            local remove = Q:register_reviewer("fixture", function() mare.task.sleep(10000); return {decision="allow"} end)
            assert(RUN().refused)
            remove()
            remove = Q:register_reviewer("fixture", function() return {decision="allow_session"} end)
            assert(RUN().refused)
            remove()
            assert(RUN().refused)
            remove = Q:register_reviewer("fixture", function() reviewed=reviewed+1; return {decision="allow"} end)
            local token = mare.task.token(); token:cancel()
            local result = Q:request("tool", {}, function() return {decision="ask"} end, function() error("must not ask") end, token)
            assert(result.decision == "deny" and reviewed == 0)
            return ran, answered, #S:list_records("approval/resolved").data
        "#).await;
        assert_eq!((ran, answered, outcomes), (0, 0, 4));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn granular_policy_has_closed_categories_and_never_changes_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let (ran, answered, network_denied, invalid): (i64, i64, bool, bool) = eval(&h, r#"
            local sandbox = mare.json.encode(mare.proc.sandbox.current())
            Q:set({mode="granular",categories={tool=true}})
            assert(RUN().content == "ran")
            local network = Q:request("network", {host="example.invalid",port=443}, function() return {decision="ask"} end, function() error("must not ask") end)
            local invalid = not pcall(function() Q:set("allow-all") end)
            Q:set("never")
            assert(mare.json.encode(mare.proc.sandbox.current()) == sandbox)
            return ran, answered, network.decision == "deny", invalid
        "#).await;
        assert_eq!((ran, answered), (1, 1));
        assert!(network_denied && invalid);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn model_reviewer_uses_real_llm_request_without_tools() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().config(json!({"plugins": {
            "permissions":{"preset":"full-access"}, "approvals":{"reviewer":"model"}
        }})).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, SETUP).await;
        let (ran, answered, reviewed): (i64, i64, i64) = eval(&h, r#"
            L:register_provider({provider="approval-fixture", is_configured=function() return true end,
                models={{model_id="m",context_window=100000,max_output=1024}},
                stream=function(req)
                    reviewed=reviewed+1
                    assert(#req.tools == 0 and req.system:find("untrusted evidence",1,true))
                    local action=mare.json.decode(req.messages[1].content[1].text)
                    assert(action.call.name == "effect" and action.schema.type == "object")
                    return {role="assistant",stop_reason="stop",content={{type="text",
                        text=reviewed == 1 and '{"decision":"allow","reason":"authorized"}' or '{"decision":"deny","reason":"outside scope"}'}}}
                end})
            L:set_model("approval-fixture/m",{save=false})
            assert(RUN().content == "ran")
            assert(RUN().refused)
            return ran,answered,reviewed
        "#).await;
        assert_eq!((ran, answered, reviewed), (1, 0, 2));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn model_reviewer_rejects_allow_json_from_incomplete_or_failed_responses() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({"plugins": {
                "permissions":{"preset":"full-access"}, "approvals":{"reviewer":"model"}
            }}))
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, SETUP).await;
        exec(
            &h,
            r#"
            L:register_provider({provider="incomplete-review", is_configured=function() return true end,
                models={{model_id="m",context_window=100000,max_output=1024}},
                stream=function(req)
                    reviewed=reviewed+1
                    assert(#req.tools == 0)
                    return {role="assistant",stop_reason=REVIEW_STOP,content={{type="text",
                        text='{"decision":"allow","reason":"authorized"}'}}}
                end})
            L:set_model("incomplete-review/m",{save=false})
            for _, reason in ipairs({"length", "aborted", "filtered", "paused", "other", "error", "tool_use"}) do
                REVIEW_STOP = reason
                assert(RUN().refused == true, "unfinished reviewer authorized an action: " .. reason)
                assert(ran == 0 and answered == 0)
            end
            REVIEW_STOP = "stop"
            assert(RUN().content == "ran")
            assert(ran == 1 and answered == 0 and reviewed == 8)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn durable_policy_survives_restart_and_new_session_resets_default() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"ready"}])).build().await;
        expose(&h);
        h.prompt("start session", json!({})).await.unwrap();
        exec(&h, "C.cordis:get('approvals'):set('never')").await;
        h.shutdown().await;
        let h = env.host().faux(json!([])).resume().build().await;
        expose(&h);
        exec(&h, SETUP).await;
        let (restored, denied, reset): (String, bool, String) = eval(
            &h,
            r#"
            local restored = Q:policy()
            local denied = RUN().refused == true
            S:create()
            return restored,denied,Q:policy()
        "#,
        )
        .await;
        assert_eq!((restored.as_str(), denied, reset.as_str()), ("never", true, "ask"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn inherited_policy_chain_cannot_be_relaxed_by_child_override() {
    local(async {
        let env = TestEnv::new().await;
        let h =
            host(&env, true, json!({"preset":"full-access"}), json!({ "approval_policy_ceilings":["never"] })).await;
        exec(&h, SETUP).await;
        let (denied, inherited): (bool, String) = eval(
            &h,
            r#"
            Q:set("ask")
            local result=RUN()
            local inheritance=Q:inheritance()
            return result.refused == true, inheritance[1]
        "#,
        )
        .await;
        assert!(denied);
        assert_eq!(inherited, "never");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn untrusted_requires_approval_for_effects_but_keeps_reads() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs", "tools-apply-patch"])
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "full-access" },
                    "approvals": { "policy": "untrusted" }
                }
            }))
            .project_trusted(true)
            .build_exposed()
            .await;
        exec(&h, SETUP).await;
        let (safe, effect, answered): (String, bool, i64) = eval(
            &h,
            r#"
            T:register("safe_read",{description="read",side_effect="read",parameters={type="object"},
                execute=function() return {content="read"} end})
            local safe=EXECUTE({id="read",name="safe_read",arguments={}}, {})
            local effect=EXECUTE({id="effect",name="effect",arguments={}}, {})
            return safe.content,effect.refused == true,answered
        "#,
        )
        .await;
        assert_eq!((safe.as_str(), effect, answered), ("read", true, 0));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn real_child_uses_parents_live_policy_after_delegation() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env,true,json!({"preset":"read-only"}),json!({"no_session":true})).await;
        exec(&h, r#"
            Q=C.cordis:get("approvals")
            answered=0
            C.cordis:get("policy"):voter("tool/approval",function() answered=answered+1; return {decision="allow"} end)
        "#).await;
        let (messages,answered): (String,i64)=eval(&h,r#"
            local script={
                {text=string.rep("waiting ",20),delay_ms=20,tool_calls={{name="write",arguments={path="child-policy.txt",content="must not run"}}}},
                {text="done"}
            }
            local child=mare.subagent.start({prompt="write",no_session=true,approval_policy="ask",
                config={model="faux/echo",plugins={approvals={policy="ask"},
                    ["provider-faux"]={script_json=mare.json.encode(script)},permissions={enabled=true}}}})
            Q:set("never")
            local result=child:wait()
            return mare.json.encode(result.messages),answered
        "#).await;
        assert_eq!(answered,0);
        assert!(!env.cwd.join("child-policy.txt").exists());
        assert!(messages.contains("approval policy"),"{messages}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancelling_pending_reviewer_withdraws_and_discards_late_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({"plugins":{
                "permissions":{"preset":"full-access"},"approvals":{"reviewer":"fixture"}
            }}))
            .build()
            .await;
        expose(&h);
        exec(&h, SETUP).await;
        let (decision, completed): (String, i64) = eval(
            &h,
            r#"
            completed=0
            Q:register_reviewer("fixture",function()
                mare.task.sleep(1000)
                completed=completed+1
                return {decision="allow"}
            end)
            local cancel=mare.task.token()
            local which,result=mare.task.select(function()
                return Q:request("tool",{},function() return {decision="ask"} end,function() error("must not prompt") end,cancel)
            end,function()
                mare.task.sleep(10);cancel:cancel();mare.task.sleep(1000)
            end)
            assert(which==1)
            return result.decision,completed
        "#,
        )
        .await;
        assert_eq!((decision.as_str(), completed), ("deny", 0));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sandbox_category_and_permission_request_metadata_are_enforced() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"read-only"}), json!({})).await;
        exec(&h, SETUP).await;
        let (blocked, written, permission_denied): (bool, String, bool) = eval(
            &h,
            r#"
            Q:set({mode="granular",categories={tool=true}})
            local call={id="write",name="write",arguments={path="category.txt",content="approved"}}
            local blocked=EXECUTE(call,{approve=function() error("sandbox category disabled") end})
            Q:set({mode="granular",categories={tool=true,sandbox=true}})
            local allowed=EXECUTE(call,{approve=function() return {decision="allow"} end})
            assert(not allowed.is_error,allowed.content)
            T:register("effect",{description="permission request",permission_request=true,
                parameters={type="object"},execute=function() error("permission category disabled") end})
            local permissions=RUN()
            return blocked.refused == true,mare.fs.read("category.txt"),permissions.refused == true
        "#,
        )
        .await;
        assert!(blocked && permission_denied);
        assert_eq!(written, "approved");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn review_all_admission_and_read_operations_use_same_reviewer() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({"plugins":{
                "permissions":{"preset":"full-access"},"approvals":{"reviewer":"fixture","review_all":true}
            }}))
            .build()
            .await;
        expose(&h);
        exec(&h, SETUP).await;
        let (blocked, allowed, reviewed): (bool, String, i64) = eval(
            &h,
            r#"
            local ready=false
            Q:register_reviewer("fixture",function() reviewed=reviewed+1;return {decision="allow"} end,
                {admit=function() return ready,"review not ready" end})
            T:register("safe_read",{description="read",side_effect="read",parameters={type="object"},
                execute=function() return {content="read"} end})
            local function read() return EXECUTE({id="read",name="safe_read",arguments={}}, {}) end
            local blocked=read()
            assert(reviewed==0)
            ready=true
            local allowed=read()
            return blocked.refused == true,allowed.content,reviewed
        "#,
        )
        .await;
        assert!(blocked);
        assert_eq!((allowed.as_str(), reviewed), ("read", 1));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_invalid_review_model_does_not_fall_back_to_main_model() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({"plugins":{
                "permissions":{"preset":"full-access"},"approvals":{"reviewer":"model"},
                "approval-review-model":{"model":"missing-review-provider/unknown"}
            }}))
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, SETUP).await;
        let (blocked, reviewed): (bool, i64) = eval(
            &h,
            r#"
            L:register_provider({provider="main-only",is_configured=function() return true end,
                models={{model_id="m",context_window=100000,max_output=1024}},stream=function()
                    reviewed=reviewed+1
                    return {role="assistant",stop_reason="stop",content={{type="text",text='{"decision":"allow"}'}}}
                end})
            L:set_model("main-only/m",{save=false})
            return RUN().refused == true,reviewed
        "#,
        )
        .await;
        assert!(blocked);
        assert_eq!(reviewed, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_cannot_remove_inherited_review_by_disabling_provider() {
    local(async {
        let env=TestEnv::new().await;
        let h=host(&env,true,json!({"preset":"full-access"}),json!({"no_session":true})).await;
        exec(&h,"C.cordis:get('approvals'):set('never')").await;
        let result:String=eval(&h,r#"
            local script={{tool_calls={{name="write",arguments={path="disabled-approval.txt",content="must not run"}}}},{text="done"}}
            local child=mare.subagent.start({prompt="write",no_session=true,
                config={model="faux/echo",plugins={approvals={enabled=false},permissions={enabled=false},
                    ["provider-faux"]={script_json=mare.json.encode(script)}}}})
            return mare.json.encode(child:wait())
        "#).await;
        assert!(result.contains("required service") && result.contains("approvals"),"{result}");
        assert!(!env.cwd.join("disabled-approval.txt").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn nested_policy_categories_are_all_required_and_invalid_arrays_deny() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"read-only"}), json!({})).await;
        exec(&h, SETUP).await;
        let (denied, allowed, bad): (bool, bool, String) = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("permission/verdict",function() return {decision="ask",category="organization"} end)
            Q:set({mode="granular",categories={tool=true,sandbox=true}})
            local call={id="nested",name="write",arguments={path="nested.txt",content="approved"}}
            local denied=EXECUTE(call,{approve=function() error("organization category disabled") end})
            Q:set({mode="granular",categories={tool=true,sandbox=true,organization=true}})
            local allowed=EXECUTE(call,{approve=function() return {decision="allow"} end})
            -- a categories list that is not a string array is an invalid vote: deny (tool/before,
            -- where the permissions plugin's asks carry `categories`); the control shows the
            -- same call is not denied without it
            local probe={name="categories_probe",arguments={}}
            local control=C.cordis:get("policy"):decide("tool/before",{ probe }).decision
            assert(control ~= "deny", "control: " .. tostring(control))
            local off_bad=C.cordis:get("policy"):voter("tool/before",function() return {decision="ask",categories={value="invalid"}} end)
            local bad=C.cordis:get("policy"):decide("tool/before",{ probe }).decision
            off_bad()
            return denied.refused == true,not allowed.is_error,bad
        "#,
        )
        .await;
        assert!(denied && allowed);
        assert_eq!(bad, "deny");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_cli_never_overrides_saved_ask_on_resume() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"ready"}])).build().await;
        expose(&h);
        h.prompt("start", json!({})).await.unwrap();
        exec(&h, "C.cordis:get('approvals'):set('ask')").await;
        h.shutdown().await;
        let h = env
            .host()
            .faux(json!([]))
            .config(json!({ "plugins": { "approvals": { "policy": "never" } } }))
            .args(json!({ "continue": true, "cli_flag_paths": ["plugins.approvals.policy"] }))
            .build()
            .await;
        expose(&h);
        exec(&h, SETUP).await;
        let (policy, denied): (String, bool) = eval(&h, "return Q:policy(),RUN().refused == true").await;
        assert_eq!(policy, "never");
        assert!(denied);
        h.shutdown().await;
    })
    .await
}

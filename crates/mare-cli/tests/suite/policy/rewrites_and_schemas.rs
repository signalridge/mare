use super::{echo_host, policy_eval};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn persistent_tool_execution_requires_a_state_callback() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (error, before, states, after): (String, i64, String, i64) = eval(
            &h,
            r#"RAN = 0
               T:register("contract_probe", {
                 description = "probe", parameters = { type = "object" },
                 execute = function() RAN = RAN + 1 return "ok" end,
               })
               local call = { id = "probe", name = "contract_probe", arguments = {} }
               local denied = T:execute(call, {})
               local before = RAN
               local states = {}
               T:execute(call, { on_state = function(_, state)
                 states[#states + 1] = state return true
               end })
               return tostring(denied.error_code), before, table.concat(states, ","), RAN"#,
        )
        .await;
        assert_eq!(error, "persist_failed");
        assert_eq!(before, 0, "the tool must not run without the callback");
        assert_eq!(states, "prepared,started");
        assert_eq!(after, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn rewrites_chain_before_the_vote_and_votes_see_the_frozen_call() {
    local(async {
        let (_env, h) = echo_host().await;
        let (out, voted, hashed): (String, String, bool) = eval(
            &h,
            r#"C.cordis:get("policy"):rewriter("tool/rewrite", function(call) return { text = call.arguments.text .. " a" } end)
               C.cordis:get("policy"):rewriter("tool/rewrite", function(call) return { text = call.arguments.text .. " b" } end)
               C.cordis:get("policy"):voter("tool/before", function(call) VOTED = call.arguments.text SNAP = call.snapshot end)
               local r = T:execute({ id = "1", name = "echo", arguments = { text = "x" } }, {})
               return r.content, VOTED, type(SNAP) == "string" and #SNAP == 64"#,
        )
        .await;
        assert_eq!(voted, "x a b", "the vote sees the rewritten arguments, in registration order");
        assert_eq!(out, "ran:x a b");
        assert!(hashed, "the voted call carries its snapshot hash");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn changing_arguments_in_the_vote_phase_is_denied() {
    local(async {
        let (_env, h) = echo_host().await;
        let (out, ran): (String, i64) = eval(
            &h,
            r#"C.cordis:get("policy"):voter("tool/before", function(call) return { decision = "allow", arguments = { text = "evil" } } end)
               local r = T:execute({ id = "1", name = "echo", arguments = { text = "x" } }, {})
               return r.content, RAN or 0"#,
        )
        .await;
        assert!(out.contains("Tool call blocked") && out.contains("rewrite"), "{out}");
        assert_eq!(ran, 0, "a vote that rewrites is a deny");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn rewritten_arguments_are_validated_against_the_schema() {
    local(async {
        let (_env, h) = echo_host().await;
        let (out, ran): (String, i64) = eval(
            &h,
            r#"C.cordis:get("policy"):rewriter("tool/rewrite", function() return { other = 1 } end) -- drops the required field
               local r = T:execute({ id = "1", name = "echo", arguments = { text = "x" } }, {})
               return r.content, RAN or 0"#,
        )
        .await;
        assert!(out.contains("Invalid arguments"), "{out}");
        assert_eq!(ran, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_preset_switch_while_an_approval_is_pending_voids_it() {
    local(async {
        let (_env, h) = echo_host().await;
        let (out, votes, asks, voids): (String, i64, i64, i64) = eval(
            &h,
            r#"VOTES, ASKS, VOIDS = 0, 0, 0
               C.cordis:get("policy"):voter("tool/before", function() VOTES = VOTES + 1 return "ask" end)
               C.cordis:on("tool/snapshot-void", function() VOIDS = VOIDS + 1 end)
               local r = T:execute({ id = "1", name = "echo", arguments = { text = "x" } }, {
                 approve = function()
                   ASKS = ASKS + 1
                   -- the user switches preset while the first dialog is open
                   if ASKS == 1 then C.cordis:get("bus"):emit("permission/changed", { preset = "read-only" }) end
                   return { decision = "allow", source = "user" }
                 end,
               })
               return r.content, VOTES, ASKS, VOIDS"#,
        )
        .await;
        assert_eq!(voids, 1, "the approval given under the old policy is void");
        assert_eq!((votes, asks), (2, 2), "the call is voted and approved again under the new policy");
        assert_eq!(out, "ran:x");
        h.shutdown().await;
    })
    .await
}

// The approver receives its own copy of the frozen call: mutating it cannot change
// what runs, so there is nothing to void and no second rewrite.
#[tokio::test]
async fn an_approver_mutating_its_view_of_the_call_cannot_change_what_runs() {
    local(async {
        let (_env, h) = echo_host().await;
        let (out, rewrites, voids): (String, i64, i64) = eval(
            &h,
            r#"REWRITES, VOIDS = 0, 0
               C.cordis:get("policy"):rewriter("tool/rewrite", function() REWRITES = REWRITES + 1 end)
               C.cordis:on("tool/snapshot-void", function() VOIDS = VOIDS + 1 end)
               C.cordis:get("policy"):voter("tool/before", function() return "ask" end)
               local r = T:execute({ id = "1", name = "echo", arguments = { text = "x" } }, {
                 approve = function(call)
                   call.arguments.text = "swapped"
                   return { decision = "allow", source = "user" }
                 end,
               })
               return r.content, REWRITES, VOIDS"#,
        )
        .await;
        assert_eq!(out, "ran:x", "what runs is the frozen original, never the approver's mutated copy");
        assert_eq!((rewrites, voids), (1, 0), "an unchanged snapshot is neither voided nor rewritten again");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn dynamic_tool_schema_rejects_fraction_before_vote() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "dynamic_schema_probe", "arguments": { "count": 1.5 } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"RAN, VOTED = 0, 0
            T:register("dynamic_schema_probe", {
                description = "probe", parameters = function() return {
                    type = "object", properties = { count = { type = "integer" } }, required = { "count" },
                } end,
                execute = function() RAN = RAN + 1 return "ran" end,
            })
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "dynamic_schema_probe" then VOTED = VOTED + 1 end end)"#,
        )
        .await;
        let response = h.prompt("go", json!({})).await.unwrap();
        let (ran, voted): (i64, i64) = eval(&h, "return RAN, VOTED").await;
        assert_eq!((ran, voted), (0, 0), "invalid dynamic schema arguments reached execution or vote: {response}");
        assert!(response.to_string().contains("Invalid arguments"), "{response}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_json_null_and_empty_containers_reach_execution_intact() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "json_identity_probe", "arguments": { "null_value": null, "array": [], "object": {} } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        exec(&h, r#"SEEN = "never"
            T:register("json_identity_probe", {
                description = "probe", parameters = { type = "object", properties = {
                    null_value = { type = "null" }, array = { type = "array" }, object = { type = "object" },
                }, required = { "null_value", "array", "object" } },
                execute = function(args) SEEN = mare.json.encode(args) return "ran" end,
            })"#).await;
        let response = h.prompt("go", json!({})).await.unwrap();
        let seen: String = eval(&h, "return SEEN").await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&seen).ok(), Some(json!({ "null_value": null, "array": [], "object": {} })), "{response}; seen={seen}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn tool_schema_registration_and_snapshots_fail_closed() {
    local(async {
        let (valid, errors, preserved, original, rewritten): (bool, bool, bool, String, String) = policy_eval(r#"
            RAN, VOTED = 0, 0
            local def = { description = "probe", parameters = { type = "object", properties = {
                item = { type = "object", required = { "n" }, properties = { n = { type = "integer" } }, additionalProperties = false },
            }, required = { "item" } }, execute = function() RAN = RAN + 1 return "ran" end }
            T:register("strict_probe", def)
            local generation = T:revision()
            local a, ea = pcall(T.register, T, "strict_probe", { description = "probe", parameters = { type = "object", properties = { x = { format = "uuid" } } }, execute = function() end })
            local b = pcall(T.register, T, "missing_probe", { execute = function() end })
            local intact = T:revision() == generation and T:get("missing_probe") == nil
            def.parameters.properties.item.properties.n.type = "string"
            local exposed = T:get("strict_probe")
            exposed.parameters.properties.item.properties.n.type = "string"
            local good = T:execute({ id = "a", name = "strict_probe", arguments = { item = { n = 1 } } }, {})
            local bad = T:execute({ id = "b", name = "strict_probe", arguments = { item = { n = 1.5 } } }, {})
            C.cordis:get("policy"):rewriter("tool/rewrite", function(call) if call.name == "strict_probe" then return { item = { n = 1, extra = true } } end end)
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "strict_probe" then VOTED = VOTED + 1 end end)
            local rewrite = T:execute({ id = "c", name = "strict_probe", arguments = { item = { n = 1 } } }, {})
            return good.content == "ran" and RAN == 1 and VOTED == 0,
                not a and not b and tostring(ea):find("$/properties/x/format", 1, true) ~= nil,
                intact, bad.content, rewrite.content
        "#).await;
        assert!(valid && errors && preserved, "{valid} {errors} {preserved}");
        assert!(original.contains("Invalid arguments"), "{original}");
        assert!(rewritten.contains("Invalid arguments"), "{rewritten}");
    }).await
}

#[tokio::test]
async fn rewrite_can_repair_schema_errors_but_invalid_final_arguments_are_refused() {
    local(async {
        let (repaired, refused, non_json_refused, rewritten, voted, ran): (bool, bool, bool, i64, i64, i64) =
            policy_eval(
                r#"
            REWRITTEN, VOTED, RAN = 0, 0, 0
            T:register("original_probe", { description = "probe", parameters = { type = "object", properties = {
                n = { type = "integer" },
            }, required = { "n" } }, execute = function(args) RAN = RAN + 1; return tostring(args.n) end })
            C.cordis:get("policy"):rewriter("tool/rewrite", function(call)
                if call.name == "original_probe" then
                    REWRITTEN = REWRITTEN + 1
                    return { n = call.arguments.n == 1.5 and 1 or 2.5 }
                end
            end)
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "original_probe" then VOTED = VOTED + 1 end end)
            local good = T:execute({ id = "x", name = "original_probe", arguments = { n = 1.5 } }, {})
            local bad = T:execute({ id = "y", name = "original_probe", arguments = { n = 3.5 } }, {})
            local non_json = T:execute({ id = "z", name = "original_probe", arguments = { n = function() end } }, {})
            return good.content == "1" and not good.is_error,
                bad.refused == true and bad.content:find("Invalid arguments", 1, true) ~= nil,
                non_json.refused == true and non_json.content:find("Invalid arguments", 1, true) ~= nil,
                REWRITTEN, VOTED, RAN
        "#,
            )
            .await;
        assert!(
            repaired && refused && non_json_refused && rewritten == 2 && voted == 1 && ran == 1,
            "{repaired} {refused} {non_json_refused} {rewritten} {voted} {ran}"
        );
    })
    .await
}

#[tokio::test]
async fn original_schema_and_order_hint_are_validated_before_copying() {
    local(async {
        let (static_rejected, dynamic_rejected, hints_rejected, preserved): (bool, bool, bool, bool) = policy_eval(r#"
            local generation = T:revision()
            local function register(name, params, order)
                return pcall(T.register, T, name, { description = "probe", parameters = params, param_order = order, execute = function() end })
            end
            local bad_annotation = { type = "object", ["x-mare-order"] = false }
            local bad_meta = setmetatable({ type = "object" }, {})
            local a = register("bad_annotation", bad_annotation, { "a" })
            local b = register("bad_meta", bad_meta, { "a" })
            local c = register("bad_order_meta", { type = "object" }, setmetatable({ "a" }, {}))
            local d = register("bad_order_false", { type = "object" }, false)
            local e = register("bad_order_sparse", function() return { type = "object" } end, { [1] = "a", [3] = "b" })
            local before_dynamic = T:revision() == generation
            local registered = register("bad_dynamic", function() return bad_annotation end, { "a" })
            local dynamic_ok = pcall(T.definitions, T, {}, { "bad_dynamic" })
            local hint = { "a" }
            local good = register("good_order", { type = "object", properties = { a = { type = "string" } } }, hint)
            hint[1] = "changed"
            local declaration = T:definitions({}, { "good_order" })[1]
            return not a and not b, registered and not dynamic_ok, not c and not d and not e,
                before_dynamic and good and declaration.parameters["x-mare-order"][1] == "a"
        "#).await;
        assert!(static_rejected && dynamic_rejected && hints_rejected && preserved,
            "{static_rejected} {dynamic_rejected} {hints_rejected} {preserved}");
    }).await
}

#[tokio::test]
async fn tool_shadow_is_revoked_with_its_parent_and_approval_does_not_transfer() {
    local(async {
        let (content, ran, remaining, generation_changed, stale_rejected): (String, i64, bool, bool, bool) = policy_eval(r#"
            RAN = 0
            local base = T:register("shadow_probe", { description = "base", parameters = { type = "object" }, execute = function() RAN = RAN + 1 end })
            local copy = T:get("shadow_probe")
            copy.description = "wrapped"
            T:shadow("shadow_probe", copy)
            local nested = T:get("shadow_probe")
            nested.description = "nested"
            T:shadow("shadow_probe", nested)
            local stale = pcall(T.shadow, T, "shadow_probe", copy)
            local declarations = T:definitions({}, { "shadow_probe" })
            local bound = T:bind_declarations(declarations)
            local generation = T:revision()
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "shadow_probe" then return "ask" end end)
            local result = T:execute({ id = "a", name = "shadow_probe", arguments = {} }, {
                declarations = bound, approve = function()
                    base()
                    T:register("shadow_probe", { description = "replacement", parameters = { type = "object" }, execute = function() RAN = RAN + 10 end })
                    return { decision = "allow" }
                end,
            })
            return result.content, RAN, T:get("shadow_probe").description == "replacement", T:revision() ~= generation, not stale
        "#).await;
        assert!(content.contains("blocked"), "{content}");
        assert_eq!(ran, 0);
        assert!(remaining && generation_changed && stale_rejected);
    }).await
}

#[tokio::test]
async fn revoking_a_tool_cancels_its_pending_approval_wait() {
    local(async {
        let (finished, refused, content, ran): (bool, bool, String, i64) = policy_eval(
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local ran = 0
            local revoke = T:register("revoke_pending_approval_probe", {
                description = "probe",
                parameters = { type = "object" },
                execute = function() ran = ran + 1; return "ok" end,
            })
            C.cordis:get("policy"):voter("tool/before", function(call)
                if call.name == "revoke_pending_approval_probe" then return "ask" end
            end)
            local pending = mare.task.spawn(function()
                return T:execute({ id = "r1", name = "revoke_pending_approval_probe", arguments = {} }, {
                    approve = function(_, _, _, cancel)
                        entered_tx:send(true)
                        cancel:wait()
                        return { decision = "deny", reason = "approval cancelled", source = "cancelled" }
                    end,
                })
            end)
            entered_rx:recv()
            revoke()
            local finished, result = mare.task.timeout(2000, function() return pending:wait() end)
            return finished, finished and result.is_error == true and result.refused == true,
                finished and result.content or "", ran
        "#,
        )
        .await;
        assert!(finished, "revoking the tool must cancel the pending approval wait promptly");
        assert!(refused, "expected refused error result, got: {content}");
        assert!(content.contains("blocked"), "{content}");
        assert_eq!(ran, 0);
    })
    .await
}

#[tokio::test]
async fn concurrent_dynamic_tool_calls_use_their_own_declaration() {
    local(async {
        let (a, b, wrong_a, wrong_b, ran): (String, String, bool, bool, i64) = policy_eval(r#"
            RAN = 0
            T:register("per_model", { description = "probe", parameters = function(model)
                return { type = "object", properties = { v = { type = model.id == "number" and "integer" or "string" } }, required = { "v" } }
            end, execute = function(args) RAN = RAN + 1 return tostring(args.v) end })
            local a = T:bind_declarations(T:definitions({ id = "number" }, { "per_model" }))
            local b = T:bind_declarations(T:definitions({ id = "string" }, { "per_model" }))
            local function run(id, value, bindings)
                return T:execute({ id = id, name = "per_model", arguments = { v = value } }, { declarations = bindings })
            end
            local x = mare.task.spawn(function() return run("a", 1, a) end)
            local y = mare.task.spawn(function() return run("b", "text", b) end)
            local xr, yr = x:wait(), y:wait()
            local bad_a, bad_b = run("c", "text", a), run("d", 1, b)
            return xr.content, yr.content, bad_a.refused == true, bad_b.refused == true, RAN
        "#).await;
        assert_eq!((a.as_str(), b.as_str(), wrong_a, wrong_b, ran), ("1", "text", true, true, 2));
    }).await
}

#[tokio::test]
async fn rewritten_non_json_values_never_reach_a_vote() {
    local(async {
        let (refused, voted, ran): (i64, i64, i64) = policy_eval(r#"
            RAN, VOTED = 0, 0
            T:register("non_json", { description = "probe", parameters = { type = "object" }, execute = function() RAN = RAN + 1 end })
            local rewrites = { function() return { bad = function() end } end,
                function() return { bad = 0/0 } end, function() return { bad = { [1] = 1, [3] = 3 } } end }
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "non_json" then VOTED = VOTED + 1 end end)
            local refused = 0
            for _, rewrite in ipairs(rewrites) do
                local off = C.cordis:get("policy"):rewriter("tool/rewrite", rewrite)
                local result = T:execute({ id = "1", name = "non_json", arguments = {} }, {})
                if result.refused and result.content:find("Invalid arguments", 1, true) then refused = refused + 1 end
                off()
            end
            return refused, VOTED, RAN
        "#).await;
        assert_eq!((refused, voted, ran), (3, 0, 0));
    }).await
}

#[tokio::test]
async fn a_model_cannot_call_a_tool_removed_from_its_request() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([
            { "tool_calls": [{ "name": "not_declared_probe", "arguments": {} }] }, { "text": "done" }
        ])).no_session().build().await;
        expose(&h);
        exec(&h, r#"RAN = 0
            T:register("not_declared_probe", { description = "probe", parameters = { type = "object" }, execute = function() RAN = RAN + 1 return "ran" end })
            C.cordis:on("request/build", function(req, next)
                local keep = {}
                for _, d in ipairs(req.tools) do if d.name ~= "not_declared_probe" then keep[#keep + 1] = d end end
                req.tools = keep
                return next(req)
            end)"#).await;
        let result = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return RAN").await, 0, "{result}");
        assert!(result.to_string().contains("not declared"), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn direct_tool_call_rejects_a_json_text_string_as_the_argument_root() {
    local(async {
        let (refused, voted, ran): (bool, i64, i64) = policy_eval(r#"RAN, VOTED = 0, 0
            T:register("root_probe", { description = "probe", parameters = { type = "object" }, execute = function() RAN = RAN + 1 end })
            C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "root_probe" then VOTED = VOTED + 1 end end)
            local r = T:execute({ id = "1", name = "root_probe", arguments = "{}" }, {})
            return r.refused == true, VOTED, RAN"#).await;
        assert_eq!((refused, voted, ran), (true, 0, 0));
    }).await
}

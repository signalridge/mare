//! The `policy` host plugin: voters register with
//! the service as effects of their plugin's fiber, the owning service decides, and Rust folds the
//! votes fail-closed.

use super::{host, write_plugin};
use crate::common::*;

const VOTERS: &str = r#"return { name = "voters", inject = { "policy" }, apply = function(ctx)
  local policy = ctx.policy
  mare._log = {}
  -- allow unless the call is "bad"; then deny (the first deny stops the rest)
  policy:voter("tool/before", function(call)
    table.insert(mare._log, "plain")
    if call.name == "bad" then return { decision = "deny", reason = "bad call" } end
  end)
  -- prepend: runs first
  policy:voter("tool/before", function(call)
    table.insert(mare._log, "first")
  end, { prepend = true })
  -- one decision slot: only the earliest registered of a slot runs
  policy:voter("tool/before", function() table.insert(mare._log, "slot-a") end, { slot = "obs" })
  policy:voter("tool/before", function() table.insert(mare._log, "slot-b") end, { slot = "obs" })
  -- a voter that fails or exceeds its timeout is a deny
  policy:voter("tool/approval", function(call)
    if call.name == "boom" then error("voter crashed") end
    if call.name == "slow" then mare.task.sleep(200) end
  end, { timeout_ms = 50 })
  policy:rewriter("tool/rewrite", function(call)
    return { path = "/rewritten/" .. call.arguments.path }
  end)
end }"#;

const DECIDER: &str = r#"return { name = "decider", inject = { "policy" }, apply = function(ctx)
  local policy = ctx.policy
  mare._decide = function(point, call) return policy:decide(point, { call }) end
  mare._rewrite = function(call) return policy:rewrite("tool/rewrite", call) end
end }"#;

#[tokio::test]
async fn eval_approver_votes_on_escalation_requests() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "eval-approver",
            include_str!(
                "../../../../../evals/tasks/dev-sandbox-escalation/environment/plugins/eval-approver/init.lua"
            ),
        );
        write_plugin(&env, "decider", DECIDER);
        let h = host(&env).await;
        let (allow, deny, reason): (String, String, String) = eval(
            &h,
            r#"local yes = mare._decide("tool/approval", { arguments = {
                   sandbox_permissions = "require_escalated", justification = "write report" } })
               local no = mare._decide("tool/approval", { arguments = { sandbox_permissions = "require_escalated" } })
               return yes.decision, no.decision, tostring(no.reason)"#,
        )
        .await;
        assert_eq!(allow, "allow", "a complete escalation request must be approved");
        assert_eq!(deny, "deny", "an incomplete escalation request must be refused: {reason}");
        assert!(reason.contains("not approved"), "unexpected refusal: {reason}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn voters_are_registrations_and_the_fold_is_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let voters = write_plugin(&env, "voters", VOTERS);
        write_plugin(&env, "decider", DECIDER);
        let h = host(&env).await;
        let decide = |point: &'static str, name: &'static str| {
            let h = h.clone();
            async move {
                eval::<String>(
                    &h,
                    &format!(
                        "mare._log = {{}} local d = mare._decide('{point}', {{ name = '{name}', arguments = {{}} }}) \
                         return d.decision .. '|' .. table.concat(mare._log, ',') .. '|' .. tostring(d.reason)"
                    ),
                )
                .await
            }
        };
        let allowed = decide("tool/before", "ok").await;
        // prepend first, then registration order; only the earliest voter of the "obs" slot runs
        assert!(allowed.starts_with("allow|first,plain,slot-a|"), "{allowed}; {}", h.kernel.problems().join("\n"));
        let denied = decide("tool/before", "bad").await;
        assert!(denied.starts_with("deny|first,plain|"), "the first deny stops the rest: {denied}");
        assert!(decide("tool/approval", "boom").await.starts_with("deny|"), "a crashing voter is a deny");
        assert!(decide("tool/approval", "slow").await.starts_with("deny|"), "a timed out voter is a deny");
        let rewritten: String =
            eval(&h, "local r = mare._rewrite({ name = 'read', arguments = { path = 'x' } }) return r.path").await;
        assert_eq!(rewritten, "/rewritten/x");
        // the voters go with their plugin
        std::fs::remove_file(&voters).unwrap();
        h.reload_all().await.unwrap();
        let after = decide("tool/before", "bad").await;
        assert!(!after.starts_with("deny|"), "a removed plugin's voter still voted: {after}");
        h.shutdown().await;
    })
    .await
}

/// The policy epoch (an approval's fingerprint, rechecked before an approved call runs) moves only
/// when a policy service voter or rewriter comes or goes. An ordinary listener leaves an approval
/// valid (flip F2: the first tool call of `-p` was refused with "approval changed before execution").
#[tokio::test]
async fn only_policy_relevant_registrations_move_the_policy_epoch() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let epoch = || h.kernel.policy_epoch();
        let e0 = epoch();
        exec(&h, "local d = C.cordis:on('custom/ordinary', function() end) d()").await;
        assert_eq!(epoch(), e0, "an ordinary listener came and went");

        exec(&h, "mare._listener = C.cordis:on('tool/before', function() end)").await;
        let added = epoch();
        exec(&h, "mare._listener()").await;
        assert_eq!((added, epoch()), (e0, e0), "ordinary listener registration and removal on a policy point");

        exec(&h, "mare._v = C.cordis:get('policy'):voter('tool/approval', function() end)").await;
        let e1 = epoch();
        assert!(e1 > e0, "a policy service voter");
        exec(&h, "mare._v()").await;
        assert!(epoch() > e1, "its disposal");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ordinary_listener_on_unlisted_point_does_not_vote() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "custom_policy",
            r#"return {
                name = "custom_policy",
                contracts = {
                    custom = {
                        owner = "custom_policy",
                        events = {
                            ["unlisted-policy"] = {
                                doc = "custom policy point",
                                mode = "policy",
                                expose = "lua",
                            },
                        },
                    },
                },
                apply = function() end,
            }"#,
        );
        let h = host(&env).await;
        let point = "custom/unlisted-policy";
        exec(&h, "C.cordis:on('custom/unlisted-policy', function() return { decision = 'deny' } end)").await;

        let decision = h.kernel.policy(point, mlua::MultiValue::new()).await.unwrap();
        assert_eq!(decision.get::<String>("decision").unwrap(), "allow", "an ordinary listener must not vote");
        assert_eq!(h.kernel.voter_count(point), 0);
        assert_eq!(h.kernel.listener_count(point), 1);
        h.shutdown().await;
    })
    .await
}

/// `ctx.policy:epoch()` reads the policy epoch; `ctx.policy:has_voters(point)` whether a point has
/// voters now, replacing ctx:policy_epoch / ctx:has_listeners.
#[tokio::test]
async fn the_policy_service_reads_the_epoch_and_whether_a_point_has_voters() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "nat",
            r#"return { name = "nat", inject = { "policy" }, apply = function(ctx)
  mare._epoch = function() return ctx.policy:epoch() end
  mare._has = function(point) return ctx.policy:has_voters(point) end
  mare._vote = function(point) return ctx.policy:voter(point, function() end) end
end }"#,
        );
        let h = host(&env).await;
        let epoch: i64 = eval(&h, "return mare._epoch()").await;
        assert_eq!(epoch as u64, h.kernel.policy_epoch());
        let before: bool = eval(&h, "return mare._has('session/before-fork')").await;
        assert!(!before);
        exec(&h, "mare._dispose = mare._vote('session/before-fork')").await;
        let (has, moved): (bool, bool) =
            eval(&h, &format!("return mare._has('session/before-fork'), mare._epoch() > {epoch}")).await;
        assert!(has && moved, "has {has}, epoch moved {moved}");
        exec(&h, "mare._dispose()").await;
        assert!(!eval::<bool>(&h, "return mare._has('session/before-fork')").await);
        h.shutdown().await;
    })
    .await
}

/// `policy:decide(point, args, { require_explicit_approval = true })`: the producer-declared option
/// reaches the fold through the Lua service (types/mare/services/ctx.lua `policy_with`; the Rust
/// producers pass it as PolicyOptions, approvals/net.rs, approvals/child.rs). With it, a point
/// nobody votes on, or where every voter abstains, is an `unavailable` deny; without it, allow.
#[tokio::test]
async fn decide_with_require_explicit_approval_denies_when_nobody_answers() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "explicit",
            r#"return { name = "explicit", inject = { "policy" }, apply = function(ctx)
  local policy = ctx.policy
  mare._has = function(point) return policy:has_voters(point) end
  mare._vote = function(point, vote) return policy:voter(point, function() return vote end) end
  mare._decide = function(point, opts)
    local d = policy:decide(point, { { name = "probe", arguments = {} } }, opts)
    return d.decision .. "|" .. tostring(d.unavailable == true)
  end
end }"#,
        );
        let h = host(&env).await;
        assert!(!eval::<bool>(&h, "return mare._has('session/before-fork')").await, "the point has no voters here");
        let decide = |opts: &'static str| {
            let h = h.clone();
            async move { eval::<String>(&h, &format!("return mare._decide('session/before-fork', {opts})")).await }
        };
        let explicit = "{ require_explicit_approval = true }";
        assert_eq!(decide("nil").await, "allow|false", "nobody votes, no option: allow");
        assert_eq!(decide(explicit).await, "deny|true", "nobody votes, explicit approval required: unavailable deny");
        exec(&h, "mare._abstain = mare._vote('session/before-fork', nil)").await;
        assert_eq!(decide("nil").await, "allow|false", "an abstaining voter, no option: allow");
        assert_eq!(decide(explicit).await, "deny|true", "only abstaining voters, explicit approval required: deny");
        exec(&h, "mare._abstain() mare._allow = mare._vote('session/before-fork', 'allow')").await;
        assert_eq!(decide(explicit).await, "allow|false", "an explicit allow satisfies the option");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn policy_rewrite_returns_arguments_or_raises() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(
            &env,
            "rw_probe",
            r#"return {
                name = "rw_probe",
                inject = { "policy" },
                apply = function(ctx)
                    local policy = ctx.policy
                    mare._rewrite = function(call)
                        return policy:rewrite("tool/rewrite", call)
                    end
                    mare._add_rewriter = function(fn)
                        return policy:rewriter("tool/rewrite", fn)
                    end
                end,
            }"#,
        );
        let h = host(&env).await;
        // 1. No rewriters: returns arguments directly (not `{ ok = true, arguments = ... }`).
        let passthrough: String = eval(
            &h,
            r#"
            local args = mare._rewrite({ name = "read", arguments = { path = "a.txt" } })
            assert(type(args) == "table" and args.ok == nil and args.arguments == nil, mare.json.encode(args))
            return args.path
            "#,
        )
        .await;
        assert_eq!(passthrough, "a.txt");

        // 2. Rewriter transforms arguments: returns the rewritten arguments table directly.
        let rewritten: String = eval(
            &h,
            r#"
            local dispose = mare._add_rewriter(function(call)
                return { path = "/rewritten/" .. call.arguments.path }
            end)
            local args = mare._rewrite({ name = "read", arguments = { path = "b.txt" } })
            dispose()
            assert(type(args) == "table" and args.ok == nil and args.arguments == nil, mare.json.encode(args))
            return args.path
            "#,
        )
        .await;
        assert_eq!(rewritten, "/rewritten/b.txt");

        // 3. Failing rewriter raises structured `policy.rewrite_refused` error with the reason in `message`.
        let (err_code, err_msg): (String, String) = eval(
            &h,
            r#"
            local dispose = mare._add_rewriter(function()
                error("forbidden path")
            end)
            local ok, err = pcall(mare._rewrite, { name = "read", arguments = { path = "c.txt" } })
            dispose()
            assert(not ok, "failing rewriter must raise")
            assert(type(err) == "table", "rewrite refusal must be a structured mare.Error, got " .. type(err) .. ": " .. tostring(err))
            return tostring(err.code), tostring(err.message)
            "#,
        )
        .await;
        assert_eq!(err_code, "policy.rewrite_refused");
        assert!(err_msg.contains("forbidden path"), "message must preserve reason: {err_msg}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn pipeline_guard_propagates_non_rewrite_refused_errors() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(serde_json::json!([
                { "tool_calls": [{ "id": "turn-c1", "name": "rw_guard_probe", "arguments": { "path": "c" } }] },
                { "text": "after crash" }
            ]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let (refused_ok, direct_propagated): (bool, bool) = eval(
            &h,
            r#"
            local tools = C.cordis:get("tools")
            local policy = C.cordis:get("policy")
            tools:register("rw_guard_probe", {
                description = "probe tool for policy:rewrite error handling",
                side_effect = "read",
                parameters = { type = "object", properties = { path = { type = "string" } } },
                execute = function(args) return { content = "ran:" .. tostring(args.path) } end,
            })

            -- 1. A rewriter refusal (`policy.rewrite_refused`) turns into a tool refusal result.
            local dispose = policy:rewriter("tool/rewrite", function()
                error("rewriter blocked call")
            end)
            local refused = tools:execute({ id = "c1", name = "rw_guard_probe", arguments = { path = "a" } }, {})
            dispose()
            local refused_ok = refused.is_error == true
                and refused.refused == true
                and tostring(refused.content):find("rewriter blocked call", 1, true) ~= nil

            -- 2. Any other error from `policy:rewrite` (e.g. service crash) must propagate directly
            -- out of `tools:execute` instead of returning `{ refused = true, content = "Invalid arguments: ..." }`.
            local svc = C.cordis.reflect:getImpl("policy").value
            local orig_rewrite = svc.rewrite
            svc.rewrite = function()
                error("policy service crashed")
            end
            local exec_ok, exec_res = pcall(tools.execute, tools, { id = "c2", name = "rw_guard_probe", arguments = { path = "b" } }, {})
            svc.rewrite = orig_rewrite
            assert(not exec_ok, "non-refusal error must propagate from tools:execute, got " .. tostring(exec_res))
            assert(tostring(exec_res):find("policy service crashed", 1, true) ~= nil, tostring(exec_res))
            assert(tostring(exec_res):find("Invalid arguments:", 1, true) == nil, tostring(exec_res))
            assert(tostring(exec_res):find("Tool call blocked:", 1, true) == nil, tostring(exec_res))

            -- 3. Set up agent-turn capture: when `policy:rewrite` crashes during a turn, `tool_exec`
            -- records a failed tool result (refused ~= true, status ~= "denied", no "Invalid arguments:").
            CAPTURED_COMPLETED = nil
            C.cordis:on("item/completed", function(ev)
                if type(ev) == "table" and type(ev.item) == "table" and ev.item.type == "tool_call" and ev.item.name == "rw_guard_probe" then
                    CAPTURED_COMPLETED = ev
                end
            end)
            ORIG_POLICY_REWRITE = orig_rewrite
            svc.rewrite = function()
                error("policy service crashed")
            end

            return refused_ok, not exec_ok
            "#,
        )
        .await;
        assert!(refused_ok, "policy.rewrite_refused must turn into a refused tool result");
        assert!(direct_propagated, "unexpected policy service errors must propagate out of tools:execute");

        h.prompt("trigger tool with crashed policy service", serde_json::json!({})).await.unwrap();
        let (status, refused_flag, content): (String, bool, String) = eval(
            &h,
            r#"
            local svc = C.cordis.reflect:getImpl("policy").value
            svc.rewrite = ORIG_POLICY_REWRITE
            assert(type(CAPTURED_COMPLETED) == "table", "expected item/completed for rw_guard_probe")
            local item = CAPTURED_COMPLETED.item or {}
            local res = item.result or {}
            local msg = item.message or {}
            local text = type(msg.content) == "table" and msg.content[1] and msg.content[1].text or ""
            return tostring(CAPTURED_COMPLETED.status), res.refused == true, tostring(text)
            "#,
        )
        .await;
        assert!(!refused_flag, "policy service crash must not set refused = true: status={status} content={content}");
        assert_ne!(status, "denied", "policy service crash must not be classified as denied: status={status} content={content}");
        assert_eq!(status, "failed", "policy service crash must complete as failed: status={status} content={content}");
        assert!(content.contains("policy service crashed"), "expected crash message in tool output: {content}");
        assert!(!content.contains("Invalid arguments:"), "crash must not be labeled Invalid arguments: {content}");
        assert!(!content.contains("Tool call blocked:"), "crash must not be labeled Tool call blocked: {content}");
        h.shutdown().await;
    })
    .await
}

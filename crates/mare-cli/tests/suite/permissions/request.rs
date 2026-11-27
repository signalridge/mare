//! Model-visible structured permission requests use the ordinary tool approval pipeline.
use super::*;

#[tokio::test]
async fn provider_request_partial_grant_is_used_and_does_not_open_other_paths_or_network() {
    local(async {
        let env = TestEnv::new().await;
        let allowed = env.cwd.join("allowed");
        let refused = env.cwd.join("refused");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&refused).unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "request_permissions", "arguments": {
                "additional_permissions": { "write": [allowed, refused], "network": true },
                "scope": "session", "justification": "save the requested reports"
            } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": allowed.join("yes.txt"), "content": "yes" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": refused.join("no.txt"), "content": "no" } }] },
            { "text": "done" }
        ]);
        let h = env
            .host()
            .no_session()
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }))
            .build()
            .await;
        expose(&h);
        global(&h, "ALLOWED", allowed.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            -- methods read through the service's tracking proxy are callable tables
            local callable = require("mare_lib").callable
            assert(callable(P.describe_request) and callable(P.resolve_request))
            assert(callable(P.commit_request) and callable(P.grants))
            assert(P._request == nil and P._resolve == nil and P._commit == nil and P._grants == nil)
            asks = 0
            local seen = {}
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if not seen[call.id] then
                    seen[call.id] = true
                    asks = asks + 1
                end
                if call.arguments.additional_permissions then
                    return { decision = "allow", permissions = { write = { ALLOWED } }, scope = "session" }
                end
                return { decision = "deny", reason = "outside the approved paths" }
            end)
        "#,
        )
        .await;
        h.prompt("save reports", json!({})).await.unwrap();
        let results = tool_results(&h).await;
        assert!(results.contains("permissions") && results.contains("session"), "{results}");
        assert_eq!(std::fs::read_to_string(allowed.join("yes.txt")).unwrap(), "yes");
        assert!(!refused.join("no.txt").exists());
        assert_eq!(eval::<i64>(&h, "return asks").await, 2, "request + ungranted write only");
        let (net, outside, approved): (bool, bool, bool) = eval(
            &h,
            r#"
            local v = C.cordis:get("policy"):decide("tool/before", { { name = "probe", arguments = {} }, { side_effect = "read" } })
            return mare.proc.sandbox.check("network", "example.com", v.sandbox_grant),
                mare.proc.sandbox.check("write", mare.cwd .. "/outside.txt", v.sandbox_grant),
                mare.proc.sandbox.check("write", ALLOWED .. "/after.txt", v.sandbox_grant)
        "#,
        )
        .await;
        assert!(!net && !outside && approved);
        let audited: bool = eval(
            &h,
            r#"
            local requested, granted = false, false
            for _, r in ipairs(S:list_records("permission/request").data) do
                if r.tool == "request_permissions" then
                    requested = r.scope == "session" and r.permissions.network == true and #r.permissions.write == 2
                end
            end
            for _, r in ipairs(S:list_records("permission/grant").data) do
                if r.tool == "request_permissions" then
                    granted = r.scope == "session" and #r.permissions.write == 1 and r.permissions.network == nil
                        and mare.proc.sandbox.within(ALLOWED, r.permissions.write)
                end
            end
            return requested and granted
        "#,
        )
        .await;
        assert!(audited, "requested scope and effective partial grant must share the permission audit chain");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn request_headless_denial_cannot_seed_a_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (denied, empty): (bool, bool) = eval(
            &h,
            r#"
            local r = C.cordis:get("tools"):execute({ id = "request", name = "request_permissions", arguments = {
                additional_permissions = { write = { mare.cwd } }, justification = "save files", scope = "session"
            } })
            local g = P:grants()
            return r.refused == true and r.content:find("nobody can answer", 1, true) ~= nil,
                not g.write or #g.write == 0
        "#,
        )
        .await;
        assert!(denied && empty);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn turn_grants_expire_and_partial_subdirectories_never_grow_to_their_parent() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("out/sub")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (first, covered, remaining, expired): (String, String, String, String) = eval(&h, r#"
            local function request(path, approval, scope)
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "request_permissions", arguments = {
                    additional_permissions = { write = { path } }, justification = "save report", scope = scope or "turn"
                } }, { approve = approval })
            end
            local first = request(mare.cwd .. "/out", function()
                return { decision = "allow", permissions = { write = { mare.cwd .. "/out/sub" } }, scope = "turn" }
            end)
            local covered = request(mare.cwd .. "/out/sub", function() error("already granted") end)
            local remaining = request(mare.cwd .. "/out", function()
                return { decision = "deny", reason = "parent is not approved" }
            end)
            C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" })
            local expired = request(mare.cwd .. "/out/sub", function()
                return { decision = "deny", reason = "turn ended" }
            end)
            return first.content, covered.content, remaining.content, expired.content
        "#).await;
        assert!(first.contains("turn") && first.contains("sub"), "{first}");
        assert!(covered.contains("permissions") && !covered.contains("denied"), "{covered}");
        assert!(remaining.contains("parent is not approved"), "{remaining}");
        assert!(expired.contains("turn ended"), "{expired}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn completing_a_turn_without_grants_does_not_invalidate_an_authorized_tool() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (result, ran, before, after): (String, bool, i64, i64) = eval(
            &h,
            r#"
            local tools, bus = C.cordis:get("tools"), C.cordis:get("bus")
            local ran, after = false, -1
            tools:register("boundary_probe", {
                description = "probe", side_effect = "read", parameters = { type = "object" },
                execute = function() ran = true; return { content = "ran" } end,
            })
            local before = P:policy_revision()
            local result = tools:execute({ id = "probe", name = "boundary_probe", arguments = {} }, {
                on_state = function(_, state)
                    if state == "started" then
                        bus:emit("turn/completed", { turn_id = "t-1", status = "completed" })
                        after = P:policy_revision()
                    end
                    return true
                end,
            })
            return result.content, ran, before, after
        "#,
        )
        .await;
        assert!(ran, "the authorized tool did not run: {result}");
        assert_eq!(result, "ran");
        assert_eq!(before, after, "an empty turn must not change the permission policy");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_scoped_tool_approval_survives_an_empty_turn_boundary() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (result, ran, granted, before, after): (String, bool, bool, i64, i64) = eval(
            &h,
            r#"
            local tools, bus = C.cordis:get("tools"), C.cordis:get("bus")
            local ran, after = false, -1
            tools:register("session_grant_probe", {
                description = "probe", side_effect = "read",
                parameters = { type = "object", properties = {
                    additional_permissions = { type = "object" },
                    justification = { type = "string" }, scope = { type = "string" },
                } },
                execute = function(_, opts)
                    ran = opts.permission_scope == "session" and opts.permissions.write ~= nil
                    return { content = "ran" }
                end,
            })
            local before = P:policy_revision()
            local result = tools:execute({ id = "session", name = "session_grant_probe", arguments = {
                additional_permissions = { write = { mare.cwd } }, justification = "save", scope = "session",
            } }, {
                approve = function() return { decision = "allow_session", scope = "session" } end,
                on_state = function(_, state)
                    if state == "started" then
                        bus:emit("turn/completed", { turn_id = "t-1", status = "completed" })
                        after = P:policy_revision()
                    end
                    return true
                end,
            })
            local grants = P:grants()
            return result.content, ran, grants.write ~= nil and #grants.write > 0, before, after
        "#,
        )
        .await;
        assert!(ran && granted, "the session approval must be committed before the tool runs: {result}");
        assert_eq!(result, "ran");
        assert_eq!(before, after, "a session approval does not create a turn grant");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn completing_a_turn_with_a_grant_invalidates_in_flight_tools_and_expires_the_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (result, ran, was_granted, expired, refused, before, after): (String, bool, bool, bool, bool, i64, i64) =
            eval(
                &h,
                r#"
            local tools, bus = C.cordis:get("tools"), C.cordis:get("bus")
            local initial = P:request({ write = { mare.cwd } }, {
                justification = "save", scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            assert(not initial.refused, initial.content)
            local grants = P:grants()
            local was_granted = grants.write ~= nil and #grants.write > 0
            local ran, after = false, -1
            tools:register("boundary_probe", {
                description = "probe", side_effect = "read", parameters = { type = "object" },
                execute = function() ran = true; return { content = "ran" } end,
            })
            local before = P:policy_revision()
            local result = tools:execute({ id = "probe", name = "boundary_probe", arguments = {} }, {
                on_state = function(_, state)
                    if state == "started" then
                        bus:emit("turn/completed", { turn_id = "t-1", status = "completed" })
                        after = P:policy_revision()
                    end
                    return true
                end,
            })
            grants = P:grants()
            local again = P:request({ write = { mare.cwd } }, {
                justification = "save again", scope = "turn",
                approve = function() return { decision = "deny", reason = "turn ended" } end,
            })
            return result.content, ran, was_granted, not grants.write or #grants.write == 0,
                again.refused == true, before, after
        "#,
            )
            .await;
        assert!(was_granted && expired && refused, "turn grants must expire and require a new approval");
        assert!(after > before, "expiring a grant must change the policy revision");
        assert!(!ran && result.contains("approval changed"), "the old authorization must be refused: {result}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_uncommitted_turn_grant_cannot_cross_the_turn_boundary() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("seed")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (during_approval, after_approval, ran, has_grants): (String, String, bool, bool) = eval(
            &h,
            r#"
            local tools, bus = C.cordis:get("tools"), C.cordis:get("bus")
            local ran = false
            tools:register("turn_grant_probe", {
                description = "probe", side_effect = "read",
                parameters = { type = "object", properties = {
                    additional_permissions = { type = "object" },
                    justification = { type = "string" }, scope = { type = "string" },
                } },
                execute = function() ran = true; return { content = "ran" } end,
            })
            local function finish_turn()
                bus:emit("turn/completed", { turn_id = "t-1", status = "completed" })
            end
            local function run(phase)
                local result = tools:execute({ id = phase, name = "turn_grant_probe", arguments = {
                    additional_permissions = { write = { mare.cwd } }, justification = "save", scope = "turn",
                } }, {
                    approve = function()
                        if phase == "approval" then finish_turn() end
                        return { decision = "allow", scope = "turn" }
                    end,
                    on_state = function(_, state)
                        if phase == "started" and state == "started" then finish_turn() end
                        return true
                    end,
                })
                assert(result.refused, result.content)
                return result.content
            end
            local during_approval, after_approval = run("approval"), run("started")
            tools:register("legacy_grant_probe", {
                description = "probe", side_effect = "external",
                parameters = { type = "object", properties = {
                    sandbox_permissions = { type = "string" }, justification = { type = "string" },
                } },
                execute = function() ran = true; return { content = "ran" } end,
            })
            local legacy = tools:execute({ id = "legacy", name = "legacy_grant_probe", arguments = {
                sandbox_permissions = "write:" .. mare.cwd, justification = "save",
            } }, { approve = function()
                finish_turn()
                return { decision = "allow", scope = "turn" }
            end })
            assert(legacy.refused, legacy.content)
            local seed = P:request({ write = { mare.cwd .. "/seed" } }, {
                justification = "seed grant", scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            assert(not seed.refused, seed.content)
            local ended = false
            local stale = tools:execute({ id = "legacy-existing", name = "legacy_grant_probe", arguments = {
                sandbox_permissions = "write:" .. mare.cwd, justification = "save",
            } }, { approve = function()
                if not ended then ended = true; finish_turn() end
                return { decision = "allow", scope = "turn" }
            end })
            assert(stale.refused, stale.content)
            local grants = P:grants()
            return during_approval, after_approval, ran, grants.write ~= nil and #grants.write > 0
        "#,
        )
        .await;
        assert!(
            !ran && !has_grants,
            "a grant that outlived its turn ran or was stored: {during_approval}; {after_approval}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn structured_grants_obey_every_policy_vote_and_cancelled_requests_grant_nothing() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (blocked, empty): (bool, bool) = eval(&h, r#"
            C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", permissions = { write = { mare.cwd } }, scope = "session" } end)
            C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", permissions = {} } end)
            local r = C.cordis:get("tools"):execute({ id = "intersection", name = "request_permissions", arguments = {
                additional_permissions = { write = { mare.cwd } }, justification = "save", scope = "session"
            } })
            local content = mare.json.decode(r.content)
            local t = mare.task.token()
            local cancelled = C.cordis:get("tools"):execute({ id = "cancelled", name = "request_permissions", arguments = {
                additional_permissions = { network = true }, justification = "fetch", scope = "session"
            } }, { cancel = t, approve = function() t:cancel(); return { decision = "allow_session" } end })
            local g = P:grants()
            return (not content.permissions.write or #content.permissions.write == 0) and content.scope == "turn",
                g.network ~= true and (not g.write or #g.write == 0) and cancelled.refused == true
        "#).await;
        assert!(blocked && empty);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn child_structured_grants_cross_parent_approval_and_remain_bounded() {
    local(async {
        let env = TestEnv::new().await;
        let out = env.cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        global(&h, "OUT", out.to_string_lossy().to_string());
        let result: String = eval(&h, r#"
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if call.arguments.additional_permissions then
                    return { decision = "allow", permissions = { write = { OUT } }, scope = "session" }
                end
                return { decision = "deny", reason = "not granted" }
            end)
            local script = {
                { tool_calls = {{ name = "request_permissions", arguments = {
                    additional_permissions = { write = { OUT }, network = true },
                    justification = "save child output", scope = "session"
                } }} },
                { tool_calls = {{ name = "write", arguments = { path = OUT .. "/child.txt", content = "child" } }} },
                { tool_calls = {{ name = "write", arguments = { path = mare.cwd .. "/outside.txt", content = "bad" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({ prompt = "save child output", no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) } } } })
            local result = child:wait()
            return mare.json.encode(result)
        "#).await;
        assert!(out.join("child.txt").exists(), "{result}");
        assert!(!env.cwd.join("outside.txt").exists(), "{result}");
        let empty: bool = eval(&h, "local g = P:grants(); return not g.write or #g.write == 0").await;
        assert!(empty, "child grants leaked to parent");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn explicit_read_grant_extends_only_requested_read_roots() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("read-grant")).unwrap();
        std::fs::create_dir_all(env.cwd.join("other")).unwrap();
        std::fs::write(env.cwd.join("read-grant/public"), "public").unwrap();
        std::fs::write(env.cwd.join("read-grant/secret"), "secret").unwrap();
        std::fs::write(env.cwd.join("other/private"), "private").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only", "accept_partial": ["fs_protect", "fs_deny_read"] }), json!({ "no_session": true })).await;
        let (public, secret, other, grant): (bool, bool, bool, String) = eval(&h, r#"
            mare.proc.sandbox.set_current({ level = "read-only", restrict_read = true,
                readable = {}, deny_read = { mare.cwd .. "/read-grant/secret" }, network = false,
                accept_partial = { "fs_protect", "fs_deny_read" } })
            local forbidden = C.cordis:get("permissions"):request({ read = { mare.cwd .. "/read-grant/secret" } }, {
                justification = "read denied file", approve = function() error("deny_read must not ask") end })
            assert(forbidden.refused, forbidden.content)
            local r = C.cordis:get("tools"):execute({ id = "read-request", name = "request_permissions", arguments = {
                additional_permissions = { read = { mare.cwd .. "/read-grant" } }, justification = "inspect public output"
            } }, { approve = function() return { decision = "allow", scope = "turn" } end })
            C.cordis:get("tools"):register("read_probe", { description = "probe", side_effect = "read", parameters = { type = "object" }, execute = function()
                local public = pcall(mare.fs.read, mare.cwd .. "/read-grant/public")
                local secret = pcall(mare.fs.read, mare.cwd .. "/read-grant/secret")
                local other = pcall(mare.fs.read, mare.cwd .. "/other/private")
                return { content = "read outcomes", details = { public = public, secret = secret, other = other } }
            end })
            local probe = C.cordis:get("tools"):execute({ id = "probe", name = "read_probe", arguments = {} })
            assert(probe.details and probe.details.public ~= nil, probe.content)
            return probe.details.public, probe.details.secret, probe.details.other, r.content
        "#).await;
        assert!(public && !secret && !other, "grant={grant}; read outcomes={public}/{secret}/{other}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn invalid_and_policy_denied_requests_cannot_reach_approval_or_seed_permissions() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("single-file"), "existing file").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            local function run(a)
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "request_permissions", arguments = a }, {
                    approve = function() error("must not ask") end
                })
            end
            local empty = run({ additional_permissions = {}, justification = "empty" })
            local empty_network = run({ additional_permissions = { network = {} }, justification = "empty network" })
            local single_file = run({ additional_permissions = { write = { mare.cwd .. "/single-file" } }, justification = "one file" })
            local missing_dir = run({ additional_permissions = { write = { mare.cwd .. "/missing-dir" } }, justification = "missing directory" })
            local typo = run({ additional_permissions = { writes = { mare.cwd } }, justification = "typo" })
            local no_reason = run({ additional_permissions = { write = { mare.cwd } }, justification = "  " })
            C.cordis:get("policy"):voter("permission/verdict", function() return { decision = "deny", reason = "organization policy" } end)
            local denied = run({ additional_permissions = { write = { mare.cwd } }, justification = "save" })
            local grants = P:grants()
            return empty.refused and empty_network.refused and single_file.refused and missing_dir.refused and typo.refused and no_reason.refused and denied.refused
                and denied.content:find("organization policy", 1, true) ~= nil
                and (not grants.write or #grants.write == 0)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_legacy_escalation_limits_actual_execution_and_does_not_cache_the_full_request() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("out/sub")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (first, second, prompts): (String, String, i64) = eval(&h, r#"
            C.cordis:get("tools"):register("scoped_probe", { description = "probe", side_effect = "external", parameters = { type = "object", properties = {
                sandbox_permissions = { type = "string" }, justification = { type = "string" }
            } }, execute = function()
                local yes = pcall(mare.fs.write, mare.cwd .. "/out/sub/yes", "yes")
                local no = pcall(mare.fs.write, mare.cwd .. "/out/no", "no")
                return { content = "write outcomes", details = { allowed = yes, outside = no } }
            end })
            local n = 0
            local function run(id)
                return C.cordis:get("tools"):execute({ id = id, name = "scoped_probe", arguments = {
                    sandbox_permissions = "write:" .. mare.cwd .. "/out", justification = "save output"
                } }, { approve = function()
                    n = n + 1
                    if n == 1 then return { decision = "allow_session", permissions = { write = { mare.cwd .. "/out/sub" } } } end
                    return { decision = "deny", reason = "still outside grant" }
                end })
            end
            local a, b = run("first"), run("second")
            local outcome = a.details and (tostring(a.details.allowed) .. "/" .. tostring(a.details.outside)) or a.content
            return outcome, b.content, n
        "#).await;
        assert_eq!(first, "true/false");
        assert!(env.cwd.join("out/sub/yes").exists());
        assert!(!env.cwd.join("out/no").exists());
        assert!(second.contains("still outside grant"), "{second}");
        assert_eq!(prompts, 2);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn permission_verdict_observers_cannot_rewrite_the_requested_grant() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("allowed")).unwrap();
        std::fs::create_dir_all(env.cwd.join("outside")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("permission/verdict", function(ev)
                if ev.verdict.permissions then
                    ev.verdict.permissions.write = { mare.cwd .. "/outside" }
                    ev.verdict.scope = "session"
                end
            end)
            C.cordis:get("policy"):voter("tool/approval", function()
                return { decision = "allow", permissions = { write = { mare.cwd } }, scope = "session" }
            end)
            local r = C.cordis:get("tools"):execute({ id = "frozen", name = "request_permissions", arguments = {
                additional_permissions = { write = { mare.cwd .. "/allowed" } }, justification = "save", scope = "turn"
            } })
            local grant = mare.json.decode(r.content)
            local g = P:grants()
            return grant.scope == "turn" and #grant.permissions.write > 0
                and mare.proc.sandbox.within(mare.cwd .. "/allowed", g.write)
                and not mare.proc.sandbox.within(mare.cwd .. "/outside", g.write)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn observing_an_approval_is_not_an_explicit_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("tool/approval", function() return nil end)
            local r = C.cordis:get("permissions"):request({ write = { mare.cwd } }, { justification = "save", scope = "session" })
            local g = P:grants()
            return r.refused == true and (not g.write or #g.write == 0)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn canonical_approval_policy_can_disable_permission_requests_before_answerers() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            local approval = C.cordis:get("approvals")
            approval:set({ mode = "granular", categories = { tool = true, sandbox = true, request_permissions = false } })
            local answered = false
            local r = C.cordis:get("permissions"):request({ write = { mare.cwd } }, { justification = "save", scope = "session",
                approve = function() answered = true; return { decision = "allow_session" } end })
            local g = C.cordis:get("permissions"):grants()
            return r.refused == true and not answered and (not g.write or #g.write == 0)
                and r.content:find("request_permissions", 1, true) ~= nil
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn malformed_partial_answers_never_fall_back_to_a_full_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            local answers = {
                { decision = "allow", permission = { write = {} } },
                { decision = "allow", permissions = "invalid" },
                { decision = "allow", permissions = {}, scope = "forever" },
                { decision = "allow", permission_constraints = { typo = {} } },
            }
            for i, answer in ipairs(answers) do
                local r = C.cordis:get("permissions"):request({ write = { mare.cwd } }, { id = "malformed-" .. i,
                    justification = "save", scope = "session", approve = function() return answer end })
                if not r.refused then return false end
            end
            local g = C.cordis:get("permissions"):grants()
            return not g.write or #g.write == 0
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn generic_structured_calls_keep_both_sandbox_and_prompt_rule_categories() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(
            &env,
            true,
            json!({ "preset": "read-only", "rules": [{ "tool": "grant_probe", "decision": "prompt" }] }),
            json!({ "no_session": true }),
        )
        .await;
        let safe: bool = eval(
            &h,
            r#"
            C.cordis:get("tools"):register("grant_probe", { description = "probe", side_effect = "read", parameters = { type = "object", properties = {
                additional_permissions = { type = "object" }, justification = { type = "string" }
            } }, execute = function() return { content = "ran" } end })
            local q = C.cordis:get("approvals")
            local n = 0
            local function run()
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "grant_probe", arguments = {
                    additional_permissions = { write = { mare.cwd } }, justification = "save"
                } }, { approve = function() n = n + 1; return { decision = "allow" } end })
            end
            q:set({ mode = "granular", categories = { tool = true, rules = true } })
            local no_sandbox = run()
            q:set({ mode = "granular", categories = { tool = true, sandbox = true } })
            local no_rules = run()
            q:set({ mode = "granular", categories = { tool = true, sandbox = true, rules = true } })
            local allowed = run()
            local g = C.cordis:get("permissions"):grants()
            return no_sandbox.refused == true and no_rules.refused == true and allowed.content == "ran" and n == 1
                and (not g.write or #g.write == 0)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_permissions_cannot_be_ignored_when_the_permissions_service_is_absent() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let safe: bool = eval(
            &h,
            r#"
            local ran = false
            C.cordis:get("tools"):register("opaque_probe", { description = "probe", side_effect = "external", parameters = { type = "object" },
                execute = function() ran = true; return { content = "ran" } end })
            C.cordis:get("policy"):voter("tool/before", function() return { decision = "ask", reason = "review" } end)
            local r = C.cordis:get("tools"):execute({ id = "no-resolver", name = "opaque_probe", arguments = {} }, {
                approve = function() return { decision = "allow", permissions = {} } end
            })
            return r.refused == true and not ran and r.content:find("permission resolver unavailable", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn replacing_the_host_baseline_revokes_grants_even_when_its_level_is_unchanged() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            local baseline = C.cordis:get("permissions"):request({ read = { mare.cwd } }, { justification = "inspect", scope = "session",
                approve = function() error("already readable") end })
            assert(not baseline.is_error, baseline.content)
            mare.proc.sandbox.set_current({ level = "read-only", restrict_read = true, readable = {}, network = false,
                accept_partial = { "fs_protect" } })
            local g = C.cordis:get("permissions"):grants()
            local again = C.cordis:get("permissions"):request({ read = { mare.cwd } }, { justification = "inspect", scope = "session",
                approve = function() return { decision = "deny", reason = "new baseline applies" } end })
            return (not g.read or #g.read == 0) and again.refused == true
                and again.content:find("new baseline applies", 1, true) ~= nil
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn child_context_cannot_forge_read_grants_through_a_legacy_command_lift() {
    local(async {
        let env = TestEnv::new().await;
        let file = env.cwd.join("read-receipt.txt");
        std::fs::write(&file, "approved read").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let spec: mlua::Value = h
            .lua()
            .load(
                r#"return { level = "read-only", restrict_read = true,
            readable = {}, network = false, accept_partial = { "fs_protect" } }"#,
            )
            .eval()
            .unwrap();
        let policy = mare_binding::ossandbox::Policy::from_lua(h.lua(), &spec, &env.cwd).unwrap().unwrap();
        mare_binding::ossandbox::set_inherited_ceiling(h.lua(), Some(policy));
        // A legacy command approval may lift writes/network, but never grants new reads.
        mare_binding::ossandbox::lift_ceiling(h.lua(), Some("receipt"), "legacy", None);
        const PROBE: &str = r#"
            return mare.exec.run({ tool_call_id = "receipt", approval_key = "legacy", sandbox = "danger-full-access",
                read_grants = { mare.cwd } }, function()
                return pcall(mare.fs.read, mare.cwd .. "/read-receipt.txt")
            end)
        "#;
        let before: bool = eval(&h, PROBE).await;
        mare_binding::ossandbox::stage_permission_reads(h.lua(), "receipt", "legacy", vec![file]);
        let approved: bool = eval(&h, PROBE).await;
        let consumed: bool = eval(&h, PROBE).await;
        assert!(!before && approved && !consumed, "forged={before}, approved={approved}, consumed={consumed}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn allowing_one_legacy_call_does_not_turn_it_into_a_turn_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            C.cordis:get("tools"):register("once_probe", { description = "probe", side_effect = "external", parameters = { type = "object", properties = {
                sandbox_permissions = { type = "string" }, justification = { type = "string" }
            } }, execute = function() return { content = "ran" } end })
            local n = 0
            local function run()
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "once_probe", arguments = {
                    sandbox_permissions = "write:" .. mare.cwd, justification = "one call"
                } }, { approve = function()
                    n = n + 1
                    return { decision = n == 1 and "allow" or "deny", reason = "one call only" }
                end })
            end
            local first, second = run(), run()
            local g = C.cordis:get("permissions"):grants()
            return first.content == "ran" and second.refused == true and n == 2 and (not g.write or #g.write == 0)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn permission_request_metadata_cannot_be_rewritten_to_bypass_its_category() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            C.cordis:get("approvals"):set({ mode = "granular", categories = { tool = true, sandbox = true, request_permissions = false } })
            C.cordis:get("policy"):voter("tool/before", function(_, info) info.permission_request = false end, { prepend = true })
            local answered = false
            local r = C.cordis:get("permissions"):request({ write = { mare.cwd } }, { justification = "save", scope = "session",
                approve = function() answered = true; return { decision = "allow_session" } end })
            local g = C.cordis:get("permissions"):grants()
            return r.refused == true and not answered and (not g.write or #g.write == 0)
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn structured_arguments_do_not_bypass_dangerous_command_review_in_full_access() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            local v = C.cordis:get("policy"):decide("tool/before", { { id = "danger", name = "bash", arguments = {
                command = "rm -rf missing-output", additional_permissions = { read = { mare.cwd } },
                justification = "inspect output"
            } }, { side_effect = "external", idempotent = false, unconfined = false, permission_request = false,
                command_tool = true, command = { shell = "bash", arg = "command" } } })
            return v.decision == "ask" and v.reason:find("dangerous command", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn replacing_an_approved_read_root_with_a_symlink_cannot_redirect_the_grant() {
    local(async {
        let env = TestEnv::new().await;
        let approved = env.cwd.join("read-root");
        let private = env.cwd.join("private");
        std::fs::create_dir_all(&approved).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(private.join("secret"), "must stay private").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(&h, r#"
            mare.proc.sandbox.set_current({ level = "read-only", restrict_read = true, readable = {}, network = false,
                accept_partial = { "fs_protect" } })
            local r = C.cordis:get("permissions"):request({ read = { mare.cwd .. "/read-root" } }, { justification = "inspect report", scope = "session",
                approve = function() return { decision = "allow_session" } end })
            assert(not r.is_error, r.content)
        "#).await;
        std::fs::remove_dir(&approved).unwrap();
        std::os::unix::fs::symlink(&private, &approved).unwrap();
        let safe: bool = eval(&h, r#"
            local r = C.cordis:get("tools"):execute({ id = "redirect", name = "read", arguments = { path = "read-root/secret" } })
            local asked = false
            local denied = C.cordis:get("permissions"):request({ read = { mare.cwd .. "/private" } }, { justification = "new target", scope = "session",
                approve = function() asked = true; return { decision = "deny", reason = "not the approved target" } end })
            local fresh = C.cordis:get("permissions"):request({ read = { mare.cwd .. "/private" } }, { justification = "approve the new target", scope = "session",
                approve = function() return { decision = "allow_session" } end })
            local approved = C.cordis:get("tools"):execute({ id = "new-target", name = "read", arguments = { path = "private/secret" } })
            return r.is_error == true and r.content:find("must stay private", 1, true) == nil
                and denied.refused == true and asked and not fresh.is_error
                and approved.content:find("must stay private", 1, true) ~= nil
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn a_partial_profile_cannot_pretend_to_confine_an_external_tool() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            local ran = false
            C.cordis:get("tools"):register("external_probe", { description = "probe", side_effect = "external", unconfined = true,
                parameters = { type = "object", properties = { additional_permissions = { type = "object" }, justification = { type = "string" } } },
                execute = function() ran = true; return { content = "ran" } end })
            local r = C.cordis:get("tools"):execute({ id = "external", name = "external_probe", arguments = {
                additional_permissions = { write = { mare.cwd }, network = true }, justification = "save output"
            } }, { approve = function() return { decision = "allow", permissions = { write = { mare.cwd } } } end })
            local g = C.cordis:get("permissions"):grants()
            return r.refused == true and not ran and (not g.write or #g.write == 0)
                and r.content:find("unconfined tool", 1, true) ~= nil
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn an_explicit_turn_scope_caps_legacy_session_decisions_and_command_caches() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            C.cordis:get("tools"):register("turn_probe", { description = "probe", side_effect = "external", parameters = { type = "object", properties = {
                sandbox_permissions = { type = "string" }, justification = { type = "string" }
            } }, execute = function() return { content = "ran" } end })
            local n = 0
            local function run()
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "turn_probe", arguments = {
                    sandbox_permissions = "write:" .. mare.cwd, justification = "this turn only"
                } }, { approve = function()
                    n = n + 1
                    if n == 1 then return { decision = "allow_session", scope = "turn" } end
                    return { decision = "deny", reason = "turn ended" }
                end })
            end
            local first = run()
            C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" })
            local second = run()
            return first.content == "ran" and second.refused == true and n == 2
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_session_scope_is_independent_of_the_legacy_command_cache_decision() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            C.cordis:get("tools"):register("session_probe", { description = "probe", side_effect = "external", parameters = { type = "object", properties = {
                sandbox_permissions = { type = "string" }, justification = { type = "string" }
            } }, execute = function() return { content = "ran" } end })
            local n = 0
            local function run()
                return C.cordis:get("tools"):execute({ id = tostring(math.random()), name = "session_probe", arguments = {
                    sandbox_permissions = "write:" .. mare.cwd, justification = "save this session"
                } }, { approve = function()
                    n = n + 1
                    if n == 1 then return { decision = "allow", scope = "session", permissions = { write = { mare.cwd } } } end
                    return { decision = "deny", reason = "preset revoked the grant" }
                end })
            end
            local first, second = run(), run()
            C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" })
            local third = run()
            C.cordis:get("permissions"):set("read-only")
            local fourth = run()
            return first.content == "ran" and second.content == "ran" and third.content == "ran"
                and fourth.refused == true and n == 2
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn a_permission_request_cannot_commit_after_its_originating_turn_ends() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            local r = C.cordis:get("permissions"):request({ write = { mare.cwd } }, {
                justification = "save", scope = "session",
                approve = function() return { decision = "allow_session" } end,
                on_state = function(_, state)
                    if state == "started" then C.cordis:get("bus"):emit("turn/completed", { turn_id = "t-1", status = "completed" }) end
                    return true
                end
            })
            local g = C.cordis:get("permissions"):grants()
            return r.refused == true and (not g.write or #g.write == 0)
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn restored_policy_views_cannot_change_the_frozen_permission_request_or_its_lifetime() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("allowed")).unwrap();
        std::fs::create_dir_all(env.cwd.join("allowed_meta")).unwrap();
        std::fs::create_dir_all(env.cwd.join("outside")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let safe: bool = eval(
            &h,
            r#"
            local mode, answered, intact = "arguments", 0, 0
            C.cordis:get("policy"):voter("tool/before", function(call, info)
                if mode == "arguments" then
                    call.arguments.additional_permissions.write = { mare.cwd .. "/outside" }
                else info.permission_request = false end
            end, { prepend = true })
            C.cordis:get("policy"):voter("tool/before", function(call, info)
                if mode == "arguments" then
                    if call.arguments.additional_permissions.write[1] == mare.cwd .. "/allowed" then intact = intact + 1 end
                elseif info.permission_request == true then
                    intact = intact + 1
                end
            end)
            local function run(dir)
                return C.cordis:get("permissions"):request({ write = { mare.cwd .. "/" .. dir } }, { justification = "save", scope = "session",
                    approve = function() answered = answered + 1; return { decision = "allow_session" } end })
            end
            local args = run("allowed")
            mode = "metadata"
            local metadata = run("allowed_meta")
            local g = C.cordis:get("permissions"):grants()
            return not args.refused and not metadata.refused and answered == 2 and intact == 2
                and g.write and #g.write == 2
                and g.write[1]:find("/outside", 1, true) == nil
                and g.write[2]:find("/outside", 1, true) == nil
        "#,
        )
        .await;
        assert!(safe);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn policy_views_cannot_relabel_the_execution_identity_to_skip_review() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "full-access" }), json!({ "no_session": true })).await;
        let safe: bool = eval(&h, r#"
            local ran, answered, intact, field, current_id = 0, 0, 0, "name", ""
            C.cordis:get("tools"):register("bash", { description = "probe", side_effect = "external", command_tool = true,
                command = { shell = "bash", arg = "command" },
                parameters = { type = "object", properties = { command = { type = "string" } } },
                execute = function() ran = ran + 1; return { content = "ran" } end })
            C.cordis:get("policy"):voter("tool/before", function(call)
                call[field] = field == "cwd" and (mare.cwd .. "/different") or "forged"
            end, { prepend = true })
            C.cordis:get("policy"):voter("tool/before", function(call)
                if call.name == "bash" and call.cwd == mare.cwd and call.id == current_id
                    and call.snapshot ~= "forged" and call.approval_key ~= "forged" then
                    intact = intact + 1
                end
            end)
            for _, key in ipairs({ "name", "cwd", "id", "snapshot", "approval_key" }) do
                field = key
                current_id = "identity-" .. key
                local r = C.cordis:get("tools"):execute({ id = current_id, name = "bash", arguments = { command = "rm -rf missing-output" } }, {
                    approve = function() answered = answered + 1; return { decision = "deny" } end })
                if not r.refused then return false end
            end
            return ran == 0 and answered == 5 and intact == 5
        "#).await;
        assert!(safe);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn command_tool_additional_permissions_survives_policy_binding_through_tools_execute() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("allowed")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let result_json: String = eval(
            &h,
            r#"
            local ran, answered = 0, 0
            local T = C.cordis:get("tools")
            T:register("custom_cmd", {
                description = "custom shell command tool",
                side_effect = "external",
                command_tool = true,
                command = { shell = "bash", arg = "cmd" },
                parameters = {
                    type = "object",
                    properties = {
                        cmd = { type = "string" },
                        additional_permissions = { type = "object" },
                        justification = { type = "string" },
                        scope = { type = "string" },
                    },
                    required = { "cmd" },
                },
                execute = function(a)
                    ran = ran + 1
                    return { content = "ran:" .. tostring(a.cmd) }
                end,
            })
            local approve_session = function()
                answered = answered + 1
                return { decision = "allow_session" }
            end
            local r1 = T:execute({
                id = "cmd-add-1",
                name = "custom_cmd",
                arguments = {
                    cmd = "echo one",
                    additional_permissions = { write = { mare.cwd .. "/allowed" } },
                    justification = "write report output",
                    scope = "session",
                },
            }, { approve = approve_session })
            local r2 = T:execute({
                id = "cmd-add-2",
                name = "custom_cmd",
                arguments = {
                    cmd = "echo two",
                    additional_permissions = { write = { mare.cwd .. "/allowed" } },
                    justification = "write second output",
                    scope = "session",
                },
            }, { approve = approve_session })
            return mare.json.encode({
                r1_err = r1.is_error == true,
                r1_refused = r1.refused == true,
                r1_content = r1.content,
                r2_err = r2.is_error == true,
                r2_refused = r2.refused == true,
                r2_content = r2.content,
                ran = ran,
                answered = answered,
            })
        "#,
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(v["r1_err"], json!(false), "{v}");
        assert_eq!(v["r1_refused"], json!(false), "{v}");
        assert_eq!(v["r1_content"], json!("ran:echo one"), "{v}");
        assert_eq!(v["r2_err"], json!(false), "{v}");
        assert_eq!(v["r2_refused"], json!(false), "{v}");
        assert_eq!(v["r2_content"], json!("ran:echo two"), "{v}");
        assert_eq!(v["ran"], json!(2), "{v}");
        assert_eq!(v["answered"], json!(1), "{v}");
        h.shutdown().await;
    })
    .await
}

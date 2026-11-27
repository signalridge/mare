use super::*;

/// Real children with the same label start in the reverse order of their transcript files.
/// Manual transcripts stay separate; each approval follows its host-owned transcript.
#[tokio::test]
async fn same_named_children_keep_their_transcript_and_approval_identity() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(write_script("identity.txt"))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({}));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let seen: String = eval(
            &h,
            r#"
            local ctx = { session = {
              info = function() return S:info() end,
              children = function() return S:children() end,
            } }
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local one = S:child({run_id="r1", seq=1, label="same"})
            local two = S:child({run_id="r1", seq=2, label="same"})
            local seen = {}
            C.cordis:get("policy"):voter("tool/approval", function(call)
              local snap = store.snapshot()
              local origin = call.origin
              assert(origin.parent_session_id == S:info().session_id)
              for _, m in ipairs(snap.members) do
                if m.waiting == 0 then
                  assert(m.subagent_id == origin.subagent_id and m.path and not m.unassociated)
                  seen[#seen+1] = origin.task .. ":bound"
                end
              end
              return {decision="deny"}
            end)
            local b = mare.subagent.start({prompt="two", label="same", run_id="r1", seq=2,
                cwd=CWD, no_session=true, permission="read-only"})
            local a = mare.subagent.start({prompt="one", label="same", run_id="r1", seq=1,
                cwd=CWD, no_session=true, permission="read-only"})
            a:wait() b:wait()
            one.close() two.close()
            table.sort(seen)
            return table.concat(seen, ",")
        "#,
        )
        .await;
        assert_eq!(seen, "one:bound,two:bound");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn approval_without_identity_is_shown_separately_and_cleared_by_request_id() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", json!([])).await;
        exec(
            &h,
            r#"
            local ctx = {}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            C.cordis:get("bus"):emit("subagent/started", {subagent_id="one", label="same"})
            C.cordis:get("bus"):emit("agent/child-approval", {origin={label="same", run_id="r1"}, request_id=10})
            local snap = store.snapshot()
            assert(#snap.members == 2 and snap.waiting == 1)
            assert(snap.members[1].waiting == nil)
            assert(snap.members[2].waiting == 0 and snap.members[2].unassociated)
            C.cordis:get("bus"):emit("pipeline/approval-decided", {child_approval_id=11, call={id="same-call", origin={label="same"}}})
            assert(store.snapshot().waiting == 1)
            C.cordis:get("bus"):emit("pipeline/approval-decided", {child_approval_id=10, call={id="same-call", origin={label="same"}}})
            assert(store.snapshot().waiting == 0)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn duplicate_workflow_coordinates_do_not_merge_or_assign_outcomes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["subagent", "agents"]).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local ctx = {session = {
              info = function() return S:info() end,
              children = function() return S:children() end,
            }}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local one = S:child({run_id="r1", seq=1, label="one"})
            local two = S:child({run_id="r1", seq=1, label="two"})
            local three = S:child({run_id="r2", seq=1, label="one"})
            one.close() two.close() three.close()
            C.cordis:get("bus"):emit("workflow/agent-completed", {run_id="r1", seq=1, ok=false})
            local parent = S:info()
            C.cordis:get("bus"):emit("subagent/started", {subagent_id=one.session_id, label="one", run_id="r1", seq=1,
                parent_session_id=parent.session_id, parent_session_epoch=parent.session_epoch})
            C.cordis:get("bus"):emit("subagent/completed", {subagent_id=one.session_id, status="failed",
                parent_session_id=parent.session_id, parent_session_epoch=parent.session_epoch})
            local snap = store.snapshot()
            assert(#snap.members == 4)
            local keys = {}
            for _, m in ipairs(snap.members) do
              assert(not keys[m.key] and m.unassociated)
              keys[m.key] = true
              if m.path then assert(m.state == "done") else assert(m.state == "failed") end
            end
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn late_child_events_stay_with_the_session_that_started_them() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", json!([])).await;
        exec(
            &h,
            r#"
            local sid, epoch = "old", "old-epoch"
            local ctx = {session = {info = function() return {session_id=sid, session_epoch=epoch} end,
                                    children = function() return {} end}}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            C.cordis:get("bus"):emit("subagent/started", {subagent_id="100", label="same", parent_session_id="old", parent_session_epoch="old-epoch"})
            sid, epoch = "new", "new-epoch"
            C.cordis:get("bus"):emit("subagent/started", {subagent_id="101", label="same", parent_session_id="new", parent_session_epoch="new-epoch"})
            C.cordis:get("bus"):emit("subagent/started", {subagent_id="102", label="same", parent_session_id="old", parent_session_epoch="old-epoch"})
            C.cordis:get("bus"):emit("subagent/completed", {subagent_id="100", status="failed", parent_session_id="old", parent_session_epoch="old-epoch"})
            C.cordis:get("bus"):emit("agent/child-approval", {origin={subagent_id="100", label="same", parent_session_id="old", parent_session_epoch="old-epoch"}, request_id=9})
            C.cordis:get("bus"):emit("agent/approval-queue", {pending={{subagent_id="102", label="same", parent_session_id="old", parent_session_epoch="old-epoch"}}})
            local snap = store.snapshot()
            assert(#snap.members == 1 and snap.members[1].subagent_id == "101")
            assert(snap.members[1].state == "running" and snap.waiting == 0)
            sid, epoch = "old", "new-epoch"
            assert(#store.snapshot().members == 0)
            epoch = "old-epoch"
            snap = store.snapshot()
            assert(#snap.members == 2 and snap.waiting == 2)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn host_emitted_parent_session_epoch_matches_session_info_epoch() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", json!([{ "text": "done" }])).await;
        expose(&h);
        exec(
            &h,
            r#"
            local ctx = {
                session = {
                    info = function() return S:info() end,
                    children = function() return S:children() end,
                },
            }
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local seen_started
            local done_tx, done_rx = mare.task.channel()
            local expected_epoch = S:info().session_epoch
            assert(type(expected_epoch) == "string" and expected_epoch ~= "", "missing kernel session_epoch")
            C.cordis:on("subagent/started", function(ev) seen_started = ev end)
            C.cordis:on("subagent/completed", function(ev) done_tx:send(ev) end)
            local sub = C.cordis:get("subagent")
            local started = sub:start({ prompt = "say done", name = "epoch-probe" })
            local res = sub:wait(started.subagent_id, { timeout_ms = 15000 })
            assert(res.status == "completed", "unexpected child status: " .. tostring(res.status))
            local ok_ev, seen_completed = mare.task.timeout(5000, function() return done_rx:recv() end)
            assert(ok_ev and seen_completed, "timed out waiting for subagent/completed")
            assert(seen_started and seen_started.parent_session_epoch == expected_epoch,
                "started parent_session_epoch mismatch: " .. tostring(seen_started and seen_started.parent_session_epoch) .. " vs " .. expected_epoch)
            assert(seen_completed.parent_session_epoch == expected_epoch,
                "completed parent_session_epoch mismatch: " .. tostring(seen_completed.parent_session_epoch) .. " vs " .. expected_epoch)
            local found_member = false
            for _, m in ipairs(store.snapshot().members) do
                if m.subagent_id == started.subagent_id then
                    found_member = true
                    break
                end
            end
            assert(found_member, "expected agents.store snapshot members to include child " .. tostring(started.subagent_id))
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

/// The host owns both transcripts and their bindings, even if callers repeat or forge metadata.
#[tokio::test]
async fn host_records_and_binds_same_named_concurrent_children() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(write_script("bound.txt"))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({}));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let count: usize = eval(
            &h,
            r#"
            local ctx = {session = {info=function() return S:info() end,
                                    children=function() return S:children() end}}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local seen = {}
            assert(S:info().path == nil)
            C.cordis:get("policy"):voter("tool/approval", function(call)
              local origin = call.origin
              assert(origin.subagent_id and origin.subagent_id ~= "forged")
              for _, m in ipairs(store.snapshot().members) do
                if m.waiting == 0 then
                  assert(m.path and not m.unassociated and m.subagent_id == origin.subagent_id)
                  seen[origin.subagent_id] = true
                end
              end
              return {decision="deny"}
            end)
            local a = mare.subagent.start({prompt="first", label="same", run_id="repeat", seq=1,
                child_session_id="forged", cwd=CWD, no_session=true, permission="read-only"})
            local b = mare.subagent.start({prompt="second", label="same", run_id="repeat", seq=1,
                child_session_id="forged", cwd=CWD, no_session=true, permission="read-only"})
            assert(S:info().path ~= nil)
            a:wait() b:wait()
            local snap = store.snapshot()
            assert(#snap.members == 2)
            local n = 0
            for _, m in ipairs(snap.members) do
              assert(m.path and m.state == "done" and not m.unassociated)
              local entries = mare.store.read(m.path)
              assert(entries[1].session_id == m.subagent_id and #entries >= 4)
              if seen[m.subagent_id] then n = n + 1 end
            end
            return n
        "#,
        )
        .await;
        assert_eq!(count, 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn real_child_approval_and_end_do_not_cross_a_session_switch() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(write_script("switch.txt"))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({}));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            local ctx = {session = {info=function() return S:info() end,
                                    children=function() return S:children() end}}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local awaiting_tx, awaiting_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("policy"):voter("tool/approval", function()
              awaiting_tx:send(true)
              release_rx:recv()
              return {decision="deny"}
            end)
            local a = mare.subagent.start({prompt="old", label="same", cwd=CWD,
                no_session=true, permission="read-only"})
            local old = S:info()
            local reached = mare.task.timeout(30000, function() awaiting_rx:recv() end)
            assert(reached and store.snapshot().waiting == 1)
            S:create()
            local new = S:info()
            assert(new.session_id ~= old.session_id and new.session_epoch ~= old.session_epoch)
            assert(#store.snapshot().members == 0 and store.snapshot().waiting == 0)
            local finished, err = pcall(a.wait, a)
            assert(not finished and tostring(err):find("cancelled", 1, true), tostring(err))
            S:load({ path = old.path })
            local resumed = S:info()
            assert(resumed.session_id == old.session_id and resumed.session_epoch ~= old.session_epoch)
            local snap = store.snapshot()
            assert(#snap.members == 1 and snap.members[1].unassociated and snap.waiting == 0)
            release_tx:send(true)
            snap = store.snapshot()
            assert(#snap.members == 1 and snap.members[1].unassociated and snap.waiting == 0)
            S:create()
            assert(mare.store.delete(old.path), "the drained child must release the old session files")
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

/// Host-assigned `child_session_id` (both with a persisted child transcript and under
/// `--no-session`) is adopted as the child VM's own `session:info().session_id`,
/// `agent:activity().session_id`, and `session/start` identity, ignoring forged spawn specs.
#[tokio::test]
async fn child_vm_adopts_host_assigned_session_identity_and_ignores_forged_spec() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("child-identity-probe.lua"),
            r#"
          return {
            name = "child-identity-probe",
            inject = { "bus", "session", "agent" },
            contracts = { probe = { owner = "child-identity-probe", events = {
              ["child-identity"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
            } } },
            apply = function(ctx)
              if mare.mode ~= "child" then
                return
              end
              -- A replacement session provider needs the public id during plugin startup.
              local public_session_id = mare.args and mare.args.session_id or nil
              local start_id
              ctx:on("session/started", function(data)
                start_id = type(data) == "table" and data.session_id or nil
              end)
              ctx:on("turn/started", function()
                local sinfo = ctx.session:info()
                local act = ctx.agent:activity()
                ctx.bus:emit("probe/child-identity", {
                  session_id = sinfo and sinfo.session_id or nil,
                  activity_session_id = act and act.session_id or nil,
                  start_id = start_id,
                  public_session_id = public_session_id,
                  arg_child_session_id = mare.args.child_session_id,
                  arg_no_session = mare.args.no_session,
                  ephemeral = sinfo and sinfo.ephemeral == true or false,
                  session_name = sinfo and sinfo.name or nil,
                  arg_name = mare.args.name,
                  arg_trust = mare.args.trust,
                  arg_approve_mcps = mare.args.approve_mcps,
                  arg_agent_label = mare.args.agent_label,
                  arg_permission = mare.args.permission,
                  arg_permission_ceiling = mare.args.permission_ceiling,
                  cfg_preset = mare.config.plugins and mare.config.plugins.permissions and mare.config.plugins.permissions.preset,
                })
              end)
            end,
          }
        "#,
        )
        .unwrap();

        // 1. Persisted parent session: child transcript assigns `child_session_id`.
        let h_saved = env.host().user_plugins().faux(json!([{ "text": "done" }])).build().await;
        expose(&h_saved);
        global(&h_saved, "CWD", env.cwd.to_string_lossy().to_string());
        let (expected_id, got_sid, got_act, got_start): (String, String, String, String) = eval(
            &h_saved,
            r#"
            assert(S:info().children_dir ~= nil)
            local a = mare.subagent.start({
              prompt = "check identity",
              cwd = CWD,
              name = "child-display-name",
              no_session = true,
              permission = "read-only",
              child_session_id = "forged-child-id",
              session_id = "forged-session-id-12345",
              parent_session = "forged-parent-id",
              trust = "trusted",
              approve_mcps = true,
            })
            local seen
            while true do
              local ev = a:next_event()
              if not ev then break end
              if ev.name == "probe/child-identity" then seen = ev.payload break end
            end
            local done = a:wait()
            assert(done and done.text == "done")
            local listed = mare.subagent.list()
            local info = assert(listed[1])
            local child_id = assert(info.subagent_id)
            local got = mare.subagent.get(child_id)
            local binding = mare.subagent.identity(child_id)
            assert(got and got.subagent_id == child_id)
            assert(binding and binding.subagent_id == child_id)
            assert(seen.public_session_id == child_id,
              "public child session id must match host: " .. tostring(seen.public_session_id))
            assert(seen.arg_no_session == false and seen.ephemeral == false,
              "persisted child must not be ephemeral")
            assert(seen.arg_child_session_id == nil and seen.arg_name == nil
              and seen.arg_trust == nil and seen.arg_approve_mcps == nil and seen.arg_permission == nil,
              "spawn metadata must not leak into child startup args")
            assert(seen.arg_agent_label == "child-display-name" and seen.cfg_preset == "read-only"
              and seen.arg_permission_ceiling == "full-access",
              "child agent label, permission config, and ceiling must remain available")
            assert(seen.session_name == nil, "agent name must not become the child session name")
            return child_id, seen.session_id, seen.activity_session_id, seen.start_id
        "#,
        )
        .await;
        assert_ne!(expected_id, "forged-child-id");
        assert_ne!(expected_id, "forged-session-id-12345");
        assert_eq!(got_sid, expected_id, "child session:info().session_id must match child_session_id");
        assert_eq!(got_act, expected_id, "child agent:activity().session_id must match child_session_id");
        assert_eq!(got_start, expected_id, "child session/start id must match child_session_id");
        h_saved.shutdown().await;

        // 2. Ephemeral parent (`--no-session`): child VM still matches `done.session_id`.
        let h_ephem = env.host().user_plugins().no_session().faux(json!([{ "text": "done" }])).build().await;
        expose(&h_ephem);
        global(&h_ephem, "CWD", env.cwd.to_string_lossy().to_string());
        let (eph_expected, eph_sid, eph_act): (String, String, String) = eval(
            &h_ephem,
            r#"
            local a = mare.subagent.start({
              prompt = "check ephemeral identity",
              cwd = CWD,
              no_session = false,
              session_id = "forged-session-id-99999",
            })
            local seen
            while true do
              local ev = a:next_event()
              if not ev then break end
              if ev.name == "probe/child-identity" then seen = ev.payload break end
            end
            local done = a:wait()
            assert(done and done.text == "done")
            local listed = mare.subagent.list()
            local info = assert(listed[1])
            assert(seen.public_session_id == info.subagent_id,
              "public ephemeral child session id must match host: " .. tostring(seen.public_session_id))
            assert(seen.arg_no_session == true and seen.ephemeral == true,
              "child without a transcript must be ephemeral")
            return info.subagent_id, seen.session_id, seen.activity_session_id
        "#,
        )
        .await;
        assert_ne!(eph_expected, "forged-session-id-99999");
        assert_eq!(eph_sid, eph_expected);
        assert_eq!(eph_act, eph_expected);
        h_ephem.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_user_session_replacement_uses_public_child_identity() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("child-replacement-probe.lua"),
            r#"return {
              name = "child-replacement-probe", inject = { "bus", "session" },
              contracts = { probe = { owner = "child-replacement-probe", events = {
                ["child-replacement"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
              } } },
              apply = function(ctx)
                if mare.mode ~= "child" then return end
                local id_at_start = mare.args.session_id
                ctx:on("turn/started", function()
                  local session = ctx.session:info()
                  local status = mare.plugins.status("session")
                  ctx.bus:emit("probe/child-replacement", {
                    from_args = id_at_start,
                    from_service = session and session.session_id,
                    origin = status and status.origin,
                  })
                end)
              end,
            }"#,
        )
        .unwrap();
        // Start the parent with the built-in provider, then install the user replacement for the child.
        let h = env.host().user_plugins().no_session().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        std::fs::write(
            pdir.join("session.lua"),
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session_replacement.lua")),
        )
        .unwrap();

        let (host_id, public_id, service_id, origin): (String, Option<String>, String, String) = eval(
            &h,
            r#"
            local child = mare.subagent.start({ prompt = "check replacement", cwd = CWD })
            local seen
            while true do
              local ev = child:next_event()
              if not ev then break end
              if ev.name == "probe/child-replacement" then seen = ev.payload break end
            end
            local done = child:wait()
            assert(seen, "replacement session did not start: " .. mare.json.encode(done or {}))
            assert(done and done.text == "done", "child failed: " .. mare.json.encode(done or {}))
            return assert(mare.subagent.list()[1]).subagent_id, seen.from_args, seen.from_service, seen.origin
        "#,
        )
        .await;
        assert_eq!(origin, "user", "the child must use the user-provided session service");
        assert_eq!(public_id.as_deref(), Some(host_id.as_str()));
        assert_eq!(service_id, host_id);
        h.shutdown().await;
    })
    .await
}

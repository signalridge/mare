use super::*;

#[tokio::test]
async fn an_ephemeral_parent_does_not_create_a_child_transcript() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", json!([{ "text": "done" }])).await;
        let (n, path): (usize, Option<String>) = eval(
            &h,
            r#"
            local a = mare.subagent.start({prompt="ephemeral", label="same", cwd=CWD, no_session=true})
            a:wait()
            return #S:children(), S:info().path
        "#,
        )
        .await;
        assert_eq!(n, 0);
        assert!(path.is_none());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resumed_session_in_another_directory_keeps_its_children_with_the_parent() {
    local(async {
        let env = TestEnv::new().await;
        let parent_path = env.write_session("foreign.jsonl", &[]);
        let other = env.work.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let mut b = env.host().builtins(&["subagent", "agents"]).faux(json!([{ "text": "done" }]));
        b.opts.cwd = other.clone();
        let h = b.build().await;
        expose(&h);
        global(&h, "PARENT_PATH", parent_path.to_string_lossy().to_string());
        global(&h, "CWD", other.to_string_lossy().to_string());
        let paths: Vec<String> = eval(
            &h,
            r#"
            S:load({ path = PARENT_PATH })
            local manual = S:child({run_id="manual", seq=1, label="manual"})
            manual.close()
            local a = mare.subagent.start({prompt="go", label="auto", cwd=CWD, no_session=true})
            a:wait()
            local out = {}
            for _, f in ipairs(S:children()) do out[#out+1] = f.path end
            return out
        "#,
        )
        .await;
        assert_eq!(paths.len(), 2);
        let dir = parent_path.parent().unwrap();
        assert!(paths.iter().all(|p| std::path::Path::new(p).starts_with(dir.join("agents"))));
        h.shutdown().await;
        drop(h);
        mare_store::delete_session(&parent_path).unwrap();
        assert!(paths.iter().all(|p| !std::path::Path::new(p).exists()));
    })
    .await
}

#[tokio::test]
async fn host_child_writer_ignores_legacy_agent_persist_plugin_listeners() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("persist-probe.lua"),
            r#"
            return { name="persist-probe", apply=function(ctx)
              if mare.mode == "child" then
                ctx:on("agent/persist", function(ev)
                  ev.message.probe_intercepted = true
                  return true
                end, {prepend=true})
              end
            end }
        "#,
        )
        .unwrap();
        let ephemeral = env.host().faux(json!([{ "text": "done" }])).no_session().build().await;
        expose(&ephemeral);
        global(&ephemeral, "CWD", env.cwd.to_string_lossy().to_string());
        let hit: bool = eval(
            &ephemeral,
            r#"
            local a = mare.subagent.start({prompt="probe", cwd=CWD, no_session=true})
            local r = a:wait()
            return r.messages[1].probe_intercepted == true
        "#,
        )
        .await;
        assert!(!hit, "persistence uses the session SPI, never agent/persist bus listeners");
        ephemeral.shutdown().await;
        drop(ephemeral);
        let h = env.host().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (hit, count): (bool, usize) = eval(
            &h,
            r#"
            local a = mare.subagent.start({prompt="recorded", cwd=CWD, no_session=true})
            local r = a:wait()
            local files = S:children()
            local entries = mare.store.read(files[1].path)
            return r.messages[1].probe_intercepted == true, #entries
        "#,
        )
        .await;
        assert!(!hit, "the host's writer handles persistence via SPI");
        assert!(count >= 4, "the transcript contains messages");
        h.shutdown().await;
    })
    .await
}

/// The host's child-transcript writer overrides `session.persist_message` even when the child
/// VM's own session recorder is active (a child with its own session): the host transcript holds
/// the turn.
#[tokio::test]
async fn host_child_writer_answers_before_the_childs_own_session_recorder() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (entries, text): (usize, String) = eval(
            &h,
            r#"
            local a = mare.subagent.start({prompt="with its own recorder", cwd=CWD})
            local r = a:wait()
            local files = S:children()
            local entries = mare.store.read(files[#files].path)
            local text = {}
            for _, e in ipairs(entries) do
              if e.type == "message" then text[#text + 1] = mare.json.encode(e.message) end
            end
            return #entries, table.concat(text, "\n")
        "#,
        )
        .await;
        assert!(entries >= 4, "the host transcript holds the turn: {entries}");
        assert!(text.contains("with its own recorder"), "{text}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn same_named_children_finish_in_reverse_order_without_swapping_transcripts() {
    local(async {
        let env = TestEnv::new().await;
        // A file latch crosses the child VM boundary without sharing Lua userdata.
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("child-finish-gate.lua"),
            r#"
            return { name="child-finish-gate", 
              config_schema={gate_dir={type={"boolean","string"}, default=false, desc="where to signal"}},
              apply=function(ctx, cfg)
              if mare.mode ~= "child" or not cfg.gate_dir then return end
              ctx:on("turn/completed", function()
                assert(mare.fs.write(cfg.gate_dir .. "/slow-entered", "ready"))
                for _=1,3000 do
                  if mare.fs.read(cfg.gate_dir .. "/slow-release") then
                    assert(mare.fs.write(cfg.gate_dir .. "/slow-released", "observed"))
                    return
                  end
                  mare.task.sleep(10)
                end
                error("slow child was not released by fast child-end")
              end)
            end }
            "#,
        )
        .unwrap();
        let h = env.host().builtins(&["subagent", "agents"]).faux(json!([{ "text": "default" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let order: String = eval(
            &h,
            r#"
            local names, identities, ended = {}, {}, {}
            C.cordis:on("subagent/started", function(ev)
              identities[ev.subagent_id] = true
              for _, f in ipairs(S:children()) do
                local head = mare.store.read(f.path)[1]
                if head.session_id == ev.subagent_id then names[ev.subagent_id] = head.meta.task end
              end
            end)
            C.cordis:on("subagent/completed", function(ev)
              assert(identities[ev.subagent_id])
              if names[ev.subagent_id] == "slow" then
                assert(mare.fs.read(CWD .. "/slow-released"), "slow finished without release")
              end
              ended[#ended+1] = names[ev.subagent_id]
              if names[ev.subagent_id] == "fast" then
                assert(mare.fs.write(CWD .. "/slow-release", "go"))
              end
            end)
            local a = mare.subagent.start({prompt="slow", label="same", task="slow", run_id="run", seq=1,
              cwd=CWD, no_session=true, config={plugins={["child-finish-gate"]={gate_dir=CWD},
                ["provider-faux"]={script_json='[{"text":"slow result"}]'}}}})
            -- Observe that slow is held at its real turn/completed before starting fast.
            local entered = false
            for _=1,3000 do
              if mare.fs.read(CWD .. "/slow-entered") then entered = true break end
              mare.task.sleep(10)
            end
            assert(entered, "slow child did not enter its completion gate")
            assert(#ended == 0, "slow child completed before release")
            local b = mare.subagent.start({prompt="fast", label="same", task="fast", run_id="run", seq=2,
              cwd=CWD, no_session=true, config={plugins={["provider-faux"]={script_json='[{"text":"fast result"}]'}}}})
            a:wait() b:wait()
            for _=1,100 do if #ended == 2 then break end mare.task.sleep(10) end
            local results = {}
            for _, f in ipairs(S:children()) do
              local entries = mare.store.read(f.path)
              local task = entries[1].meta.task
              for _, e in ipairs(entries) do
                if e.type == "message" and e.message.role == "assistant" then
                  results[task] = e.message.content[1].text
                end
              end
            end
            assert(results.slow == "slow result" and results.fast == "fast result")
            return table.concat(ended, ",")
        "#,
        )
        .await;
        assert_eq!(order, "fast,slow");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_same_named_child_status_survives_session_reload() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "done", "delay_ms": 1000 }]))
            .build()
            .await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let parent_path: String = eval(
            &h,
            r#"
            local a = mare.subagent.start({prompt="cancel", label="same", task="cancel", run_id="r1", seq=1,
                cwd=CWD, no_session=true})
            local b = mare.subagent.start({prompt="finish", label="same", task="finish", run_id="r1", seq=2,
                cwd=CWD, no_session=true, config={plugins={["provider-faux"]={script_json='[{"text":"done"}]'}}}})
            a:cancel()
            assert(not pcall(a.wait, a))
            b:wait()
            return S:info().path
        "#,
        )
        .await;
        h.shutdown().await;
        drop(h);
        let resumed = env.host().builtins(&["subagent", "agents"]).build().await;
        expose(&resumed);
        global(&resumed, "PARENT_PATH", parent_path);
        let states: String = eval(
            &resumed,
            r#"
            S:load({ path = PARENT_PATH })
            local ctx = {session={info=function() return S:info() end,
                                  children=function() return S:children() end}}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local members = require("agents.store")(ctx, ctx).snapshot().members
            local states = {}
            for _, m in ipairs(members) do states[#states+1] = m.task .. ":" .. m.state end
            table.sort(states)
            return table.concat(states, ",")
        "#,
        )
        .await;
        assert_eq!(states, "cancel:failed,finish:done");
        resumed.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_creation_holds_the_parent_lock_until_the_child_exists() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["subagent", "agents"]).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            assert(S:info().path == nil)
            local child = S:child({run_id="run", seq=1, label="child"})
            local old = S:info().path
            assert(old and mare.fs.stat(old))
            local ok, err = mare.store.delete(old)
            assert(not ok and err:find("already open")) -- parent's writer holds the lock
            S:create()
            ok, err = mare.store.delete(old)
            assert(not ok and err:find("already open")) -- now the child writer holds its lock
            child.close() -- closing an old child is safe after switching sessions
            ok, err = mare.store.delete(old)
            assert(ok, err)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_locked_parent_cannot_gain_a_child_from_its_read_only_view() {
    local(async {
        let env = TestEnv::new().await;
        let parent = env.write_session("locked-parent.jsonl", &[]);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let parent_for_thread = parent.clone();
        let worker = std::thread::spawn(move || {
            let guard = mare_store::lock_session(&parent_for_thread).unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(guard);
        });
        locked_rx.recv().unwrap();
        let h = env.host().builtins(&["subagent", "agents"]).build().await;
        expose(&h);
        global(&h, "PARENT_PATH", parent.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.select = function() return 2 end -- view the locked parent read-only
            S:load({ path = PARENT_PATH })
            assert(S:info().readonly)
            local ok = pcall(S.child, S, {run_id="run", seq=1, label="child"})
            assert(not ok and #S:children() == 0)
        "#,
        )
        .await;
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn duplicate_child_session_identity_never_selects_a_transcript() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "done", "delay_ms": 1000 }]))
            .build()
            .await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            local ctx = {session={info=function() return S:info() end,
                                  children=function() return S:children() end}}
            function ctx:on(n, f) C.cordis:on(n, f) end
            local store = require("agents.store")(ctx, ctx)
            local copied = false
            C.cordis:on("subagent/started", function(ev)
              for _, f in ipairs(S:children()) do
                local head = mare.store.read(f.path)[1]
                if head.session_id == ev.subagent_id then
                  local path = f.path:gsub("[^/\\]+$", "999-duplicate.jsonl")
                  local fake = mare.store.open(path, {auxiliary=true})
                  fake:append(head, {sync=true})
                  fake:append({type="state", state="closed"}, {sync=true})
                  fake:close()
                  copied = true
                  break
                end
              end
            end)
            local a = mare.subagent.start({prompt="go", label="same", run_id="run", seq=1,
                cwd=CWD, no_session=true})
            for _=1,500 do if copied then break end mare.task.sleep(10) end
            assert(copied)
            local snap = store.snapshot()
            assert(#snap.members == 3)
            for _, m in ipairs(snap.members) do assert(m.unassociated) end
            a:cancel()
            pcall(a.wait, a)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn real_agent_tool_can_spawn_with_saved_and_ephemeral_parent_sessions() {
    for ephemeral in [false, true] {
        local(async {
            let env = TestEnv::new().await;
            let script = json!([
                { "tool_calls": [{ "name": "delegate_probe", "arguments": {} }] },
                { "text": "parent done" }
            ]);
            let mut builder = env.host().builtins(&["subagent", "agents"]).faux(script);
            if ephemeral {
                builder = builder.no_session();
            }
            let h = builder.build().await;
            expose(&h);
            global(&h, "CWD", env.cwd.to_string_lossy().to_string());
            assert!(eval::<bool>(&h, "return S:info().path == nil").await);
            exec(
                &h,
                r#"
                TOOL_ACTIVE, TOOL_PATH, TOOL_DONE, START_PATH = false, nil, nil, nil
                C.cordis:on("item/started", function(ev)
                  if ev.item and ev.item.type == "tool_call" and ev.item.name == "delegate_probe" then START_PATH = S:info().path end
                end)
                T:register("delegate_probe", {description="delegate", side_effect="read", idempotent=true,
                  parameters={type="object", properties={}}, execute=function()
                    TOOL_ACTIVE = mare.exec.current() ~= nil
                    TOOL_PATH = S:info().path
                    local a = mare.subagent.start({prompt="child", label="same", cwd=CWD, no_session=true,
                      config={plugins={["provider-faux"]={script_json='[{"text":"child done"}]'}}}})
                    TOOL_DONE = a:wait().text
                    return TOOL_DONE
                  end})
            "#,
            )
            .await;
            let result = h.prompt("delegate", json!({})).await.unwrap();
            assert!(result.get("error").is_none_or(serde_json::Value::is_null), "{result}");
            let (active, path, start_path, done, children): (bool, Option<String>, Option<String>, String, usize) =
                eval(&h, "return TOOL_ACTIVE, TOOL_PATH, START_PATH, TOOL_DONE, #S:children()").await;
            assert!(active, "spawn ran inside a real tool execution context");
            assert_eq!(done, "child done");
            if ephemeral {
                assert!(path.is_none() && start_path.is_none());
                assert_eq!(children, 0, "an ephemeral parent creates no transcript");
            } else {
                let path = path.expect("user message saved the parent before tool execution");
                assert_eq!(start_path.as_deref(), Some(path.as_str()));
                assert_eq!(children, 1, "the host recorded the child");
                let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
                assert!(entries.iter().any(|e| e["type"] == "message" && e["message"]["role"] == "user"));
                assert!(
                    entries
                        .iter()
                        .any(|e| e["type"] == "tool_state" && e["name"] == "delegate_probe" && e["state"] == "started")
                );
            }
            h.shutdown().await;
        })
        .await
    }
}

#[tokio::test]
async fn real_agent_tool_spawn_in_read_only_parent_does_not_create_a_transcript() {
    local(async {
        let env = TestEnv::new().await;
        let parent = env.write_session("readonly-parent.jsonl", &[]);
        let original = std::fs::read(&parent).unwrap();
        let guard = mare_store::lock_session(&parent).unwrap();
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([
                { "tool_calls": [{ "name": "delegate_probe", "arguments": {} }] },
                { "text": "parent done" }
            ]))
            .build()
            .await;
        expose(&h);
        global(&h, "PARENT_PATH", parent.to_string_lossy().to_string());
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.select = function() return 2 end
            S:load({ path = PARENT_PATH })
            assert(S:info().readonly)
            TOOL_ACTIVE, TOOL_DONE = false, nil
            T:register("delegate_probe", {description="delegate", side_effect="read", idempotent=true,
              parameters={type="object", properties={}}, execute=function()
                TOOL_ACTIVE = mare.exec.current() ~= nil
                local a = mare.subagent.start({prompt="child", label="same", cwd=CWD, no_session=true,
                  config={plugins={["provider-faux"]={script_json='[{"text":"child done"}]'}}}})
                TOOL_DONE = a:wait().text
                return TOOL_DONE
              end})
        "#,
        )
        .await;
        let result = h.prompt("delegate", json!({})).await.unwrap();
        assert!(result.get("error").is_none_or(serde_json::Value::is_null), "{result}");
        let (active, done, children): (bool, String, usize) =
            eval(&h, "return TOOL_ACTIVE, TOOL_DONE, #S:children()").await;
        assert!(active);
        assert_eq!(done, "child done");
        assert_eq!(children, 0);
        assert_eq!(std::fs::read(&parent).unwrap(), original);
        h.shutdown().await;
        drop(guard);
    })
    .await
}

#[tokio::test]
async fn direct_tool_without_saved_parent_rejects_recorded_spawn_without_creating_files() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["subagent", "agents"]).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (result, children, path): (String, usize, Option<String>) = eval(
            &h,
            r#"
            T:register("direct_probe", {description="delegate", side_effect="read", idempotent=true,
              parameters={type="object", properties={}}, execute=function()
                assert(mare.exec.current() ~= nil)
                local ok, err = pcall(mare.subagent.start, {prompt="child", cwd=CWD, no_session=true})
                return ok and "unexpected success" or tostring(err)
              end})
            local r = T:execute({id="direct", name="direct_probe", arguments={}}, {on_state=function() return true end})
            local text = type(r.content) == "string" and r.content or r.content[1].text
            return text, #S:children(), S:info().path
        "#,
        )
        .await;
        assert!(result.contains("parent session must be saved"), "{result}");
        assert_eq!(children, 0);
        assert!(path.is_none());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_transcript_input_error_is_not_accepted_by_the_ephemeral_recorder() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("bad-record.lua"),
            r#"
          return {name="bad-record", inject={"bus", "session"},
            contracts={probe={owner="bad-record", events={
              record={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode == "child" then
              ctx:on("turn/started", function()
                local attempts = 0
                local ev = setmetatable({}, {__index=function(_, k)
                  if k == "message" then
                    attempts = attempts + 1
                    if attempts == 1 then error("injected message lookup failure") end
                    return {role="user", content={{type="text", text="fallback would accept"}}}
                  end
                end})
                local ok, err = pcall(ctx.session.persist_message, ctx.session, ev)
                ctx.bus:emit("probe/record", {accepted = ok == true,
                  error = type(err) == "table" and (err.message or tostring(err)) or tostring(err)})
              end)
            end
          end}
        "#,
        )
        .unwrap();
        let h = env.host().builtins(&["subagent", "agents"]).faux(json!([{ "text": "done" }])).build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (accepted, error): (bool, String) = eval(
            &h,
            r#"
            local a = mare.subagent.start({prompt="go", cwd=CWD, no_session=true})
            local seen
            while true do
              local ev = a:next_event()
              if not ev then break end
              if ev.name == "probe/record" then seen = ev.payload break end
            end
            a:wait()
            return seen.accepted, seen.error
        "#,
        )
        .await;
        assert!(!accepted, "the in-memory recorder must not mask a failed host writer");
        assert!(error.contains("injected message lookup failure"), "{error}");
        h.shutdown().await;
    })
    .await
}

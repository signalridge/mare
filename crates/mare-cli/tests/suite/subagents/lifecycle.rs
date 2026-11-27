use super::*;

#[tokio::test]
async fn replacing_a_parent_session_drains_and_hides_its_subagents() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text": "a long running child response", "delay_ms": 1500 }]))
            .build()
            .await;
        expose(&h);
        let (leaked, visible, got, waited, steered, cancelled, status, notified): (
            usize,
            bool,
            bool,
            bool,
            bool,
            bool,
            String,
            bool,
        ) = eval(
            &h,
            r#"
                S:create()
                local svc = C.cordis:get("subagent")
                local finished = {}
                C.cordis:on("subagent/completed", function(ev) finished[ev.subagent_id] = true end)
                local child = mare.subagent.start({prompt = "hold"})
                local id = child:subagent_id()
                assert(mare.subagent.get(id).status == "running")
                S:create()
                local listed = #svc:list().data
                local visible = mare.subagent.get(id) ~= nil
                local got = pcall(svc.get, svc, id)
                local waited = pcall(svc.wait, svc, id, {timeout_ms = 0})
                local steered = pcall(svc.steer, svc, id, "from another session")
                local cancelled = pcall(svc.cancel, svc, id)
                -- On the old implementation the explicit cancel above releases the long stream.
                local ok, done = pcall(child.wait, child)
                local status = ok and done.status or tostring(done)
                return listed, visible, got, waited, steered, cancelled, status, finished[id] == true
                "#,
        )
        .await;
        assert_eq!(leaked, 0, "the new session must not list the old session's children");
        assert!(!visible && !got && !waited && !steered && !cancelled, "old child was reachable after replacement");
        assert!(status.contains("cancelled"), "session replacement must cancel and drain the child: {status}");
        assert!(notified, "the child's completion notification must be drained before publication");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn failed_session_load_does_not_cancel_a_running_child() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("locked-load-target.jsonl", &[msg("m1", None, "user", "other session")]);
        let _writer = mare_store::SessionFile::open(&path).unwrap();
        let h = env.host().faux(json!([{ "text":"slow", "delay_ms":60000 }])).build().await;
        expose(&h);
        global(&h, "LOCKED_LOAD", path.to_string_lossy().to_string());
        let (loaded, status): (bool, String) = eval(
            &h,
            r#"
          local handle = mare.subagent.start({prompt="active child"})
          local id = handle:subagent_id()
          local ok = pcall(S.load, S, {path=LOCKED_LOAD})
          local state = mare.subagent.get(id).status
          mare.subagent.cancel(id)
          pcall(handle.wait, handle)
          return ok, state
        "#,
        )
        .await;
        assert!(!loaded, "locked session must be rejected");
        assert_eq!(status, "running", "a failed load must preserve the current session's child");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn background_model_subagent_notice_does_not_cross_a_session_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(SUBAGENT_BUILTINS).faux(json!([
            {"tool_calls":[{"name":"subagent", "arguments":{"prompt":"S1-PRIVATE-CHILD", "name":"S1-worker", "wait":false}}]},
            {"text":"S1 complete"}, {"text":"S2 complete"}
        ])).config(json!({"agents":{"max_depth":1}})).build().await;
        expose(&h);
        exec(&h, r#"
          local spawn = mare.subagent.start
          mare.subagent.start = function(opts)
            opts.config = {plugins={["provider-faux"]={script_json='[{"text":"S1-PRIVATE-CHILD-OUTPUT","delay_ms":60000}]'}}}
            BACKGROUND_CHILD=spawn(opts)
            return BACKGROUND_CHILD
          end
        "#).await;
        let first = h.prompt("S1 request", json!({})).await.unwrap();
        assert_eq!(first["text"], "S1 complete", "{first}");
        let (old_id, new_id, child_id): (String, String, String) = eval(&h, r#"
          local parked = mare.task.timeout(10000, function()
            while true do
              local event = BACKGROUND_CHILD:next_event()
              if not event then return false end
              if event.name == "step/started" then return true end
            end
          end)
          assert(parked, "background child did not reach its request")
          local old = S:info().session_id
          local child = BACKGROUND_CHILD:subagent_id()
          A:notify("S1-QUEUED-NOTE", {wake=false})
          S:create()
          return old, S:info().session_id, child
        "#).await;
        assert_ne!(old_id, new_id);
        let second = h.prompt("S2 request", json!({})).await.unwrap();
        assert!(second["error"].is_null(), "{second}");
        let (history, log): (String, String) = eval(&h, r#"
          local complete, found = mare.task.timeout(10000, function()
            while true do
              local messages = mare.json.encode(A:messages())
              if messages:find("S2 request", 1, true) and not A:activity().busy then return true end
              mare.task.yield()
            end
          end)
          assert(complete and found, "replacement prompt did not complete")
          return mare.json.encode(A:list_messages().data), mare.json.encode(S:entries())
        "#).await;
        for text in [&history, &log] {
            assert!(!text.contains(&child_id) && !text.contains("S1-PRIVATE-CHILD") && !text.contains("S1-QUEUED-NOTE") && !text.contains("<subagent-output"),
                "the old child's notification leaked into the replacement session: {text}");
            assert!(text.contains("S2 request"), "replacement session prompt missing: response={second}, history={history}, log={log}");
        }
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn parent_cancel_and_user_interrupt_keep_distinct_transcript_sources() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{ "text": "a deliberately long response for cancellation", "delay_ms": 1500 }]))
            .build()
            .await;
        expose(&h);
        let (child_records, parent_records): (String, String) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local child = mare.subagent.start({prompt="child work"})
          local id = child:subagent_id()
          local entered = false
          while true do
            local ev = child:next_event()
            if not ev then break end
            if ev.name == "step/started" then entered = true break end
          end
          assert(entered, "child did not reach its model step")
          local path
          for _, entry in ipairs(S:children()) do
            if entry.name:find(id, 1, true) then path = entry.path end
          end
          assert(path, "child transcript missing")
          svc:cancel(id)
          pcall(child.wait, child)
          local records = mare.store.read(path)

          local step_tx, step_rx = mare.task.channel()
          local release = C.cordis:on("step/started", function() step_tx:send(true) end)
          local parent_task = mare.task.spawn(function() return A:wait(A:prompt("parent work").turn_id) end)
          assert(mare.task.timeout(5000, function() return step_rx:recv() end), "parent did not reach its model step")
          A:cancel()
          parent_task:wait()
          release()
          return mare.json.encode(records), mare.json.encode(S:entries())
        "#,
        )
        .await;
        let child: Value = serde_json::from_str(&child_records).unwrap();
        let parent: Value = serde_json::from_str(&parent_records).unwrap();
        let cancellation = |value: &Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|entry| entry.get("message"))
                .find(|message| message["turn_aborted"] == true)
                .cloned()
        };
        let child_cancel = cancellation(&child).unwrap_or_else(|| panic!("child interruption marker: {child_records}"));
        let user_cancel = cancellation(&parent).unwrap_or_else(|| panic!("user interruption marker: {parent_records}"));
        assert_eq!(child_cancel["cancel_reason"]["source"], "parent", "{child_cancel}");
        assert_eq!(user_cancel["cancel_reason"]["source"], "user", "{user_cancel}");
        assert!(!child_cancel.to_string().contains("The user interrupted"), "{child_cancel}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn host_shutdown_records_its_cancellation_in_the_child_transcript() {
    local(async {
        let env = TestEnv::new().await;
        let h =
            env.host().faux(json!([{ "text": "a deliberately long child response", "delay_ms": 1500 }])).build().await;
        expose(&h);
        let path: String = eval(
            &h,
            r#"
          local child = mare.subagent.start({prompt="child work"})
          local id = child:subagent_id()
          local entered = false
          while true do
            local ev = child:next_event()
            if not ev then break end
            if ev.name == "step/started" then entered = true break end
          end
          assert(entered, "child did not reach its model step")
          for _, entry in ipairs(S:children()) do
            if entry.name:find(id, 1, true) then return entry.path end
          end
          error("child transcript missing")
        "#,
        )
        .await;
        h.shutdown().await;
        let records = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let marker = records
            .iter()
            .filter_map(|entry| entry.get("message"))
            .find(|message| message["turn_aborted"] == true)
            .expect("child interruption marker");
        let mut reason = &marker["cancel_reason"];
        while reason.get("cause").is_some() {
            reason = &reason["cause"];
        }
        assert_eq!(reason["source"], "host", "{marker}");
        assert!(!marker.to_string().contains("The user interrupted"), "{marker}");
    })
    .await;
}

#[tokio::test]
async fn selecting_another_host_does_not_cancel_sibling_host_children() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "a long response for a running child", "delay_ms": 1500 }]);
        let h1 = env.host().faux(script.clone()).no_session().build().await;
        let h2 = env.host().faux(script).no_session().build().await;
        expose(&h1);
        expose(&h2);
        let manager = mare_host::manager::HostManager::new();
        let s1 = manager.register_host(h1.clone(), None, true).await.unwrap();
        let s2 = manager.register_host(h2.clone(), None, true).await.unwrap();
        let start = r#"
          CHILD = mare.subagent.start({prompt="work"})
          while true do
            local event = CHILD:next_event()
            assert(event, "child ended before its first step")
            if event.name == "step/started" then break end
          end
          return CHILD:subagent_id()
        "#;
        let id1: String = eval(&h1, start).await;
        let id2: String = eval(&h2, start).await;
        assert_ne!(id1, id2);
        // Selecting another runtime for display is a lookup; neither host is closed.
        assert_eq!(manager.session(&s2.session_id).unwrap().session_id, s2.session_id);
        assert_eq!(manager.session(&s1.session_id).unwrap().session_id, s1.session_id);
        assert_eq!(eval::<String>(&h1, "return CHILD:info().status").await, "running");
        assert_eq!(eval::<String>(&h2, "return CHILD:info().status").await, "running");
        manager.close_session(&s2.session_id).await.unwrap();
        let cancelled2: String =
            eval(&h2, "local ok, e = pcall(CHILD.wait, CHILD); return ok and 'completed' or tostring(e)").await;
        assert!(cancelled2.contains("cancelled"), "{cancelled2}");
        assert_eq!(eval::<String>(&h1, "return CHILD:info().status").await, "running");
        manager.close_session(&s1.session_id).await.unwrap();
        let cancelled1: String =
            eval(&h1, "local ok, e = pcall(CHILD.wait, CHILD); return ok and 'completed' or tostring(e)").await;
        assert!(cancelled1.contains("cancelled"), "{cancelled1}");
    })
    .await;
}

#[tokio::test]
async fn child_mcp_connections_close_independently_of_parent_and_sibling() {
    local(async {
        let env = TestEnv::new().await;
        let pid_log = env.home.path().join("mcp-pids.log");
        let log = env.home.path().join("mcp-calls.log");
        let release = env.cwd.join("release-sibling");
        let fake = json!({ "log": log, "pid_log": pid_log, "tools": ["echo"] });
        let spec = json!({ "name": "lifetime", "command": fake_mcp_bin(), "args": ["scripted", fake.to_string()] });
        let spec_literal = serde_json::to_string(&spec.to_string()).unwrap();
        let release_literal = serde_json::to_string(&release.to_string_lossy()).unwrap();
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("mcp-lifetime-probe.lua"),
            format!(
                r#"
          return {{ name="mcp-lifetime-probe", inject={{"tools", "bus"}},
            contracts = {{ probe = {{ owner = "mcp-lifetime-probe", events = {{
              ["mcp-ready"] = {{ doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = {{ type = "object", additionalProperties = true }}, example = {{}} }},
              ["mcp-result"] = {{ doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = {{ type = "object", additionalProperties = true }}, example = {{}} }},
            }} }} }}, apply=function(ctx)
            ctx.tools:register("mcp_lifetime_probe", {{description="exercise an independent MCP connection",
              side_effect="read", parameters={{type="object"}}, execute=function()
                local client = mare.mcp.connect(mare.json.decode({spec_literal}))
                local cleanup <close> = setmetatable({{}}, {{__close=function() client:close() end}})
                if mare.args.agent_label == "held" then
                  ctx.bus:emit("probe/mcp-ready", {{}})
                  while not mare.fs.exists({release_literal}) do mare.task.sleep(10) end
                end
                local result = client:call("echo", {{text=mare.args.agent_label or "unknown"}})
                ctx.bus:emit("probe/mcp-result", {{text=mare.json.encode(result)}})
                return {{content=mare.json.encode(result)}}
              end}})
          end }}
        "#
            ),
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .faux(json!([
                {"tool_calls":[{"name":"mcp_lifetime_probe", "arguments":{}}]}, {"text":"done"}
            ]))
            .build()
            .await;
        expose(&h);
        global(&h, "MCP_SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        exec(&h, "PARENT_MCP = mare.mcp.connect(MCP_SPEC)").await;
        exec(
            &h,
            r#"
          HELD = mare.subagent.start({prompt="held child", label="held"})
          local ready = false
          while true do
            local event = HELD:next_event()
            if not event then break end
            if event.name == "probe/mcp-ready" then ready = true break end
          end
          assert(ready, "sibling did not open its MCP connection")
        "#,
        )
        .await;
        let read_pids =
            || -> Vec<u32> { std::fs::read_to_string(&pid_log).unwrap().lines().map(|s| s.parse().unwrap()).collect() };
        let original = read_pids();
        assert_eq!(original.len(), 2, "parent and held child must own separate processes");
        exec(
            &h,
            r#"
          local fast = mare.subagent.start({prompt="fast child", label="fast"})
          fast:wait()
        "#,
        )
        .await;
        let all = read_pids();
        assert_eq!(all.len(), 3, "each Host must open its own MCP connection");
        let fast_pid = *all.iter().find(|pid| !original.contains(pid)).unwrap();
        wait_until(10, "finished child's MCP process to exit", || async { !pid_alive(fast_pid) }).await;
        assert!(original.iter().all(|pid| pid_alive(*pid)), "parent and sibling connections must remain alive");
        global(&h, "MCP_RELEASE", release.to_string_lossy().to_string());
        let (parent_reply, sibling_reply): (String, String) = eval(
            &h,
            r#"
          local parent = mare.json.encode(PARENT_MCP:call("echo", {text="parent-after-fast"}))
          mare.fs.write(MCP_RELEASE, "go")
          local sibling
          while true do
            local event = HELD:next_event()
            if not event then break end
            if event.name == "probe/mcp-result" then sibling = event.payload.text break end
          end
          assert(sibling, "sibling did not call MCP after the other child exited")
          HELD:wait()
          PARENT_MCP:close()
          return parent, sibling
        "#,
        )
        .await;
        assert!(parent_reply.contains("parent-after-fast"), "{parent_reply}");
        assert!(sibling_reply.contains("held"), "{sibling_reply}");
        h.shutdown().await;
        wait_until(10, "all MCP processes to exit", || async { all.iter().all(|pid| !pid_alive(*pid)) }).await;
    })
    .await;
}

#[tokio::test]
async fn child_progress_coalesces_per_identity_without_losing_completion() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("burst-progress.lua"),
            r#"
          return { name="burst-progress", inject={"tools", "bus"}, apply=function(ctx)
            ctx.tools:register("emit_progress_burst", {description="emit a status burst", side_effect="read",
              parameters={type="object"}, execute=function()
                for i=1,20 do
                  ctx.bus:emit("status/changed", {state="working", message="BURST:" .. i, session={}, seq=i})
                end
                return {content="done"}
              end})
          end }
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .faux(json!([
                {"tool_calls":[{"name":"emit_progress_burst", "arguments":{}}]}, {"text":"done"}
            ]))
            .build()
            .await;
        expose(&h);
        let (id1, id2, updates, completions): (String, String, String, usize) = eval(
            &h,
            r#"
          local seen, ends = mare.json.array(), {}
          local tx, rx = mare.task.channel()
          C.cordis:on("subagent/progress", function(event)
            if type(event.progress) == "table" and (event.progress.message or ""):find("BURST:", 1, true) then
              seen[#seen+1] = {id=event.subagent_id, message=event.progress.message}
            end
          end)
          C.cordis:on("subagent/completed", function(event) ends[event.subagent_id] = true tx:send(true) end)
          local a = mare.subagent.start({prompt="one"})
          local b = mare.subagent.start({prompt="two"})
          local id1, id2 = a:subagent_id(), b:subagent_id()
          a:wait() b:wait()
          assert(mare.task.timeout(5000, function() rx:recv(); rx:recv() end), "completion events missing")
          return id1, id2, mare.json.encode(seen), (ends[id1] and 1 or 0) + (ends[id2] and 1 or 0)
        "#,
        )
        .await;
        let updates: Value = serde_json::from_str(&updates).unwrap();
        let updates = updates.as_array().unwrap();
        for id in [&id1, &id2] {
            let child: Vec<_> = updates.iter().filter(|event| event["id"] == *id).collect();
            assert!(!child.is_empty(), "each child must publish its own progress: {updates:?}");
            assert!(child.len() < 20, "the burst must be coalesced: {updates:?}");
            assert_eq!(child.last().unwrap()["message"], "BURST:20", "{updates:?}");
        }
        assert_eq!(completions, 2, "terminal events must not be coalesced");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn child_runtime_failure_is_not_reported_as_a_cancellation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([{"error":"fixture failure", "retryable":false, "kind":"invalid_request"}]))
            .build()
            .await;
        expose(&h);
        let (status, error): (String, String) = eval(
            &h,
            r#"
          local svc = C.cordis:get("subagent")
          local child = svc:start({prompt="failure"})
          local result = svc:wait(child.subagent_id, {timeout_ms=10000})
          return result.status, type(result.error) == "table" and result.error.message or tostring(result.error or "")
        "#,
        )
        .await;
        assert_eq!(status, "failed", "{error}");
        assert!(error.contains("fixture failure"), "{error}");
        h.shutdown().await;
    })
    .await;
}

/// The writer is swapped to the kernel's /dev/full only in an isolated test process. The child
/// is held at a real approval while its transcript fd is changed, so its next message hits ENOSPC.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn child_transcript_disk_failure_after_tool_start_is_reported_without_claiming_an_ack() {
    const CHILD: &str = "MARE_TEST_CHILD_TRANSCRIPT_DISK_FAILURE";
    if std::env::var_os(CHILD).is_none() {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "suite::subagents::lifecycle::child_transcript_disk_failure_after_tool_start_is_reported_without_claiming_an_ack",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(40), command.output())
            .await
            .expect("isolated disk-failure test timed out")
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    local(async {
        use std::os::fd::AsRawFd;
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(write_script("fault-side-effect.txt"))
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({}));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            ASKED, RELEASE, DONE, DONE_OK, DONE_ERROR = false, false, false, nil, nil
            C.cordis:get("policy"):voter("tool/approval", function()
              ASKED = true
              while not RELEASE do mare.task.sleep(10) end
              return {decision="allow", grant="allow"}
            end)
            local a = mare.subagent.start({prompt="write", label="writer", cwd=CWD,
              no_session=true, permission="read-only"})
            mare.task.spawn(function()
              DONE_OK, DONE_ERROR = pcall(a.wait, a)
              DONE = true
            end)
        "#,
        )
        .await;
        wait_until(20, "child approval", || async { eval::<bool>(&h, "return ASKED").await }).await;
        let path: String = eval(&h, "return S:children()[1].path").await;
        let path = std::path::PathBuf::from(path);
        let transcript_fd = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .find_map(|entry| {
                let fd: i32 = entry.file_name().to_string_lossy().parse().ok()?;
                let target = std::fs::read_link(entry.path()).ok()?;
                if target != path {
                    return None;
                }
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                (flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDWR).then_some(fd)
            })
            .expect("open child transcript fd");
        let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
        // SAFETY: this is a dedicated subprocess, the child is paused before its next write,
        // and transcript_fd is the open descriptor owned by that child's SessionFile.
        assert_eq!(unsafe { libc::dup2(full.as_raw_fd(), transcript_fd) }, transcript_fd);
        exec(&h, "RELEASE = true").await;
        wait_until(20, "child to report transcript failure", || async { eval::<bool>(&h, "return DONE").await }).await;
        let (ok, error): (bool, String) = eval(&h, "return DONE_OK, tostring(DONE_ERROR)").await;
        assert!(!ok && error.contains("child transcript persistence failed"), "{error}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("fault-side-effect.txt")).unwrap(), "hi");
        let entries = mare_store::read_entries(&path).unwrap();
        assert!(entries.iter().any(|e| e["type"] == "message" && e["message"]["role"] == "assistant"));
        assert!(!entries.iter().any(|e| e["type"] == "tool_state"));
        assert!(!entries.iter().any(|e| e["type"] == "state" && e["state"] == "closed"));
        h.shutdown().await;
    })
    .await
}

/// Kill a real host while a child tool has performed a side effect but not returned. The
/// transcript is intentionally only a conversation record: no started/ack or recovery claim.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn killed_child_host_leaves_no_false_tool_recovery_evidence() {
    const CONTROL: &str = "MARE_TEST_CHILD_CRASH_CONTROL";
    if let Some(control) = std::env::var_os(CONTROL) {
        local(async {
            let control = std::path::PathBuf::from(control);
            let home = control.join("home");
            let cwd = control.join("work");
            std::fs::create_dir_all(home.join("config/plugins")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::write(
                home.join("config/plugins/crash-probe.lua"),
                r#"
              return {name="crash-probe", inject={"tools"}, apply=function(ctx)
                if mare.mode == "child" then
                  ctx.tools:register("crash_probe", {description="crash probe", side_effect="write",
                    idempotent=false, parameters={type="object", properties={}}, execute=function()
                      assert(mare.fs.write(mare.cwd .. "/side-effect", "written"))
                      while true do mare.task.sleep(1000) end
                    end})
                end
              end}
            "#,
            )
            .unwrap();
            // This process is exclusively the crash fixture.
            unsafe { std::env::set_var("MARE_HOME", &home) };
            mare_shared::ui::install_hooks(mare_tui::hooks());
            let h = mare_host::Host::new(mare_host::HostOptions {
                cwd: cwd.clone(),
                mode: mare_shared::ui::Mode::Print,
                config: json!({"model":"faux/echo", "plugins": {
                    "permissions":{"enabled":false},
                    "provider-faux":{"script_json":json!([{"tool_calls":[{"name":"crash_probe", "arguments":{}}]}]).to_string()}
                }}),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            expose(&h);
            global(&h, "CWD", cwd.to_string_lossy().to_string());
            exec(
                &h,
                r#"CH = mare.subagent.start({prompt="perform side effect", cwd=CWD,
                no_session=true, permission="full-access"})"#,
            )
            .await;
            let path: String = eval(&h, "return S:children()[1].path").await;
            std::fs::write(control.join("transcript-path"), path).unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(40)).await;
            panic!("crash fixture should have been killed");
        })
        .await;
        return;
    }
    let control = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "suite::subagents::lifecycle::killed_child_host_leaves_no_false_tool_recovery_evidence",
            "--nocapture",
        ])
        .env(CONTROL, control.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("crash fixture exited early: {status}");
            }
            if control.path().join("transcript-path").exists() && control.path().join("work/side-effect").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child did not reach its side effect");
    child.kill().await.unwrap();
    let path = std::fs::read_to_string(control.path().join("transcript-path")).unwrap();
    let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
    assert_eq!(std::fs::read_to_string(control.path().join("work/side-effect")).unwrap(), "written");
    assert!(entries.iter().any(|e| e["type"] == "message"
        && e["message"]["role"] == "assistant"
        && e["message"]["content"].as_array().is_some_and(|c| c.iter().any(|b| b["name"] == "crash_probe"))));
    assert!(!entries.iter().any(|e| e["type"] == "tool_state"));
    assert!(
        !entries
            .iter()
            .any(|e| e["type"] == "custom" && e["kind"].as_str().is_some_and(|k| k.starts_with("agent/request")))
    );
    assert!(!entries.iter().any(|e| e["type"] == "state" && e["state"] == "closed"));
    let _file = mare_store::SessionFile::open_existing(std::path::Path::new(&path), None).unwrap();
}

#[tokio::test]
async fn cancelling_child_clears_its_pending_parent_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", touch_script("cancelled-child.txt")).await;
        let result: String = tokio::time::timeout(
            std::time::Duration::from_secs(WAIT_SECS),
            eval(
                &h,
                r#"
            local asked, result = false, nil
            C.cordis:get("policy"):voter("tool/approval", function()
                asked = true
                mare.task.sleep(60000)
                return { decision = "allow", grant = "allow_session" }
            end, { timeout_ms = 0 })
            C.cordis:on("pipeline/approval-decided", function(ev) result = ev end)
            local child = mare.subagent.start({ prompt = "touch", cwd = CWD, no_session = true })
            while not asked do mare.task.sleep(5) end
            child:cancel()
            pcall(child.wait, child)
            while not result do mare.task.sleep(5) end
            return result.decision .. ":" .. result.source
        "#,
            ),
        )
        .await
        .expect("the cancelled approval remained pending");
        assert_eq!(result, "deny:cancelled");
        assert!(!env.cwd.join("cancelled-child.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn core_agent_admission_and_shutdown_cover_unawaited_children() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "done", "delay_ms": 60000 }]))
            .config(json!({"agents":{"max_active":1,"max_depth":1}}))
            .build()
            .await;
        let rejected: bool = eval(
            &h,
            r#"
            CHILD = mare.subagent.start({prompt="wait", no_session=true})
            local ok, err = pcall(mare.subagent.start, {prompt="over capacity", no_session=true})
            return not ok and tostring(err):find("agent_capacity_limit", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(rejected);
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown()).await.unwrap();
        let closed: bool = eval(
            &h,
            r#"
            local ok, err = pcall(mare.subagent.start, {prompt="after close", no_session=true})
            return not ok and tostring(err):find("agent_tree_closed", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(closed);
    })
    .await;
}

#[tokio::test]
async fn shutdown_drains_real_grandchildren_and_child_config_cannot_raise_depth() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("tree-probe.lua"),
            r#"
          return {name="tree-probe", apply=function(ctx)
            if mare.mode ~= "child" then return end
            local depth = mare.config.probe_depth or 1
            ctx:on("turn/started", function()
              local ok, err = pcall(mare.subagent.start, {prompt="descendant", no_session=true,
                config={probe_depth=depth+1, agents={max_active=999,max_depth=999}}})
              if not ok then
                assert(tostring(err):find("agent_depth_limit",1,true))
                assert(mare.fs.write(mare.cwd .. "/depth-rejected", "yes"))
              end
            end)
            ctx:on("app/shutdown", function()
              assert(mare.fs.write(mare.cwd .. "/closed-" .. depth, "yes"))
            end)
          end}
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "done", "delay_ms": 60000 }]))
            .config(json!({"agents":{"max_active":4,"max_depth":2},"plugins":{"permissions":{"enabled":false}}}))
            .build()
            .await;
        exec(&h, "CHILD = mare.subagent.start({prompt='root child',no_session=true})").await;
        let rejected = env.cwd.join("depth-rejected");
        wait_until(WAIT_SECS, "grandchild depth rejection", || {
            let rejected = rejected.clone();
            async move { rejected.exists() }
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), h.shutdown()).await.unwrap();
        assert!(env.cwd.join("closed-1").exists());
        assert!(env.cwd.join("closed-2").exists());
    })
    .await;
}

#[tokio::test]
async fn cancelling_first_real_child_wait_preserves_later_and_repeated_results() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "child completed", "delay_ms": 200 }]))
            .no_session()
            .build()
            .await;
        let (text, repeated, cancelled): (String, String, bool) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            eval(
                &h,
                r#"
            local child = mare.subagent.start({prompt="finish after waiter cancellation", no_session=true})
            local entered = false
            local closed_context
            local first = mare.exec.run({tool="wait-probe"}, function()
                return mare.task.spawn(function()
                    local closing <close> = setmetatable({}, {__close=function()
                        closed_context = (mare.exec.current() or {}).tool
                    end})
                    entered = true
                    return child:wait()
                end)
            end)
            while not entered do mare.task.sleep(1) end
            -- The task runs through child:wait() to its first suspension before this resumes.
            first:abort()
            local first_ok, first_err = pcall(first.wait, first)
            assert(closed_context == "wait-probe", "cancelled waiter closes under its inherited tool context")
            local result = child:wait()
            local again = child:wait()
            return result.text, again.text, not first_ok and tostring(first_err):find("cancel",1,true) ~= nil
            "#,
            ),
        )
        .await
        .expect("a cancelled first waiter must not block later waits");
        assert!(cancelled, "the original waiter was cancelled while the child remained live");
        assert_eq!(text, "child completed");
        assert_eq!(repeated, text);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn model_subagent_tool_wait_round_trip() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "subagent", "arguments": { "prompt": "child work", "name": "worker-1", "wait": true } }] },
            { "text": "parent done" }
        ]);
        let h = env
            .host().builtins(&["subagent", "agents"])
            .faux(script)
            .config(json!({ "agents": { "max_depth": 1 } }))
            .no_session()
            .build()
            .await;
        let res = h.prompt("delegate", json!({})).await.unwrap();
        assert_eq!(res["text"], "parent done");
        let summary: String = eval(
            &h,
            r#"
            local items = mare.subagent.list()
            assert(#items == 1, "expected 1 spawned subagent, got " .. #items)
            return items[1].name .. ":" .. items[1].status
            "#,
        )
        .await;
        assert_eq!(summary, "worker-1:completed");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn model_subagent_tool_list_steer_and_cancel() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "subagent_list", "arguments": {} }] },
            { "tool_calls": [{ "name": "subagent_steer", "arguments": { "id": "slow-worker", "text": "please wrap up" } }] },
            { "tool_calls": [{ "name": "subagent_cancel", "arguments": { "id": "slow-worker" } }] },
            { "tool_calls": [{ "name": "subagent_wait", "arguments": { "id": "slow-worker" } }] },
            { "text": "controlled" }
        ]);
        let h = env.host().builtins(&["subagent", "agents"]).faux(script).no_session().build().await;
        exec(
            &h,
            r#"
            SLOW = mare.subagent.start({
                prompt = "slow child",
                name = "slow-worker",
                no_session = true,
                config = { plugins = { ["provider-faux"] = { script_json = '[{"text":"slow done","delay_ms":60000}]' } } }
            })
            "#,
        )
        .await;
        let res = h.prompt("control subagent", json!({})).await.unwrap();
        assert_eq!(res["text"], "controlled");
        let status: String = eval(&h, "return mare.subagent.wait('slow-worker').status").await;
        assert_eq!(status, "cancelled");
        h.shutdown().await;
    })
    .await;
}

/// `subagent{wait=true}` returns the child's last non-empty answer and nothing else
/// (agent-loop corpus; reference implementation
/// assistant-output.spec.ts:39, :67; reference implementation :320):
/// not its reasoning, not text it wrote before calling a tool, not the tool's output. Parent and
/// child each run the faux script from its start (separate turn counters); the child cannot
/// nest (max_depth 1), so its first turn's subagent call fails and it goes on with the script.
#[tokio::test]
async fn model_subagent_wait_returns_only_the_childs_last_answer() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("note.txt"), "TOOL-OUTPUT\n").unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "subagent", "arguments": { "prompt": "child work", "name": "w", "wait": true } }] },
            { "reasoning": "CHILD-THINKING", "text": "EARLY-TEXT", "tool_calls": [{ "name": "read", "arguments": { "path": "note.txt" } }] },
            { "text": "FINAL-ANSWER" },
            { "text": "parent done" }
        ]);
        let h = env.host().builtins(&["subagent"]).faux(script).config(json!({ "agents": { "max_depth": 1 } })).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"SUB = {} C.cordis:on("item/completed", function(ev)
                 local it = ev.item
                 local m = it and it.message
                 if m and m.tool_name == "subagent" then SUB[#SUB + 1] = mare.json.encode(m.content) end
               end)"#,
        )
        .await;
        h.prompt("delegate", json!({})).await.unwrap();
        let results: Vec<String> = eval(&h, "return SUB").await;
        h.shutdown().await;
        assert_eq!(results.len(), 1, "{results:?}");
        let content: Value = serde_json::from_str(&results[0]).unwrap();
        let text = content.as_str().map(str::to_string).unwrap_or_else(|| {
            content.as_array().unwrap().iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")
        });
        assert_eq!(text, "FINAL-ANSWER", "the subagent result is the child's last answer only: {content}");
    })
    .await;
}

/// Bounds one phase of a crash-resume flow so a missing event fails fast with the phase name.
async fn within<T>(phase: &str, fut: impl std::future::Future<Output = T>) -> T {
    eprintln!("phase: {phase}");
    tokio::time::timeout(std::time::Duration::from_secs(30), fut).await.unwrap_or_else(|_| panic!("timed out: {phase}"))
}

/// A background subagent left running when the parent exits/crashes must be reconciled on resume:
/// the parent writes an `end` (`interrupted`) record, `subagent_list` / `subagent_wait` report
/// `interrupted` instead of `unknown subagent`, and the model receives a one-time `wake=false`
/// notification naming the stopped subagent and its transcript path.
#[tokio::test]
async fn resumed_session_reconciles_unclosed_background_subagent_and_notifies_model() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(json!([
                {"tool_calls": [{"name": "subagent", "arguments": {"prompt": "bg work", "name": "bg-worker", "description": "long background task", "wait": false}}]},
                {"text": "parent turn 1 done"}
            ]))
            .config(json!({"agents": {"max_depth": 1}}))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            local orig_start = mare.subagent.start
            mare.subagent.start = function(opts)
                opts.config = { plugins = { ["provider-faux"] = { script_json = '[{"text":"never","delay_ms":60000}]' } } }
                BG_CHILD = orig_start(opts)
                return BG_CHILD
            end
            "#,
        )
        .await;
        let first = within("first parent prompt", h.prompt("launch background subagent", json!({}))).await.unwrap();
        assert_eq!(first["text"], "parent turn 1 done", "{first}");

        // Wait until the child reaches its model step and capture the live on-disk state BEFORE
        // clean shutdown (simulating a parent crash while the background child is still in-flight).
        let (parent_path, child_id, child_path): (String, String, String) = within(
            "child reaches step/started",
            eval(
            &h,
            r#"
            local entered = mare.task.timeout(10000, function()
                while true do
                    local ev = BG_CHILD:next_event()
                    if not ev then return false end
                    if ev.name == "step/started" then return true end
                end
            end)
            assert(entered, "background child did not reach step/started")
            local id = BG_CHILD:subagent_id()
            local cpath = ""
            for _, entry in ipairs(S:children()) do
                if entry.name:find(id, 1, true) then cpath = entry.path break end
            end
            assert(cpath ~= "", "expected child transcript file on disk")
            return S:info().path, id, cpath
            "#,
            ),
        )
        .await;

        // Snapshot the state directory as it existed mid-run, shut down, then put the snapshot
        // back: the resumed host sees what a crashed parent left behind (an open subagent start).
        fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) {
            std::fs::create_dir_all(dst).unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                let sp = entry.path();
                let dp = dst.join(entry.file_name());
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    copy_dir_recursive(&sp, &dp);
                } else if kind.is_file() {
                    std::fs::copy(&sp, &dp).unwrap();
                }
            }
        }
        let state = env.home.path().join("state");
        let crashed = env.work.path().join("crashed-state");
        copy_dir_recursive(&state, &crashed);
        within("first host shutdown", h.shutdown()).await;
        // Release the first host and close the process-global SQLite session index handle before
        // deleting `state/` so Windows does not reject `remove_dir_all` with ERROR_SHARING_VIOLATION.
        drop(h);
        mare_store::index::close_idle(&state.join("sessions")).unwrap();
        std::fs::remove_dir_all(&state).unwrap();
        copy_dir_recursive(&crashed, &state);

        // Resume on the crashed snapshot.
        let h2 = within(
            "resume host build",
            env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .faux(json!([{"text": "resumed reply"}]))
                .args(json!({"resume": &parent_path}))
                .build(),
        )
        .await;
        expose(&h2);
        global(&h2, "LOST_ID", child_id.clone());
        let (list_json, wait_json, end_records_json): (String, String, String) = within(
            "list and wait on the lost subagent",
            eval(
            &h2,
            r#"
            local svc = C.cordis:get("subagent")
            local list_out = svc:list()
            local wait_out = svc:wait(LOST_ID)
            local recs = S:list_records("subagent").data
            return mare.json.encode(list_out), mare.json.encode(wait_out), mare.json.encode(recs)
            "#,
            ),
        )
        .await;
        let list_val: Value = serde_json::from_str(&list_json).unwrap();
        let wait_val: Value = serde_json::from_str(&wait_json).unwrap();
        let recs_val: Value = serde_json::from_str(&end_records_json).unwrap();
        assert!(
            list_val["data"].as_array().unwrap().iter().any(|item| {
                item["subagent_id"] == child_id && item["status"] == "interrupted"
            }),
            "subagent:list() must report interrupted subagent after crash resume: {list_val}"
        );
        assert_eq!(
            wait_val["status"], "interrupted",
            "subagent:wait() must report interrupted instead of unknown subagent: {wait_val}"
        );
        assert!(
            recs_val.as_array().unwrap().iter().any(|r| {
                r["event"] == "end" && r["subagent_id"] == child_id && r["status"] == "interrupted"
            }),
            "parent session must persist terminal end=interrupted subagent record on resume: {recs_val}"
        );

        // Next user prompt must deliver the wake=false notification containing subagent id, label, and transcript path.
        let resumed = within("prompt after resume", h2.prompt("continue after crash", json!({}))).await.unwrap();
        assert_eq!(resumed["text"], "resumed reply", "{resumed}");
        let msgs_json: String = eval(&h2, "return mare.json.encode(A:list_messages().data)").await;
        assert!(
            msgs_json.contains(&child_id)
                && msgs_json.contains("long background task")
                && msgs_json.contains(std::path::Path::new(&child_path).file_name().unwrap().to_str().unwrap()),
            "model must receive wake=false interrupted subagent notice with id, label, and transcript path: {msgs_json}"
        );
        within("resumed host shutdown", h2.shutdown()).await;

        // A second resume must not duplicate the interrupted notice or end record.
        let h3 = within(
            "second resume host build",
            env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .faux(json!([{"text": "second resume reply"}]))
                .args(json!({"resume": &parent_path}))
                .build(),
        )
        .await;
        expose(&h3);
        let resumed2 = within("prompt after second resume", h3.prompt("second prompt", json!({}))).await.unwrap();
        assert_eq!(resumed2["text"], "second resume reply", "{resumed2}");
        let end_count: i64 = eval(
            &h3,
            r#"
            local n = 0
            for _, r in ipairs(S:list_records("subagent").data) do
                if r.event == "end" and r.status == "interrupted" then n = n + 1 end
            end
            return n
            "#,
        )
        .await;
        assert_eq!(end_count, 1, "second resume must not duplicate interrupted end record");
        within("second resumed host shutdown", h3.shutdown()).await;
    })
    .await;
}

#[tokio::test]
async fn subagent_progress_continues_after_handle_is_dropped() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("drop-progress.lua"),
            r#"
          return { name="drop-progress", inject={"tools", "bus"}, apply=function(ctx)
            ctx.tools:register("emit_after_drop", {description="emit status after handle gc", side_effect="read",
              parameters={type="object"}, execute=function()
                ctx.bus:emit("status/changed", {state="working", message="AFTER_DROP", session={}, seq=1})
                return {content="done"}
              end})
          end }
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .faux(json!([
                {"tool_calls":[{"name":"emit_after_drop", "arguments":{}}]},
                {"text":"child finished"}
            ]))
            .build()
            .await;
        expose(&h);
        let (saw_after_drop, done_status): (bool, String) = within(
            "dropped handle progress test",
            eval(
                &h,
                r#"
                S:create()
                local saw_after_drop = false
                local tx, rx = mare.task.channel()
                C.cordis:on("subagent/progress", function(ev)
                    if type(ev.progress) == "table" and ev.progress.message == "AFTER_DROP" then
                        saw_after_drop = true
                    end
                end)
                C.cordis:on("subagent/completed", function() tx:send(true) end)
                local child = mare.subagent.start({ prompt = "emit progress after gc" })
                local id = child:subagent_id()
                -- Drop the handle without draining next_event() and force GC so ev_rx closes.
                child = nil
                collectgarbage("collect")
                collectgarbage("collect")
                local done = mare.subagent.wait(id)
                assert(mare.task.timeout(5000, function() return rx:recv() end), "completion event missing")
                return saw_after_drop, done.status
                "#,
            ),
        )
        .await;
        assert_eq!(done_status, "completed");
        assert!(saw_after_drop, "subagent/progress must still reach parent even after handle ev_rx is dropped");
        within("host shutdown", h.shutdown()).await;
    })
    .await;
}

#[tokio::test]
async fn subagent_drain_records_cancelled_and_service_start_tracks_end() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([{ "text": "slow-child", "delay_ms": 60000 }]))
            .build()
            .await;
        expose(&h);
        let (end_status, lost_count): (String, usize) = within(
            "subagent drain end record test",
            eval(
                &h,
                r#"
                S:create()
                local orig_id = S:info().session_id
                local SA = C.cordis:get("subagent")
                -- Start a slow background subagent through the service API.
                local info = SA:start({ prompt = "stay busy until session switch", label = "bg-drain" })
                assert(info and info.subagent_id, "subagent.start must return info")
                -- Switch to a new session: drains running subagent and must persist an end record with status=cancelled.
                S:switch({ kind = "new" })
                assert(S:info().session_id ~= orig_id)
                -- Resume the original session: drained subagent must NOT be misclassified as interrupted.
                S:switch({ kind = "load", session_id = orig_id })
                assert(S:info().session_id == orig_id)
                local end_status = ""
                for _, r in ipairs(require("mare_lib").all_records(S, "subagent") or {}) do
                    if r.subagent_id == info.subagent_id and r.event == "end" then
                        end_status = tostring(r.status)
                    end
                end
                local lost_n = 0
                for _, item in ipairs(SA:list().data or {}) do
                    if item.subagent_id == info.subagent_id and item.status == "interrupted" then
                        lost_n = lost_n + 1
                    end
                end
                return end_status, lost_n
                "#,
            ),
        )
        .await;
        assert_eq!(
            end_status, "cancelled",
            "session switch drain must persist status=cancelled end record for drained subagent"
        );
        assert_eq!(lost_count, 0, "resuming original session must not report drained subagent as interrupted");
        within("host shutdown", h.shutdown()).await;
    })
    .await;
}

#[tokio::test]
async fn direct_session_load_and_self_reload_succeed_while_background_subagent_is_running() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(json!([
                { "text": "seed-b" },
                { "text": "seed-a" },
                { "text": "slow-child-1", "delay_ms": 60000 },
                { "text": "slow-child-2", "delay_ms": 60000 }
            ]))
            .build()
            .await;
        expose(&h);
        let (loaded_b, end_status_1, reloaded_a, end_status_2): (bool, String, bool, String) = within(
            "direct session load with running background subagent",
            eval(
                &h,
                r#"
                S:create()
                local b_id = S:info().session_id
                assert(A:wait(A:prompt("seed b").turn_id).text == "seed-b")

                S:switch({ kind = "new" })
                local a_id = S:info().session_id
                assert(a_id ~= b_id)
                assert(A:wait(A:prompt("seed a").turn_id).text == "seed-a")

                local SA = C.cordis:get("subagent")
                local bg1 = SA:start({ prompt = "stay busy during direct load", label = "bg-direct-load" })
                assert(bg1 and bg1.subagent_id)

                -- Directly load session B while A has a running background subagent.
                S:switch({ kind = "load", session_id = b_id })
                local loaded_b = (S:info().session_id == b_id)

                -- Reload A and confirm the drained subagent's cancelled end record was persisted.
                S:switch({ kind = "load", session_id = a_id })
                assert(S:info().session_id == a_id)
                local end_status_1 = ""
                for _, r in ipairs(require("mare_lib").all_records(S, "subagent") or {}) do
                    if r.subagent_id == bg1.subagent_id and r.event == "end" then
                        end_status_1 = tostring(r.status)
                    end
                end

                -- Also verify self-reload (reuse == true) while a background subagent is running in A.
                local bg2 = SA:start({ prompt = "stay busy during self reload", label = "bg-self-reload" })
                assert(bg2 and bg2.subagent_id)
                S:switch({ kind = "load", session_id = a_id })
                local reloaded_a = (S:info().session_id == a_id)
                local end_status_2 = ""
                for _, r in ipairs(require("mare_lib").all_records(S, "subagent") or {}) do
                    if r.subagent_id == bg2.subagent_id and r.event == "end" then
                        end_status_2 = tostring(r.status)
                    end
                end
                return loaded_b, end_status_1, reloaded_a, end_status_2
                "#,
            ),
        )
        .await;
        assert!(loaded_b, "direct load of session B must succeed when A has a running subagent");
        assert_eq!(end_status_1, "cancelled", "drained subagent in A must record status=cancelled");
        assert!(reloaded_a, "self-reload of A must succeed when A has a running subagent");
        assert_eq!(end_status_2, "cancelled", "self-reload must refresh snapshot with drained end record");
        within("host shutdown", h.shutdown()).await;
    })
    .await;
}

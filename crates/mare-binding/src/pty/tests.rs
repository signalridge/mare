use super::*;

fn setup() -> Lua {
    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::proc::install(&lua, &mare, std::env::temp_dir().to_string_lossy().into_owned()).unwrap();
    crate::job::install(&lua, &mare, std::env::temp_dir().to_string_lossy().into_owned()).unwrap();
    install(&lua, &mare, std::env::temp_dir().to_string_lossy().into_owned()).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua
}

#[tokio::test]
async fn pty_text_is_stream_safe_and_policy_is_fenced_until_stop() {
    let lua = setup();
    lua.load(
        r#"
        local caps = mare.pty.capabilities()
        assert(caps.policy_fence and caps.streaming_text and caps.max_write_bytes == 16384)
        p = mare.pty.spawn {
            cmd = 'sh',
            args = { '-c', "printf 'safe\\033]52;c;secret'; printf '\\007tail\\n'; IFS= read -r _; exit 0" },
        }
        local ok, err = pcall(mare.proc.sandbox.set_current, { level = 'read-only' })
        assert(not ok and tostring(err):find('PTY_POLICY_BUSY', 1, true), tostring(err))
        assert(not pcall(p.write, p, string.rep('x', 16385)))
    "#,
    )
    .exec()
    .unwrap();
    let text: String = lua
        .load(
            r#"
        local out = ''
        while not out:find('tail', 1, true) do out = out .. (p:read_text() or '') end
        local r = p:stop { grace_ms = 0 }
        assert(r.stopped and not r.stop_unconfirmed)
        assert(not pcall(p.write, p, 'echo after-stop\n'))
        mare.proc.sandbox.set_current({ level = 'read-only' })
        assert(not pcall(p.write, p, 'echo forbidden\n'))
        return out
    "#,
        )
        .eval_async()
        .await
        .unwrap();
    assert!(text.contains("safetail"), "{text:?}");
    assert!(!text.contains("secret") && !text.contains('\x1b'));
}

#[test]
fn pty_input_queue_is_bounded_when_child_does_not_read() {
    let lua = setup();
    let p: mlua::AnyUserData = lua.load("return mare.pty.spawn{cmd='tail',args={'-f','/dev/null'}}").eval().unwrap();
    let p = p.borrow::<Pty>().unwrap();
    let chunk = vec![b'x'; MAX_WRITE];
    let mut rejected = false;
    for _ in 0..1000 {
        if p.write(&chunk).is_err() {
            rejected = true;
            break;
        }
    }
    assert!(rejected, "a stalled child must not accept unbounded queued input");
    p.kill().unwrap();
}

#[tokio::test]
async fn pty_foreground_signal_rejects_shell_kill_and_stops_foreground_command() {
    let lua = setup();
    lua.load(
        r#"
        p = mare.pty.spawn { cmd = 'sh', args = { '-c', "printf 'READY\\n'; exec tail -f /dev/null" } }
        local out = ''
        while not out:find('READY', 1, true) do out = out .. (p:read_text() or '') end
    "#,
    )
    .exec_async()
    .await
    .unwrap();
    let p: mlua::AnyUserData = lua.globals().get("p").unwrap();
    let p = p.borrow::<Pty>().unwrap();
    assert!(p.foreground(&lua).unwrap().is_some());
    if let Inner::Native { term, .. } = &p.inner {
        assert!(term.lock().unwrap().signal_foreground("SIGKILL", None).is_err());
        assert!(term.lock().unwrap().signal_foreground("NOT_A_SIGNAL", None).is_err());
        term.lock().unwrap().signal_foreground("SIGTERM", None).unwrap();
    }
    drop(p);
    lua.load("assert(p:stop{grace_ms=0}.stopped)").exec_async().await.unwrap();
}

#[tokio::test]
async fn pty_stop_includes_shell_job_control_groups() {
    let lua = setup();
    lua.load(
        r#"
        p = mare.pty.spawn { cmd = 'bash', args = { '--noprofile', '--norc', '-i' }, env = { PS1 = '', PS2 = '' } }
        p:write("tail -f /dev/null &\nprintf 'JOB_%s\\n' 'STARTED'\ntail -f /dev/null\n")
        local out = ''
        while not out:find('JOB_STARTED', 1, true) do out = out .. (p:read_text() or '') end
    "#,
    )
    .exec_async()
    .await
    .unwrap();
    let pid = lua.globals().get::<mlua::AnyUserData>("p").unwrap().borrow::<Pty>().unwrap().pid();
    assert!(platform::tree_alive(pid));
    lua.load("local r=p:stop{grace_ms=0}; assert(r.stopped, 'job-control tree was not confirmed stopped')")
        .exec_async()
        .await
        .unwrap();
    assert!(!crate::trees::tree_has_live_member(pid.unwrap()));
}

#[tokio::test]
async fn persistent_pty_rejects_a_policy_lift_without_starting_a_process() {
    let lua = setup();
    lua.load("mare.proc.sandbox.set_current{level='read-only'}").exec().unwrap();
    let rejected: bool = lua
        .load(
            r#"
        local ok, err = pcall(function()
            mare.exec.run({tool='test',sandbox='danger-full-access'},function()
                return mare.pty.spawn{cmd='sh',args={'-c','exit 0'}}
            end)
        end)
        return not ok and tostring(err):find('PTY_POLICY',1,true) ~= nil
    "#,
        )
        .eval_async()
        .await
        .unwrap();
    assert!(rejected, "single-call authority must not become a persistent input channel");
}

#[tokio::test]
async fn pty_exit_status_preserves_real_signal() {
    let lua = setup();
    let status: Table = lua
        .load(
            r#"
        local p = mare.pty.spawn { cmd = 'sh', args = { '-c', 'kill -TERM $$' } }
        while p:read_text() do end
        return assert(p:exit_status())
    "#,
        )
        .eval_async()
        .await
        .unwrap();
    assert_eq!(status.get::<Option<u32>>("code").unwrap(), None);
    assert_eq!(status.get::<i32>("signal").unwrap(), libc::SIGTERM);
}

#[tokio::test]
async fn pty_emulation_answers_queries_and_keeps_raw_mode_opt_in() {
    let lua = setup();
    let text: String = lua
        .load(
            r#"
        local p = mare.pty.spawn{cmd='sh',args={'-c',"stty raw -echo; printf 'abc\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -t x1"},emulate=true}
        local text = ""
        while true do local s = p:read_text(); if not s then break end; text = text .. s end
        assert(p:view().screen:find("abc",1,true))
        assert(p:exit_status().code == 0)
        local raw = mare.pty.spawn{cmd='sh',args={'-c','exit 0'}}
        assert(not pcall(raw.view, raw))
        raw:stop{grace_ms=0}
        return text
    "#,
        )
        .eval_async()
        .await
        .unwrap();
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(normalized.contains("1b 5b 31 3b 34 52"), "{text}");
}

#[tokio::test]
async fn cached_pty_methods_cannot_escape_ephemeral_authority() {
    let lua = setup();
    lua.load(
        r#"
        local p=mare.pty.spawn{cmd='tail',args={'-f','/dev/null'},emulate=true}
        local write,read,stop,kill,resize,signal,view,page=p.write,p.read,p.stop,p.kill,p.resize,p.signal,p.view,p.page
        mare.exec.run({tool="inspect",lifetime="ephemeral-read-only",sandbox={level="read-only"}},function()
            for _,f in ipairs({
                function() mare.pty.spawn{cmd="sh"} end,
                function() write(p,"wrong\n") end,
                function() read(p) end,
                function() stop(p,{grace_ms=0}) end,
                function() kill(p) end,
                function() resize(p,80,24) end,
                function() signal(p,"SIGINT") end,
                function() view(p) end,
                function() page(p,{}) end,
            }) do local ok,err=pcall(f); assert(not ok,tostring(err)) end
        end)
        assert(p:try_wait()==nil)
        assert(p:stop{grace_ms=0}.stopped)
    "#,
    )
    .exec_async()
    .await
    .unwrap();
}

#[tokio::test]
async fn proc_start_interactive_tty_and_stdin_jobs_are_policy_fenced_and_reject_lifts() {
    let lua = setup();
    lua.load(
        r#"
        mare.proc.sandbox.set_current({ level = 'read-only' })
        -- Per-call elevated contexts must not spawn persistent writable PTY or stdin jobs.
        for _, mode in ipairs({
            { cmd = 'sh', args = { '-c', 'exit 0' }, tty = true },
            { cmd = 'sh', args = { '-c', 'exit 0' }, stdin = true },
        }) do
            local ok, err = pcall(function()
                mare.exec.run({ tool = 'test', sandbox = 'danger-full-access' }, function()
                    return mare.proc.start(mode)
                end)
            end)
            assert(not ok and tostring(err):find('PTY_POLICY', 1, true), 'expected PTY_POLICY: ' .. tostring(err))
        end

        -- Both tty=true and stdin=true jobs must fence sandbox policy changes with PTY_POLICY_BUSY
        -- and reject writes after a policy generation change with PTY_POLICY_CHANGED.
        for _, spec in ipairs({
            { cmd = 'sh', args = { '-c', 'IFS= read -r _; exit 0' }, tty = true },
            { cmd = 'sh', args = { '-c', 'IFS= read -r _; exit 0' }, stdin = true },
        }) do
            mare.proc.sandbox.set_current({ level = 'workspace-write' })
            local j = mare.proc.start(spec)
            local ok_busy, err_busy = pcall(mare.proc.sandbox.set_current, { level = 'read-only' })
            assert(not ok_busy and tostring(err_busy):find('PTY_POLICY_BUSY', 1, true), tostring(err_busy))
            -- Writing inside a different exec_ctx policy must also be rejected by lease.check.
            local ok_ctx, err_ctx = pcall(function()
                mare.exec.run({ tool = 'test', sandbox = { level = 'read-only' } }, function()
                    return j:write('blocked\n')
                end)
            end)
            assert(not ok_ctx and tostring(err_ctx):find('PTY_POLICY_CHANGED', 1, true), tostring(err_ctx))
            j:stop({ grace_ms = 0 })
            mare.proc.sandbox.set_current({ level = 'read-only' })
        end

        -- Opt-in on_policy_change = "stop" allows policy changes by stopping the job and
        -- invalidating subsequent writes.
        mare.proc.sandbox.set_current({ level = 'workspace-write' })
        local auto = mare.proc.start({ cmd = 'sh', args = { '-c', 'IFS= read -r _; exit 0' }, stdin = true, on_policy_change = 'stop' })
        mare.proc.sandbox.set_current({ level = 'read-only' })
        local ok_after, err_after = pcall(auto.write, auto, 'late\n')
        assert(not ok_after, 'write after policy change must fail: ' .. tostring(err_after))
        auto:wait({ timeout_ms = 3000 })
        assert(auto:status().state == 'exited', 'auto-stopped job must exit on policy change')
    "#,
    )
    .exec_async()
    .await
    .unwrap();
}

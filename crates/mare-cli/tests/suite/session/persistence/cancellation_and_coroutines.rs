//! Cancelled tool/host persistence requests and coroutine scope confinement.

use super::*;

/// A real plugin ctx for a hand-built session store, injected like the session plugin that owns
/// the store in production (`preset/plugins/session.lua`: agent, task, bus, policy — the store reads
/// `ctx.agent`, store_switch.lua) plus `ui`, as `ctx_probes` gives. Handed to the test as
/// `mare._pctx.store`.
fn store_probe(env: &TestEnv) {
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ctx-store.lua"),
        r#"return { name = "ctx-store", inject = { "agent", "task", "bus", "policy", "ui" },
  apply = function(ctx) mare._pctx = mare._pctx or {}; mare._pctx.store = ctx end }"#,
    )
    .unwrap();
}

async fn probe_cancelled_tool_writer_requests(nested: bool) {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        h.prompt("seed", json!({})).await.unwrap();
        global(&h, "NESTED", nested);
        let (queued_record, queued_child, running_closed, delivered_closed): (i64, i64, bool, bool) = eval(&h, r#"
            local open = mare.store.open
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            mare.store.open = function(path, opts)
              if opts and opts.auxiliary and path:find('001%-running') then
                entered_tx:send(true); release_rx:recv()
              end
              return open(path, opts)
            end
            local function launch(id, fn)
              assert(type(S:record_tool_state({tool_call_id=id, name='probe', state='started', durable=true})) == 'string')
              return mare.task.spawn(function()
                return mare.exec.run({tool='probe', tool_call_id=id}, function()
                  if not NESTED then return fn() end
                  local co = coroutine.create(function() return coroutine.wrap(fn)() end)
                  local ok, result = coroutine.resume(co)
                  assert(ok, result)
                  return result
                end)
              end)
            end
            local running = launch('run', function() return S:child({run_id='cancel', seq=1, label='running'}) end)
            assert(mare.task.timeout(2000, function() entered_rx:recv() end), 'worker did not enter open')
            local record = launch('record', function() return S:add_record('cancel-queued', {}, {persist=true}) end)
            local child = launch('child', function() return S:child({run_id='cancel', seq=2, label='queued'}) end)
            mare.task.sleep(30)
            record:abort(); child:abort(); running:abort()
            pcall(record.wait, record); pcall(child.wait, child); pcall(running.wait, running)
            release_tx:send(true)
            mare.task.sleep(50)
            mare.store.open = open
            local list = S:children('cancel')
            local running_closed, queued_child = true, 0
            for _, c in ipairs(list) do
              if c.path:find('001%-running') then
                local running, closed = false, false
                for _, e in ipairs(mare.store.read(c.path)) do
                  if e.type == 'state' and e.state == 'running' then running = true end
                  if e.type == 'state' and e.state == 'closed' then closed = true end
                end
                running_closed = not running or closed
                local ok, file = pcall(mare.store.open, c.path, {auxiliary=true})
                if ok then file:close() end
                running_closed = running_closed and ok
              elseif c.path:find('002%-queued') then
                queued_child = queued_child + 1
              end
            end
            local channel = mare.task.channel
            local ready_tx, ready_rx = channel()
            mare.task.channel = function(...)
              local tx, rx = channel(...)
              if mare.exec.current() then
                mare.task.channel = channel
                local original = rx
                rx = { recv = function() local result = original:recv(); ready_tx:send(true); mare.task.sleep(10000); return result end }
              end
              return tx, rx
            end
            local delivered = launch('delivered', function() return S:child({run_id='cancel', seq=3, label='delivered'}) end)
            assert(mare.task.timeout(2000, function() ready_rx:recv() end), 'worker did not deliver')
            delivered:abort(); pcall(delivered.wait, delivered)
            mare.task.sleep(50)
            local delivered_closed = false
            for _, c in ipairs(S:children('cancel')) do
              if c.path:find('003%-delivered') then
                for _, e in ipairs(mare.store.read(c.path)) do
                  if e.type == 'state' and e.state == 'closed' then delivered_closed = true end
                end
              end
            end
            return #S:list_records('cancel-queued').data, queued_child, running_closed, delivered_closed
        "#).await;
        assert_eq!((queued_record, queued_child, running_closed, delivered_closed), (0, 0, true, true));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancelled_tool_writer_requests_do_not_outlive_their_callers() {
    probe_cancelled_tool_writer_requests(false).await;
}

#[tokio::test]
async fn nested_coroutines_cancel_tool_writer_requests_and_handoff() {
    probe_cancelled_tool_writer_requests(true).await;
}

#[tokio::test]
async fn cancelled_host_record_during_attachment_write_stops_before_record_commit() {
    local(async {
        let env = TestEnv::new().await;
    // the store is built for a plugin ctx (it spawns its writer there): a real one
        store_probe(&env);
        let h = env.host().user_plugins().build().await;
        expose(&h);
        exec(&h, "PC = mare._pctx.store").await;
        global(&h, "STORE_SOURCE", include_str!("../../../../../mare-plugin/preset/plugins/lua/session/store.lua"));
        global(&h, "TEST_PATH", env.cwd.join("cancel-image.jsonl").to_string_lossy().to_string());
        let (records, no_file, attachment_exists): (i64, bool, bool) = eval(&h, r#"
            local image = { type='message', message={ role='user', content={{type='image', mime='image/png', data='SU1BR0U='}} } }
            local state = {id='image-test', generation='image-gen', planned=TEST_PATH, list={image}, entries={}}
            local store = assert(load(STORE_SOURCE, '@session/store.lua'))()({ctx=PC, S=state, dir='.'})
            local write = mare.fs.write
            local ready_tx, ready_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local image_path
            mare.fs.write = function(path, ...)
              local result = write(path, ...)
              if path:find('cancel%-image%.attachments/') then
                image_path = path; ready_tx:send(true); release_rx:recv()
              end
              return result
            end
            assert(type(S:record_tool_state({tool_call_id='image', name='probe', state='started', durable=true})) == 'string')
            local task = mare.task.spawn(function()
              mare.exec.run({tool='probe', tool_call_id='image'}, function()
                return store.run_host(function(valid)
                  return store.add({type='custom', kind='cancel-image'}, true, nil, valid)
                end, {id=state.id, generation=state.generation})
              end)
            end)
            assert(mare.task.timeout(2000, function() ready_rx:recv() end), 'attachment did not start')
            task:abort(); pcall(task.wait, task)
            release_tx:send(true)
            mare.task.sleep(50)
            mare.fs.write = write
            local count = 0
            for _, entry in ipairs(state.list) do if entry.kind == 'cancel-image' then count = count + 1 end end
            return count, state.file == nil and state.path == nil, mare.fs.exists(image_path)
        "#).await;
        assert_eq!(records, 0);
        assert!(no_file, "the interrupted parent writer must be released");
        assert!(attachment_exists, "the already issued attachment write is not rolled back");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancelling_a_tool_spawn_closes_its_scope_under_the_tool_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "DENIED_PATH", env.cwd.join("cancelled-close-escape").to_string_lossy().to_string());
        let (closed, confined): (bool, bool) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current('read-only')
            local closed, confined = false, false
            mare.exec.run({tool='probe'}, function()
              local ready_tx, ready_rx = mare.task.channel()
              local task = mare.task.spawn(function()
                local guard <close> = setmetatable({}, {__close=function()
                  closed = mare.exec.current() ~= nil
                  confined = not pcall(mare.fs.write, DENIED_PATH, 'unsafe')
                end})
                ready_tx:send(true)
                mare.task.sleep(10000)
              end)
              ready_rx:recv()
              task:abort(); pcall(task.wait, task)
            end)
            return closed, confined
        "#,
        )
        .await;
        assert!(closed && confined);
        assert!(!env.cwd.join("cancelled-close-escape").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_or_replaced_host_request_never_starts_the_next_attachment() {
    local(async {
        let env = TestEnv::new().await;
    // the store is built for a plugin ctx (it spawns its writer there): a real one
        store_probe(&env);
        let h = env.host().user_plugins().build().await;
        expose(&h);
        exec(&h, "PC = mare._pctx.store").await;
        global(&h, "STORE_SOURCE", include_str!("../../../../../mare-plugin/preset/plugins/lua/session/store.lua"));
        global(&h, "TEST_DIR", env.cwd.to_string_lossy().to_string());
        let (cancel_count, owner_count, first_written, released): (i64, i64, bool, bool) = eval(&h, r#"
            local function probe(mode)
              local image = {type='message', message={role='user', content={
                {type='image', mime='image/png', data='RklSU1Q='},
                {type='image', mime='image/png', data='U0VDT05E'},
              }}}
              local state = {id='image-test', generation='image-gen', planned=TEST_DIR .. '/' .. mode .. '.jsonl', list={image}, entries={}}
              local store = assert(load(STORE_SOURCE, '@session/store.lua'))()({ctx=PC, S=state, dir='.'})
              local ready_tx, ready_rx = mare.task.channel()
              local release_tx, release_rx = mare.task.channel()
              local done_tx, done_rx = mare.task.channel()
              local write, paths = mare.fs.write, {}
              mare.fs.write = function(path, ...)
                local result = write(path, ...)
                if path:find(mode .. '%.attachments/') then
                  paths[#paths+1] = path
                  if #paths == 1 then ready_tx:send(true); release_rx:recv() end
                end
                return result
              end
              local call_id = 'image-' .. mode
              assert(type(S:record_tool_state({tool_call_id=call_id, name='probe', state='started', durable=true})) == 'string')
              local task = mare.task.spawn(function()
                pcall(mare.exec.run, {tool='probe', tool_call_id=call_id}, function()
                  return store.run_host(function(valid)
                    return store.add({type='custom', kind='cancel-image'}, true, nil, valid)
                  end, {id=state.id, generation=state.generation})
                end)
                done_tx:send(true)
              end)
              assert(mare.task.timeout(2000, function() ready_rx:recv() end), 'first attachment did not start')
              if mode == 'cancel' then task:abort(); pcall(task.wait, task) else state.generation = 'replacement'; state.path = nil end
              release_tx:send(true)
              if mode == 'owner' then assert(mare.task.timeout(2000, function() done_rx:recv() end)) else mare.task.sleep(50) end
              mare.fs.write = write
              return #paths, mare.fs.exists(paths[1]), state.file == nil and state.path == nil
            end
            local a, aw, ar = probe('cancel')
            local b, bw, br = probe('owner')
            return a, b, aw and bw, ar and br
        "#).await;
        assert_eq!((cancel_count, owner_count), (1, 1), "no later attachment should be issued");
        assert!(first_written, "already issued writes remain on disk");
        assert!(released, "the interrupted writer must be released");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn finished_tool_coroutines_remain_dead_after_success_and_error() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (success, failed, success_resume, failed_resume): (String, String, bool, bool) = eval(
            &h,
            r#"
            local success, failed
            mare.exec.run({tool='probe'}, function() success = coroutine.running() end)
            pcall(mare.exec.run, {tool='probe'}, function() failed = coroutine.running(); error('expected') end)
            local success_status, failed_status = coroutine.status(success), coroutine.status(failed)
            return success_status, failed_status, coroutine.resume(success), coroutine.resume(failed)
        "#,
        )
        .await;
        assert_eq!((success.as_str(), failed.as_str(), success_resume, failed_resume), ("dead", "dead", false, false));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nested_coroutine_yields_and_cancelled_closers_keep_tool_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "DENIED_PATH", env.cwd.join("nested-close-escape").to_string_lossy().to_string());
        let (manual, closed, confined): (bool, bool, bool) = eval(&h, r#"
            mare.proc.sandbox.set_current('read-only')
            local manual = mare.exec.run({tool='probe'}, function()
              local co = coroutine.create(function()
                local value = coroutine.yield('pause')
                mare.task.sleep(1)
                return value
              end)
              local a, first = coroutine.resume(co)
              local status = coroutine.status(co)
              local b, last = coroutine.resume(co, 'resumed')
              return a and first == 'pause' and status == 'suspended' and b and last == 'resumed' and coroutine.status(co) == 'dead'
            end)
            local closed, confined = false, false
            local ready_tx, ready_rx = mare.task.channel()
            local task = mare.task.spawn(function()
              mare.exec.run({tool='probe'}, function()
                local co = coroutine.create(function()
                  coroutine.wrap(function()
                    local guard <close> = setmetatable({}, {__close=function()
                      closed = mare.exec.current() ~= nil
                      confined = not pcall(mare.fs.write, DENIED_PATH, 'unsafe')
                    end})
                    ready_tx:send(true)
                    mare.task.sleep(10000)
                  end)()
                end)
                assert(coroutine.resume(co))
              end)
            end)
            assert(mare.task.timeout(2000, function() ready_rx:recv() end))
            task:abort(); pcall(task.wait, task)
            return manual, closed, confined
        "#).await;
        assert!(manual && closed && confined);
        assert!(!env.cwd.join("nested-close-escape").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn failed_tool_and_wrapped_coroutines_close_scopes_under_their_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "DENIED_PATH", env.cwd.join("error-close-escape").to_string_lossy().to_string());
        let (direct, nested, closer, manual, dead, errors): (bool, bool, bool, bool, bool, bool) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current('read-only')
            local direct, nested, closer, manual, confined = false, false, false, false, true
            local thread
            local function scope(mark, fail)
              return setmetatable({}, {__close=function()
                mark()
                confined = confined and mare.exec.current() ~= nil and not pcall(mare.fs.write, DENIED_PATH, 'unsafe')
                if fail then error('closer-error') end
              end})
            end
            local ok1, err1 = pcall(mare.exec.run, {tool='probe'}, function()
              thread = coroutine.running()
              local guard <close> = scope(function() direct = true end)
              mare.task.sleep(1)
              error('direct-error')
            end)
            local dead = coroutine.status(thread) == 'dead'
            local direct_closer, failed_thread = false, nil
            local ok4, err4 = pcall(mare.exec.run, {tool='probe'}, function()
              failed_thread = coroutine.running()
              local guard <close> = scope(function() direct_closer = true end, true)
              error('direct-before-close-error')
            end)
            dead = dead and coroutine.status(failed_thread) == 'dead'
            local ok2, err2 = pcall(mare.exec.run, {tool='probe'}, function()
              coroutine.wrap(function()
                local guard <close> = scope(function() nested = true end)
                mare.task.sleep(1)
                error('nested-error')
              end)()
            end)
            local ok3, err3 = pcall(mare.exec.run, {tool='probe'}, function()
              coroutine.wrap(function()
                local guard <close> = scope(function() closer = true end, true)
                error('before-close-error')
              end)()
            end)
            mare.exec.run({tool='probe'}, function()
              local co = coroutine.create(function()
                local guard <close> = scope(function() manual = true end)
                error('manual-error')
              end)
              local ok = coroutine.resume(co)
              assert(not ok and not manual, 'resume must leave error scope for explicit close')
              coroutine.close(co)
            end)
            return direct and direct_closer and confined, nested and confined, closer and confined, manual, dead,
              not ok4 and tostring(err4):find('direct%-before%-close%-error') ~= nil and
              not ok1 and tostring(err1):find('direct%-error') ~= nil and
              not ok2 and tostring(err2):find('nested%-error') ~= nil and
              not ok3 and tostring(err3):find('closer%-error') ~= nil
        "#,
        )
        .await;
        assert_eq!((direct, nested, closer, manual, dead, errors), (true, true, true, true, true, true));
        assert!(!env.cwd.join("error-close-escape").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_wrapped_coroutine_closes_nested_timeout_and_releases_channel() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "DENIED_PATH", env.cwd.join("wrap-timeout-escape").to_string_lossy().to_string());
        let (closed, confined, unlocked): (bool, bool, bool) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current('read-only')
            local closed, confined = false, false
            local ready_tx, ready_rx = mare.task.channel()
            local lock_tx, lock_rx = mare.task.channel()
            local task = mare.task.spawn(function()
              mare.exec.run({tool='probe'}, function()
                coroutine.wrap(function()
                  mare.task.timeout(10000, function()
                    local guard <close> = setmetatable({}, {__close=function()
                      closed = mare.exec.current() ~= nil
                      confined = not pcall(mare.fs.write, DENIED_PATH, 'unsafe')
                    end})
                    ready_tx:send(true)
                    lock_rx:recv()
                  end)
                end)()
              end)
            end)
            assert(mare.task.timeout(2000, function() ready_rx:recv() end))
            -- Yield once so lock_rx:recv() inside the nested timeout enters its mutex wait.
            mare.task.sleep(10)
            task:abort(); pcall(task.wait, task)
            lock_tx:send('after-cancel')
            local ok, next_msg = mare.task.timeout(500, function() return lock_rx:recv() end)
            return closed, confined, ok and next_msg == 'after-cancel'
        "#,
        )
        .await;
        assert_eq!((closed, confined, unlocked), (true, true, true));
        assert!(!env.cwd.join("wrap-timeout-escape").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn committed_session_append_publishes_when_rpc_caller_ctx_disposes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        h.prompt("seed", json!({})).await.unwrap();

        let entered = std::rc::Rc::new(tokio::sync::Notify::new());
        let release = std::rc::Rc::new(tokio::sync::Notify::new());
        let entered_fn = entered.clone();
        let release_fn = release.clone();
        h.lua()
            .globals()
            .set(
                "_TEST_APPEND_BARRIER",
                h.lua()
                    .create_async_function(move |_, ()| {
                        let entered = entered_fn.clone();
                        let release = release_fn.clone();
                        async move {
                            entered.notify_waiters();
                            release.notified().await;
                            Ok(())
                        }
                    })
                    .unwrap(),
            )
            .unwrap();
        exec(
            &h,
            r#"
            local orig_pcall = _G.pcall
            _G.pcall = function(fn, ...)
                local arg1, arg2, arg3 = ...
                local is_target = false
                if type(arg1) == 'function' and type(arg2) == 'userdata' and type(arg3) == 'table' then
                    for _, e in ipairs(arg3) do
                        if type(e) == 'table' and e.kind == 'disconnect-after-commit' then
                            is_target = true
                            break
                        end
                    end
                end
                if is_target then
                    _G.pcall = orig_pcall
                    local res = table.pack(orig_pcall(fn, ...))
                    _TEST_APPEND_BARRIER()
                    return table.unpack(res, 1, res.n)
                end
                return orig_pcall(fn, ...)
            end
            "#,
        )
        .await;

        let caller = h.kernel.open_owner("rpc-disconnect-test", "user").unwrap();
        let kernel = h.kernel.clone();
        let call_task = tokio::task::spawn_local(async move {
            kernel
                .call_service_export(
                    caller,
                    None,
                    "session",
                    "add_record",
                    json!({ "kind": "disconnect-after-commit", "data": { "v": 1 }, "persist": true }),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        });

        // Wait until `store.lua` has marked committed and written the JSONL entry, then dispose caller ctx
        // (matching `rpc/serve.rs` on client disconnect).
        entered.notified().await;
        let _ = h.kernel.dispose_ctx_defer_callbacks(caller).await;
        release.notify_waiters();

        let call_res = call_task.await.unwrap();
        assert!(call_res.is_ok(), "committed session.add_record must complete after caller disconnect: {call_res:?}");

        // Both the on-disk JSONL file and in-memory `session.list_records` must contain the record.
        let (mem_count, disk_count): (i64, i64) = eval(
            &h,
            r#"
            local mem = #S:list_records('disconnect-after-commit').data
            local path = S:info().path
            local disk = 0
            for _, e in ipairs(mare.store.read(path)) do
                if e.type == 'custom' and e.kind == 'disconnect-after-commit' then disk = disk + 1 end
            end
            return mem, disk
            "#,
        )
        .await;
        assert_eq!((mem_count, disk_count), (1, 1), "in-memory list_records and JSONL must stay in sync");
        h.shutdown().await;
    })
    .await
}

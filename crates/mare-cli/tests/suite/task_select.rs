//! Public task.select structured cancellation and resource ownership.
use crate::common::*;
use std::net::TcpListener;

#[tokio::test]
async fn winner_waits_for_losers_to_close_and_preserves_results() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (index, value, closed): (i64, String, bool) = eval(&h, r#"
            CLOSED = false
            local index, value = mare.task.select(
                function() local scope <close> = setmetatable({}, {__close=function() CLOSED=true end}); mare.task.sleep(1000) end,
                function() mare.task.sleep(30); return 'winner' end)
            return index, value, CLOSED
        "#).await;
        assert_eq!((index, value.as_str(), closed), (2, "winner", true));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn outer_timeout_aborts_all_select_branches() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (finished, closed, late): (bool, bool, bool) = eval(&h, r#"
            CLOSED, LATE = false, false
            local finished = mare.task.timeout(40, function()
                mare.task.select(
                    function() local s <close> = setmetatable({}, {__close=function() CLOSED=true end}); mare.task.sleep(150); LATE=true end,
                    function() mare.task.sleep(1000) end)
            end)
            mare.task.sleep(250)
            return finished, CLOSED, LATE
        "#).await;
        assert!(!finished && closed && !late);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn winner_error_survives_loser_cleanup() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (ok, err, closed): (bool, String, bool) = eval(&h, r#"
            CLOSED = false
            local ok, err = pcall(mare.task.select,
                function() local s <close> = setmetatable({}, {__close=function() CLOSED=true end}); mare.task.sleep(1000) end,
                function() mare.task.sleep(30); error('winner failed') end)
            return ok, tostring(err), CLOSED
        "#).await;
        assert!(!ok && err.contains("winner failed") && closed, "{err}, closed={closed}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn loser_cleanup_keeps_the_inherited_tool_context() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (index, before_context, closed_context): (i64, String, String) = eval(&h, r#"
            CLOSED_CONTEXT = 'not closed'
            local index = mare.exec.run({tool='select-probe'}, function()
                return mare.task.select(
                    function() BEFORE_CONTEXT = (mare.exec.current() or {}).tool or 'none'; local s <close> = setmetatable({}, {__close=function() CLOSED_CONTEXT = (mare.exec.current() or {}).tool or 'none' end}); mare.task.sleep(1000) end,
                    function() mare.task.sleep(30); return 'winner' end)
            end)
            return index, BEFORE_CONTEXT, CLOSED_CONTEXT
        "#).await;
        assert_eq!(index, 2);
        assert_eq!(before_context, "select-probe", "the running branch inherits the tool context");
        assert_eq!(closed_context, "select-probe", "the cancelled branch retains its context while closing");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn losing_oauth_listener_releases_its_port_before_returning() {
    local(async {
        let env = TestEnv::new().await;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let h = env.host().no_session().build().await;
        global(&h, "PORT", port);
        let index: i64 = eval(&h, r#"
            local tx, rx = mare.task.channel()
            return mare.task.select(
                function() return mare.http.listen_once({port=PORT, path='/auth/callback', timeout_ms=5000, on_bound=function(p) tx:send(p) end}) end,
                function() return rx:recv() end)
        "#).await;
        assert_eq!(index, 2);
        let rebound = TcpListener::bind(("127.0.0.1", port)).expect("OAuth listener still holds the port");
        drop(rebound);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn separately_spawned_side_effect_can_outlive_the_losing_branch() {
    local(async {
        let env = TestEnv::new().await;
        let effect = env.cwd.join("detached-effect.txt");
        let h = env.host().no_session().build().await;
        global(&h, "EFFECT", effect.to_string_lossy().to_string());
        let index: i64 = eval(
            &h,
            r#"
            local index = mare.task.select(
                function()
                    mare.task.spawn(function() mare.task.sleep(120); mare.fs.write(EFFECT, 'done') end)
                    mare.task.sleep(1000)
                end,
                function() mare.task.sleep(30); return true end)
            mare.task.sleep(180)
            return index
        "#,
        )
        .await;
        assert_eq!(index, 2);
        assert_eq!(std::fs::read_to_string(effect).unwrap(), "done");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_error_in_a_cancelled_closer_does_not_replace_the_winner() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (index, result, attempted): (i64, String, bool) = eval(
            &h,
            r#"
            ATTEMPTED = false
            local index, result = mare.task.select(
                function()
                    local s <close> = setmetatable({}, {__close=function() ATTEMPTED=true; error('close failed') end})
                    mare.task.sleep(1000)
                end,
                function() mare.task.sleep(30); return 'winner' end)
            return index, result, ATTEMPTED
        "#,
        )
        .await;
        assert_eq!((index, result.as_str(), attempted), (2, "winner", true));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn synchronous_closing_work_delays_select_return() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let elapsed: f64 = eval(
            &h,
            r#"
            local start = mare.task.clock()
            mare.task.select(
                function()
                    local scope <close> = setmetatable({}, {__close=function()
                        local until_time = mare.task.clock() + 120
                        while mare.task.clock() < until_time do end
                    end})
                    mare.task.sleep(1000)
                end,
                function() mare.task.sleep(30); return true end)
            return mare.task.clock() - start
        "#,
        )
        .await;
        assert!(elapsed >= 115.0, "synchronous cleanup must finish before return: {elapsed} ms");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nested_timeout_in_tool_context_cancels_every_select_branch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (finished, closed, late): (bool, bool, bool) = eval(&h, r#"
            CLOSED, LATE = false, false
            local finished = mare.exec.run({tool='outer'}, function()
                return mare.task.timeout(40, function()
                    return mare.task.select(
                        function() local s <close> = setmetatable({}, {__close=function() CLOSED=true end}); mare.task.sleep(150); LATE=true end,
                        function() mare.task.sleep(1000) end)
                end)
            end)
            mare.task.sleep(250)
            return finished, CLOSED, LATE
        "#).await;
        assert!(!finished && closed && !late, "finished={finished}, closed={closed}, late={late}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn tool_context_timeout_closes_callback_before_returning() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (finished, closed_context): (bool, String) = eval(&h, r#"
            CLOSED_CONTEXT = 'not closed'
            return mare.exec.run({tool='timeout-probe'}, function()
                local finished = mare.task.timeout(30, function()
                    local s <close> = setmetatable({}, {__close=function() CLOSED_CONTEXT=(mare.exec.current() or {}).tool or 'none' end})
                    mare.task.sleep(1000)
                end)
                return finished, CLOSED_CONTEXT
            end)
        "#).await;
        assert!(!finished);
        assert_eq!(closed_context, "timeout-probe");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn parallel_tool_contexts_do_not_clear_each_others_tags() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (a, b, outside): (String, String, bool) = eval(
            &h,
            r#"
            local a = mare.task.spawn(function()
                return mare.exec.run({tool='A'}, function()
                    return mare.task.timeout(30, function() mare.task.sleep(1000) end)
                end)
            end)
            local b = mare.task.spawn(function()
                return mare.exec.run({tool='B'}, function()
                    local finished, observed = mare.task.timeout(300, function()
                        mare.task.sleep(80)
                        return (mare.exec.current() or {}).tool or 'none'
                    end)
                    assert(finished)
                    return observed
                end)
            end)
            local av = a:wait()
            local bv = b:wait()
            return tostring(av), bv, mare.exec.current() == nil
        "#,
        )
        .await;
        assert_eq!(a, "false");
        assert_eq!(b, "B");
        assert!(outside);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn loser_closer_cannot_escape_its_tool_sandbox() {
    use serde_json::json;
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let out = dunce::canonicalize(outside.path()).unwrap();
        let h = env.host().no_session().faux(json!([
            {"tool_calls":[{"name":"select_sandbox_probe","arguments":{}}]}, {"text":"done"}
        ])).build().await;
        expose(&h);
        global(&h, "OUT", out.to_string_lossy().to_string());
        exec(&h, r#"
            mare.proc.sandbox.set_current({ level = 'workspace-write', exclude_tmpdir_env_var = true, exclude_slash_tmp = true })
            T:register('select_sandbox_probe', {
                description='probe', parameters={type='object',properties={}}, side_effect='read',
                execute=function()
                    local index = mare.task.select(
                        function()
                            local s <close> = setmetatable({}, {__close=function()
                                CLOSE_TOOL = (mare.exec.current() or {}).tool or 'none'
                                pcall(function() local f = io.open(OUT .. '/escape', 'w'); if f then f:write('bad'); f:close() end end)
                            end})
                            mare.task.sleep(1000)
                        end,
                        function() mare.task.sleep(30); return true end)
                    return tostring(index)
                end,
            })
        "#).await;
        h.prompt("go", json!({})).await.unwrap();
        let tool: String = eval(&h, "return CLOSE_TOOL or 'missing'").await;
        assert_eq!(tool, "select_sandbox_probe");
        assert!(!out.join("escape").exists(), "cancelled tool closer wrote outside sandbox");
        exec(&h, "mare.proc.sandbox.set_current(nil)").await;
        h.shutdown().await;
    }).await
}

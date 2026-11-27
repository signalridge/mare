use mlua::Lua;
async fn run(source: &str) -> mlua::Result<()> {
    let lua = Lua::new();
    let mare = lua.create_table()?;
    super::local::install(&lua, &mare)?;
    crate::task::install(&lua, &mare)?;
    lua.globals().set("mare", mare)?;
    tokio::task::LocalSet::new().run_until(lua.load(source).exec_async()).await
}
#[tokio::test]
async fn strict_bridge_and_capability_boundary() {
    run(r#"
      local vm = mare.sandbox.new { expose = { echo = function(v) return v end } }
      local r, e = vm:run([[assert(not io and not os and not require and not load and not debug and not mare)
        local v = echo(json.decode('{"a":null,"b":[],"c":9007199254740993}'))
        assert(v.a == json.null and #v.b == 0 and v.c == 9007199254740993)
        assert(json.encode(v.b) == '[]')
        assert(not pcall(echo, {f=function() end}))
        local t = {}; t.self=t; assert(not pcall(echo,t))
        return v]])
      assert(r, e); assert(r.result.c == 9007199254740993)
      assert(not vm:run('return 1'))
    "#)
    .await
    .unwrap();
}
#[tokio::test]
async fn limits_cannot_be_swallowed() {
    tokio::time::timeout(std::time::Duration::from_secs(3), run(r#"
      local r,e = mare.sandbox.new { max_instructions=10000 }:run('while true do pcall(function() while true do end end) end')
      assert(not r and e:find('instruction limit'),e)
      r,e = mare.sandbox.new {max_bytes=128}:run([[while true do pcall(print,string.rep('x',100)) end]])
      assert(not r and e:find('byte limit'),e)
      local c=mare.task.token(); c:cancel()
      r,e=mare.sandbox.new {cancel=c}:run('return 1'); assert(not r and e:find('cancelled'),e)
    "#)).await.unwrap().unwrap();
}
#[tokio::test]
async fn invalid_limits_memory_and_reserved_names() {
    run(r#"
      assert(not pcall(mare.sandbox.new,{timeout_ms=0}))
      assert(not pcall(mare.sandbox.new,{timeout_ms=1.5}))
      assert(not pcall(mare.sandbox.new,{expose={print=function() end}}))
      local r,e=mare.sandbox.new {memory_bytes=200000}:run([[return string.rep('x',1000000)]])
      assert(not r and e:find('memory'),e)
    "#)
    .await
    .unwrap();
}

#[tokio::test]
async fn timer_scopes_and_explicit_no_deadline() {
    run(r#"
      local vm = mare.sandbox.new {
        timeout_ms = false,
        expose = {
          yield_host = function()
            mare.task.yield()
            return true
          end,
        },
      }
      local r, e = vm:run([[
        local count = 0
        setTimeout(function() count = count + 1 end, 0)
        local id = setTimeout(function() count = 99 end, 0)
        clearTimeout(id)
        while count == 0 do
          yield_host()
        end
        return count
      ]])
      assert(r and r.result == 1, e)
      local r2, e2 = mare.sandbox.new{}:run('return nil')
      assert(r2, e2)
    "#)
    .await
    .unwrap();
}

#[tokio::test]
async fn output_budget_and_bridge_json_size_enforce_exact_limits() {
    run(r#"
      local r, e = mare.sandbox.new { max_output_bytes = 12 }:run([[
        print("12345")
        print("67890")
      ]])
      assert(r and r.output == "12345\n67890\n", tostring(e))

      local r_over, e_over = mare.sandbox.new { max_output_bytes = 12 }:run([[
        print("12345")
        print("678901")
      ]])
      assert(not r_over and e_over:find("byte limit"), tostring(e_over))

      local vm = mare.sandbox.new {
        max_message_bytes = 16,
        expose = { echo = function(v) return v end },
      }
      local ok_msg, err_msg = vm:run([[
        assert(echo("ok") == "ok")
        return true
      ]])
      assert(ok_msg and ok_msg.result == true, tostring(err_msg))

      local vm_over = mare.sandbox.new {
        max_message_bytes = 16,
        expose = { echo = function(v) return v end },
      }
      local ok_over, err_over = vm_over:run([[
        local _ = pcall(echo, "0123456789abcdef")
        return true
      ]])
      assert(not ok_over and tostring(err_over):find("byte limit", 1, true), tostring(err_over))
    "#)
    .await
    .unwrap();
}

#[tokio::test]
async fn instruction_hook_does_not_lose_async_callback_wakeup() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        super::local::install(&lua, &mare).unwrap();
        crate::task::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        // Expose an async callback whose future parks on a oneshot woken by a spawned
        // local task on the next turn. Sweeping instruction padding guarantees the
        // 1000-instruction hook fires inside mlua's `=__mlua_async_poll` wrapper between
        // `poll()` returning `Poll::Pending` and `yield(PENDING)`.
        let async_ping = lua
            .create_async_function(|_, x: i64| async move {
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::task::spawn_local(async move {
                    let _ = tx.send(x + 1);
                });
                rx.await.map_err(mlua::Error::external)
            })
            .unwrap();
        lua.globals().set("async_ping", async_ping).unwrap();

        tokio::task::LocalSet::new()
            .run_until(
                lua.load(
                    r#"
                    for pad = 0, 120, 5 do
                        local vm = mare.sandbox.new {
                            timeout_ms = 500,
                            expose = { ping = async_ping },
                        }
                        local code = ("local z=0; for k=1,%d do z=z+k end; local s=0; for i=1,60 do s=s+ping(i) end; return s"):format(pad)
                        local r, e = vm:run(code)
                        assert(r and r.result == 1890, ("pad=%d failed: %s"):format(pad, tostring(e)))
                    end
                    -- Terminal instruction limit still aborts tight async-callback loops even under pcall.
                    local lim_vm = mare.sandbox.new {
                        timeout_ms = 2000,
                        max_instructions = 5000,
                        expose = { ping = async_ping },
                    }
                    local lr, le = lim_vm:run([[
                        local i = 0
                        while true do
                            i = i + 1
                            pcall(ping, i)
                        end
                    ]])
                    assert(not lr and tostring(le):find("instruction limit", 1, true), tostring(le))
                    "#,
                )
                .exec_async(),
            )
            .await
            .unwrap();
    })
    .await
    .expect("sandbox must not deadlock on lost async wakeup");
}

//! `ctx:spawn` / `ctx:after` / `ctx:every` on the cordis tree (host plugin `task`, task.lua): a task
//! acts for its plugin's owner, its effect goes when the task ends however it ends, a normal end
//! never aborts the task, and `spawn`'s values come back whole (before the native implementation).

use super::write_plugin;
use crate::common::*;
use std::sync::Arc;

async fn host(env: &TestEnv) -> Arc<mare_host::Host> {
    env.host().user_plugins().no_session().build_exposed().await
}

/// A native plugin that creates its mare.Service only after an await, inside `ctx:spawn`: the
/// service is attributed to the plugin (its owner), and goes with it.
const LATE_SERVICE: &str = r#"return { name = "latesvc", inject = { "task" }, provide = { "latesvc" },
  contracts = { latesvc = { methods = {
    who = { doc = "who", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } },
  } } },
  apply = function(ctx)
    mare._task_prims_hidden = ctx.task.prims == nil
    local Svc = require("cordis").class("Svc", mare.Service)
    function Svc:constructor(c) Svc.super.constructor(self, c, "latesvc") end
    function Svc:who() return "late" end
    ctx:spawn(function()
      mare.task.sleep(1)
      local ok, err = pcall(Svc.new, ctx)
      mare._late = ok and "made" or tostring(err)
    end)
  end }"#;

#[tokio::test]
async fn a_service_created_after_an_await_in_spawn_belongs_to_its_plugin() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "latesvc", LATE_SERVICE);
        let h = host(&env).await;
        let prims_hidden: bool = eval(&h, "return mare._task_prims_hidden").await;
        assert!(prims_hidden, "the injected task service exposed host primitives");
        for _ in 0..200 {
            if eval::<Option<String>>(&h, "return mare._late").await.is_some() {
                break;
            }
            // sleep-ok: poll interval for the late service (bounded, 200 tries)
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let made: Option<String> = eval(&h, "return mare._late").await;
        assert_eq!(made.as_deref(), Some("made"), "creating the service in the task failed");
        let who: String = eval(&h, r#"return C.cordis:get("latesvc"):who()"#).await;
        assert_eq!(who, "late");
        std::fs::remove_file(env.home.path().join("config/plugins/latesvc.lua")).unwrap();
        h.reload_all().await.unwrap();
        let gone: bool = eval(&h, r#"return not C.cordis:get("kernel"):provides("latesvc")"#).await;
        h.shutdown().await;
        assert!(gone, "the service outlived its plugin: it was not attributed to it");
    })
    .await
}

/// Tasks that return, raise or are aborted all leave no effect behind; a task that returns is not
/// aborted by its own cleanup, and its values (nil in the middle included) come back whole.
const TASKS: &str = r#"return { name = "tasks", inject = { "task" }, apply = function(ctx)
  mare._ctx = ctx
  mare._effects = function() return #ctx.fiber._disposables:values() end
  mare._stuck = ctx:spawn(function() mare.task.sleep(60000) end)
end }"#;

/// spawn's values, then the effect count after a return, a raise and an abort, and the base.
type Outcome = (i64, Option<i64>, Option<i64>, Option<i64>, i64, i64, i64, i64);

#[tokio::test]
async fn a_task_ends_cleanly_however_it_ends() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "tasks", TASKS);
        let h = host(&env).await;
        // a raising task starts after build(), whose startup must report no plugin problem
        let (n, a, b, c, returned, raised, aborted, base): Outcome = eval(
            &h,
            r#"local base = mare._effects()
                   local t = mare._ctx:spawn(function() mare.task.sleep(1) return 1, nil, 3 end)
                   local r = table.pack(t:wait())
                   mare.task.sleep(5)
                   local returned = mare._effects()
                   local bad = mare._ctx:spawn(function() mare.task.sleep(1) error("boom") end)
                   pcall(bad.wait, bad)
                   mare.task.sleep(5)
                   local raised = mare._effects()
                   mare._stuck:abort()
                   mare.task.sleep(20)
                   return r.n, r[1], r[2], r[3], returned, raised, mare._effects(), base"#,
        )
        .await;
        h.shutdown().await;
        assert_eq!((n, a, b, c), (3, Some(1), None, Some(3)), "spawn's values, nil in the middle included");
        assert_eq!(returned, base, "a task that returned left its effect");
        assert_eq!(raised, base, "a task that raised left its effect");
        assert_eq!(aborted, base - 1, "an aborted task left its effect");
    })
    .await
}

/// A task aborted before it ever ran leaves no effect either, and a second waiter on a task gets
/// its values too (audit quick review).
#[tokio::test]
async fn a_task_aborted_before_it_ran_leaves_no_effect() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "tasks", TASKS);
        let h = host(&env).await;
        let (base, after, first, second): (i64, i64, Option<i64>, Option<i64>) = eval(
            &h,
            r#"local base = mare._effects()
               local t = mare._ctx:spawn(function() return 1 end)
               t:abort()
               mare.task.sleep(20)
               local after = mare._effects()
               local v = mare._ctx:spawn(function() mare.task.sleep(5) return 7 end)
               -- the task's own watcher waits first; the waiters after it get the values too
               mare.task.sleep(0)
               local other = mare.task.spawn(function() return (v:wait()) end)
               local first = v:wait()
               return base, after, first, (other:wait())"#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(after, base, "a task aborted before it ran left its effect");
        assert_eq!((first, second), (Some(7), Some(7)), "every waiter gets the task's values");
    })
    .await
}

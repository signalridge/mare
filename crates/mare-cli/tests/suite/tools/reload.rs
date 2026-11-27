//! A plugin's tools follow its fiber across a reload: the old definition is withdrawn with the
//! old fiber, the new one is what runs. The plugin repository's e2e-terminal probe must not
//! depend on the plugin it reloads.

use crate::common::*;

fn write_plugin(env: &TestEnv, name: &str, src: &str) {
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.lua")), src).unwrap();
}

const OWNER: &str = r#"return { name = "reg-tool", inject = { "tools" }, apply = function(ctx)
  mare._applies = (mare._applies or 0) + 1
  local gen = mare._applies
  ctx.tools:register("reg_probe", { description = "probe", side_effect = "read",
    parameters = { type = "object", properties = {} },
    execute = function() return { content = "gen" .. gen } end })
end }"#;

const DRIVER: &str = r#"return { name = "driver", inject = { "tools", "kernel" }, apply = function(ctx)
  mare._reload_from_plugin = function() ctx.kernel:reload("reg-tool") end
end }"#;

#[tokio::test]
async fn a_reloaded_plugins_tool_runs_its_new_definition_and_the_old_one_is_gone() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "reg-tool", OWNER);
        write_plugin(&env, "driver", DRIVER);
        let b = env.host().user_plugins().no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        expose(&h);
        let call = |id: &'static str| {
            format!(r#"local r = T:execute({{ id = "{id}", name = "reg_probe", arguments = {{}} }}); return tostring(r.content)"#)
        };
        let count = r#"local n = 0; for _, d in ipairs(T:list()) do if d.name == "reg_probe" then n = n + 1 end end; return n"#;
        assert_eq!(eval::<String>(&h, &call("a")).await, "gen1");
        // reloaded from the host
        h.kernel.reload(Some("reg-tool")).await.unwrap();
        assert_eq!(eval::<String>(&h, &call("b")).await, "gen2", "the new definition runs");
        assert_eq!(eval::<i64>(&h, count).await, 1, "the old definition was withdrawn with its fiber");
        // reloaded by another plugin
        exec(&h, "mare._reload_from_plugin()").await;
        assert_eq!(eval::<String>(&h, &call("c")).await, "gen3");
        assert_eq!(eval::<i64>(&h, count).await, 1);
        h.shutdown().await;
    })
    .await
}

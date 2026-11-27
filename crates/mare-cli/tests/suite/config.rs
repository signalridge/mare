//! configuration: layer precedence and sources, lazy.nvim opts merge, did-you-mean
//! warnings, disabling / replacing built-ins (name slots), `/reload config` and init.lua side effects.

use std::sync::Arc;

use crate::common::*;
use mare_host::Host;
use serde_json::json;

mod callbacks;
mod cfg2;
mod concurrent;
mod layers;
mod listeners;
mod modules;
mod reload;
mod schema;
mod surface;
mod txn;

const PROBE: &str = r#"return { name = "NAME",
  config_schema = {
    a = { type = "object", desc = "a group", fields = {
      x = { type = "number", default = 0, desc = "x" }, y = { type = "number", default = 0, desc = "y" } } },
    list = { type = "list", default = {}, desc = "a list" },
    n = { type = "number", default = 0, desc = "a number" },
    cb = { type = "function", scope = "lua", desc = "a callback" },
  },
  apply = function(ctx, cfg)
    mare._applies = mare._applies or {}
    mare._applies.NAME = (mare._applies.NAME or 0) + 1
    mare._cfg_NAME = cfg
  end }"#;

const USER_INIT: &str = r#"
mare.setup { model = "faux/echo", icons = "always", slow_callback_table_rows = 5, modle = "typo" }
mare.o.editor = 42 -- wrong type: warns, still set
mare.plugin("probe", { a = { x = 1, y = 2 }, list = { 1, 2, 3 } })
mare.plugin("probe2", { n = 1, cb = function() return 42 end })
mare.plugin("usage", { enabled = false })
mare.plugin("file-ref", { enabled = false })
mare.keymap.set("editor", "<C-y>", "transcript")
mare.prompt.remove("probe-sec")
"#;

/// Ends the park of an `init.lua` / plugin / hook that waits in Lua for `mare._reload_release`
/// (`for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end`: the 60 s
/// cap only catches a test that never releases it).
async fn release_reload(h: &Host) {
    exec(h, "mare._reload_release = true").await
}

/// Wait until Lua has set `mare.<flag>` to true.
async fn wait_flag(h: &Host, flag: &str) {
    let code = format!("return mare.{flag} == true");
    wait_until(WAIT_SECS, flag, || {
        let code = code.clone();
        async move { eval::<bool>(h, &code).await }
    })
    .await;
}

/// state.json < user init.lua < project .mare/init.lua < env MARE_SLOW_CALLBACK_TABLE_ROWS < CLI (`editor`)
async fn setup(env: &mut TestEnv) -> Arc<Host> {
    env.set_var("MARE_SLOW_CALLBACK_TABLE_ROWS", "9");
    env.set_var("MARE_EDITOR", "nano");
    let cfg = env.config_dir();
    for name in ["probe", "probe2"] {
        std::fs::create_dir_all(cfg.join("plugins").join(name)).unwrap();
        std::fs::write(cfg.join("plugins").join(name).join("init.lua"), PROBE.replace("NAME", name)).unwrap();
    }
    // replaces the builtin `notify` (`override = true`: the only way to replace a definition)
    std::fs::create_dir_all(cfg.join("plugins/notify")).unwrap();
    std::fs::write(
        cfg.join("plugins/notify/init.lua"),
        r#"return { name = "notify", override = true, apply = function() mare._my_notify = true end }"#,
    )
    .unwrap();
    std::fs::write(cfg.join("init.lua"), USER_INIT).unwrap();
    std::fs::create_dir_all(env.home.path().join("state")).unwrap();
    std::fs::write(env.home.path().join("state/state.json"), r#"{"version":1,"model":"faux/other","theme_name":"x"}"#)
        .unwrap();
    std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
    std::fs::write(
        env.cwd.join(".mare/init.lua"),
        r#"mare.setup { icons = "never", slow_callback_table_rows = 7 }
           mare.plugin("probe", { a = { y = 3 }, list = { 9 } })"#,
    )
    .unwrap();
    env.host().user_config(true).config(json!({ "editor": "vim" })).no_session().build().await
}

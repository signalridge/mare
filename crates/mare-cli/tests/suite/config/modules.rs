//! Config modules: layer scoping, watching, cycles and the staged config.

use super::*;

#[tokio::test]
async fn project_optional_mare_paths_ignore_a_regular_file() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".mcp.json"), "{}").unwrap();
        std::fs::write(env.cwd.join(".mare"), "protected file").unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        let (project, mcp): (Option<String>, Option<String>) = eval(
            &h,
            r#"
            local lib = require('mare_lib')
            return lib.project_file('.mare/mcp.json'), lib.project_file('.mcp.json')
        "#,
        )
        .await;
        assert!(project.is_none(), "a non-directory .mare cannot contain optional project files");
        assert!(mcp.is_some(), "other project files remain discoverable");
        assert_eq!(std::fs::read_to_string(env.cwd.join(".mare")).unwrap(), "protected file");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_modules_are_layer_scoped_reloaded_and_consistent_in_short_vm() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir();
        let project = env.cwd.join(".mare");
        std::fs::create_dir_all(user.join("lua/nested")).unwrap();
        std::fs::create_dir_all(project.join("lua")).unwrap();
        std::fs::write(user.join("lua/settings.lua"), "return { value = 'user' }").unwrap();
        std::fs::write(user.join("lua/nested/init.lua"), "return { value = 'nested' }").unwrap();
        std::fs::write(project.join("lua/settings.lua"), "return { value = 'project' }").unwrap();
        std::fs::write(user.join("init.lua"), r#"local a = require('settings'); assert(a == require('settings')); mare.setup { notify = a.value, editor = require('nested').value }"#).unwrap();
        std::fs::write(project.join("init.lua"), r#"mare.setup { theme_name = require('settings').value, editor = require('nested').value }"#).unwrap();
        let untrusted = env.host().user_config(false).no_session().build().await;
        assert_eq!(untrusted.kernel.config()["notify"], "user");
        assert!(untrusted.kernel.config()["theme_name"].is_null());
        untrusted.shutdown().await;
        let h = env.host().user_config(true).no_session().build().await;
        assert_eq!(h.kernel.config()["notify"], "user");
        assert_eq!(h.kernel.config()["theme_name"], "project");
        assert_eq!(h.kernel.config()["editor"], "nested");
        let short = mare_plugin::config_load::load(&env.cwd, true).await.unwrap();
        assert_eq!(short.value["notify"], "user");
        assert_eq!(short.value["theme_name"], "project");
        let short_untrusted = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert_eq!(short_untrusted.value["notify"], "user");
        assert!(short_untrusted.value["theme_name"].is_null());
        exec(&h, "package.loaded.settings = { value = 'plugin-owned' }").await;
        std::fs::write(user.join("lua/settings.lua"), "return { value = 'changed' }").unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(h.kernel.config()["notify"], "changed");
        assert_eq!(eval::<String>(&h, "return package.loaded.settings.value").await, "plugin-owned");
        std::fs::write(user.join("lua/settings.lua"), "return { broken = }").unwrap();
        let error = h.reload_config().await.unwrap_err().to_string();
        assert!(error.contains("settings.lua"), "{error}");
        assert_eq!(h.kernel.config()["notify"], "changed");
        h.shutdown().await;
    }).await
}

/// Change notices watch only what the config used: init.lua and the `lua/` modules the last
/// evaluation required, never an unrelated file under `<config>/lua`; `/reload config` updates
/// the set.
#[tokio::test]
async fn config_watch_covers_required_modules_only() {
    local(async {
        let env = TestEnv::new().await;
        let user = env.config_dir();
        std::fs::create_dir_all(user.join("lua")).unwrap();
        std::fs::write(user.join("lua/settings.lua"), "return { value = 'user' }").unwrap();
        std::fs::write(user.join("lua/scratch.lua"), "return {}").unwrap();
        std::fs::write(user.join("init.lua"), "mare.setup { notify = require('settings').value }").unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        let watched = || async {
            eval::<String>(&h, "return table.concat(mare.plugins.watched(), '\\n')").await.replace('\\', "/")
        };
        let w = watched().await;
        assert!(w.contains("init.lua") && w.contains("lua/settings.lua"), "{w}");
        assert!(!w.contains("scratch.lua"), "unused module watched: {w}");
        std::fs::write(user.join("init.lua"), "mare.setup { notify = require('scratch').value or 'x' }").unwrap();
        h.reload_config().await.unwrap();
        let w = watched().await;
        assert!(w.contains("lua/scratch.lua") && !w.contains("settings.lua"), "{w}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_module_cycle_fails_with_a_useful_error_and_preload_fallback_works() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.config_dir();
        std::fs::create_dir_all(root.join("lua")).unwrap();
        std::fs::write(root.join("init.lua"), "require('one')").unwrap();
        std::fs::write(root.join("lua/one.lua"), "return require('two')").unwrap();
        std::fs::write(root.join("lua/two.lua"), "return require('one')").unwrap();
        let err = mare_plugin::config_load::load(&env.cwd, false).await.unwrap_err().to_string();
        assert!(err.contains("config module import cycle: one"), "{err}");
        std::fs::write(root.join("init.lua"), "package.preload.local_test = function() return {value='fallback'} end; mare.setup {editor = require('local_test').value}").unwrap();
        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert_eq!(cfg.value["editor"], "fallback");
    }).await
}

#[tokio::test]
async fn config_modules_and_plugin_config_api_see_the_staged_config() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let cfg = env.config_dir();
        std::fs::create_dir_all(cfg.join("lua")).unwrap();
        std::fs::write(cfg.join("lua/checkview.lua"), "assert(mare.config.logo == 'compact'); return true").unwrap();
        std::fs::write(cfg.join("plugins/probe2/init.lua"), r#"return {name='probe2', config_api={check_view=function(C) assert(mare.config == C and C.logo == 'compact') end}, apply=function() end}"#).unwrap();
        std::fs::write(cfg.join("init.lua"), format!("{USER_INIT}\nmare.setup {{logo='compact'}}\nassert(require('checkview')); mare.check_view()" )).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(h.kernel.config()["logo"], "compact");
        h.shutdown().await;
    }).await
}

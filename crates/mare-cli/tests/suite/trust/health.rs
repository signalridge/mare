//! Host health stays consistent between startup and a full reload.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn optional_package_selection_error_is_reported_on_startup_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let plugins = env.config_dir().join("plugins.json");
        std::fs::write(&plugins, "{broken").unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let opts =
            env.host().user_config(false).faux(json!([{ "text": "first" }, { "text": "second" }])).no_session().opts;
        let host = mare_host::Host::new(opts).await.unwrap();
        let startup = host.startup_notices();
        assert!(
            startup.iter().any(|(message, level)| *level == "warning"
                && message.contains("plugins.json")
                && message.contains("/plugins")),
            "{startup:?}"
        );
        let first = host.prompt("first", json!({})).await.unwrap();
        assert_eq!(first["status"], "completed", "{first}");
        let reload = host.reload_all().await.unwrap();
        assert!(
            reload["notices"].to_string().contains("plugins.json")
                && reload["notices"].to_string().contains("/plugins"),
            "{reload}"
        );
        let after = host.startup_notices();
        assert!(
            after.iter().any(|(message, level)| *level == "warning"
                && message.contains("plugins.json")
                && message.contains("/plugins")),
            "{after:?}"
        );
        let second = host.prompt("second", json!({})).await.unwrap();
        assert_eq!(second["status"], "completed", "{second}");
        assert_eq!(std::fs::read_to_string(&plugins).unwrap(), "{broken");
        std::fs::remove_file(plugins).unwrap();
        host.reload_all().await.unwrap();
        assert!(host.startup_notices().iter().all(|(message, _)| !message.contains("plugins not loaded")));
        host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn broken_required_config_blocks_on_startup_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nerror('required config sentinel')\n",
        )
        .unwrap();
        let opts = env.host().user_config(false).faux(json!([{ "text": "not sent" }])).no_session().opts;
        let host = mare_host::Host::new(opts).await.unwrap();
        assert!(host.config_error().unwrap().contains("required config sentinel"));
        let first = host.prompt("first", json!({})).await.unwrap();
        assert_eq!(first["not_sent"], true, "{first}");
        assert!(host.reload_all().await.is_err(), "broken config must fail reload");
        let second = host.prompt("second", json!({})).await.unwrap();
        assert_eq!(second["not_sent"], true, "{second}");
        assert!(
            second["error"]["message"].as_str().unwrap_or_default().contains("required config sentinel"),
            "{second}"
        );
        host.shutdown().await;
    })
    .await
}

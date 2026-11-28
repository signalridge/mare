//! first run, /login, privacy. The update check sends nothing when turned off and
//! only asks GitHub releases (here a mock) when on; API keys are typed masked and stored 0600.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn the_update_check_sends_nothing_when_off_and_only_asks_releases_when_on() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.0.0"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        // off (`mare.o.update_check = false`): no request at all
        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "update_check": false, "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        global(&h, "UC", h.service_table("update-check").unwrap());
        let (res, why): (Option<mlua::Value>, String) = eval(&h, "return UC:check()").await;
        assert!(res.is_none() && why == "disabled", "{why}");
        h.shutdown().await;
        assert_eq!(mock.count("/releases"), 0, "no network when off");

        // on: one GET to the releases endpoint, nothing identifying in it
        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        global(&h, "UC", h.service_table("update-check").unwrap());
        let (latest, newer): (String, bool) = eval(&h, "local r = UC:check() return r.latest, r.newer").await;
        assert_eq!((latest.as_str(), newer), ("v99.0.0", true));
        h.shutdown().await;
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert!(r.body.is_empty(), "a GET without a body: {}", r.body);
        for h in ["authorization", "cookie", "x-api-key"] {
            assert!(r.header(h).is_none(), "{h} sent");
        }
    })
    .await
}

/// A persisted `update_check_at` timestamp in the future (e.g. after wall-clock rollback)
/// must be treated as expired rather than suppressing update checks indefinitely.
#[tokio::test]
async fn update_check_treats_future_timestamp_as_expired() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.1.0"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        let future_epoch =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 365 * 24 * 3600;
        let state_dir = env.home.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), format!(r#"{{"update_check_at":{future_epoch}}}"#)).unwrap();

        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        // Emit ui/ready in non-debug TUI mode to exercise the update_check_at throttle check.
        exec(
            &h,
            r#"local orig_mode, orig_dbg = mare.mode, mare.debug_build
               mare.mode, mare.debug_build = "tui", false
               local ok, err = pcall(function() C.cordis:get("bus"):emit("ui/ready", {}) end)
               mare.mode, mare.debug_build = orig_mode, orig_dbg
               assert(ok, tostring(err))"#,
        )
        .await;
        wait_until(WAIT_SECS, "update check request", || async { mock.count("/releases/latest") >= 1 }).await;
        assert!(
            mock.count("/releases/latest") >= 1,
            "update check must ignore future update_check_at and hit /releases/latest"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn update_check_uses_semver_precedence_for_prereleases() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route(
            "/releases/latest",
            vec![
                Resp::status(200, r#"{"tag_name":"v1.0.0"}"#),
                Resp::status(200, r#"{"tag_name":"v1.0.0-rc.10"}"#),
                Resp::status(200, r#"{"tag_name":"v1.0.0-rc.2"}"#),
                Resp::status(200, r#"{"tag_name":"v1.0.0-rc.1"}"#),
                Resp::status(200, r#"{"tag_name":"v1.0.1-rc.1"}"#),
            ],
        );
        let url = format!("{}/releases/latest", mock.base);
        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        global(&h, "UC", h.service_table("update-check").unwrap());

        for (current, expected_tag, expected_newer) in [
            ("1.0.0-rc.1", "v1.0.0", true),       // RC -> stable notifies
            ("1.0.0-rc.2", "v1.0.0-rc.10", true), // RC numeric ordering (10 > 2)
            ("1.0.0-rc.10", "v1.0.0-rc.2", false),
            ("1.0.0", "v1.0.0-rc.1", false), // stable never upgrades to RC
            ("1.0.0", "v1.0.1-rc.1", false), // stable never upgrades to higher RC
        ] {
            let (latest, newer): (String, bool) = eval(
                &h,
                &format!(
                    r#"local orig = mare.version
                       mare.version = {current:?}
                       local r = UC:check()
                       mare.version = orig
                       return r.latest, r.newer"#
                ),
            )
            .await;
            assert_eq!(
                (latest.as_str(), newer),
                (expected_tag, expected_newer),
                "current={current} tag={expected_tag}"
            );
        }
        let (cmp_rc, semver_rc, rel_rc, cmp_meta, cmp_bad): (i8, bool, bool, i8, Option<i8>) = eval(
            &h,
            r#"return mare.semver.compare("v1.0.1-rc.1", "1.0.0"),
                      mare.semver.is_newer("v1.0.1-rc.1", "1.0.0"),
                      mare.semver.is_newer_release("v1.0.1-rc.1", "1.0.0"),
                      mare.semver.compare("1.0.0+b2", "1.0.0+b1"),
                      mare.semver.compare("bad", "1.0.0")"#,
        )
        .await;
        assert_eq!((cmp_rc, semver_rc, rel_rc, cmp_meta, cmp_bad), (1, true, false, 0, None));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn update_check_policy_accepts_notify_auto_false_and_exposes_channel() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.2.0"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        // policy = "auto" is accepted and exposed on check() result along with install channel
        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "update_check": "auto", "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        assert!(h.plugin_warnings().is_empty(), "warnings: {:?}", h.plugin_warnings());
        global(&h, "UC", h.service_table("update-check").unwrap());
        let (policy, channel, off_policy): (String, String, String) = eval(
            &h,
            r#"local r = UC:check()
               return r.policy, r.channel, mare.install.policy("off", true)"#,
        )
        .await;
        assert_eq!(policy, "auto");
        assert!(!channel.is_empty());
        assert_eq!(off_policy, "disabled");
        h.shutdown().await;

        // invalid policy emits config warning when loading init.lua
        std::fs::create_dir_all(env.home.path().join("config")).unwrap();
        std::fs::write(env.home.path().join("config/init.lua"), "mare.setup { update_check = \"bogus\" }\n").unwrap();
        let cfg = mare_plugin::config_load::load(env.home.path(), false).await.unwrap();
        assert!(
            cfg.warnings.iter().any(|w| w.contains("update_check")),
            "expected warning for invalid update_check: {:?}",
            cfg.warnings
        );
    })
    .await
}

#[tokio::test]
async fn update_check_records_failed_403_attempts_and_throttles_repeat_requests() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route(
            "/releases/latest",
            vec![Resp::status(403, r#"{"message":"rate limit"}"#), Resp::status(403, r#"{"message":"rate limit"}"#)],
        );
        let url = format!("{}/releases/latest", mock.base);

        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        expose(&h);

        for _ in 0..2 {
            exec(
                &h,
                r#"local orig_mode, orig_dbg = mare.mode, mare.debug_build
                   mare.mode, mare.debug_build = "tui", false
                   C.cordis:get("bus"):emit("ui/ready", {})
                   mare.mode, mare.debug_build = orig_mode, orig_dbg"#,
            )
            .await;
            wait_until(WAIT_SECS, "first 403 attempt", || async { mock.count("/releases/latest") >= 1 }).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(mock.count("/releases/latest"), 1, "403 attempt must be throttled until interval expires");
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(env.home.path().join("state/state.json")).unwrap()).unwrap();
        assert!(
            state["update_check_at"].as_i64().unwrap_or(0) > 0,
            "failed attempt must persist update_check_at: {state}"
        );
        h.shutdown().await;
    })
    .await
}

/// Windows: a private DACL (`mare_shared::private`), below a MARE_HOME open to everyone.
#[tokio::test]
async fn login_takes_the_key_masked_and_stores_it_0600() {
    local(async {
        let env = TestEnv::new().await;
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(env.home.path());
        let h = env.host().builtins(&["update-check"]).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let secret: bool = eval(
            &h,
            r#"local secret
               mare.ui.input = function(o) secret = o.secret return "sk-ant-test-123" end
               -- the public operation (providers() gives data, not the provider's functions)
               for _, p in ipairs(LLM:providers()) do
                 if p.name == "anthropic" and p.login then LLM.login(LLM, "anthropic") end
               end
               return secret == true"#,
        )
        .await;
        assert!(secret, "the API key prompt is masked");
        let auth = env.config_dir().join("auth.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "auth.json is private");
        }
        #[cfg(windows)]
        assert_eq!(mare_shared::private::check(&auth), Ok(()), "auth.json is private");
        assert!(std::fs::read_to_string(&auth).unwrap().contains("sk-ant-test-123"));
        h.shutdown().await;
    })
    .await
}

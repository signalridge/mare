//! init.lua's `mare.on` / `mare.policy.voter`: native registrations on the host's fiber. A
//! waterfall listener takes the around form and passes on with `next`; a decision point is voted on
//! with `mare.policy.voter`; `mare.on` on a decision point is a listener, not a voter.

use super::*;

#[tokio::test]
async fn init_lua_listeners_are_native_and_vote_only_through_policy_voter() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.on("probe/wf", function(v, next) return next(v .. "+init") end)
            mare.policy.voter("tool/before", function(call)
              if call.name == "blocked" then return { decision = "deny", reason = "init.lua says no" } end
            end)
            -- a listener on a decision point does not take part in the decision
            mare.on("tool/before", function() mare._init_saw_tool_before = true return { decision = "deny" } end)
            -- on a point with no other host voter, the voter's owner is the host fiber's
            mare.policy.voter("tool/approval", function() end)
            "#,
        )
        .unwrap();
        // a short-lived VM (mare plugin / sandbox / doctor, flag discovery) evaluates the same config
        // without the host's decision points: the valid voters load there too (a-rnh4-q #1)
        let short = mare_plugin::config_load::load(&env.cwd, true).await;
        assert!(short.is_ok(), "a short VM refused the config's voters: {:?}", short.err());
        install_probe_events_plugin(&env, "probe", &[("wf", "waterfall")]);
        let h = env.host().user_config(true).user_plugins().no_session().build().await;
        expose(&h);
        let (wf, blocked, other): (String, String, String) = eval(
            &h,
            r#"local bus = C.cordis.bus
            return bus:waterfall("probe/wf", "s"),
              C.cordis:get("policy"):decide("tool/before", { { name = "blocked" } }).decision,
              C.cordis:get("policy"):decide("tool/before", { { name = "other" } }).decision"#,
        )
        .await;
        let owners = h.kernel.listener_owners("tool/approval");
        h.shutdown().await;
        assert_eq!(wf, "s+init", "the waterfall listener passed on with next ({blocked}, {other}, {owners:?})");
        assert_eq!(blocked, "deny", "mare.policy.voter voted");
        assert_eq!(other, "allow", "mare.on on a decision point is not a voter");
        assert_eq!(owners, ["<root>"], "the voter is the host fiber's");
    })
    .await
}

/// a-rnh4 #6: the host's readiness vote is the first voter of the host segment, ahead of init.lua's
/// voters (at a cold start, where the config's voters are installed in the same startup): when both
/// refuse, the reason is the host's (here an unavailable package), not the config's.
#[tokio::test]
async fn the_hosts_readiness_vote_precedes_init_lua_voters() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let missing = env.home.path().join("missing-package");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"mare.policy.voter("request/ready", function() return {{ decision = "deny", reason = "init deny" }} end)
                mare.plugins.add {{ {{ dir = {:?}, name = "local-missing", required = true }} }}"#,
                missing.to_string_lossy()
            ),
        )
        .unwrap();
        // the missing package is a startup problem on purpose: built without the builder's check
        let h = mare_host::Host::new(env.host().user_config(true).no_session().opts).await.unwrap();
        expose(&h);
        let (decision, reason): (String, String) = eval(
            &h,
            "local v = C.cordis:get('policy'):decide('request/ready', {}); return v.decision, v.reason or ''",
        )
        .await;
        h.shutdown().await;
        assert_eq!(decision, "deny");
        assert!(reason.contains("local-missing"), "the host's refusal comes first: {reason}");
    })
    .await
}

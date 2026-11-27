//! Name slots and `plugins.preset`:
//! `plugins.<name>.enabled` switches the whole slot (every source); replacing a definition always
//! takes `override = true`; `plugins.preset = "minimal"` turns off built-ins tagged neither
//! `minimal` nor `provider` as a preset default, which explicit `enabled` values beat.
//!
//! Sources: lazy.nvim `enabled` per spec (`folke/lazy.nvim/lua/lazy/types.lua:57`) and LazyVim's
//! distribution defaults before user files (`LazyVim/lua/lazyvim/config/init.lua:286-300`), per
//! /4.5. reference implementation, Codex, Kimi Code and pi have no tests for a
//! preset that switches plugin sets by tag; these cases follow the SPEC's acceptance list.
use crate::common::*;
use serde_json::json;

/// (origin, active) of the effective definition of `name`, or None when no definition is loaded.
fn slot(h: &mare_host::Host, name: &str) -> Option<(String, bool)> {
    h.kernel.plugin_status().into_iter().find(|p| p.0 == name).map(|(_, origin, active)| (origin, active))
}

fn active(h: &mare_host::Host, name: &str) -> bool {
    slot(h, name).is_some_and(|(_, a)| a)
}

fn user_plugin(env: &TestEnv, name: &str, source: &str) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.lua")), source).unwrap();
}

async fn config_warnings(h: &mare_host::Host) -> String {
    eval(h, "return table.concat(mare.config_warnings() or {}, '\\n')").await
}

/// `minimal` keeps the built-ins tagged `minimal` and every `provider`; the rest is off.
#[tokio::test]
async fn preset_minimal_keeps_tagged_builtins_and_every_provider() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "plugins": { "preset": "minimal" } })).build().await;
        for name in [
            "agent",
            "llm",
            "tools",
            "session",
            "context",
            "ui",
            "statusline",
            "commands",
            "theme-default",
            "approvals",
            "approval-review-model",
            "agent-approvals",
            "provider-faux",
            "provider-openai",
        ] {
            assert!(active(&h, name), "{name} is in the minimal set");
        }
        for name in ["review", "jobs", "mcp"] {
            assert!(!active(&h, name), "{name} is turned off by preset minimal");
        }
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null(), "the minimal set still answers");
        h.shutdown().await;
    })
    .await
}

/// Explicit `enabled` values beat the preset in both directions.
#[tokio::test]
async fn explicit_enabled_beats_preset_minimal() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({ "plugins": {
                "preset": "minimal", "jobs": { "enabled": true }, "statusline": { "enabled": false }
            } }))
            .build()
            .await;
        assert!(active(&h, "jobs"), "explicit true re-enables an untagged built-in");
        assert!(!active(&h, "statusline"), "explicit false turns off a tagged built-in");
        assert!(!active(&h, "review"), "the preset still applies to the rest");
        h.shutdown().await;
    })
    .await
}

/// A slot whose effective definition replaced the built-in (`override = true`) is not a
/// built-in any more: the preset leaves it alone. Turning the slot off removes every source.
#[tokio::test]
async fn preset_skips_an_override_and_enabled_false_closes_the_whole_slot() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "review",
            "return { name = 'review', override = true, apply = function() mare.g.user_review = true end }",
        );
        let h =
            env.host().user_plugins().no_session().config(json!({ "plugins": { "preset": "minimal" } })).build().await;
        assert_eq!(slot(&h, "review"), Some(("user".to_string(), true)), "the replacement is not a built-in");
        assert!(eval::<bool>(&h, "return mare.g.user_review == true").await);
        h.shutdown().await;

        let h = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({ "plugins": { "review": { "enabled": false } } }))
            .build()
            .await;
        assert!(!active(&h, "review"), "no source of a closed slot loads");
        assert!(!eval::<bool>(&h, "return mare.g.user_review == true").await, "the user definition didn't run");
        assert!(h.plugin_warnings().is_empty(), "a closed slot is a deliberate disable: {:?}", h.plugin_warnings());
        h.shutdown().await;
    })
    .await
}

/// An unknown preset name is a configuration error: reported, then treated as no preset.
#[tokio::test]
async fn an_unknown_preset_is_reported_and_ignored() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', plugins = { preset = 'tiny' } }\n",
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let w = config_warnings(&h).await;
        assert!(w.contains("unknown plugins.preset `tiny`") && w.contains("minimal"), "{w}");
        assert!(!w.contains("unknown plugin `preset`"), "preset is a reserved field, not a plugin name: {w}");
        assert!(active(&h, "review"), "an unknown preset turns nothing off");
        h.shutdown().await;
    })
    .await
}

/// `mare.builtins` is gone without an alias: the old key is an unknown option and has no effect.
#[tokio::test]
async fn the_builtins_key_is_gone() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', builtins = { disabled = { 'review' } } }\nG_BUILTINS = mare.builtins\n",
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let w = config_warnings(&h).await;
        assert!(w.contains("unknown option `builtins`"), "{w}");
        assert!(active(&h, "review"), "the old key has no effect");
        assert!(eval::<bool>(&h, "return G_BUILTINS == nil").await, "no mare.builtins table");
        h.shutdown().await;
    })
    .await
}

/// `preset` is a reserved field of the `plugins` table: a plugin may not be named so.
#[tokio::test]
async fn a_plugin_named_preset_is_a_naming_error() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(&env, "preset", "return { name = 'preset', apply = function() mare.g.preset_ran = true end }");
        let opts = env.host().user_plugins().no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("a refused definition must not stop startup");
        assert!(!eval::<bool>(&h, "return mare.g.preset_ran == true").await, "not loaded");
        let warnings = h.plugin_warnings();
        assert!(warnings.iter().any(|w| w.contains("`preset`") && w.contains("reserved")), "{warnings:?}");
        h.shutdown().await;
    })
    .await
}

/// two same-name definitions from one source (here `review.lua` and
/// in the user directory) are an error: neither loads, the error names both files, and the
/// built-in of that name stays. It is a warning, not a blocking failure.
#[tokio::test]
async fn two_same_name_definitions_from_one_source_both_stay_out() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(
            dir.join("review.lua"),
            "return { name = 'review', override = true, apply = function() mare.g.dup_a = true end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("review/init.lua"),
            "return { name = 'review', override = true, apply = function() mare.g.dup_b = true end }",
        )
        .unwrap();
        let opts = env.host().builtins(&["review"]).user_plugins().no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("a duplicate must not stop startup");
        assert!(!eval::<bool>(&h, "return mare.g.dup_a or mare.g.dup_b or false").await, "neither definition ran");
        assert_eq!(slot(&h, "review"), Some(("builtin".to_string(), true)), "the built-in stays");
        let warnings = h.plugin_warnings();
        let w = warnings.iter().find(|w| w.contains("`review`")).unwrap_or_else(|| panic!("{warnings:?}"));
        let normalized = w.replace('\\', "/");
        assert!(
            normalized.contains("plugins/review.lua")
                && normalized.contains("plugins/review)")
                && normalized.contains("twice"),
            "{w}"
        );
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null(), "not a blocking failure");
        h.shutdown().await;
    })
    .await
}

/// every read definition stays queryable by name and source. `status(name)` is the
/// effective definition and `sources` lists all same-name sources (origin, tags, provide,
/// override), a slot turned off by the preset included.
#[tokio::test]
async fn status_lists_every_source_of_a_name() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "review",
            "return { name = 'review', override = true, tags = { 'mine' }, apply = function() end }",
        );
        let h =
            env.host().user_plugins().no_session().config(json!({ "plugins": { "preset": "minimal" } })).build().await;
        let review: String = eval(&h, "return mare.json.encode(mare.plugins.status('review'))").await;
        let review: serde_json::Value = serde_json::from_str(&review).unwrap();
        assert_eq!(review["origin"], "user", "{review}");
        let sources = review["sources"].as_array().unwrap_or_else(|| panic!("{review}"));
        let origins: Vec<_> = sources.iter().map(|s| s["origin"].as_str().unwrap()).collect();
        assert_eq!(origins, ["builtin", "user"], "{review}");
        assert_eq!(sources[1]["override"], true, "{review}");
        assert_eq!(sources[1]["tags"], json!(["mine"]), "{review}");
        let jobs: String = eval(&h, "return mare.json.encode(mare.plugins.status('jobs'))").await;
        let jobs: serde_json::Value = serde_json::from_str(&jobs).unwrap();
        assert_eq!(jobs["defined"], false, "turned off by the preset: {jobs}");
        assert_eq!(jobs["sources"][0]["origin"], "builtin", "still queryable: {jobs}");
        h.shutdown().await;
    })
    .await
}

/// 条: a full `/reload` expands the preset again — turning it on unloads the
/// untagged built-ins, turning it off brings them back.
#[tokio::test]
async fn a_full_reload_recomputes_the_preset() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert!(!active(&h, "review"));
        std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
        h.reload_all().await.unwrap();
        assert!(active(&h, "review"), "no preset after the reload");
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        h.reload_all().await.unwrap();
        assert!(!active(&h, "review"), "the preset again after the next reload");
        assert!(active(&h, "llm"));
        h.shutdown().await;
    })
    .await
}

/// The commands a `mare --rpc` process with these args lists (`commands.list`): `/review` exists
/// only while the `review` plugin is loaded.
fn rpc_commands(home: &std::path::Path, work: &std::path::Path, args: &[&str]) -> Vec<String> {
    use std::process::Stdio;
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .args(args)
        .env("MARE_HOME", home)
        .env("HOME", home)
        .current_dir(work)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let list = rpc.call_raw("commands.list", json!({}), |_| json!(null));
    let names = list["result"]["data"]
        .as_array()
        .unwrap_or_else(|| panic!("{list}"))
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    rpc.shutdown_and_wait();
    names
}

/// Leaf by leaf across real layers of a `mare` process: a trusted project's `false` beats the
/// user's `true`, `--set` beats both; `--clean` drops user and project config but keeps
/// `--set plugins.preset=minimal`.
#[test]
fn enabled_and_preset_across_user_project_and_command_line_layers() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".mare")).unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(home.path().join("config/init.lua"), "mare.plugin('review', { enabled = true })\n").unwrap();
    std::fs::write(work.join(".mare/init.lua"), "mare.plugin('review', { enabled = false })\n").unwrap();
    let has = |args: &[&str]| rpc_commands(home.path(), &work, args).iter().any(|c| c == "review");
    assert!(!has(&["--trust"]), "the trusted project's false beats the user's true");
    assert!(has(&["--no-trust"]), "an untrusted project's config is not read");
    assert!(has(&["--trust", "--set", "plugins.review.enabled=true"]), "the command line beats both");
    assert!(has(&["--clean"]), "--clean: built-in defaults");
    assert!(!has(&["--clean", "--set", "plugins.preset=minimal"]), "--clean keeps --set plugins.preset");
}

/// A refused definition does not count for the slot: two `override = true` definitions of
/// `review` from one source both stay out, so the slot's effective definition is still the
/// built-in, and `minimal` turns it off like any untagged built-in.
#[tokio::test]
async fn refused_same_source_duplicates_do_not_shield_a_builtin_from_the_preset() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(dir.join("review.lua"), "return { name = 'review', override = true, apply = function() end }")
            .unwrap();
        std::fs::write(
            dir.join("review/init.lua"),
            "return { name = 'review', override = true, apply = function() end }",
        )
        .unwrap();
        let opts = env.host().user_plugins().no_session().config(json!({ "plugins": { "preset": "minimal" } })).opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert!(!active(&h, "review"), "the built-in is turned off by the preset: {:?}", slot(&h, "review"));
        assert!(
            h.plugin_warnings().iter().any(|w| w.contains("`review`") && w.contains("twice")),
            "{:?}",
            h.plugin_warnings()
        );
        h.shutdown().await;
    })
    .await
}

// ---- cfg-r1 ----

/// A project trusted mid-session goes through the same slot and preset
/// resolution as startup — a closed slot stays closed for the project's definition, a definition
/// without `override = true` does not take a name, and `preset` stays a reserved name.
#[tokio::test]
async fn a_project_trusted_mid_session_obeys_slots_and_reserved_names() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.cwd.join(".mare/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("jobs.lua"),
            "return { name = 'jobs', override = true, apply = function() mare.g.proj_jobs = true end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("review.lua"),
            "return { name = 'review', apply = function() mare.g.proj_review = true end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("preset.lua"),
            "return { name = 'preset', apply = function() mare.g.proj_preset = true end }",
        )
        .unwrap();
        let mut b = env
            .host()
            .builtins(&["review"])
            .faux(json!([]))
            .user_plugins()
            .config(json!({ "plugins": { "jobs": { "enabled": false } } }))
            .args(json!({ "no_session": true, "trust": "undecided" }));
        b.opts.project_trusted = false;
        let h = b.build().await;
        h.answer_trust(mare_host::TrustChoice::Trust).await.unwrap();
        let ran: (bool, bool, bool) =
            eval(&h, "return mare.g.proj_jobs == true, mare.g.proj_review == true, mare.g.proj_preset == true").await;
        assert_eq!(ran, (false, false, false), "closed slot, no override, reserved name");
        assert!(!active(&h, "jobs"));
        assert_eq!(slot(&h, "review"), Some(("builtin".to_string(), true)), "the built-in keeps its name");
        let sources: String = eval(&h, "return mare.json.encode(mare.plugins.status('review').sources)").await;
        assert!(sources.contains("\"project\""), "the project's definition is recorded: {sources}");
        h.shutdown().await;
    })
    .await
}

/// `/reload config` re-expands the plugin set when `plugins.preset` or a
/// slot's `enabled` changed — both directions.
#[tokio::test]
async fn reload_config_resolves_slots_again() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        let h = {
            std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
            env.host().user_config(false).no_session().build().await
        };
        assert!(active(&h, "review"));
        for (body, review, jobs) in [
            ("plugins = { preset = 'minimal' }", false, false),
            ("plugins = { preset = 'minimal', review = { enabled = true } }", true, false),
            ("", true, true),
            ("plugins = { jobs = { enabled = false } }", true, false),
            ("", true, true),
        ] {
            std::fs::write(&init, format!("mare.setup {{ model = 'faux/echo', {body} }}\n")).unwrap();
            h.reload_config().await.unwrap();
            assert_eq!((active(&h, "review"), active(&h, "jobs")), (review, jobs), "{body}");
        }
        h.shutdown().await;
    })
    .await
}

/// A running replacement that gets a same-source twin is refused by a full
/// `/reload` like at startup — not kept as "the running definition" — and the built-in under
/// it is decided by the preset.
#[tokio::test]
async fn a_full_reload_refuses_a_new_same_source_twin_of_a_running_plugin() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(
            dir.join("review/init.lua"),
            "return { name = 'review', override = true, apply = function() end }",
        )
        .unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert_eq!(slot(&h, "review"), Some(("user".to_string(), true)));
        std::fs::write(dir.join("review.lua"), "return { name = 'review', override = true, apply = function() end }")
            .unwrap();
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        h.reload_all().await.unwrap();
        assert!(
            !active(&h, "review"),
            "neither twin runs and the built-in is off by the preset: {:?}",
            slot(&h, "review")
        );
        assert!(
            h.plugin_warnings().iter().any(|w| w.contains("`review`") && w.contains("twice")),
            "{:?}",
            h.plugin_warnings()
        );
        h.shutdown().await;
    })
    .await
}

/// cfg-r2 should-fix 2: the running user turns into invalid Lua while a valid
/// user `review.lua` (override) appears, with `minimal` on. The unreadable one is kept as the
/// running definition, so the two are same-source twins and both are refused: the new one must
/// not decide the slot first (as a replacement it kept the built-in's slot open), so the
/// built-in `review` is decided by the preset and stays off.
#[tokio::test]
async fn an_unreadable_running_plugin_and_its_new_same_source_twin_leave_the_slot_to_the_preset() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(
            dir.join("review/init.lua"),
            "return { name = 'review', override = true, apply = function() end }",
        )
        .unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert_eq!(slot(&h, "review"), Some(("user".to_string(), true)));
        std::fs::write(dir.join("review/init.lua"), "return { name = 'review', override = true, apply = ").unwrap();
        std::fs::write(dir.join("review.lua"), "return { name = 'review', override = true, apply = function() end }")
            .unwrap();
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        let _ = h.reload_all().await;
        assert!(!active(&h, "review"), "no twin runs and the built-in is off by the preset: {:?}", slot(&h, "review"));
        h.shutdown().await;
    })
    .await
}

/// cfg-r1 should-fix 1: a refused definition does not stand for a name in `declaring`: with
/// `provider-openai` closed and two refused same-source twins declaring no wires, the
/// `openai-responses` wire still belongs to the built-in.
#[tokio::test]
async fn declaring_answers_from_the_effective_definition() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(dir.join("provider-openai")).unwrap();
        std::fs::write(
            dir.join("provider-openai.lua"),
            "return { name = 'provider-openai', override = true, apply = function() end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("provider-openai/init.lua"),
            "return { name = 'provider-openai', override = true, apply = function() end }",
        )
        .unwrap();
        let opts = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({ "plugins": { "provider-openai": { "enabled": false } } }))
            .opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        let owners: String =
            eval(&h, "return table.concat(mare.plugins.declaring('wires', 'openai-responses'), ',')").await;
        assert_eq!(owners, "provider-openai", "the built-in still declares the wire");
        let wires: String = eval(&h, "return table.concat(mare.plugins.status('provider-openai').wires, ',')").await;
        assert_eq!(wires, "openai-responses");
        h.shutdown().await;
    })
    .await
}

/// cfg-r1 should-fix 2: every built-in provider (by name, independent of its tag) stays in the
/// minimal set, and so does the permissions service.
#[tokio::test]
async fn preset_minimal_keeps_every_builtin_provider_by_name() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "plugins": { "preset": "minimal" } })).build().await;
        for name in [
            "provider-faux",
            "provider-openai",
            "provider-anthropic",
            "provider-google",
            "provider-openai-compat",
            "provider-opencode-go",
        ] {
            assert!(active(&h, name), "{name} is kept by preset minimal");
        }
        h.shutdown().await;
    })
    .await
}

// ---- cfg-r2 ----

/// Trusting a project whose config turns on `minimal` and whose plugin
/// replaces `review` (`override = true`) really runs the project's plugin — the replacement is
/// reloaded inside the config reload that already holds the reload lock.
#[tokio::test]
async fn trusting_a_project_that_changes_the_preset_runs_its_replacement() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let dir = env.cwd.join(".mare/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(env.cwd.join(".mare/init.lua"), "mare.setup { plugins = { preset = 'minimal' } }\n").unwrap();
        std::fs::write(
            dir.join("review.lua"),
            "return { name = 'review', override = true, apply = function() mare.g.proj_review = true end }",
        )
        .unwrap();
        let mut b = env.host().user_config(false).args(json!({ "no_session": true, "trust": "undecided" }));
        b.opts.project_trusted = false;
        let h = b.build().await;
        assert_eq!(slot(&h, "review"), Some(("builtin".to_string(), true)));
        h.answer_trust(mare_host::TrustChoice::Trust).await.unwrap();
        assert!(eval::<bool>(&h, "return mare.g.proj_review == true").await, "the project's replacement ran");
        assert_eq!(slot(&h, "review"), Some(("project".to_string(), true)));
        assert!(!active(&h, "jobs"), "the project's preset applies");
        h.shutdown().await;
    })
    .await
}

/// A real `/reload config` (run by the `commands` plugin) that closes the
/// `commands` slot still finishes the slot sync — the other slot change in the same config takes
/// effect although the reload's caller is disposed on the way — and nothing is left half done.
#[tokio::test]
async fn reload_config_that_closes_the_commands_slot_finishes_the_slot_sync() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        std::fs::write(
            &init,
            "mare.setup { model = 'faux/echo', plugins = { commands = { enabled = false }, query = { enabled = false } } }\n",
        )
        .unwrap();
        exec(&h, "CM:run('/reload config')").await;
        wait_until(WAIT_SECS, "the slot sync", || {
            let h = h.clone();
            async move { !active(&h, "commands") && !active(&h, "query") }
        })
        .await;
        // (`query` starts before `commands`, so it is disposed after it)
        // the reload the command started finishes even though it turned off the plugin that ran it:
        // on the cordis tree the slots read closed before that reload has returned, so the next one
        // is refused ("reload already in progress") until it has — a reload that never finished
        // fails here
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
        let again = loop {
            match h.reload_config().await {
                Ok(v) => break v,
                Err(e) if e.to_string().contains("reload already in progress") && std::time::Instant::now() < deadline => {
                    // sleep-ok: retry interval while the earlier reload is still in progress (bounded by the deadline)
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                Err(e) => panic!("the command's reload did not finish: {e}"),
            }
        };
        // the same config again: nothing pending, nothing changes
        assert!(!again["changed"].to_string().contains("slots"), "{again}");
        assert!(!active(&h, "query") && !active(&h, "commands"));
        h.shutdown().await;
    })
    .await
}

// ---- plugins.preset completeness ----

#[tokio::test]
async fn discovered_plugins_register_enabled_leaf_and_describe_preset_minimal() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "user_custom",
            "return { name = 'user_custom', tags = { 'extra' }, apply = function() mare.g.user_custom = true end }",
        );
        user_plugin(
            &env,
            "user_off",
            "return { name = 'user_off', tags = { 'tag-off' }, wires = { 'wire-off' }, apply = function() mare.g.user_off = true end }",
        );
        // Reserved name `preset` and same-source twins `dup` must not enter slot registration.
        user_plugin(&env, "preset", "return { name = 'preset', apply = function() end }");
        user_plugin(&env, "dup_a", "return { name = 'dup', apply = function() end }");
        user_plugin(&env, "dup_b", "return { name = 'dup', apply = function() end }");
        let opts = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({ "plugins": { "preset": "minimal", "jobs": { "enabled": true }, "user_off": { "enabled": false } } }))
            .opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());

        let (declared_json, described_json, view): (String, String, String) = eval(
            &h,
            r#"
            local decl = mare.config_declared()
            local decl_map = {}
            for _, k in ipairs(decl) do decl_map[k] = true end
            local d = mare.config_describe({
              'plugins.preset',
              'plugins.agent.enabled',
              'plugins.review.enabled',
              'plugins.jobs.enabled',
              'plugins.user_custom.enabled',
            })
            local overlay, connected = ui.overlay, ui.connected
            ui.connected = function() return false end
            ui.overlay = function(_, node) mare._captured_overlay = mare.json.encode(node) end
            CM:run('/config review')
            ui.overlay, ui.connected = overlay, connected
            return mare.json.encode(decl_map), mare.json.encode(d), mare._captured_overlay
            "#,
        )
        .await;

        let decl: serde_json::Value = serde_json::from_str(&declared_json).unwrap();
        for expected in [
            "plugins.preset",
            "plugins.agent.enabled",
            "plugins.review.enabled",
            "plugins.jobs.enabled",
            "plugins.user_custom.enabled",
        ] {
            assert_eq!(decl[expected], json!(true), "expected {expected} in config_declared()");
        }
        assert!(decl.get("plugins.dup.enabled").is_none(), "refused twin must not register: {decl}");
        assert!(decl.get("plugins.preset.enabled").is_none(), "reserved preset must not register: {decl}");

        let d: serde_json::Value = serde_json::from_str(&described_json).unwrap();
        assert_eq!(d["plugins.preset"]["effective"], json!("minimal"), "{d}");
        assert_eq!(d["plugins.preset"]["type"], json!("minimal"), "{d}");
        assert_eq!(d["plugins.preset"]["source"]["layer"], json!("cli"), "{d}");
        assert_eq!(d["plugins.review.enabled"]["default"], json!(true), "{d}");
        assert_eq!(d["plugins.review.enabled"]["type"], json!("boolean"), "{d}");
        assert_eq!(d["plugins.review.enabled"]["effective"], json!(false), "{d}");
        assert_eq!(d["plugins.review.enabled"]["source"]["layer"], json!("preset minimal"), "{d}");
        assert_eq!(d["plugins.agent.enabled"]["effective"], json!(true), "{d}");
        assert_eq!(d["plugins.agent.enabled"]["source"]["layer"], json!("default"), "{d}");
        assert_eq!(d["plugins.jobs.enabled"]["effective"], json!(true), "{d}");
        assert_eq!(d["plugins.jobs.enabled"]["source"]["layer"], json!("cli"), "{d}");
        assert_eq!(d["plugins.user_custom.enabled"]["effective"], json!(true), "{d}");
        assert_eq!(d["plugins.user_custom.enabled"]["source"]["layer"], json!("default"), "{d}");
        assert!(view.contains("preset minimal"), "/config must display 'preset minimal': {view}");
        assert!(!active(&h, "user_off"));
        assert!(!eval::<bool>(&h, "return mare.g.user_off == true").await, "disabled slot must not run apply");
        assert_eq!(h.kernel.declaring("tags", "tag-off"), vec!["user_off"]);
        assert_eq!(h.kernel.declaring("wires", "wire-off"), vec!["user_off"]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unknown_plugin_preset_clears_previous_preset_warns_and_fails_strict_config() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n",
        )
        .unwrap();
        let h =
            env.host().user_config(false).no_session().config(json!({ "plugins": { "preset": "tiny" } })).build().await;
        expose(&h);
        let w = config_warnings(&h).await;
        assert!(w.contains("unknown plugins.preset `tiny`") && w.contains("minimal"), "{w}");
        let validate = mare_shared::lua::core::config::validate_options_fn(h.lua()).unwrap();
        let validate_err = serde_json::to_string(
            &validate
                .call::<Vec<String>>(
                    mare_shared::lua::json_to_lua(h.lua(), &json!({ "plugins": { "preset": "tiny" } })).unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        let (preset_is_nil, review_desc): (bool, String) = eval(
            &h,
            r#"
            return (mare.config.plugins == nil or mare.config.plugins.preset == nil),
              mare.json.encode(mare.config_describe({'plugins.review.enabled'}))
            "#,
        )
        .await;
        assert!(preset_is_nil, "unknown preset must be cleared so no preset applies");
        let rd: serde_json::Value = serde_json::from_str(&review_desc).unwrap();
        assert_eq!(rd["plugins.review.enabled"]["effective"], json!(true), "{rd}");
        assert_eq!(rd["plugins.review.enabled"]["source"]["layer"], json!("default"), "{rd}");
        assert!(active(&h, "review"), "unknown preset must leave review active");
        assert!(
            validate_err.contains("unknown plugins.preset") || validate_err.contains("tiny"),
            "runtime validation must reject unknown preset: {validate_err}"
        );
        h.shutdown().await;

        let out = mare_cmd()
            .args([
                "-p",
                "--model",
                "faux/echo",
                "--no-session",
                "--strict-config",
                "--set",
                "plugins.preset=tiny",
                "hi",
            ])
            .env("MARE_HOME", env.home.path())
            .current_dir(&env.cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("unknown plugins.preset `tiny`"), "{stderr}");
    })
    .await
}

#[tokio::test]
async fn package_install_remove_and_reload_recompute_registered_slots_and_preset() {
    local(async {
        let env = TestEnv::new().await;
        let repo = crate::suite::packages::package_repo("hello", None, "v1.0.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert!(!active(&h, "hello"));

        std::fs::write(
            &init,
            format!(
                "mare.setup {{ model = 'faux/echo', plugins = {{ preset = 'minimal', review = {{ enabled = false }} }} }}\nmare.plugins.add {{ {{ 'me/hello', url = {:?} }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        h.reload_all().await.unwrap();
        assert!(active(&h, "hello"), "package plugin loads even under preset minimal");
        assert!(!active(&h, "review"), "explicitly disabled built-in stays disabled on package install");
        let after_install: String = eval(
            &h,
            "return mare.json.encode(mare.config_describe({'plugins.hello.enabled'}))",
        )
        .await;
        let d_in: serde_json::Value = serde_json::from_str(&after_install).unwrap();
        assert_eq!(d_in["plugins.hello.enabled"]["default"], json!(true), "{d_in}");
        assert_eq!(d_in["plugins.hello.enabled"]["effective"], json!(true), "{d_in}");
        assert_eq!(d_in["plugins.hello.enabled"]["source"]["layer"], json!("default"), "{d_in}");

        std::fs::write(
            &init,
            format!(
                "mare.setup {{ model = 'faux/echo', plugins = {{ preset = 'minimal', review = {{ enabled = false }}, hello = {{ enabled = false }} }} }}\nmare.plugins.add {{ {{ 'me/hello', url = {:?} }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        h.reload_all().await.unwrap();
        assert!(!active(&h, "hello"), "explicitly disabled package plugin must stay disabled");
        assert!(!active(&h, "review"), "explicitly disabled built-in stays disabled");

        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal', review = { enabled = false } } }\n").unwrap();
        h.reload_all().await.unwrap();
        assert!(!active(&h, "hello"));
        assert!(!active(&h, "review"), "explicitly disabled built-in must stay disabled after package removal");
        let still_declared: bool = eval(
            &h,
            "for _, k in ipairs(mare.config_declared()) do if k == 'plugins.hello.enabled' then return true end end return false",
        )
        .await;
        assert!(!still_declared, "removed package plugin slot must be removed from config_declared");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn clean_mode_respects_set_plugins_preset_minimal() {
    local(async {
        let env = TestEnv::new().await;
        let out = mare_cmd()
            .args([
                "-p",
                "--clean",
                "--model",
                "faux/echo",
                "--no-session",
                "--set",
                "plugins.preset=minimal",
                "--set",
                "plugins.jobs.enabled=true",
                "hi",
            ])
            .env("MARE_HOME", env.home.path())
            .current_dir(&env.cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

        for (flag, needle) in
            [("plugins.preset=tiny", "unknown plugins.preset `tiny`"), ("plugins.review.enabled=notabool", "boolean")]
        {
            let bad = mare_cmd()
                .args(["-p", "--clean", "--strict-config", "--model", "faux/echo", "--no-session", "--set", flag, "hi"])
                .env("MARE_HOME", env.home.path())
                .current_dir(&env.cwd)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert_eq!(bad.status.code(), Some(2), "{flag}: {}", String::from_utf8_lossy(&bad.stderr));
            let stderr = String::from_utf8_lossy(&bad.stderr);
            assert!(stderr.contains(needle), "{flag}: {stderr}");
        }

        // Direct Host with load_init = false checks preset provenance + slot effects
        let mut b = env.host().user_config(false).no_session().config(json!({
            "plugins": { "preset": "minimal", "jobs": { "enabled": true } }
        }));
        b.opts.load_init = false;
        b.opts.no_user_plugins = true;
        let h = b.build().await;
        expose(&h);
        assert!(!active(&h, "review"), "clean + preset=minimal turns off review");
        assert!(active(&h, "jobs"), "clean + explicit enabled=true keeps jobs");
        let desc: String = eval(&h, "return mare.json.encode(mare.config_describe({'plugins.review.enabled'}))").await;
        let d: serde_json::Value = serde_json::from_str(&desc).unwrap();
        assert_eq!(d["plugins.review.enabled"]["effective"], json!(false), "{d}");
        assert_eq!(d["plugins.review.enabled"]["source"]["layer"], json!("preset minimal"), "{d}");
        // mare.config and mare.o/_cfg_internal.C must share the same table instance in --clean mode.
        let same_table: bool = eval(
            &h,
            r#"
            mare.o["prompt"] = ">>"
            return mare.config.prompt == ">>" and mare.o["prompt"] == ">>"
            "#,
        )
        .await;
        assert!(same_table, "mare.o write must mutate mare.config directly in load_init=false mode");
        h.shutdown().await;

        // Clean + invalid preset (non-strict): config_describe reports effective = null via shared C/mare.o table.
        let mut b2 = env.host().user_config(false).no_session().config(json!({
            "plugins": { "preset": "tiny" }
        }));
        b2.opts.load_init = false;
        b2.opts.no_user_plugins = true;
        let h2 = b2.build().await;
        let desc_tiny: String =
            eval(&h2, "return mare.json.encode(mare.config_describe({'plugins.preset', 'plugins.review.enabled'}))")
                .await;
        let d2: serde_json::Value = serde_json::from_str(&desc_tiny).unwrap();
        assert!(
            d2["plugins.preset"]["effective"].is_null(),
            "clean invalid preset must describe effective = null: {d2}"
        );
        assert_eq!(d2["plugins.review.enabled"]["effective"], json!(true), "{d2}");
        h2.shutdown().await;
    })
    .await;
}

#[test]
fn clean_mode_rpc_options_set_and_get_share_config_table() {
    // End-to-end: `--clean --rpc` `options.set` and parameterless `options.get` must see the same table.
    let env_rpc = tokio::runtime::Runtime::new().unwrap().block_on(TestEnv::new());
    let rpc_env =
        std::collections::BTreeMap::from([("MARE_HOME".to_string(), env_rpc.home.path().display().to_string())]);
    let mut rpc = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--clean", "--model", "faux/echo", "--no-session"],
        &rpc_env,
        &env_rpc.cwd,
    );
    let init = rpc.call("initialize", json!({"protocol_version": "3.0"}));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    rpc.call("options.set", json!({ "session_id": sid, "values": { "prompt": ">>" } }));
    let got = rpc.call("options.get", json!({ "session_id": sid }));
    assert_eq!(got["prompt"], json!(">>"), "clean rpc options.get must see options.set write: {got}");
    let bad_model = rpc.call_raw(
        "options.set",
        json!({ "session_id": sid, "values": { "model": "no_such_provider/no_such_model" } }),
        |_| json!(null),
    );
    assert!(bad_model.get("error").is_some(), "unknown model must fail options.set: {bad_model}");
    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn reload_with_unknown_preset_and_failed_reload_preserves_live_config_and_slots() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        assert!(!active(&h, "review"));

        // Editing init.lua to an unknown preset and running /reload config & /reload
        // must succeed without mutating live old_config and crashing unchanged().
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'tiny' } }\n").unwrap();
        h.reload_config().await.expect("/reload config with unknown preset must succeed");
        assert!(active(&h, "review"), "unknown preset clears minimal on config reload");
        let w = config_warnings(&h).await;
        assert!(w.contains("unknown plugins.preset `tiny`"), "{w}");

        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\n").unwrap();
        h.reload_all().await.unwrap();
        assert!(!active(&h, "review"));
        std::fs::write(&init, "mare.setup { model = 'faux/echo', plugins = { preset = 'tiny' } }\n").unwrap();
        h.reload_all().await.expect("/reload all with unknown preset must succeed");
        assert!(active(&h, "review"));

        // Rollback check: failed reload after modifying user plugin slot must not leak core.config.state.plugin_slots.
        let before_decl: String = eval(&h, "return mare.json.encode(mare.config_declared())").await;
        user_plugin(&env, "temp_leak", "return { name = 'temp_leak', apply = function() end }");
        std::fs::write(
            &init,
            "mare.setup { model = 'faux/echo', plugins = { preset = 'minimal' } }\nerror('boom during init')\n",
        )
        .unwrap();
        assert!(h.reload_all().await.is_err());
        let after_decl: String = eval(&h, "return mare.json.encode(mare.config_declared())").await;
        assert_eq!(before_decl, after_decl, "failed reload must restore mare.plugin_slots");
        let runtime_err: String = eval(
            &h,
            "local ok, err = pcall(function() mare.plugin('preset', 'minimal') end); return ok and '' or tostring(err)",
        )
        .await;
        assert!(runtime_err.contains("cannot be modified at runtime"), "{runtime_err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn higher_layer_valid_preset_minimal_overrides_lower_layer_unknown_preset() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', plugins = { preset = 'tiny' } }\n",
        )
        .unwrap();
        // CLI valid preset overrides lower init.lua invalid preset (`--set` equivalent).
        let h = env
            .host()
            .user_config(false)
            .no_session()
            .config(json!({ "plugins": { "preset": "minimal" } }))
            .build()
            .await;
        assert!(!active(&h, "review"), "CLI valid minimal must win over user invalid tiny");
        let (eff, layer, no_match_layer): (String, String, String) = eval(
            &h,
            r#"
            local d = mare.config_describe({'plugins.preset', 'plugins.review.enabled'})
            -- Also verify Should-fix 1: if runtime C forces enabled=true without _sources, source.layer must not claim 'preset minimal'.
            mare.config.plugins.review = { enabled = true }
            local d2 = mare.config_describe({'plugins.review.enabled'})
            return tostring(d['plugins.preset'].effective),
              tostring(d['plugins.preset'].source and d['plugins.preset'].source.layer),
              tostring(d2['plugins.review.enabled'].source and d2['plugins.review.enabled'].source.layer)
            "#,
        )
        .await;
        assert_eq!(eff, "minimal");
        assert_eq!(layer, "cli");
        assert_ne!(no_match_layer, "preset minimal", "mismatched preset value must not claim preset source");
        h.shutdown().await;
        drop(env);

        // Same file / higher project layer via `mare.plugin('preset', ...)`: invalid then valid resets marker.
        let env2 = TestEnv::new().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(
            env2.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('preset', 'tiny')\nmare.plugin('preset', 'minimal')\n",
        )
        .unwrap();
        let h2 = env2.host().user_config(false).no_session().build().await;
        assert!(!active(&h2, "review"), "later mare.plugin('preset','minimal') must reset cleared flag");
        h2.shutdown().await;
    })
    .await
}

//! Project trust and instruction files: untrusted projects don't load project skills,
//! prompt templates or the project system prompt; AGENTS.md always loads (root → cwd, plus the
//! user's), capped, labelled in untrusted projects, with subdirectory files added on read; the
//! system prompt says tool results and file contents are data. One test per behavior.

use crate::common::*;
use serde_json::{Value, json};

mod health;

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn host(env: &TestEnv, trust: &str, script: Value) -> std::sync::Arc<mare_host::Host> {
    let h = env
        .host()
        .faux(script)
        .no_session()
        .args(json!({ "no_session": true, "trust": trust }))
        .project_trusted(trust == "trusted")
        .build_exposed()
        .await;
    global(&h, "X", h.service_table("context").unwrap());
    global(&h, "CMD", h.service_table("commands").unwrap());
    h
}

#[tokio::test]
async fn untrusted_projects_skip_project_skills_templates_and_system_prompt() {
    local(async {
        let env = TestEnv::new().await;
        write(
            &env.cwd.join(".mare/skills/proj-skill/SKILL.md"),
            "---\nname: proj-skill\ndescription: project skill\n---\nbody",
        );
        write(&env.cwd.join(".mare/prompts/proj-tpl.md"), "Project template $1");
        write(&env.cwd.join(".mare/SYSTEM.md"), "PROJECT-SYSTEM-PROMPT");
        write(
            &env.config_dir().join("prompts/user-tpl.md"),
            "---\ndescription: mine\n---\nUser template ${1:-none} / $ARGUMENTS",
        );
        for (trust, expect) in [("untrusted", false), ("trusted", true)] {
            let h = host(&env, trust, json!([])).await;
            let (prompt, skill, proj, user, expanded): (String, bool, bool, bool, String) = eval(
                &h,
                r#"local skill = false
                   for _, s in ipairs(X:skills()) do skill = skill or s.name == "proj-skill" end
                   return (X:system_prompt()), skill, CMD:get("proj-tpl") ~= nil, CMD:get("user-tpl") ~= nil,
                          X:expand_template("A ${1:-none} B ${@:2} C $ARGUMENTS", 'one "two three" four')"#,
            )
            .await;
            assert_eq!(skill, expect, "{trust}: project skill");
            // a trusted project's SYSTEM.md is the whole prompt
            assert_eq!(prompt == "PROJECT-SYSTEM-PROMPT", expect, "{trust}: project SYSTEM.md: {prompt}");
            assert_eq!(proj, expect, "{trust}: project prompt template");
            assert!(user, "{trust}: the user's templates always load");
            assert_eq!(expanded, "A one B two three four C one two three four");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn trust_instructions_cross_empty_git_directory_to_real_project_root() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.cwd.clone();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), "REAL-ROOT-DOC");
        let cwd = root.join("nested");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write(&cwd.join("AGENTS.md"), "CHILD-DOC");
        let mut b = env.host().no_session().args(json!({ "no_session": true, "trust": "untrusted" }));
        b.opts.cwd = cwd;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt(nil))").await;
        assert!(prompt.contains("REAL-ROOT-DOC") && prompt.contains("CHILD-DOC"), "{prompt}");
        h.shutdown().await;
    })
    .await
}

// reference implementation "treats a .git file as a project root marker and does not
// search above it": a worktree whose gitfile names a missing directory (its main repository moved)
// is where the walk ends — the outer repository's AGENTS.md is not loaded (Git: not a repository)
#[tokio::test]
async fn trust_instructions_stop_at_an_invalid_gitfile() {
    local(async {
        let env = TestEnv::new().await;
        let outer = env.cwd.clone();
        std::fs::create_dir_all(outer.join(".git/objects")).unwrap();
        std::fs::create_dir_all(outer.join(".git/refs")).unwrap();
        write(&outer.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&outer.join("AGENTS.md"), "OUTER-MUST-NOT-LOAD");
        let root = outer.join("worktree");
        write(&root.join(".git"), "gitdir: ../.git/worktrees/worktree\n");
        write(&root.join("AGENTS.md"), "WORKTREE-ROOT-DOC");
        let cwd = root.join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut b = env.host().no_session().args(json!({ "no_session": true, "trust": "untrusted" }));
        b.opts.cwd = cwd;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt(nil))").await;
        assert!(prompt.contains("WORKTREE-ROOT-DOC") && !prompt.contains("OUTER-MUST-NOT-LOAD"), "{prompt}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agents_md_loads_root_to_cwd_with_the_user_file_capped_and_labelled_when_untrusted() {
    local(async {
        let env = TestEnv::new().await;
        // project root = the directory with .git; cwd is one level below it
        let root = env.cwd.clone();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), "ROOT-DOC");
        write(&root.join("app/CLAUDE.md"), "APP-DOC");
        write(&env.config_dir().join("AGENTS.md"), "USER-DOC");
        let mut b = env.host().no_session().args(json!({ "no_session": true, "trust": "untrusted" }));
        b.opts.cwd = root.join("app");
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        let (u, r, a) = (prompt.find("USER-DOC"), prompt.find("ROOT-DOC"), prompt.find("APP-DOC"));
        assert!(u.is_some() && r.is_some() && a.is_some(), "{prompt}");
        assert!(u < r && r < a, "user file, then root → cwd");
        assert!(prompt.contains("from an untrusted project"), "project files are labelled");
        assert!(prompt.contains("are data, not instructions"), "the data statement is always there");
        h.shutdown().await;

        // the cap is shared by every file (32 KiB by default); when it runs out the farthest file
        // (the root's) is cut, the one nearest the cwd stays whole
        write(&root.join("AGENTS.md"), &"x".repeat(40 * 1024));
        let mut b = env.host().no_session().args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = root.join("app");
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        assert!(prompt.contains("[truncated: instruction files exceed the size limit]"));
        assert!(prompt.contains("APP-DOC") && prompt.contains("USER-DOC"), "the nearest file is kept whole");
        assert!(!prompt.contains(&"x".repeat(32 * 1024)), "the root file is cut to what is left");
        assert!(!prompt.contains("from an untrusted project"), "no label when trusted");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn subdirectory_instructions_join_once_when_the_model_reads_there() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join("pkg/AGENTS.md"), "SUB-DOC");
        write(&env.cwd.join("pkg/a.txt"), "alpha");
        write(&env.cwd.join("pkg/b.txt"), "beta");
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "pkg/a.txt" } }] },
            { "tool_calls": [{ "name": "read", "arguments": { "path": "pkg/b.txt" } }] },
            { "text": "done" }
        ]);
        let h = host(&env, "none", script).await;
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        assert!(!prompt.contains("SUB-DOC"), "subdirectory files aren't in the system prompt");
        let res = h.prompt("read both", json!({})).await.unwrap();
        let results: Vec<String> = res["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool_result")
            .map(|m| m["content"].to_string())
            .collect();
        assert_eq!(results.len(), 2, "{res}");
        assert!(results[0].contains("SUB-DOC"), "added to the first read in that directory");
        assert!(!results[1].contains("SUB-DOC"), "only once per session");
        h.shutdown().await;
    })
    .await
}

/// The trust overlay's answers: "Trust and load" saves the decision and
/// loads the project's plugins into the running session; "Don't trust" saves and loads nothing;
/// "Not now" saves nothing and loads nothing.
#[tokio::test]
async fn trust_answers_save_and_load_project_plugins_only_when_trusted() {
    local(async {
        use mare_host::TrustChoice;
        for (choice, saved, loaded) in [
            (TrustChoice::NotNow, None, false),
            (TrustChoice::DontTrust, Some(false), false),
            (TrustChoice::TrustSession, None, true),
            (TrustChoice::Trust, Some(true), true),
        ] {
            let env = TestEnv::new().await;
            write(
                &env.cwd.join(".mare/plugins/projplug.lua"),
                "return { name = 'projplug', apply = function(ctx) mare.g.projplug_loaded = true end }",
            );
            let mut b =
                env.host().faux(json!([])).user_plugins().args(json!({ "no_session": true, "trust": "undecided" }));
            b.opts.project_trusted = false;
            let h = b.build().await;
            let before: bool = eval(&h, "return mare.g.projplug_loaded == true").await;
            assert!(!before, "project plugins wait for the answer");
            h.answer_trust(choice).await.unwrap();
            let after: bool = eval(&h, "return mare.g.projplug_loaded == true").await;
            assert_eq!(after, loaded, "{choice:?}: project plugin loaded");
            assert_eq!(mare_config::trust::decision(&env.cwd), saved, "{choice:?}: saved decision");
            let trust: String = eval(&h, "return tostring(mare.args.trust)").await;
            let want = match choice {
                TrustChoice::Trust => "trusted",
                TrustChoice::TrustSession => "session",
                TrustChoice::DontTrust => "untrusted",
                TrustChoice::NotNow => "undecided",
            };
            assert_eq!(trust, want, "{choice:?}: mare.args.trust");
            h.shutdown().await;
        }
    })
    .await
}

/// Failed and waiting optional project plugins must not leave the workspace half trusted.
#[tokio::test]
async fn first_trust_with_optional_project_failures_synchronizes_sessions() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &env.cwd.join(".mare/plugins/healthy.lua"),
            "return { name = 'project-healthy', apply = function() mare.g.project_healthy = true end }",
        );
        write(
            &env.cwd.join(".mare/plugins/broken.lua"),
            "return { name = 'project-broken', apply = function() error('optional project sentinel') end }",
        );
        write(
            &env.cwd.join(".mare/plugins/waiting.lua"),
            "return { name = 'project-waiting', inject = { 'missing-project-service' }, apply = function() end }",
        );
        let opts = env
            .host()
            .user_config(false)
            .faux(json!([{ "text": "first" }, { "text": "second" }]))
            .args(json!({ "no_session": true, "trust": "undecided", "trust_source": "pending", "trust_pending": true }))
            .opts;
        let manager = mare_host::HostManager::new();
        let first = manager.create_session(opts.clone()).await.unwrap();
        let second = manager.create_session(opts).await.unwrap();
        for s in [&first, &second] {
            expose(&s.host);
            exec(
                &s.host,
                "TRUST_EVENTS = 0; C.cordis:on('trust/changed', function(ev) TRUST_EVENTS = TRUST_EVENTS + 1; TRUST_PAYLOAD = ev end)",
            )
            .await;
        }
        first
            .host
            .answer_trust(mare_host::TrustChoice::Trust)
            .await
            .expect("optional project failures must not fail trust");
        assert_eq!(mare_config::trust::decision(&env.cwd), Some(true));
        for s in [&first, &second] {
            assert!(s.host.is_project_trusted());
            let state: (bool, String, bool, i64, bool, bool, bool, bool) = eval(
                &s.host,
                r#"
                local bad = mare.plugins.status('project-broken').failure
                local pending = mare.plugins.status('project-waiting').failure
                return mare.trusted, mare.args.trust, mare.args.trust_pending, TRUST_EVENTS,
                  mare.g.project_healthy == true, bad ~= nil and not bad.blocks,
                  pending ~= nil and not pending.blocks, mare.plugins.status('project-healthy').active
            "#,
            )
            .await;
            assert_eq!(state, (true, "trusted".into(), false, 1, true, true, true, true));
            let event: (bool, bool) = eval(&s.host, "return TRUST_PAYLOAD.trusted, TRUST_PAYLOAD.saved").await;
            assert_eq!(event, (true, true));
            let turn = s.host.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        write(&env.cwd.join(".mare/plugins/broken.lua"), "return { name = 'project-broken', apply = function() end }");
        write(
            &env.cwd.join(".mare/plugins/waiting.lua"),
            "return { name = 'project-waiting', apply = function() end }",
        );
        for s in [&first, &second] {
            s.host.reload_all().await.unwrap();
            let problems = s.host.kernel.problems().join("\n");
            assert!(
                !problems.contains("optional project sentinel") && !problems.contains("project-waiting not started"),
                "{problems}"
            );
            let turn = s.host.prompt("again", json!({})).await.unwrap();
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        manager.shutdown_all().await;
    })
    .await
}

/// A required project failure remains fail-closed without splitting sibling trust state.
#[tokio::test]
async fn first_trust_with_required_project_failure_finishes_workspace_state() {
    local(async {
      for via_registry in [false, true] {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&env.cwd.join(".mare/plugins/required.lua"), "return { name = 'project-required', required = true, apply = function() error('required project sentinel') end }");
        let opts = env.host().user_config(false).faux(json!([{ "text": "not sent" }]))
            .args(json!({ "no_session": true, "trust": "undecided", "trust_source": "pending", "trust_pending": true })).opts;
        let manager = mare_host::HostManager::new();
        let first = manager.create_session(opts.clone()).await.unwrap();
        let second = manager.create_session(opts.clone()).await.unwrap();
        let third = manager.create_session(opts).await.unwrap();
        for s in [&first, &second, &third] {
            expose(&s.host);
            exec(&s.host, "TRUST_EVENTS = 0; C.cordis:on('trust/changed', function(ev) TRUST_EVENTS = TRUST_EVENTS + 1; TRUST_PAYLOAD = ev end)").await;
        }
        let err = if via_registry {
            manager.set_workspace_trust(&first.workspace, true, true).await.expect_err("required plugin failure must be reported").to_string()
        } else {
            first.host.answer_trust(mare_host::TrustChoice::Trust).await.expect_err("required plugin failure must be reported").to_string()
        };
        assert!(err.contains("project-required"), "{err}");
        assert_eq!(mare_config::trust::decision(&env.cwd), Some(true));
        for s in [&first, &second, &third] {
            assert!(s.host.is_project_trusted());
            let state: (bool, String, bool, i64, bool) = eval(&s.host, r#"
                local f = mare.plugins.status('project-required').failure
                return mare.trusted, mare.args.trust, mare.args.trust_pending, TRUST_EVENTS, f ~= nil and f.blocks
            "#).await;
            assert_eq!(state, (true, "trusted".into(), false, 1, true));
            let event: (bool, bool) = eval(&s.host, "return TRUST_PAYLOAD.trusted, TRUST_PAYLOAD.saved").await;
            assert_eq!(event, (true, true), "via_registry={via_registry}");
            let turn = s.host.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["not_sent"], true, "{turn}");
            assert!(turn["error"]["message"].as_str().unwrap_or_default().contains("project-required"), "{turn}");
        }
        manager.shutdown_all().await;
      }
    }).await
}

#[tokio::test]
async fn first_trust_with_broken_project_config_keeps_sessions_consistent() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
        let project_init = env.cwd.join(".mare/init.lua");
        write(&project_init, "error('project config sentinel')\n");
        let opts = env
            .host()
            .user_config(false)
            .faux(json!([{ "text": "recovered" }]))
            .args(json!({ "no_session": true, "trust": "undecided", "trust_source": "pending", "trust_pending": true }))
            .opts;
        let manager = mare_host::HostManager::new();
        let first = manager.create_session(opts.clone()).await.unwrap();
        let second = manager.create_session(opts).await.unwrap();
        for s in [&first, &second] {
            expose(&s.host);
            exec(
                &s.host,
                "TRUST_EVENTS = 0; C.cordis:on('trust/changed', function(ev) TRUST_EVENTS = TRUST_EVENTS + 1; TRUST_PAYLOAD = ev end)",
            )
            .await;
        }
        assert!(first.host.answer_trust(mare_host::TrustChoice::Trust).await.is_err());
        assert_eq!(mare_config::trust::decision(&env.cwd), Some(true));
        for s in [&first, &second] {
            assert!(s.host.is_project_trusted());
            let state: (bool, bool, i64) =
                eval(&s.host, "return mare.trusted, mare.args.trust_pending, TRUST_EVENTS").await;
            assert_eq!(state, (true, false, 1));
            let event: (bool, bool) = eval(&s.host, "return TRUST_PAYLOAD.trusted, TRUST_PAYLOAD.saved").await;
            assert_eq!(event, (true, true));
            assert!(
                s.host.kernel.problems().iter().any(|p| p.contains("project config sentinel")),
                "{:?}",
                s.host.kernel.problems()
            );
            let turn = s.host.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["not_sent"], true, "{turn}");
        }
        write(&project_init, "-- repaired\n");
        for s in [&first, &second] {
            s.host.reload_all().await.unwrap();
            assert!(
                s.host.kernel.problems().iter().all(|p| !p.contains("project config sentinel")),
                "{:?}",
                s.host.kernel.problems()
            );
            let turn = s.host.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        manager.shutdown_all().await;
    })
    .await
}

#[tokio::test]
async fn automatic_trust_with_required_project_failure_synchronizes_sessions() {
    local(async {
        for via_plugin in [false, true] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
            std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
            write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
            write(&env.cwd.join(".mare/plugins/required.lua"), "return { name = 'project-required', required = true, apply = function() error('required project sentinel') end }");
            if via_plugin {
                write(&env.config_dir().join("plugins/trust-policy.lua"), r#"
                    return { name = 'trust-policy', inject = { 'bus' }, apply = function(ctx)
                      ctx:on('trust/resolve', function(ev, next) ev.decision = true; return next(ev) end)
                    end }
                "#);
            }
            let default = if via_plugin { "ask" } else { "trust" };
            let opts = env.host().user_config(false).faux(json!([{ "text": "not sent" }]))
                .config(json!({ "trust_default": default }))
                .args(json!({ "no_session": true, "trust": "undecided", "trust_source": "pending", "trust_pending": true })).opts;
            let manager = mare_host::HostManager::new();
            let first = manager.create_session(opts.clone()).await.unwrap();
            let second = manager.create_session(opts).await.unwrap();
            for s in [&first, &second] {
                expose(&s.host);
                exec(&s.host, "TRUST_EVENTS = 0; C.cordis:on('trust/changed', function(ev) TRUST_EVENTS = TRUST_EVENTS + 1; TRUST_PAYLOAD = ev end)").await;
            }
            let err = first.host.settle_trust().await.expect_err("required plugin failure must be reported");
            assert!(err.to_string().contains("project-required"), "{err}");
            assert_eq!(mare_config::trust::decision(&env.cwd), None, "automatic trust is not saved");
            for s in [&first, &second] {
                assert!(s.host.is_project_trusted(), "via_plugin={via_plugin}");
                let state: (bool, bool, i64) = eval(&s.host, "return mare.trusted, mare.args.trust_pending, TRUST_EVENTS").await;
                assert_eq!(state, (true, false, 1), "via_plugin={via_plugin}");
                let event: (bool, bool) = eval(&s.host, "return TRUST_PAYLOAD.trusted, TRUST_PAYLOAD.saved").await;
                assert_eq!(event, (true, false), "via_plugin={via_plugin}");
                let turn = s.host.prompt("hello", json!({})).await.unwrap();
                assert_eq!(turn["not_sent"], true, "{turn}");
            }
            manager.shutdown_all().await;
        }
    }).await
}

#[tokio::test]
async fn rpc_first_trust_with_optional_project_failure_completes_for_both_sessions() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&env.cwd.join(".mare/plugins/healthy.lua"), "return { name = 'project-healthy', apply = function() mare.g.project_healthy = true end }");
        write(&env.cwd.join(".mare/plugins/broken.lua"), "return { name = 'project-broken', apply = function() error('optional project sentinel') end }");
        write(&env.config_dir().join("plugins/trust-probe.lua"), r#"
            return { name = 'trust-probe', inject = { 'bus' }, provide = { 'trust_probe' },
              contracts = { trust_probe = { methods = { get = {
                doc = 'Observe trust and plugin health', expose = 'both', scope = 'session',
                params = { type = 'object', properties = {}, additionalProperties = false }, order = {},
                result = { type = 'object', properties = {
                  trusted = { type = 'boolean' }, pending = { type = 'boolean' }, events = { type = 'integer' },
                  saved = { type = 'boolean' }, event_trusted = { type = 'boolean' },
                  healthy = { type = 'boolean' }, failed = { type = 'boolean' }, blocks = { type = 'boolean' }
                }, required = { 'trusted', 'pending', 'events', 'saved', 'event_trusted', 'healthy', 'failed', 'blocks' }, additionalProperties = false }
              } } } },
              apply = function(ctx)
                local count, saved, event_trusted = 0, false, false
                ctx:on('trust/changed', function(ev) count = count + 1; saved = ev.saved; event_trusted = ev.trusted end)
                require('mare_lib').service(ctx, 'trust_probe', { get = function()
                  local failure = mare.plugins.status('project-broken').failure
                  return { trusted = mare.trusted == true, pending = mare.args.trust_pending == true, events = count,
                    saved = saved, event_trusted = event_trusted,
                    healthy = mare.g.project_healthy == true, failed = failure ~= nil, blocks = failure ~= nil and failure.blocks }
                end })
              end }
        "#);
        let script = env.home.path().join("faux.json");
        write(&script, &json!([{ "text": "first" }, { "text": "second" }]).to_string());
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--no-trust", "--model", "faux/echo"])
            .env("MARE_HOME", env.home.path()).env("MARE_FAUX_SCRIPT", &script)
            .current_dir(&env.cwd).stderr(std::process::Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init = rpc.call("initialize", json!({ "protocol_version": "3.0" }));
        let first = init["session"]["session_id"].as_str().unwrap().to_string();
        let second = rpc.call("session.create", json!({}))["session_id"].as_str().unwrap().to_string();
        assert_ne!(first, second);
        let changed = rpc.call("workspace.set_trust", json!({ "cwd": env.cwd, "trusted": true, "save": true }));
        assert_eq!(changed, json!({}));
        assert_eq!(mare_config::trust::decision(&env.cwd), Some(true));
        for sid in [&first, &second] {
            let state = rpc.call("trust_probe.get", json!({ "session_id": sid }));
            assert_eq!(state, json!({ "trusted": true, "pending": false, "events": 1, "saved": true, "event_trusted": true, "healthy": true, "failed": true, "blocks": false }));
            let prompt = rpc.call("agent.prompt", json!({ "session_id": sid, "input": "hello" }));
            let turn = rpc.call("agent.wait", json!({ "session_id": sid, "turn_id": prompt["turn_id"] }));
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        rpc.shutdown_and_wait();
    }).await
}

/// The trust order after the command line: a
/// `trust/resolve` plugin decides before saved decisions — trusting an undecided folder loads its
/// plugins now, distrusting a saved-trusted one reloads without them — and nothing is saved;
/// without a plugin, `trust_default` decides an undecided folder ("ask" leaves it to the overlay).
#[tokio::test]
async fn trust_resolve_plugins_and_the_default_settle_the_decision() {
    local(async {
        let loaded = |h: &mare_host::Host| h.kernel.plugin_status().iter().any(|(n, _, active)| n == "projplug" && *active);
        // (plugin decision, saved decision, trust_source, trust_default) → (loaded, asks)
        for (policy, saved, source, default, want_loaded, want_ask) in [
            (Some(true), None, "undecided", "ask", true, false),
            (Some(false), Some(true), "saved", "ask", false, false),
            (None, None, "undecided", "trust", true, false),
            (None, None, "pending", "distrust", false, false),
            (None, None, "pending", "ask", false, true),
        ] {
            let env = TestEnv::new().await;
            write(
                &env.cwd.join(".mare/plugins/projplug.lua"),
                "return { name = 'projplug', apply = function(ctx) end }",
            );
            if let Some(p) = policy {
                write(
                    &env.config_dir().join("plugins/policy.lua"),
                    &format!(
                        "return {{ name = 'policy', inject = {{ 'bus' }}, apply = function(ctx) ctx:on('trust/resolve', function(ev, next)
                           assert(ev.resources.plugins == 1, 'resources are listed')
                           ev.decision = {p}; return next(ev) end) end }}"
                    ),
                );
            }
            if let Some(s) = saved {
                mare_config::trust::set(&env.cwd, s).unwrap();
            }
            let trusted_at_start = saved == Some(true);
            let mut b = env.host().faux(json!([])).user_plugins().config(json!({ "trust_default": default })).args(
                json!({ "no_session": true, "trust": if trusted_at_start { "trusted" } else { "undecided" }, "trust_source": source }),
            );
            b.opts.project_trusted = trusted_at_start;
            let h = b.build().await;
            assert_eq!(loaded(&h), trusted_at_start);
            let ask = h.settle_trust().await.unwrap();
            let case = format!("{policy:?}/{saved:?}/{source}/{default}");
            assert_eq!((loaded(&h), ask), (want_loaded, want_ask), "{case}");
            let trust: String = eval(&h, "return tostring(mare.args.trust)").await;
            let want = if want_loaded {
                "trusted"
            } else if want_ask {
                "undecided"
            } else {
                "untrusted"
            };
            assert_eq!(trust, want, "{case}: mare.args.trust");
            assert_eq!(mare_config::trust::decision(&env.cwd), saved, "{case}: nothing new is saved");
            h.shutdown().await;
        }
    })
    .await
}

/// Headless never asks: with no saved decision the project resources stay unloaded and stderr
/// says why.
#[tokio::test]
async fn headless_without_a_decision_skips_project_resources_and_says_so() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/prompts/tpl.md"), "template");
        let out = mare_cmd()
            .args(["-p", "hi", "--model", "faux/echo", "--no-session"])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("were not loaded") && err.contains("--trust"), "{err}");
        assert_eq!(mare_config::trust::decision(&env.cwd), None, "headless saves nothing");
    })
    .await
}

/// The overlay on a real terminal: it appears over a live first frame, Esc = "Not now" (nothing
/// saved or loaded), selecting "Trust and load" saves and loads the project plugin.
#[cfg(unix)]
#[test]
fn the_trust_overlay_appears_after_the_first_frame_and_answers_take_effect() {
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn run_case(keys: &[u8], expect_trust: bool, expect_loaded: bool) -> (serde_json::Value, bool) {
        let cmd = MareCmd::new().no_init().no_term_probe().args(["--model", "faux/echo", "--no-session"]);
        let home = cmd.home_path().to_path_buf();
        let work = cmd.work_path().to_path_buf();
        let cmd = cmd.env("HOME", &home);

        let plugins_dir = work.join(".mare").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let marker = work.join("loaded.txt");
        let marker_lit = serde_json::to_string(&marker.to_string_lossy().as_ref()).unwrap();
        std::fs::write(
            plugins_dir.join("projplug.lua"),
            format!(
                "return {{ name = 'projplug', apply = function(ctx) \
                 local f = io.open({marker_lit}, 'w'); f:write('yes'); f:close() end }}"
            ),
        )
        .unwrap();

        let mut p = cmd.pty(100, 30);
        p.wait_raw("trust overlay", |raw| {
            find_subslice(raw, b"Trust this folder?").is_some() && find_subslice(raw, b"all keys").is_some()
        });
        let raw = p.bytes();
        let first_box = find_subslice(&raw, "╰".as_bytes()).expect("input box");
        let question = find_subslice(&raw, b"Trust this folder?").expect("trust question");
        assert!(first_box < question, "the first frame (input box) must come before the question");

        let before = raw.len();
        p.send(keys);
        let trust_file = home.join("state").join("trust.json");
        if expect_trust {
            let _saved = p.wait_file(&trust_file);
        } else {
            p.wait_raw("trust overlay closed after dismiss", |r| {
                r.len() > before && find_subslice(&r[before..], b"Ask mare anything").is_some()
            });
            let after_close = p.bytes().len();
            p.send(b"zq7");
            p.wait_raw("input box took keys after Esc", |r| {
                r.len() > after_close && find_subslice(&r[after_close..], b"zq7").is_some()
            });
        }
        if expect_loaded {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
            while !marker.exists() {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for project plugin marker");
                std::thread::sleep(std::time::Duration::from_millis(20)); // sleep-ok: poll until project plugin marker exists
            }
        }

        mare_binding::platform::signal_tree(Some(p.pid()), mare_binding::platform::Signal::Term);
        p.wait_exit(WAIT_SECS);

        let saved = if trust_file.exists() {
            let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&trust_file).unwrap()).unwrap();
            assert_eq!(val["version"], 1, "trust.json carries its format version: {val}");
            val["decisions"].clone()
        } else {
            serde_json::json!({})
        };
        (saved, marker.exists())
    }

    let (saved, loaded) = run_case(b"\x1b", false, false);
    let all_null = saved.as_object().is_some_and(|m| m.values().all(|v| v.is_null()));
    assert!(all_null, "Esc saved a decision: {saved}");
    assert!(!loaded, "Esc (Not now) loaded the project plugin");

    let (saved, loaded) = run_case(b"\x03", false, false);
    assert!(saved.as_object().is_some_and(|m| m.is_empty()), "Ctrl+C saved a decision: {saved}");
    assert!(!loaded, "Ctrl+C (Not now) loaded the project plugin");

    let (saved, loaded) = run_case(b"\r", false, false);
    assert!(saved.as_object().is_some_and(|m| m.is_empty()), "Enter saved a decision: {saved}");
    assert!(!loaded, "Enter (Not now) loaded the project plugin");

    let (saved, loaded) = run_case(b"\x1b[A\x1b[A\r", false, true);
    assert!(
        saved.as_object().is_some_and(|m| m.is_empty()),
        "Trust this session only must not persist to disk: {saved}"
    );
    assert!(loaded, "Trust this session only didn't load the project plugin");

    let (saved, loaded) = run_case(b"\x1b[A\x1b[A\x1b[A\r", true, true);
    let any_true = saved.as_object().is_some_and(|m| m.values().any(|v| v.as_bool() == Some(true)));
    assert!(any_true, "Trust and load wasn't saved: {saved}");
    assert!(loaded, "Trust and load didn't load the project plugin");
}

fn init_git(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
    std::fs::create_dir_all(dir.join(".git/refs")).unwrap();
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
}

#[tokio::test]
async fn cfg_root_trusted_repo_root_init_lua_sets_option_from_subdir() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub/dir");
        init_git(&repo);
        std::fs::create_dir_all(&sub).unwrap();
        write(&repo.join(".mare/init.lua"), "mare.setup { model = 'faux/echo' }\n");

        mare_config::trust::set_dir(&repo, true).unwrap();

        let out = mare_cmd()
            .args(["-p", "hi", "--json", "--no-session"])
            .current_dir(&sub)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "trusted root init.lua should set model without --trust: stderr={stderr}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("hi") && stdout.contains("echo"), "expected faux/echo response in stdout: {stdout}");
    })
    .await
}

#[tokio::test]
async fn cfg_root_session_workspace_identity_stays_at_start_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub/dir");
        init_git(&repo);
        std::fs::create_dir_all(&sub).unwrap();
        write(&repo.join(".mare/init.lua"), "mare.setup { model = 'faux/echo' }\n");
        mare_config::trust::set_dir(&repo, true).unwrap();

        let out = mare_cmd()
            .args(["-p", "hello", "--json"])
            .current_dir(&sub)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

        let sessions_dir = env.home.path().join("state/sessions");
        let sub_slug = mare_store::cwd_slug(&sub);
        let repo_slug = mare_store::cwd_slug(&repo);
        assert!(sessions_dir.join(&sub_slug).is_dir(), "session must be keyed by start cwd slug {sub_slug}");
        assert!(!sessions_dir.join(&repo_slug).exists(), "session must NOT be keyed by repo root {repo_slug}");

        // Also check mare.project exposure in-process when starting in sub.
        let mut b = env.host().faux(json!([])).no_session().args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = sub.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        let (root, start): (Option<String>, String) = eval(&h, "return mare.project.root, mare.project.start").await;
        assert_eq!(root.as_deref(), Some(repo.to_string_lossy().as_ref()));
        assert_eq!(std::path::Path::new(&start), sub);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cfg_root_sandbox_protects_subdir_resource_root_mare_and_mcp_json() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub");
        let deep = sub.join("deep");
        init_git(&repo);
        std::fs::create_dir_all(sub.join(".mare")).unwrap();
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();
        std::fs::create_dir_all(&deep).unwrap();

        let mut b = env.host().faux(json!([])).no_session().args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = sub.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "REPO", repo.to_string_lossy().to_string());
        global(&h, "SUB", sub.to_string_lossy().to_string());
        global(&h, "DEEP", deep.to_string_lossy().to_string());

        let (mare_ok, mcp_ok, other_ok): (bool, bool, bool) = eval(
            &h,
            r#"return mare.exec.run({ cwd = SUB, sandbox = { level = "workspace-write", writable = { REPO }, network = true } }, function()
                local m = pcall(mare.fs.write, SUB .. "/.mare/x", "bad")
                local j = pcall(mare.fs.write, SUB .. "/.mcp.json", "bad")
                local o = pcall(mare.fs.write, SUB .. "/other.txt", "ok")
                return m, j, o
            end)"#,
        )
        .await;
        assert!(!mare_ok, "repo/sub/.mare/x must be refused");
        assert!(!mcp_ok, "repo/sub/.mcp.json must be refused");
        assert!(other_ok, "repo/sub/other.txt must be allowed");

        // `SUB/.mcp.json` is itself excluded under `SUB` (and also covered below via `project_protected`
        // when cwd is `DEEP` with `write = { REPO }`).
        // Also when starting in deep (sub/deep != sub) with workspace write={REPO}:
        let (deep_mare_ok, deep_mcp_ok): (bool, bool) = eval(
            &h,
            r#"return mare.exec.run({ cwd = DEEP, sandbox = { level = "workspace-write", write = { REPO }, network = true } }, function()
                local m = pcall(mare.fs.write, SUB .. "/.mare/y", "bad")
                local j = pcall(mare.fs.write, SUB .. "/.mcp.json", "bad")
                return m, j
            end)"#,
        )
        .await;
        assert!(!deep_mare_ok, "repo/sub/.mare/y from sub/deep must be refused");
        assert!(!deep_mcp_ok, "repo/sub/.mcp.json from sub/deep must be refused");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cfg_root_doctor_cli_trust_dir_and_headless_message() {
    local(async {
        let env = TestEnv::new().await;
        let base = dunce::canonicalize(&env.cwd).unwrap();
        let repo = base.join("repo");
        let sub = repo.join("sub");
        let leaf = sub.join("leaf");
        let outside = base.join("outside-mare");
        init_git(&repo);
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, leaf.join(".mare")).unwrap();

        // mare doctor --json reports project resource root, ignored project markers, and ignored symlinks.
        let doc = mare_cmd()
            .args(["doctor", "--json"])
            .current_dir(&leaf)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        let doc_out = String::from_utf8_lossy(&doc.stdout);
        let doc_val: Value = serde_json::from_str(&doc_out).unwrap_or_else(|e| panic!("bad json {e}: {doc_out}"));
        let checks = doc_val["checks"].as_array().expect("missing checks array");
        let find_check = |name: &str| checks.iter().find(|c| c["name"].as_str() == Some(name)).cloned();
        let rr_check = find_check("project resource root").expect("missing project resource root");
        assert!(
            rr_check["detail"].as_str().unwrap_or("").contains(&sub.display().to_string())
                && rr_check["detail"].as_str().unwrap_or("").contains(&leaf.display().to_string()),
            "{rr_check}"
        );
        let ign_check = find_check("ignored project markers").expect("missing ignored project markers");
        assert!(ign_check["detail"].as_str().unwrap_or("").contains(&repo.display().to_string()), "{ign_check}");
        #[cfg(unix)]
        {
            let rej_check = find_check("ignored symlinks").expect("missing ignored symlinks");
            assert!(
                rej_check["detail"].as_str().unwrap_or("").contains("outside the root")
                    && rej_check["detail"].as_str().unwrap_or("").contains(".mare"),
                "{rej_check}"
            );
        }

        // `mare trust <dir>` sets trust on exact dir without inheriting down to child with own root.
        let tr = mare_cmd()
            .args(["trust", repo.to_str().unwrap()])
            .current_dir(&leaf)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        assert!(tr.status.success(), "stderr={}", String::from_utf8_lossy(&tr.stderr));
        // Still untrusted for `leaf` (whose resource root is `sub`, not `repo`):
        std::fs::create_dir_all(sub.join(".mare/prompts")).unwrap();
        std::fs::write(sub.join(".mare/prompts/p.md"), "prompt").unwrap();
        let hd = mare_cmd()
            .args(["-p", "hi", "--model", "faux/echo", "--no-session"])
            .current_dir(&leaf)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        let err = String::from_utf8_lossy(&hd.stderr);
        assert!(err.contains(&sub.display().to_string()) && err.contains("were not loaded"), "{err}");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn cfg_root_rejected_mare_symlink_is_not_loaded_even_when_sibling_mcp_is_root() {
    local(async {
        let env = TestEnv::new().await;
        let base = dunce::canonicalize(&env.cwd).unwrap();
        let repo = base.join("repo");
        let sub = repo.join("sub");
        let evil = repo.join("evil");
        init_git(&repo);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();
        write(&evil.join("init.lua"), "mare.setup { model = 'faux/echo' }\n");
        write(
            &evil.join("plugins/evil.lua"),
            "return { name = 'evil-plug', apply = function() mare.g.evil_ran = true end }\n",
        );
        std::os::unix::fs::symlink(&evil, sub.join(".mare")).unwrap();

        let sources = mare_plugin::loader::discover(&serde_json::json!({}), &sub, true, false);
        assert!(
            !sources.iter().any(|s| s.origin == mare_kernel::Origin::Project),
            "rejected .mare symlink must not yield project plugin sources: {:?}",
            sources.iter().map(|s| (&s.origin, &s.path)).collect::<Vec<_>>()
        );
        let cfg = mare_plugin::config_load::load(&sub, true).await.unwrap();
        assert_ne!(cfg.get_str("model"), Some("faux/echo"), "rejected .mare symlink must not execute evil init.lua");
    })
    .await
}

#[tokio::test]
async fn cfg_root_sandbox_protects_workspace_root_mcp_json() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        init_git(&repo);
        std::fs::write(repo.join(".mcp.json"), "{}").unwrap();

        let mut b = env.host().faux(json!([])).no_session().args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = repo.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "REPO", repo.to_string_lossy().to_string());
        let (mcp_ok, other_ok): (bool, bool) = eval(
            &h,
            r#"return mare.exec.run({ cwd = REPO, sandbox = { level = "workspace-write", network = true } }, function()
                local m = pcall(mare.fs.write, REPO .. "/.mcp.json", "evil")
                local o = pcall(mare.fs.write, REPO .. "/ok.txt", "ok")
                return m, o
            end)"#,
        )
        .await;
        assert!(!mcp_ok, "workspace root .mcp.json must be protected under workspace-write");
        assert!(other_ok, "normal file in workspace root must be writable");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cfg_root_cli_trust_without_dir_resolves_to_resource_root() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub1 = repo.join("sub1");
        let sub2 = repo.join("sub2");
        init_git(&repo);
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::create_dir_all(&sub2).unwrap();
        write(&repo.join(".mare/init.lua"), "mare.setup { model = 'faux/echo' }\n");

        // Running `mare trust` without <dir> in sub1 resolves to repo root and prints it.
        let tr = mare_cmd()
            .args(["trust"])
            .current_dir(&sub1)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        assert!(tr.status.success(), "stderr={}", String::from_utf8_lossy(&tr.stderr));
        let stdout = String::from_utf8_lossy(&tr.stdout);
        assert!(
            stdout.contains(&repo.display().to_string()) && !stdout.contains(&sub1.display().to_string()),
            "expected printed resource root {}, got: {stdout}",
            repo.display()
        );

        // Now starting in sub2 loads repo/.mare/init.lua without --trust.
        let out = mare_cmd()
            .args(["-p", "ping", "--json", "--no-session"])
            .current_dir(&sub2)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
        let out_str = String::from_utf8_lossy(&out.stdout);
        assert!(out_str.contains("ping") && out_str.contains("echo"), "expected faux/echo output: {out_str}");

        // Non-existent dir argument is refused.
        let bad = mare_cmd()
            .args(["trust", "does-not-exist"])
            .current_dir(&repo)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        assert!(!bad.status.success(), "non-existent dir must fail");
    })
    .await
}

/// `TrustChoice::TrustSession`:
/// - Does NOT persist to disk (`trust.json` stays None).
/// - Does NOT sync sibling hosts on the same workspace.
/// - Revokes session-only trust on `/new` (`session:create`), `/resume` (`session:load`), and restart.
#[tokio::test]
async fn trust_session_only_is_scoped_and_revokes_on_new_resume_and_restart() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.project_only_marker = 'session-secret-marker'\n");
        write(
            &env.cwd.join(".mare/plugins/projplug.lua"),
            r#"
            return {
              name = "projplug",
              apply = function(ctx)
                mare.g.projplug_loaded = true
                mare.g.projplug_observed_switches = mare.g.projplug_observed_switches or 0
                ctx:on("session/changed", function()
                  mare.g.projplug_observed_switches = mare.g.projplug_observed_switches + 1
                end)
                ctx:on("session/new", function()
                  mare.g.projplug_observed_switches = mare.g.projplug_observed_switches + 1
                end)
                ctx:on("session/loaded", function()
                  mare.g.projplug_observed_switches = mare.g.projplug_observed_switches + 1
                end)
                ctx:effect(function()
                  return function() mare.g.projplug_loaded = false end
                end)
              end,
            }
            "#,
        );
        let saved_session = env.write_session("resume-target.jsonl", &[msg("m1", None, "user", "hello")]);
        let manager = mare_host::HostManager::new();
        let mut opts = env
            .host()
            .user_plugins()
            .faux(json!([{ "text": "ok" }, { "text": "ok2" }]))
            .args(json!({ "trust": "undecided" }))
            .opts;
        opts.load_init = true;
        opts.project_trusted = false;

        let first = manager.create_session(opts.clone()).await.unwrap();
        let sibling = manager.create_session(opts.clone()).await.unwrap();

        // Answering TrustSession trusts ONLY `first`, not `sibling`, and persists nothing.
        first.host.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert_eq!(mare_config::trust::decision(&env.cwd), None, "TrustSession must never persist to disk");
        assert!(first.host.is_project_trusted(), "first host must become trusted for this session");
        assert_eq!(
            first.host.config().get("project_only_marker").and_then(|v| v.as_str()),
            Some("session-secret-marker")
        );
        assert!(!sibling.host.is_project_trusted(), "sibling host must NOT receive TrustSession");
        assert!(
            sibling.host.config().get("project_only_marker").is_none_or(|v| v.is_null()),
            "sibling host must not inherit TrustSession init.lua marker: {:?}",
            sibling.host.config()
        );
        assert!(
            eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await,
            "first host must load project plugin"
        );
        assert!(
            !eval::<bool>(&sibling.host, "return mare.g.projplug_loaded == true").await,
            "sibling host must not load project plugin"
        );
        // WorkspaceSnapshot must stay untrusted AND keep untrusted baseline config when sibling or only session-only host exists.
        let ws_snap = manager.workspaces().get(&mare_host::WorkspaceId::local(&env.cwd).unwrap()).unwrap();
        assert!(!ws_snap.trusted, "workspace snapshot stays untrusted");
        assert!(
            ws_snap.config_snapshot.get("project_only_marker").is_none_or(|v| v.is_null()),
            "workspace snapshot config must remain untrusted baseline: {:?}",
            ws_snap.config_snapshot
        );

        // 1) `/new` (`session:create`) revokes TrustSession BEFORE session/changed or session/new runs.
        expose(&first.host);
        exec(
            &first.host,
            r#"
            SESSION_TRUST_REVOKED = 0
            C.cordis:on("trust/changed", function(ev)
                if type(ev) == "table" and ev.source == "session" and ev.trusted == false then
                    SESSION_TRUST_REVOKED = SESSION_TRUST_REVOKED + 1
                end
            end)
            local s = assert(C.cordis:get("session"))
            s:switch({ kind = "new" })
            "#,
        )
        .await;
        assert!(!first.host.is_project_trusted(), "/new must synchronously complete TrustSession revocation barrier");
        assert!(!eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await);
        assert_eq!(
            eval::<i64>(&first.host, "return mare.g.projplug_observed_switches or 0").await,
            0,
            "project plugin must NEVER observe incoming session hooks on /new"
        );
        assert!(
            first.host.config().get("project_only_marker").is_none_or(|v| v.is_null()),
            "project init.lua marker must be cleared on /new: {:?}",
            first.host.config()
        );
        assert_eq!(eval::<String>(&first.host, "return tostring(mare.args.trust)").await, "undecided");
        assert_eq!(eval::<i64>(&first.host, "return SESSION_TRUST_REVOKED").await, 1);

        // 2) TrustSession again, then `/resume` (`session:switch load`) must revoke BEFORE session/changed or session/loaded.
        first.host.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(first.host.is_project_trusted());
        assert!(eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await);

        global(&first.host, "SAVED_SESSION", saved_session.to_string_lossy().to_string());
        exec(&first.host, "local s = assert(C.cordis:get('session')); s:switch({ kind = 'load', path = SAVED_SESSION })").await;
        assert!(
            !first.host.is_project_trusted(),
            "/resume must synchronously complete TrustSession revocation barrier"
        );
        assert!(!eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await);
        assert_eq!(
            eval::<i64>(&first.host, "return mare.g.projplug_observed_switches or 0").await,
            0,
            "project plugin must NEVER observe incoming session hooks on /resume"
        );
        assert_eq!(mare_config::trust::decision(&env.cwd), None);

        // 3) TrustSession again, then `/fork` (`session:switch fork`) must ALSO revoke BEFORE session/changed or session/loaded.
        first.host.prompt("hi", json!({})).await.unwrap();
        first.host.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(first.host.is_project_trusted());
        assert!(eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await);

        exec(
            &first.host,
            "local s = assert(C.cordis:get('session')); s:switch({ kind = 'fork', session_id = s:info().session_id, name = 'forked' })",
        )
        .await;
        assert!(!first.host.is_project_trusted(), "/fork must synchronously complete TrustSession revocation barrier");
        assert!(!eval::<bool>(&first.host, "return mare.g.projplug_loaded == true").await);
        assert_eq!(
            eval::<i64>(&first.host, "return mare.g.projplug_observed_switches or 0").await,
            0,
            "project plugin must NEVER observe incoming session hooks on /fork"
        );
        assert_eq!(mare_config::trust::decision(&env.cwd), None);

        // 4) Single session-only host in WorkspaceRegistry must ALSO keep untrusted baseline config in snapshot.
        let _ = manager.close_session(&sibling.session_id).await;
        first.host.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        let single_snap = manager.workspaces().get(&mare_host::WorkspaceId::local(&env.cwd).unwrap()).unwrap();
        assert!(!single_snap.trusted, "single session-only host must report workspace snapshot trusted=false");
        assert!(
            single_snap.config_snapshot.get("project_only_marker").is_none_or(|v| v.is_null()),
            "single session-only host must not leak project-merged config into WorkspaceSnapshot: {:?}",
            single_snap.config_snapshot
        );

        // 5) Restarting a fresh Host on the same cwd starts untrusted.
        let mut restart_opts = env.host().user_plugins().faux(json!([])).opts;
        restart_opts.load_init = true;
        restart_opts.project_trusted = mare_config::trust::decision(&env.cwd).unwrap_or(false);
        let restarted = mare_host::Host::new(restart_opts).await.unwrap();
        assert!(!restarted.is_project_trusted(), "restarted host must not inherit TrustSession");
        assert!(!eval::<bool>(&restarted, "return mare.g.projplug_loaded == true").await);
        restarted.shutdown().await;

        let _ = manager.close_session(&first.session_id).await;
    })
    .await
}

/// If revoking `TrustSession` fails during a session transition, admission stays fail-closed,
/// `WorkspaceSnapshot` remains untrusted with baseline config, and a SECOND transition STILL runs
/// revocation (never skipping it because `session_only` / `pending_session_trust_revoke` stayed set).
#[tokio::test]
async fn trust_session_only_revocation_failure_remains_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        write(
            &env.cwd.join(".mare/init.lua"),
            "mare.config.project_only_marker = 'leak-check'\nmare.setup({ plugins = { session = { branch_summary = false } } })\n",
        );
        write(
            &env.cwd.join(".mare/plugins/projplug.lua"),
            r#"
            return {
              name = "projplug",
              apply = function(ctx)
                mare.g.projplug_observed_switches = mare.g.projplug_observed_switches or 0
                ctx:on("session/new", function()
                  mare.g.projplug_observed_switches = mare.g.projplug_observed_switches + 1
                end)
              end,
            }
            "#,
        );
        let manager = mare_host::HostManager::new();
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let rt = manager.create_session(opts).await.unwrap();
        let ws_id = mare_host::WorkspaceId::local(&env.cwd).unwrap();
        let baseline_cfg = manager.workspaces().get(&ws_id).unwrap().config_snapshot.clone();
        let h = rt.host.clone();
        expose(&h);
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        let (switch_ok, decision, lease_err): (bool, String, String) = eval(
            &h,
            r#"
            local s = assert(C.cordis:get("session"))
            local ok = pcall(function() s:switch({ kind = "new" }) end)
            local vote = C.cordis:get("policy"):decide("request/ready", {})
            return ok, tostring(vote and vote.decision), tostring(vote and vote.reason)
            "#,
        )
        .await;
        assert!(!switch_ok, "first session transition must fail when session plugin revocation fails");
        assert!(h.is_pending_session_trust_revoke(), "pending revocation flag must remain set after failed revoke");
        assert_eq!(decision, "deny", "request/ready must stay denied after revocation failure");
        assert!(
            lease_err.contains("revocation") || lease_err.contains("incomplete"),
            "host admission must remain closed after revocation failure: {lease_err}"
        );

        let snap = manager.workspaces().get(&ws_id).unwrap();
        assert!(!snap.trusted, "WorkspaceSnapshot must stay untrusted during pending failed revocation");
        assert_eq!(
            snap.config_snapshot, baseline_cfg,
            "WorkspaceSnapshot config_snapshot must not leak project-only config on failed revocation"
        );

        // Second transition attempt MUST NOT skip revocation or succeed!
        let (switch2_ok, decision2, switches2): (bool, String, i64) = eval(
            &h,
            r#"
            local s = assert(C.cordis:get("session"))
            local ok = pcall(function() s:switch({ kind = "new" }) end)
            local vote = C.cordis:get("policy"):decide("request/ready", {})
            return ok, tostring(vote and vote.decision), mare.g.projplug_observed_switches or 0
            "#,
        )
        .await;
        assert!(!switch2_ok, "second session transition must STILL attempt and fail revocation (not skip it)");
        assert!(h.is_pending_session_trust_revoke(), "pending revocation flag must still remain set");
        assert_eq!(decision2, "deny", "request/ready must stay denied after second failed revocation");
        assert_eq!(switches2, 0, "project plugin must never observe session/new when revocation fails");
        let _ = manager.close_session(&rt.session_id).await;
    })
    .await
}

/// When non-session plugin revocation fails after untrusted config was committed (`old == new` on
/// subsequent retries), `reload_retry` and `report.restored` MUST keep the barrier active across
/// both retry switches and `/reload` until restoration actually succeeds cleanly.
#[tokio::test]
async fn trust_session_only_retry_keeps_dirty_plugins_and_restored_components_blocked() {
    local(async {
        let env = TestEnv::new().await;
        write(
            &env.config_dir().join("plugins/myuser.lua"),
            r#"
            return {
              name = "myuser",
              apply = function()
                if mare.g.fail_myuser_restore then
                  error("user plugin restore blocked")
                end
              end,
            }
            "#,
        );
        write(&env.cwd.join(".mare/init.lua"), "mare.setup({ plugins = { compact = { enabled = false } } })\n");
        write(
            &env.cwd.join(".mare/plugins/myuser.lua"),
            r#"
            return {
              name = "myuser",
              override = true,
              apply = function(ctx)
                mare.g.proj_myuser_switches = mare.g.proj_myuser_switches or 0
                ctx:on("session/new", function()
                  mare.g.proj_myuser_switches = mare.g.proj_myuser_switches + 1
                end)
              end,
            }
            "#,
        );
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        exec(&h, "mare.g.fail_myuser_restore = true").await;
        let (ok1, dec1): (bool, String) = eval(
            &h,
            r#"
            local s = assert(C.cordis:get("session"))
            local ok = pcall(function() s:create({}) end)
            local vote = C.cordis:get("policy"):decide("request/ready", {})
            return ok, tostring(vote and vote.decision)
            "#,
        )
        .await;
        assert!(!ok1, "first revocation with failing restored plugin must fail");
        assert!(h.is_pending_session_trust_revoke());
        assert_eq!(dec1, "deny");

        // Now untrusted config is already current (old == new): second switch must STILL retry `myuser`!
        let (ok2, dec2, obs2): (bool, String, i64) = eval(
            &h,
            r#"
            local s = assert(C.cordis:get("session"))
            local ok = pcall(function() s:create({}) end)
            local vote = C.cordis:get("policy"):decide("request/ready", {})
            return ok, tostring(vote and vote.decision), mare.g.proj_myuser_switches or 0
            "#,
        )
        .await;
        assert!(!ok2, "second revocation retry must NOT clear barrier while myuser still fails/restores");
        assert!(h.is_pending_session_trust_revoke());
        assert_eq!(dec2, "deny");
        assert_eq!(obs2, 0);

        // `/reload` while myuser still fails and restores previous component must stay incomplete.
        h.reload_all().await.unwrap();
        assert!(h.is_reload_full_incomplete(), "reload_all must remain incomplete when a component restores");
        assert!(
            h.is_pending_session_trust_revoke(),
            "pending_session_trust_revoke must NOT clear while reload restored project component"
        );

        // Fixing user plugin allows `/reload` + session switch to recover cleanly.
        exec(&h, "mare.g.fail_myuser_restore = false").await;
        h.reload_all().await.unwrap();
        assert!(!h.is_reload_full_incomplete());
        assert!(!h.is_pending_session_trust_revoke());
        assert!(!h.is_project_trusted());
        exec(&h, "local s = assert(C.cordis:get('session')); s:create({})").await;
        assert_eq!(eval::<i64>(&h, "return mare.g.proj_myuser_switches or 0").await, 0);

        h.shutdown().await;
    })
    .await
}

/// Connected project stdio MCP servers from `.mcp.json` launched under `TrustSession` must
/// disconnect, unregister tools, and refuse dispatch across `/new`, `/resume`, and `/fork`.
#[tokio::test]
async fn trust_session_only_disconnects_project_mcp_server_on_new_resume_and_fork() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        let mcp_log = env.home.path().join("proj-session-mcp.log");
        let mcp_cfg = json!({ "log": mcp_log });
        std::fs::write(
            env.cwd.join(".mcp.json"),
            json!({
                "mcpServers": {
                    "proj": {
                        "command": fake_mcp_bin(),
                        "args": ["scripted", mcp_cfg.to_string()],
                        "tier": "direct"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let saved_session = env.write_session("mcp-resume.jsonl", &[msg("m1", None, "user", "hello")]);
        let mut opts = env
            .host()
            .user_plugins()
            .builtins(&["mcp"])
            .faux(json!([{ "text": "ok" }]))
            .args(json!({ "trust": "undecided", "approve_mcps": true }))
            .opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        global(&h, "SAVED_SESSION", saved_session.to_string_lossy().to_string());

        assert!(eval::<bool>(&h, "return T:get('mcp__proj__echo') == nil").await);
        assert!(!mcp_log.exists(), "project MCP server must not launch while untrusted");

        for action in ["new", "resume", "fork"] {
            if action == "fork" {
                h.prompt("hi", json!({})).await.unwrap();
            }
            h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
            wait_until(15, &format!("project MCP tool registered before {action}"), || {
                let hc = h.clone();
                async move { eval::<bool>(&hc, "return T:get('mcp__proj__echo') ~= nil").await }
            })
            .await;

            let switch_lua = match action {
                "new" => "local s = assert(C.cordis:get('session')); s:create({})",
                "resume" => "local s = assert(C.cordis:get('session')); s:load({ path = SAVED_SESSION })",
                "fork" => {
                    "local s = assert(C.cordis:get('session')); s:fork({ session_id = s:info().session_id, name = 'forked' })"
                }
                _ => unreachable!(),
            };
            exec(&h, switch_lua).await;
            assert!(!h.is_project_trusted(), "{action} must revoke TrustSession");

            let (tool_gone, connect_err, list_omits_tool, server_untrusted): (bool, String, bool, bool) = eval(
                &h,
                r#"
                local mcp = assert(C.cordis:get("mcp"))
                local ok_conn, err_conn = pcall(mcp.connect, mcp, "proj")
                local listed = mcp:list_tools({})
                local has_proj_tool = false
                for _, item in ipairs((listed and listed.items) or {}) do
                  if item.name == "mcp__proj__echo" then has_proj_tool = true end
                end
                local srvs = mcp:list_servers()
                local proj_untrusted = false
                for _, s in ipairs((srvs and srvs.data) or {}) do
                  if s.name == "proj" and s.state == "untrusted" then proj_untrusted = true end
                end
                return T:get("mcp__proj__echo") == nil,
                  ok_conn and "unexpectedly connected" or mare.json.encode(err_conn),
                  not has_proj_tool,
                  proj_untrusted
                "#,
            )
            .await;
            assert!(tool_gone, "project MCP tool must be unregistered after {action}");
            assert!(
                connect_err.contains("mcp.connect_failed") && connect_err.contains("trusted project"),
                "exported mcp:connect('proj') must fail with coded untrusted refusal after {action}: {connect_err}"
            );
            assert!(list_omits_tool, "mcp:list_tools must omit project tool after {action}");
            assert!(server_untrusted, "mcp:list_servers must report 'proj' as untrusted after {action}");
        }

        h.shutdown().await;
    })
    .await
}

/// An in-flight `connecting` project stdio MCP server started under `TrustSession` must be
/// cancelled and awaited BEFORE `/new` completes, and a subsequent grant starts fresh.
#[tokio::test]
async fn trust_session_only_aborts_inflight_mcp_start_on_switch() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        let mcp_log = env.home.path().join("inflight-mcp.log");
        let pid_log = env.home.path().join("inflight-mcp.pids");
        let gate_file = env.home.path().join("inflight-gate");
        let mcp_cfg = json!({ "log": mcp_log, "pid_log": pid_log, "list_gate": gate_file });
        std::fs::write(
            env.cwd.join(".mcp.json"),
            json!({
                "mcpServers": {
                    "proj": {
                        "command": fake_mcp_bin(),
                        "args": ["scripted", mcp_cfg.to_string()],
                        "tier": "direct"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut opts = env
            .host()
            .user_plugins()
            .builtins(&["mcp"])
            .faux(json!([{ "text": "ok" }]))
            .args(json!({ "trust": "undecided", "approve_mcps": true }))
            .opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);

        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        wait_until(15, "in-flight MCP child blocked at tools/list", || {
            let log = mcp_log.clone();
            async move { std::fs::read_to_string(&log).is_ok_and(|s| s.contains("tools/list")) }
        })
        .await;
        let pid1: i32 = std::fs::read_to_string(&pid_log).unwrap().lines().next().unwrap().trim().parse().unwrap();
        assert!(eval::<bool>(&h, "return T:get('mcp__proj__echo') == nil").await);

        // Revoke via /new WHILE child #1 is still blocked inside `tools/list`.
        exec(&h, "local s = assert(C.cordis:get('session')); s:create({})").await;
        assert!(!h.is_project_trusted(), "/new must complete only after revoking trust");
        assert!(
            eval::<bool>(&h, "return T:get('mcp__proj__echo') == nil").await,
            "cancelled in-flight MCP start must never register tools"
        );
        wait_until(10, "cancelled MCP stdio child exited", || async move {
            !std::path::Path::new(&format!("/proc/{pid1}")).exists()
        })
        .await;

        // Open the gate: cancelled start must not leak; a fresh TrustSession must spawn child #2 cleanly.
        std::fs::write(&gate_file, "go").unwrap();
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        wait_until(15, "fresh TrustSession MCP server connected cleanly", || {
            let hc = h.clone();
            async move { eval::<bool>(&hc, "return T:get('mcp__proj__echo') ~= nil").await }
        })
        .await;
        let pids: Vec<String> = std::fs::read_to_string(&pid_log)
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(pids.len(), 2, "must spawn a fresh process after cancelling in-flight start: {pids:?}");
        assert_ne!(pids[0], pids[1]);

        h.shutdown().await;
    })
    .await
}

/// Full `/reload` (`reload_all`) while reopening a persisted session under active or pending
/// `TrustSession` must complete without deadlocking on `reload_gate`, and reopen the session
/// with its ID, path, and conversation messages intact.
#[tokio::test]
async fn trust_session_only_full_reload_reopening_persisted_session_does_not_deadlock() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.reload_probe = 'active'\n");
        let saved_session = env.write_session("reload-persisted.jsonl", &[msg("m1", None, "user", "hello")]);
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        global(&h, "SAVED_SESSION", saved_session.to_string_lossy().to_string());
        exec(&h, "local s = assert(C.cordis:get('session')); s:load({ path = SAVED_SESSION })").await;
        let pre_id: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;

        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        // Full reload while session_only + persisted session is active (triggers kernel/reloaded -> load_file).
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), h.reload_all()).await;
        let reload_res = res.expect("reload_all must NOT deadlock against load_file revocation barrier");
        assert!(reload_res.is_ok(), "reload_all failed: {reload_res:?}");

        let (post_id, post_path, msgs_json): (String, String, String) = eval(
            &h,
            r#"
            local info = C.cordis:get("session"):info()
            local msgs = C.cordis:get("agent"):list_messages({})
            return tostring(info.session_id), tostring(info.path), mare.json.encode(msgs)
            "#,
        )
        .await;
        assert_eq!(post_id, pre_id, "reload_all must reopen persisted session with same id");
        assert_eq!(post_path, saved_session.to_string_lossy().as_ref(), "reopened session path must match");
        assert!(msgs_json.contains("hello"), "reopened session messages must be preserved: {msgs_json}");

        // Revoke active TrustSession cleanly first via /new, then configure a failing revocation,
        // answer TrustSession again, fail a switch to leave `pending_session_trust_revoke = true`,
        // repair `.mare/init.lua`, and verify `reload_all` completes revocation without deadlocking.
        exec(&h, "local s = assert(C.cordis:get('session')); s:create({})").await;
        assert!(!h.is_project_trusted());
        write(&env.cwd.join(".mare/init.lua"), "mare.setup({ plugins = { session = { branch_summary = false } } })\n");
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        let failed: bool =
            eval(&h, "local s = assert(C.cordis:get('session')); return pcall(function() s:create({}) end)").await;
        assert!(!failed);
        assert!(h.is_pending_session_trust_revoke());

        write(&env.cwd.join(".mare/init.lua"), "mare.config.reload_probe = 'fixed'\n");
        let res2 = tokio::time::timeout(std::time::Duration::from_secs(10), h.reload_all()).await;
        let reload2 = res2.expect("recovery reload_all must NOT deadlock");
        assert!(reload2.is_ok(), "recovery reload_all failed: {reload2:?}");
        assert!(!h.is_project_trusted(), "recovery reload_all must finish pending TrustSession revocation");
        assert!(!h.is_pending_session_trust_revoke(), "pending flag must clear once reload_all revokes cleanly");

        h.shutdown().await;
    })
    .await
}

/// A rejected switch attempt (e.g. session busy/held) must NOT revoke active `TrustSession`
/// or cancel the active session token.
#[tokio::test]
async fn trust_session_only_rejected_busy_switch_keeps_trust_and_active_request() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.project_only_marker = 'kept'\n");
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        let bound_call = h.bind_session_call().unwrap();
        let (ok, err_str): (bool, String) = eval(
            &h,
            r#"
            local s = assert(C.cordis:get("session"))
            local ok, err = pcall(function() s:create({}) end)
            return ok, tostring(err)
            "#,
        )
        .await;
        drop(bound_call);
        assert!(!ok, "bound session call must reject s:create");
        assert!(err_str.contains("busy"), "expected busy rejection, got: {err_str}");
        assert!(h.is_project_trusted(), "rejected switch must NOT revoke active TrustSession");
        assert!(!h.is_pending_session_trust_revoke(), "rejected switch must NOT mark revocation pending");
        assert!(
            !h.session_cancel_token().is_cancelled(),
            "rejected switch must NOT cancel current session cancel token"
        );
        assert_eq!(
            eval::<String>(&h, "return tostring(mare.config.project_only_marker)").await,
            "kept",
            "project-only config must remain loaded after rejected switch"
        );

        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn instructions_reject_escaping_symlinks_and_untrusted_symlinks() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("SECRET.md");
        write(&secret, "OUTSIDE-SECRET");

        // 1. Even in a trusted repo, an AGENTS.md symlink pointing outside the project root is rejected.
        let repo = env.cwd.join("trusted-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::os::unix::fs::symlink(&secret, repo.join("AGENTS.md")).unwrap();
        let mut b = env.host().no_session().args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = repo;
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt_trusted: String = eval(&h, "return (X:system_prompt())").await;
        assert!(!prompt_trusted.contains("OUTSIDE-SECRET"), "escaping symlink must be rejected");
        h.shutdown().await;

        // 2. In an untrusted repo, even an intra-repo AGENTS.md symlink is rejected.
        let untrusted_repo = env.cwd.join("untrusted-repo");
        std::fs::create_dir_all(untrusted_repo.join(".git")).unwrap();
        write(&untrusted_repo.join("inner.md"), "INTRA-SYMLINK-TEXT");
        std::os::unix::fs::symlink(untrusted_repo.join("inner.md"), untrusted_repo.join("AGENTS.md")).unwrap();
        let mut b2 = env.host().no_session().args(json!({ "no_session": true, "trust": "untrusted" }));
        b2.opts.cwd = untrusted_repo;
        b2.opts.project_trusted = false;
        let h2 = b2.build().await;
        global(&h2, "X", h2.service_table("context").unwrap());
        let prompt_untrusted: String = eval(&h2, "return (X:system_prompt())").await;
        assert!(!prompt_untrusted.contains("INTRA-SYMLINK-TEXT"), "untrusted symlink must be rejected");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn begin_switch_revokes_session_only_trust_before_publish() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().args(json!({ "trust": "session" })).project_trusted(true);
        b.opts.load_init = true;
        let h = b.build_exposed().await;
        h.workspace.set_session_only(true);
        assert!(h.workspace.is_trusted() && h.workspace.is_session_only());

        let (trusted_after_begin, still_session_only): (bool, bool) = eval(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local sw <close> = k:begin_session_switch({ session_id = "post-revoke-sid" })
            return mare.trusted == true, sw ~= nil
            "#,
        )
        .await;
        assert!(still_session_only);
        assert!(!trusted_after_begin, "begin_session_switch must revoke session-only trust before publish");
        assert!(!h.workspace.is_trusted(), "host workspace trust must be false after begin_session_switch");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn switch_during_reload_leaves_revocation_to_the_reload() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().args(json!({ "trust": "session" })).project_trusted(true);
        b.opts.load_init = true;
        let h = b.build_exposed().await;
        h.workspace.set_session_only(true);

        // During a real reload, session startup calls begin_session_switch while reload_active is
        // true: begin must not fail or double-lock reload_gate, and must defer revocation to the
        // reload itself.
        let (reloaded, trusted_after): (bool, bool) = {
            let res = h.reload_all().await;
            (res.is_ok(), h.workspace.is_trusted())
        };
        assert!(reloaded, "reload with session-only trust must succeed cleanly");
        assert!(!trusted_after, "reload must complete deferred session-only trust revocation");
        h.shutdown().await;
    })
    .await
}

/// Review #4a: `kernel:reload("session")` while `TrustSession` + a persisted session are active
/// must NOT self-deadlock on `reload_gate` (or leave `reload_full_incomplete = true`) and must
/// cleanly reopen the session and settle session-only trust revocation.
#[tokio::test]
async fn kernel_reload_session_under_trust_session_does_not_deadlock_and_revokes_cleanly() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.kernel_reload_probe = 'from-session-trust'\n");
        let saved_session = env.write_session("kernel-reload-persisted.jsonl", &[msg("m1", None, "user", "keep me")]);
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        global(&h, "SAVED_SESSION", saved_session.to_string_lossy().to_string());
        exec(&h, "local s = assert(C.cordis:get('session')); s:load({ path = SAVED_SESSION })").await;
        let pre_id: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;

        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        // Trigger Lua `kernel:reload("session")` with a 3s budget (< 4s shutdown_grace deadlock).
        let reload_future = eval::<(bool, String)>(
            &h,
            r#"
            local k = C.cordis:get("kernel")
            local ok, res = pcall(function() return k:reload("session") end)
            return ok, tostring(res)
            "#,
        );
        let (ok, detail) = tokio::time::timeout(std::time::Duration::from_secs(3), reload_future)
            .await
            .expect("kernel:reload('session') must not self-deadlock on reload_gate");
        assert!(ok, "kernel:reload('session') failed: {detail}");

        let (post_id, msgs_json): (String, String) = eval(
            &h,
            r#"
            local info = C.cordis:get("session"):info()
            local msgs = C.cordis:get("agent"):list_messages({})
            return tostring(info.session_id), mare.json.encode(msgs)
            "#,
        )
        .await;
        assert_eq!(post_id, pre_id, "persisted session id must survive kernel:reload('session')");
        assert!(msgs_json.contains("keep me"), "persisted session messages must survive: {msgs_json}");
        assert!(!h.is_project_trusted(), "deferred TrustSession revocation must settle after kernel:reload('session')");
        assert!(!h.is_pending_session_trust_revoke(), "pending revocation flag must be cleared");
        let prompt = h.prompt("after reload", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "host must not stay blocked by reload_full_incomplete: {prompt}");

        h.shutdown().await;
    })
    .await
}

/// Review #4b: `Host::reload_config()` must settle `pending_session_trust_revoke` inside its
/// reload barrier when a session switch runs during `reload_config()` while `TrustSession` is active.
#[tokio::test]
async fn reload_config_settles_deferred_session_only_trust_revocation() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.cfg_reload_probe = 'from-session-trust'\n");
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        write(
            &pdir.join("switch-on-cfg-reload.lua"),
            r#"return {
                name = "switch-on-cfg-reload",
                inject = { "kernel" },
                config_schema = { trigger = { type = "string", default = "init", desc = "trigger" } },
                apply = function(ctx, cfg)
                    if cfg.trigger == "switch" then
                        local sw <close> = ctx.kernel:begin_session_switch({ session_id = "cfg-reload-switch-sid" })
                        sw:commit("cfg-reload-switch-sid")
                    end
                end,
            }"#,
        );
        let mut opts =
            env.host().user_plugins().faux(json!([{ "text": "ok" }])).args(json!({ "trust": "undecided" })).opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();

        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());

        // Trigger a config reload that re-applies `switch-on-cfg-reload`, invoking
        // `begin_session_switch` while `reload_active = true` (deferring revocation).
        write(&env.config_dir().join("init.lua"), "mare.plugin('switch-on-cfg-reload', { trigger = 'switch' })\n");
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), h.reload_config())
            .await
            .expect("reload_config must not deadlock");
        assert!(res.is_ok(), "reload_config failed: {res:?}");
        assert!(!h.is_project_trusted(), "reload_config must settle deferred TrustSession revocation");
        assert!(!h.is_pending_session_trust_revoke(), "pending revocation flag must be cleared after reload_config");

        h.shutdown().await;
    })
    .await
}

/// On a single-runtime host, `session.create`, `session.load`, and `session.fork` routed through
/// the host-provided orchestrator must revoke active `TrustSession` synchronously before
/// `session/changed` or `session/loaded` fires.
#[tokio::test]
async fn trust_session_only_revokes_through_orchestrator_on_create_load_and_fork() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join(".mare/init.lua"), "mare.config.orch_trust_marker = true\n");
        let saved = env.write_session("saved-for-trust.jsonl", &[msg("m1", None, "user", "saved")]);
        let mut opts = env
            .host()
            .user_plugins()
            .faux(json!([{ "text": "ok" }, { "text": "ok2" }]))
            .args(json!({ "trust": "undecided" }))
            .opts;
        opts.load_init = true;
        opts.project_trusted = false;
        let h = mare_host::Host::new(opts).await.unwrap();
        expose(&h);
        global(&h, "SAVED_PATH", saved.to_string_lossy().to_string());

        // 1) session.create through orchestrator revokes TrustSession.
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());
        exec(&h, "assert(S:create({}))").await;
        assert!(!h.is_project_trusted(), "orchestrated session.create must revoke TrustSession");

        // 2) session.load through orchestrator revokes TrustSession.
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());
        exec(&h, "assert(S:load({ path = SAVED_PATH }))").await;
        assert!(!h.is_project_trusted(), "orchestrated session.load must revoke TrustSession");

        // 3) session.fork through orchestrator revokes TrustSession.
        h.prompt("persist-turn", json!({})).await.unwrap();
        h.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
        assert!(h.is_project_trusted());
        exec(&h, "assert(S:fork({ session_id = S:info().session_id, name = 'orch-fork' }))").await;
        assert!(!h.is_project_trusted(), "orchestrated session.fork must revoke TrustSession");

        h.shutdown().await;
    })
    .await
}

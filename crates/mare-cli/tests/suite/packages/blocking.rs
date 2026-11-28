//! When plugin, package or provider failures block requests, and the readiness order.

use super::*;

/// Write `~/.config/mare/plugins/<name>/init.lua`.
fn user_plugin(env: &TestEnv, name: &str, code: &str) -> std::path::PathBuf {
    let plugin = env.config_dir().join("plugins").join(name).join("init.lua");
    std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::fs::write(&plugin, code).unwrap();
    plugin
}

/// A broken optional plugin must remain diagnosable when the management panel is disabled.
#[tokio::test]
async fn disabled_plugin_panel_keeps_optional_failure_diagnosable() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "broken_user",
            "return { name = 'broken_user', apply = function() error('diagnostic sentinel') end }",
        );
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('plugins', { enabled = false })",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert_eq!(blocked_reason(&h).await, None);
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null());
        let warnings = h.plugin_warnings();
        let warning = warnings.iter().find(|w| w.contains("broken_user")).expect("optional plugin warning");
        assert!(warning.contains("mare doctor"), "warning needs a working CLI diagnostic: {warning}");
        assert!(h.startup_notices().iter().any(|(notice, _)| notice == warning));
        h.kernel.add_problem("package extra diagnostic problem".to_string());
        assert!(
            h.startup_notices().iter().any(
                |(notice, _)| notice.contains("package extra diagnostic problem") && notice.contains("mare doctor")
            ),
            "{:?}",
            h.startup_notices()
        );
        let doctor =
            mare_cmd().args(["doctor", "--json"]).env("MARE_HOME", env.home.path()).current_dir(&env.cwd).run_output();
        let output = String::from_utf8_lossy(&doctor.stdout);
        assert!(
            output.contains("broken_user") && output.contains("diagnostic sentinel"),
            "doctor: {output}\nstderr: {}",
            String::from_utf8_lossy(&doctor.stderr)
        );
        h.shutdown().await;
    })
    .await
}

/// an optional plugin that fails to start doesn't block requests and doesn't stop
/// the plugins around it (lazy.nvim `Util.try`) — it is one short warning (notice, `/status`,
/// the status line, headless stderr; the traceback in the problems) until a full /reload.
#[tokio::test]
async fn a_failed_optional_user_plugin_warns_and_requests_continue() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(&env, "a_fine", "return { name = 'a_fine', apply = function() mare.g.a_fine = true end }");
        let plugin = user_plugin(
            &env,
            "broken_user",
            "return { name = 'broken_user', apply = function() error('user plugin sentinel') end }",
        );
        user_plugin(&env, "z_fine", "return { name = 'z_fine', apply = function() mare.g.z_fine = true end }");
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
        assert_eq!(blocked_reason(&h).await, None, "an optional plugin doesn't block");
        let warnings = h.plugin_warnings();
        assert!(
            warnings.len() == 1 && warnings[0].contains("broken_user") && warnings[0].contains("user plugin sentinel"),
            "{warnings:?}"
        );
        assert!(warnings[0].starts_with("plugin `broken_user` setup failed ("), "{warnings:?}");
        assert!(warnings[0].contains("/reload") && warnings[0].contains("mare --clean"), "{warnings:?}");
        assert!(!warnings[0].contains("stack traceback"), "the traceback isn't in the notice: {warnings:?}");
        assert_eq!(h.startup_notices(), vec![(warnings[0].clone(), "warning")], "shown once");
        let problems = h.kernel.problems().join("\n");
        assert!(problems.contains("user plugin sentinel") && problems.contains("stack traceback"), "{problems}");
        assert!(eval::<bool>(&h, "return mare.g.a_fine and mare.g.z_fine").await, "the others load");
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        let failed: (String, bool) = eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
        assert_eq!(failed, ("broken_user".to_string(), false));
        std::fs::write(&plugin, "return { name = 'broken_user', apply = function() end }").unwrap();
        let out = h.reload_all().await.unwrap();
        assert_eq!(out["notices"], json!([]), "{out}");
        assert!(h.plugin_warnings().is_empty(), "a full reload clears the warning");
        assert_eq!(eval::<usize>(&h, "return #mare.plugins.failures()").await, 0);
        assert!(!h.kernel.problems().join("\n").contains("sentinel"), "{:?}", h.kernel.problems());
        h.shutdown().await;
    })
    .await
}

/// `mare.plugins` tells any plugin a plugin's state — failed, turned off, running,
/// unknown — and lists the failures; the earlier private names answer the same.
#[tokio::test]
async fn mare_plugins_reports_failed_disabled_running_and_unknown_plugins() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(&env, "broken_user", "return { name = 'broken_user', apply = function() error('probe') end }");
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('jobs', { enabled = false })",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        let row = |name: &str| {
            format!(
                "local s = mare.plugins.status('{name}')
                 return table.concat({{ tostring(s.defined), tostring(s.active), tostring(s.disabled),
                   tostring(s.origin), table.concat(s.provide, '+'), tostring(s.failure and s.failure.phase),
                   tostring(s.failure and s.failure.blocks) }}, ',')"
            )
        };
        assert_eq!(eval::<String>(&h, &row("broken_user")).await, "true,false,false,user,,setup,false");
        assert_eq!(eval::<String>(&h, &row("jobs")).await, "false,false,true,nil,,nil,nil");
        assert_eq!(eval::<String>(&h, &row("session")).await, "true,true,false,builtin,session,nil,nil");
        assert_eq!(eval::<String>(&h, &row("no_such_plugin")).await, "false,false,false,nil,,nil,nil");
        let listed: (bool, bool) = eval(
            &h,
            "local seen = {}
             for _, s in ipairs(mare.plugins.list()) do seen[s.name] = s end
             return seen.broken_user ~= nil and not seen.broken_user.active, seen.jobs == nil",
        )
        .await;
        assert_eq!(listed, (true, true), "every defined plugin, a turned-off one is not defined");
        let failures: String = eval(
            &h,
            "local f = mare.plugins.failures()
             return #f .. ',' .. f[1].plugin .. ',' .. f[1].kind",
        )
        .await;
        assert_eq!(failures, "1,broken_user,plugin");
        let disabled: (bool, bool) =
            eval(&h, "return mare.plugins.status('jobs').disabled, mare.plugins.status('session').disabled").await;
        assert_eq!(disabled, (true, false));
        h.shutdown().await;
    })
    .await
}

/// an init.lua that fails doesn't stop the interactive startup (WezTerm, Neovim, pi),
/// so slash commands and /reload work — but every model request is refused until it is fixed and
/// a reload succeeds (Codex's caution: the half-read config may lack the user's limits).
#[tokio::test]
async fn a_failing_init_lua_starts_but_blocks_requests_until_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        for (code, line) in [
            ("mare.setup { model = 'faux/echo' }\nlocal x = nil\nx.y = 1\n", Some("init.lua:3")),
            ("mare.setup { model = 'faux/echo'\n", None),
        ] {
            std::fs::write(&init, code).unwrap();
            let mut opts = env.host().user_config(false).no_session().opts;
            opts.config = json!({ "model": "faux/echo" });
            let h = mare_host::Host::new(opts).await.expect("a broken init.lua must not stop startup");
            let reason = blocked_reason(&h).await.expect("every request is blocked");
            assert!(
                reason.starts_with("init.lua has an error (")
                    && reason.contains("/reload")
                    && reason.contains("mare --clean"),
                "{reason}"
            );
            if let Some(line) = line {
                assert!(reason.contains(line) && reason.contains("attempt to index"), "{reason}");
            }
            assert!(h.plugin_warnings().is_empty(), "{:?}", h.plugin_warnings());
            assert_eq!(h.startup_notices(), vec![(reason.clone(), "error")], "shown once");
            assert!(h.config_error().is_some_and(|e| e.starts_with("init.lua has an error")));
            let prompt = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(prompt["not_sent"], true, "{prompt}");
            assert!(prompt["error"]["message"].as_str().unwrap().starts_with("init.lua has an error"), "{prompt}");
            assert!(h.kernel.problems().iter().any(|p| p.starts_with("Error detected while processing")));
            std::fs::write(&init, "mare.setup { model = 'faux/echo' }\n").unwrap();
            h.reload_all().await.unwrap();
            assert_eq!(blocked_reason(&h).await, None, "a successful reload lifts the block");
            assert!(h.config_error().is_none());
            h.shutdown().await;
        }
    })
    .await
}

/// /reload follows the startup rule — a plugin it can't bring back blocks requests
/// only when it is essential or required; any other is a warning.
#[tokio::test]
async fn a_plugin_failing_on_reload_blocks_only_when_essential() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        // fails once `mare.g.break_<name>` is set — the new and the previous definition alike. A full
        // reload restarts only plugins whose definition changed (the cordis tree's row
        // fingerprint), so each reload below rewrites the file with a new revision comment.
        let flaky = |name: &str, required: bool, rev: u32| {
            format!("-- rev {rev}\nreturn {{ name = '{name}', required = {required}, apply = function() if mare.g.break_{name} then error('reload sentinel') end end }}")
        };
        let optional = user_plugin(&env, "optional_one", &flaky("optional_one", false, 0));
        let must = user_plugin(&env, "must_have", &flaky("must_have", true, 0));
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert_eq!(blocked_reason(&h).await, None);
        exec(&h, "mare.g.break_optional_one = true").await;
        std::fs::write(&optional, flaky("optional_one", false, 1)).unwrap();
        let out = h.reload_all().await.unwrap();
        assert!(!crate::common::plugin_active(&h.kernel, "optional_one"));
        assert!(out["notices"].to_string().contains("plugin `optional_one` setup failed"), "{out}");
        assert_eq!(blocked_reason(&h).await, None, "a non-essential plugin only warns");
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null());
        assert!(h.plugin_warnings().iter().any(|w| w.contains("optional_one")), "{:?}", h.plugin_warnings());
        exec(&h, "mare.g.break_must_have = true").await;
        std::fs::write(&must, flaky("must_have", true, 1)).unwrap();
        h.reload_all().await.unwrap();
        let reason = blocked_reason(&h).await.expect("a required plugin blocks");
        assert!(reason.starts_with("plugins are not ready:") && reason.contains("must_have"), "{reason}");
        assert!(!reason.contains("optional_one"), "{reason}");
        exec(&h, "mare.g.break_must_have = nil; mare.g.break_optional_one = nil").await;
        std::fs::write(&optional, flaky("optional_one", false, 2)).unwrap();
        std::fs::write(&must, flaky("must_have", true, 2)).unwrap();
        h.reload_all().await.unwrap();
        assert_eq!(blocked_reason(&h).await, None);
        assert!(h.plugin_warnings().is_empty(), "{:?}", h.plugin_warnings());
        h.shutdown().await;
    })
    .await
}

/// A plugin declaring `required = true`, and one responsible for an essential service, block
/// every request when they fail, until a full /reload succeeds. Essential is decided by service
/// the services a plugin declares, or those of the definition it replaces — an
/// override that doesn't declare them, or a same-name file whose definition can't be read.
#[tokio::test]
async fn required_and_essential_plugin_failures_block_requests_until_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        for (name, code, phase) in [
            (
                "must_have",
                "return { name = 'must_have', required = true, apply = function() error('required sentinel') end }",
                "failed to start",
            ),
            (
                "permissions",
                "return { name = 'permissions', override = true, provide = { 'permissions' }, apply = function() error('policy sentinel') end }",
                "failed to start",
            ),
            (
                "session",
                "return { name = 'session', override = true, apply = function() error('store sentinel') end }",
                "failed to start",
            ),
            ("tools", "return { name = 'tools', override = true,", "definitions failed"),
        ] {
            let plugin = user_plugin(&env, name, code);
            let opts = env.host().user_config(false).no_session().opts;
            let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
            let reason = blocked_reason(&h).await.unwrap_or_else(|| panic!("{name} must block"));
            assert!(reason.contains(name) && reason.contains(phase), "{reason}");
            assert!(h.plugin_warnings().is_empty(), "a blocking failure is not a mere warning");
            let prompt = h.prompt("hello", json!({})).await.unwrap();
            assert!(prompt.to_string().contains("startup is incomplete"), "{prompt}");
            assert_eq!(prompt["not_sent"], true, "nothing went to the model: {prompt}");
            assert_eq!(h.startup_notices().len(), 1, "one notice: {:?}", h.startup_notices());
            std::fs::remove_dir_all(plugin.parent().unwrap()).unwrap();
            h.reload_all().await.unwrap();
            assert_eq!(blocked_reason(&h).await, None, "{name}: a full reload lifts the block");
            h.shutdown().await;
        }
    })
    .await
}

/// Name slots: closing a core built-in's slot is a deliberate disable.
/// No definition under the name loads — not the built-in, not a user replacement (which would
/// need `override = true` anyway) — so nothing fails and nothing is blocked; a failing
/// replacement that does load blocks (the `override = true` cases above).
#[tokio::test]
async fn closing_a_core_builtins_slot_loads_no_source_and_does_not_block() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "session",
            "return { name = 'session', override = true, apply = function() error('slot sentinel') end }",
        );
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('session', { enabled = false })",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert_eq!(blocked_reason(&h).await, None, "a closed slot is a deliberate disable");
        assert!(h.plugin_warnings().iter().all(|w| !w.contains("slot sentinel")), "{:?}", h.plugin_warnings());
        assert!(!h.kernel.plugin_active("session"));
        h.shutdown().await;
    })
    .await
}

/// an `override = true` replacement of `permissions` that runs but provides no
/// `permissions` service is exactly an explicit `mare.plugin("permissions", { enabled = false })`
/// — the same "permissions
/// service is off" notice (statusline.lua's `not ctx.permissions` branch, which also draws the
/// PERMISSIONS OFF capsule; doctor keys on the same missing service) and the same tool behaviour
/// (no approvals: a write outside the workspace goes through, where the built-in would ask and
/// headless deny). If it fails instead, requests are blocked: never a silent run without the
/// policy it was to provide.
#[test]
fn a_permissions_slot_holder_is_an_explicit_disable_when_it_runs_and_blocks_when_it_fails() {
    let home = tempfile::tempdir().unwrap();
    let (config, work) = (home.path().join("config"), home.path().join("work"));
    let holder = config.join("plugins/permissions/init.lua");
    std::fs::create_dir_all(holder.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(config.join("init.lua"), "mare.plugin('permissions', { enabled = false })\n").unwrap();
    let outside = home.path().join("outside.txt");
    let script = home.path().join("faux.json");
    let call = json!({ "name": "write", "arguments": { "path": outside, "content": "x" } });
    std::fs::write(&script, json!([{ "tool_calls": [call] }, { "text": "done" }]).to_string()).unwrap();
    // (exit code, stderr, whether the write outside the workspace happened)
    let run = || {
        let _ = std::fs::remove_file(&outside);
        let out = mare_cmd()
            .args(["-p", "go", "--model", "faux/echo", "--no-session"])
            .current_dir(&work)
            .env("MARE_HOME", home.path())
            .env("HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .stdin(std::process::Stdio::null())
            .run_output();
        (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned(), outside.exists())
    };
    let disabled = run();
    assert!(disabled.1.contains("Built-in permissions service is off") && disabled.2, "{disabled:?}");
    // the replacement needs an open slot: the closed one loads no source
    std::fs::write(config.join("init.lua"), "").unwrap();
    std::fs::write(&holder, "return { name = 'permissions', override = true, apply = function() end }").unwrap();
    assert_eq!(run(), disabled, "a replacement providing nothing is the explicit disable");
    std::fs::write(
        &holder,
        "return { name = 'permissions', override = true, apply = function() error('perm sentinel') end }",
    )
    .unwrap();
    let (code, err, wrote) = run();
    assert!(code != Some(0) && !wrote, "{err}");
    assert!(
        err.contains("plugin `permissions` setup failed")
            && err.contains("perm sentinel")
            && err.contains("essential service `permissions` has no provider"),
        "{err}"
    );
}

/// A failed provider plugin blocks only requests with the selected model of that provider:
/// another model works; switching to one of its models blocks with the reason.
#[tokio::test]
async fn a_failed_provider_plugin_blocks_only_its_selected_models() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "provider-other",
            "return { name = 'provider-other', providers = { 'other' }, inject = { 'llm' }, apply = function() error('provider sentinel') end }",
        );
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
        assert_eq!(blocked_reason(&h).await, None, "the selected model's provider works");
        assert!(h.plugin_warnings().iter().any(|w| w.contains("provider-other")), "{:?}", h.plugin_warnings());
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null());
        global(&h, "L", h.service_table("llm").unwrap());
        let selected: String = eval(&h, "return L:set_model('other/big', { save = false }).ref").await;
        assert_eq!(selected, "other/big", "the model stays selectable, never a silent fallback");
        let reason = blocked_reason(&h).await.expect("its provider failed");
        assert!(
            reason.contains("other/big") && reason.contains("provider-other") && reason.contains("provider sentinel"),
            "{reason}"
        );
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt.to_string().contains("provider other is unavailable"), "{prompt}");
        exec(&h, "L:set_model('faux/echo', { save = false })").await;
        assert_eq!(blocked_reason(&h).await, None, "back on a working provider");
        h.shutdown().await;
    })
    .await
}

/// Attribution uses only effective `providers` and `wires` declarations: when a multi-provider
/// plugin fails, only the provider names it declares (plus config-layer providers whose wire it
/// declares) are blamed on it — another failed plugin is not, and a misspelled provider is an
/// unknown model, not an unavailable one.
#[tokio::test]
async fn a_failed_multi_provider_plugin_is_blamed_only_for_the_providers_it_declares() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "provider-openai-compat",
            "return { name = 'provider-openai-compat', override = true, inject = { 'llm' },\
             wires = { 'openai-chat' }, providers = { 'deepseek', 'kimi', 'openrouter', 'ollama' },\
             apply = function() error('compat sentinel') end }",
        );
        user_plugin(
            &env,
            "provider-other",
            "return { name = 'provider-other', providers = { 'other' }, inject = { 'llm' }, apply = function() error('other sentinel') end }",
        );
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo',\n\
               providers = { acct2 = { base_url = 'http://localhost:1/v1' } } }",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
        global(&h, "L", h.service_table("llm").unwrap());
        for declared in ["deepseek/x", "acct2/x"] {
            exec(&h, &format!("L:set_model('{declared}', {{ save = false }})")).await;
            let reason = blocked_reason(&h).await.unwrap_or_else(|| panic!("{declared} must be unavailable"));
            assert!(reason.contains("compat sentinel"), "{declared}: {reason}");
            assert!(!reason.contains("other sentinel"), "{declared}: provider-other is not blamed: {reason}");
        }
        // its own plugin is still exact
        exec(&h, "L:set_model('other/x', { save = false })").await;
        let reason = blocked_reason(&h).await.expect("provider-other failed");
        assert!(reason.contains("other sentinel") && !reason.contains("compat sentinel"), "{reason}");
        // a misspelled provider is unknown, whoever failed
        let err: String = eval(&h, "local ok, e = pcall(L.set_model, L, 'deepseeek/x', { save = false }); return tostring(e)").await;
        assert!(err.contains("unknown model: deepseeek/x"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// An override replacement that declares neither `wires` nor `providers` must NOT be blamed
/// for the built-in's old providers or wires, and `"provider-" .. name` must never be guessed.
#[tokio::test]
async fn undeclared_replacement_plugin_is_not_blamed() {
    local(async {
        let env = TestEnv::new().await;
        user_plugin(
            &env,
            "provider-openai-compat",
            "return { name = 'provider-openai-compat', override = true, inject = { 'llm' },\
             apply = function() error('undeclared compat sentinel') end }",
        );
        user_plugin(
            &env,
            "provider-ghost",
            "return { name = 'provider-ghost', inject = { 'llm' }, apply = function() error('ghost sentinel') end }",
        );
        user_plugin(
            &env,
            "wire-readonly",
            "return { name = 'wire-readonly', inject = { 'llm' }, wires = { 'readonly-wire' },\
             apply = function(ctx)\
               ctx.llm:register_wire({\
                 name = 'readonly-wire', route = 'readonly', path = '/ro', auth = { 'bearer' },\
                 encode = function() return { method = 'POST', url = 'http://localhost:1/ro', body = {} } end,\
                 decoder = function() return { event = function() end, finish = function() return {} end } end,\
               })\
             end }",
        );
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo',\n\
               providers = {\n\
                 acct2 = { base_url = 'http://localhost:1/v1' },\n\
                 ant_cfg = { base_url = 'http://localhost:2/v1', wire_api = 'anthropic', no_key = true },\n\
                 ro_cfg = { base_url = 'http://localhost:3/v1', wire = 'readonly-wire' },\n\
               } }\n\
             mare.plugin('provider-google', { enabled = false })",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("startup must not refuse a failed plugin");
        global(&h, "L", h.service_table("llm").unwrap());

        // Built-in `deepseek` is no longer declared by the effective `provider-openai-compat` override.
        let ds_err: String =
            eval(&h, "local ok, e = pcall(L.set_model, L, 'deepseek/x', { save = false }); return tostring(e)").await;
        assert!(ds_err.contains("unknown model: deepseek/x"), "undeclared replacement must not be blamed: {ds_err}");

        // `provider-ghost` omitted `providers = { 'ghost' }` — never guess `"provider-" .. name`.
        let ghost_err: String =
            eval(&h, "local ok, e = pcall(L.set_model, L, 'ghost/x', { save = false }); return tostring(e)").await;
        assert!(ghost_err.contains("unknown model: ghost/x"), "must not guess provider-ghost: {ghost_err}");

        // Config entry on unknown wire `openai-chat` reports unknown wire, NOT `undeclared compat sentinel`.
        exec(&h, "L:set_model('acct2/x', { save = false })").await;
        let acct2_reason = blocked_reason(&h).await.expect("acct2 must be unresolved");
        assert!(
            acct2_reason.contains("unknown wire openai-chat") && !acct2_reason.contains("undeclared compat sentinel"),
            "{acct2_reason}"
        );

        // Config entry on `anthropic-messages` now resolves cleanly via its data-tier factory.
        exec(&h, "L:set_model('ant_cfg/x', { save = false })").await;
        let ant_resolved: bool = eval(&h, "return L:resolve('ant_cfg/x') ~= nil").await;
        assert!(ant_resolved, "ant_cfg/x must resolve on anthropic-messages");
        assert_eq!(blocked_reason(&h).await, None, "ant_cfg with no_key=true must not be blocked");

        // Config entry on a registered wire without a `provider` factory still reports explicit refusal.
        exec(&h, "L:set_model('ro_cfg/x', { save = false })").await;
        let ro_reason = blocked_reason(&h).await.expect("ro_cfg must be unresolved");
        assert!(ro_reason.contains("wire readonly-wire does not accept configured endpoints"), "{ro_reason}");

        // Disabled built-in provider plugin reports disabled with plugin name.
        exec(&h, "L:set_model('google/gemini-2.5-pro', { save = false })").await;
        let google_reason = blocked_reason(&h).await.expect("google is disabled");
        assert!(
            google_reason.contains("provider google is disabled") && google_reason.contains("provider-google"),
            "{google_reason}"
        );

        h.shutdown().await;
    })
    .await
}

/// Disabled wire attribution: a config-layer provider whose wire plugin is disabled reports
/// the disabled wire and its declaring plugin.
#[tokio::test]
async fn disabled_wire_plugin_is_attributed_to_config_providers() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo',\n\
               providers = { lab = { base_url = 'http://localhost:9/v1', wire_api = 'chat' } } }\n\
             mare.plugin('provider-openai-compat', { enabled = false })",
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("startup must succeed");
        global(&h, "L", h.service_table("llm").unwrap());

        exec(&h, "L:set_model('lab/m1', { save = false })").await;
        let reason = blocked_reason(&h).await.expect("lab must be unavailable while wire plugin is disabled");
        assert!(
            reason.contains("wire openai-chat")
                && reason.contains("provider-openai-compat")
                && reason.contains("disabled"),
            "{reason}"
        );
        h.shutdown().await;
    })
    .await
}

/// The readiness contract runs in one order whatever the load order: the
/// host, then llm (model, credentials), then listeners without a priority — also after the llm
/// plugin is reloaded (its listeners are registered again, after the user's). A prompt the model
/// can't take is "not sent", before anything goes to a provider.
#[tokio::test]
async fn readiness_refuses_in_one_order_and_before_anything_is_sent() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "answered" }])).no_session().build().await;
        expose(&h);
        exec(&h, "C.cordis:get('policy'):voter('request/ready', function() if mare.g.veto then return { decision = 'deny', reason = 'user veto' } end end)").await;
        let pick = "L:set_model('anthropic/claude-sonnet-5-5', { save = false }); mare.g.veto = true";
        let missing = "model anthropic/claude-sonnet-5-5: Anthropic is not configured: run `mare login anthropic`";
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, pick).await;
        let reason = blocked_reason(&h).await.unwrap();
        assert!(reason.starts_with(missing), "llm's band runs before a listener without a priority: {reason}");
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        assert!(prompt["error"]["message"].as_str().unwrap().starts_with(missing), "{prompt}");
        h.kernel.reload(Some("llm")).await.unwrap();
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, pick).await;
        let reason = blocked_reason(&h).await.unwrap();
        assert!(reason.starts_with(missing), "the same order after a reload: {reason}");
        exec(&h, "L:set_model('faux/echo', { save = false })").await;
        assert_eq!(blocked_reason(&h).await.as_deref(), Some("user veto"));
        exec(&h, "mare.g.veto = nil").await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        h.shutdown().await;
    })
    .await
}

/// The host's vote comes first: with a config error and a model without credentials, the reason
/// is the config error; once it is fixed, the model's.
#[tokio::test]
async fn readiness_reports_the_host_before_the_model() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'anthropic/claude-sonnet-5-5' }\nerror('init sentinel')\n")
            .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        let reason = blocked_reason(&h).await.unwrap();
        assert!(reason.starts_with("init.lua has an error") && reason.contains("init sentinel"), "{reason}");
        // llm's voter registers again on its own reload (a native prepend): still after the host's
        h.kernel.reload(Some("llm")).await.unwrap();
        let reason = blocked_reason(&h).await.unwrap();
        assert!(reason.starts_with("init.lua has an error"), "the host before the model after llm reloads: {reason}");
        std::fs::write(&init, "mare.setup { model = 'anthropic/claude-sonnet-5-5' }\n").unwrap();
        h.reload_all().await.unwrap();
        let reason = blocked_reason(&h).await.unwrap();
        assert!(reason.contains("Anthropic is not configured"), "then the model: {reason}");
        h.shutdown().await;
    })
    .await
}

/// A replacement `llm` service owns its readiness: a user plugin named `llm` with `override = true`
/// replaces the built-in one, so the built-in's readiness voter does not
/// run (it would refuse with its own model, providers and credentials); the replacement's voter
/// decides.
#[tokio::test]
async fn a_replaced_llm_service_is_judged_by_the_replacement_only() {
    local(async {
        let env = TestEnv::new().await;
        // the replacement implements the built-in contract's methods (stubs; read from the contract)
        let probe = env.host().no_session().build().await;
        let contract = probe.kernel.declared_contract("llm").unwrap();
        let mut reg_methods = Vec::new();
        let mut other_methods = Vec::new();
        for (name, spec) in contract.methods {
            if spec.registration {
                reg_methods.push(format!("{name:?}"));
            } else {
                other_methods.push(format!("{name:?}"));
            }
        }
        probe.shutdown().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("llm.lua"),
            format!(
                r#"return {{ name = "llm", override = true, provide = {{ "llm" }}, inject = {{ "policy" }},
  apply = function(ctx)
    local impl = {{}}
    for _, m in ipairs({{ {} }}) do impl[m] = function() end end
    for _, m in ipairs({{ {} }}) do impl[m] = function() return function() end end end
    ctx.policy:voter("request/ready", function()
      return {{ decision = "deny", reason = "the replacement llm refuses" }}
    end, {{ prepend = true }})
    require("mare_lib").service(ctx, "llm", impl)
  end }}"#,
                other_methods.join(", "),
                reg_methods.join(", "),
            ),
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let problems = h.kernel.problems();
        let reason = blocked_reason(&h).await;
        h.shutdown().await;
        assert_eq!(reason.as_deref(), Some("the replacement llm refuses"), "{problems:?}");
    })
    .await
}

/// A system prompt that can't be built (a section raising) is "not sent" with the reason, not a
/// failed turn after the prompt was taken; once it builds, the prompt goes out.
#[tokio::test]
async fn a_system_prompt_that_cannot_be_built_is_not_sent() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "answered" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            "C.cordis:get('context'):section({ name = 'boom', text = function() if mare.g.boom then error('section sentinel') end end })
             mare.g.boom = true",
        )
        .await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        let error = prompt["error"]["message"].as_str().unwrap();
        assert!(error.starts_with("system prompt could not be built:") && error.contains("section sentinel"), "{error}");
        exec(&h, "mare.g.boom = nil").await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        h.shutdown().await;
    })
    .await
}

/// An in-flight `mare.plugins.fetch_catalog()` (such as onboarding's background catalog fetch) runs
/// inside the host's blocking scope so `Host::shutdown()` cancels git, waits for the git process
/// group and blocking worker to finish unwinding out of `<MARE_HOME>/data`, and never leaves
/// concurrent writers behind when `TestEnv` removes `MARE_HOME`.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_drains_inflight_fetch_catalog_before_home_cleanup() {
    use std::os::unix::fs::PermissionsExt;

    local(async {
        let mut env = TestEnv::new().await;
        let started = env.cwd.join("ssh_started");
        let pid_file = env.cwd.join("ssh.pid");
        let churn = env.home.path().join("data/packages/.catalogs/churn.tmp");
        let fake_ssh = env.cwd.join("fake-ssh.sh");
        std::fs::write(
            &fake_ssh,
            format!(
                "#!/bin/sh\nmkdir -p '{}'\necho $$ > '{}'\ntouch '{}'\nwhile true; do\n  echo x > '{}'\n  sleep 0.01\ndone\n",
                churn.parent().unwrap().display(),
                pid_file.display(),
                started.display(),
                churn.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gitconfig = env.cwd.join("gitconfig");
        std::fs::write(
            &gitconfig,
            "[url \"ssh://git@mare-test.invalid/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        env.set_var("GIT_CONFIG_GLOBAL", &gitconfig);
        env.set_var("GIT_CONFIG_NOSYSTEM", "1");
        env.set_var("GIT_SSH_COMMAND", &fake_ssh);

        let h = env.host().builtins(&["plugins"]).no_session().build_exposed().await;
        exec(&h, "C.cordis:spawn(function() mare.plugins.fetch_catalog() end)").await;
        wait_until(WAIT_SECS, "fake ssh started by fetch_catalog", || async { started.exists() }).await;

        let report = h.shutdown_report().await;
        assert!(report.unconfirmed_tasks.is_empty(), "shutdown must drain fetch_catalog: {report:?}");
        assert!(
            !mare_binding::blocking::close(h.lua()),
            "no blocking package task may remain after Host::shutdown"
        );
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let rc = unsafe { libc::kill(pid, 0) };
        assert!(
            rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "git ssh helper (pid {pid}) must be dead before Host::shutdown returns"
        );
        drop(h);
        drop(env);
    })
    .await
}

//! A plugin's `config_schema`: one declaration gives the defaults its `apply`
//! receives and the checks every config entry point runs — types, enum choices, ranges, nested
//! keys, list items, close-name hints — with `on_invalid` deciding what an invalid value does.
//!
//! Sources: reference implementation (one schema per setting: type,
//! default, enum, range) and Codex `codex-rs/config/src/strict_config.rs:1-15` (unknown nested keys
//! are reported); the cases follow the SPEC entry format, written here.
pub(super) use crate::common::*;
pub(super) use serde_json::{Value, json};

pub(super) const PROBE: &str = r#"
return { name = "probe",
  config_schema = {
    count = { type = "integer", default = 1, min = 0, max = 10, desc = "how many" },
    mode = { type = "enum", choices = { "fast", "deep" }, default = "fast", desc = "how" },
    strict = { type = "enum", choices = { "on", "off" }, default = "off", on_invalid = "error", desc = "guarded" },
    ratio = { type = "number", default = 0.5, min = 0, max = 1, desc = "share" },
    nested = { type = "object", desc = "a group", fields = {
      flag = { type = "boolean", default = false, desc = "a flag" },
      label = { type = "string", default = "x", desc = "a label" },
    } },
    names = { type = "list", items = { type = "string" }, default = {}, desc = "names" },
    limits = { type = "map", values = { type = "integer", min = 0 }, default = {}, desc = "per name" },
    hook = { type = "function", desc = "a callback", scope = "lua" },
    target = { type = { "string", "table" }, desc = "a command: string or argv" },
  },
  apply = function(ctx, cfg) mare.g.probe_cfg = mare.json.encode(cfg) end,
}
"#;

pub(super) async fn probe_host(init: &str) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
    std::fs::write(env.config_dir().join("plugins/probe.lua"), PROBE).unwrap();
    std::fs::write(env.config_dir().join("init.lua"), format!("mare.setup {{ model = 'faux/echo' }}\n{init}")).unwrap();
    let h = env.host().user_config(false).no_session().build().await;
    (env, h)
}

pub(super) async fn cfg_and_warnings(h: &mare_host::Host) -> (Value, String) {
    let cfg: String = eval(h, "return mare.g.probe_cfg").await;
    let w: String = eval(h, "return table.concat(mare.config_warnings(), '\\n')").await;
    (serde_json::from_str(&cfg).unwrap(), w)
}

/// The defaults come from the declaration, nested fields included.
#[tokio::test]
async fn defaults_come_from_the_schema() {
    local(async {
        let (_env, h) = probe_host("").await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert_eq!(cfg["count"], 1);
        assert_eq!(cfg["mode"], "fast");
        assert_eq!(cfg["nested"], json!({ "flag": false, "label": "x" }));
        assert_eq!(cfg["ratio"], 0.5);
        assert!(w.is_empty(), "{w}");
        h.shutdown().await;
    })
    .await
}

/// Valid values are taken, leaf by leaf (nested tables merge by key).
#[tokio::test]
async fn valid_values_are_taken() {
    local(async {
        let (_env, h) = probe_host(
            "mare.plugin('probe', { count = 3, mode = 'deep', nested = { flag = true }, names = { 'a' }, limits = { x = 2 }, \
             target = { 'cmd', '-x' } })",
        )
        .await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.is_empty(), "{w}");
        assert_eq!(cfg["count"], 3);
        assert_eq!(cfg["mode"], "deep");
        assert_eq!(cfg["nested"], json!({ "flag": true, "label": "x" }));
        assert_eq!(cfg["names"], json!(["a"]));
        assert_eq!(cfg["limits"], json!({ "x": 2 }));
        assert_eq!(cfg["target"], json!(["cmd", "-x"]));
        h.shutdown().await;
    })
    .await
}

/// A plugin that declares no options still has its slot switch checked: a string
/// `enabled` (here `"false"`, as read from an environment variable) is warned about and that
/// layer's value is dropped, so the boolean `false` an earlier layer set stays and the slot stays
/// off (reference implementation: a string plugin switch is a
/// boolean type error).
#[tokio::test]
async fn a_string_enabled_for_a_plugin_without_options_is_warned_and_dropped() {
    local(async {
        let (_env, h) = probe_host(
            "mare.plugin('query-disk', { enabled = false })\nmare.plugin('query-disk', { enabled = 'false' })",
        )
        .await;
        let (_, w) = cfg_and_warnings(&h).await;
        let enabled = h.kernel.config()["plugins"]["query-disk"]["enabled"].clone();
        h.shutdown().await;
        assert!(w.contains("plugin `query-disk` option `enabled` expects a boolean, got string"), "{w}");
        assert_eq!(enabled, json!(false), "the earlier boolean stays");
    })
    .await
}

/// Each invalid leaf is reported with its path and why, and does not take effect: the declared
/// default stays (warn_default, the default `on_invalid`).
#[tokio::test]
async fn invalid_leaves_are_reported_and_keep_the_default() {
    local(async {
        let (_env, h) = probe_host(
            "mare.plugin('probe', { count = 99, mode = 'slow', ratio = -1, nested = { flag = 'yes', lable = 'y' }, \
             names = { 'a', 2 }, limits = { x = -1 }, cuont = 4, target = 5 })",
        )
        .await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        for want in [
            "plugin `probe` option `count`: 99 is above the maximum 10",
            "plugin `probe` option `mode`: `slow` is not one of fast, deep",
            "plugin `probe` option `ratio`: -1 is below the minimum 0",
            "plugin `probe` option `nested.flag` expects a boolean, got string",
            "plugin `probe` has no option `nested.lable` — did you mean `nested.label`?",
            "plugin `probe` option `names[2]` expects a string, got number",
            "plugin `probe` option `limits.x`: -1 is below the minimum 0",
            "plugin `probe` has no option `cuont` — did you mean `count`?",
            "plugin `probe` option `target` expects a string or table, got number",
        ] {
            assert!(w.contains(want), "missing {want:?} in:\n{w}");
        }
        assert_eq!(cfg["count"], 1, "invalid: the default stays");
        assert_eq!(cfg["mode"], "fast");
        assert_eq!(cfg["ratio"], 0.5);
        assert_eq!(cfg["nested"]["flag"], false);
        assert_eq!(cfg["names"], json!([]), "a list is one value: invalid as a whole");
        assert!(cfg.get("cuont").is_none());
        h.shutdown().await;
    })
    .await
}

/// `on_invalid = "error"` is a configuration error (`--strict-config` refuses to start on any);
/// the value does not take effect either.
#[tokio::test]
async fn on_invalid_error_is_reported_as_an_error() {
    local(async {
        let (_env, h) = probe_host("mare.plugin('probe', { strict = 'maybe' })").await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.contains("error: plugin `probe` option `strict`: `maybe` is not one of on, off"), "{w}");
        assert_eq!(cfg["strict"], "off");
        h.shutdown().await;
    })
    .await
}

/// `--set plugins.<p>.<path>=…` (the command-line layer) goes through the same checks.
#[tokio::test]
async fn the_command_line_layer_is_checked_the_same_way() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/probe.lua"), PROBE).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env
            .host()
            .user_config(false)
            .no_session()
            .config(json!({ "plugins": { "probe": { "count": 42, "mode": "deep" } } }))
            .build()
            .await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.contains("plugin `probe` option `count`: 42 is above the maximum 10"), "{w}");
        assert_eq!(cfg["count"], 1);
        assert_eq!(cfg["mode"], "deep");
        h.shutdown().await;
    })
    .await
}

async fn local_package_host(
    plugin: (&str, &str),
    pkg_fields: &str,
    init_tail: &str,
) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    let pkg = env.cwd.join("pkg");
    std::fs::create_dir_all(pkg.join("plugins")).unwrap();
    std::fs::write(pkg.join("plugins").join(format!("{}.lua", plugin.0)), plugin.1).unwrap();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("init.lua"),
        format!(
            "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ dir = {:?}, name = 'x/pkg'{pkg_fields} }} }}\n{init_tail}",
            pkg.to_string_lossy()
        ),
    )
    .unwrap();
    let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
    (env, h)
}

/// A local package (`mare.plugins.add { { dir = … } }`) shipping `plugin`, with `opts`; the host
/// built over it (startup problems are left for the caller to inspect).
async fn package_host(plugin: (&str, &str), opts: &str) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    local_package_host(plugin, &format!(", opts = {opts}"), "").await
}

/// A package's `opts` (`mare.plugins.add`, merged after the config files ran) go through the same
/// checks as every other entry point, against the declaration of the plugin the package ships:
/// an out-of-range value is reported, naming the package, and the default stays; a valid value
/// is taken. They were merged unchecked, so a plugin relying on its declared bounds got e.g.
/// `count = 99`.
#[tokio::test]
async fn package_opts_are_checked_the_same_way() {
    local(async {
        let (_env, h) = package_host(("probe", PROBE), "{ probe = { count = 99, mode = 'deep', ratio = -1 } }").await;
        assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.contains("plugin `probe` option `count`: 99 is above the maximum 10 (package x/pkg)"), "{w}");
        assert!(w.contains("plugin `probe` option `ratio`: -1 is below the minimum 0 (package x/pkg)"), "{w}");
        assert_eq!(cfg["count"], 1, "invalid: the default stays");
        assert_eq!(cfg["ratio"], 0.5);
        assert_eq!(cfg["mode"], "deep", "a valid value is taken");
        h.shutdown().await;
    })
    .await
}

/// A package option declared `on_invalid = "fail"` keeps its plugin from starting, as the same
/// value in init.lua does, and the problem names the option.
#[tokio::test]
async fn an_invalid_package_option_declared_fail_keeps_its_plugin_from_starting() {
    local(async {
        let guarded = r#"return { name = "guarded",
  config_schema = { limit = { type = "integer", default = 5, min = 1, on_invalid = "fail", desc = "a bound" } },
  apply = function(ctx, cfg) mare.g.guarded_limit = cfg.limit end }"#;
        let (_env, h) = package_host(("guarded", guarded), "{ guarded = { limit = 0 } }").await;
        let started: bool = eval(&h, "return mare.g.guarded_limit ~= nil").await;
        let problems = h.kernel.problems().join("\n");
        assert!(!started, "the plugin does not start: {problems}");
        assert!(
            problems.contains("plugin `guarded` option `limit`: 0 is below the minimum 1"),
            "the problem names the option: {problems}"
        );
        h.shutdown().await;
    })
    .await
}

/// A package from a custom source (`plugins.url_format`, e.g. a company Git mirror), already installed:
/// its opts are checked against the plugin it ships whichever layer sets the source — `init.lua`
/// or the command line, which is merged after the config files. The package is located with the
/// final configuration, as the runtime locates it (located with
/// the default source instead, the schema was missing, `count = 99` reached the plugin).
async fn custom_source_host(cli_url_format: bool) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    let format = "https://git.example.com/%s.git";
    let dir = mare_plugin_manager::checkout_dir(&format.replace("%s", "x/pkg"));
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::write(dir.join("plugins/probe.lua"), PROBE).unwrap();
    // A real installed checkout and lock, so startup can trust it without fetching.
    let source = format.replace("%s", "x/pkg");
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "plugins/probe.lua"],
        vec!["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-q", "-m", "fixture"],
        vec!["remote", "add", "origin", source.as_str()],
    ] {
        let output = fixture_git(&dir).args(&args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    let head = fixture_git(&dir).args(["rev-parse", "HEAD"]).output().unwrap();
    assert!(head.status.success());
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("mare-lock.json"),
        json!({ "version": 1, "packages": { "x/pkg": { "url": source, "commit": String::from_utf8_lossy(&head.stdout).trim() } } }).to_string(),
    ).unwrap();
    let set_source = if cli_url_format { String::new() } else { format!("mare.o.plugins.url_format = {format:?}\n") };
    std::fs::write(
        env.config_dir().join("init.lua"),
        format!(
            "mare.setup {{ model = 'faux/echo' }}\n{set_source}mare.plugins.add {{ {{ 'x/pkg', opts = {{ probe = {{ count = 99, mode = 'deep' }} }} }} }}"
        ),
    )
    .unwrap();
    let mut b = env.host().user_config(false).no_session();
    if cli_url_format {
        b.opts.config = json!({ "plugins": { "url_format": format } });
    }
    let h = mare_host::Host::new(b.opts).await.unwrap();
    (env, h)
}

#[tokio::test]
async fn package_opts_from_a_custom_source_are_checked() {
    local(async {
        for cli_url_format in [false, true] {
            let (env, h) = custom_source_host(cli_url_format).await;
            assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
            let (cfg, w) = cfg_and_warnings(&h).await;
            assert!(
                w.contains("plugin `probe` option `count`: 99 is above the maximum 10 (package x/pkg)"),
                "source set on the command line: {cli_url_format}: {w}"
            );
            assert_eq!(cfg["count"], 1, "invalid: the default stays");
            assert_eq!(cfg["mode"], "deep", "a valid value is taken");
            h.shutdown().await;
            drop(env);
        }
    })
    .await
}

#[tokio::test]
async fn valid_package_plugin_options_in_init_lua_do_not_warn_and_pass_strict_config() {
    local(async {
        let (env, h) =
            local_package_host(("probe", PROBE), "", "mare.plugin('probe', { count = 7, mode = 'deep' })\n").await;
        assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.is_empty(), "valid package plugin options must not warn: {w}");
        assert_eq!(cfg["count"], 7);
        assert_eq!(cfg["mode"], "deep");
        h.shutdown().await;

        let out = mare_cmd()
            .env("MARE_HOME", env.home.path())
            .current_dir(&env.cwd)
            .args(["--strict-config", "--model", "faux/echo", "--no-session", "-p", "hi"])
            .run_output();
        assert_eq!(
            out.status.code(),
            Some(0),
            "valid package plugin options must pass --strict-config: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    })
    .await
}

#[tokio::test]
async fn misspelled_package_plugin_option_warns_with_location_prunes_key_and_fails_strict_config() {
    local(async {
        let (env, h) =
            local_package_host(("probe", PROBE), "", "mare.plugin('probe', { cont = 7, count = 99, mode = 'deep' })\n")
                .await;
        assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(
            w.contains("plugin `probe` has no option `cont` — did you mean `count`?") && w.contains("init.lua:3"),
            "misspelled key must warn with hint and init.lua line: {w}"
        );
        assert!(
            w.contains("plugin `probe` option `count`: 99 is above the maximum 10") && w.contains("init.lua:3"),
            "out-of-range value must warn with init.lua line: {w}"
        );
        assert!(!w.contains("unknown plugin `probe`"), "{w}");
        assert_eq!(cfg["count"], 1, "invalid count must fall back to default");
        assert_eq!(cfg["mode"], "deep", "valid option stays");
        let raw_probe: Value =
            serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(mare.config.plugins.probe)").await)
                .unwrap();
        assert!(raw_probe.get("cont").is_none(), "unknown key must be pruned from config.plugins: {raw_probe}");
        assert!(raw_probe.get("count").is_none(), "invalid key must be pruned from config.plugins: {raw_probe}");
        h.shutdown().await;

        let out = mare_cmd()
            .env("MARE_HOME", env.home.path())
            .current_dir(&env.cwd)
            .args(["--strict-config", "--model", "faux/echo", "--no-session", "-p", "hi"])
            .run_output();
        assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("--strict-config") && err.contains("`cont`"), "{err}");
    })
    .await
}

#[tokio::test]
async fn init_lua_declared_package_checks_table_form_and_still_warns_unknown_plugin_names() {
    local(async {
        let (env, h) = local_package_host(
            ("probe", PROBE),
            "",
            "mare.setup { plugins = { probe = { cont = 1, mode = 'deep' } } }\nmare.plugin('no_such_plugin', { x = 1 })\n",
        )
        .await;
        assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(
            w.contains("plugin `probe` has no option `cont` — did you mean `count`?")
                && w.contains("init.lua:3"),
            "plugins table form must warn on package plugin option with location: {w}"
        );
        assert!(
            w.contains("unknown plugin `no_such_plugin`") && w.contains("init.lua:4"),
            "still-unknown plugin must still warn with original location: {w}"
        );
        assert_eq!(cfg["mode"], "deep");
        let raw_probe: Value =
            serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(mare.config.plugins.probe)").await)
                .unwrap();
        assert!(raw_probe.get("cont").is_none(), "unknown key in table form must be pruned: {raw_probe}");
        h.shutdown().await;

        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ dir = {:?}, name = 'x/pkg' }} }}\nmare.plugin('no_such_plugin', {{ x = 1 }})\n",
                env.cwd.join("pkg").to_string_lossy()
            ),
        )
        .unwrap();
        let out = mare_cmd()
            .env("MARE_HOME", env.home.path())
            .current_dir(&env.cwd)
            .args(["--strict-config", "--model", "faux/echo", "--no-session", "-p", "hi"])
            .run_output();
        assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("--strict-config") && err.contains("unknown plugin `no_such_plugin`"), "{err}");
    })
    .await
}

#[tokio::test]
async fn package_opts_for_an_unknown_plugin_still_warn() {
    local(async {
        let (_env, h) = package_host(("probe", PROBE), "{ ghost = { x = 1 } }").await;
        assert!(h.kernel.problems().is_empty(), "{:?}", h.kernel.problems());
        let (_, w) = cfg_and_warnings(&h).await;
        assert!(
            w.contains("unknown plugin `ghost`") && w.contains("package x/pkg"),
            "package opts targeting an unknown plugin must warn: {w}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn non_config_setup_with_unknown_plugin_warns_immediately() {
    local(async {
        let (_env, h) = local_package_host(("probe", PROBE), "", "").await;
        eval::<()>(&h, "mare.setup { plugins = { runtime_ghost = { x = 1 } } }").await;
        let (_, w) = cfg_and_warnings(&h).await;
        assert!(
            w.contains("unknown plugin `runtime_ghost`"),
            "non-config mare.setup must warn immediately instead of deferring: {w}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_reload_does_not_leak_deferred_plugins_into_the_next_reload() {
    local(async {
        let (env, h) = local_package_host(
            ("probe", PROBE),
            "",
            "mare.plugin('probe', { count = 5 })\n",
        )
        .await;
        let pkg = env.cwd.join("pkg");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ dir = {:?}, name = 'x/pkg' }} }}\nmare.plugin('probe', {{ cont = 1 }})\nerror('boom')\n",
                pkg.to_string_lossy()
            ),
        )
        .unwrap();
        assert!(h.reload_config().await.is_err());

        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}\nmare.plugins.add {{ {{ dir = {:?}, name = 'x/pkg' }} }}\nmare.plugin('probe', {{ count = 7 }})\n",
                pkg.to_string_lossy()
            ),
        )
        .unwrap();
        h.reload_config().await.unwrap();
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.is_empty(), "failed reload must not leak deferred warnings: {w}");
        assert_eq!(cfg["count"], 7);
        h.shutdown().await;
    })
    .await
}

/// The fixture `git` (`fixture_git`) ignores a `GIT_DIR` the test process inherited: initializing
/// the fixture checkout above must not write into the repository `GIT_DIR` names (a fixture
/// `git init` once set `core.bare = true` on the developer's checkout that way).
#[tokio::test]
async fn a_fixture_git_ignores_an_inherited_git_dir() {
    local(async {
        let mut env = TestEnv::new().await;
        let outer = env.cwd.join("outer");
        std::fs::create_dir_all(&outer).unwrap();
        assert!(fixture_git(&outer).args(["init", "-q"]).status().unwrap().success());
        let config = outer.join(".git/config");
        let before = std::fs::read_to_string(&config).unwrap();
        env.set_var("GIT_DIR", outer.join(".git").to_string_lossy().as_ref());
        env.set_var("GIT_WORK_TREE", outer.to_string_lossy().as_ref());
        let fixture = env.cwd.join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        assert!(fixture_git(&fixture).args(["init", "-q"]).status().unwrap().success());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before, "the inherited GIT_DIR was written");
        assert!(fixture.join(".git").is_dir(), "the fixture got its own repository");
    })
    .await
}

mod layers_and_fields;

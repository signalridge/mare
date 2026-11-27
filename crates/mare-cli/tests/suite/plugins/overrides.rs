use crate::common::*;

/// Write `~/.config/mare/plugins/review.lua`, a user plugin named like the built-in `review`.
fn user_review(env: &TestEnv, override_line: &str) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("review.lua"),
        format!("return {{ name = 'review', {override_line} apply = function() mare.g.user_review = true end }}"),
    )
    .unwrap();
}

fn status_of(h: &mare_host::Host, name: &str) -> (String, bool) {
    let (_, origin, active) = h.kernel.plugin_status().into_iter().find(|p| p.0 == name).unwrap();
    (origin, active)
}

/// (name slots): a user plugin with a built-in's name isn't
/// loaded without `override = true`; the built-in stays, the error names it and both fixes
/// (`override = true` or a rename), and it is a warning (not an essential-plugin failure).
/// Closing the slot is no way around it: `enabled = false` turns off every source of the name.
#[tokio::test]
async fn a_user_plugin_colliding_with_a_builtin_is_refused_without_override() {
    local(async {
        let env = TestEnv::new().await;
        user_review(&env, "");
        let opts = env.host().builtins(&["review"]).user_plugins().no_session().opts;
        let h = mare_host::Host::new(opts).await.expect("a refused definition must not stop startup");
        assert_eq!(status_of(&h, "review"), ("builtin".to_string(), true), "the built-in stays active");
        assert!(!eval::<bool>(&h, "return mare.g.user_review == true").await, "the user plugin didn't run");
        let warnings = h.plugin_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let w = &warnings[0];
        assert!(w.contains("built-in `review`") && w.contains("not loaded"), "{w}");
        assert!(w.contains("override = true") && w.contains("rename your plugin `review`"), "{w}");
        assert!(!w.contains("mare.builtins"), "no removed API in the advice: {w}");
        assert!(w.contains("requests continue"), "not a blocking failure: {w}");
        assert!(h.kernel.problems().iter().any(|p| p.contains("built-in `review`")), "{:?}", h.kernel.problems());
        assert_eq!(h.startup_notices(), vec![(w.clone(), "warning")], "shown once");
        let failure: (String, bool) = eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
        assert_eq!(failure, ("review".to_string(), false));
        assert!(h.prompt("hello", serde_json::json!({})).await.unwrap()["error"].is_null());
        h.shutdown().await;

        // closing the slot turns off both definitions: no "disable the built-in, then take its name"
        let h = env
            .host()
            .builtins(&["review"])
            .user_plugins()
            .no_session()
            .config(serde_json::json!({ "plugins": { "review": { "enabled": false } } }))
            .build()
            .await;
        assert!(h.kernel.plugin_status().iter().all(|p| p.0 != "review"), "no source of a closed slot loads");
        assert!(!eval::<bool>(&h, "return mare.g.user_review == true").await);
        assert!(h.plugin_warnings().is_empty(), "{:?}", h.plugin_warnings());
        h.shutdown().await;
    })
    .await
}

/// With `override = true` the user plugin wins as before and `/plugins` shows what it replaced.
#[tokio::test]
async fn a_user_plugin_with_override_replaces_the_builtin_and_is_listed() {
    local(async {
        let env = TestEnv::new().await;
        user_review(&env, "override = true,");
        let h = env.host().builtins(&["review", "plugins"]).user_plugins().no_session().build().await;
        assert_eq!(status_of(&h, "review"), ("user".to_string(), true));
        assert!(eval::<bool>(&h, "return mare.g.user_review == true").await);
        assert!(h.startup_notices().is_empty(), "{:?}", h.startup_notices());
        let overrides: Option<String> = eval(&h, "return mare.plugins.status('review').overrides").await;
        assert_eq!(overrides.as_deref(), Some("builtin"));
        // the runtime tree renders the "overrides built-in review" note without error
        global(&h, "CMD", h.service_table("commands").unwrap());
        exec(&h, "CMD:run('/plugins tree')").await;
        h.shutdown().await;
    })
    .await
}

/// `mare doctor` lists an override and reports a refused collision with its two fixes.
#[test]
fn doctor_shows_overrides_and_refused_collisions() {
    let run = |override_line: &str| {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review.lua"), format!("return {{ name = 'review', {override_line} }}")).unwrap();
        let out = mare_cmd()
            .arg("doctor")
            .env("MARE_HOME", home.path())
            .env("NO_COLOR", "1")
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let text = run("override = true,");
    assert!(text.contains("review overrides built-in review"), "{text}");
    let text = run("");
    assert!(!text.contains("overrides built-in"), "{text}");
    assert!(text.contains("built-in `review`") && text.contains("override = true"), "{text}");
}

/// a built-in's wire module stays overridable via `require("provider-openai.wire")`: replacing a
/// function on the cached module table reaches wire's own internal calls (`convert_messages` →
/// `M.normalize`).
#[tokio::test]
async fn replacing_a_provider_openai_wire_function_reaches_its_internal_calls() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let via_internal_call: bool = eval(
            &h,
            r#"local wire = require("provider-openai.wire")
               local orig = wire.normalize
               wire.normalize = function() return "replaced" end
               local r = wire.convert_messages({}, "gpt-test", "openai")
               wire.normalize = orig
               return r == "replaced""#,
        )
        .await;
        assert!(via_internal_call, "require('provider-openai.wire') did not reach internal wire calls");
        h.shutdown().await;
    })
    .await
}

/// Lua code loads source text only: `load`, `loadfile` and `dofile` refuse a binary chunk from a
/// string or a file whatever mode they are asked for (only the host loads bytecode it compiled
/// itself — built-ins, the module cache, functions carried to a sub-agent); text still loads,
/// with an explicit environment.
#[tokio::test]
async fn lua_code_cannot_load_binary_chunks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let code = h.lua().load("return 42").into_function().unwrap().dump(false);
        let path = env.cwd.join("chunk.luac");
        std::fs::write(&path, &code).unwrap();
        global(&h, "BC", h.lua().create_string(&code).unwrap());
        global(&h, "PATH", path.to_string_lossy().to_string());
        let got: String = eval(
            &h,
            r#"local out = {}
               local function refused(f, e) return f == nil and tostring(e):find("binary chunk", 1, true) ~= nil end
               out[#out + 1] = tostring(refused(load(BC)))
               out[#out + 1] = tostring(refused(load(BC, "x", "b")))
               out[#out + 1] = tostring(refused(load(BC, "x", "bt", {})))
               out[#out + 1] = tostring(refused(loadfile(PATH, "b")))
               local ok, err = pcall(dofile, PATH)
               out[#out + 1] = tostring(not ok and tostring(err):find("binary chunk", 1, true) ~= nil)
               out[#out + 1] = tostring(load("return X", "=t", "b", { X = 7 })())
               return table.concat(out, " ")"#,
        )
        .await;
        assert_eq!(got, "true true true true true 7");
        h.shutdown().await;
    })
    .await
}

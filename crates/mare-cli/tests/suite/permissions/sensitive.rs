//! Credential-path reminders through the real tool and approval pipeline.

use super::*;

#[tokio::test]
async fn sensitive_file_and_directory_defaults_prompt() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        let names = [
            ".env",
            ".env.local",
            "id_rsa",
            "id_rsa-backup",
            "id_ed25519",
            "id_ed25519.bak",
            "id_ecdsa",
            "id_ecdsa_work",
            ".netrc",
            ".git-credentials",
            ".npmrc",
            ".pypirc",
        ];
        for name in names {
            std::fs::write(env.cwd.join(name), "credential-contents\n").unwrap();
        }
        std::fs::write(env.cwd.join("credentials"), "ordinary file\n").unwrap();
        let directories =
            [".ssh", ".aws", ".azure", ".gnupg", ".kube", ".docker", ".config/gcloud", ".config/gh", ".gcp"];
        for dir in directories {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::create_dir_all(env.cwd.join("other")).unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "TEST_HOME", home.to_string_lossy().to_string());
        exec(&h, "mare.fs.home = TEST_HOME").await;
        let output: String = eval(
            &h,
            r#"
            local outcomes, asked = {}, 0
            local function denied(call)
                local before = asked
                local r = T:execute(call, { approve = function()
                    asked = asked + 1
                    return { decision = "deny", source = "user" }
                end })
                outcomes[#outcomes + 1] = { name = call.name .. ":" .. tostring(call.arguments.path or call.id),
                    asked = asked - before, refused = r.refused == true }
            end
            for i, name in ipairs({ ".env", ".env.local", "id_rsa", "id_rsa-backup", "id_ed25519",
                "id_ed25519.bak", "id_ecdsa", "id_ecdsa_work", ".netrc", ".git-credentials", ".npmrc", ".pypirc" }) do
                denied({ id = "read-" .. i, name = "read", arguments = { path = name } })
            end
            for i, dir in ipairs({ ".ssh", ".aws", ".azure", ".gnupg", ".kube", ".docker",
                ".config/gcloud", ".config/gh", ".gcp" }) do
                denied({ id = "ls-" .. i, name = "ls", arguments = { path = TEST_HOME .. "/" .. dir } })
            end
            denied({ id = "write-key", name = "write", arguments = { path = "other/id_rsa", content = "new" } })
            T:observe(".env.local")
            denied({ id = "edit-env", name = "edit", arguments = {
                path = ".env.local", old_string = "credential-contents", new_string = "changed" } })
            denied({ id = "patch-env", name = "apply_patch", arguments = { input =
                "*** Begin Patch\n*** Add File: other/.netrc\n+new\n*** End Patch" } })
            local ordinary = T:execute({ id = "ordinary", name = "read", arguments = { path = "credentials" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return mare.json.encode_strict({ outcomes = outcomes, ordinary_error = ordinary.is_error == true })
            "#,
        )
        .await;
        h.shutdown().await;
        let result: Value = serde_json::from_str(&output).unwrap();
        for outcome in result["outcomes"].as_array().unwrap() {
            assert_eq!(outcome["asked"], 1, "sensitive path must ask: {outcome}");
            assert_eq!(outcome["refused"], true, "the denial must stop this call: {outcome}");
        }
        assert_eq!(result["ordinary_error"], false, "a generic credentials basename must not be sensitive");
        assert!(!env.cwd.join("other/id_rsa").exists());
        assert!(!env.cwd.join("other/.netrc").exists());
        assert_eq!(std::fs::read_to_string(env.cwd.join(".env.local")).unwrap(), "credential-contents\n");
    })
    .await
}

#[tokio::test]
async fn sensitive_policy_respects_forbidden_never_and_headless() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let rules_file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(rules_file.parent().unwrap()).unwrap();
        std::fs::write(
            &rules_file,
            json!({ "version": 1, "rules": [
            { "tool": "read", "path": ".env", "op": "read", "decision": "forbidden" }
        ] })
            .to_string(),
        )
        .unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (forbidden, forbidden_asks): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "forbidden", name = "read", arguments = { path = ".env" } }, {
                approve = function() asked = asked + 1; return { decision = "allow", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(forbidden);
        assert_eq!(forbidden_asks, 0, "forbidden wins before any answerer");
        std::fs::remove_file(&rules_file).unwrap();

        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (auto_denied, auto_asks, never_denied, never_asks, headless_denied, full_allowed, full_asks, danger_asked):
            (bool, i64, bool, i64, bool, bool, i64, bool) = eval(&h, r#"
            local approval = C.cordis:get("approvals")
            local asked = 0
            local function deny() asked = asked + 1; return { decision = "deny", source = "user" } end
            local function call(id, opts)
                return T:execute({ id = id, name = "read", arguments = { path = ".env" } }, opts)
            end
            local auto = call("auto", { approve = deny })
            local auto_asks = asked
            approval:set("never")
            local never = call("never", { approve = deny })
            local never_asks = asked - auto_asks
            approval:set("ask")
            local headless = call("headless")
            assert(P:set("full-access"))
            local full = call("full", { approve = deny })
            local full_asks = asked - auto_asks - never_asks
            local before = asked
            local danger = T:execute({ id = "danger", name = "bash", arguments = { command = "rm -rf missing-dir" } }, {
                approve = deny,
            })
            return auto.refused == true, auto_asks, never.refused == true, never_asks,
                headless.refused == true, full.is_error ~= true, full_asks,
                danger.refused == true and asked == before + 1
        "#).await;
        h.shutdown().await;
        assert!(auto_denied && auto_asks == 1, "auto must ask and honor a refusal");
        assert!(never_denied && never_asks == 0, "never must refuse before calling the answerer");
        assert!(headless_denied, "a headless sensitive request must fail closed");
        assert!(full_allowed && full_asks == 0, "full-access omits the sensitive reminder");
        assert!(danger_asked, "dangerous commands still ask in full-access");
    })
    .await
}

#[tokio::test]
async fn sensitive_shell_literal_and_unanalysable_fallback() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let result: String = eval(&h, r##"
            local name = mare.os == "windows" and "powershell" or "bash"
            local commands = mare.os == "windows"
                and { "Get-Content .env", "Get-Content --file=.env", "Get-Content '.env'", "Get-Content .env; echo $(Get-Date)", "# comment\nGet-Content .env", "tool --env-file=.env" }
                or { "cat .env", "cat --file=.env", "cat '.env'", "cat .env; echo $(printf dynamic)", "bash -lc 'cat .env'", "# comment\ncat .env", "docker run --env-file=.env img", "curl --config=.netrc https://example.com" }
            local out = {}
            for i, command in ipairs(commands) do
                local asked, reason = 0, ""
                local r = T:execute({ id = "shell-" .. i, name = name, arguments = { command = command } }, {
                    approve = function(_, _, verdict)
                        asked, reason = asked + 1, tostring(verdict.reason)
                        return { decision = "deny", source = "user" }
                    end,
                })
                out[i] = { asked = asked, refused = r.refused == true,
                    sensitive = reason:find("credential path", 1, true) ~= nil,
                    leaked = mare.json.encode_strict(r):find("credential-contents", 1, true) ~= nil }
            end
            return mare.json.encode_strict(out)
        "##).await;
        h.shutdown().await;
        let cases: Value = serde_json::from_str(&result).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(case["asked"], 1, "literal credential path must be presented: {case}");
            assert_eq!(case["refused"], true, "a refused shell must not execute: {case}");
            assert_eq!(case["sensitive"], true, "the reason must identify the reminder: {case}");
            assert_eq!(case["leaked"], false, "a denial must not return file contents: {case}");
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn sensitive_shell_current_home_literal_prompts() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/config"), "sensitive fixture\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (asked, refused, sensitive): (i64, bool, bool) = eval(
            &h,
            r#"
            local asked, reason = 0, ""
            local r = T:execute({ id = "home-literal", name = "bash", arguments = { command = "cat ~/.ssh/config" } }, {
                approve = function(_, _, verdict)
                    asked, reason = asked + 1, tostring(verdict.reason)
                    return { decision = "deny", source = "user" }
                end,
            })
            return asked, r.refused == true, reason:find("credential path", 1, true) ~= nil
        "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(asked, 1, "the current home's literal path must ask");
        assert!(refused && sensitive, "the credential reminder must block a refused call");
    })
    .await
}

#[tokio::test]
async fn sensitive_uses_xdg_credential_paths() {
    local(async {
        let mut env = TestEnv::new().await;
        let root = env.home.path().to_path_buf();
        env.set_var("MARE_HOME", "");
        env.set_var("HOME", &root);
        env.set_var("USERPROFILE", &root);
        for (name, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            env.set_var(name, root.join(dir));
        }
        let auth = root.join("config/mare/auth.json");
        let oauth = root.join("state/mare/mcp-oauth/session");
        for file in [&auth, &oauth] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "fixture\n").unwrap();
        }
        std::fs::write(&auth, "{\"version\":1}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "XDG_CREDENTIALS", vec![auth.to_string_lossy().to_string(), oauth.to_string_lossy().to_string()]);
        let outcomes: Vec<bool> = eval(
            &h,
            r#"
            local out = {}
            for i, path in ipairs(XDG_CREDENTIALS) do
                local asked = 0
                local r = T:execute({ id = "xdg-" .. i, name = "read", arguments = { path = path } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
                out[i] = asked == 1 and r.refused == true
            end
            return out
        "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(outcomes, vec![true, true], "XDG credential paths must prompt: {outcomes:?}");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn sensitive_rule_is_not_saved_if_path_changes_during_result_event() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env.control"), "control\n").unwrap();
        std::fs::write(env.cwd.join(".env"), "old\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        exec(&h, r#"
            local function remember(_, _, verdict)
                return { decision = "allow_amend", amendment = verdict.amendment, source = "user" }
            end
            local control = T:execute({ id = "control", name = "read", arguments = { path = ".env.control" } }, { approve = remember })
            assert(control.is_error ~= true, mare.json.encode(control))
            SELF_ENTER_TX, SELF_ENTER_RX = mare.task.channel()
            SELF_RELEASE_TX, SELF_RELEASE_RX = mare.task.channel()
            local first_event = true
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.call and ev.call.id == "event-swap" and first_event then
                    first_event = false
                    SELF_ENTER_TX:send(true)
                    SELF_RELEASE_RX:recv()
                end
            end, { prepend = true })
            local answers = 0
            SELF_TASK = mare.task.spawn(function()
                SELF_RESULT = T:execute({ id = "event-swap", name = "read", arguments = { path = ".env" } }, {
                    approve = function(...)
                        answers = answers + 1
                        if answers == 1 then return remember(...) end
                        return { decision = "deny", source = "user" }
                    end,
                })
            end)
            SELF_ENTER_RX:recv()
        "#).await;
        std::fs::rename(env.cwd.join(".env"), env.cwd.join("old.env")).unwrap();
        std::fs::write(env.cwd.join(".env"), "replacement\n").unwrap();
        exec(&h, r#"SELF_RELEASE_TX:send(true); SELF_TASK:wait()"#).await;
        h.shutdown().await;
        let file = env.home.path().join("state/permissions.json");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let rules = value["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "only the unchanged control rule may persist: {value}");
        assert!(rules[0]["path"].as_str().unwrap().ends_with(".env.control"), "{value}");
    }).await
}

#[tokio::test]
async fn only_exact_tool_path_rule_exempts_sensitive() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let save = |rules: Value| std::fs::write(&file, json!({ "version": 1, "rules": rules }).to_string()).unwrap();
        save(json!([{ "tool": "read", "path": ".env", "op": "read", "decision": "allow" }]));
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (ok, asked): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "exact", name = "read", arguments = { path = ".env" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.is_error ~= true, asked
        "#,
        )
        .await;
        assert!(ok && asked == 0, "one exact same-tool path rule must exempt the reminder");
        save(json!([{ "tool": "read", "path": "**", "op": "read", "decision": "allow" }]));
        let (refused, asked): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "glob", name = "read", arguments = { path = ".env" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        assert!(refused && asked == 1, "a broad allow must not exempt a sensitive file");

        save(json!([{ "tool": "read", "decision": "allow" }]));
        let (tool_only_refused, tool_only_asked): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "tool-only", name = "read", arguments = { path = ".env" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        assert!(tool_only_refused && tool_only_asked == 1, "tool-only allow must not exempt a sensitive file");

        save(json!([{ "tool": "write", "path": ".env", "op": "read", "decision": "allow" }]));
        let (cross_refused, cross_asked): (bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local r = T:execute({ id = "cross-tool", name = "read", arguments = { path = ".env" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        assert!(cross_refused && cross_asked == 1, "another tool's exact path rule must not exempt a different tool");

        save(json!([]));
        let (sess_ok_1, sess_ok_2, sess_asked): (bool, bool, i64) = eval(
            &h,
            r#"
            local asked = 0
            local opts = { approve = function() asked = asked + 1; return { decision = "allow_session", source = "user" } end }
            local r1 = T:execute({ id = "sess-1", name = "read", arguments = { path = ".env" } }, opts)
            local r2 = T:execute({ id = "sess-2", name = "read", arguments = { path = ".env" } }, opts)
            return r1.is_error ~= true, r2.is_error ~= true, asked
        "#,
        )
        .await;
        assert!(sess_ok_1 && sess_ok_2 && sess_asked == 2, "allow_session must not cache sensitive calls");

        std::fs::write(env.cwd.join(".netrc"), "machine example login a password b\n").unwrap();
        save(json!([
            { "tool": "apply_patch", "path": ".env", "op": "read", "decision": "allow" },
            { "tool": "apply_patch", "path": ".env", "op": "write", "decision": "allow" }
        ]));
        let (patch_refused, patch_asked): (bool, i64) = eval(
            &h,
            r#"
            for _, p in ipairs({ ".env", ".netrc" }) do T:observe(p) end
            local asked = 0
            local r = T:execute({ id = "partial-patch", name = "apply_patch", arguments = {
                input = "*** Begin Patch\n*** Update File: .env\n@@\n-credential-contents\n+new-env\n*** Update File: .netrc\n@@\n-machine example login a password b\n+new-netrc\n*** End Patch" } }, {
                approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
            })
            return r.refused == true, asked
        "#,
        )
        .await;
        assert!(patch_refused && patch_asked == 1, "multi-file patch with partial exemption must still ask");
        assert_eq!(std::fs::read_to_string(env.cwd.join(".env")).unwrap(), "credential-contents\n");
        assert_eq!(std::fs::read_to_string(env.cwd.join(".netrc")).unwrap(), "machine example login a password b\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sensitive_exemptions_do_not_escape_parent_directory() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        for name in [".env.example", ".env.sample", ".env.template", "id_rsa.pub", "id_ed25519.pub", "id_ecdsa.pub", ".env.example.bak", "id_rsa.pub.bak"] {
            std::fs::write(env.cwd.join(name), "fixture\n").unwrap();
        }
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".aws/.env.example"), "fixture\n").unwrap();
        std::fs::write(home.join(".ssh/id_rsa.pub"), "fixture\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "TEST_HOME", home.to_string_lossy().to_string());
        exec(&h, "mare.fs.home = TEST_HOME").await;
        let result: String = eval(&h, r#"
            local out = {}
            local paths = { ".env.example", ".env.sample", ".env.template", "id_rsa.pub", "id_ed25519.pub", "id_ecdsa.pub",
                ".env.example.bak", "id_rsa.pub.bak", TEST_HOME .. "/.aws/.env.example", TEST_HOME .. "/.ssh/id_rsa.pub" }
            for i, path in ipairs(paths) do
                local asked = 0
                local r = T:execute({ id = "exemption-" .. i, name = "read", arguments = { path = path } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
                out[i] = { asked = asked, refused = r.refused == true, error = r.is_error == true }
            end
            return mare.json.encode_strict(out)
        "#).await;
        h.shutdown().await;
        let cases: Value = serde_json::from_str(&result).unwrap();
        let cases = cases.as_array().unwrap();
        for case in &cases[..6] {
            assert_eq!(case["asked"], 0, "exact public/example names are exempt: {case}");
            assert_eq!(case["error"], false, "the control read must succeed: {case}");
        }
        for case in &cases[6..] {
            assert_eq!(case["asked"], 1, "backup or sensitive parent still asks: {case}");
            assert_eq!(case["refused"], true, "the denial must stop the read: {case}");
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn sensitive_resolves_logical_physical_and_mare_paths() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "secret\n").unwrap();
        std::fs::write(env.cwd.join("plain.txt"), "ordinary\n").unwrap();
        std::os::unix::fs::symlink(env.cwd.join(".env"), env.cwd.join("alias.txt")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("plain.txt"), env.cwd.join(".env.local")).unwrap();
        let auth = env.home.path().join("config/auth.json");
        let oauth = env.home.path().join("state/mcp-oauth/session");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::create_dir_all(oauth.parent().unwrap()).unwrap();
        std::fs::write(&auth, "{\"version\":1}\n").unwrap();
        std::fs::write(auth.with_extension("json.bak"), "{\"version\":1}\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(auth.with_extension("json.bak"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&oauth, "fixture\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "TEST_AUTH", auth.to_string_lossy().to_string());
        global(&h, "TEST_OAUTH", oauth.to_string_lossy().to_string());
        let result: String = eval(
            &h,
            r#"
            local out = {}
            for i, path in ipairs({ "alias.txt", ".env.local", TEST_AUTH, TEST_AUTH .. ".bak", TEST_OAUTH }) do
                local asked = 0
                local r = T:execute({ id = "identity-" .. i, name = "read", arguments = { path = path } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
                out[i] = { asked = asked, refused = r.refused == true }
            end
            return mare.json.encode_strict(out)
        "#,
        )
        .await;
        h.shutdown().await;
        let cases: Value = serde_json::from_str(&result).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(case["asked"], 1, "logical/physical or configured credential path must ask: {case}");
            assert_eq!(case["refused"], true);
        }
    })
    .await
}

#[tokio::test]
async fn sensitive_defaults_cover_windows_home_directory_set() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        env.set_var("USERPROFILE", &home);
        let paths: Vec<_> = mare_binding::winsandbox_user::default_deny_read(&home, None, None)
            .into_iter()
            .filter(|p| {
                ![".netrc", ".git-credentials", "auth.json"]
                    .contains(&p.file_name().unwrap().to_string_lossy().as_ref())
            })
            .collect();
        for path in &paths {
            std::fs::create_dir_all(path).unwrap();
        }
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "TEST_HOME", home.to_string_lossy().to_string());
        exec(&h, "mare.fs.home = TEST_HOME").await;
        let paths: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        global(&h, "WIN_HOME_DIRS", paths.clone());
        let outcomes: Vec<bool> = eval(
            &h,
            r#"
            local out = {}
            for i, path in ipairs(WIN_HOME_DIRS) do
                local asked = 0
                local r = T:execute({ id = "win-default-" .. i, name = "ls", arguments = { path = path } }, {
                    approve = function() asked = asked + 1; return { decision = "deny", source = "user" } end,
                })
                out[i] = asked == 1 and r.refused == true
            end
            return out
        "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(outcomes.len(), paths.len());
        assert!(outcomes.iter().all(|v| *v), "each Windows home directory default must trigger a reminder");
    })
    .await
}

#[tokio::test]
async fn sensitive_structured_approval_preserves_constraints() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (refused, asked, kept_tool, kept_request, no_grant): (bool, i64, bool, bool, bool) = eval(&h, r#"
            T:register("sensitive_request", {
                description = "read a file and request permissions", side_effect = "read", permission_request = true,
                parameters = { type = "object", properties = { path = { type = "string" },
                    additional_permissions = { type = "object" }, justification = { type = "string" }, scope = { type = "string" } },
                    required = { "path", "additional_permissions", "justification" } },
                access = function(a) return { { kind = "file", op = "read", path = a.path, recursive = false } } end,
                execute = function(a) return { content = assert(mare.fs.read(a.path)) } end,
            })
            local asked, kept_tool, kept_request = 0, false, false
            local r = T:execute({ id = "structured-sensitive", name = "sensitive_request", arguments = {
                path = ".env", additional_permissions = { network = true }, justification = "need access", scope = "session" } }, {
                approve = function(_, _, verdict)
                    asked = asked + 1
                    for _, category in ipairs(verdict.categories or {}) do if category == "tool" then kept_tool = true end end
                    kept_request = verdict.requested_permissions and verdict.requested_permissions.network == true
                        and verdict.permission_scope == "session"
                    return { decision = "deny", source = "user" }
                end,
            })
            local grants = P:grants()
            return r.refused == true, asked, kept_tool, kept_request, grants.network ~= true
        "#).await;
        h.shutdown().await;
        assert!(refused && asked == 1);
        assert!(kept_tool && kept_request, "sensitive and structured approval constraints must coexist");
        assert!(no_grant, "refusing the request must not persist a grant");
    })
    .await
}

#[tokio::test]
async fn approved_sensitive_write_gets_the_existing_readonly_write_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (ok, asked, message): (bool, i64, String) = eval(&h, r#"
            local asked = 0
            local r = T:execute({ id = "sensitive-write", name = "write", arguments = { path = ".env", content = "fixture" } }, {
                approve = function() asked = asked + 1; return { decision = "allow", source = "user" } end,
            })
            return r.is_error ~= true, asked, tostring(r.content)
        "#).await;
        h.shutdown().await;
        assert!(ok && asked == 1, "an approved sensitive write must execute: {message}");
        assert_eq!(std::fs::read_to_string(env.cwd.join(".env")).unwrap(), "fixture");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn sensitive_one_sided_symlink_exact_rule_still_prompts() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "secret\n").unwrap();
        std::fs::write(env.cwd.join("plain.txt"), "ordinary\n").unwrap();
        std::os::unix::fs::symlink(env.cwd.join(".env"), env.cwd.join("alias.txt")).unwrap();
        std::os::unix::fs::symlink(env.cwd.join("plain.txt"), env.cwd.join(".env.link")).unwrap();
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            json!({ "version": 1, "rules": [
                { "tool": "read", "path": ".env", "op": "read", "decision": "allow" },
                { "tool": "read", "path": ".env.link", "op": "read", "decision": "allow" }
            ] })
            .to_string(),
        )
        .unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (alias_refused, alias_asked, link_refused, link_asked): (bool, i64, bool, i64) = eval(
            &h,
            r#"
            local a1, a2 = 0, 0
            local r1 = T:execute({ id = "alias", name = "read", arguments = { path = "alias.txt" } }, {
                approve = function() a1 = a1 + 1; return { decision = "deny", source = "user" } end,
            })
            local r2 = T:execute({ id = "link", name = "read", arguments = { path = ".env.link" } }, {
                approve = function() a2 = a2 + 1; return { decision = "deny", source = "user" } end,
            })
            return r1.refused == true, a1, r2.refused == true, a2
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(alias_refused && alias_asked == 1, "symlink to .env must not be exempted by .env exact rule");
        assert!(
            link_refused && link_asked == 1,
            ".env symlink to plain file must not be exempted by .env.link exact rule"
        );
    })
    .await
}

#[tokio::test]
async fn deny_read_auth_json_does_not_flag_ordinary_workspace_files() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        let auth = home.join("config/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, "{\"version\":1}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(env.cwd.join("README.md"), "# hello\n").unwrap();
        std::fs::create_dir_all(env.cwd.join("src")).unwrap();
        let h = host(
            &env,
            true,
            json!({
                "preset": "auto",
                "deny_read": [auth.to_string_lossy()],
                "accept_partial": ["fs_deny_read"]
            }),
            json!({ "no_session": true }),
        )
        .await;
        global(&h, "AUTH_PATH", auth.to_string_lossy().to_string());
        let (read_ok, write_ok, ls_ok, asked, auth_err): (bool, bool, bool, i64, bool) = eval(
            &h,
            r#"
            local asked = 0
            local deny = function() asked = asked + 1; return { decision = "deny", source = "user" } end
            local r = T:execute({ id = "readme", name = "read", arguments = { path = "README.md" } }, { approve = deny })
            local w = T:execute({ id = "src", name = "write", arguments = { path = "src/lib.rs", content = "fn main() {}\n" } }, { approve = deny })
            local l = T:execute({ id = "ls", name = "ls", arguments = { path = "." } }, { approve = deny })
            local a = T:execute({ id = "auth", name = "read", arguments = { path = AUTH_PATH } }, { approve = deny })
            return r.is_error ~= true, w.is_error ~= true, l.is_error ~= true, asked, a.is_error == true
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(read_ok && write_ok && ls_ok && asked == 0, "ordinary files must not ask when auth.json is in deny_read");
        assert!(auth_err, "reading auth.json itself must still fail");
    })
    .await
}

#[tokio::test]
async fn sensitive_preserves_rule_danger_categories_and_sandbox_permissions() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let outside_raw = env.home.path().join("outside_sandbox");
        std::fs::create_dir_all(&outside_raw).unwrap();
        let outside_dir = dunce::canonicalize(&outside_raw).unwrap();
        let rules_file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(rules_file.parent().unwrap()).unwrap();
        std::fs::write(
            &rules_file,
            json!({ "version": 1, "rules": [
                { "tool": "read", "path": ".env", "op": "read", "decision": "prompt" },
                { "argv": ["rm"], "decision": "prompt" }
            ] })
            .to_string(),
        )
        .unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "OUTSIDE_DIR", outside_dir.to_string_lossy().to_string());
        let (has_tool_cat, has_rules_cat, gran_refused, gran_asked, danger_cats_ok, no_why_refused, no_why_asked, gran_sandbox_refused, gran_sandbox_asked, sandbox_cats_ok, scoped_exec_ok):
            (bool, bool, bool, i64, bool, bool, i64, bool, i64, bool, bool) = eval(
            &h,
            r#"
            local approval = C.cordis:get("approvals")
            local has_tool, has_rules = false, false
            local r1 = T:execute({ id = "rule-sens", name = "read", arguments = { path = ".env" } }, {
                approve = function(_, _, v)
                    for _, c in ipairs(v.categories or {}) do
                        if c == "tool" then has_tool = true end
                        if c == "rules" then has_rules = true end
                    end
                    return { decision = "deny", source = "user" }
                end,
            })
            assert(r1.refused == true)

            approval:set({ mode = "granular", categories = { tool = true, rules = false } })
            local g_asked = 0
            local r_gran = T:execute({ id = "gran-sens", name = "read", arguments = { path = ".env" } }, {
                approve = function() g_asked = g_asked + 1; return { decision = "allow", source = "user" } end,
            })
            approval:set("ask")

            local d_tool, d_rules = false, false
            local r_danger = T:execute({ id = "danger-sens", name = "bash", arguments = { command = "rm -rf .env" } }, {
                approve = function(_, _, v)
                    for _, c in ipairs(v.categories or {}) do
                        if c == "tool" then d_tool = true end
                        if c == "rules" then d_rules = true end
                    end
                    return { decision = "deny", source = "user" }
                end,
            })
            assert(r_danger.refused == true)

            local nw_asked = 0
            local r_nowhy = T:execute({ id = "no-why", name = "bash", arguments = {
                command = "cat .env", sandbox_permissions = "network" } }, {
                approve = function() nw_asked = nw_asked + 1; return { decision = "allow", source = "user" } end,
            })

            assert(P:set("read-only"))
            local scope = "write:" .. OUTSIDE_DIR
            local cmd = mare.os == "windows"
                and ("Get-Content .env; New-Item -ItemType File -Force -Path '" .. OUTSIDE_DIR .. "\\ok.txt' | Out-Null")
                or ("cat .env && touch '" .. OUTSIDE_DIR .. "/ok.txt'")
            local shell_tool = mare.os == "windows" and "powershell" or "bash"

            approval:set({ mode = "granular", categories = { tool = true, sandbox = false } })
            local gs_asked = 0
            local r_gs = T:execute({ id = "gran-sandbox", name = shell_tool, arguments = {
                command = cmd, sandbox_permissions = scope, justification = "need write" } }, {
                approve = function() gs_asked = gs_asked + 1; return { decision = "allow", source = "user" } end,
            })
            approval:set("ask")

            local s_tool, s_sandbox = false, false
            local r_scoped = T:execute({ id = "scoped-sens", name = shell_tool, arguments = {
                command = cmd, sandbox_permissions = scope, justification = "need write" } }, {
                approve = function(_, _, v)
                    for _, c in ipairs(v.categories or {}) do
                        if c == "tool" then s_tool = true end
                        if c == "sandbox" then s_sandbox = true end
                    end
                    return { decision = "allow", source = "user" }
                end,
            })
            return has_tool, has_rules, r_gran.refused == true, g_asked,
                d_tool and d_rules, r_nowhy.refused == true, nw_asked,
                r_gs.refused == true, gs_asked,
                s_tool and s_sandbox, r_scoped.is_error ~= true
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(has_tool_cat && has_rules_cat, "sensitive + prompt rule must include both tool and rules categories");
        assert!(gran_refused && gran_asked == 0, "granular rules=false must deny sensitive+prompt rule without asking");
        assert!(danger_cats_ok, "danger + sensitive + prompt rule must include categories");
        assert!(no_why_refused && no_why_asked == 0, "sensitive shell command with invalid sandbox_permissions must be refused");
        assert!(gran_sandbox_refused && gran_sandbox_asked == 0, "granular sandbox=false must deny sensitive shell command with widening request");
        assert!(sandbox_cats_ok && scoped_exec_ok, "sensitive shell command must validate and apply sandbox_permissions");
        assert!(outside_dir.join("ok.txt").exists(), "widened write scope must take effect on approved sensitive shell call");
    })
    .await
}

#[tokio::test]
async fn readonly_sensitive_write_requires_sandbox_approval() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "original\n").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (refused, asked): (bool, i64) = eval(
            &h,
            r#"
            C.cordis:get("approvals"):set({ mode = "granular", categories = { tool = true, sandbox = false } })
            local asked = 0
            local result = T:execute({ id = "sensitive-write", name = "write",
                arguments = { path = ".env", content = "changed" } }, {
                approve = function() asked = asked + 1; return { decision = "allow", source = "user" } end,
            })
            return result.refused == true, asked
        "#,
        )
        .await;
        assert!(refused, "a sensitive write that widens read-only must be refused when sandbox approval is disabled");
        assert_eq!(asked, 0, "a disabled approval category must not reach the user");
        assert_eq!(std::fs::read_to_string(env.cwd.join(".env")).unwrap(), "original\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn allow_amend_does_not_void_snapshot_or_double_prompt() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "credential-contents\n").unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (env_ok, env_asked, env_voids, write_ok, write_asked, write_voids): (
            bool,
            i64,
            i64,
            bool,
            i64,
            i64,
        ) = eval(
            &h,
            r#"
            local approval = C.cordis:get("approvals")
            approval:set("untrusted")
            local voids = 0
            C.cordis:on("tool/snapshot-void", function() voids = voids + 1 end)

            local asked_env = 0
            local r_env = T:execute({ id = "amend-env", name = "read", arguments = { path = ".env" } }, {
                approve = function(_, _, verdict)
                    asked_env = asked_env + 1
                    return { decision = "allow_amend", amendment = verdict.amendment, source = "user" }
                end,
            })
            local env_voids = voids

            voids = 0
            local asked_write = 0
            local r_write = T:execute({ id = "amend-write", name = "write", arguments = { path = "note.txt", content = "ok" } }, {
                approve = function(_, _, verdict)
                    asked_write = asked_write + 1
                    return { decision = "allow_amend", amendment = verdict.amendment, source = "user" }
                end,
            })
            return r_env.is_error ~= true, asked_env, env_voids, r_write.is_error ~= true, asked_write, voids
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(env_ok && env_asked == 1 && env_voids == 0, "persisted allow_amend must not trigger snapshot-void or double prompt: ok={env_ok} asked={env_asked} voids={env_voids}");
        assert!(write_ok && write_asked == 1 && write_voids == 0, "session allow_amend must not trigger snapshot-void or double prompt: ok={write_ok} asked={write_asked} voids={write_voids}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("note.txt")).unwrap(), "ok");
    })
    .await
}

#[cfg(windows)]
#[tokio::test]
async fn sensitive_windows_case_and_separator_variants_prompt() {
    local(async {
        let mut env = TestEnv::new().await;
        let home = env.home.path().to_path_buf();
        env.set_var("HOME", &home);
        env.set_var("USERPROFILE", &home);
        std::fs::write(env.cwd.join(".ENV"), "WIN_SECRET=1\n").unwrap();
        std::fs::create_dir_all(home.join(".SSH")).unwrap();
        std::fs::write(home.join(".SSH/ID_RSA"), "KEY\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        global(&h, "TEST_HOME", home.to_string_lossy().to_string());
        exec(&h, "mare.fs.home = TEST_HOME").await;
        global(&h, "WIN_SSH_KEY", home.join(".SSH\\ID_RSA").to_string_lossy().to_string());
        let (env_refused, env_asked, key_refused, key_asked): (bool, i64, bool, i64) = eval(
            &h,
            r#"
            local a1, a2 = 0, 0
            local r1 = T:execute({ id = "win-env", name = "read", arguments = { path = ".ENV" } }, {
                approve = function() a1 = a1 + 1; return { decision = "deny", source = "user" } end,
            })
            local r2 = T:execute({ id = "win-key", name = "read", arguments = { path = WIN_SSH_KEY } }, {
                approve = function() a2 = a2 + 1; return { decision = "deny", source = "user" } end,
            })
            return r1.refused == true, a1, r2.refused == true, a2
        "#,
        )
        .await;
        h.shutdown().await;
        assert!(env_refused && env_asked == 1, "uppercase .ENV must trigger sensitive reminder on Windows");
        assert!(
            key_refused && key_asked == 1,
            "mixed-separator/uppercase .SSH\\ID_RSA must trigger sensitive reminder on Windows"
        );
    })
    .await
}

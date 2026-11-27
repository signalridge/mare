//! Real process regressions for doctor.
use super::*;

/// `mare doctor` lists a session's half-written last entry (a crash mid-write) as a warning and a
/// damaged line in the middle as a failure.
#[test]
fn doctor_reports_torn_and_damaged_session_files() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("state/sessions/project");
    std::fs::create_dir_all(&dir).unwrap();
    let header = r#"{"type":"session","version":3,"session_id":"s1","cwd":"/x"}"#;
    std::fs::write(dir.join("torn.jsonl"), format!("{header}\n{{\"type\":\"mess")).unwrap();
    std::fs::write(dir.join("bad.jsonl"), format!("{header}\nnot json\n")).unwrap();
    let out = mare_cmd()
        .args(["doctor", "--json"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let checks = report.as_array().or(report["checks"].as_array()).unwrap_or_else(|| panic!("{text}"));
    let check = |name: &str| checks.iter().find(|c| c["name"] == name).cloned();
    let torn = check("session torn writes").expect("torn tail reported");
    assert!(torn["warn"] == true && torn["detail"].as_str().unwrap().contains("torn.jsonl"), "{torn}");
    let files = check("session files").unwrap();
    assert!(files["ok"] == false && files["detail"].as_str().unwrap().contains("bad.jsonl"), "{files}");
}

/// `mare doctor` reports the session index: an unusable file is set aside and
/// rebuilt from the session files, then it is ok, with as many rows as session files.
#[test]
fn doctor_reports_the_session_index_state() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("state/sessions");
    for id in ["s1", "s2"] {
        let dir = sessions.join("project");
        std::fs::create_dir_all(&dir).unwrap();
        let header = json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":id,"cwd":"/x"});
        std::fs::write(dir.join(format!("{id}.jsonl")), format!("{header}\n")).unwrap();
    }
    std::fs::write(sessions.join("index.sqlite"), "not a database, just some text long enough").unwrap();
    let legacy = home.path().join("data/sessions/index.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "{\"id\":\"s1\",\"seq\":1}\n").unwrap();
    let index_check = || {
        let out = mare_cmd()
            .args(["doctor", "--json"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        let checks = report.as_array().or(report["checks"].as_array()).cloned().unwrap_or_default();
        checks.into_iter().find(|c| c["name"] == "session index").unwrap_or_else(|| panic!("{text}"))
    };
    let first = index_check();
    let detail = first["detail"].as_str().unwrap();
    assert!(first["ok"] == false && first["warn"] == true && detail.starts_with("rebuilt"), "{first}");
    assert!(detail.contains("2 rows, 2 session files"), "{first}");
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "{\"id\":\"s1\",\"seq\":1}\n");
    assert!(!legacy.with_extension("jsonl.migrated").exists());
    let second = index_check();
    assert!(second["ok"] == true && second["detail"].as_str().unwrap().starts_with("ok: 2 rows"), "{second}");
}

/// `mare doctor --fix` only creates missing directories and tightens permissions.
/// Windows: DACLs (`mare_shared::private`) — auth.json inherits a MARE_HOME open to everyone.
#[test]
fn doctor_fix_creates_directories_and_tightens_permissions() {
    let home = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(home.path());
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let auth = config.join("auth.json");
    std::fs::write(&auth, r#"{"version":1}"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let doctor = |fix: bool| {
        let mut c = mare_cmd();
        c.arg("doctor").env("MARE_HOME", home.path()).current_dir(home.path()).stdin(Stdio::null());
        if fix {
            c.arg("--fix");
        }
        String::from_utf8_lossy(&c.run_output().stdout).to_string()
    };
    let before = doctor(false);
    // the Windows wording names users, not mode bits
    let loose = if cfg!(windows) { "readable by other users" } else { "readable by group/others" };
    assert!(before.contains(loose) && before.contains("auth.json"), "{before}");
    let _ = doctor(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777, 0o600);
    }
    #[cfg(windows)]
    assert_eq!(mare_shared::private::check(&auth), Ok(()));
    let after = doctor(false);
    assert!(after.contains("credentials and sessions are private"), "{after}");
    assert!(!after.contains("directories") || !after.contains("missing:"), "{after}");
}

/// `mare doctor` and `mare doctor --fix` inspect nested `sessions/<slug>/*.jsonl` files.
#[test]
fn doctor_scans_and_fixes_nested_session_file_permissions() {
    let home = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    mare_shared::private::testing::open_to_everyone(home.path());
    let nested_dir = home.path().join("state/sessions/project-slug");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let session = nested_dir.join("test.jsonl");
    std::fs::write(&session, "{\"type\":\"session\",\"version\":3,\"session_id\":\"s1\",\"cwd\":\"/x\"}\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let doctor = |fix: bool| {
        let mut c = mare_cmd();
        c.arg("doctor").env("MARE_HOME", home.path()).current_dir(home.path()).stdin(Stdio::null());
        if fix {
            c.arg("--fix");
        }
        String::from_utf8_lossy(&c.run_output().stdout).to_string()
    };
    let before = doctor(false);
    let loose = if cfg!(windows) { "readable by other users" } else { "readable by group/others" };
    assert!(before.contains(loose) && before.contains("test.jsonl"), "{before}");
    let _ = doctor(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&session).unwrap().permissions().mode() & 0o777, 0o600);
    }
    #[cfg(windows)]
    assert_eq!(mare_shared::private::check(&session), Ok(()));
    let after = doctor(false);
    assert!(after.contains("credentials and sessions are private"), "{after}");
}

/// essential actions left with no key are reported as warnings by `mare doctor`.
#[test]
fn doctor_warns_when_essential_key_has_no_binding() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("init.lua"), r#"mare.plugin("keymap-default", { enabled = false })"#).unwrap();
    let out = mare_cmd()
        .args(["doctor", "--json"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let checks = report["checks"].as_array().unwrap_or_else(|| panic!("{text}"));
    assert!(
        checks.iter().any(|c| c["warn"] == true && c["detail"].as_str().is_some_and(|d| d.contains("editor.submit"))),
        "doctor must warn about unbound editor.submit: {text}"
    );
}

/// `mare doctor` must not suggest forking from a damaged session file.
#[test]
fn doctor_does_not_suggest_forking_damaged_session() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("state/sessions/project");
    std::fs::create_dir_all(&dir).unwrap();
    let header = r#"{"type":"session","version":3,"session_id":"s1","cwd":"/x"}"#;
    std::fs::write(dir.join("bad.jsonl"), format!("{header}\nnot json\n")).unwrap();
    let out = mare_cmd()
        .args(["doctor", "--json"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let checks = report["checks"].as_array().unwrap_or_else(|| panic!("{text}"));
    let files = checks.iter().find(|c| c["name"] == "session files").expect("session files check");
    let detail = files["detail"].as_str().unwrap_or("");
    assert_eq!(files["ok"], false, "{files}");
    assert!(detail.contains("view read-only"), "expected read-only guidance: {detail}");
    assert!(
        !detail.to_ascii_lowercase().contains("fork"),
        "doctor must not suggest forking a damaged session: {detail}"
    );
}

/// `mare doctor` compile-checks cold modules of third-party plugins without executing them, so
/// syntax errors in modules not loaded during `apply` are caught proactively with zero side effects.
#[test]
fn doctor_preloads_third_party_cold_modules_and_reports_syntax_errors() {
    let home = tempfile::tempdir().unwrap();
    let plugin_dir = home.path().join("config/plugins/coldcheck");
    let lua_dir = plugin_dir.join("lua/coldcheck");
    std::fs::create_dir_all(&lua_dir).unwrap();
    std::fs::write(
        plugin_dir.join("init.lua"),
        r#"return {
            name = "coldcheck",
            apply = function(ctx)
                ctx:inject({ "commands" }, function(sub)
                    sub.commands:register("coldcheck", {
                        description = "lazy cold module test",
                        run = function()
                            return require("coldcheck.broken")
                        end,
                    })
                end)
            end,
        }"#,
    )
    .unwrap();
    let broken_path = lua_dir.join("broken.lua");
    std::fs::write(&broken_path, "return function( end\n").unwrap();
    std::fs::write(lua_dir.join("healthy.lua"), "return { ok = true }\n").unwrap();
    let sentinel_path = home.path().join("cold-side-effect.marker");
    std::fs::write(
        lua_dir.join("side_effect.lua"),
        format!(
            "local f = io.open({:?}, 'w'); if f then f:write('ran'); f:close() end\nerror('cold module body must not execute during doctor')\n",
            sentinel_path.display().to_string()
        ),
    )
    .unwrap();

    let run_doctor = || {
        let out = mare_cmd()
            .args(["doctor", "--json"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        (out.status.success(), report, text)
    };

    let (ok_before, report_before, text_before) = run_doctor();
    let checks_before = report_before["checks"].as_array().unwrap_or_else(|| panic!("{text_before}"));
    let broken_check = checks_before
        .iter()
        .find(|c| {
            c["name"] == "plugin module"
                && c["ok"] == false
                && c["warn"].is_null()
                && c["detail"].as_str().is_some_and(|d| d.contains("coldcheck") && d.contains("coldcheck.broken"))
        })
        .unwrap_or_else(|| panic!("expected failing plugin module check for coldcheck.broken:\n{text_before}"));
    assert_eq!(broken_check["ok"], false, "{broken_check}");
    assert!(!ok_before, "doctor must exit non-zero when a cold module fails to load");
    assert!(
        !checks_before.iter().any(|c| c["name"] == "plugin module"
            && c["detail"]
                .as_str()
                .is_some_and(|d| d.contains("coldcheck.healthy") || d.contains("coldcheck.side_effect"))),
        "valid cold modules (even with top-level error/side effects) must not execute or fail:\n{text_before}"
    );
    assert!(
        !sentinel_path.exists(),
        "doctor precheck must be compile-only and never execute cold module top-level code"
    );

    // Fixing the syntax error makes the plugin modules check pass.
    std::fs::write(&broken_path, "return { fixed = true }\n").unwrap();
    let (_ok_after, report_after, text_after) = run_doctor();
    assert!(!sentinel_path.exists(), "doctor precheck must remain compile-only after syntax errors are fixed");
    let checks_after = report_after["checks"].as_array().unwrap_or_else(|| panic!("{text_after}"));
    let summary = checks_after
        .iter()
        .find(|c| c["name"] == "plugin modules")
        .unwrap_or_else(|| panic!("expected plugin modules summary check:\n{text_after}"));
    assert_eq!(summary["ok"], true, "{summary}");
    assert!(
        !checks_after.iter().any(|c| c["name"] == "plugin module" && c["ok"] == false),
        "expected no failing plugin module checks after fix:\n{text_after}"
    );
}

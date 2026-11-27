//! Versioned state and credential artifact regressions.

use super::*;

/// reference implementation: unsupported versions
/// must reject both reads and writes and leave the original bytes untouched.
#[tokio::test]
async fn artifact_state_versions_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let path = env.home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cases = [
            "",
            " ",
            r#"{}"#,
            r#"{"version":null}"#,
            r#"{"version":true}"#,
            r#"{"version":"1"}"#,
            r#"{"version":1.0}"#,
            r#"{"version":-1}"#,
            r#"{"version":0}"#,
            r#"{"version":2}"#,
        ];
        let mut violations = Vec::new();
        for raw in cases {
            std::fs::write(&path, raw).unwrap();
            let (read_rejected, err): (bool, String) =
                eval(&h, "local v, e = mare.state.read(); return v == nil, tostring(e)").await;
            let write_rejected: bool = eval(&h, "return not pcall(mare.state.set, 'k', 'v')").await;
            let intact = std::fs::read_to_string(&path).unwrap() == raw;
            if !read_rejected || !err.contains("corrupt") || !write_rejected || !intact {
                violations
                    .push(format!("{raw:?}: read={read_rejected} err={err} write={write_rejected} intact={intact}"));
            }
        }
        assert!(violations.is_empty(), "{}", violations.join("\n"));
        h.shutdown().await;
    })
    .await;
}

/// Codex `codex-rs/login/src/auth/storage_tests.rs:44`: refused credential writes preserve
/// the existing bytes; Kimi Code `apps/reference implementation/test/utils/persistence.test.ts:56`: reject type errors.
#[tokio::test]
async fn artifact_auth_versions_fail_closed() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "env-must-not-bypass-corrupt-auth");
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let path = env.config_dir().join("auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cases = [
            "", " ", r#"{}"#, r#"{"version":null}"#, r#"{"version":true}"#,
            r#"{"version":"1"}"#, r#"{"version":1.0}"#, r#"{"version":-1}"#,
            r#"{"version":0}"#, r#"{"version":2}"#,
        ];
        let mut violations = Vec::new();
        for raw in cases {
            mare_shared::paths::write_atomic(&path, raw.as_bytes(), WriteMode::Private).unwrap();
            let (read_rejected, err): (bool, String) =
                eval(&h, "local ok, e = pcall(LLM.auth_get, LLM, 'openai'); return not ok, tostring(e)").await;
            let write_rejected: bool =
                eval(&h, "return not pcall(LLM.auth_set, LLM, 'openai', {key='new'})").await;
            let fallback_rejected: bool =
                eval(&h, "return not pcall(LLM.api_key, LLM, 'openai', {'OPENAI_API_KEY'})").await;
            let status_rejected: bool = eval(
                &h,
                "return not pcall(LLM.credential_sources, LLM, 'openai') and not pcall(LLM.logged_in, LLM) and not pcall(LLM.auth_status, LLM, 'openai')",
            )
            .await;
            let intact = std::fs::read_to_string(&path).unwrap() == raw;
            if !read_rejected || !err.contains("corrupt") || !write_rejected || !fallback_rejected || !status_rejected || !intact {
                violations.push(format!("{raw:?}: read={read_rejected} err={err} write={write_rejected} fallback={fallback_rejected} status={status_rejected} intact={intact}"));
            }
        }
        assert!(violations.is_empty(), "{}", violations.join("\n"));
        h.shutdown().await;
    })
    .await;
}

/// Codex `codex-rs/login/src/auth/storage_tests.rs:44`: a read failure must not replace credentials.
#[tokio::test]
async fn artifact_auth_read_error_never_overwrites() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        exec(&h, r#"LLM:auth_set("openai", { key = "existing" })"#).await;
        let path = env.config_dir().join("auth.json");
        let before = std::fs::read(&path).unwrap();
        let (ok, err): (bool, String) = eval(
            &h,
            r#"local read = mare.fs.read
               mare.fs.read = function(path, ...)
                 if path == mare.paths.auth then return nil, "simulated I/O failure" end
                 return read(path, ...)
               end
               local ok, e = pcall(LLM.auth_set, LLM, "openai", {key="replacement"})
               mare.fs.read = read
               return ok, tostring(e)"#,
        )
        .await;
        assert!(!ok && err.contains("read"), "read failure must be returned: {err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "read failure must not replace credentials");
        h.shutdown().await;
    })
    .await;
}

/// Codex `storage_tests.rs:44`: refuse writes that cannot preserve existing credentials.
#[tokio::test]
async fn artifact_auth_unrepresentable_number_does_not_create_corrupt_backup() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let path = env.config_dir().join("auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = br#"{"version":1,"extra":18446744073709551615,"openai":{"key":"existing"}}"#;
        mare_shared::paths::write_atomic(&path, raw, WriteMode::Private).unwrap();
        let (read_ok, write_ok, error): (bool, bool, String) = eval(
            &h,
            r#"local record = LLM:auth_get("openai")
               local ok, err = pcall(LLM.auth_set, LLM, "openai", {key="replacement"})
               return record.key == "existing", ok, tostring(err)"#,
        )
        .await;
        assert!(read_ok);
        assert!(!write_ok && error.contains("lossless") && !error.contains("corrupt"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        assert!(!path.with_extension("json.bak").exists(), "valid JSON must not be backed up as corrupt");
        h.shutdown().await;
    })
    .await;
}

/// A backup read failure must not mask the original corrupt-file error.
#[tokio::test]
async fn artifact_auth_backup_read_failure_keeps_corrupt_diagnostic() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let path = env.config_dir().join("auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = b"{";
        mare_shared::paths::write_atomic(&path, raw, WriteMode::Private).unwrap();
        let (ok, error): (bool, String) = eval(
            &h,
            r#"local read = mare.fs.read
               mare.fs.read = function(path, ...)
                 if path == mare.paths.auth .. ".bak" then error("backup read denied") end
                 return read(path, ...)
               end
               local ok, err = pcall(LLM.auth_get, LLM, "openai")
               mare.fs.read = read
               return ok, tostring(err)"#,
        )
        .await;
        assert!(!ok && error.contains("corrupt") && !error.contains("backup read denied"), "{error}");
        assert_eq!(std::fs::read(path).unwrap(), raw);
        h.shutdown().await;
    })
    .await;
}

/// Kimi `persistence.test.ts:56`: invalid bytes must be rejected without replacement.
#[tokio::test]
async fn artifact_state_non_utf8_is_never_overwritten() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let path = env.home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = b"\xff\xfe";
        std::fs::write(&path, raw).unwrap();
        let (read_rejected, write_rejected): (bool, bool) =
            eval(&h, "local value = mare.state.read(); return value == nil, not pcall(mare.state.set, 'k', 'v')").await;
        assert!(read_rejected && write_rejected);
        assert_eq!(std::fs::read(path).unwrap(), raw);
        h.shutdown().await;
    })
    .await;
}

/// Codex `storage_tests.rs:44`: an actual read restriction must prevent replacement.
#[cfg(unix)]
#[tokio::test]
async fn artifact_state_unreadable_is_never_overwritten() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let path = env.home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = br#"{"version":1,"model":"existing"}"#;
        std::fs::write(&path, raw).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        let (read_rejected, write_rejected): (bool, bool) =
            eval(&h, "local value = mare.state.read(); return value == nil, not pcall(mare.state.set, 'k', 'v')").await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_rejected && write_rejected);
        assert_eq!(std::fs::read(path).unwrap(), raw);
        h.shutdown().await;
    })
    .await;
}

/// Kimi `persistence.test.ts:46`: writes must retain the entire JSON document.
#[tokio::test]
async fn artifact_state_unrepresentable_number_is_never_overwritten() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let path = env.home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = br#"{"version":1,"extra":18446744073709551615}"#;
        std::fs::write(&path, raw).unwrap();
        let (ok, error): (bool, String) =
            eval(&h, "local ok, err = pcall(mare.state.set, 'k', 'v'); return ok, tostring(err)").await;
        assert!(!ok && error.contains("lossless"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        assert!(!path.with_extension("json.bak").exists());
        h.shutdown().await;
    })
    .await;
}

/// trust.rs:504-514 preserves unknown fields; Kimi persistence.test.ts:46 checks roundtrip.
#[tokio::test]
async fn artifact_versioned_files_keep_unknown_fields_on_rewrite() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let state = env.home.path().join("state/state.json");
        let auth = env.config_dir().join("auth.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        let extra =
            serde_json::json!({"none": null, "empty": {}, "items": [null, {}, []], "integer": 9007199254740993_i64});
        let state_before = serde_json::json!({"version": 1, "extra": extra});
        let auth_before = serde_json::json!({"version": 1, "extra": extra});
        mare_shared::paths::write_atomic(&state, state_before.to_string().as_bytes(), WriteMode::Private).unwrap();
        mare_shared::paths::write_atomic(&auth, auth_before.to_string().as_bytes(), WriteMode::Private).unwrap();
        exec(&h, r#"mare.state.set("fresh", "ok"); LLM:auth_set("openai", {key="new"})"#).await;
        let state_after: serde_json::Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        let auth_after: serde_json::Value = serde_json::from_slice(&std::fs::read(&auth).unwrap()).unwrap();
        assert_eq!(
            (state_after["extra"].clone(), auth_after["extra"].clone()),
            (state_before["extra"].clone(), auth_before["extra"].clone()),
            "versioned files must preserve unknown fields"
        );
        assert_eq!(state_after["version"], 1);
        assert_eq!(auth_after["version"], 1);
        h.shutdown().await;
    })
    .await;
}

/// Codex `codex-rs/login/src/auth/storage_tests.rs:71`: compare-and-save must accept the
/// observed credential but never overwrite a newer one.
#[tokio::test]
async fn auth_expected_record_with_null_fields_commits_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let path = env.config_dir().join("auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let initial = serde_json::json!({"version":1,"openai":{"key":"old","extra":null},"other":{"nested":[null,{}]}});
        mare_shared::paths::write_atomic(&path, initial.to_string().as_bytes(), WriteMode::Private).unwrap();
        let (committed, stale): (bool, bool) = eval(
            &h,
            r#"local previous = LLM:auth_get("openai")
               local first = LLM:auth_set("openai", {key="new"}, {expected=previous})
               local stale = LLM:auth_set("openai", {key="stale"}, {expected=previous})
               return first, stale"#,
        )
        .await;
        assert!(committed, "the record returned by auth_get must match the stored record");
        assert!(!stale, "a stale record must not replace a newer credential");
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(after["openai"]["key"], "new");
        assert_eq!(after["other"], initial["other"]);
        h.shutdown().await;
    })
    .await;
}

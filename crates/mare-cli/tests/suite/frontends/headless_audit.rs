use super::*;

#[test]
fn headless_approval_audit_never_exposes_rewrite_only_execution_data() {
    let home = tempfile::tempdir().unwrap();
    approval_privacy_fixture(home.path());
    let script = home.path().join("privacy.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"name":"audit_effect","arguments":{"visible":"public request"}}]},{"text":"done"}
        ])
        .to_string(),
    )
    .unwrap();
    let output = mare_cmd()
        .args(["--json", "--yolo", "--model", "faux/echo", "--no-session", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .run_output();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        home.path().join("private-audit-observed").exists(),
        "full local data was not observed: {stdout}\n{stderr}"
    );
    assert!(!stdout.contains(APPROVAL_PRIVACY_SECRET) && !stderr.contains(APPROVAL_PRIVACY_SECRET));
    let events: Vec<Value> = stdout.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect();
    let audit = events.iter().find(|record| record["type"] == "approval/resolved").expect("public audit missing");
    let approval_id = audit["data"]["approval_id"].as_str().expect("approval_id");
    assert!(
        events
            .iter()
            .any(|record| record["type"] == "approval/requested" && record["data"]["approval_id"] == approval_id),
        "matching approval/requested missing: {stdout}"
    );
    assert_eq!(audit["data"]["result"]["call"].as_object().unwrap().len(), 2, "{audit}");
    assert_eq!(audit["data"]["result"]["decision"], "unavailable");
}

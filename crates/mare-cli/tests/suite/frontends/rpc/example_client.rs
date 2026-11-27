use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

fn find_python() -> &'static str {
    for candidate in ["python3", "python"] {
        if Command::new(candidate).arg("--version").output().is_ok_and(|out| out.status.success()) {
            return candidate;
        }
    }
    panic!("python3 or python is required to run examples/rpc/minimal_client.py");
}

fn example_script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/rpc/minimal_client.py")
}

#[test]
fn rpc_example_client_walks_initialize_subscribe_prompt_and_shutdown() {
    let script_path = example_script_path();
    assert!(script_path.is_file(), "missing RPC example script at {}", script_path.display());

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let faux_script = tmp.path().join("faux.json");
    std::fs::write(&faux_script, json!([{ "text": "hello from rpc example" }]).to_string()).unwrap();

    let output = Command::new(find_python())
        .arg("-B")
        .arg(&script_path)
        .args(["--mare", mare_bin(), "--model", "faux/echo", "--no-session", "--prompt", "say hello", "--json"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .env("MARE_TEST_OFFLINE", "1")
        .current_dir(&work)
        .output()
        .expect("spawn examples/rpc/minimal_client.py");

    assert!(
        output.status.success(),
        "example client failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let summary: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!("invalid JSON from example client ({e}): {}", String::from_utf8_lossy(&output.stdout))
    });
    assert!(
        summary["protocol_version"].as_str().is_some_and(|v| v.starts_with("3.")),
        "unexpected protocol_version: {summary}"
    );
    assert!(summary["session_id"].as_str().is_some_and(|s| !s.is_empty()), "missing session_id: {summary}");
    assert!(summary["subscription"]["mode"].as_str().is_some(), "expected events.subscribe result: {summary}");
    assert_eq!(summary["text"], "hello from rpc example", "unexpected reply: {summary}");
    let events = summary["events"].as_array().expect("events array");
    assert!(events.iter().any(|e| e == "turn/completed"), "expected turn/completed in received events: {summary}");
}

#[test]
fn rpc_example_client_reuses_startup_session_without_creating_a_second_one() {
    let script_path = example_script_path();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let faux_script = tmp.path().join("faux.json");
    std::fs::write(&faux_script, json!([{ "text": "reused startup session" }]).to_string()).unwrap();

    let output = Command::new(find_python())
        .arg("-B")
        .arg(&script_path)
        .args(["--mare", mare_bin(), "--model", "faux/echo", "--prompt", "say hello", "--json"])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .env("MARE_TEST_OFFLINE", "1")
        .current_dir(&work)
        .output()
        .expect("spawn examples/rpc/minimal_client.py");

    assert!(
        output.status.success(),
        "example client failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["text"], "reused startup session", "{summary}");

    // Only one persisted session should exist: the startup session returned in `initialize.session`.
    let mut jsonl_files = Vec::new();
    let mut stack = vec![home];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "jsonl")
                    && std::fs::read_to_string(&p)
                        .ok()
                        .and_then(|t| t.lines().next().map(str::to_string))
                        .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                        .is_some_and(|h| h["type"] == "session")
                {
                    jsonl_files.push(p);
                }
            }
        }
    }
    assert_eq!(
        jsonl_files.len(),
        1,
        "minimal_client.py should reuse initialize.session instead of calling session.create: {jsonl_files:?}"
    );
}

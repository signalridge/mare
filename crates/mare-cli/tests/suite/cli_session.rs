//! CLI session management through the real binary (Kimi Code `session.test.ts`).

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use crate::common::{ChildCommandExt, RpcClient, mare_cmd};

struct Sessions {
    _root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    other: PathBuf,
}

impl Sessions {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("work");
        let other = root.path().join("other");
        for path in [&home, &work, &other] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self { _root: root, home, work, other }
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        mare_cmd().args(args).env("MARE_HOME", &self.home).current_dir(cwd).stdin(Stdio::null()).run_output()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    }

    fn create(&self, cwd: &Path, name: &str) -> String {
        self.ok(cwd, &["session", "new", "--name", name]).trim().to_owned()
    }

    fn turn(&self, cwd: &Path, id: &str) {
        self.ok(cwd, &["--resume", id, "-p", "--model", "faux/echo", "--yolo", "hello"]);
    }

    fn list(&self, cwd: &Path, options: &[&str]) -> Vec<Value> {
        let mut args = vec!["session", "list", "--json"];
        args.extend_from_slice(options);
        let val: Value = serde_json::from_str(&self.ok(cwd, &args)).unwrap();
        val["data"].as_array().cloned().unwrap_or_default()
    }
}

fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter().map(|row| row["session_id"].as_str().unwrap()).collect()
}

#[test]
fn cli_session_list_scopes_formats_and_limits_results() {
    let s = Sessions::new();
    let one = s.create(&s.work, "first session");
    s.turn(&s.work, &one);
    let two = s.create(&s.work, "second session");
    s.turn(&s.work, &two);
    let outside = s.create(&s.other, "other session");
    s.turn(&s.other, &outside);

    let local = s.ok(&s.work, &["session", "list"]);
    let short_ids: Vec<&str> = local.lines().filter_map(|l| l.split_whitespace().next()).collect();
    assert_eq!(short_ids.len(), 2, "{local}");
    assert_ne!(short_ids[0], short_ids[1], "{local}");
    assert!(local.contains(&one[..8]) && local.contains(&two[..8]), "{local}");
    assert!(local.contains("first session") && local.contains("second session"), "{local}");
    assert!(
        !local.contains("other session")
            && short_ids.iter().all(|id| !outside.starts_with(id))
            && !local.contains(s.work.to_str().unwrap()),
        "{local}"
    );
    let all = s.ok(&s.work, &["session", "list", "--all"]);
    assert!(
        all.contains(&outside[..8]) && all.contains("other session") && all.contains(s.other.to_str().unwrap()),
        "{all}"
    );
    assert_eq!(s.list(&s.work, &["--limit", "1"]).len(), 1);
    let invalid = s.run(&s.work, &["session", "list", "--limit", "0"]);
    assert_eq!(invalid.status.code(), Some(2), "{}", String::from_utf8_lossy(&invalid.stderr));
}

#[test]
fn cli_session_list_json_matches_rpc_session_list() {
    let s = Sessions::new();
    let id = s.create(&s.work, "rpc parity");
    s.turn(&s.work, &id);
    let second = s.create(&s.work, "rpc second");
    s.turn(&s.work, &second);
    let cli = s.list(&s.work, &[]);
    assert_eq!(ids(&cli), vec![second.as_str(), id.as_str()]);

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"]).env("MARE_HOME", &s.home).current_dir(&s.work).stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {}}), |_| Value::Null);
    assert!(init["result"]["protocol_version"].is_string(), "{init}");
    let result = rpc.call_raw("session.list", json!({}), |_| Value::Null);
    assert_eq!(Value::Array(cli), result["result"]["data"], "{result}");

    let limited = rpc.call_raw("session.list", json!({"limit": 1}), |_| Value::Null);
    assert_eq!(limited["result"]["data"].as_array().unwrap().len(), 1, "{limited}");

    s.ok(&s.work, &["session", "archive", &id]);
    let active_after = rpc.call_raw("session.list", json!({}), |_| Value::Null);
    assert!(!active_after["result"]["data"].to_string().contains(&id), "{active_after}");
    let archived = rpc.call_raw("session.list", json!({"archived": true}), |_| Value::Null);
    assert!(archived["result"]["data"].to_string().contains(&id), "{archived}");
    assert_eq!(Value::Array(s.list(&s.work, &["--archived"])), archived["result"]["data"], "{archived}");

    rpc.kill_and_wait();
}

#[test]
fn cli_session_archive_and_unarchive_by_name() {
    let s = Sessions::new();
    let id = s.create(&s.work, "archive target");
    s.turn(&s.work, &id);
    s.ok(&s.work, &["session", "archive", "ARCHIVE TARGET"]);
    assert!(!ids(&s.list(&s.work, &[])).contains(&id.as_str()));
    assert!(ids(&s.list(&s.work, &["--archived"])).contains(&id.as_str()));
    s.ok(&s.work, &["session", "unarchive", "archive target"]);
    assert!(ids(&s.list(&s.work, &[])).contains(&id.as_str()));
    assert!(!ids(&s.list(&s.work, &["--archived"])).contains(&id.as_str()));
}

#[test]
fn cli_session_fork_prints_id_of_listed_session() {
    let s = Sessions::new();
    let id = s.create(&s.work, "fork source");
    s.turn(&s.work, &id);
    let fork = s.ok(&s.work, &["session", "fork", "FORK SOURCE", "--name", "forked session"]);
    let fork = fork.trim();
    assert!(!fork.is_empty() && fork != id, "{fork}");
    assert!(ids(&s.list(&s.work, &[])).contains(&fork));
    assert!(s.ok(&s.work, &["session", "list"]).contains("forked session"));
}

#[test]
fn cli_session_delete_requires_yes_without_tty() {
    let s = Sessions::new();
    let id = s.create(&s.work, "delete target");
    s.turn(&s.work, &id);
    let refused = s.run(&s.work, &["session", "delete", &id]);
    assert_eq!(refused.status.code(), Some(2), "{}", String::from_utf8_lossy(&refused.stderr));
    assert!(ids(&s.list(&s.work, &[])).contains(&id.as_str()));
    s.ok(&s.work, &["session", "delete", &id, "--yes"]);
    assert!(!ids(&s.list(&s.work, &[])).contains(&id.as_str()));
    let missing = s.run(&s.work, &["session", "archive", "missing-session-id"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no session matches"));
}

#[test]
fn cli_session_delete_reports_unknown_address_before_confirmation() {
    // Codex cli/tests/delete.rs:4 resolves a missing session before asking to confirm.
    let s = Sessions::new();
    let output = s.run(&s.work, &["session", "delete", "missing-session-id"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{error}");
    assert!(error.contains("no session matches"), "{error}");
}

#[cfg(unix)]
#[test]
fn cli_session_delete_asks_for_confirmation_on_tty() {
    use crate::common::MareCmd;

    let s = Sessions::new();
    let id = s.create(&s.work, "tty target");
    s.turn(&s.work, &id);
    for (answer, remains) in [(b"n\n".as_slice(), true), (b"y\n".as_slice(), false)] {
        let mut tty = MareCmd::new().home_dir(&s.home).args(["session", "delete", &id]).pty(80, 24);
        tty.wait_text("delete confirmation", |text| text.contains("Delete") && text.contains('?'));
        tty.send(answer);
        let success = tty.wait_exit(20);
        assert!(remains || success);
        assert_eq!(ids(&s.list(&s.work, &[])).contains(&id.as_str()), remains);
    }
}

#[test]
fn cli_session_rejects_ambiguous_id_prefix() {
    let s = Sessions::new();
    let mut seen = std::collections::HashMap::new();
    let mut pair = None;
    for _ in 0..17 {
        let id = s.create(&s.work, "prefix target");
        let prefix = id.chars().next().unwrap();
        if let Some(previous) = seen.insert(prefix, id.clone()) {
            pair = Some((prefix, previous, id));
            break;
        }
    }
    let (prefix, first, second) = pair.expect("UUIDs have hexadecimal first digits");
    s.turn(&s.work, &first);
    s.turn(&s.work, &second);
    let output = s.run(&s.work, &["session", "archive", &prefix.to_string()]);
    assert_eq!(output.status.code(), Some(2), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("matches"));
    let listed = s.list(&s.work, &[]);
    assert!(ids(&listed).contains(&first.as_str()) && ids(&listed).contains(&second.as_str()));
}

#[test]
fn cli_session_startup_boundaries_session_id_fork_continue_all_and_relative_resume() {
    let s = Sessions::new();
    let one = s.create(&s.work, "first boundary");
    s.turn(&s.work, &one);
    std::thread::sleep(std::time::Duration::from_millis(50)); // sleep-ok: separate session timestamps so --continue picks the newer session deterministically
    let two = s.create(&s.work, "second boundary");
    s.turn(&s.work, &two);

    // 1. `--session-id` with an existing id fails loudly.
    let dup = s.run(&s.work, &["--session-id", &one, "-p", "--model", "faux/echo", "--yolo", "hi"]);
    assert!(!dup.status.success(), "duplicate --session-id must fail");
    let dup_err = String::from_utf8_lossy(&dup.stderr);
    assert!(dup_err.contains("already exists"), "{dup_err}");

    // 2. `--fork-session` without `-c` or `-r` fails loudly.
    let bad_fork = s.run(&s.work, &["--fork-session", "-p", "--model", "faux/echo", "--yolo", "hi"]);
    assert!(!bad_fork.status.success(), "--fork-session alone must fail");
    let fork_err = String::from_utf8_lossy(&bad_fork.stderr);
    assert!(fork_err.contains("--fork-session goes with -c or --resume"), "{fork_err}");

    // 3. `-r -2` resumes the second most recent session cleanly.
    let rel = s.run(&s.work, &["-r", "-2", "-p", "--model", "faux/echo", "--yolo", "second-latest"]);
    assert!(rel.status.success(), "-r -2 failed: {}", String::from_utf8_lossy(&rel.stderr));

    // 4. `-c --all` from another directory continues the most recent session across directories.
    let cont_all = s.run(&s.other, &["-c", "--all", "-p", "--model", "faux/echo", "--yolo", "from-other"]);
    assert!(cont_all.status.success(), "-c --all failed: {}", String::from_utf8_lossy(&cont_all.stderr));
}

use super::*;

fn shell_write(path: &std::path::Path, contents: &str) -> Value {
    if cfg!(windows) {
        let path = path.to_string_lossy().replace('\'', "''");
        json!({ "name": "powershell", "arguments": { "command": format!(
            "Set-Content -NoNewline -LiteralPath '{path}' -Value '{contents}'"
        ) } })
    } else {
        json!({ "name": "bash", "arguments": { "command": format!("printf '{contents}' > {path:?}") } })
    }
}

#[test]
fn acp_new_session_additional_directories_valid_write_shell_dedup_and_isolation() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra = root.join("extra");
    let ungranted = root.join("ungranted");
    let marker = root.join("evil-loaded.txt");
    for d in [&home, &cwd, &extra, &ungranted] {
        std::fs::create_dir_all(d).unwrap();
    }
    // A `.mare/` plugin inside an additional directory must NEVER execute or grant project trust.
    std::fs::create_dir_all(extra.join(".mare/plugins")).unwrap();
    std::fs::write(
        extra.join(".mare/plugins/evil.lua"),
        format!(
            "mare.fs.write({:?}, 'pwned')\nreturn {{ name = 'evil', apply = function() end }}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    std::fs::write(
        extra.join(".mare/init.lua"),
        format!("mare.fs.write({:?}, 'pwned-init')\n", marker.to_string_lossy()),
    )
    .unwrap();

    let extra_write = extra.join("from-write.txt");
    let extra_bash = extra.join("from-bash.txt");
    let ungranted_write = ungranted.join("denied-write.txt");
    let ungranted_bash = ungranted.join("denied-bash.txt");
    let session2_write = extra.join("session2-denied.txt");

    let script = json!([
        { "tool_calls": [{ "name": "write", "arguments": { "path": extra_write, "content": "ok-write" } }] },
        { "tool_calls": [shell_write(&extra_bash, "ok-bash")] },
        { "tool_calls": [{ "name": "write", "arguments": { "path": ungranted_write, "content": "no" } }] },
        { "tool_calls": [shell_write(&ungranted_bash, "no")] },
        { "text": "session-1-done" },
        { "tool_calls": [{ "name": "write", "arguments": { "path": session2_write, "content": "isolated" } }] },
        { "text": "session-2-done" }
    ]);

    let mut rpc = acp_in_home(&home, script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    // Empty additionalDirectories is valid too, and non-empty deduplicates extra + cwd.
    let s1 = rpc.call_raw(
        "session/new",
        json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra, &extra, &cwd] }),
        |_| json!(null),
    );
    let sid1 = s1["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s1}")).to_string();

    let p1 = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "exercise extra dir" }] }),
    );
    let (r1, _) = read_until(&mut rpc, p1, Some(&sid1));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

    assert_eq!(std::fs::read_to_string(&extra_write).unwrap(), "ok-write");
    assert_eq!(std::fs::read_to_string(&extra_bash).unwrap(), "ok-bash");
    assert!(!ungranted_write.exists(), "ungranted sibling must remain non-writable via write");
    assert!(!ungranted_bash.exists(), "ungranted sibling must remain non-writable via bash");
    assert!(!marker.exists(), ".mare plugins in additionalDirectories must not load or run");

    // Second session with empty `additionalDirectories: []` must NOT inherit `extra`.
    let s2 = rpc
        .call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [] }), |_| json!(null));
    let sid2 = s2["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s2}")).to_string();
    let p2 = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "isolated session" }] }),
    );
    let (r2, _) = read_until(&mut rpc, p2, Some(&sid2));
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");
    assert!(!session2_write.exists(), "second session must not inherit previous additionalDirectories");

    rpc.kill_and_wait();
}

#[test]
fn acp_new_session_additional_directories_read_only_stays_non_writable() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra = root.join("extra");
    for d in [&home, &cwd, &extra] {
        std::fs::create_dir_all(d).unwrap();
    }
    let blocked_write = extra.join("blocked.txt");
    let blocked_bash = extra.join("blocked-bash.txt");
    let readable = extra.join("note.txt");
    std::fs::write(&readable, "read-ok").unwrap();

    let script = json!([
        { "tool_calls": [{ "name": "read", "arguments": { "path": readable } }] },
        { "tool_calls": [{ "name": "write", "arguments": { "path": blocked_write, "content": "nope" } }] },
        { "tool_calls": [shell_write(&blocked_bash, "nope")] },
        { "text": "ro-done" }
    ]);
    let mut rpc = acp_in_home(&home, script, &["--permission", "read-only", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s =
        rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra] }), |_| {
            json!(null)
        });
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let pid = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read only check" }] }),
    );
    let (resp, updates) = read_until(&mut rpc, pid, Some(&sid));
    assert_eq!(resp["result"]["stopReason"], "end_turn", "{resp}");
    assert!(!blocked_write.exists(), "read-only mode must keep additionalDirectories non-writable");
    assert!(!blocked_bash.exists(), "read-only mode bash must keep additionalDirectories non-writable");
    assert!(
        updates.iter().any(|u| format!("{u}").contains("read-ok")),
        "reading from additionalDirectories should succeed under read-only: {updates:?}"
    );

    rpc.kill_and_wait();
}

#[cfg(unix)]
#[test]
fn acp_new_session_additional_directories_rejects_invalid_and_atomic_no_session() {
    use std::os::unix::fs::symlink;

    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let valid_extra = root.join("valid-extra");
    let outside_target = root.join("outside-target");
    let leaf_symlink = root.join("leaf-link");
    let ancestor_real = root.join("ancestor-real");
    let ancestor_sub = ancestor_real.join("sub");
    let ancestor_link = root.join("ancestor-link");
    let regular_file = root.join("not-a-dir.txt");
    let dot_mare = cwd.join(".mare");
    for d in [&home, &cwd, &valid_extra, &outside_target, &ancestor_sub, &dot_mare] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(&regular_file, "file").unwrap();
    symlink(&outside_target, &leaf_symlink).unwrap();
    symlink(&ancestor_real, &ancestor_link).unwrap();

    let mut rpc =
        acp_in_home(&home, json!([{ "text": "unused" }]), &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let bad_cases = vec![
        ("missing", vec![json!(root.join("missing-dir"))]),
        ("regular-file", vec![json!(regular_file)]),
        ("relative", vec![json!("relative/dir")]),
        ("leaf-symlink", vec![json!(leaf_symlink)]),
        ("ancestor-symlink", vec![json!(ancestor_link.join("sub"))]),
        ("mixed-valid-and-invalid", vec![json!(valid_extra), json!(root.join("missing-second"))]),
        ("protected-dot-mare", vec![json!(dot_mare)]),
    ];

    for (label, dirs) in bad_cases {
        let resp =
            rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": dirs }), |_| {
                json!(null)
            });
        assert_eq!(resp["error"]["code"], -32602, "{label}: expected -32602, got {resp}");
    }

    let listed = rpc.call_raw("session/list", json!({ "cwd": cwd }), |_| json!(null));
    let sessions = listed["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{listed}"));
    assert!(sessions.is_empty(), "failed session/new must leave zero persisted sessions: {listed}");

    rpc.kill_and_wait();
}

#[cfg(unix)]
#[test]
fn acp_restore_accepts_recorded_additional_directories_and_rejects_unrecorded() {
    use std::os::unix::fs::symlink;

    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cfg_dir = home.join("config");
    let cwd = root.join("cwd");
    let keep = root.join("keep");
    let will_delete = root.join("will-delete");
    let will_swap = root.join("will-swap");
    let swap_target = root.join("swap-target");
    for d in [&home, &cfg_dir, &cwd, &keep, &will_delete, &will_swap, &swap_target] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(cfg_dir.join("init.lua"), "mare.setup { permission = \"auto\" }\n").unwrap();

    // Phase 1: create session with [keep, will_delete, will_swap] and run a prompt to persist it.
    let sid = {
        let mut rpc = acp_in_home(
            &home,
            json!([{ "text": "persisted-turn-1" }]),
            &["-C", cwd.to_str().unwrap(), "--trust", "--approval-policy", "never"],
            &[],
        );
        let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
        let s = rpc.call_raw(
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [&keep, &will_delete, &will_swap] }),
            |_| json!(null),
        );
        let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
        let p = rpc.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "persist me" }] }),
        );
        let (r, _) = read_until(&mut rpc, p, Some(&sid));
        assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
        let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
        assert!(closed.get("result").is_some(), "{closed}");
        rpc.kill_and_wait();
        sid
    };

    // Mutate filesystem between sessions: remove `will_delete` and replace `will_swap` with a symlink.
    std::fs::remove_dir_all(&will_delete).unwrap();
    std::fs::remove_dir_all(&will_swap).unwrap();
    symlink(&swap_target, &will_swap).unwrap();

    // Phase 2: `session/load` & `session/resume` accept valid recorded `additionalDirectories`,
    // reject invalid/unrecorded ones, and revalidates recorded roots (`keep` stays writable, `will_swap` dropped).
    let keep_file = keep.join("after-resume.txt");
    let swap_escape = will_swap.join("escaped.txt");
    let unrecorded = root.join("unrecorded");
    std::fs::create_dir_all(&unrecorded).unwrap();
    let mut rpc = acp_in_home(
        &home,
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": keep_file, "content": "kept-ok" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": swap_escape, "content": "nope" } }] },
            { "text": "after-resume" }
        ]),
        &["-C", cwd.to_str().unwrap(), "--trust", "--approval-policy", "never"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let bad_load_unrecorded = rpc.call_raw(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&unrecorded] }),
        |_| json!(null),
    );
    assert_eq!(bad_load_unrecorded["error"]["code"], -32602, "{bad_load_unrecorded}");

    let bad_resume_unrecorded = rpc.call_raw(
        "session/resume",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&unrecorded] }),
        |_| json!(null),
    );
    assert_eq!(bad_resume_unrecorded["error"]["code"], -32602, "{bad_resume_unrecorded}");

    let bad_resume_swapped = rpc.call_raw(
        "session/resume",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&will_swap] }),
        |_| json!(null),
    );
    assert_eq!(bad_resume_swapped["error"]["code"], -32602, "{bad_resume_swapped}");

    let loaded_with_keep = rpc.call_raw(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&keep] }),
        |_| json!(null),
    );
    assert!(loaded_with_keep.get("result").is_some(), "recorded valid root must load cleanly: {loaded_with_keep}");

    let pid = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "check restored roots" }] }),
    );
    let (resp, _) = read_until(&mut rpc, pid, Some(&sid));
    assert_eq!(resp["result"]["stopReason"], "end_turn", "{resp}");

    assert_eq!(std::fs::read_to_string(&keep_file).unwrap(), "kept-ok");
    assert!(!swap_target.join("escaped.txt").exists(), "symlink-swapped root must be dropped on resume");

    rpc.kill_and_wait();
}

#[test]
fn acp_load_and_resume_restore_recorded_additional_dirs_and_respect_readonly() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra = root.join("extra");
    let unauthorised = root.join("unauthorised");
    for d in [&home, &cwd, &extra, &unauthorised] {
        std::fs::create_dir_all(d).unwrap();
    }
    let extra_file = extra.join("hello.txt");
    std::fs::write(&extra_file, "from-extra-dir").unwrap();
    let blocked_write = extra.join("ro-blocked.txt");

    // Create and persist session with `extra`.
    let sid = {
        let mut rpc = acp_in_home(
            &home,
            json!([{ "text": "seed" }]),
            &["-C", cwd.to_str().unwrap(), "--permission", "read-only", "--approval-policy", "never"],
            &[],
        );
        let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
        let s = rpc.call_raw(
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra] }),
            |_| json!(null),
        );
        let sid = s["result"]["sessionId"].as_str().unwrap().to_string();
        let p =
            rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }));
        let (r, _) = read_until(&mut rpc, p, Some(&sid));
        assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
        let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
        assert!(closed.get("result").is_some(), "{closed}");
        rpc.kill_and_wait();
        sid
    };

    // Restart ACP process: load with `additionalDirectories: [extra]` runs steps 1-3 (read succeeds,
    // write fails in read-only), then close, then resume starts a fresh host and replays steps 1-3.
    let mut rpc = acp_in_home(
        &home,
        json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": extra_file } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": blocked_write, "content": "nope" } }] },
            { "text": "after-restore" }
        ]),
        &["-C", cwd.to_str().unwrap(), "--permission", "read-only", "--approval-policy", "never"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let bad_load = rpc.call_raw(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&unauthorised] }),
        |_| json!(null),
    );
    assert_eq!(bad_load["error"]["code"], -32602, "{bad_load}");

    let loaded = rpc.call_raw(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra] }),
        |_| json!(null),
    );
    assert!(loaded.get("result").is_some(), "{loaded}");

    let p1 = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read" }] }));
    let (r1, u1) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    assert!(u1.iter().any(|u| format!("{u}").contains("from-extra-dir")), "expected read from extra dir: {u1:?}");
    assert!(!blocked_write.exists(), "read-only session must still forbid writing to extra dir");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    let bad_resume = rpc.call_raw(
        "session/resume",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&unauthorised] }),
        |_| json!(null),
    );
    assert_eq!(bad_resume["error"]["code"], -32602, "{bad_resume}");

    let resumed = rpc.call_raw(
        "session/resume",
        json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra] }),
        |_| json!(null),
    );
    assert!(resumed.get("result").is_some(), "{resumed}");

    let p2 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read2" }] }));
    let (r2, u2) = read_until(&mut rpc, p2, Some(&sid));
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");
    assert!(u2.iter().any(|u| format!("{u}").contains("from-extra-dir")), "expected read after resume: {u2:?}");

    rpc.kill_and_wait();
}

#[cfg(unix)]
#[test]
fn acp_additional_directories_toctou_symlink_swap_refused() {
    use std::os::unix::fs::symlink;

    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra = root.join("extra");
    let outside = root.join("outside");
    for d in [&home, &cwd, &extra, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }

    let first_ok = extra.join("initial.txt");
    let swapped_write = extra.join("pwned-write.txt");
    let swapped_bash = extra.join("pwned-bash.txt");
    let script = json!([
        { "tool_calls": [{ "name": "write", "arguments": { "path": first_ok, "content": "before-swap" } }] },
        { "text": "turn-1-ok" },
        { "tool_calls": [{ "name": "write", "arguments": { "path": swapped_write, "content": "pwn" } }] },
        { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("printf 'pwn' > {:?}", swapped_bash) } }] },
        { "text": "turn-2-ok" }
    ]);

    let mut rpc = acp_in_home(&home, script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s =
        rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [&extra] }), |_| {
            json!(null)
        });
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let p1 = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "t1" }] }));
    let (r1, _) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    assert_eq!(std::fs::read_to_string(&first_ok).unwrap(), "before-swap");

    // Swap `extra` to a symlink pointing outside after authorization!
    std::fs::remove_dir_all(&extra).unwrap();
    symlink(&outside, &extra).unwrap();

    let p2 = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "t2" }] }));
    let (r2, _) = read_until(&mut rpc, p2, Some(&sid));
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");
    assert!(!outside.join("pwned-write.txt").exists(), "file tool must refuse symlink-swapped additionalDirectory");
    assert!(!outside.join("pwned-bash.txt").exists(), "sandboxed bash must refuse symlink-swapped additionalDirectory");

    rpc.kill_and_wait();
}

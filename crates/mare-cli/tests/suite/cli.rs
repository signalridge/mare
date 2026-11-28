//! Command-line behaviour of the real binary: prompt from piped stdin (after Codex
//! exec/tests/suite/prompt_stdin.rs), headless output and signals, RPC input limits, config
//! warnings, `--set` validation, `mare doctor` colour / JSON, version and completion.

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::Duration;

use crate::common::*;
use serde_json::{Value, json};

#[test]
fn plugin_list_json_redacts_source_credentials() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "mare.plugins.add { { 'example/private', url = 'https://private-user:private-password@example.com/repo.git' } }\n",
    )
    .unwrap();
    let out = mare_cmd()
        .args(["plugin", "list", "--json"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let rows: Value = serde_json::from_str(&stdout).expect("JSON plugin list");
    assert_eq!(rows[0]["name"], "example/private");
    assert!(!stdout.contains("private-user"), "{stdout}");
    assert!(!stdout.contains("private-password"), "{stdout}");
}

/// `mare -p … <args>` with `stdin` piped in: (exit code, stdout, stderr).
fn headless(home: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> (Option<i32>, String, String) {
    let mut c = mare_cmd();
    c.args(["-p", "--model", "faux/echo", "--no-session"])
        .args(args)
        .env("MARE_HOME", home)
        .env_remove("NO_COLOR")
        .current_dir(home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = ChildGroup::spawn(&mut c).unwrap();
    if let Some(data) = stdin {
        let mut w = child.0.stdin.take().unwrap();
        w.write_all(data).unwrap();
    }
    let out = wait_output_timeout(child, DEFAULT_CMD_TIMEOUT);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn piped_stdin_is_appended_read_with_dash_or_alone_and_empty_input_is_handled() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    // a prompt argument + piped stdin: appended as a <stdin> block
    let (code, out, err) = headless(h, &["Summarize this"], Some(b"my output\n"));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: Summarize this\n\n<stdin>\nmy output\n</stdin>", "{out}");
    // a prompt argument + empty stdin: the prompt alone
    let (code, out, err) = headless(h, &["Summarize this"], Some(b""));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: Summarize this");
    // `-`: stdin is the prompt
    let (code, out, err) = headless(h, &["-"], Some(b"prompt from stdin\n"));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: prompt from stdin");
    // no prompt argument: piped stdin is the prompt
    let (code, out, err) = headless(h, &[], Some(b"prompt from stdin\n"));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: prompt from stdin");
    // `-` with empty stdin, and invalid UTF-8: usage errors (exit 2), said clearly
    let (code, _, err) = headless(h, &["-"], Some(b""));
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("no prompt provided via stdin"), "{err}");
    let (code, _, err) = headless(h, &["-"], Some(b"bad \xff\xfe bytes"));
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("not valid UTF-8"), "{err}");
}

/// Explicitly piped stdin arriving after >1s is read to EOF and appended as `<stdin>` (Codex
/// `exec/src/lib.rs:2643-2674`).
#[test]
fn delayed_piped_stdin_is_read_to_eof_and_appended_to_prompt() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["-p", "--model", "faux/echo", "--no-session", "Explain this"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut stdin = child.0.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        // Delay past the former 1s first-byte cutoff before sending the marker and EOF.
        // sleep-ok: deliberate producer delay to verify delayed piped stdin is not dropped
        std::thread::sleep(Duration::from_millis(1200));
        stdin.write_all(b"SLOW_STDIN_MARKER_9182\n").unwrap();
    });
    let out = wait_output_timeout(child, Duration::from_secs(30));
    writer.join().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim_end(), "echo: Explain this\n\n<stdin>\nSLOW_STDIN_MARKER_9182\n</stdin>", "{stdout}");
}

/// An unclosed stdin pipe is bounded by `--timeout`: expires with exit code 4, `--json` `timeout`
/// terminal state, and zero model requests, and any time spent reading stdin counts against the
/// same `--timeout` budget for the subsequent turn.
#[test]
fn piped_stdin_without_eof_honors_timeout() {
    let home = tempfile::tempdir().unwrap();
    let req_log = home.path().join("requests.log");
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("req_counter.lua"),
        format!(
            r#"return {{
                name = "req_counter",
                inject = {{ "agent" }},
                apply = function(ctx)
                    ctx:on("request/build", function(_, next)
                        local f = assert(io.open({:?}, "a"))
                        f:write("req\n")
                        f:close()
                        return next()
                    end)
                end,
            }}"#,
            req_log.to_string_lossy()
        ),
    )
    .unwrap();

    // 1) Write end stays open past `--timeout 300ms`: exits 4 with timeout settled record and 0 requests.
    let mut cmd = mare_cmd();
    cmd.args(["-p", "--json", "--model", "faux/echo", "--no-session", "--timeout", "300ms", "hello"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let _keep_open = child.0.stdin.take().unwrap();
    let out = wait_output_timeout(child, Duration::from_secs(10));
    drop(_keep_open);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("--timeout"), "{stderr}");
    assert!(!req_log.exists(), "provider must receive zero requests when stdin times out");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL ({e}): {l}")))
        .collect();
    assert_eq!(lines[0]["type"], "session", "{lines:?}");
    let tf = lines.iter().find(|l| l["type"] == "turn.failed").expect("turn.failed line");
    assert_eq!(tf["data"]["error"]["code"], "timed_out", "{tf}");
    let settled = lines.last().expect("settled line");
    assert_eq!(settled["type"], "settled", "{lines:?}");
    assert_eq!(settled["data"]["exit_code"], 4, "{settled}");
    assert_eq!(settled["data"]["reason"], "timeout", "{settled}");

    // 2) Remaining budget: stdin spends ~250ms before EOF; the subsequent slow stream exceeds the
    // remaining ~200ms of a 450ms total budget and exits 4 (`timeout`).
    let slow_script = home.path().join("slow.json");
    std::fs::write(&slow_script, json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]).to_string()).unwrap();
    let mut cmd2 = mare_cmd();
    cmd2.args(["-p", "--json", "--model", "faux/echo", "--no-session", "--timeout", "450ms", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &slow_script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child2 = ChildGroup::spawn(&mut cmd2).unwrap();
    let mut stdin2 = child2.0.stdin.take().unwrap();
    let writer2 = std::thread::spawn(move || {
        // sleep-ok: deliberate delay consuming part of the total --timeout budget before stdin EOF
        std::thread::sleep(Duration::from_millis(250));
        stdin2.write_all(b"partial budget stdin\n").unwrap();
    });
    let out2 = wait_output_timeout(child2, Duration::from_secs(10));
    writer2.join().unwrap();
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert_eq!(out2.status.code(), Some(4), "stdout:\n{stdout2}\nstderr:\n{stderr2}");
    let settled2: Value =
        serde_json::from_str(stdout2.lines().last().expect("settled line")).expect("valid settled JSON");
    assert_eq!(settled2["type"], "settled");
    assert_eq!(settled2["data"]["reason"], "timeout", "{settled2}");
}

/// stdout closed under a `--json` run (`| head -1`): the turn stops and mare leaves quietly —
/// no panic, no crash report, no "failed printing" noise.
#[cfg(unix)]
#[test]
fn headless_stops_quietly_when_stdout_is_closed() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    // a slow, long stream: the run would take ~20 s if it kept going for nobody
    std::fs::write(&script, json!([{ "text": "x".repeat(2000), "delay_ms": 10 }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--json", "--model", "faux/echo", "--no-session", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut out = BufReader::new(child.0.stdout.take().unwrap());
    let mut first = String::new();
    out.read_line(&mut first).unwrap();
    assert!(first.contains("\"session\""), "{first}");
    drop(out); // the reader goes away, like `head -1`
    let out = wait_output_timeout(child, Duration::from_secs(15));
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(141), "{err}");
    assert!(!err.contains("panicked") && !err.contains("crash"), "{err}");
    assert!(!home.path().join("state/crashes").exists(), "no crash report");
}

/// SIGTERM during a headless turn: the turn is stopped, plugins shut down, mare exits.
#[cfg(unix)]
#[test]
fn headless_sigterm_ends_the_run() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": "touch started; sleep 30" } }] }, { "text": "done" }])
            .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--json", "--model", "faux/echo", "--no-session", "--yolo", "go"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = ChildGroup::spawn(&mut cmd).unwrap();
    let t0 = std::time::Instant::now();
    while !home.path().join("started").exists() {
        assert!(t0.elapsed() < std::time::Duration::from_secs(30), "the command never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let out = wait_output_timeout(child, Duration::from_secs(15));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "settled", "{stdout}");
    assert_eq!(last["data"]["exit_code"], 1, "{last}");
    assert_eq!(out.status.code(), Some(1));
}

/// RPC input: a malformed line, an oversized line and an unknown method each get their error,
/// and the connection keeps working.
#[test]
fn rpc_rejects_malformed_oversized_and_unknown_requests_and_keeps_going() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut w = child.0.stdin.take().unwrap();
    let mut r = BufReader::new(child.0.stdout.take().unwrap());
    // responses only (skip notifications)
    let mut next = || loop {
        let mut l = String::new();
        assert!(r.read_line(&mut l).unwrap() > 0, "rpc closed");
        let v: Value = serde_json::from_str(&l).unwrap();
        if v.get("id").is_some() || v.get("error").is_some() {
            return v;
        }
    };
    w.write_all(b"{not json\n").unwrap();
    assert_eq!(next()["error"]["code"], -32700);
    // one line over the 64 MiB limit
    let big = vec![b'x'; 64 * 1024 * 1024 + 16];
    w.write_all(&big).unwrap();
    w.write_all(b"\n").unwrap();
    assert_eq!(next()["error"]["code"], -32600);
    w.write_all(format!("{}\n", json!({ "jsonrpc": "2.0", "id": 7, "method": "no/such" })).as_bytes()).unwrap();
    let v = next();
    assert_eq!((v["id"].as_i64(), v["error"]["code"].as_i64()), (Some(7), Some(-32601)), "{v}");
    w.write_all(format!("{}\n", json!({ "jsonrpc": "2.0", "id": 8, "method": "status.get" })).as_bytes()).unwrap();
    let v = next();
    assert_eq!((v["id"].as_i64(), v["error"]["code"].as_i64()), (Some(8), Some(-32600)), "{v}");
    w.write_all(
        format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 9, "method": "initialize", "params": { "protocol_version": "3.0" } })
        )
        .as_bytes(),
    )
    .unwrap();
    let v = next();
    assert_eq!(v["id"], 9, "{v}");
    assert!(v.get("result").is_some(), "initialize failed after bad input: {v}");
    w.write_all(format!("{}\n", json!({ "jsonrpc": "2.0", "id": 10, "method": "status.get" })).as_bytes()).unwrap();
    let v = next();
    assert_eq!(v["id"], 10, "{v}");
    assert!(v.get("result").is_some(), "still serving after bad input: {v}");
    drop(w);
    let _ = wait_output_timeout(child, DEFAULT_CMD_TIMEOUT);
}

/// Headless runs print configuration warnings on stderr; `--strict-config` refuses to start.
#[test]
fn headless_prints_config_warnings_and_strict_config_refuses() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.o.scrol_lines = 5\n").unwrap();
    let (code, out, err) = headless(home.path(), &["hi"], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert!(err.contains("config warning") && err.contains("scrol_lines"), "{err}");
    let (code, _, err) = headless(home.path(), &["--strict-config", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--strict-config") && err.contains("scrol_lines"), "{err}");
}

/// A plugin definition's removed field is a configuration warning like an unknown option: on
/// stderr in a headless run (which still answers), refused under `--strict-config`.
#[test]
fn headless_warns_about_a_removed_definition_field_and_strict_config_refuses() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("legacy-def.lua"),
        "return { name = 'legacy-def', optional = { 'jobs' }, apply = function() end }",
    )
    .unwrap();
    let (code, out, err) = headless(home.path(), &["hi"], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert!(err.contains("config warning") && err.contains("`legacy-def`") && err.contains("`optional`"), "{err}");
    let (code, _, err) = headless(home.path(), &["--strict-config", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--strict-config") && err.contains("`optional`"), "{err}");
}

fn setup_invocation_override_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(home.path().join("config/init.lua"), "mare.options.custom_switch = { type = 'boolean' }\n").unwrap();
    let dir = home.path().join("config/plugins/probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        "return { name='probe', config_schema={count={type='integer', default=1, desc='a count'}}, apply=function() end }",
    )
    .unwrap();
    home
}

fn assert_set_override_warning(home: &std::path::Path, key: &str, value: &str, warning: &str, non_strict_code: i32) {
    for strict in [false, true] {
        let mut cmd = mare_cmd();
        cmd.args(["-p", "--model", "faux/echo", "--no-session"])
            .env("MARE_HOME", home)
            .current_dir(home)
            .stdin(Stdio::null());
        if strict {
            cmd.arg("--strict-config");
        }
        cmd.args(["--set", &format!("{key}={value}"), "hi"]);
        let output = cmd.run_output();
        let err = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(if strict { 2 } else { non_strict_code }), "{key}: {err}");
        assert!(err.contains(warning) && err.contains("cli"), "{key}: {err}");
    }
}

/// Unknown and scalar `--set` overrides warn in default mode and exit 2 under `--strict-config`.
#[test]
fn invocation_set_unknown_and_scalar_overrides_warn_and_strict_refuses() {
    let home = setup_invocation_override_home();
    for (key, value, warning, non_strict_code) in [
        ("modle", "typo", "unknown option `modle`", 0),
        ("custom_switch", "oops", "option `custom_switch` expects a boolean, got string", 0),
        ("reduce_motion", "oops", "option `reduce_motion` expects a boolean, got string", 0),
        ("editor", "42", "option `editor` expects a string, got number", 0),
    ] {
        assert_set_override_warning(home.path(), key, value, warning, non_strict_code);
    }
}

/// Union and retired `--set` overrides warn in default mode and exit 2 under `--strict-config`.
#[test]
fn invocation_set_union_and_retired_overrides_warn_and_strict_refuses() {
    let home = setup_invocation_override_home();
    for (key, value, warning, non_strict_code) in [
        ("logo", "42", "option `logo` expects a boolean or string, got number", 0),
        ("peer", "42", "option `peer` expects a boolean or table, got number", 0),
        ("permission", "auto", "option `permission` has been retired; use `plugins.permissions.preset` instead", 0),
        ("working_indicator", "42", "option `working_indicator` has been retired; use `ui.working` instead", 0),
    ] {
        assert_set_override_warning(home.path(), key, value, warning, non_strict_code);
    }
}

/// Nested table and plugin schema `--set` overrides warn in default mode and exit 2 under `--strict-config`.
#[test]
fn invocation_set_nested_and_plugin_overrides_warn_and_strict_refuses() {
    let home = setup_invocation_override_home();
    for (key, value, warning, non_strict_code) in [
        ("ui.working", "42", "option `ui.working` expects a table, got number", 0),
        ("plugins", r#"{"probe":{"count":"oops"}}"#, "plugin `probe` option `count` expects an integer", 0),
        ("plugins", r#"{"probe":{"coutn":3}}"#, "plugin `probe` has no option `coutn`", 0),
    ] {
        assert_set_override_warning(home.path(), key, value, warning, non_strict_code);
    }
}

/// An invalid restrictive approval policy never falls back to `ask`: startup fails (exit 1) when
/// schema-rejected or incomplete, and `--strict-config` refuses schema warnings earlier (exit 2).
#[test]
fn invocation_set_invalid_approval_policy_refuses_startup_and_strict() {
    let home = setup_invocation_override_home();
    for (value, warning) in [
        ("42", "plugin `approvals` option `policy` matches none of its forms"),
        ("sometimes", "plugin `approvals` option `policy` matches none of its forms"),
    ] {
        assert_set_override_warning(home.path(), "plugins.approvals.policy", value, warning, 1);
    }
    // A granular table the schema accepts but the approvals plugin cannot use (no categories)
    // also fails startup instead of running with `ask`, strict or not.
    for strict in [false, true] {
        let mut args = vec!["--set", r#"plugins.approvals.policy={"mode":"granular"}"#, "hi"];
        if strict {
            args.insert(0, "--strict-config");
        }
        let (code, out, err) = headless(home.path(), &args, None);
        assert_eq!(code, Some(1), "strict={strict}: {err}");
        assert!(!out.contains("echo: hi") && err.contains("approval policy must be"), "strict={strict}: {err}");
    }
}

/// Valid `--set` overrides pass cleanly under `--strict-config`, and invalid env overrides warn or exit 2.
#[test]
fn invocation_valid_strict_overrides_and_env_warnings() {
    let home = setup_invocation_override_home();
    let (code, out, err) = headless(
        home.path(),
        &[
            "--strict-config",
            "--set",
            "custom_switch=true",
            "--set",
            "watchdog_ms=10000",
            "--set",
            "plugins.approvals.policy=ask",
            "--set",
            "logo=compact",
            "--set",
            "peer=false",
            "--set",
            "ui.working.label=dots",
            "hi",
        ],
        None,
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert!(!err.contains("config warning"), "{err}");
    let (code, out, err) = headless(
        home.path(),
        &[
            "--strict-config",
            "--set",
            r#"plugins.approvals.policy={"mode":"granular","categories":{"tool":true}}"#,
            "hi",
        ],
        None,
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert!(!err.contains("config warning"), "{err}");
    for (var, val, expected, non_strict_code) in [("MARE_REDUCE_MOTION", "oops", "expects a boolean", 0)] {
        for strict in [false, true] {
            let mut cmd = mare_cmd();
            cmd.args(["-p", "--model", "faux/echo", "--no-session"])
                .env("MARE_HOME", home.path())
                .env(var, val)
                .current_dir(home.path())
                .stdin(Stdio::null());
            if strict {
                cmd.arg("--strict-config");
            }
            let output = cmd.arg("hi").run_output();
            let err = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(if strict { 2 } else { non_strict_code }), "{var}: {err}");
            assert!(err.contains(expected) && err.contains(var), "{var}: {err}");
        }
    }
}

#[test]
fn strict_config_accepts_declared_optional_plugin_options_and_rejects_wrong_types() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config/plugins/optional-probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        r#"return { name="optional-probe", config_schema={
          count={type="integer", default=1, desc="a count"},
          endpoint={type="string", desc="an optional endpoint"},
          prelude={type={"string","table"}, desc="a string or a list"},
        }, apply=function() end }"#,
    )
    .unwrap();
    for (key, value) in [
        ("provider-openai.base_url", "http://127.0.0.1:1"),
        ("approval-review-model.model", "faux/echo"),
        ("optional-probe.endpoint", "local"),
        ("optional-probe.prelude", r#"["one","two"]"#),
    ] {
        let assignment = format!("plugins.{key}={value}");
        let (code, out, err) = headless(home.path(), &["--strict-config", "--set", &assignment, "hi"], None);
        assert_eq!(code, Some(0), "{key}: {err}");
        assert_eq!(out.trim_end(), "echo: hi");
        assert!(!err.contains("config warning"), "{key}: {err}");
    }
    for key in ["provider-openai.base_url", "optional-probe.endpoint", "optional-probe.prelude"] {
        let assignment = format!("plugins.{key}=42");
        let (code, _, err) = headless(home.path(), &["--strict-config", "--set", &assignment, "hi"], None);
        assert_eq!(code, Some(2), "{key}: {err}");
        assert!(err.contains("expects a string") && err.contains("got number"), "{key}: {err}");
    }
    // Replacing a plugin also replaces its declarations; built-in optional keys do not leak.
    let replacement = home.path().join("config/plugins/provider-openai");
    std::fs::create_dir_all(&replacement).unwrap();
    std::fs::write(
        replacement.join("init.lua"),
        "return {name='provider-openai', override=true, config_schema={count={type='integer', default=1, desc='a count'}}, apply=function() end}",
    )
    .unwrap();
    let (code, _, err) =
        headless(home.path(), &["--strict-config", "--set", "plugins.provider-openai.base_url=local", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("has no option `base_url`"), "{err}");
}

#[test]
fn strict_config_rpc_startup_rejects_invocation_type_warnings() {
    let home = tempfile::tempdir().unwrap();
    for (set_arg, expected) in
        [("editor=42", "expects a string"), ("plugins.approvals.policy=42", "matches none of its forms")]
    {
        let output = mare_cmd()
            .args(["--rpc", "--strict-config", "--model", "faux/echo", "--set", set_arg, "--no-session"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let err = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{set_arg}: {err}");
        assert!(err.contains(expected) && err.contains("cli") && err.contains("--strict-config"), "{set_arg}: {err}");
    }
    let output = mare_cmd()
        .args(["--rpc", "--strict-config", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_REDUCE_MOTION", "oops")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{err}");
    assert!(
        err.contains("expects a boolean") && err.contains("MARE_REDUCE_MOTION") && err.contains("--strict-config"),
        "{err}"
    );
}

#[test]
fn strict_config_clean_startup_checks_cli_overrides() {
    let home = tempfile::tempdir().unwrap();
    let (code, _, err) = headless(home.path(), &["--clean", "--strict-config", "--set", "modle=typo", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("unknown option `modle`") && err.contains("cli"), "{err}");
}

/// a mare started from a tool's shell inherits the outer session's
/// `MARE_SESSION_MODEL`; that is a fact for the child, not the `MARE_<OPTION>` config layer, so
/// the nested mare keeps its own init.lua model (with the old `MARE_MODEL` name it took the
/// outer one, which outranks init.lua).
#[test]
fn a_nested_mare_does_not_take_the_injected_session_model_as_its_model() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
    let out = mare_cmd()
        .args(["-p", "--no-session", "hi"])
        .env("MARE_HOME", home.path())
        .env("MARE_SESSION_MODEL", "outer-provider/outer-model")
        .env("MARE_SESSION_EFFORT", "max")
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(stdout.trim_end(), "echo: hi", "{stderr}");
    assert!(!stderr.contains("outer"), "{stderr}");
}

/// a non-essential plugin that fails to start is a warning on stderr; the headless
/// run goes on without it.
#[test]
fn headless_warns_about_a_failed_optional_plugin_and_runs() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config/plugins/broken_optional");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        "return { name = 'broken_optional', apply = function() error('optional sentinel') end }",
    )
    .unwrap();
    // a definition error too: one line each, nothing raw (no loader text, no traceback)
    std::fs::write(home.path().join("config/plugins/no_return.lua"), "local x = 1\n").unwrap();
    let (code, out, err) = headless(home.path(), &["hi"], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert_eq!(err.matches("mare: warning: plugin `broken_optional` setup failed").count(), 1, "{err}");
    assert!(err.contains("optional sentinel") && err.contains("/reload"), "{err}");
    assert_eq!(err.matches("mare: warning: plugin `no_return` definition failed").count(), 1, "{err}");
    assert!(!err.contains("stack traceback") && !err.contains("loading plugin"), "{err}");
    assert_eq!(err.lines().count(), 2, "{err}");
}

/// headless has no way to fix a broken init.lua, so it reports it and exits non-zero;
/// the escape hatch `--clean` (`nvim --clean`, `wezterm --skip-config`) skips user config.
#[test]
fn a_broken_init_lua_stops_headless_and_clean_skips_it() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(cfg.join("plugins")).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup {\n").unwrap();
    std::fs::write(cfg.join("plugins/boom.lua"), "return { name = 'boom', apply = function() error('x') end }")
        .unwrap();
    let (code, out, err) = headless(home.path(), &["hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(out, "");
    assert!(err.contains("init.lua has an error") && err.contains("mare --clean"), "{err}");
    let (code, out, err) = headless(home.path(), &["--clean", "hi"], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.trim_end(), "echo: hi");
    assert!(!err.contains("warning"), "nothing of the user config ran: {err}");
}

/// `--set` without `=` or with broken JSON is a usage error, also in `--json` (one settled line).
#[test]
fn invalid_set_values_are_usage_errors() {
    let home = tempfile::tempdir().unwrap();
    let (code, _, err) = headless(home.path(), &["--set", "foo", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("KEY=VALUE"), "{err}");
    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "--set", "plugins.x={broken", "hi"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!((last["type"].as_str(), last["data"]["reason"].as_str()), (Some("settled"), Some("usage")), "{last}");
    assert_eq!(last["data"]["error"]["code"], "invalid_arguments", "{last}");
    assert!(last["data"]["error"]["message"].as_str().unwrap().contains("invalid JSON"), "{last}");
}

/// An absent setup is informational by default, but cannot satisfy the enhanced CI gate.
#[test]
fn sandbox_status_requires_a_self_test_only_when_requested() {
    let home = tempfile::tempdir().unwrap();
    for (extra, expected) in [(None, 0), (Some("--require-full"), 1)] {
        let out = mare_cmd()
            .args(["sandbox", "status"])
            .args(extra)
            .env("MARE_HOME", home.path())
            .env("MARE_SANDBOX_DIR", home.path().join("not-installed"))
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(expected), "{text}; stderr={:?}", out.stderr);
        assert!(text.contains("setup: not installed"), "{text}");
        assert!(text.contains("self-test: skipped"), "{text}");
    }
}

/// `mare doctor`: no ANSI colours when stdout isn't a terminal or NO_COLOR is set; `--json`.
#[test]
fn doctor_has_no_colour_off_a_terminal_and_a_json_mode() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = mare_cmd()
            .arg("doctor")
            .args(args)
            .env("MARE_HOME", home.path())
            .env("NO_COLOR", "1")
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let text = run(&[]);
    assert!(text.contains("doctor") && !text.contains('\x1b'), "{text:?}");
    let doc: Value = serde_json::from_str(&run(&["--json"])).unwrap();
    let checks = doc["checks"].as_array().unwrap();
    assert!(checks.iter().any(|c| c["name"] == "sandbox" && c["ok"].is_boolean()), "{doc}");
    assert_eq!(doc["ok"], doc["failures"] == 0);
}

#[test]
fn version_is_long_and_completion_scripts_are_generated() {
    let out = mare_cmd().arg("--version").run_output();
    let v = String::from_utf8_lossy(&out.stdout);
    assert!(v.contains("commit:") && v.contains("target:"), "{v}");
    let head = fixture_git(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .unwrap();
    if head.status.success() {
        let expected = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert!(v.contains(&format!("commit: {expected}")), "the binary must report its source revision: {v}");
    }
    let out = mare_cmd().args(["completion", "bash"]).run_output();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("_mare"), "bash completion");
    // --timeout is for headless runs only
    let out = mare_cmd().args(["--rpc", "--timeout", "5s"]).stdin(Stdio::null()).run_output();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn extension_cli_flags_discover_help_parse_and_respect_trust_and_no_plugins() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    let plug_dir = cfg.join("plugins/ext-flags");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        r#"
mare.flags.register("init-mode", { type = "string", description = "Init mode", default = "fast" })
"#,
    )
    .unwrap();
    // Declarative plugin flags + print flag values into stdout marker via RPC or headless hook?
    // Even cleaner: register a slash command or read flags in apply & expose via custom rpc/status or check via --rpc flags.list/get!
    std::fs::write(
        plug_dir.join("init.lua"),
        r#"
return {
    name = "ext-flags",
    flags = {
        ["plugin-bool"] = { type = "boolean", description = "Enable feature", default = false },
        ["plugin-str"] = { type = "string", description = "Target label", default = "default-label" },
    },
    apply = function(ctx)
        assert(mare.flags.get("plugin-bool") ~= nil)
        assert(mare.flags.get("init-mode") ~= nil)
        mare.g.ext_bool = mare.flags.get("plugin-bool")
        mare.g.ext_str = mare.flags.get("plugin-str")
        mare.g.init_mode = mare.flags.get("init-mode")
    end,
}
"#,
    )
    .unwrap();

    // 1. `--help` displays "Plugin CLI Flags" with registered flags.
    let help_out = mare_cmd()
        .arg("--help")
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(help_out.status.success());
    let help = String::from_utf8_lossy(&help_out.stdout);
    assert!(help.contains("Plugin CLI Flags"), "{help}");
    assert!(help.contains("--init-mode <VALUE>"), "{help}");
    assert!(help.contains("--plugin-bool"), "{help}");
    assert!(help.contains("--plugin-str <VALUE>"), "{help}");

    // 2. Passing extension flags reaches runtime `flags.get` / `flags.list` via `--rpc`.
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--plugin-bool", "--plugin-str", "canary"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    assert!(init.get("result").is_some(), "{init}");
    let res = rpc.call_raw("flags.get", json!({ "name": "plugin-bool" }), |_| json!(null));
    assert_eq!(res["result"]["registered"], true, "{res}");
    assert_eq!(res["result"]["value"], true, "{res}");
    let res = rpc.call_raw("flags.get", json!({ "name": "plugin-str" }), |_| json!(null));
    assert_eq!(res["result"]["value"], "canary", "{res}");
    let res = rpc.call_raw("flags.get", json!({ "name": "init-mode" }), |_| json!(null));
    assert_eq!(res["result"]["value"], "fast", "{res}");
    let res = rpc.call_raw("flags.get", json!({ "name": "missing-flag" }), |_| json!(null));
    assert_eq!(res["result"]["registered"], false, "{res}");
    assert!(res["result"].get("value").is_none(), "{res}");
    let list = rpc.call_raw("flags.list", json!({}), |_| json!(null));
    let names: Vec<&str> =
        list["result"]["data"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str()).collect();
    assert_eq!(names, vec!["init-mode", "plugin-bool", "plugin-str"]);
    rpc.shutdown_and_wait();

    // 3. `--clean` does not discover user flags, so `--plugin-bool` fails as unknown.
    let (code, _, err) = headless(home.path(), &["--clean", "--plugin-bool", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--plugin-bool"), "{err}");

    // 4. Untrusted project `.mare/` flags are NOT discovered without `--trust`.
    let proj = home.path().join("untrusted-proj");
    std::fs::create_dir_all(proj.join(".mare/plugins/proj-flag")).unwrap();
    std::fs::write(
        proj.join(".mare/plugins/proj-flag/init.lua"),
        r#"return { name = "proj-flag", flags = { ["proj-only"] = { type = "boolean" } } }"#,
    )
    .unwrap();
    let denied = mare_cmd()
        .args(["-C", proj.to_str().unwrap(), "--proj-only", "--json", "-p", "--model", "faux/echo", "hi"])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(denied.status.code(), Some(2));
    let approved = mare_cmd()
        .args([
            "-C",
            proj.to_str().unwrap(),
            "--trust",
            "--proj-only",
            "-p",
            "--model",
            "faux/echo",
            "--no-session",
            "hi",
        ])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(approved.status.success(), "{}", String::from_utf8_lossy(&approved.stderr));
}

/// (:1007, :1009): invalid extension CLI flags (missing string value, value on a
/// boolean flag, unknown flag) fail with exit code 2, and under `--json` emit a final
/// `settled` record with `exit_code == 2`.
/// Ref: Kimi Code `packages/node-sdk/test/config.test.ts:34-59`; Codex `codex-rs/cli/src/exec_args_tests.rs:8`.
#[test]
fn extension_cli_flags_reject_missing_values_type_errors_and_emit_settled_on_json() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    let plug_dir = cfg.join("plugins/ext-flags");
    std::fs::create_dir_all(&plug_dir).unwrap();
    std::fs::write(
        plug_dir.join("init.lua"),
        r#"return {
    name = "ext-flags",
    flags = {
        ["plugin-bool"] = { type = "boolean", description = "Enable feature", default = false },
        ["plugin-str"] = { type = "string", description = "Target label", default = "dev" },
    },
}"#,
    )
    .unwrap();

    // 1. Missing value for string extension flag -> exit 2.
    let (code, _, err) = headless(home.path(), &["--plugin-str"], None);
    assert_eq!(code, Some(2), "missing string value must exit 2: {err}");
    assert!(err.contains("--plugin-str"), "{err}");

    // 2. Passing an explicit value to a boolean extension flag -> exit 2.
    let (code, _, err) = headless(home.path(), &["--plugin-bool=yes", "hi"], None);
    assert_eq!(code, Some(2), "value on boolean flag must exit 2: {err}");

    // 3. Unknown flag -> exit 2.
    let (code, _, err) = headless(home.path(), &["--does-not-exist", "hi"], None);
    assert_eq!(code, Some(2), "unknown flag must exit 2: {err}");
    assert!(err.contains("--does-not-exist"), "{err}");

    // 4. With `--json`, usage errors still exit 2 and emit a valid `settled` JSON line.
    for args in [&["--json", "-p", "--plugin-str"][..], &["--json", "-p", "--does-not-exist", "hi"][..]] {
        let out = mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(out.status.code(), Some(2), "args={args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let settled: Value = serde_json::from_str(stdout.lines().last().unwrap_or(""))
            .unwrap_or_else(|e| panic!("expected valid settled JSON on stdout for {args:?} ({e}): stdout={stdout:?}"));
        assert_eq!(settled["type"], "settled", "{settled}");
        assert_eq!(settled["data"]["exit_code"], 2, "{settled}");
        assert_eq!(settled["data"]["reason"], "usage", "{settled}");
        assert_eq!(settled["data"]["error"]["code"], "invalid_arguments", "{settled}");
        assert!(settled["data"]["error"]["message"].as_str().is_some_and(|s| !s.is_empty()), "{settled}");
    }
}

#[test]
fn token_rotate_creates_0600_token_and_rotates_without_printing_secret() {
    let assert_rotate_pair_at = |home_path: &std::path::Path, extra_env: &[(&str, &str)], extra_args: &[&str]| {
        let token_path = home_path.join("state").join("host").join("token");

        let mut args1 = vec!["token", "rotate"];
        args1.extend_from_slice(extra_args);
        let mut cmd1 = mare_cmd();
        cmd1.args(&args1).env("MARE_HOME", home_path).stdin(Stdio::null());
        for (k, v) in extra_env {
            cmd1.env(k, v);
        }
        let out1 = cmd1.run_output();
        assert!(out1.status.success(), "{}", String::from_utf8_lossy(&out1.stderr));
        let tok1 = std::fs::read_to_string(&token_path).unwrap();
        assert_eq!(tok1.len(), 43);
        assert!(!String::from_utf8_lossy(&out1.stdout).contains(&tok1));
        assert!(!String::from_utf8_lossy(&out1.stderr).contains(&tok1));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(token_path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }

        let mut cmd2 = mare_cmd();
        cmd2.args(&args1).env("MARE_HOME", home_path).stdin(Stdio::null());
        for (k, v) in extra_env {
            cmd2.env(k, v);
        }
        let out2 = cmd2.run_output();
        assert!(out2.status.success(), "{}", String::from_utf8_lossy(&out2.stderr));
        let tok2 = std::fs::read_to_string(&token_path).unwrap();
        assert_eq!(tok2.len(), 43);
        assert_ne!(tok1, tok2);
        assert!(!String::from_utf8_lossy(&out2.stdout).contains(&tok2));
        assert!(!String::from_utf8_lossy(&out2.stderr).contains(&tok2));
    };

    let assert_rotate_pair = |extra_env: &[(&str, &str)], extra_args: &[&str]| {
        let home = tempfile::tempdir().unwrap();
        assert_rotate_pair_at(home.path(), extra_env, extra_args);
    };

    let assert_refused_at = |home_path: &std::path::Path, extra_env: &[(&str, &str)], extra_args: &[&str]| {
        let token_path = home_path.join("state").join("host").join("token");
        let mut args = vec!["token", "rotate"];
        args.extend_from_slice(extra_args);
        let mut cmd = mare_cmd();
        cmd.args(&args).env("MARE_HOME", home_path).stdin(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.run_output();
        assert!(!out.status.success(), "expected refusal, got success");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("SANDBOX_INSUFFICIENT"), "{stderr}");
        assert!(stderr.contains("fs_host_protect"), "{stderr}");
        assert!(!token_path.exists(), "refused token rotate must not create {}", token_path.display());
    };

    let assert_refused = |extra_env: &[(&str, &str)], extra_args: &[&str]| {
        let home = tempfile::tempdir().unwrap();
        assert_refused_at(home.path(), extra_env, extra_args);
    };

    // 1. Default host backend: evaluate `host_protect_available()` against the exact same
    //    `MARE_HOME` passed to the `mare token rotate` subprocess.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let env = rt.block_on(TestEnv::new());
    let default_home = env.home.path().to_path_buf();
    if mare_binding::ossandbox::protected::host_protect_available() {
        assert_rotate_pair_at(&default_home, &[], &[]);
    } else {
        assert_refused_at(&default_home, &[], &[]);
        assert_rotate_pair_at(&default_home, &[], &["--accept-partial", "fs_host_protect"]);
    }
    drop(env);

    // 2. All platforms: narrowed caps without fs_host_protect strictly refuse unless explicitly accepted.
    let no_host_caps = [("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket")];
    assert_refused(&no_host_caps, &[]);
    assert_refused(&no_host_caps, &["--accept-partial", "fs_protect"]);
    assert_rotate_pair(&no_host_caps, &["--accept-partial", "fs_host_protect"]);

    // 3. Linux Landlock backend (if available): strictly refuses without --accept-partial, succeeds with it.
    #[cfg(target_os = "linux")]
    if mare_binding::ossandbox::landlock_abi().is_some() {
        let ll_env = [("MARE_SANDBOX_BACKEND", "landlock")];
        assert_refused(&ll_env, &[]);
        assert_rotate_pair(&ll_env, &["--accept-partial", "fs_host_protect"]);
    }
}

/// `-C/dir` is clap's attached short value: approving the project at `dir` must evaluate that
/// project's configuration, never the untrusted current directory's.
#[test]
fn attached_short_cwd_approves_that_project_and_never_runs_the_current_directorys_config() {
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for (dir, mark) in [(a.path(), "a-ran"), (b.path(), "b-ran")] {
        std::fs::create_dir_all(dir.join(".mare")).unwrap();
        let marker = dir.join(mark);
        std::fs::write(
            dir.join(".mare/init.lua"),
            format!("local f = io.open({:?}, 'w'); f:write('x'); f:close()\n", marker.display().to_string()),
        )
        .unwrap();
    }
    let out = mare_cmd()
        .current_dir(a.path())
        .args([format!("-C{}", b.path().display()), "--trust".into(), "--help".into()])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!a.path().join("a-ran").exists(), "the unapproved current directory's init.lua ran");
    assert!(b.path().join("b-ran").exists(), "the approved -C project was not the one evaluated");
}

/// Non-UTF-8 `-C` / `--cwd=` arguments must not panic in `main.rs` startup dispatch or in
/// `cli.rs` parse-error handling (lazy#50 #3).
#[cfg(unix)]
#[test]
fn non_utf8_cwd_args_do_not_panic_in_startup_or_parse_error() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let home = tempfile::tempdir().unwrap();
    for arg in [OsStr::from_bytes(b"-C/tmp/nonutf8-\xff"), OsStr::from_bytes(b"--cwd=/tmp/nonutf8-\xff")] {
        let out = mare_cmd()
            .args([arg, OsStr::new("--help")])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "panic on {arg:?}: {stderr}");
        assert!(out.status.success(), "expected exit 0 for {arg:?} --help, got {:?}: {stderr}", out.status.code());
    }

    let out = mare_cmd()
        .args([
            OsStr::new("--json"),
            OsStr::new("--nonexistent-flag-for-test"),
            OsStr::from_bytes(b"-C/tmp/nonutf8-\xff"),
        ])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "panic on --json parse error: {stderr}");
    assert_eq!(out.status.code(), Some(2), "expected usage error exit 2, got {:?}: {stderr}", out.status.code());
}

/// `mare plugin tree` lists the tree a run with the same flags would start: a user plugin is listed,
/// and with `--clean` it is not (lazy#51 #4: it used to ignore the global flags).
#[test]
fn mare_plugins_follows_the_global_flags() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config/plugins/listed-probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), r#"return { name = "listed-probe", apply = function() end }"#).unwrap();
    let ids = |extra: &[&str]| -> Vec<String> {
        let out = mare_cmd()
            .args(extra)
            .args(["plugin", "tree", "--json"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let dump: Value = serde_json::from_slice(&out.stdout).unwrap();
        dump.as_array().unwrap().iter().filter_map(|r| r["id"].as_str().map(str::to_owned)).collect()
    };
    let all = ids(&[]);
    assert!(all.iter().any(|i| i == "listed-probe"), "{all:?}");
    assert!(all.iter().any(|i| i == "llm"), "{all:?}");
    let none = ids(&["--clean"]);
    assert!(!none.iter().any(|i| i == "listed-probe"), "{none:?}");
    assert!(none.iter().any(|i| i == "llm"), "built-ins still run: {none:?}");
}

#[test]
fn cold_start_init_lua_skips_discovery_vm_for_standard_args_and_runs_for_ext_flags_and_help() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    let eval_log = home.path().join("eval_count.log");
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "ok" }, { "text": "ok" }, { "text": "ok" }]).to_string()).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            r#"
local f = assert(io.open({:?}, "a"))
f:write("eval\n")
f:close()
mare.flags.register("cold-flag", {{ type = "string", description = "Cold start test flag" }})
"#,
            eval_log.to_string_lossy()
        ),
    )
    .unwrap();

    let count_evals = || -> usize { std::fs::read_to_string(&eval_log).map(|s| s.lines().count()).unwrap_or(0) };
    let clear_evals = || {
        let _ = std::fs::remove_file(&eval_log);
    };

    // 1. Standard headless invocation evaluates init.lua once (no pre-startup flag discovery VM).
    clear_evals();
    let out = mare_cmd()
        .args(["-p", "--model", "faux/echo", "--no-session", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(count_evals(), 1, "standard CLI args must skip discover_cli_flags VM");

    // 2. `--` prompt args (`-- --help`) and `--set` values containing `--trust` also skip discovery.
    clear_evals();
    let out = mare_cmd()
        .args(["-p", "--model", "faux/echo", "--no-session", "--set", "model=faux/echo", "--", "--help"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(count_evals(), 1, "`-- --help` and `--set` must skip discover_cli_flags VM");

    // 3. `--help` triggers discovery so `--cold-flag` appears in help output.
    clear_evals();
    let help = mare_cmd()
        .arg("--help")
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--cold-flag"));
    assert_eq!(count_evals(), 1, "`--help` runs only discovery VM");

    // 4. Passing an extension flag triggers discovery (1) and then runs the host VM (2).
    clear_evals();
    let ext = mare_cmd()
        .args(["--cold-flag", "v1", "-p", "--model", "faux/echo", "--no-session", "hello"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(ext.status.success(), "{}", String::from_utf8_lossy(&ext.stderr));
    assert_eq!(count_evals(), 2, "extension flag runs discovery VM + host VM");

    // 5. Untrusted project `.mare/init.lua` is never evaluated on `--help` even with `--set foo=--trust`.
    let untrusted = home.path().join("untrusted-proj");
    let untrusted_marker = untrusted.join("ran.txt");
    std::fs::create_dir_all(untrusted.join(".mare")).unwrap();
    std::fs::write(
        untrusted.join(".mare/init.lua"),
        format!(
            r#"local f = assert(io.open({:?}, "w")); f:write("ran"); f:close()"#,
            untrusted_marker.to_string_lossy()
        ),
    )
    .unwrap();
    let out = mare_cmd()
        .args(["-C", untrusted.to_str().unwrap(), "--set", "foo=--trust", "--help"])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success());
    assert!(!untrusted_marker.exists(), "untrusted project init.lua must never run during discovery");
}

#[test]
fn real_session_list_limit_one_disambiguates_colliding_workspace_and_archived_ids() {
    use std::io::Write;

    let home = tempfile::tempdir().unwrap();
    let sessions_root = home.path().join("state/sessions");
    let ws1 = home.path().join("ws1");
    let ws2 = home.path().join("ws2");
    std::fs::create_dir_all(&ws1).unwrap();
    std::fs::create_dir_all(&ws2).unwrap();

    let d1 = sessions_root.join(mare_store::cwd_slug(&ws1));
    let d2 = sessions_root.join(mare_store::cwd_slug(&ws2));
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();

    let write_sess = |p: &std::path::Path, id: &str, cwd: &std::path::Path, archived: bool, ts: i64| {
        let mut f = std::fs::File::create(p).unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":id,"cwd":cwd,"created_at_ms":ts})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            json!({"type":"message","entry_id":"u1","created_at_ms":ts + 1,"message":{"role":"user","content":"hello"}})
        )
        .unwrap();
        if archived {
            writeln!(f, "{}", json!({"type":"archive","archived":true,"created_at_ms":ts + 2})).unwrap();
        }
    };

    write_sess(&d1.join("2026-01-03T00-00-00_abcd12340001.jsonl"), "abcd12340001", &ws1, false, 3000);
    write_sess(&d1.join("2026-01-02T00-00-00_abcd12340002.jsonl"), "abcd12340002", &ws1, true, 2000);
    write_sess(&d2.join("2026-01-01T00-00-00_abcd12340003.jsonl"), "abcd12340003", &ws2, false, 1000);

    let out = mare_cmd()
        .args(["--clean", "-C", ws1.to_str().unwrap(), "session", "list", "--limit", "1"])
        .env("MARE_HOME", home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 session line: {stdout}");
    assert!(
        lines[0].starts_with("abcd12340001  "),
        "real `session list --limit 1` must preserve global/archive short-ID disambiguation: {stdout}"
    );
}

#[test]
fn cli_add_dir_validates_registers_writable_roots_skips_project_resources_and_persists_resume() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra1 = root.join("extra1");
    let extra2 = root.join("extra2");
    let ungranted = root.join("ungranted");
    let marker = root.join("evil-marker.txt");
    let plain_file = root.join("not-a-dir.txt");
    for d in [&home, &cwd, &extra1, &extra2, &ungranted] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(&plain_file, "file").unwrap();

    // Project resources inside --add-dir directories must NEVER load or execute.
    std::fs::create_dir_all(extra1.join(".mare/plugins")).unwrap();
    std::fs::write(
        extra1.join(".mare/init.lua"),
        format!("mare.fs.write({:?}, 'init-ran')\n", marker.to_string_lossy()),
    )
    .unwrap();
    std::fs::write(
        extra1.join(".mare/plugins/evil.lua"),
        format!(
            "mare.fs.write({:?}, 'plugin-ran')\nreturn {{ name = 'evil', apply = function() end }}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    // 1. Invalid --add-dir inputs fail loudly (exit 2) and emit clean --json settled records.
    for bad_arg in [
        root.join("does-not-exist").to_string_lossy().into_owned(),
        plain_file.to_string_lossy().into_owned(),
        "relative-subdir".to_string(),
    ] {
        let out = mare_cmd()
            .args(["--json", "--model", "faux/echo", "--no-session", "--add-dir", &bad_arg, "hi"])
            .env("MARE_HOME", &home)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(out.status.code(), Some(2), "{bad_arg}: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "settled", "{stdout}");
        assert_eq!(last["data"]["exit_code"], 2, "{stdout}");
    }

    #[cfg(unix)]
    {
        let sym = root.join("sym-dir");
        std::os::unix::fs::symlink(&extra1, &sym).unwrap();
        let out = mare_cmd()
            .args(["-p", "--model", "faux/echo", "--no-session", "--add-dir", sym.to_str().unwrap(), "hi"])
            .env("MARE_HOME", &home)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(out.status.code(), Some(2), "symlink --add-dir must fail loudly");
    }

    // 2. Repeatable --add-dir grants workspace + sandbox writable access across extra1 and extra2
    //    without loading project resources or touching ungranted siblings, and persists to the session log.
    let w1 = extra1.join("w1.txt");
    let w2 = extra2.join("w2.txt");
    let b1 = extra1.join("b1.txt");
    let denied = ungranted.join("denied.txt");
    let script = root.join("script.json");
    let bash_write = if cfg!(windows) {
        json!({ "name": "powershell", "arguments": { "command": format!("Set-Content -NoNewline -LiteralPath '{}' -Value 'b1'", b1.display()) } })
    } else {
        json!({ "name": "bash", "arguments": { "command": format!("printf 'b1' > {:?}", b1) } })
    };
    let bash_denied = if cfg!(windows) {
        json!({ "name": "powershell", "arguments": { "command": format!("Set-Content -NoNewline -LiteralPath '{}' -Value 'no'", denied.display()) } })
    } else {
        json!({ "name": "bash", "arguments": { "command": format!("printf 'no' > {:?}", denied) } })
    };
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [
                { "name": "write", "arguments": { "path": w1, "content": "ok1" } },
                { "name": "write", "arguments": { "path": w2, "content": "ok2" } },
                bash_write,
                { "name": "write", "arguments": { "path": denied, "content": "no" } },
                bash_denied,
            ] },
            { "text": "added-dirs-done" },
            { "tool_calls": [
                { "name": "write", "arguments": { "path": extra1.join("resumed.txt"), "content": "resumed-ok" } },
                { "name": "write", "arguments": { "path": extra2.join("dropped.txt"), "content": "no" } },
            ] },
            { "text": "resumed-done" }
        ])
        .to_string(),
    )
    .unwrap();

    let sid = "add-dir-persist-01";
    let out = mare_cmd()
        .args([
            "-p",
            "--model",
            "faux/echo",
            "--session-id",
            sid,
            "--permission",
            "auto",
            "--approval-policy",
            "never",
            "--add-dir",
            extra1.to_str().unwrap(),
            "--add-dir",
            extra2.to_str().unwrap(),
            "--add-dir",
            extra1.to_str().unwrap(),
            "run",
        ])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(&w1).unwrap(), "ok1");
    assert_eq!(std::fs::read_to_string(&w2).unwrap(), "ok2");
    assert_eq!(std::fs::read_to_string(&b1).unwrap(), "b1");
    assert!(!denied.exists(), "ungranted sibling must remain non-writable");
    assert!(!marker.exists(), ".mare init/plugins in --add-dir must never run");

    // 3. Delete extra2 before --resume: revalidation drops extra2 with a warning and keeps extra1 writable.
    std::fs::remove_dir_all(&extra2).unwrap();
    let resume_script = root.join("resume_script.json");
    std::fs::write(
        &resume_script,
        json!([
            { "tool_calls": [
                { "name": "write", "arguments": { "path": extra1.join("resumed.txt"), "content": "resumed-ok" } },
                { "name": "write", "arguments": { "path": extra2.join("dropped.txt"), "content": "no" } },
            ] },
            { "text": "resumed-done" }
        ])
        .to_string(),
    )
    .unwrap();
    let resume_out = mare_cmd()
        .args([
            "-p",
            "--model",
            "faux/echo",
            "--resume",
            sid,
            "--permission",
            "auto",
            "--approval-policy",
            "never",
            "resume",
        ])
        .env("MARE_HOME", &home)
        .env("MARE_FAUX_SCRIPT", &resume_script)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .run_output();
    assert!(
        resume_out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resume_out.stdout),
        String::from_utf8_lossy(&resume_out.stderr)
    );
    let resume_out_str = String::from_utf8_lossy(&resume_out.stdout);
    let resume_err = String::from_utf8_lossy(&resume_out.stderr);
    assert_eq!(
        std::fs::read_to_string(extra1.join("resumed.txt")).unwrap_or_else(|e| {
            panic!("missing resumed.txt ({e}); stdout={resume_out_str:?}; stderr={resume_err:?}")
        }),
        "resumed-ok"
    );
    assert!(!extra2.exists(), "removed extra2 must not be recreated on resume");
    assert!(
        resume_err.contains("additional workspace directory is no longer valid and was removed"),
        "expected resume revalidation warning: {resume_err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_exclude_tools_filters_declarations_execution_wildcard_and_rpc() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("hello.txt"), "hello-world").unwrap();

    // Setup a mock OpenAI server so we inspect exact model tool declarations across turns.
    let server = MockServer::start().await;
    let openai_msg = |text: &str| {
        [
            json!({ "type": "response.output_item.done", "item": { "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": text }] } }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 1 } } }),
        ]
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>()
    };
    let openai_tool_call = |id: &str, name: &str, args: &str| {
        [
            json!({ "type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": id, "name": name, "arguments": args
            } }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 1 } } }),
        ]
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>()
    };

    // Route 1: --exclude-tools bash,write + repeatable --exclude-tools edit
    // Model tries calling excluded `write`, gets Unknown tool refusal, then replies.
    server.route(
        "/v1/responses",
        vec![
            Resp::sse(openai_tool_call(
                "call_w",
                "write",
                &json!({ "path": cwd.join("should_not_exist.txt"), "content": "nope" }).to_string(),
            )),
            Resp::sse(openai_msg("done-partial")),
            // Route 2 (request 2): --tools read,bash --exclude-tools bash
            Resp::sse(openai_msg("done-combo")),
            // Route 3 (requests 3-4): --exclude-tools *
            Resp::sse(openai_tool_call("call_r", "read", &json!({ "path": cwd.join("hello.txt") }).to_string())),
            Resp::sse(openai_msg("done-star")),
            // Route 4 (requests 5-6): RPC --exclude-tools bash
            Resp::sse(openai_tool_call("call_rpc_b", "bash", &json!({ "command": "echo nope" }).to_string())),
            Resp::sse(openai_msg("done-rpc")),
            // Route 5 (request 7): subagent inheriting tools_excluded
            Resp::sse(openai_msg("done-subagent")),
        ],
    );

    let cfg_dir = home.join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        format!(
            r#"
mare.setup {{
    model = "openai/gpt-6.1-terra",
    plugins = {{
        ["provider-openai"] = {{ base_url = {:?} }},
    }},
}}
"#,
            format!("{}/v1", server.base)
        ),
    )
    .unwrap();

    let run_cli = |extra_args: &[&str]| {
        let home = home.clone();
        let cwd = cwd.clone();
        let extra: Vec<String> = extra_args.iter().map(|s| (*s).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let mut args: Vec<String> =
                vec!["-p", "--no-session", "--permission", "auto", "--approval-policy", "never"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
            args.extend(extra);
            args.push("go".to_owned());
            let out = mare_cmd()
                .args(&args)
                .env("MARE_HOME", &home)
                .env("OPENAI_API_KEY", "test-key")
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .run_output();
            assert!(
                out.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
    };

    // 1. Comma-separated + repeatable --exclude-tools
    let out1 = run_cli(&["--exclude-tools", "bash,write", "--exclude-tools", "edit"]).await.unwrap();
    assert_eq!(out1.trim(), "done-partial");
    assert!(!cwd.join("should_not_exist.txt").exists(), "excluded write tool must be refused at execution");

    // 2. --tools + --exclude-tools: whitelist minus excluded
    let out2 = run_cli(&["--tools", "read,bash", "--exclude-tools", "bash"]).await.unwrap();
    assert_eq!(out2.trim(), "done-combo");

    // 3. --exclude-tools *
    let out3 = run_cli(&["--exclude-tools", "*"]).await.unwrap();
    assert_eq!(out3.trim(), "done-star");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 5, "expected 5 OpenAI requests across 3 CLI runs");
    let declared = |req_idx: usize| -> Vec<String> {
        reqs[req_idx].json()["tools"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let d0 = declared(0);
    assert!(d0.contains(&"read".to_string()), "{d0:?}");
    assert!(!d0.iter().any(|n| matches!(n.as_str(), "bash" | "write" | "edit")), "{d0:?}");

    let d2 = declared(2);
    assert_eq!(d2, vec!["read".to_string()], "whitelist minus excluded must leave only read: {d2:?}");

    let d3 = declared(3);
    assert!(d3.is_empty(), "--exclude-tools * must declare zero tools: {d3:?}");
    // Verify execution refusal reached the model in request 4's input messages.
    let body4 = reqs[4].json().to_string();
    assert!(body4.contains("Unknown tool: read"), "expected execution gate refusal for *: {body4}");

    // 4. RPC startup respects --exclude-tools too.
    let rpc_home = home.clone();
    let rpc_cwd = cwd.clone();
    tokio::task::spawn_blocking(move || {
        let mut rpc_cmd = mare_cmd();
        rpc_cmd
            .args([
                "--rpc",
                "--no-session",
                "--permission",
                "auto",
                "--approval-policy",
                "never",
                "--exclude-tools",
                "bash",
            ])
            .env("MARE_HOME", &rpc_home)
            .env("OPENAI_API_KEY", "test-key")
            .current_dir(&rpc_cwd);
        let mut rpc = RpcClient::spawn_cmd(&mut rpc_cmd);
        let init =
            rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| Value::Null);
        let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        let prompt_rep = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go rpc" }), |_| Value::Null);
        assert!(prompt_rep["error"].is_null(), "{prompt_rep}");
        let wait_rep = rpc.call_raw("agent.wait", json!({ "session_id": sid }), |_| Value::Null);
        assert!(wait_rep["error"].is_null(), "{wait_rep}");
        rpc.kill_and_wait();
    })
    .await
    .unwrap();

    // 5. Subagent inherits excluded tools from parent config (declarations + execution gate).
    let sub_base = server.base.clone();
    local(async move {
        let mut sub_env = TestEnv::new().await;
        sub_env.set_var("OPENAI_API_KEY", "test-key");
        let sub_host = sub_env
            .host()
            .no_session()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "provider-openai": { "base_url": format!("{}/v1", sub_base) } },
                "tools_excluded": ["bash", "write"],
            }))
            .build()
            .await;
        expose(&sub_host);
        let (child_text, parent_exec_err): (String, bool) = eval(
            &sub_host,
            r#"
            local child = mare.subagent.start({
                prompt = "check tools",
                model = "openai/gpt-6.1-terra",
                tools = { "read", "bash" },
            })
            local res = child:wait()
            local exec_res = T:execute({ id = "ex1", name = "bash", arguments = { command = "echo x" } }, {})
            return tostring(res.text), exec_res.is_error == true and exec_res.refused == true
            "#,
        )
        .await;
        assert_eq!(child_text, "done-subagent");
        assert!(parent_exec_err);
        sub_host.shutdown().await;
    })
    .await;

    let all_reqs = server.requests();
    assert_eq!(all_reqs.len(), 8, "expected 8 OpenAI requests (5 CLI + 2 RPC + 1 subagent)");
    let declared_at = |idx: usize| -> Vec<String> {
        all_reqs[idx].json()["tools"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let rpc_tools = declared_at(5);
    assert!(!rpc_tools.contains(&"bash".to_string()), "RPC must omit excluded bash: {rpc_tools:?}");
    let rpc_body6 = all_reqs[6].json().to_string();
    assert!(rpc_body6.contains("Unknown tool: bash"), "RPC must refuse excluded bash at execution: {rpc_body6}");
    let child_tools = declared_at(7);
    assert_eq!(child_tools, vec!["read".to_string()], "subagent must inherit tools_excluded: {child_tools:?}");
}

#[tokio::test]
async fn cli_tools_restricts_plugin_tools_empty_list_and_subagent_cannot_widen() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();

    let server = MockServer::start().await;
    let openai_msg = |text: &str| {
        [
            json!({ "type": "response.output_item.done", "item": { "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": text }] } }),
            json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 1 } } }),
        ]
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>()
    };
    server.route(
        "/v1/responses",
        vec![
            Resp::sse(openai_msg("ok-parent-read")),
            Resp::sse(openai_msg("ok-child-clamped")),
            Resp::sse(openai_msg("ok-child-disjoint")),
            Resp::sse(openai_msg("ok-empty")),
        ],
    );

    let sub_base = server.base.clone();
    local(async move {
        // 1. Parent with tools_enabled = ["read"] and tools_excluded = ["edit"]:
        //    plugin-registered tools are excluded, and subagents cannot widen via spec.config or spec.tools.
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-key");
        let host = env
            .host()
            .no_session()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "provider-openai": { "base_url": format!("{}/v1", sub_base) } },
                "tools_enabled": ["read"],
                "tools_excluded": ["edit"],
            }))
            .build()
            .await;
        expose(&host);
        let (plugin_refused, child1_text, child2_text): (bool, String, String) = eval(
            &host,
            r#"
            T:register("custom_plugin_tool", {
                description = "Plugin tool",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function() return { content = "ran" } end,
            })
            local p_res = T:execute({ id = "p1", name = "custom_plugin_tool", arguments = {} }, {})
            local r0 = A:wait(A:prompt("parent check").turn_id)
            assert(r0.text == "ok-parent-read", tostring(r0.text))

            -- Subagent tries to widen tools_enabled and clear tools_excluded via spec.config
            local c1 = mare.subagent.start({
                prompt = "child clamped",
                model = "openai/gpt-6.1-terra",
                tools = { "read", "bash" },
                config = {
                    tools_enabled = { "read", "bash", "write" },
                    tools_excluded = {},
                },
            })
            local r1 = c1:wait()

            -- Subagent requests disjoint tools_enabled -> intersection is empty -> ZERO tools (never all tools!)
            local c2 = mare.subagent.start({
                prompt = "child disjoint",
                model = "openai/gpt-6.1-terra",
                config = {
                    tools_enabled = { "bash" },
                },
            })
            local r2 = c2:wait()
            return p_res.is_error == true and p_res.refused == true, tostring(r1.text), tostring(r2.text)
            "#,
        )
        .await;
        assert!(plugin_refused, "plugin tool must be refused when excluded by tools_enabled");
        assert_eq!(child1_text, "ok-child-clamped");
        assert_eq!(child2_text, "ok-child-disjoint");
        host.shutdown().await;

        // 2. Explicit empty tools_enabled = [] disables ALL tools (does not mean unrestricted).
        let empty_host = env
            .host()
            .no_session()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "provider-openai": { "base_url": format!("{}/v1", sub_base) } },
                "tools_enabled": [],
            }))
            .build()
            .await;
        expose(&empty_host);
        let (empty_text, read_refused): (String, bool) = eval(
            &empty_host,
            r#"
            local r = A:wait(A:prompt("empty check").turn_id)
            local ex = T:execute({ id = "e1", name = "read", arguments = { path = "x" } }, {})
            return tostring(r.text), ex.is_error == true and ex.refused == true
            "#,
        )
        .await;
        assert_eq!(empty_text, "ok-empty");
        assert!(read_refused, "empty tools_enabled must refuse read at execution");
        empty_host.shutdown().await;
    })
    .await;

    let reqs = server.requests();
    assert_eq!(reqs.len(), 4, "expected 4 OpenAI requests");
    let declared_at = |idx: usize| -> Vec<String> {
        reqs[idx].json()["tools"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    assert_eq!(declared_at(0), vec!["read".to_string()], "parent must only declare read (no plugin tool)");
    assert_eq!(declared_at(1), vec!["read".to_string()], "subagent must not widen past parent tools_enabled");
    assert!(
        declared_at(2).is_empty(),
        "disjoint subagent tools_enabled intersection must yield zero tools: {:?}",
        declared_at(2)
    );
    assert!(declared_at(3).is_empty(), "empty tools_enabled must yield zero tools: {:?}", declared_at(3));
}

#[tokio::test]
async fn mcp_skips_startup_connect_and_approval_when_tools_enabled_excludes_mcp() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_srv = accepts.clone();
        let srv = tokio::spawn(async move {
            while let Ok((_sock, _)) = listener.accept().await {
                accepts_srv.fetch_add(1, Ordering::SeqCst);
            }
        });

        let env = TestEnv::new().await;
        std::fs::write(
            env.cwd.join(".mcp.json"),
            json!({
                "mcpServers": {
                    "proj_srv": { "url": format!("http://127.0.0.1:{port}/proj") }
                }
            })
            .to_string(),
        )
        .unwrap();

        let h = env
            .host()
            .no_session()
            .config(json!({
                "tools_enabled": ["read"],
                "plugins": {
                    "mcp": {
                        "enabled": true,
                        "servers": {
                            "eager_srv": {
                                "url": format!("http://127.0.0.1:{port}/eager"),
                                "tier": "direct"
                            }
                        }
                    }
                }
            }))
            .build()
            .await;
        expose(&h);

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let (eager_state, proj_state, saw_approval_notify): (String, String, bool) = eval(
            &h,
            r#"
            local mcp = C.cordis:get("mcp")
            local res = mcp:list_servers()
            local es, ps = "", ""
            for _, s in ipairs(res.data or {}) do
                if s.name == "eager_srv" then es = tostring(s.state) end
                if s.name == "proj_srv" then ps = tostring(s.state) end
            end
            return es, ps, false
            "#,
        )
        .await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            0,
            "direct MCP server must not auto-connect when --tools excludes MCP (state={eager_state})"
        );
        assert_eq!(eager_state, "idle", "direct MCP server must stay idle when excluded by --tools");
        assert_eq!(proj_state, "needs_approval", "project MCP server stays recorded as needs_approval");
        assert!(!saw_approval_notify);
        h.shutdown().await;
        srv.abort();
    })
    .await
}

#[test]
fn cli_add_dir_rejects_batch_protected_subpaths() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let repo = root.join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join(".mare")).unwrap();

    for pair in [
        [&repo, &repo.join(".git")],
        [&repo.join(".git"), &repo],
        [&repo, &repo.join(".mare")],
        [&repo.join(".mare"), &repo],
    ] {
        let out = mare_cmd()
            .args([
                "--json",
                "--model",
                "faux/echo",
                "--no-session",
                "--add-dir",
                pair[0].to_str().unwrap(),
                "--add-dir",
                pair[1].to_str().unwrap(),
                "hi",
            ])
            .env("MARE_HOME", &home)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(
            out.status.code(),
            Some(2),
            "batch protected pair {pair:?} must fail: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_add_dir_rejects_non_utf8_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let non_utf8 = root.join(OsString::from_vec(b"dir-\xff-bad".to_vec()));

    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "--add-dir"])
        .arg(&non_utf8)
        .arg("hi")
        .env("MARE_HOME", &home)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .run_output();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("not valid UTF-8"), "expected UTF-8 usage error in --json output: {stdout}");
}

#[test]
fn cli_rpc_add_dir_does_not_leak_to_session_new_or_load() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dunce::canonicalize(base.path()).unwrap();
    let home = root.join("home");
    let cwd = root.join("cwd");
    let extra = root.join("extra");
    for d in [&home, &cwd, &extra] {
        std::fs::create_dir_all(d).unwrap();
    }
    let probe = extra.join("probe.txt");
    let script = root.join("rpc_script.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": probe, "content": "ok" } }] },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args([
        "--rpc",
        "--model",
        "faux/echo",
        "--permission",
        "auto",
        "--approval-policy",
        "never",
        "--add-dir",
        extra.to_str().unwrap(),
    ])
    .env("MARE_HOME", &home)
    .env("MARE_FAUX_SCRIPT", &script)
    .current_dir(&cwd);
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| Value::Null);
    let sid0 = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let p0 = rpc.call_raw("agent.prompt", json!({ "session_id": sid0, "input": "turn0" }), |_| Value::Null);
    assert!(p0["error"].is_null(), "{p0}");
    let w0 = rpc.call_raw("agent.wait", json!({ "session_id": sid0 }), |_| Value::Null);
    assert!(w0["error"].is_null(), "{w0}");
    assert_eq!(std::fs::read_to_string(&probe).unwrap(), "ok");
    std::fs::remove_file(&probe).unwrap();

    let snew = rpc.call_raw("session.create", json!({}), |_| Value::Null);
    let sid1 = snew["result"]["session_id"].as_str().unwrap_or_else(|| panic!("{snew}")).to_string();
    let p1 = rpc.call_raw("agent.prompt", json!({ "session_id": sid1, "input": "turn1" }), |_| Value::Null);
    assert!(p1["error"].is_null(), "{p1}");
    let w1 = rpc.call_raw("agent.wait", json!({ "session_id": sid1 }), |_| Value::Null);
    assert!(w1["error"].is_null(), "{w1}");
    assert!(!probe.exists(), "RPC session.new must not inherit CLI --add-dir grants");

    let sload = rpc.call_raw("session.load", json!({ "session_id": sid1 }), |_| Value::Null);
    assert!(sload["error"].is_null(), "{sload}");
    let p2 = rpc.call_raw("agent.prompt", json!({ "session_id": sid1, "input": "turn2" }), |_| Value::Null);
    assert!(p2["error"].is_null(), "{p2}");
    let w2 = rpc.call_raw("agent.wait", json!({ "session_id": sid1 }), |_| Value::Null);
    assert!(w2["error"].is_null(), "{w2}");
    assert!(!probe.exists(), "RPC session.load must not inherit CLI --add-dir grants");

    rpc.kill_and_wait();
}

#[tokio::test]
async fn mare_workspace_add_is_not_promoted_to_writable_on_auto_preset_while_add_dir_stays_writable() {
    local(async {
        let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dunce::canonicalize(base.path()).unwrap();
        let ro_dir = root.join("ro-added");
        let rw_dir = root.join("rw-added");
        std::fs::create_dir_all(&ro_dir).unwrap();
        std::fs::create_dir_all(rw_dir.join(".git")).unwrap();

        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }))
            .build_exposed()
            .await;
        global(&h, "P", h.service_table("permissions").unwrap());
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "RO_DIR", ro_dir.to_string_lossy().to_string());
        global(&h, "RW_DIR", rw_dir.to_string_lossy().to_string());

        let (ro_ok, rw_ok, batch_bad): (bool, bool, bool) = eval(
            &h,
            r#"
            assert(mare.workspace.add(RO_DIR))
            S:add_workspace_dirs({ RW_DIR })
            P:set("auto")
            local spec = mare.proc.sandbox.current()
            local ro_w = mare.proc.sandbox.check("write", RO_DIR .. "/x.txt", spec)
            local rw_w = mare.proc.sandbox.check("write", RW_DIR .. "/x.txt", spec)
            local ok_batch = pcall(S.add_workspace_dirs, S, { RW_DIR .. "/.git" })
            return ro_w == true, rw_w == true, ok_batch == false
            "#,
        )
        .await;
        assert!(!ro_ok, "read-only mare.workspace.add directory must NOT become writable on auto preset");
        assert!(rw_ok, "session add_workspace_dirs directory must stay writable on auto preset");
        assert!(batch_bad, "protected subpath under added workspace root must be rejected");

        // Sequential `/add-dir repo/.git` -> `/add-dir repo` and concurrent `add_workspace_dirs` serialization.
        let seq_repo = root.join("seq-repo");
        let seq_git = seq_repo.join(".git");
        let conc_a = root.join("conc-a");
        let conc_b = root.join("conc-b");
        std::fs::create_dir_all(&seq_git).unwrap();
        std::fs::create_dir_all(&conc_a).unwrap();
        std::fs::create_dir_all(&conc_b).unwrap();
        global(&h, "SEQ_REPO", seq_repo.to_string_lossy().to_string());
        global(&h, "SEQ_GIT", seq_git.to_string_lossy().to_string());
        global(&h, "CONC_A", conc_a.to_string_lossy().to_string());
        global(&h, "CONC_B", conc_b.to_string_lossy().to_string());
        let (seq_parent_err, has_a, has_b): (bool, bool, bool) = eval(
            &h,
            r#"
            S:add_workspace_dirs({ SEQ_GIT })
            local ok_parent = pcall(S.add_workspace_dirs, S, { SEQ_REPO })
            local f1 = mare.task.spawn(function() S:add_workspace_dirs({ CONC_A }) end)
            local f2 = mare.task.spawn(function() S:add_workspace_dirs({ CONC_B }) end)
            f1:wait()
            f2:wait()
            local cur = mare.workspace.dirs()
            local fa, fb = false, false
            for _, d in ipairs(cur) do
                if d == CONC_A then fa = true end
                if d == CONC_B then fb = true end
            end
            return ok_parent == false, fa, fb
            "#,
        )
        .await;
        assert!(seq_parent_err, "sequential add of parent repo after repo/.git must be rejected");
        assert!(has_a && has_b, "concurrent add_workspace_dirs must serialize without losing updates");
        h.shutdown().await;
    })
    .await
}

/// `--append-system-prompt` on the real CLI binary
/// reaches the outgoing provider request's system prompt end-to-end.
#[tokio::test]
async fn append_system_prompt_cli_flag_reaches_provider_request() {
    let server = MockServer::start().await;
    let sse = [
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"ok\"}\n\n",
        "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}]}}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
    ]
    .concat();
    server.route("/openai", vec![Resp::sse(sse)]);

    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!(r#"mare.provider("openai", {{ base_url = "{}/openai", api_key = "sk-test-append" }})"#, server.base),
    )
    .unwrap();

    let out = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let work = work.path().to_path_buf();
        move || {
            mare_cmd()
                .args([
                    "-p",
                    "--no-session",
                    "--model",
                    "openai/gpt-6.1-terra",
                    "--append-system-prompt",
                    "CLI_APPEND_SENTINEL_99",
                    "hello",
                ])
                .env("MARE_HOME", &home)
                .current_dir(&work)
                .stdin(Stdio::null())
                .run_output()
        }
    })
    .await
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "CLI failed: {stderr}\n{}", String::from_utf8_lossy(&out.stdout));
    let req = server.requests().into_iter().find(|r| r.path.starts_with("/openai")).expect("provider request");
    let instructions = req.json()["instructions"].as_str().unwrap_or("").to_string();
    assert!(
        instructions.contains("CLI_APPEND_SENTINEL_99"),
        "system prompt must include --append-system-prompt text: {instructions}"
    );
}

/// Dedicated CLI permission/approval flags (`--permission`, `--yolo`, `--approval-policy`) must
/// flow through the `cli` config layer (visible in `mare.config`, `mare.config_sources()`, and
/// `/config`) without leaving a `mare.args.permission` / `mare.args.approval_policy` bypass,
/// while still overriding untrusted defaults and saved session state.
#[test]
fn cli_permission_and_approval_flags_populate_cli_config_layer_without_args_bypass() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    let plugins_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        r#"
        mare.setup { model = "faux/echo" }
        mare.plugin("permissions", { preset = "auto" })
        mare.plugin("approvals", { policy = "ask" })
        "#,
    )
    .unwrap();
    let out_file = home.path().join("probe.json");
    std::fs::write(
        plugins_dir.join("cli-layer-probe.lua"),
        r#"
        return {
          name = "cli-layer-probe",
          inject = { "permissions", "approvals", "commands" },
          apply = function(ctx)
            ctx:on("app/ready", function()
              local path = mare.env.get("PROBE_OUT")
              if not path or path == "" then return end
              local overlays = {}
              local orig_overlay, orig_connected = mare.ui.overlay, mare.ui.connected
              mare.ui.connected = function() return false end
              mare.ui.overlay = function(_, node)
                overlays[#overlays + 1] = mare.json.encode(node)
              end
              ctx.commands:run("/config plugins.permissions.preset")
              ctx.commands:run("/config plugins.approvals.policy")
              mare.ui.overlay, mare.ui.connected = orig_overlay, orig_connected
              local sources = mare.config_sources() or {}
              local psrc = sources["plugins.permissions.preset"]
              local asrc = sources["plugins.approvals.policy"]
              local cur_sb = mare.proc.sandbox.current()
              local report = {
                cfg_preset = mare.config.plugins and mare.config.plugins.permissions and mare.config.plugins.permissions.preset,
                cfg_policy = mare.config.plugins and mare.config.plugins.approvals and mare.config.plugins.approvals.policy,
                src_preset = psrc and psrc.layer or nil,
                src_policy = asrc and asrc.layer or nil,
                arg_permission = mare.args and mare.args.permission or nil,
                arg_approval_policy = mare.args and mare.args.approval_policy or nil,
                live_preset = ctx.permissions:preset(),
                live_sandbox = cur_sb and cur_sb.level or "none",
                live_policy = ctx.approvals:policy(),
                slash_preset = overlays[1] or "",
                slash_policy = overlays[2] or "",
              }
              mare.fs.write(path, mare.json.encode(report))
            end)
          end,
        }
        "#,
    )
    .unwrap();

    let run_probe = |extra_args: &[&str]| -> serde_json::Value {
        let _ = std::fs::remove_file(&out_file);
        let mut cmd = mare_cmd();
        cmd.args(["-p", "--model", "faux/echo"])
            .args(extra_args)
            .arg("hi")
            .env("MARE_HOME", home.path())
            .env("PROBE_OUT", &out_file)
            .current_dir(work.path())
            .stdin(Stdio::null());
        let output = cmd.run_output();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "probe run failed for {extra_args:?}: {stderr}");
        let raw = std::fs::read_to_string(&out_file).expect("probe plugin must write report");
        serde_json::from_str(&raw).unwrap()
    };

    // 1. `--permission read-only --approval-policy never` populates the `cli` config layer and `/config`.
    let r1 = run_probe(&["--no-session", "--permission", "read-only", "--approval-policy", "never"]);
    assert_eq!(r1["cfg_preset"], "read-only", "{r1}");
    assert_eq!(r1["cfg_policy"], "never", "{r1}");
    assert_eq!(r1["src_preset"], "cli", "{r1}");
    assert_eq!(r1["src_policy"], "cli", "{r1}");
    assert!(r1["arg_permission"].is_null(), "mare.args.permission bypass must be removed: {r1}");
    assert!(r1["arg_approval_policy"].is_null(), "mare.args.approval_policy bypass must be removed: {r1}");
    assert_eq!(r1["live_preset"], "read-only", "{r1}");
    assert_eq!(r1["live_policy"], "never", "{r1}");
    assert!(
        r1["slash_preset"].as_str().unwrap().contains("read-only")
            && r1["slash_preset"].as_str().unwrap().contains("cli"),
        "/config must show cli source for plugins.permissions.preset: {r1}"
    );
    assert!(
        r1["slash_policy"].as_str().unwrap().contains("never") && r1["slash_policy"].as_str().unwrap().contains("cli"),
        "/config must show cli source for plugins.approvals.policy: {r1}"
    );

    // 2. Explicit CLI flags `--yolo` and `--permission full-access` override `untrusted_preset`,
    // whereas `--set plugins.permissions.preset=full-access` stays in the `cli` config layer
    // without bypassing the untrusted `read-only` ceiling.
    for extra in [&["--no-session", "--yolo"][..], &["--no-session", "--permission", "full-access"][..]] {
        let r = run_probe(extra);
        assert_eq!(r["cfg_preset"], "full-access", "{extra:?}: {r}");
        assert_eq!(r["src_preset"], "cli", "{extra:?}: {r}");
        assert!(r["arg_permission"].is_null(), "mare.args.permission bypass must be removed for {extra:?}: {r}");
        assert_eq!(r["live_preset"], "full-access", "explicit CLI flag {extra:?} must override untrusted ceiling: {r}");
        assert_eq!(r["live_sandbox"], "none", "{extra:?}: {r}");
    }
    let r2_set = run_probe(&["--no-session", "--set", "plugins.permissions.preset=full-access"]);
    assert_eq!(r2_set["cfg_preset"], "full-access", "{r2_set}");
    assert_eq!(r2_set["src_preset"], "cli", "{r2_set}");
    assert_eq!(
        r2_set["live_preset"], "read-only",
        "--set plugins.permissions.preset=full-access must remain capped by untrusted ceiling: {r2_set}"
    );
    assert_eq!(r2_set["live_sandbox"], "read-only", "{r2_set}");

    // 3. Explicit CLI `--permission` and `--approval-policy` override saved session records on `--resume`.
    let session_path = home.path().join("saved-session.jsonl");
    let lines = [
        serde_json::json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"sess-cli-layer-01","created_at_ms":1,"cwd":work.path()}),
        serde_json::json!({"type":"message","entry_id":"m1","parent_entry_id":null,"message":{"role":"user","content":"hi"}}),
        serde_json::json!({"type":"custom","kind":"permission/preset","provenance":"session-host","data":{"preset":"full-access","source":"user"}}),
        serde_json::json!({"type":"custom","kind":"approval/policy","provenance":"session-host","data":{"policy":"ask"}}),
    ];
    let body = lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(&session_path, body).unwrap();
    let r3 = run_probe(&[
        "--trust",
        "--resume",
        &session_path.to_string_lossy(),
        "--permission",
        "read-only",
        "--approval-policy",
        "never",
    ]);
    assert_eq!(r3["src_preset"], "cli", "{r3}");
    assert_eq!(r3["src_policy"], "cli", "{r3}");
    assert_eq!(r3["live_preset"], "read-only", "CLI --permission must beat resumed session preset: {r3}");
    assert_eq!(r3["live_policy"], "never", "CLI --approval-policy must beat resumed session policy: {r3}");

    // 3b. `--set plugins.permissions.preset=full-access` does NOT override a resumed session's saved `read-only`.
    let ro_session_path = home.path().join("saved-ro-session.jsonl");
    let ro_lines = [
        serde_json::json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"sess-cli-layer-02","created_at_ms":1,"cwd":work.path()}),
        serde_json::json!({"type":"message","entry_id":"m1","parent_entry_id":null,"message":{"role":"user","content":"hi"}}),
        serde_json::json!({"type":"custom","kind":"permission/preset","provenance":"session-host","data":{"preset":"read-only","source":"user"}}),
    ];
    std::fs::write(&ro_session_path, ro_lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n")
        .unwrap();
    let r3_set = run_probe(&[
        "--trust",
        "--resume",
        &ro_session_path.to_string_lossy(),
        "--set",
        "plugins.permissions.preset=full-access",
    ]);
    assert_eq!(r3_set["cfg_preset"], "full-access", "{r3_set}");
    assert_eq!(r3_set["src_preset"], "cli", "{r3_set}");
    assert_eq!(
        r3_set["live_preset"], "read-only",
        "--set plugins.permissions.preset must not override resumed session preset: {r3_set}"
    );
    assert_eq!(r3_set["live_sandbox"], "read-only", "{r3_set}");

    // 4. Explicit CLI `--permission` and `--approval-policy` beat user init, trusted project init,
    // and `MARE_APPROVAL_POLICY` env simultaneously.
    let proj_mare = work.path().join(".mare");
    std::fs::create_dir_all(&proj_mare).unwrap();
    std::fs::write(
        proj_mare.join("init.lua"),
        r#"
        mare.plugin("permissions", { preset = "full-access" })
        mare.plugin("approvals", { policy = "ask" })
        "#,
    )
    .unwrap();
    let _ = std::fs::remove_file(&out_file);
    let mut cmd_prec = mare_cmd();
    cmd_prec
        .args([
            "-p",
            "--model",
            "faux/echo",
            "--no-session",
            "--trust",
            "--permission",
            "read-only",
            "--approval-policy",
            "never",
            "hi",
        ])
        .env("MARE_HOME", home.path())
        .env("MARE_APPROVAL_POLICY", "ask")
        .env("PROBE_OUT", &out_file)
        .current_dir(work.path())
        .stdin(Stdio::null());
    let out_prec = cmd_prec.run_output();
    assert_eq!(out_prec.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out_prec.stderr));
    let r4: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(r4["cfg_preset"], "read-only", "CLI --permission must beat user + trusted project init: {r4}");
    assert_eq!(r4["cfg_policy"], "never", "CLI --approval-policy must beat user + trusted project init + env: {r4}");
    assert_eq!(r4["src_preset"], "cli", "{r4}");
    assert_eq!(r4["src_policy"], "cli", "{r4}");
    assert_eq!(r4["live_preset"], "read-only", "{r4}");
    assert_eq!(r4["live_policy"], "never", "{r4}");
}

/// Auth subcommands (`login status`, `logout`, `model refresh`) honor `--no-trust` and `--clean`
/// just like normal runs: in a saved-trusted project, a project plugin provider is present in a
/// plain run, absent under `--no-trust`, and absent along with user providers under `--clean`.
#[test]
fn auth_subcommands_honor_no_trust_and_clean() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    let user_plug = home.path().join("config/plugins/user_prov");
    std::fs::create_dir_all(&user_plug).unwrap();
    std::fs::write(
        user_plug.join("init.lua"),
        r#"return {
            name = "user_prov",
            inject = { "llm" },
            apply = function(ctx)
                ctx.llm:register_provider({
                    provider = "user-test-prov",
                    default_model = "m1",
                    is_configured = function() return true end,
                    list_models = function()
                        return { { id = "m1", name = "M1", context_window = 1000, max_output = 100 } }
                    end,
                    models = { { model_id = "m1", name = "M1", context_window = 1000, max_output = 100 } },
                    stream = function() error("unused") end,
                })
            end,
        }"#,
    )
    .unwrap();

    let proj_plug = proj.path().join(".mare/plugins/proj_prov");
    std::fs::create_dir_all(&proj_plug).unwrap();
    std::fs::write(
        proj_plug.join("init.lua"),
        r#"return {
            name = "proj_prov",
            inject = { "llm" },
            apply = function(ctx)
                ctx.llm:register_provider({
                    provider = "proj-test-prov",
                    default_model = "m1",
                    is_configured = function() return true end,
                    list_models = function()
                        return { { id = "m1", name = "M1", context_window = 1000, max_output = 100 } }
                    end,
                    models = { { model_id = "m1", name = "M1", context_window = 1000, max_output = 100 } },
                    stream = function() error("unused") end,
                })
            end,
        }"#,
    )
    .unwrap();

    let trust_out = mare_cmd()
        .args(["trust"])
        .env("MARE_HOME", home.path())
        .current_dir(proj.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(trust_out.status.success(), "{}", String::from_utf8_lossy(&trust_out.stderr));

    let run = |args: &[&str]| -> (Option<i32>, String, String) {
        let out = mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .current_dir(proj.path())
            .stdin(Stdio::null())
            .run_output();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (code, plain_status, err) = run(&["login", "status"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(plain_status.contains("proj-test-prov"), "plain login status missing project provider:\n{plain_status}");
    assert!(plain_status.contains("user-test-prov"), "plain login status missing user provider:\n{plain_status}");

    let (code, no_trust_status, err) = run(&["--no-trust", "login", "status"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        !no_trust_status.contains("proj-test-prov"),
        "--no-trust login status must not load project provider:\n{no_trust_status}"
    );
    assert!(
        no_trust_status.contains("user-test-prov"),
        "--no-trust login status must keep user provider:\n{no_trust_status}"
    );

    let (code, clean_status, err) = run(&["--clean", "login", "status"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        !clean_status.contains("proj-test-prov"),
        "--clean login status must not load project provider:\n{clean_status}"
    );
    assert!(
        !clean_status.contains("user-test-prov"),
        "--clean login status must not load user provider:\n{clean_status}"
    );

    // `logout` and `model refresh` also honor `--no-trust` and `--clean`: save credentials for both
    // custom providers so `logout` / `model refresh` would see them if those flags were ignored.
    let auth_path = home.path().join("config/auth.json");
    std::fs::write(
        &auth_path,
        json!({
            "version": 1,
            "proj-test-prov": { "type": "api_key", "key": "sk-proj-test-0000" },
            "user-test-prov": { "type": "api_key", "key": "sk-user-test-0000" }
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let (_code, plain_refresh, _) = run(&["model", "refresh"]);
    assert!(
        plain_refresh.contains("proj-test-prov: refreshed") && plain_refresh.contains("user-test-prov: refreshed"),
        "plain model refresh must refresh both project and user providers:\n{plain_refresh}"
    );

    let (_code, no_trust_refresh, _) = run(&["--no-trust", "model", "refresh"]);
    assert!(
        !no_trust_refresh.contains("proj-test-prov"),
        "--no-trust model refresh must not load project provider:\n{no_trust_refresh}"
    );
    assert!(
        no_trust_refresh.contains("user-test-prov: refreshed"),
        "--no-trust model refresh must keep user provider:\n{no_trust_refresh}"
    );

    let (_code, clean_refresh, _) = run(&["--clean", "model", "refresh"]);
    assert!(
        !clean_refresh.contains("proj-test-prov") && !clean_refresh.contains("user-test-prov"),
        "--clean model refresh must not load project or user providers:\n{clean_refresh}"
    );

    // Without --no-trust / --clean, `logout` without argument sees both registered providers and asks to pick one;
    // under `--clean`, neither provider is registered so `logout` reports neither as a registered login.
    let (code, plain_logout, _) = run(&["logout"]);
    assert_eq!(code, Some(2), "{plain_logout}");
    assert!(
        plain_logout.contains("proj-test-prov") && plain_logout.contains("user-test-prov"),
        "plain logout must see both registered providers:\n{plain_logout}"
    );
    let (code, clean_logout, err) = run(&["--clean", "logout"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        !clean_logout.contains("proj-test-prov") && !clean_logout.contains("user-test-prov"),
        "--clean logout must not load project or user providers:\n{clean_logout}"
    );
}

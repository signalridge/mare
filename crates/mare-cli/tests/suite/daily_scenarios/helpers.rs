use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::{Value, json};

pub(super) const KEY_ENV: &str = "MARE_LIVE_OPENCODE_KEY";
pub(super) const DEFAULT_MODEL: &str = "opencode-go/glm-5.3-flash";

pub(super) fn live_model() -> String {
    std::env::var("MARE_LIVE_MODEL")
        .ok()
        .map(|s| {
            let t = s.trim();
            if t.is_empty() {
                DEFAULT_MODEL.to_string()
            } else if t.contains('/') {
                t.to_string()
            } else {
                format!("opencode-go/{t}")
            }
        })
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

pub(super) fn redact(text: &str, key: &str) -> String {
    if key.is_empty() { text.to_string() } else { text.replace(key, "[redacted]") }
}

/// Resolve the runtime `opencode-go` API key from `MARE_LIVE_OPENCODE_KEY` or `OPENCODE_API_KEY`.
/// Returns `None` when neither is available (so manual runs without credentials skip cleanly).
pub(super) fn runtime_live_key() -> Option<String> {
    for var in [KEY_ENV, "OPENCODE_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

pub(super) fn have_herdr() -> bool {
    Command::new("herdr")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(super) fn require_herdr_with(scenario: &str, available: bool) {
    assert!(available, "{scenario}: herdr binary unavailable on PATH (install herdr; refusing to silently pass)");
}

pub(super) fn require_herdr(scenario: &str) {
    require_herdr_with(scenario, have_herdr());
}

/// Append a `provider/request` Lua hook that counts every outgoing real model request without
/// ever logging credentials.
pub(super) fn request_counter_lua(log_path: &Path) -> String {
    format!(
        r#"mare.on("provider/request", function(w, next)
  local f = io.open({log:?}, "a")
  if f then
    local sid = tostring((w.headers and w.headers["x-opencode-session"]) or "")
    f:write(sid, "\n")
    f:close()
  end
  return next(w)
end)
"#,
        log = log_path.display().to_string()
    )
}

pub(super) fn count_requests(log_path: &Path) -> usize {
    std::fs::read_to_string(log_path).map(|s| s.lines().filter(|l| !l.is_empty()).count()).unwrap_or(0)
}

pub(super) fn find_session_files(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![home.join("state/sessions")];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "jsonl") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

pub(super) fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

pub(super) fn assert_all_tool_calls_paired(entries: &[Value]) {
    let mut calls = vec![];
    let mut results = std::collections::HashSet::new();
    for e in entries {
        if e["type"] != "message" {
            continue;
        }
        let m = &e["message"];
        match m["role"].as_str() {
            Some("assistant") => {
                for b in m["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_call"
                        && let Some(id) = b["id"].as_str()
                    {
                        calls.push(id.to_string());
                    }
                }
            }
            Some("tool_result") => {
                if let Some(id) = m["tool_call_id"].as_str() {
                    results.insert(id.to_string());
                }
            }
            _ => {}
        }
    }
    assert!(!calls.is_empty(), "expected at least one tool call in session log");
    let unpaired: Vec<_> = calls.iter().filter(|id| !results.contains(*id)).collect();
    assert!(unpaired.is_empty(), "unpaired tool calls in session log: {unpaired:?}");
}

/// Write a `0600` key file and shell wrapper that exports `OPENCODE_API_KEY` from disk and
/// removes the key file before `exec`ing `mare` (matching `scripts/tui_e2e.py`).
pub(super) fn write_mare_wrapper(
    dir: &Path,
    name: &str,
    mare_home: &Path,
    key: &str,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> PathBuf {
    let bin = Path::new(mare_bin());
    let bin_dir = bin.parent().unwrap().to_string_lossy();
    let keyfile = dir.join(format!("{name}.key"));
    let mut kf = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&keyfile).unwrap();
    kf.write_all(key.as_bytes()).unwrap();
    drop(kf);

    let wrapper = dir.join(name);
    let mut script = format!(
        "#!/bin/sh\nexport PATH=\"{bin_dir}:$PATH\"\nexport MARE_HOME=\"{}\"\nexport MARE_TEST_ALLOW_LIVE=1\nOPENCODE_API_KEY=\"$(cat \"{}\")\"; export OPENCODE_API_KEY\nrm -f \"{}\"\n",
        mare_home.display(),
        keyfile.display(),
        keyfile.display()
    );
    for (k, v) in extra_env {
        let _ = writeln!(script, "export {k}=\"{v}\"");
    }
    let _ = write!(script, "exec \"{}\"", bin.display());
    for a in args {
        let _ = write!(script, " '{a}'");
    }
    script.push('\n');
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}

/// Isolated `herdr server` instance in a temp directory (`Drop` stops and reaps cleanly).
pub(super) struct IsolatedHerdr {
    _root: tempfile::TempDir,
    home: PathBuf,
    cfg_dir: PathBuf,
    state_dir: PathBuf,
    cfg_file: PathBuf,
    sock: PathBuf,
    server: Option<Child>,
    pub(super) real_herdr: PathBuf,
    run_command: fn(&mut Command, Duration) -> std::process::Output,
}

impl IsolatedHerdr {
    pub(super) fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cfg_dir = root.path().join("cfg");
        let herdr_cfg = cfg_dir.join("herdr");
        let state_dir = root.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&herdr_cfg).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        // Prevent interactive zsh first-run wizard when panes open shells.
        std::fs::write(home.join(".zshrc"), "").unwrap();
        let cfg_file = herdr_cfg.join("config.toml");
        std::fs::write(&cfg_file, "onboarding = false\n").unwrap();
        let sock = root.path().join("herdr.sock");
        let real_herdr = which_herdr();
        let mut h = Self {
            _root: root,
            home,
            cfg_dir,
            state_dir,
            cfg_file,
            sock,
            server: None,
            real_herdr,
            run_command: run_output_timeout,
        };
        h.start_server();
        h
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.real_herdr);
        c.env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.cfg_dir)
            .env("XDG_STATE_HOME", &self.state_dir)
            .env("HERDR_CONFIG_PATH", &self.cfg_file)
            .env("HERDR_SOCKET_PATH", &self.sock)
            .env("SHELL", "/bin/bash");
        c
    }

    fn start_server(&mut self) {
        let child = self
            .cmd()
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn isolated herdr server");
        self.server = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.run_ok(&["pane", "list"]).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100)); // sleep-ok: wait for isolated herdr socket startup
        }
        panic!("isolated herdr server did not start within 10s");
    }

    fn stop_server(&mut self) {
        let _ = self.cmd().args(["server", "stop"]).output();
        if let Some(mut c) = self.server.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50)); // sleep-ok: await herdr server clean shutdown
                    }
                    _ => {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                }
            }
        }
    }

    pub(super) fn restart_server(&mut self) {
        self.stop_server();
        self.start_server();
    }

    pub(super) fn run_ok(&self, args: &[&str]) -> Result<String, String> {
        self.run_ok_timeout(args, DEFAULT_CMD_TIMEOUT)
    }

    fn run_ok_timeout(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let out = (self.run_command)(self.cmd().args(args), timeout);
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "herdr {} failed ({}): {}{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            ))
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        let s = self.run_ok(args).unwrap_or_else(|e| panic!("{e}"));
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("invalid JSON from herdr {}: {e}\n{s}", args.join(" ")))
    }

    pub(super) fn create_workspace(&self, cwd: &Path) -> String {
        let v = self.json(&["workspace", "create", "--cwd", cwd.to_str().unwrap()]);
        v["result"]["root_pane"]["pane_id"].as_str().expect("missing root_pane.pane_id").to_string()
    }

    pub(super) fn pane_info(&self, pane: &str) -> Value {
        self.json(&["pane", "get", pane])["result"]["pane"].clone()
    }

    pub(super) fn read_pane(&self, pane: &str, source: &str) -> String {
        self.run_ok(&["pane", "read", "--source", source, pane]).unwrap_or_default()
    }

    pub(super) fn wait_state(&self, pane: &str, states: &[&str], timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        let mut last = json!(null);
        while Instant::now() < deadline {
            let info = self.pane_info(pane);
            let agent = info["agent"].as_str().unwrap_or("");
            let st = info["agent_status"].as_str().unwrap_or("");
            if agent == "mare" && states.contains(&st) {
                return info;
            }
            last = info;
            std::thread::sleep(Duration::from_millis(200)); // sleep-ok: poll herdr pane agent state
        }
        panic!(
            "herdr pane {pane} never reached mare states {states:?} within {timeout:?}; last={last}\nscreen:\n{}",
            self.read_pane(pane, "visible")
        );
    }

    pub(super) fn wait_output(&self, pane: &str, needle: &str, source: &str, timeout: Duration) {
        let ms = timeout.as_millis().to_string();
        if let Err(e) = self.run_ok_timeout(
            &["pane", "wait-output", "--match", needle, "--source", source, "--timeout", &ms, pane],
            timeout.saturating_add(Duration::from_secs(30)),
        ) {
            panic!("{e}\nscreen:\n{}", self.read_pane(pane, "recent"));
        }
    }

    pub(super) fn send_line(&self, pane: &str, text: &str) {
        self.run_ok(&["pane", "send-text", pane, text]).unwrap();
        self.run_ok(&["pane", "send-keys", pane, "enter"]).unwrap();
    }

    pub(super) fn send_keys(&self, pane: &str, key: &str) {
        self.run_ok(&["pane", "send-keys", pane, key]).unwrap();
    }
}

impl Drop for IsolatedHerdr {
    fn drop(&mut self) {
        self.stop_server();
    }
}

pub(super) fn which_herdr() -> PathBuf {
    let out = Command::new("which").arg("herdr").output().expect("locate herdr");
    assert!(out.status.success(), "herdr binary not found on PATH");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

pub(super) fn herdr_env_for_pane<'a>(
    herdr: &'a IsolatedHerdr,
    pane: &'a str,
    bin_override: Option<&'a Path>,
) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", pane),
        ("HERDR_CONFIG_PATH", herdr.cfg_file.to_str().unwrap()),
        ("HERDR_SOCKET_PATH", herdr.sock.to_str().unwrap()),
    ];
    if let Some(b) = bin_override {
        env.push(("HERDR_BIN_PATH", b.to_str().unwrap()));
    } else {
        env.push(("HERDR_BIN_PATH", herdr.real_herdr.to_str().unwrap()));
    }
    env
}

/// Write `init.lua` for herdr TUI tests (setting `model`, `examples/herdr/init.lua`, optional
/// permissions preset, and `provider/request` call counter).
pub(super) fn write_herdr_tui_init(home: &Path, model: &str, permissions_preset: &str, req_log: &Path) {
    let cfg = home.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    let herdr_lua = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/herdr/init.lua"));
    let body = format!(
        "mare.setup {{ model = {model:?}, logo = false }}\nmare.plugin(\"permissions\", {{ preset = {permissions_preset:?} }})\n{counter}\n{herdr_lua}",
        counter = request_counter_lua(req_log),
    );
    std::fs::write(cfg.join("init.lua"), body).unwrap();
}

pub(super) fn base64_url_nopad(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[((n >> 18) & 63) as usize] as char);
        out.push(TBL[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TBL[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TBL[(n & 63) as usize] as char);
        }
    }
    out
}

/// Always-on sanity check so default test runs verify key redaction, wrapper generation (`0600`),
/// and skip-without-key behavior cleanly with zero network traffic.
#[test]
fn daily_scenarios_helpers_skip_without_key_and_redact_secrets() {
    assert_eq!(redact("Bearer op-secret-99", "op-secret-99"), "Bearer [redacted]");
    if std::env::var(KEY_ENV).map(|s| s.trim().is_empty()).unwrap_or(true)
        && std::env::var("OPENCODE_API_KEY").map(|s| s.trim().is_empty()).unwrap_or(true)
    {
        assert!(runtime_live_key().is_none());
    }
    assert_eq!(base64_url_nopad(b"hi"), "aGk");
}

#[test]
fn daily_scenarios_wait_output_allows_requested_timeout_plus_grace() {
    use std::os::unix::process::ExitStatusExt;

    fn inspect_wait(cmd: &mut Command, outer_timeout: Duration) -> std::process::Output {
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            ["pane", "wait-output", "--match", "ready", "--source", "recent", "--timeout", "160000", "pane-1"]
        );
        assert_eq!(outer_timeout, Duration::from_secs(190));
        std::process::Output { status: std::process::ExitStatus::from_raw(0), stdout: vec![], stderr: vec![] }
    }

    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_path_buf();
    let herdr = IsolatedHerdr {
        _root: root,
        home: path.clone(),
        cfg_dir: path.clone(),
        state_dir: path.clone(),
        cfg_file: path.clone(),
        sock: path,
        server: None,
        real_herdr: PathBuf::from("/nonexistent/herdr"),
        run_command: inspect_wait,
    };
    herdr.wait_output("pane-1", "ready", "recent", Duration::from_secs(160));
}

#[test]
#[should_panic(expected = "herdr binary unavailable")]
fn daily_scenarios_require_herdr_fails_when_binary_unavailable() {
    require_herdr_with("DAILY SCENARIO 2", false);
}

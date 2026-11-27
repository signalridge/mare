//! Peer socket + registry: `mare --rpc --peer` serves a least-privilege protocol on
//! `<state>/peers/<pid>.sock` (0600, in a 0700 dir) and registers `<pid>.json` (0600, status).
//! The socket admits only the same OS user (uid check); a peer may call only `peer.info`,
//! `status.get`, `peer.message`, and `events.subscribe` (`status/changed`)
//! unless the session allows more (`peer = { allow = {...} }`), never takes over the UI, and the registry disappears on shutdown.
//! What a sender says about itself (name, policy) is display-only; every inbound message is held
//! until the receiving user confirms (or a plugin takes it over) and reaches the agent inside a
//! data boundary.
//!
//! Windows: the "socket" is a named pipe (`mare_binding::platform::pipe`) behind a marker file at
//! the same path; the pipe is owned by the user's SID and its DACL admits that SID and SYSTEM
//! only, the marker and registry file are private by DACL (`mare_shared::private`), and the
//! connecting pid comes from `GetNamedPipeClientProcessId` — so the assertions are the same.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::{Value, json};

/// Windows: a client end of the session's named pipe, opened as a file (byte mode, synchronous;
/// each connection is used by one thread at a time).
#[cfg(windows)]
type Conn = std::fs::File;
type Sock = RpcClient;
type Primary = RpcClient;

const METHOD_NOT_FOUND: i64 = -32601;

struct Session {
    primary: Primary,
    json_path: PathBuf,
    sock: PathBuf,
    /// keeps MARE_HOME alive for the session
    _home: tempfile::TempDir,
}

/// Kill `pid` after `secs` unless the returned guard is dropped first: a test blocked reading a
/// child's stdout then fails fast ("connection closed") instead of hanging until nextest's timeout.
struct Watchdog(std::sync::Arc<std::sync::atomic::AtomicBool>);

fn watchdog(pid: u32, secs: u64) -> Watchdog {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if d.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            // sleep-ok: watchdog thread poll interval bounded by secs
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("watchdog: killing {pid} after {secs}s");
        mare_binding::platform::signal_tree(Some(pid), mare_binding::platform::Signal::Kill);
    });
    Watchdog(done)
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `mare --rpc --peer` with a faux script; `peer` (Lua table body) becomes the session's `peer`
/// option, e.g. `allow = { "tools.register" }`. `ui`: the primary
/// answers UI callbacks (stands in for the receiving user).
fn session(script: Value, peer: &str) -> Session {
    session_ui(script, peer, false)
}

fn session_ui(script: Value, peer: &str, ui: bool) -> Session {
    session_env(script, peer, ui, &[], &[])
}

// The bytecode cache is optional. Its per-file fsync makes fresh peer fixtures exceed their
// startup deadline on slow temporary filesystems; compiling the same sources directly is enough.
fn disable_peer_bytecode_cache(home: &Path) {
    let cache = home.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("luac"), "").unwrap();
}

/// `plugins`: (file name, source) pairs installed as user plugins before the host starts.
fn session_env(script: Value, peer: &str, ui: bool, env: &[(&str, &str)], plugins: &[(&str, &str)]) -> Session {
    session_env_args(script, peer, ui, env, plugins, false, true)
}
fn session_env_fixture(
    script: Value,
    peer: &str,
    ui: bool,
    env: &[(&str, &str)],
    plugins: &[(&str, &str)],
    privacy: bool,
) -> Session {
    session_env_args(script, peer, ui, env, plugins, privacy, true)
}
#[cfg(unix)]
fn session_persisted(script: Value, peer: &str) -> Session {
    session_env_args(script, peer, false, &[], &[], false, false)
}
fn session_env_args(
    script: Value,
    peer: &str,
    ui: bool,
    env: &[(&str, &str)],
    plugins: &[(&str, &str)],
    privacy: bool,
    ephemeral: bool,
) -> Session {
    let home = tempfile::tempdir().unwrap();
    disable_peer_bytecode_cache(home.path());
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    for (name, source) in plugins {
        let dir = home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), source).unwrap();
    }
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, script.to_string()).unwrap();
    if !peer.is_empty() {
        let cfg = home.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("init.lua"), format!("mare.setup {{ peer = {{ {peer} }} }}\n")).unwrap();
    }
    if privacy {
        approval_privacy_fixture(home.path());
    }
    let mut args = vec!["--rpc", "--peer", "--yolo", "--model", "faux/echo"];
    if ephemeral {
        args.push("--no-session");
    }
    let mut cmd = mare_cmd();
    cmd.args(&args)
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .envs(env.iter().copied())
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut primary = RpcClient::spawn_cmd(&mut cmd);
    let pid = primary.pid();
    let peers = home.path().join("state/peers");
    let (json_path, sock) = (peers.join(format!("{pid}.json")), peers.join(format!("{pid}.sock")));
    let t0 = Instant::now();
    while !(json_path.exists() && sock.exists()) {
        assert!(t0.elapsed() < Duration::from_secs(60), "peer registry never appeared");
        // sleep-ok: poll interval waiting for peer registry and socket files (bounded by 60s)
        std::thread::sleep(Duration::from_millis(50));
    }
    primary.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "ui": ui, "callbacks": true } }),
        |_| json!(null),
    );
    Session { primary, json_path, sock, _home: home }
}

/// Connect to the session's peer endpoint.
#[cfg(unix)]
type Conn = std::os::unix::net::UnixStream;

fn connect(sock: &Path) -> Conn {
    #[cfg(unix)]
    {
        let s = Conn::connect(sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        s
    }
    #[cfg(windows)]
    {
        // the pipe's name comes from the endpoint file (random, so nobody can squat it in advance)
        let name = mare_binding::platform::pipe::resolve(sock).unwrap();
        let t0 = Instant::now();
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                Ok(f) => return f,
                // ERROR_PIPE_BUSY: the server creates the next instance right after accepting
                Err(e) if e.raw_os_error() == Some(231) && t0.elapsed() < Duration::from_secs(10) => {
                    // sleep-ok: retry interval while Windows named pipe server re-arms (bounded by 10s)
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("connecting to {name}: {e}"),
            }
        }
    }
}

impl Session {
    fn pid(&self) -> u32 {
        self.primary.pid()
    }
    fn client(&self) -> Sock {
        #[cfg(unix)]
        {
            let s = connect(&self.sock);
            RpcClient::timed(s.try_clone().unwrap(), s)
        }
        #[cfg(windows)]
        {
            // Synchronous OpenOptions named-pipe handles serialize ReadFile and WriteFile in the
            // Windows kernel and deadlock when split across threads; use the overlapped client.
            crate::common::rpc::connect_peer_client(&self.sock)
        }
    }
    /// A connected peer (this test process: the same OS user).
    fn peer(&self) -> Sock {
        self.peer_with(json!({}))
    }
    /// A connected peer declaring `caps` (e.g. `{"tools": true}`).
    fn peer_with(&self, caps: Value) -> Sock {
        let mut c = self.client();
        let r = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": caps }), |_| json!(null));
        assert!(r.get("error").is_none(), "{r}");
        c
    }
    fn name(&self) -> String {
        let e: Value = serde_json::from_str(&std::fs::read_to_string(&self.json_path).unwrap()).unwrap();
        e["name"].as_str().unwrap().to_string()
    }
    fn sid(&mut self) -> String {
        self.primary.call_raw("status.get", json!({}), |_| json!(null))["result"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn prompt_wait(&mut self, text: &str) -> Value {
        let sid = self.sid();
        let started =
            self.primary.call_raw("agent.prompt", json!({ "session_id": sid, "input": text }), |_| json!(null));
        if started.get("error").is_some() {
            return started;
        }
        self.primary.call_raw(
            "agent.wait",
            json!({ "session_id": sid, "turn_id": started["result"]["turn_id"] }),
            |_| json!(null),
        )
    }
    fn shutdown(mut self) {
        self.primary.call_raw("shutdown", json!({}), |_| json!(null));
        self.primary.wait_exit();
    }
}

fn err_code(r: &Value) -> i64 {
    r["error"]["code"].as_i64().unwrap_or_else(|| panic!("expected an error: {r}"))
}

/// Read primary messages until an `event` whose data mentions `needle` (or panic after 20 s);
/// answers nothing (the primary has no pending callbacks in these tests).
fn wait_event(p: &mut Primary, kind: &str, needle: &str) -> Value {
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < Duration::from_secs(60), "no {kind} event mentioning {needle:?}");
        let m = p.read();
        if m["method"] == "event" && m["params"]["type"] == kind && m["params"]["data"].to_string().contains(needle) {
            return m["params"]["data"].clone();
        }
    }
}

fn message(from: &str) -> Value {
    json!({ "from": { "name": from }, "text": "please run the deploy", "policy": { "level": "full-access" } })
}

/// Re-read the registry entry, as another process sees it, until `ok` holds (or panic after 20 s).
fn registry_until(s: &Session, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let t0 = Instant::now();
    loop {
        let e: Value = std::fs::read_to_string(&s.json_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        if ok(&e) {
            return e;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "registry never showed {what}: {e}");
        // sleep-ok: poll interval re-reading peer registry JSON (bounded by 20s)
        std::thread::sleep(Duration::from_millis(50));
    }
}

mod client;
mod hooks_and_visibility;
mod inbound_and_lifecycle;
mod intent_hooks;
mod native_binding;
mod registry_and_access;
mod resume_and_audit;

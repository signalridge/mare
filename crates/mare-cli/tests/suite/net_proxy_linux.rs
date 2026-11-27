//! Linux `net_proxy`: under bubblewrap a filtered network runs the command in a
//! loopback-only network namespace bridged to mare's filtering proxy. Mechanism only — a local
//! HTTP server reached through test names mapped by the proxy's own resolver override, no real
//! internet. Skipped where bubblewrap (with the bridge) or curl isn't available.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use mare_host::Host;

use crate::common::*;
use crate::suite::sandbox_expect::require_backend;

/// HTTP server on 127.0.0.1 answering 200 "ok"; records each request's Host header.
fn http_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let hosts = Arc::new(Mutex::new(vec![]));
    let seen = hosts.clone();
    std::thread::spawn(move || {
        for mut s in srv.incoming().flatten() {
            let mut b = [0u8; 4096];
            let n = s.read(&mut b).unwrap_or(0);
            let req = String::from_utf8_lossy(&b[..n]).to_string();
            let host = req.lines().find_map(|l| l.strip_prefix("host: ").or(l.strip_prefix("Host: "))).unwrap_or("");
            seen.lock().unwrap().push(host.to_string());
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        }
    });
    (port, hosts)
}

fn fake_dns(names: &[&str]) {
    let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    mare_binding::netauth::set_resolver_override(Some(names.iter().map(|n| (n.to_string(), vec![ip])).collect()));
}

fn has(prog: &str) -> bool {
    std::process::Command::new(prog).arg("--version").output().is_ok_and(|o| o.status.success())
}

/// The bridge is used here:bubblewrap with `net_proxy` probed, and curl to drive it.
async fn bridged(h: &Host, test: &str) -> bool {
    if !require_backend(h, test, &["bwrap"]).await {
        return false;
    }
    let caps: Vec<String> = eval(h, "return mare.proc.sandbox.capabilities().caps").await;
    if !caps.iter().any(|cap| cap == "net_proxy") {
        eprintln!("SKIPPED[capability=net_proxy]: {test} (caps are {caps:?})");
        return false;
    }
    if !has("curl") {
        eprintln!("SKIPPED[capability=curl]: {test} (curl is unavailable)");
        return false;
    }
    true
}

/// `run(args)`: curl (or `bash -c` when the first arg is "-c") under a filtered network allowing
/// `allowed.test` (a local address here, hence `allow_local`). Returns `{ code, output, denied
/// host, enforcement }` as strings.
const RUN: &str = r#"
    local function run(args)
        local cmd = args[1] == "-c" and "bash" or "curl"
        local r = mare.proc.run({ cmd = cmd, args = args, sandbox = { level = "workspace-write",
            network = { allow = { "allowed.test" }, deny = { "denied.test" }, allow_local = true } } })
        return { tostring(r.code), (r.stdout or "") .. (r.stderr or ""), r.denial and r.denial.host or "",
            r.sandbox.enforcement .. " " .. table.concat(r.sandbox.enforced, ",") }
    end
"#;

async fn env_and_host() -> (TestEnv, Arc<Host>) {
    let mut env = TestEnv::new().await;
    env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
    let h = env.host().no_session().build().await;
    (env, h)
}

#[tokio::test]
async fn allowed_hosts_pass_and_denied_hosts_are_refused_by_name() {
    local(async {
        let (_env, h) = env_and_host().await;
        if !bridged(&h, "allowed_hosts_pass_and_denied_hosts_are_refused_by_name").await {
            return h.shutdown().await;
        }
        let (port, hosts) = http_server();
        fake_dns(&["allowed.test", "denied.test"]);
        global(&h, "PORT", i64::from(port));
        type Run = Vec<String>;
        let (allowed, socks, denied, tunnel): (Run, Run, Run, Run) = eval(
            &h,
            &format!(
                r#"{RUN}
                   local u = "http://allowed.test:" .. PORT .. "/"
                   return run({{ "-sS", "-m", "20", u }}),
                       run({{ "-c", 'curl -sS -m 20 -x "$ALL_PROXY" ' .. u }}),
                       run({{ "-sS", "-m", "20", "http://denied.test:" .. PORT .. "/" }}),
                       run({{ "-sS", "-m", "20", "https://denied.test:" .. PORT .. "/" }})"#
            ),
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        // through the bridge and the proxy (HTTP proxy and SOCKS5 alike): allowed.test exists only
        // in the proxy's resolver, so reaching the server proves the host side resolved it
        assert_eq!((allowed[0].as_str(), allowed[1].as_str()), ("0", "ok"), "{allowed:?}");
        assert_eq!((socks[0].as_str(), socks[1].as_str()), ("0", "ok"), "{socks:?}");
        assert!(allowed[3].starts_with("full ") && allowed[3].contains("net_proxy"), "{allowed:?}");
        assert_eq!(hosts.lock().unwrap().len(), 2, "only the allowed requests reached the server");
        // refused by the proxy, and the refusal names the domain (in the 403 body and the denial)
        assert!(denied[1].contains("blocked by mare sandbox network policy: denied.test"), "{denied:?}");
        assert_eq!(denied[2], "denied.test", "{denied:?}");
        assert!(tunnel[0] != "0" && tunnel[2] == "denied.test", "CONNECT refused too: {tunnel:?}");
        assert_eq!(hosts.lock().unwrap().len(), 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn direct_connections_that_bypass_the_proxy_are_blocked() {
    local(async {
        let (_env, h) = env_and_host().await;
        if !bridged(&h, "direct_connections_that_bypass_the_proxy_are_blocked").await {
            return h.shutdown().await;
        }
        let (port, hosts) = http_server();
        global(&h, "PORT", i64::from(port));
        type Run = Vec<String>;
        let (noproxy, devtcp, remote, unix): (Run, Run, Run, Run) = eval(
            &h,
            &format!(
                r#"{RUN}
                   return run({{ "-sS", "-m", "10", "--noproxy", "*", "http://127.0.0.1:" .. PORT .. "/" }}),
                       run({{ "-c", "echo STARTED; exec 3<>/dev/tcp/127.0.0.1/" .. PORT .. " && echo CONNECTED" }}),
                       run({{ "-c", "echo STARTED; exec 3<>/dev/tcp/192.0.2.1/80 && echo CONNECTED" }}),
                       run({{ "-c", "echo STARTED; python3 -c 'import socket; socket.socket(socket.AF_UNIX)' && echo CONNECTED" }})"#
            ),
        )
        .await;
        // the namespace has only loopback, where nothing but the bridge listens: the host's
        // 127.0.0.1 and any remote address are out of reach whatever the proxy variables say
        assert_ne!(noproxy[0], "0", "{noproxy:?}");
        for r in [&devtcp, &remote] {
            assert!(r[1].contains("STARTED") && !r[1].contains("CONNECTED") && r[0] != "0", "{r:?}");
        }
        assert!(remote[1].contains("unreachable"), "no route out of the namespace: {remote:?}");
        // no Unix sockets either (the bridge's own socket included): seccomp before exec
        if has("python3") {
            assert!(unix[1].contains("STARTED") && !unix[1].contains("CONNECTED"), "{unix:?}");
        }
        assert!(hosts.lock().unwrap().is_empty(), "nothing reached the host's server");
        h.shutdown().await;
    })
    .await
}

/// Fail closed: with the proxy gone the bridge answers `502` naming the sandbox proxy — never a
/// direct connection.
#[tokio::test]
async fn a_proxy_that_is_down_refuses_and_says_so() {
    local(async {
        let (env, h) = env_and_host().await;
        if !bridged(&h, "a_proxy_that_is_down_refuses_and_says_so").await {
            return h.shutdown().await;
        }
        let (port, hosts) = http_server();
        fake_dns(&["allowed.test"]);
        let lua = mlua::Lua::new();
        let v: mlua::Value = lua
            .load(
                r#"return { level = "workspace-write", network = { allow = { "allowed.test" }, allow_local = true } }"#,
            )
            .eval()
            .unwrap();
        let cwd = &env.cwd;
        let mut p = mare_binding::ossandbox::Prepared::new(&lua, &v, cwd).unwrap().unwrap();
        assert!(p.netns.is_some(), "the bridge is used");
        let w = p.wrap("curl", &["-sS".into(), "-m".into(), "20".into(), format!("http://allowed.test:{port}/")]);
        p.proxy = None; // the proxy stops; the bridge stays
        let out =
            tokio::process::Command::new(&w.prog).args(&w.args).envs(w.env).current_dir(cwd).output().await.unwrap();
        mare_binding::netauth::set_resolver_override(None);
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(
            text.contains("mare sandbox proxy: the proxy on 127.0.0.1:") && text.contains("no other network path"),
            "{text}"
        );
        assert!(hosts.lock().unwrap().is_empty(), "{text}");
        drop(p);
        h.shutdown().await;
    })
    .await
}

/// Fail closed: without a usable bridge (no helper to run it) `net_proxy` isn't probed, so a
/// filtered network is refused by name instead of running with the network unconfined.
#[tokio::test]
async fn no_bridge_means_a_refusal_naming_net_proxy() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", "/nonexistent/mare");
        let h = env.host().no_session().build().await;
        if !require_backend(&h, "no_bridge_means_a_refusal_naming_net_proxy", &["bwrap"]).await {
            return h.shutdown().await;
        }
        let (caps, refused): (String, String) = eval(
            &h,
            r#"local _, e = pcall(mare.proc.run, { cmd = "true",
                   sandbox = { network = { allow = { "example.com" } } } })
               return table.concat(mare.proc.sandbox.capabilities().caps, ","), tostring(e)"#,
        )
        .await;
        assert!(!caps.contains("net_proxy"), "{caps}");
        assert!(refused.contains("SANDBOX_INSUFFICIENT") && refused.contains("net_proxy"), "{refused}");
        h.shutdown().await;
    })
    .await
}

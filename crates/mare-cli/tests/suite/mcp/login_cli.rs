//! `mare mcp login` headless CLI OAuth paste fallback on a tty.

use super::*;

/// Headless `mcp/cli` login on a tty invokes `req.ask` so a pasted redirect URL completes login
/// even without a connected UI.
#[tokio::test]
async fn mcp_cli_login_event_invokes_ask_on_tty_without_ui() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = servers(json!({
            "oauth_srv": { "url": "http://127.0.0.1:1/mcp", "oauth": true }
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        let (code, pasted_url, out): (i64, String, String) = eval(
            &h,
            r#"local got_paste = nil
               mare.mcp.oauth_begin = function()
                 local login = {
                   url = "http://127.0.0.1:9/authorize?state=st1",
                   redirect = "http://127.0.0.1:12345/callback",
                 }
                 function login:paste(u) got_paste = u end
                 function login:finish()
                   local deadline = mare.task.clock() + 1000
                   while not got_paste and mare.task.clock() < deadline do
                     mare.task.sleep(20)
                   end
                   if not got_paste then error("paste was never called") end
                 end
                 return login
               end
               local req = {
                 action = "login",
                 server = "oauth_srv",
                 tty = true,
                 ask = function()
                   return "http://127.0.0.1:12345/callback?code=abc&state=st1"
                 end,
                 lines = {},
                 code = 0,
               }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code, tostring(got_paste or ""), table.concat(req.lines, "\n")"#,
        )
        .await;
        assert_eq!(code, 0, "mcp/cli login should succeed via pasted callback: {out}");
        assert_eq!(pasted_url, "http://127.0.0.1:12345/callback?code=abc&state=st1");
        h.shutdown().await;
    })
    .await
}

/// Real `mare mcp login <server>` process on a PTY reads a pasted OAuth callback URL from stdin,
/// exchanges the code against the loopback token endpoint, and persists the credentials.
#[test]
fn mcp_cli_login_pty_reads_pasted_callback_from_stdin() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_srv = Arc::clone(&stop);
    let meta_body = format!(
        r#"{{"issuer":"http://127.0.0.1:{port}/mcp","authorization_endpoint":"http://127.0.0.1:{port}/authorize","token_endpoint":"http://127.0.0.1:{port}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"]}}"#
    );
    let token_body = r#"{"access_token":"cli-pasted-token","token_type":"bearer","expires_in":3600}"#;

    let server = std::thread::spawn(move || {
        while !stop_srv.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first = req.lines().next().unwrap_or("");
                    let resp = if first.contains("/.well-known/oauth-authorization-server") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{meta_body}",
                            meta_body.len()
                        )
                    } else if first.starts_with("POST /token") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{token_body}",
                            token_body.len()
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    };
                    let _ = s.write_all(resp.as_bytes());
                    let _ = s.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10)); // sleep-ok: mock OAuth server accept poll
                }
                Err(_) => break,
            }
        }
    });

    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mcp_cfg = json!({
        "mcpServers": {
            "oauth_srv": {
                "url": format!("http://127.0.0.1:{port}/mcp"),
                "oauth": { "client_id": "test-client" }
            }
        }
    });
    std::fs::write(config_dir.join("mcp.json"), mcp_cfg.to_string()).unwrap();

    let mut p = MareCmd::new()
        .home_dir(home.path())
        .args(["mcp", "login", "oauth_srv"])
        .env("BROWSER", "true")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .pty(120, 30);

    p.wait_text("authorization URL", |s| {
        s.contains("Open this address to log in: ") && s.contains("state=") && s.contains("redirect_uri=")
    });
    let text = p.text();
    let auth_url = text
        .lines()
        .find_map(|l| l.split_once("Open this address to log in: ").map(|(_, rest)| rest.trim()))
        .expect("authorization URL line missing");
    // PTY line wrapping at 120 cols can split long URLs across lines; extract from the full text.
    let collapsed: String = text
        .split_once("Open this address to log in: ")
        .map(|(_, rest)| rest.replace("\r\n", "").replace('\n', ""))
        .unwrap_or_else(|| auth_url.to_string());
    let raw_url = collapsed.split_whitespace().next().unwrap_or(auth_url);
    let parsed = url::Url::parse(raw_url).unwrap_or_else(|e| panic!("bad auth url {raw_url:?}: {e}\n{text}"));
    let mut redirect_uri = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        if k == "redirect_uri" {
            redirect_uri = Some(v.into_owned());
        } else if k == "state" {
            state = Some(v.into_owned());
        }
    }
    let callback = format!(
        "{}?code=pasted-code&state={}\r\n",
        redirect_uri.expect("redirect_uri query param"),
        state.expect("state query param")
    );
    p.send(callback.as_bytes());
    let exited_ok = p.wait_exit(5);
    stop.store(true, Ordering::Release);
    let _ = server.join();
    let final_text = p.text();
    assert!(exited_ok, "mare mcp login exited non-zero:\n{final_text}");
    assert!(final_text.contains("Logged in to oauth_srv"), "missing success message:\n{final_text}");

    let oauth_dir = home.path().join("state/mcp-oauth");
    let cred_file = std::fs::read_dir(&oauth_dir)
        .unwrap_or_else(|e| panic!("missing {}: {e}", oauth_dir.display()))
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .expect("stored OAuth credential file");
    let creds = std::fs::read_to_string(&cred_file).unwrap();
    assert!(creds.contains("cli-pasted-token"), "stored credentials missing token: {creds}");
}

/// `/mcp login` rejects a non-oauth server, `/mcp logout` rejects a nonexistent server, and
/// under SSH with a connected UI `/mcp login` accepts a pasted callback URL and reconnects an eager
/// server after login.
#[tokio::test]
async fn mcp_login_rejects_non_oauth_logout_nonexistent_and_ssh_paste_reconnects_eager() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("SSH_CONNECTION", "10.0.0.1 1234 10.0.0.2 22");
        let fake = Fake::new(&env, "login-cmd");
        let cfg = servers(json!({
            "stdio_srv": with(fake.entry(json!({ "tools": ["echo"] })), json!({ "tier": "deferred" })),
            "oauth_srv": { "url": "http://127.0.0.1:1/mcp", "oauth": true, "tier": "direct" },
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);

        let (non_oauth_msg, missing_logout_msg, ssh_pasted, reconnected): (String, String, String, bool) = eval(
            &h,
            r#"local CM = C.cordis:get("commands")
               local notes = {}
               mare.ui.notify = function(msg, lvl)
                 notes[#notes + 1] = tostring(lvl or "info") .. ":" .. tostring(msg)
               end
               -- 1. /mcp login rejects non-oauth server
               CM:run("/mcp login stdio_srv")
               mare.task.sleep(50)
               local non_oauth = table.concat(notes, "\n")
               notes = {}
               -- 2. /mcp logout rejects nonexistent server
               CM:run("/mcp logout does_not_exist")
               local missing_logout = table.concat(notes, "\n")
               notes = {}
               -- 3. SSH + connected UI: /mcp login opens input, pastes URL, and reconnects eager server
               mare.ui.connected = function() return true end
               local got_paste, reconnect_calls = "", 0
               mare.ui.input = function()
                 return "http://127.0.0.1:44444/callback?code=ssh-code&state=st"
               end
               mare.mcp.oauth_begin = function()
                 local login = {
                   url = "http://127.0.0.1:9/authorize?state=st",
                   redirect = "http://127.0.0.1:44444/callback",
                 }
                 function login:paste(u) got_paste = u end
                 function login:finish()
                   local deadline = mare.task.clock() + 2000
                   while got_paste == "" and mare.task.clock() < deadline do mare.task.sleep(10) end
                   assert(got_paste ~= "", "SSH paste callback never arrived")
                   mare.mcp.has_credentials = function() return true end
                 end
                 return login
               end
               local orig_connect = mare.mcp.connect
               mare.mcp.connect = function(spec)
                 if spec and spec.name == "oauth_srv" then
                   reconnect_calls = reconnect_calls + 1
                   error("stub-reconnected")
                 end
                 return orig_connect(spec)
               end
               CM:run("/mcp login oauth_srv")
               local deadline = mare.task.clock() + 2000
               while reconnect_calls == 0 and mare.task.clock() < deadline do mare.task.sleep(20) end
               mare.mcp.connect = orig_connect
               return non_oauth, missing_logout, got_paste, reconnect_calls >= 1"#,
        )
        .await;
        assert!(non_oauth_msg.contains("no `oauth` configured"), "{non_oauth_msg}");
        assert!(missing_logout_msg.contains("no MCP server named does_not_exist"), "{missing_logout_msg}");
        assert_eq!(ssh_pasted, "http://127.0.0.1:44444/callback?code=ssh-code&state=st");
        assert!(reconnected, "eager OAuth server must reconnect automatically after login");
        h.shutdown().await;
    })
    .await
}

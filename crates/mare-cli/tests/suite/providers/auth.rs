//! Credentials: key helpers, OAuth and refresh tokens, auth.json.

use super::*;

mod refresh_owner;

#[tokio::test]
async fn a_key_helper_runs_once_per_interval_and_again_after_a_401() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let n = env.cwd.join("helper-runs");
        let cmd = format!("n=$(( $(cat '{0}' 2>/dev/null || echo 0) + 1 )); echo $n > '{0}'; echo key-$n", n.display());
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse")), Resp::status(401, r#"{"error":{"message":"expired"}}"#), Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": { "compat": { "api_key": { "cmd": ["sh", "-c", cmd], "refresh_interval_ms": 600000 } } } }),
        )
        .await;
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        // the cached key is rejected: the helper runs again and the request is retried once
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        let auth: Vec<String> = server.requests().iter().filter_map(|r| r.header("authorization").map(str::to_string)).collect();
        assert_eq!(auth, ["Bearer key-1", "Bearer key-1", "Bearer key-2"], "cached within the interval, refreshed on 401");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_oauth_refreshes_spend_the_refresh_token_once() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = server.base.clone();
        server.route("/auth/oauth/token", vec![Resp::status(200, r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600}"#)]);
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": { "codex_base_url": format!("{base}/codex"), "auth_base_url": format!("{base}/auth") } } }),
        )
        .await;
        // an expired ChatGPT login; two requests (e.g. two sub-agents) need a refresh at once
        let both: mlua::Value = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct" } }), { mode = 384 })
local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
local done = 0
local tx, rx = mare.task.channel()
for _ = 1, 2 do
  mare.task.spawn(function()
    local ok, msg = pcall(LLM.stream, LLM, { model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
    tx:send(ok and msg.stop_reason ~= "error" or tostring(ok and msg.error or msg))
  end)
end
local a, b = rx:recv(), rx:recv()
return (a == true and b == true) or (tostring(a) .. " | " .. tostring(b))
"#,
        )
        .await;
        assert!(matches!(both, mlua::Value::Boolean(true)), "both requests succeed: {both:?}");
        assert_eq!(server.count("/auth/oauth/token"), 1, "the second waiter re-reads the refreshed token instead of spending r1 again");
        let auths: Vec<String> = server.requests().iter().filter(|r| r.path.starts_with("/codex")).filter_map(|r| r.header("authorization").map(str::to_string)).collect();
        assert_eq!(auths, ["Bearer new-access", "Bearer new-access"]);
        h.shutdown().await;
    })
    .await
}

/// provider-openai pointed at the mock server's ChatGPT (`/codex`) and auth (`/auth`) routes.
async fn codex_host(env: &mut TestEnv, server: &MockServer) -> Arc<Host> {
    let base = server.base.clone();
    let h = provider_host(
        env,
        server,
        json!({ "plugins": { "provider-openai": { "codex_base_url": format!("{base}/codex"), "auth_base_url": format!("{base}/auth") } } }),
    )
    .await;
    expose(&h);
    h
}

/// Lua: write auth.json with one ChatGPT login.
const WRITE_LOGIN: &str = r#"
function LOGIN(access, refresh, expires, account)
  mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = {
    type = "oauth", access = access, refresh = refresh, expires = expires, account_id = account } }), { mode = 384 })
end
function ASK()
  local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
  local ok, msg = pcall(LLM.stream, LLM, { model = m,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
  if not ok then return type(msg) == "table" and (msg.message or mare.json.encode(msg)) or tostring(msg) end
  return msg.stop_reason == "error" and tostring(msg.error) or "ok"
end
"#;

fn codex_auths(server: &MockServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .filter(|r| r.path.starts_with("/codex"))
        .filter_map(|r| r.header("authorization").map(str::to_string))
        .collect()
}

/// A refresh token the server says was already used, or revoked, is named as such (Codex
/// login/src/auth/manager.rs:1383-1401) — log in again; it is not retried.
#[tokio::test]
async fn a_used_or_revoked_refresh_token_says_so() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(400, r#"{"error":{"code":"refresh_token_reused","message":"x"}}"#),
                Resp::status(400, r#"{"error":"refresh_token_invalidated"}"#),
            ],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        let used: String = eval(&h, r#"LOGIN("old", "r1", 0, "acct"); return ASK()"#).await;
        assert!(used.contains("refresh token was already used"), "{used}");
        let revoked: String = eval(&h, r#"LOGIN("old", "r1", 0, "acct"); return ASK()"#).await;
        assert!(revoked.contains("refresh token was revoked"), "{revoked}");
        assert_eq!(server.count("/auth/oauth/token"), 2, "one refresh attempt each, no retry");
        assert_eq!(server.count("/codex"), 0);
        h.shutdown().await;
    })
    .await
}

/// While a refresh waited for the auth lock, another process logged in to a different account:
/// the refresh is refused instead of spending the other account's token (Codex manager.rs:1715).
#[tokio::test]
async fn a_refresh_checks_the_account_after_waiting_for_the_lock() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(200, r#"{"access_token":"new","refresh_token":"r2","expires_in":3600}"#)],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        let res: String = eval(
            &h,
            r#"
LOGIN("old", "r1", 0, "acct-a")
local lock_file = mare.lock.file
local guard = lock_file(mare.paths.auth .. ".lock", { timeout_ms = 5000 })
-- the refresh reads the account and then asks for the lock; wait for that ask (it can only be
-- granted after the release below), not for a fixed time that a loaded machine can overrun
local asked_tx, asked_rx = mare.task.channel()
mare.lock.file = function(...) asked_tx:send(true); return lock_file(...) end
local tx, rx = mare.task.channel()
mare.task.spawn(function() tx:send(ASK()) end)
asked_rx:recv()
mare.lock.file = lock_file
LOGIN("other", "rb", 0, "acct-b")
guard:release()
return rx:recv()"#,
        )
        .await;
        assert!(res.contains("signed in to another account"), "{res}");
        assert_eq!(server.count("/auth/oauth/token"), 0);
        h.shutdown().await;
    })
    .await
}

/// On a 401, auth.json is read again before refreshing (Codex manager.rs:1576-1590): a token
/// another process rotated meanwhile is used as is; only an unchanged one is refreshed.
#[tokio::test]
async fn a_401_rereads_auth_json_before_refreshing() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(200, r#"{"access_token":"refreshed","refresh_token":"r2","expires_in":3600}"#)],
        );
        let unauthorized = r#"{"error":{"message":"token expired"}}"#;
        server.route(
            "/codex",
            vec![
                Resp::status(401, unauthorized),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::status(401, unauthorized),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        // another process rotates the token while the first request is in flight
        let res: String = eval(
            &h,
            r#"
LOGIN("old", "r1", mare.task.now() + 3600000, "acct")
local rotated = false
C.cordis:on("provider/request", function(w, next)
  if not rotated then rotated = true; LOGIN("rotated", "r9", mare.task.now() + 3600000, "acct") end
  return next()
end)
return ASK()"#,
        )
        .await;
        assert_eq!(res, "ok");
        assert_eq!(codex_auths(&server), ["Bearer old", "Bearer rotated"]);
        assert_eq!(server.count("/auth/oauth/token"), 0, "the rotated token is used, not refreshed again");
        // unchanged on disk: the rejected token is refreshed, then the request retried once
        let res: String = eval(&h, r#"LOGIN("old", "r1", mare.task.now() + 3600000, "acct"); return ASK()"#).await;
        assert_eq!(res, "ok");
        assert_eq!(codex_auths(&server)[2..], ["Bearer old", "Bearer refreshed"]);
        assert_eq!(server.count("/auth/oauth/token"), 1);
        h.shutdown().await;
    })
    .await
}

/// The file lock coordinates distinct processes; an out-of-band writer must win the CAS.
/// Codex login/src/auth/storage_commit_tests.rs:92,151 pauses competing processes at the lock.
#[tokio::test]
async fn refresh_credential_is_single_flight_across_processes_and_cas_protected() {
    const CHILD_HOME: &str = "MARE_TEST_REFRESH_CHILD_HOME";
    const CHILD_SERVER: &str = "MARE_TEST_REFRESH_CHILD_SERVER";
    const REFRESH: &str = r#"
local function valid(a) return a.expires > mare.task.now() end
local function same_account(a,b) return a.account_id == b.account_id end
return LLM:refresh_credential('example', 'shared', function(a)
    local response = mare.http.request({url=MOCK .. '/refresh', method='POST'})
    assert(response.ok)
    return {access='rotated', refresh='rotated-refresh', expires=mare.task.now()+3600000, account_id=a.account_id}
end, {force=true, valid=valid, same_account=same_account})
"#;
    local(async {
        if let Some(shared_home) = std::env::var_os(CHILD_HOME) {
            let server = std::env::var(CHILD_SERVER).unwrap();
            let mut env = TestEnv::new().await;
            env.set_var("MARE_HOME", shared_home);
            let h = env.host().no_session().build().await;
            global(&h, "LLM", h.service_table("llm").unwrap());
            global(&h, "MOCK", server);
            exec(
                &h,
                r#"
                local file = mare.lock.file
                mare.lock.file = function(...)
                    assert(mare.http.request({url=MOCK .. '/attempt'}).ok)
                    return file(...)
                end
            "#,
            )
            .await;
            let result: mlua::Table = eval(&h, REFRESH).await;
            assert_eq!(result.get::<String>("access").unwrap(), "rotated");
            h.shutdown().await;
            return;
        }
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let first_release = Arc::new(tokio::sync::Notify::new());
        let cas_release = Arc::new(tokio::sync::Notify::new());
        server.route(
            "/refresh",
            vec![
                Resp::status(200, "{}").hold(first_release.clone()),
                Resp::status(200, "{}").hold(cas_release.clone()),
            ],
        );
        server.route("/attempt", vec![Resp::status(200, "{}")]);
        let h = provider_host(&mut env, &server, json!({})).await;
        global(&h, "MOCK", server.base.clone());
        exec(&h, "assert(type(LLM.refresh_credential)=='function', 'llm:refresh_credential is missing')").await;
        exec(&h, r#"LLM:auth_set('shared',{access='old',refresh='r1',expires=0,account_id='acct'})"#).await;
        let refresh = format!("REFRESH = mare.task.spawn(function() RESULT = (function() {REFRESH} end)() end)");
        exec(&h, &refresh).await;
        wait_until(10, "first process entered fake refresh", || async { server.count("/refresh") == 1 }).await;
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "suite::providers::auth::refresh_credential_is_single_flight_across_processes_and_cas_protected",
                "--nocapture",
            ])
            .env(CHILD_HOME, env.home.path())
            .env(CHILD_SERVER, &server.base)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        wait_until(10, "second process attempted the lock", || async { server.count("/attempt") == 1 }).await;
        first_release.notify_one();
        exec(&h, "REFRESH:wait(); assert(RESULT.access=='rotated')").await;
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output()).await.unwrap().unwrap();
        assert!(
            output.status.success(),
            "child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(server.count("/refresh"), 1, "the second process must reuse the rotated token");

        // Deliberately bypass the lock, like an external editor, while the response is held.
        exec(&h, "LLM:auth_set('shared',{access='old',refresh='r1',expires=0,account_id='acct'})").await;
        exec(&h, &refresh).await;
        wait_until(10, "second fake refresh reached the server", || async { server.count("/refresh") == 2 }).await;
        exec(
            &h,
            r#"
            mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,shared={access='external',refresh='other',
                expires=mare.task.now()+3600000,account_id='acct'}}), {mode=384})
        "#,
        )
        .await;
        cas_release.notify_one();
        exec(
            &h,
            r#"
            REFRESH:wait()
            assert(RESULT.access=='external', 'CAS conflict must re-read the current credential')
            assert(LLM:auth_get('shared').refresh=='other', 'CAS must not overwrite the external writer')
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn revoked_refresh_removes_only_the_rejected_credential_and_warns_once() {
    local(async {
        for code in ["refresh_token_invalidated", "invalid_grant"] {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            server.route("/auth/oauth/token", vec![Resp::status(400, format!(r#"{{"error":"{code}"}}"#))]);
            let h = codex_host(&mut env, &server).await;
            exec(&h, WRITE_LOGIN).await;
            let (reason, gone, warnings): (String, bool, String) = eval(
                &h,
                r#"
                LOGIN('expired','rejected',0,'acct')
                LLM:auth_set('unrelated',{type='api_key',key='still-present'})
                local warn = {}
                local log = mare.log
                mare.log = function(level, message)
                    if level == 'warn' then warn[#warn+1] = message end
                end
                local ok, why = LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'))
                assert(not ok)
                local gone = LLM:auth_get('openai-codex') == nil and LLM:auth_get('unrelated').key == 'still-present'
                LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'))
                mare.log = log
                return why, gone, table.concat(warn, '\n')
            "#,
            )
            .await;
            assert!(gone, "{code}: revoked credential remains");
            assert_eq!(warnings.lines().count(), 1, "{code}: {warnings}");
            assert!(warnings.to_lowercase().contains("revoked"), "{code}: {warnings}");
            assert!(reason.contains("sign in again"), "{code}: {reason}");
            assert_eq!(server.count("/auth/oauth/token"), 1);
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn device_login_then_expiry_refreshes_the_stored_credential() {
    const ACCESS: &str = "eyJhbGciOiAibm9uZSJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOiB7ImNoYXRncHRfYWNjb3VudF9pZCI6ICJhY2N0LTEifX0.sig";
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/api/accounts/deviceauth/usercode",
            vec![Resp::status(200, r#"{"device_auth_id":"dev-1","user_code":"ABCD-EFGH","interval":"1"}"#)],
        );
        server.route(
            "/auth/api/accounts/deviceauth/token",
            vec![Resp::status(200, r#"{"authorization_code":"auth-code","code_verifier":"ver"}"#)],
        );
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(
                    200,
                    format!(r#"{{"access_token":"{ACCESS}","refresh_token":"login-refresh","expires_in":3600}}"#),
                ),
                Resp::status(
                    200,
                    format!(r#"{{"access_token":"{ACCESS}","refresh_token":"rotated-refresh","expires_in":3600}}"#),
                ),
            ],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        let result: String = eval(
            &h,
            r#"
            LLM:login('openai',{method='device',say=function() end,ask=function() end})
            local a = assert(LLM:auth_get('openai-codex'))
            assert(a.account_id=='acct-1' and a.refresh=='login-refresh')
            a.expires = 0
            LLM:auth_set('openai-codex',a)
            local response = ASK()
            assert(LLM:auth_get('openai-codex').refresh=='rotated-refresh')
            return response
        "#,
        )
        .await;
        assert_eq!(result, "ok");
        let requests: Vec<_> =
            server.requests().into_iter().filter(|r| r.path.starts_with("/auth/oauth/token")).collect();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].body.contains("grant_type=authorization_code"));
        assert!(
            requests[1].body.contains("grant_type=refresh_token")
                && requests[1].body.contains("refresh_token=login-refresh")
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_waiters_share_a_failed_refresh_owner_and_can_cancel_independently() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"
            LLM:auth_set('shared',{access='old',expires=0,account_id='acct'})
            local entered_tx,entered_rx=mare.task.channel()
            local release_tx,release_rx=mare.task.channel()
            local joined_tx,joined_rx=mare.task.channel()
            local select_task=mare.task.select
            mare.task.select=function(...)
                joined_tx:send(true)
                return select_task(...)
            end
            local calls=0
            local function exchange()
                calls=calls+1
                if calls==1 then entered_tx:send(true); release_rx:recv() end
                error({message='exchange rejected',kind='auth'},0)
            end
            local function options(cancel)
                return {force=true,cancel=cancel,valid=function() return false end}
            end
            local first_cancel=mare.task.token()
            local second_cancel=mare.task.token()
            local first=mare.task.spawn(function()
                local cred,err=LLM:refresh_credential('example','shared',exchange,options(first_cancel))
                return cred~=nil,err
            end)
            joined_rx:recv()
            entered_rx:recv()
            local second=mare.task.spawn(function()
                local cred,err=LLM:refresh_credential('example','shared',exchange,options(second_cancel))
                return cred~=nil,err
            end)
            joined_rx:recv()
            first_cancel:cancel()
            local first_ok,first_error=first:wait()
            assert(not first_ok and tostring(first_error):find('cancelled',1,true))
            release_tx:send(true)
            local second_ok,second_error=second:wait()
            mare.task.select=select_task
            assert(not second_ok and second_error.message=='exchange rejected')
            assert(calls==1,'concurrent waiters must share the failed refresh owner: '..calls)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn revoked_refresh_cannot_delete_or_warn_about_a_replacement_login() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = Arc::new(tokio::sync::Notify::new());
        server
            .route("/auth/oauth/token", vec![Resp::status(400, r#"{"error":"invalid_grant"}"#).hold(release.clone())]);
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;
        exec(
            &h,
            r#"
            LOGIN('expired','rejected',0,'original')
            WARNINGS={}
            local log=mare.log
            mare.log=function(level,message)
                if level=='warn' then WARNINGS[#WARNINGS+1]=message end
                log(level,message)
            end
            PREPARE=mare.task.spawn(function()
                READY=LLM:prepare(LLM:resolve('openai/gpt-6.1-terra'))
            end)
        "#,
        )
        .await;
        wait_until(WAIT_SECS, "revoked refresh dispatched", || async { server.count("/auth/oauth/token") == 1 }).await;
        exec(&h, "LOGIN('replacement','replacement-refresh',mare.task.now()+3600000,'new-account')").await;
        release.notify_one();
        exec(
            &h,
            r#"
            PREPARE:wait()
            local current=LLM:auth_get('openai-codex')
            assert(current and current.refresh=='replacement-refresh','replacement login must survive revocation')
            assert(#WARNINGS==0,'a replaced credential must not generate a revoked warning')
        "#,
        )
        .await;
        assert_eq!(server.count("/auth/oauth/token"), 1);
        h.shutdown().await;
    })
    .await
}

/// Canonical credential priority: explicit `init.lua` api_key > stored
/// login (`auth.json` OAuth or key) > environment variable, enforced in `llm/credentials.lua`.
#[tokio::test]
async fn explicit_init_lua_key_wins_over_a_stored_login_and_a_stored_login_over_env() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = server.base.clone();
        server.route("/openai", vec![Resp::sse(fixture("openai-responses.sse")); 2]);
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);

        // 1. Explicit init.lua api_key beats BOTH stored ChatGPT OAuth login and OPENAI_API_KEY env.
        let h1 = provider_host(
            &mut env,
            &server,
            json!({
                "providers": { "openai": { "api_key": "sk-init-explicit" } },
                "plugins": { "provider-openai": {
                    "base_url": format!("{base}/openai"),
                    "codex_base_url": format!("{base}/codex"),
                    "auth_base_url": format!("{base}/auth"),
                } }
            }),
        )
        .await;
        env.set_var("OPENAI_API_KEY", "sk-env-fallback");
        exec(&h1, WRITE_LOGIN).await;
        let (res1, status_src1, overridden1): (String, String, String) = eval(
            &h1,
            r#"
LOGIN("oauth-stored-tok", "r1", mare.task.now() + 3600000, "acct-1")
local r = ASK()
local st = LLM:auth_status("openai").providers[1]
return r, tostring(st.source), table.concat(st.overridden or {}, ",")
"#,
        )
        .await;
        assert_eq!(res1, "ok");
        assert_eq!(status_src1, "init.lua");
        assert!(overridden1.contains("auth.json (openai-codex)"), "{overridden1}");
        assert!(overridden1.contains("env OPENAI_API_KEY"), "{overridden1}");
        assert_eq!(server.count("/codex"), 0, "stored OAuth must not be used when init.lua sets api_key");
        assert_eq!(server.count("/openai"), 1);
        assert_eq!(server.requests()[0].header("authorization"), Some("Bearer sk-init-explicit"));
        h1.shutdown().await;

        // 2. Without init.lua api_key, stored ChatGPT OAuth beats OPENAI_API_KEY env.
        let h2 = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": { "provider-openai": {
                    "base_url": format!("{base}/openai"),
                    "codex_base_url": format!("{base}/codex"),
                    "auth_base_url": format!("{base}/auth"),
                } }
            }),
        )
        .await;
        env.set_var("OPENAI_API_KEY", "sk-env-fallback");
        exec(&h2, WRITE_LOGIN).await;
        let res2: String =
            eval(&h2, r#"LOGIN("oauth-stored-tok", "r1", mare.task.now() + 3600000, "acct-2"); return ASK()"#).await;
        assert_eq!(res2, "ok");
        assert_eq!(server.count("/codex"), 1);
        assert_eq!(server.count("/openai"), 1, "env key must not override stored OAuth");
        assert_eq!(codex_auths(&server), ["Bearer oauth-stored-tok"]);
        h2.shutdown().await;

        // 3. Without init.lua api_key or stored login, OPENAI_API_KEY env is used.
        let h3 = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": { "provider-openai": {
                    "base_url": format!("{base}/openai"),
                    "codex_base_url": format!("{base}/codex"),
                    "auth_base_url": format!("{base}/auth"),
                } }
            }),
        )
        .await;
        env.set_var("OPENAI_API_KEY", "sk-env-only");
        exec(&h3, WRITE_LOGIN).await;
        let res3: String = eval(
            &h3,
            r#"mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1 }), { mode = 384 }); return ASK()"#,
        )
        .await;
        assert_eq!(res3, "ok");
        assert_eq!(server.count("/openai"), 2);
        let openai_reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path.starts_with("/openai")).collect();
        assert_eq!(openai_reqs[1].header("authorization"), Some("Bearer sk-env-only"));
        h3.shutdown().await;
    })
    .await
}

/// Scoped `llm:auth_keys(prefix?)`: returns sorted key names only (never credential values or
/// `"version"`), never runs `key_cmd`, handles `not_found` -> `{}`, distinguishes `io` vs `corrupt`,
/// supports literal prefix filtering, and handles address-projection ambiguity safely.
#[tokio::test]
async fn auth_keys_enumerates_names_safely_with_prefix_and_error_distinctions() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let marker = env.cwd.join("cmd-ran");
        let cmd = format!("touch '{}'; echo secret-cmd-val", marker.display());
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "deepseek": { "api_key": { "cmd": ["sh", "-c", cmd] } }
                }
            }),
        )
        .await;
        expose(&h);

        // 1. Missing auth.json -> {} (not an error)
        let missing: Vec<String> = eval(&h, r#"return LLM:auth_keys()"#).await;
        assert!(missing.is_empty(), "missing auth.json must return empty array: {missing:?}");

        // 2. Stored credentials + non-provider keys ("mcp:a.b", "mcp:a_c"): sorted names, no values,
        //    no "version", literal prefix filtering (`.` does not act as wildcard).
        let (all_keys, mcp_keys, literal_keys): (Vec<String>, Vec<String>, Vec<String>) = eval(
            &h,
            r#"
            LLM:auth_set("openai-codex", { type = "oauth", access = "tok-secret", refresh = "ref-secret" })
            LLM:auth_set("mcp:a.b", { type = "api_key", key = "secret-mcp-1" })
            LLM:auth_set("mcp:a_c", { type = "api_key", key = "secret-mcp-2" })
            LLM:auth_set("anthropic", { type = "api_key", key = "secret-anth" })
            return LLM:auth_keys(), LLM:auth_keys("mcp:"), LLM:auth_keys("mcp:a.")
            "#,
        )
        .await;
        assert_eq!(all_keys, vec!["anthropic", "mcp:a.b", "mcp:a_c", "openai-codex"]);
        assert_eq!(mcp_keys, vec!["mcp:a.b", "mcp:a_c"]);
        assert_eq!(literal_keys, vec!["mcp:a.b"], "dot in prefix must match literally, not as Lua pattern");
        assert!(!marker.exists(), "auth_keys must NEVER execute configured key_cmd");

        // 3. Address/value projection safety: rejects projected placeholder prefix, literal markers
        //    (even when projection is inactive), and transformed keys inside prefix, while allowing
        //    unaffected prefixes and reporting collisions without exposing values.
        let (proj_prefix_ok, proj_prefix_err, outside_ok, outside_keys, inside_ok, inside_err, lit_ok, lit_err): (
            bool,
            String,
            bool,
            Vec<String>,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            LLM:auth_set("custom:prod-key", { type = "api_key", key = "v1" })
            LLM:auth_set("svc:supersecretpart", { type = "api_key", key = "v2" })
            LLM:auth_set("lit:[[mare-value:fake]]", { type = "api_key", key = "v3" })

            -- Literal marker in stored key rejected inside its own scope even when projection is inactive,
            -- while unrelated scope ("custom:") succeeds.
            -- Exact address collision: two distinct stored key names projecting to the exact same output string.
            LLM:auth_set("coll:firstSecret", { type = "api_key", key = "kA" })
            LLM:auth_set("coll:secondSecret", { type = "api_key", key = "kB" })
            local lease_active = mare.projection.register("activate-projection")
            local orig_project = mare.projection.project
            mare.projection.project = function(s)
              if type(s) == "string" and s:sub(1, 5) == "coll:" then
                return "coll:[[mare-value:shared-address]]", false
              end
              return orig_project(s)
            end
            local ok_coll, err_coll = pcall(function() return LLM:auth_keys("coll:") end)
            mare.projection.project = orig_project
            lease_active:close()

            local ok_lit, err_lit = pcall(function() return LLM:auth_keys("lit:") end)
            local ok_clean, res_clean = pcall(function() return LLM:auth_keys("custom:") end)

            local lease = mare.projection.register("supersecretpart")
            local ok_p, err_p = pcall(function() return LLM:auth_keys(lease.reference) end)
            local ok_inside, err_inside = pcall(function() return LLM:auth_keys("svc:") end)
            lease:close()
            assert(
              not ok_coll
                and tostring(err_coll):find("collide", 1, true) ~= nil
                and not tostring(err_coll):find("firstSecret", 1, true)
                and not tostring(err_coll):find("secondSecret", 1, true),
              "exact projected address collision must fail with collision error without exposing keys: " .. tostring(err_coll)
            )
            return ok_p, tostring(err_p), ok_clean, ok_clean and res_clean or {}, ok_inside, tostring(err_inside), ok_lit, tostring(err_lit)
            "#,
        )
        .await;
        assert!(!proj_prefix_ok, "projected value in prefix must be rejected: {proj_prefix_err}");
        assert!(outside_ok && outside_keys == vec!["custom:prod-key"], "{outside_keys:?}");
        assert!(!inside_ok, "transformed key inside requested prefix must be rejected: {inside_err}");
        assert!(!inside_err.contains("supersecretpart"), "error must remain value-free: {inside_err}");
        assert!(!lit_ok, "literal marker inside requested prefix must be rejected even when inactive: {lit_err}");
        assert!(!lit_err.contains("fake"), "error must remain value-free: {lit_err}");

        // 4. Corrupt auth.json -> typed corrupt error.
        let (corrupt_ok, corrupt_code): (bool, String) = eval(
            &h,
            r#"
            mare.fs.write(mare.paths.auth, "{broken json", { mode = 384 })
            local ok, err = pcall(LLM.auth_keys, LLM)
            return ok, type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert!(!corrupt_ok && corrupt_code.contains("corrupt"), "expected corrupt error, got {corrupt_code}");

        // 5. IO error (auth path is a directory, and symlink loop) -> typed io error.
        std::fs::remove_file(env.config_dir().join("auth.json")).unwrap();
        std::fs::create_dir(env.config_dir().join("auth.json")).unwrap();
        let (io_ok, io_code): (bool, String) = eval(
            &h,
            r#"
            local ok, err = pcall(LLM.auth_keys, LLM)
            return ok, type(err) == "table" and tostring(err.code) or tostring(err)
            "#,
        )
        .await;
        assert!(!io_ok && io_code.contains("io"), "expected io error, got {io_code}");

        #[cfg(unix)]
        {
            std::fs::remove_dir(env.config_dir().join("auth.json")).unwrap();
            let auth_path = env.config_dir().join("auth.json");
            std::os::unix::fs::symlink(&auth_path, &auth_path).unwrap();
            let (loop_ok, loop_code): (bool, String) = eval(
                &h,
                r#"
                local ok, err = pcall(LLM.auth_keys, LLM)
                return ok, type(err) == "table" and tostring(err.code) or tostring(err)
                "#,
            )
            .await;
            assert!(!loop_ok && loop_code.contains("io"), "symlink loop must normalize to typed io error: {loop_code}");
        }

        h.shutdown().await;
    })
    .await
}

/// OAuth tokens refresh proactively inside the 5-minute early window (not
/// outside it), `refresh_token_expired` produces the actionable expired message without retrying,
/// and refresh HTTP calls pass the 15s timeout budget.
#[tokio::test]
async fn oauth_refresh_handles_proactive_5min_window_expired_message_and_15s_timeout() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(200, r#"{"access_token":"proactive-new","refresh_token":"r2","expires_in":3600}"#),
                Resp::status(400, r#"{"error":{"code":"refresh_token_expired","message":"expired"}}"#),
            ],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = codex_host(&mut env, &server).await;
        exec(&h, WRITE_LOGIN).await;

        // 1. Outside the 5-minute window (10 min left): no refresh request.
        let ok_fresh: String =
            eval(&h, r#"LOGIN("still-fresh", "r0", mare.task.now() + 10 * 60 * 1000, "acct"); return ASK()"#).await;
        assert_eq!(ok_fresh, "ok");
        assert_eq!(server.count("/auth/oauth/token"), 0, "token with 10 min left must not refresh");

        // 2. Inside the 5-minute window (2 min left): proactively refreshes with 15s HTTP timeout.
        let (ok_proactive, refresh_timeout_ms): (String, i64) = eval(
            &h,
            r#"local seen_timeout = 0
               local orig = mare.http.request
               mare.http.request = function(opts)
                 if type(opts) == "table" and type(opts.url) == "string" and opts.url:find("/oauth/token", 1, true) then
                   seen_timeout = opts.timeout_ms or 0
                 end
                 return orig(opts)
               end
               LOGIN("soon-expiring", "r1", mare.task.now() + 2 * 60 * 1000, "acct")
               local res = ASK()
               mare.http.request = orig
               return res, seen_timeout"#,
        )
        .await;
        assert_eq!(ok_proactive, "ok");
        assert_eq!(refresh_timeout_ms, 15_000, "OAuth refresh must use a 15s HTTP timeout");
        assert_eq!(server.count("/auth/oauth/token"), 1, "token inside 5-min window must refresh proactively");
        assert_eq!(codex_auths(&server), ["Bearer still-fresh", "Bearer proactive-new"]);

        // 3. `refresh_token_expired` reports the expired message and is not retried.
        let expired: String = eval(&h, r#"LOGIN("old", "r-exp", 0, "acct"); return ASK()"#).await;
        assert!(
            expired.contains("refresh token has expired") && expired.contains("sign in again"),
            "unexpected expired message: {expired}"
        );
        assert_eq!(server.count("/auth/oauth/token"), 2, "expired refresh token must not retry");
        h.shutdown().await;
    })
    .await
}

/// `apiKeyHelper` serializes concurrent lookups (one execution shared by all
/// waiters) and honors `expires_at` returned in the helper's JSON output.
#[tokio::test]
async fn api_key_helper_serializes_concurrent_calls_and_respects_expires_at() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let runs = env.cwd.join("helper-mutex-runs");
        let mode = env.cwd.join("helper-mode");
        std::fs::write(&mode, "long").unwrap();
        let cmd = format!(
            concat!(
                "n=$(( $(cat '{0}' 2>/dev/null || echo 0) + 1 )); ",
                "echo $n > '{0}'; ",
                "m=$(cat '{1}'); ",
                "if [ \"$m\" = \"long\" ]; then sleep 0.15; exp=$(( $(date +%s) + 3600 )); ",
                "else exp=$(( $(date +%s) - 10 )); fi; ",
                "printf '{{\"key\":\"key-%d\",\"expires_at\":%d}}\\n' \"$n\" \"$exp\""
            ),
            runs.display(),
            mode.display()
        );
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "deepseek": {
                        "api_key": { "cmd": ["sh", "-c", cmd], "refresh_interval_ms": 600000 }
                    }
                }
            }),
        )
        .await;

        // 1. Two concurrent calls share one helper execution via the per-provider mutex.
        let (k1, k2): (String, String) = eval(
            &h,
            r#"local tx, rx = mare.task.channel()
               for _ = 1, 2 do
                 mare.task.spawn(function()
                   tx:send(tostring(LLM:api_key("deepseek")))
                 end)
               end
               return rx:recv(), rx:recv()"#,
        )
        .await;
        assert_eq!((k1.as_str(), k2.as_str()), ("key-1", "key-1"));
        assert_eq!(std::fs::read_to_string(&runs).unwrap().trim(), "1", "concurrent callers must run helper once");

        // 2. Helper returning a past `expires_at` expires immediately on the next lookup even
        //    though `refresh_interval_ms` is 10 minutes.
        std::fs::write(&mode, "expired").unwrap();
        exec(&h, r#"LLM:invalidate_api_key("deepseek")"#).await;
        let (k3, k4): (String, String) = eval(
            &h,
            r#"local a = tostring(LLM:api_key("deepseek"))
               local b = tostring(LLM:api_key("deepseek"))
               return a, b"#,
        )
        .await;
        assert_eq!(k3, "key-2");
        assert_eq!(k4, "key-3", "past expires_at must force a fresh helper invocation");
        assert_eq!(std::fs::read_to_string(&runs).unwrap().trim(), "3");
        h.shutdown().await;
    })
    .await
}

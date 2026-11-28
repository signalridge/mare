//! login and provider management — logout removes every credential of a provider
//! and revokes the ChatGPT token, the /logout picker, device-code login, per-provider settings
//! (retries, headers, query parameters), /model grouped by credentials, aliases, fallback models,
//! the first-run trigger, refresh failure classification, corrupt state files and `mare login`.

use std::sync::Arc;

use crate::common::*;
use mare_host::Host;
use serde_json::{Value, json};

/// access token whose claims carry the ChatGPT account id; id token with an e-mail
const ACCESS: &str =
    "eyJhbGciOiAibm9uZSJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOiB7ImNoYXRncHRfYWNjb3VudF9pZCI6ICJhY2N0LTEifX0.sig";
const ID_TOKEN: &str = "eyJlbWFpbCI6ICJhbGljZUBleGFtcGxlLmNvbSJ9.eyJlbWFpbCI6ICJhbGljZUBleGFtcGxlLmNvbSJ9.sig";

async fn host(env: &TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    let base = &server.base;
    let h = env
        .host()
        .config(json!({
            "providers": { "deepseek": { "base_url": format!("{base}/compat") } },
            "plugins": {
                "provider-openai": {
                    "base_url": format!("{base}/openai"),
                    "codex_base_url": format!("{base}/codex"),
                    "auth_base_url": format!("{base}/auth"),
                    "transport": "http",
                },
                "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                "provider-google": { "base_url": format!("{base}/google") },
                "llm": { "retry_base_ms": 1 },
            },
        }))
        .config(extra)
        .no_session()
        .build()
        .await;
    global(&h, "LLM", h.service_table("llm").unwrap());
    h
}

const BOTH_LOGINS: &str = r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "acc", refresh = "ref-1", expires = 0, account_id = "acct" },
  openai = { type = "api_key", key = "sk-stored-0000000000" },
  anthropic = { type = "api_key", key = "sk-ant-stored-000000" },
}), { mode = 384 })"#;

/// `/logout openai` also removes the ChatGPT login stored under `openai-codex`, revoking its
/// refresh token first; other providers' credentials stay.
#[tokio::test]
async fn logout_removes_every_credential_of_the_provider_and_revokes_the_token() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/auth/oauth/revoke", vec![Resp::status(200, "{}")]);
        let h = host(&env, &server, json!({})).await;
        exec(&h, BOTH_LOGINS).await;
        let (removed, left): (String, String) = eval(
            &h,
            r#"local r = LLM:logout("openai")
               table.sort(r.removed)
               local t = mare.json.decode(mare.fs.read(mare.paths.auth))
               local keys = {}
               for k in pairs(t) do keys[#keys + 1] = k end
               table.sort(keys)
               return table.concat(r.removed, ","), table.concat(keys, ",")"#,
        )
        .await;
        assert_eq!(removed, "openai,openai-codex");
        assert_eq!(left, "anthropic,version", "another provider's key and the file version stay");
        let revoke = server.requests().into_iter().find(|r| r.path.starts_with("/auth/oauth/revoke")).unwrap();
        let body = revoke.json();
        assert_eq!(body["token"], "ref-1");
        assert_eq!(body["token_type_hint"], "refresh_token");
        h.shutdown().await;
    })
    .await
}

/// A revoke that fails doesn't keep the login (best effort, Codex revoke.rs).
#[tokio::test]
async fn a_failed_revoke_still_logs_out() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/auth/oauth/revoke", vec![Resp::status(500, "{}")]);
        let h = host(&env, &server, json!({})).await;
        exec(&h, BOTH_LOGINS).await;
        let (warnings, gone): (i64, bool) = eval(
            &h,
            r#"local r = LLM:logout("openai")
               return #r.warnings, LLM:auth_get("openai-codex") == nil"#,
        )
        .await;
        assert_eq!((warnings, gone), (1, true));
        h.shutdown().await;
    })
    .await
}

/// `/logout` without a provider: a picker of the providers with stored credentials.
#[tokio::test]
async fn logout_without_a_provider_opens_a_picker_of_the_logged_in_ones() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/auth/oauth/revoke", vec![Resp::status(200, "{}")]);
        let h = host(&env, &server, json!({})).await;
        exec(&h, BOTH_LOGINS).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"mare.ui.connected = function() return true end
               PICKED = nil
               mare.ui.select = function(t)
                 local labels = {}
                 for _, it in ipairs(t.items) do labels[#labels + 1] = it.label end
                 PICKED = table.concat(labels, ",")
                 for i, it in ipairs(t.items) do if it.label == "anthropic" then return i end end
               end
               CM:run("/logout")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the anthropic logout", || {
            let h = h.clone();
            async move { eval::<bool>(&h, r#"return LLM:auth_get("anthropic") == nil"#).await }
        })
        .await;
        let (picked, codex): (String, bool) = eval(&h, r#"return PICKED, LLM:auth_get("openai-codex") ~= nil"#).await;
        assert_eq!(picked, "anthropic,openai");
        assert!(codex, "only the picked provider is logged out");
        h.shutdown().await;
    })
    .await
}

/// Device-code login (Codex device_code_auth.rs): user code, a pending poll, then the code
/// exchange with the device redirect URI; the login is stored with the account and e-mail.
#[tokio::test]
async fn device_code_login_polls_until_approved_and_stores_the_login() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/api/accounts/deviceauth/usercode",
            vec![Resp::status(200, r#"{"device_auth_id":"dev-1","user_code":"ABCD-EFGH","interval":"1"}"#)],
        );
        server.route(
            "/auth/api/accounts/deviceauth/token",
            vec![
                Resp::status(403, "{}"),
                Resp::status(200, r#"{"authorization_code":"auth-code","code_challenge":"c","code_verifier":"ver"}"#),
            ],
        );
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(
                200,
                format!(r#"{{"access_token":"{ACCESS}","id_token":"{ID_TOKEN}","refresh_token":"r1","expires_in":3600}}"#),
            )],
        );
        let h = host(&env, &server, json!({})).await;
        let said: String = eval(
            &h,
            r#"local said = {}
               LLM:login("openai", { method = "device", say = function(t) said[#said + 1] = t end, ask = function() end })
               return table.concat(said, "\n")"#,
        )
        .await;
        assert!(said.contains("ABCD-EFGH") && said.contains("/auth/codex/device"), "{said}");
        assert_eq!(server.count("/auth/api/accounts/deviceauth/token"), 2, "one pending poll, then approved");
        let tok = server.requests().into_iter().find(|r| r.path.starts_with("/auth/oauth/token")).unwrap();
        assert!(tok.body.contains("code=auth-code") && tok.body.contains("code_verifier=ver"), "{}", tok.body);
        assert!(tok.body.contains("deviceauth%2Fcallback"), "the device redirect URI: {}", tok.body);
        let (acct, status): (String, String) = eval(
            &h,
            r#"local a = LLM:auth_get("openai-codex")
               local lines = {}
               for _, l in ipairs(LLM:status_lines()) do lines[#lines + 1] = l.text end
               return a.account_id, table.concat(lines, "\n")"#,
        )
        .await;
        assert_eq!(acct, "acct-1");
        assert!(status.contains("openai: ChatGPT (al***@example.com"), "{status}");
        h.shutdown().await;
    })
    .await
}

/// `/status` / `mare login status` / doctor name the sources a stored login overrides.
#[tokio::test]
async fn status_lists_overridden_sources_with_a_warning() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "sk-env-1111111111111");
        let server = MockServer::start().await;
        let h = host(&env, &server, json!({})).await;
        exec(&h, BOTH_LOGINS).await;
        let lines: String = eval(
            &h,
            r#"local out = {}
               for _, l in ipairs(LLM:status_lines()) do out[#out + 1] = (l.warning and "W " or "") .. l.text end
               return table.concat(out, "\n")"#,
        )
        .await;
        assert!(lines.contains("openai: ChatGPT"), "{lines}");
        assert!(lines.contains("W   ⚠ openai also has auth.json, env OPENAI_API_KEY"), "{lines}");
        assert!(lines.contains("google: not logged in — run `mare login google` or set GEMINI_API_KEY"), "{lines}");
        assert!(!lines.contains("sk-env-1111111111111") && !lines.contains("sk-stored"), "secrets are masked: {lines}");
        h.shutdown().await;
    })
    .await
}

/// mare.provider{ retries, headers, query_params }: per provider, values from env / literal.
#[tokio::test]
async fn provider_settings_apply_per_provider() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        env.set_var("MY_TENANT", "tenant-7");
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::status(500, r#"{"error":{"message":"boom"}}"#)]);
        let h = host(
            &env,
            &server,
            json!({ "providers": { "deepseek": {
                "retries": 0,
                "headers": { "x-tenant": "MY_TENANT", "x-fixed": "lit" },
                "query_params": { "api-version": "2025-01-01" },
            } } }),
        )
        .await;
        let err: String = eval(
            &h,
            r#"local m = LLM:resolve("deepseek/deepseek-flash")
               local r = LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               return r.error"#,
        )
        .await;
        assert!(err.contains("boom"), "{err}");
        assert_eq!(server.count("/compat"), 1, "retries = 0: no retry");
        let r = &server.requests()[0];
        assert_eq!(r.header("x-tenant"), Some("tenant-7"));
        assert_eq!(r.header("x-fixed"), Some("lit"));
        assert!(r.path.contains("api-version=2025-01-01"), "{}", r.path);
        h.shutdown().await;
    })
    .await
}

/// Missing credentials in a headless run name `mare login` and the variable, not /login.
#[tokio::test]
async fn missing_credentials_say_how_to_log_in_headless() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = host(&env, &server, json!({ "providers": { "anthropic": { "login_hint": "ask IT for a key" } } })).await;
        let (g, a): (String, String) = eval(
            &h,
            r#"local function err(ref)
                 return LLM:stream({ model = LLM:resolve(ref), messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }).error
               end
               return err("google/gemini-2.5-pro"), err("anthropic/claude-sonnet-5-5")"#,
        )
        .await;
        assert!(g.contains("`mare login google` or set GEMINI_API_KEY"), "{g}");
        assert!(a.contains("ask IT for a key"), "login_hint: {a}");
        assert_eq!(server.requests().len(), 0, "nothing was sent");
        h.shutdown().await;
    })
    .await
}

/// /model lists the providers with credentials first; the rest are marked "needs login".
#[tokio::test]
async fn model_picker_groups_by_credentials() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        let h = host(&env, &server, json!({})).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let (first_needs, google_detail, deepseek_before_google): (i64, String, bool) = eval(
            &h,
            r#"mare.ui.connected = function() return true end
               local items
               mare.ui.select = function(t) items = t.items end
               CM:run("/model")
               local first_needs, gd, di, gi
               for i, it in ipairs(items) do
                 local needs = it.detail:find("needs login", 1, true) ~= nil or it.detail == "not configured"
                 if needs and not first_needs then first_needs = i end
                 local ref = it.label:match("(%S+)$")
                 if ref:find("^google/") then gd = gd or it.detail; gi = gi or i end
                 if ref:find("^deepseek/") then di = i end
               end
               -- nothing usable is listed after the first "needs login" entry
               for i = first_needs, #items do
                 assert(items[i].detail:find("needs login", 1, true) or items[i].detail == "not configured", items[i].label)
               end
               return first_needs, gd, di < gi"#,
        )
        .await;
        assert!(first_needs > 1);
        assert_eq!(google_detail, "needs login — /login google");
        assert!(deepseek_before_google);
        h.shutdown().await;
    })
    .await
}

/// Aliases: mare.model{ alias } and the built-in family names (newest of the family).
#[tokio::test]
async fn model_aliases_resolve() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = host(&env, &server, json!({ "model_aliases": { "fast": "deepseek/deepseek-flash" } })).await;
        let fast: String = eval(&h, r#"return LLM:resolve("fast").ref"#).await;
        assert_eq!(fast, "deepseek/deepseek-flash");
        // a family alias is the newest catalogue entry of that family (computed from the
        // catalogue here, so a new model never needs this test touched)
        let (ok, detail): (bool, String) = eval(
            &h,
            r###"local function key(id)
                 local k = {}
                 for n in id:gmatch("%d+") do if #n < 8 then k[#k + 1] = tonumber(n) end end
                 return k
               end
               local function newer(a, b)
                 local x, y = key(a), key(b)
                 for i = 1, math.max(#x, #y) do
                   if (x[i] or -1) ~= (y[i] or -1) then return (x[i] or -1) > (y[i] or -1) end
                 end
                 return false
               end
               for _, fam in ipairs({ "opus", "sonnet", "haiku" }) do
                 local got = LLM:resolve(fam).ref
                 local found = false
                 for _, m in ipairs(LLM:models()) do
                   if m.provider == "anthropic" and m.id:find(fam, 1, true) then
                     if m.ref == got then found = true end
                     if newer(m.id, got:match("[^/]+$")) then return false, fam .. ": " .. m.ref .. " is newer than " .. got end
                   end
                 end
                 if not found then return false, fam .. ": " .. got .. " is not an anthropic " .. fam end
               end
               return true, "ok""###,
        )
        .await;
        assert!(ok, "{detail}");
        h.shutdown().await;
    })
    .await
}

/// fallback_models: a missing model falls back to the next one; the answer says which.
#[tokio::test]
async fn fallback_models_answer_when_the_model_does_not_exist() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![Resp::status(404, r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#)],
        );
        server.route("/compat", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = host(&env, &server, json!({ "fallback_models": ["fast", "deepseek/deepseek-flash"], "model_aliases": { "fast": "deepseek/deepseek-flash" } })).await;
        let (provider, from, events): (String, String, i64) = eval(
            &h,
            r#"local r = LLM:stream({ model = LLM:resolve("anthropic/claude-sonnet-5-5"), messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               return r.provider_id, tostring(r.fallback_from), #(r.content or {})"#,
        )
        .await;
        assert_eq!(provider, "deepseek");
        assert_eq!(from, "anthropic/claude-sonnet-5-5");
        assert!(events > 0);
        assert_eq!(server.count("/compat"), 1, "the alias and the ref are the same model: tried once");
        h.shutdown().await;
    })
    .await
}

/// The first-run guide is needed whenever nothing is usable, init.lua or not.
#[tokio::test]
async fn onboarding_triggers_on_missing_credentials_not_on_a_missing_init_lua() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "-- exists\n").unwrap();
        let server = MockServer::start().await;
        let h = host(&env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        let none: bool = eval(&h, "return LLM:has_credentials()").await;
        assert!(!none, "no key anywhere (ollama needs none but isn't the current model): guide");
        h.shutdown().await;
        env.set_var("GEMINI_API_KEY", "g");
        let h = host(&env, &server, json!({})).await;
        assert!(eval::<bool>(&h, "return LLM:has_credentials()").await);
        h.shutdown().await;
    })
    .await
}

/// K4: refreshing a ChatGPT token: 401 / known codes are permanent (log in again), 429 / 5xx are
/// temporary and retried by the request.
#[tokio::test]
async fn refresh_failures_are_classified() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(429, r#"{"error":"slow_down"}"#),
                Resp::status(200, format!(r#"{{"access_token":"{ACCESS}","refresh_token":"r2","expires_in":3600}}"#)),
            ],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = host(&env, &server, json!({})).await;
        let ok: String = eval(
            &h,
            r#"mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct-1" } }), { mode = 384 })
               local r = LLM:stream({ model = LLM:resolve("openai/gpt-6.1-terra"), messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               return r.stop_reason"#,
        )
        .await;
        assert_ne!(ok, "error", "a 429 on refresh is retried");
        assert_eq!(server.count("/auth/oauth/token"), 2);
        h.shutdown().await;

        let server = MockServer::start().await;
        server.route("/auth/oauth/token", vec![Resp::status(400, r#"{"error":{"code":"refresh_token_reused"}}"#)]);
        let h = host(&env, &server, json!({})).await;
        let (kind, err): (String, String) = eval(
            &h,
            r#"mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct-1" } }), { mode = 384 })
               local r = LLM:stream({ model = LLM:resolve("openai/gpt-6.1-terra"), messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               return r.error_kind, r.error"#,
        )
        .await;
        assert_eq!(kind, "auth");
        assert!(err.contains("already used"), "{err}");
        assert_eq!(server.count("/auth/oauth/token"), 1, "permanent: not retried");
        h.shutdown().await;
    })
    .await
}

/// a ChatGPT token about to expire is refreshed before the prompt's first
/// request. A refresh that needs a new login refuses the prompt ("not sent", saying how to log
/// in) before anything goes to the model; a temporary failure is tried again before the prompt
/// (llm.prepare), which then goes out.
#[tokio::test]
async fn an_expired_login_is_refreshed_before_the_prompt_or_refuses_it() {
    local(async {
        let env = TestEnv::new().await;
        let expired = r#"mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct-1" } }), { mode = 384 })"#;
        let model = json!({ "model": "openai/gpt-6.1-terra" });

        let server = MockServer::start().await;
        server.route("/auth/oauth/token", vec![Resp::status(400, r#"{"error":{"code":"refresh_token_reused"}}"#)]);
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        let h = host(&env, &server, model.clone()).await;
        exec(&h, expired).await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(prompt["not_sent"], true, "{prompt}");
        let error = prompt["error"]["message"].as_str().unwrap();
        assert!(error.starts_with("model openai/gpt-6.1-terra: "), "{error}");
        assert!(error.contains("already used") && error.contains("`mare login openai`"), "{error}");
        assert_eq!(server.count("/codex"), 0, "nothing went to the model");
        h.shutdown().await;

        let server = MockServer::start().await;
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(429, r#"{"error":"slow_down"}"#),
                Resp::status(200, format!(r#"{{"access_token":"{ACCESS}","refresh_token":"r2","expires_in":3600}}"#)),
            ],
        );
        server.route("/codex", vec![Resp::sse(fixture("openai-responses.sse"))]);
        // (the fixture asks for a tool on every turn: the run ends at the turn limit, so cap it low;
        // the default 200 turns take ~10 s idle and minutes on a starved box)
        let capped = json!({ "model": "openai/gpt-6.1-terra", "plugins": { "agent": { "max_turns": 2 } } });
        let h = host(&env, &server, capped).await;
        exec(&h, expired).await;
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["not_sent"].is_null(), "a temporary failure doesn't refuse: {}", prompt["error"]);
        assert_eq!(server.count("/auth/oauth/token"), 2, "the 429 was tried again");
        assert!(server.count("/codex") >= 1, "the prompt went out");
        h.shutdown().await;
    })
    .await
}

/// K7: a corrupt state.json is never overwritten; a copy goes to state.json.bak.
#[tokio::test]
async fn a_corrupt_state_file_is_kept_and_backed_up() {
    local(async {
        let env = TestEnv::new().await;
        let state = env.home.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let h = env.host().faux(json!([])).no_session().build().await;
        std::fs::write(state.join("state.json"), "{\"model\": \"faux/echo\",").unwrap();
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (ok, err): (bool, String) =
            eval(&h, r#"local ok, err = pcall(mare.state.set, "effort", "high") return ok, tostring(err)"#).await;
        assert!(!ok && err.contains("not overwritten"), "{err}");
        assert_eq!(std::fs::read_to_string(state.join("state.json")).unwrap(), "{\"model\": \"faux/echo\",");
        assert!(state.join("state.json.bak").exists());
        // a choice made meanwhile is not saved, and nothing breaks
        exec(&h, r#"LLM:set_effort("high")"#).await;
        assert_eq!(std::fs::read_to_string(state.join("state.json")).unwrap(), "{\"model\": \"faux/echo\",");
        h.shutdown().await;
        std::fs::write(state.join("state.json"), "{\"version\": 1, \"a\": 1}").unwrap();
        let h = env.host().faux(json!([])).no_session().build().await;
        let a: i64 = eval(&h, r#"mare.state.set("b", 2) local s = mare.state.read() return s.a + s.b"#).await;
        assert_eq!(a, 3, "update keeps the other keys");
        h.shutdown().await;
    })
    .await
}

/// `mare login <provider> --with-api-key` (key on stdin), `mare login status`, `mare logout`.
#[tokio::test]
async fn login_subcommands() {
    use std::io::Write;
    let env = TestEnv::new().await;
    let run = |args: &[&str], stdin: Option<&str>| {
        let mut c = mare_cmd();
        c.args(args)
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = ChildGroup::spawn(&mut c).unwrap();
        let mut sin = child.0.stdin.take().unwrap();
        if let Some(s) = stdin {
            sin.write_all(s.as_bytes()).unwrap();
        }
        drop(sin);
        let o = wait_output_timeout(child, DEFAULT_CMD_TIMEOUT);
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (code, out) = run(&["login", "anthropic", "--with-api-key"], Some("sk-ant-cli-0123456789\n"));
    assert_eq!(code, 0, "{out}");
    let auth = std::fs::read_to_string(env.config_dir().join("auth.json")).unwrap();
    assert!(auth.contains("sk-ant-cli-0123456789"));
    let (code, out) = run(&["login", "status"], None);
    assert_eq!(code, 0);
    assert!(out.contains("anthropic: API key (sk-…6789, auth.json)"), "{out}");
    let (code, out) = run(&["login", "anthropic"], None);
    assert_eq!(code, 2, "an API key never comes from a visible prompt: {out}");
    let (code, out) = run(&["logout"], None);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("logged out of anthropic"), "{out}");
    assert!(!std::fs::read_to_string(env.config_dir().join("auth.json")).unwrap().contains("sk-ant"));
    assert_registered(env.home.path());
}

/// Non-openai providers declaring `oauth_key` receive `method = "auto"` on `mare login <provider>`
/// without a core provider-name special case.
#[tokio::test]
async fn login_cli_derives_auto_method_from_provider_oauth_declaration() {
    let env = TestEnv::new().await;
    let plugins = env.config_dir().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("acme_oauth.lua"),
        r#"return {
  name = "acme-oauth",
  inject = { "llm" },
  apply = function(ctx)
    ctx.llm:register_provider({
      provider = "acme",
      label = "Acme",
      oauth_key = "acme-oauth",
      oauth_label = "Acme OAuth",
      login = function(lio)
        lio.say("acme-method=" .. tostring(lio.method))
      end,
    })
  end,
}"#,
    )
    .unwrap();
    let run = |args: &[&str]| {
        let mut c = mare_cmd();
        c.args(args)
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let o = c.run_output();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (code, out) = run(&["login", "acme"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("acme-method=auto"), "expected method=auto from oauth_key declaration, got: {out}");
    let (code, out) = run(&["login", "acme", "--oauth"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("acme-method=oauth"), "expected method=oauth from --oauth, got: {out}");
    let (code, out) = run(&["login"]);
    assert_eq!(code, 2, "bare non-TTY `mare login` without default model must exit 2: {out}");
    assert!(
        out.contains("acme") && out.contains("openai") && out.contains("anthropic"),
        "expected login-capable providers list, got: {out}"
    );
    assert!(!out.contains("acme-method="), "bare `mare login` must not invoke a provider before resolution: {out}");
    let (code, out) = run(&["login", "anthropic", "--device-auth"]);
    assert_eq!(code, 2, "unsupported method --device-auth on anthropic must exit 2 before login: {out}");
    assert!(out.contains("supported") && out.contains("api_key"), "expected supported methods in error: {out}");
    let (code, out) = run(&["login", "anthropic", "--oauth"]);
    assert_eq!(code, 2, "unsupported method --oauth on anthropic must exit 2 before login: {out}");
    assert!(out.contains("supported") && out.contains("api_key"), "expected supported methods in error: {out}");
    let (code, out) = run(&["login", "acme", "--device-auth"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("acme-method=device"), "{out}");

    // With configured default model, bare `mare login` resolves that model's provider.
    std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup { model = "acme/default-model" }"#).unwrap();
    std::fs::write(
        plugins.join("acme_oauth.lua"),
        r#"return {
  name = "acme-oauth",
  inject = { "llm" },
  apply = function(ctx)
    ctx.llm:register_provider({
      provider = "acme",
      label = "Acme",
      oauth_key = "acme-oauth",
      oauth_label = "Acme OAuth",
      models = { { model_id = "default-model" } },
      login = {
        methods = { "oauth", "device" },
        run = function(lio)
          lio.say("acme-method=" .. tostring(lio.method))
        end,
      },
    })
  end,
}"#,
    )
    .unwrap();
    let (code, out) = run(&["login"]);
    assert_eq!(code, 0, "bare `mare login` with configured default model must resolve provider: {out}");
    assert!(
        out.contains("acme-method=oauth") || out.contains("acme-method=device"),
        "expected resolved concrete method for descriptor provider, got: {out}"
    );
    assert!(!out.contains("acme-method=auto"), "descriptor provider must never receive raw 'auto': {out}");
    std::fs::remove_file(env.config_dir().join("init.lua")).unwrap();

    // TTY --with-api-key hint must derive env/provider from provider declarations rather than hardcoding openai.
    local(async {
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let hint: String = eval(
            &h,
            r#"
        local req = { action = "login", provider = "anthropic", with_api_key_tty = true, say = function() end, code = 0 }
        C.cordis:get("bus"):emit_checked("login/cli", req)
        assert(req.code == 2, "expected exit code 2")
        return req.tty_hint
        "#,
        )
        .await;
        assert!(
            hint.contains("printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key"),
            "expected provider-derived TTY hint, got: {hint}"
        );
        h.shutdown().await;
    })
    .await;

    // Any error thrown by a login/cli listener must fail the CLI with non-zero exit code.
    std::fs::write(
        plugins.join("fail_cli.lua"),
        r#"return {
  name = "fail-cli",
  apply = function(ctx)
    ctx:on("login/cli", function() error("login listener boom") end)
  end,
}"#,
    )
    .unwrap();
    let (code, out) = run(&["login", "acme"]);
    assert_ne!(code, 0, "erroring login/cli handler must return non-zero exit code; stdout={out}");
}

/// mare.provider{ proxy }: the request goes through that proxy; NO_PROXY still applies
/// .
#[tokio::test]
async fn a_provider_proxy_is_used_and_no_proxy_still_applies() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("NO_PROXY", "127.0.0.1");
        let proxy = MockServer::start().await;
        proxy.route("http://", vec![Resp::status(200, r#"{"via":"proxy"}"#)]);
        let direct = MockServer::start().await;
        direct.route("/direct", vec![Resp::status(200, r#"{"via":"direct"}"#)]);
        let direct_port = direct.base.rsplit(':').next().unwrap();
        let h = env.host().no_session().build().await;
        global(&h, "P", proxy.base.as_str());
        global(&h, "PROXIED_URL", format!("http://localhost:{direct_port}/via-proxy"));
        global(&h, "DIRECT_URL", format!("http://127.0.0.1:{direct_port}/direct"));
        let (via_proxy, via_direct): (String, String) = eval(
            &h,
            r#"local r1 = mare.http.request({ url = PROXIED_URL, proxy = P, timeout_ms = 5000 })
               local r2 = mare.http.request({ url = DIRECT_URL, proxy = P, timeout_ms = 5000 })
               return r1:json().via, r2:json().via"#,
        )
        .await;
        assert_eq!(via_proxy, "proxy");
        assert_eq!(via_direct, "direct");
        let proxy_paths: Vec<String> = proxy.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(proxy_paths, [format!("http://localhost:{direct_port}/via-proxy")]);
        let direct_paths: Vec<String> = direct.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(direct_paths, ["/direct"]);
        h.shutdown().await;
    })
    .await
}

/// Cancelling OpenAI's login chooser, passing an unknown/legacy method, or returning `false`
/// from a provider `login.run` must fail and NEVER emit `llm/login`; `login/cli` on a TTY
/// opens a provider selector when no default model is configured.
#[tokio::test]
async fn login_cancel_unknown_method_and_tty_selector_contract() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (cancel_ok, cancel_err, cancel_events, legacy_ok, legacy_err, tty_code, tty_method, tty_logged): (
            bool,
            String,
            i64,
            bool,
            String,
            i64,
            String,
            i64,
        ) = eval(
            &h,
            r#"
            local logged = 0
            C.cordis:on("llm/login", function() logged = logged + 1 end)

            local ok1, err1 = pcall(LLM.login, LLM, "openai", { choose = function() return nil end })
            local events_after_cancel = logged

            local ok2, err2 = pcall(LLM.login, LLM, "openai", { method = "chatgpt" })
            assert(logged == events_after_cancel, "legacy/unknown method must never emit llm/login")

            LLM:register_provider({
              provider = "tty-pick",
              label = "TTY Pick",
              login = {
                methods = { "oauth", "api_key" },
                run = function(lio)
                  picked_method = tostring(lio.method)
                end,
              },
            })
            LLM:register_provider({
              provider = "bad-login",
              login = { methods = {}, run = function() end },
            })
            assert(LLM:provider("bad-login") == nil, "empty login.methods must be rejected")

            LLM:register_provider({
              provider = "bad-login-2",
              login = { methods = { "oauth" }, run = "not-a-fn" },
            })
            assert(LLM:provider("bad-login-2") == nil, "non-function login.run must be rejected")

            LLM:register_provider({
              provider = "bad-login-sparse",
              login = { methods = { [1] = "oauth", [3] = "api_key" }, run = function() end },
            })
            assert(LLM:provider("bad-login-sparse") == nil, "sparse login.methods must be rejected")

            -- Descriptor advertising only {"oauth"} receives "oauth" (never raw "auto") on bare login.
            local oauth_only_method
            local reg_ok, reg_err = LLM:register_provider({
              provider = "oauth-only",
              label = "OAuth Only",
              login = {
                methods = { "oauth" },
                run = function(lio)
                  oauth_only_method = tostring(lio.method)
                end,
              },
            })
            assert(LLM:provider("oauth-only") ~= nil, "oauth-only must register: " .. tostring(reg_err))
            local bare_msgs = {}
            local bare_req = {
              action = "login",
              provider = "oauth-only",
              tty = false,
              say = function(s) bare_msgs[#bare_msgs + 1] = tostring(s) end,
              code = 0,
            }
            C.cordis:get("bus"):emit_checked("login/cli", bare_req)
            assert(bare_req.code == 0 and oauth_only_method == "oauth",
              ("bare CLI login on descriptor provider must resolve auto -> oauth, code=%s method=%s msgs=%s"):format(
                tostring(bare_req.code), tostring(oauth_only_method), table.concat(bare_msgs, "; ")
              ))

            -- Descriptor advertising only {"api_key"} receives method == "api_key" (never nil) with --with-api-key.
            local key_only_method
            local key_only_val
            assert(LLM:register_provider({
              provider = "key-only-desc",
              label = "Key Only Desc",
              login = {
                methods = { "api_key" },
                run = function(lio)
                  key_only_method = lio.method
                  key_only_val = lio.key
                end,
              },
            }))
            local key_req = {
              action = "login",
              provider = "key-only-desc",
              key = "sk-cli-desc-test",
              tty = false,
              say = function() end,
              code = 0,
            }
            C.cordis:get("bus"):emit_checked("login/cli", key_req)
            assert(key_req.code == 0 and key_only_method == "api_key" and key_only_val == "sk-cli-desc-test",
              ("with-api-key CLI login must pass method='api_key' and key, got code=%s method=%s"):format(
                tostring(key_req.code), tostring(key_only_method)
              ))

            local idx_picked
            local req = {
              action = "login",
              tty = true,
              say = function() end,
              ask = function(_title, _opts)
                local items_count = 0
                return idx_picked
              end,
              code = 0,
            }
            -- Determine index of tty-pick from login-capable providers list
            local n = 0
            for _, p in ipairs(LLM:providers()) do
              if p.login then
                n = n + 1
                if p.name == "tty-pick" then
                  idx_picked = tostring(n)
                end
              end
            end
            C.cordis:get("bus"):emit_checked("login/cli", req)
            return ok1, tostring(err1), events_after_cancel, ok2, tostring(err2), req.code, tostring(picked_method), logged
            "#,
        )
        .await;
        assert!(!cancel_ok, "dismissing chooser must fail: {cancel_err}");
        assert_eq!(cancel_events, 0, "dismissing chooser must never emit llm/login");
        assert!(!legacy_ok, "legacy 'chatgpt' method must be rejected: {legacy_err}");
        assert_eq!(tty_code, 0, "TTY provider selector must pick provider and succeed");
        assert_eq!(tty_method, "oauth", "descriptor provider must receive resolved method, not raw 'auto'");
        assert_eq!(tty_logged, 3, "only the valid logins emit llm/login");
        h.shutdown().await;
    })
    .await
}

/// Cancelling OpenAI device-code polling propagates cancellation immediately instead of falling back
/// to browser OAuth or emitting `llm/login`, and post-poll cancellation before `auth_set` writes no login.
#[tokio::test]
async fn device_code_cancel_does_not_fall_back_to_browser_or_emit_login() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/auth/api/accounts/deviceauth/usercode",
            vec![
                Resp::status(200, r#"{"device_auth_id":"dev-1","user_code":"ABCD-EFGH","interval":"5"}"#),
                Resp::status(200, r#"{"device_auth_id":"dev-2","user_code":"WXYZ-1234","interval":"1"}"#),
            ],
        );
        server.route(
            "/auth/api/accounts/deviceauth/token",
            vec![Resp::status(403, "{}"), Resp::status(200, r#"{"authorization_code":"ac-2","code_verifier":"cv-2"}"#)],
        );
        // Second attempt cancels deterministically once `/auth/oauth/token` exchange is reached,
        // holding a VALID token response so only cancellation can prevent credential write.
        let exchange_release = Arc::new(tokio::sync::Notify::new());
        server.route(
            "/auth/oauth/token",
            vec![
                Resp::status(
                    200,
                    format!(r#"{{"access_token":"{ACCESS}","id_token":"{ID_TOKEN}","refresh_token":"r-cancel","expires_in":3600}}"#),
                )
                .hold(exchange_release.clone()),
            ],
        );
        let h = host(&env, &server, json!({})).await;
        expose(&h);
        let (ok, err, logged, said): (bool, String, i64, String) = eval(
            &h,
            r#"
            local logged = 0
            local said = {}
            local say_seen = false
            C.cordis:on("llm/login", function() logged = logged + 1 end)
            local cancel = mare.task.token()
            mare.task.spawn(function()
              while not say_seen do
                mare.task.sleep(5)
              end
              mare.task.sleep(15)
              cancel:cancel()
            end)
            local ok, err = pcall(LLM.login, LLM, "openai", {
              method = "device",
              cancel = cancel,
              say = function(t)
                said[#said + 1] = tostring(t)
                say_seen = true
              end,
              ask = function() end,
            })
            return ok, tostring(err), logged, table.concat(said, "\n")
            "#,
        )
        .await;
        assert!(!ok, "cancelled device login must fail: {err}");
        assert!(err.to_lowercase().contains("cancel"), "expected cancellation error: {err}");
        assert_eq!(logged, 0, "cancelled device login must NEVER emit llm/login");
        assert!(
            !said.contains("falling back to the browser"),
            "must not fall back to browser login: err={err} said={said}"
        );
        assert!(!said.contains("/oauth/authorize"), "must not print browser OAuth URL: err={err} said={said}");
        assert!(
            said.contains("/codex/device"),
            "device flow itself ran before cancel: ok={ok} err={err} logged={logged} said={said}"
        );
        assert!(
            !server.requests().iter().any(|r| r.path.contains("/oauth/")),
            "browser OAuth / token exchange must not run on poll cancel: {:?}",
            server.requests()
        );

        // Second attempt: poll approves, exchange starts, cancel fires WHILE exchange is in flight.
        exec(
            &h,
            r#"
            CANCEL2 = mare.task.token()
            LOGIN2_DONE = false
            mare.task.spawn(function()
              LOGIN2_OK, LOGIN2_ERR = pcall(LLM.login, LLM, "openai", {
                method = "device",
                cancel = CANCEL2,
                say = function() end,
                ask = function() end,
              })
              LOGIN2_DONE = true
            end)
            "#,
        )
        .await;
        wait_until(5, "device login reached /auth/oauth/token exchange", || {
            let s = &server;
            async move { s.count("/auth/oauth/token") >= 1 }
        })
        .await;
        exec(&h, "CANCEL2:cancel()").await;
        exchange_release.notify_one();
        wait_until(5, "cancelled device exchange finished", || {
            let hc = h.clone();
            async move { eval::<bool>(&hc, "return LOGIN2_DONE == true").await }
        })
        .await;
        let (ok2, err2, stored): (bool, String, bool) =
            eval(&h, r#"return LOGIN2_OK == true, tostring(LOGIN2_ERR), LLM:auth_get("openai-codex") ~= nil"#).await;
        assert!(!ok2, "cancelled token exchange must fail: {err2}");
        assert!(err2.to_lowercase().contains("cancel"), "expected cancellation error: {err2}");
        assert!(!stored, "post-poll cancel during token exchange must NOT write openai-codex credential");
        h.shutdown().await;
    })
    .await
}

/// OpenAI login auto-selects device code when headless/SSH (and puts device code first in
/// the interactive picker), falls back to the second callback port when the first is busy, and
/// completes via pasted callback URL (rejecting a mismatched state).
#[tokio::test]
async fn openai_login_auto_selects_device_code_without_browser_and_supports_port_fallback_and_paste() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("SSH_CONNECTION", "10.0.0.1 55555 10.0.0.2 22");
        let server = MockServer::start().await;
        server.route(
            "/auth/api/accounts/deviceauth/usercode",
            vec![Resp::status(
                200,
                r#"{"device_auth_id":"dev-ssh","user_code":"SSH-CODE","interval":"0"}"#,
            )],
        );
        server.route(
            "/auth/api/accounts/deviceauth/token",
            vec![Resp::status(
                200,
                r#"{"authorization_code":"auth-ssh","code_verifier":"ver-ssh"}"#,
            )],
        );
        server.route(
            "/auth/oauth/token",
            vec![Resp::status(
                200,
                json!({
                    "access_token": ACCESS,
                    "refresh_token": "ref-ssh",
                    "expires_in": 3600,
                    "id_token": ID_TOKEN,
                })
                .to_string(),
            )],
        );

        // Hold port p1 busy so browser login must fall back to p2 (same mechanism as 1455 -> 1457).
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p1 = occupied.local_addr().unwrap().port();
        let free_probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p2 = free_probe.local_addr().unwrap().port();
        drop(free_probe);

        let h = host(
            &env,
            &server,
            json!({ "plugins": { "provider-openai": { "callback_ports": [p1, p2] } } }),
        )
        .await;

        // 1. Under SSH_CONNECTION, `method = "auto"` chooses device code automatically.
        let (first_choice, auto_acct): (String, String) = eval(
            &h,
            r#"local first_label = nil
               local ok_cancel = pcall(LLM.login, LLM, "openai", {
                 choose = function(_, items)
                   first_label = items[1] and items[1].label or ""
                   return nil
                 end,
               })
               assert(not ok_cancel)
               LLM:login("openai", { method = "auto", say = function() end, ask = function() end })
               return first_label, tostring((LLM:auth_get("openai-codex") or {}).account_id)"#,
        )
        .await;
        assert!(
            first_choice.contains("device code"),
            "SSH/headless picker must put device code first: {first_choice}"
        );
        assert_eq!(auto_acct, "acct-1");
        assert_eq!(server.count("/auth/api/accounts/deviceauth/usercode"), 1);

        // 2. Browser login falls back from busy p1 to p2 and accepts a pasted callback URL.
        let (bound_port_ok, paste_acct): (bool, String) = eval(
            &h,
            &format!(
                r#"mare.proc.open = function() end
                   LLM:logout("openai")
                   local url_seen = nil
                   LLM:login("openai", {{
                     method = "oauth",
                     say = function(msg) url_seen = tostring(msg) end,
                     ask = function()
                       local st = assert(url_seen:match("state=([^&%s]+)"))
                       return "http://localhost:{p2}/auth/callback?code=pasted-1&state=" .. st
                     end,
                   }})
                   local used_fallback = url_seen ~= nil and url_seen:find("localhost:{p2}/auth/callback", 1, true) ~= nil
                   return used_fallback, tostring((LLM:auth_get("openai-codex") or {{}}).account_id)"#
            ),
        )
        .await;
        assert!(bound_port_ok, "browser login must fall back from occupied {p1} to {p2}");
        assert_eq!(paste_acct, "acct-1");

        // 3. Pasted redirect URL with mismatched state is rejected.
        let mismatch_err: String = eval(
            &h,
            &format!(
                r#"local ok, err = pcall(LLM.login, LLM, "openai", {{
                     method = "oauth",
                     say = function() end,
                     ask = function()
                       return "http://localhost:{p2}/auth/callback?code=bad&state=wrong-state"
                     end,
                   }})
                   assert(not ok)
                   return type(err) == "table" and tostring(err.message) or tostring(err)"#
            ),
        )
        .await;
        assert!(mismatch_err.contains("state mismatch"), "{mismatch_err}");

        drop(occupied);
        h.shutdown().await;
    })
    .await
}

/// Provider header/query `{ cmd }` values: failures must not be cached permanently,
/// `refresh_interval_ms` must refresh expired values, and 401 must invalidate cached values.
#[tokio::test]
async fn provider_header_cmd_recovers_after_failure_refreshes_and_invalidates_on_401() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::status(401, r#"{"error":{"message":"expired token"}}"#),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );

        let counter_hdr = env.cwd.join("hdr-count.txt");
        let counter_q = env.cwd.join("qry-count.txt");
        let counter_401 = env.cwd.join("auth401-count.txt");
        let hdr_script = format!(
            "n=$(cat '{p}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{p}'; \
             if [ $n -eq 1 ]; then exit 1; fi; printf 'hdr-tok-%d' $n",
            p = counter_hdr.display()
        );
        let qry_script = format!(
            "n=$(cat '{p}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{p}'; \
             if [ $n -eq 1 ]; then exit 1; fi; printf 'qry-tok-%d' $n",
            p = counter_q.display()
        );
        let auth401_script = format!(
            "n=$(cat '{p}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{p}'; printf 'long-tok-%d' $n",
            p = counter_401.display()
        );

        let h = host(
            &env,
            &server,
            json!({ "providers": { "deepseek": {
                "retries": 0,
                "headers": {
                    "x-dyn": { "cmd": ["sh", "-c", hdr_script], "refresh_interval_ms": 200 },
                    "x-long": { "cmd": ["sh", "-c", auth401_script], "refresh_interval_ms": 60000 }
                },
                "query_params": {
                    "sig": { "cmd": ["sh", "-c", qry_script], "refresh_interval_ms": 200 }
                },
            } } }),
        )
        .await;

        let ok: bool = eval(
            &h,
            r#"local m = LLM:resolve("deepseek/deepseek-flash")
               local req = function()
                 return LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } })
               end
               -- 1st call: command fails (no header/query sent, must NOT permanently cache failure)
               assert(req().error == nil, "call 1")
               -- 2nd call: command succeeds -> produces hdr-tok-2 / qry-tok-2
               assert(req().error == nil, "call 2")
               -- 3rd call after refresh_interval_ms: re-runs command -> hdr-tok-3 / qry-tok-3
               mare.task.sleep(250)
               assert(req().error == nil, "call 3")
               -- 4th call: server returns 401 -> invalidates header/query cache (even 60s TTL) and retries
               assert(req().error == nil, "call 4 after 401 refresh")
               return true"#,
        )
        .await;
        assert!(ok);

        let reqs = server.requests();
        assert_eq!(reqs.len(), 5, "expected 5 requests (3 ok + 1 401 + 1 retry), got {}", reqs.len());
        assert_eq!(reqs[0].header("x-dyn"), None, "first call failed cmd should omit header");
        assert_eq!(reqs[1].header("x-dyn"), Some("hdr-tok-2"), "failed cmd must not be cached permanently");
        assert!(reqs[1].path.contains("sig=qry-tok-2"), "failed query cmd must not be cached permanently: {}", reqs[1].path);
        assert_eq!(reqs[2].header("x-dyn"), Some("hdr-tok-3"), "refresh_interval_ms must refresh header cmd");
        assert!(reqs[2].path.contains("sig=qry-tok-3"), "refresh_interval_ms must refresh query cmd: {}", reqs[2].path);
        assert_eq!(reqs[3].header("x-long"), Some("long-tok-1"), "long-TTL header stays cached before 401");
        assert_eq!(reqs[4].header("x-long"), Some("long-tok-2"), "401 must invalidate long-TTL header cmd cache");
        assert_ne!(reqs[3].header("x-dyn"), reqs[4].header("x-dyn"), "401 must refresh dynamic header cmd");
        assert_ne!(reqs[3].path, reqs[4].path, "401 must refresh dynamic query cmd");
        h.shutdown().await;
    })
    .await
}

/// Interactive API-key login does not print ambiguous shared-prefix advice into scrollback,
/// passes declared `key_prefixes` as transient input placeholder, warns only on real prefix
/// mismatches, and onboarding refreshes live `GET /models` before opening the default-model picker.
#[tokio::test]
async fn login_api_key_avoids_ambiguous_prefix_scrollback_and_onboarding_refreshes_live_models() {
    local(async {
        let mut env = TestEnv::new().await;
        for var in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "OPENCODE_API_KEY",
            "DEEPSEEK_API_KEY",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            env.set_var(var, "");
        }
        let server = MockServer::start().await;
        server.route(
            "/compat/models",
            vec![
                Resp::status(200, r#"{"object":"list","data":[{"id":"deepseek-live-a"},{"id":"deepseek-live-b"}]}"#),
                Resp::status(200, r#"{"object":"list","data":[{"id":"deepseek-live-a"},{"id":"deepseek-live-b"}]}"#),
            ],
        );

        let h = host(&env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;

        let res_json: String = eval(
            &h,
            r#"
            -- 1. DeepSeek (no unique key_prefixes): no say() spam, secret=true, no placeholder.
            local ds_said, ds_secret, ds_placeholder = {}, false, "unset"
            LLM:login("deepseek", {
                method = "api_key",
                say = function(t) ds_said[#ds_said + 1] = t end,
                ask = function(_, opts)
                    ds_secret = opts.secret == true
                    ds_placeholder = opts.placeholder
                    return "sk-deepseek-test-123456"
                end,
            })
            LLM:logout("deepseek")

            -- 2. Anthropic (key_prefixes = { "sk-ant-" }): placeholder is set; matching key emits no warning;
            -- non-matching key emits a clear warning.
            local ant_ok_said, ant_placeholder = {}, nil
            LLM:login("anthropic", {
                method = "api_key",
                say = function(t) ant_ok_said[#ant_ok_said + 1] = t end,
                ask = function(_, opts)
                    ant_placeholder = opts.placeholder
                    return "sk-ant-valid-123456"
                end,
            })
            LLM:logout("anthropic")

            local ant_bad_said = {}
            LLM:login("anthropic", {
                method = "api_key",
                say = function(t) ant_bad_said[#ant_bad_said + 1] = t end,
                ask = function() return "sk-not-anthropic-123456" end,
            })
            LLM:logout("anthropic")

            -- 3. Pasting an Anthropic key into DeepSeek emits a clear mismatch warning without leaking the key.
            local cross_said = {}
            local cross_secret = "sk-ant-cross-provider-999999"
            LLM:login("deepseek", {
                method = "api_key",
                say = function(t) cross_said[#cross_said + 1] = t end,
                ask = function() return cross_secret end,
            })
            LLM:logout("deepseek")

            return mare.json.encode({
                ds_said = mare.json.array(ds_said),
                ds_secret = ds_secret,
                ds_placeholder = ds_placeholder or mare.json.null,
                ant_placeholder = ant_placeholder or mare.json.null,
                ant_ok_said = mare.json.array(ant_ok_said),
                ant_bad_said = mare.json.array(ant_bad_said),
                cross_said = mare.json.array(cross_said),
                cross_leaked = table.concat(cross_said, " "):find(cross_secret, 1, true) ~= nil,
            })
            "#,
        )
        .await;
        let v: Value = serde_json::from_str(&res_json).unwrap();
        assert_eq!(v["ds_secret"], true, "{v}");
        assert_eq!(v["ds_placeholder"], Value::Null, "{v}");
        assert_eq!(v["ds_said"], json!([]), "normal deepseek login must not emit scrollback prefix text: {v}");
        assert_eq!(v["ant_placeholder"], "sk-ant-...", "{v}");
        assert_eq!(v["ant_ok_said"], json!([]), "matching prefix must not emit scrollback warning: {v}");
        assert!(
            v["ant_bad_said"].as_array().unwrap().iter().any(|s| s.as_str().unwrap_or("").contains("sk-ant-")),
            "non-matching prefix must warn about expected prefix: {v}"
        );
        assert!(
            v["cross_said"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s.as_str().unwrap_or("").contains("anthropic") && s.as_str().unwrap_or("").contains("deepseek")),
            "cross-provider key must warn clearly: {v}"
        );
        assert_eq!(v["cross_leaked"], false, "{v}");
        h.shutdown().await;
        drop(env);

        // 4. Guided onboarding flow: after entering a DeepSeek key, no prefix text is printed to
        // scrollback and the Default model picker shows the live GET /models response (not seeds).
        let server2 = MockServer::start().await;
        server2.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-live-a","name":"DeepSeek Live A"},{"id":"deepseek-live-b","name":"DeepSeek Live B"}]}"#,
            )],
        );
        let mut env2 = TestEnv::new().await;
        for var in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "OPENCODE_API_KEY",
            "DEEPSEEK_API_KEY",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            env2.set_var(var, "");
        }
        let h2 = env2
            .host()
            .builtins(&["onboarding"])
            .config(json!({
                "model": "",
                "providers": { "deepseek": { "base_url": format!("{}/compat", server2.base) } },
                "plugins": {
                    "provider-faux": { "enabled": false },
                    "onboarding": { "enabled": true, "plugins": false },
                },
            }))
            .no_session()
            .build()
            .await;
        expose(&h2);
        global(&h2, "LLM", h2.service_table("llm").unwrap());
        assert!(!eval::<bool>(&h2, "return LLM:has_credentials()").await, "onboarding requires no credentials");

        exec(
            &h2,
            r#"
            mare.mode = "tui"
            mare.ui.connected = function() return true end
            _G.__printed = {}
            _G.__notified = {}
            _G.__picked_models = nil
            _G.__done = false
            mare.ui.print = function(node)
                _G.__printed[#_G.__printed + 1] = mare.json.encode(node)
            end
            mare.ui.notify = function(msg, lvl)
                _G.__notified[#_G.__notified + 1] = tostring(lvl) .. ":" .. tostring(msg)
            end
            mare.ui.input = function(opts)
                assert(opts.secret == true, "key input must be masked")
                return "sk-deepseek-onboard-123"
            end
            mare.ui.select = function(t)
                if t.title:find("pick a provider", 1, true) then
                    for i, it in ipairs(t.items) do
                        if it.label == "deepseek" then return i end
                    end
                    error("deepseek not in provider picker")
                elseif t.title == "Default model" then
                    local labels = {}
                    for _, it in ipairs(t.items) do labels[#labels + 1] = it.label end
                    _G.__picked_models = labels
                    _G.__done = true
                    return 1
                end
            end
            C.cordis:get("bus"):emit("ui/ready")
            "#,
        )
        .await;
        wait_until(10, "onboarding Default model picker", || {
            let h2 = h2.clone();
            async move { eval::<bool>(&h2, "return _G.__done == true").await }
        })
        .await;

        let (printed_json, picked_json, cur_ref): (String, String, String) = eval(
            &h2,
            r#"return mare.json.encode(mare.json.array(_G.__printed)),
                      mare.json.encode(mare.json.array(_G.__picked_models or {})),
                      (LLM:current() and LLM:current().ref) or "" "#,
        )
        .await;
        let printed: Vec<String> = serde_json::from_str(&printed_json).unwrap();
        let picked: Vec<String> = serde_json::from_str(&picked_json).unwrap();
        assert!(printed.is_empty(), "onboarding deepseek login must not print prefix text to scrollback: {printed:?}");
        assert_eq!(
            picked,
            vec!["deepseek-live-a", "deepseek-live-b"],
            "Default model picker must show live GET /models results, not offline seeds"
        );
        assert_eq!(cur_ref, "deepseek/deepseek-live-a");
        assert!(
            server2.count("/compat/models") >= 1,
            "onboarding foreground refresh must hit GET /models"
        );
        h2.shutdown().await;
    })
    .await
}

/// Opening `/model` updates the open picker in place when `llm/providers-changed` fires after a
/// catalog refresh, and selecting the newly added item switches to that live model.
#[tokio::test]
async fn model_picker_updates_in_place_when_catalog_refreshes() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        let server = MockServer::start().await;
        server.route(
            "/compat/models",
            vec![Resp::status(200, r#"{"object":"list","data":[{"id":"deepseek-flash"},{"id":"deepseek-live-new"}]}"#)],
        );
        let h = host(&env, &server, json!({ "plugins": { "llm": { "catalog_open_refresh_ms": 999999999 } } })).await;
        global(&h, "CM", h.service_table("commands").unwrap());

        let (initial_has_new, updated_has_new, selected_ref): (bool, bool, String) = eval(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.notify = function() end
            local initial_has_new, updated_has_new = false, false
            mare.ui.select = function(opts)
                local cur_items = opts.items
                for _, it in ipairs(cur_items) do
                    if it.item_id == "deepseek/deepseek-live-new" then initial_has_new = true end
                end
                local open = true
                local handle = {
                    is_open = function() return open end,
                    update = function(_, next_items)
                        cur_items = next_items
                        return true
                    end,
                }
                if opts.on_open then opts.on_open(handle) end
                -- Simulate background refresh finishing while the picker is open.
                LLM:refresh_catalog({ name = "deepseek", force = true })
                local pick_idx
                for i, it in ipairs(cur_items) do
                    if it.item_id == "deepseek/deepseek-live-new" then
                        updated_has_new = true
                        pick_idx = i
                    end
                end
                open = false
                return pick_idx
            end
            CM:run("/model")
            local cur = LLM:current()
            return initial_has_new, updated_has_new, cur and cur.ref or ""
            "#,
        )
        .await;
        assert!(!initial_has_new, "newly listed model must not exist before refresh");
        assert!(updated_has_new, "open /model picker must update in place on llm/providers-changed");
        assert_eq!(selected_ref, "deepseek/deepseek-live-new");
        h.shutdown().await;
    })
    .await
}

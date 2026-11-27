use crate::common::*;

/// The builtin `mcp` plugin: a stdio server's tools are registered as `mcp__<server>__<tool>`
/// (unconfined) and calls go through `tools/call`.
#[tokio::test]
async fn mcp_servers_tools_are_registered_and_callable() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": { "command": fake_mcp_bin(), "args": ["simple"], "tier": "direct" } } } } });
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "the MCP tool to be registered", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__echo') ~= nil").await }
        })
        .await;
        let (unconfined, out): (bool, String) = eval(
            &h,
            r#"local _, _, unconfined = T:side_effect("mcp__fake__echo")
               local r = T:execute({ id = "m1", name = "mcp__fake__echo", arguments = { text = "hi" } }, {})
               local c = type(r.content) == "table" and r.content[1].text or r.content
               return unconfined, c"#,
        )
        .await;
        assert!(unconfined, "MCP tools run outside the local sandbox");
        assert_eq!(out, "echo: hi");
        h.shutdown().await;
    })
    .await
}

/// A stdio MCP server for the tier tests (`mare-test-mcp tiered <log> <delay> [release]`): appends
/// a line to `log` every time it starts, waits `delay` seconds before answering `initialize`, and
/// has `echo` and `shout` tools.
fn tiered_mcp_server(env: &TestEnv) -> (&'static str, std::path::PathBuf) {
    (fake_mcp_bin(), env.home.path().join("tiered_mcp.log"))
}

fn starts(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log).map(|s| s.lines().count()).unwrap_or(0)
}

/// Default tier `deferred`: nothing is declared or connected at startup; only
/// `tool_search` is. A search connects the server (first use, no cached list yet) and loads the
/// matching tool — declared from the next request on (the tools generation moves) — and the tool
/// is callable.
#[tokio::test]
async fn mcp_deferred_tools_stay_hidden_until_tool_search_loads_them() {
    local(async {
        let env = TestEnv::new().await;
        let (server, log) = tiered_mcp_server(&env);
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": {
            "command": server, "args": ["tiered", log.to_string_lossy(), "0"],
            "tools": { "shout": "hidden" } } } } } });
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        let (hidden, search): (bool, bool) =
            eval(&h, "return T:get('mcp__fake__echo') == nil, T:get('tool_search') ~= nil").await;
        assert!(hidden && search, "deferred tools aren't declared; tool_search is");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(starts(&log), 0, "a deferred server doesn't connect at startup");
        let (out, gen_moved, declared, shout, called): (String, bool, bool, bool, String) = eval(
            &h,
            r#"local g = T:revision()
               local r = T:execute({ id = "s1", name = "tool_search", arguments = { query = "echo text" } }, {})
               local text = type(r.content) == "table" and r.content[1].text or r.content
               local c = T:execute({ id = "s2", name = "mcp__fake__echo", arguments = { text = "hi" } }, {})
               return text, T:revision() ~= g, T:get('mcp__fake__echo') ~= nil,
                      T:get('mcp__fake__shout') ~= nil,
                      type(c.content) == "table" and c.content[1].text or tostring(c.content)"#,
        )
        .await;
        assert!(out.contains("mcp__fake__echo"), "{out}");
        assert!(gen_moved && declared, "the found tool is declared from the next request");
        assert!(!shout, "a per-tool `hidden` override is never loaded");
        assert_eq!(called, "echo: hi");
        assert_eq!(starts(&log), 1);
        h.shutdown().await;
    })
    .await
}

/// A deferred server whose tool list is cached from an earlier run: tool_search answers from the
/// cache without starting it, and the server starts when a loaded tool is first called.
#[tokio::test]
async fn mcp_non_direct_servers_connect_on_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let (server, log) = tiered_mcp_server(&env);
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": {
            "command": server, "args": ["tiered", log.to_string_lossy(), "0"] } } } } });
        // first run: the search connects and the tool list is cached before the next request
        let h = env.host().builtins(&["mcp"]).config(cfg.clone()).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"T:execute({ id = "s1", name = "tool_search", arguments = { query = "echo" } }, {})
               C.cordis:get("bus"):emit("request/prepare", {})"#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(starts(&log), 1);
        // second run: the search uses the cache; the call starts the server
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        let out: String = eval(
            &h,
            r#"local r = T:execute({ id = "s1", name = "tool_search", arguments = { query = "select:mcp__fake__echo" } }, {})
               return type(r.content) == "table" and r.content[1].text or r.content"#,
        )
        .await;
        assert!(out.contains("mcp__fake__echo"), "{out}");
        assert_eq!(starts(&log), 1, "tool_search answered from the cached list");
        let called: String = eval(
            &h,
            r#"local c = T:execute({ id = "s2", name = "mcp__fake__echo", arguments = { text = "yo" } }, {})
               return type(c.content) == "table" and c.content[1].text or tostring(c.content)"#,
        )
        .await;
        assert_eq!(called, "echo: yo");
        assert_eq!(starts(&log), 2, "the first call connected the server");
        h.shutdown().await;
    })
    .await
}

/// No send-time wait for `direct` servers: a prompt's prepare step
/// returns at once while the server is still connecting, `/mcp` state says "connecting", and the
/// tools join only after the turn that was running when the connect finished. A `required`
/// server is waited for instead, without a time cap, so its tools are in the first request.
///
/// Ordering, not timing: the server's `initialize` answer is parked until the test creates a
/// release file. A `direct` prompt must build its request (and finish) while the connect is still
/// parked; a `required` one must not have built a request when the test releases it.
#[tokio::test]
async fn mcp_direct_server_never_delays_the_prompt_and_joins_next_turn() {
    local(async {
        for required in [false, true] {
            let env = TestEnv::new().await;
            let (server, log) = tiered_mcp_server(&env);
            let release = env.home.path().join("mcp-release");
            let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": {
                "command": server,
                "args": ["tiered", log.to_string_lossy(), "0", release.to_string_lossy()],
                "tier": "direct", "required": required } } } } });
            let script = serde_json::json!([{ "text": "first" }, { "text": "second" }]);
            let h = env.host().builtins(&["mcp"]).faux(script).config(cfg).no_session().build().await;
            expose(&h);
            // what each request saw when it was built: (server state, mcp tool declared)
            exec(
                &h,
                r#"mare._builds = {}
                   C.cordis:on("request/build", function(req, next)
                     table.insert(mare._builds, { C.cordis:get("mcp"):list_servers().data[1].state, T:get('mcp__fake__echo') ~= nil })
                     return next()
                   end)"#,
            )
            .await;
            if required {
                // the prompt waits for the connect: parked, no request has been built yet
                let (result, builds_when_released) = tokio::join!(h.prompt("hi", serde_json::json!({})), async {
                    wait_until(WAIT_SECS, "the server to start connecting", || async { starts(&log) == 1 }).await;
                    let n: i64 = eval(&h, "return #mare._builds").await;
                    std::fs::write(&release, "go").unwrap();
                    n
                });
                assert!(result.unwrap()["error"].is_null());
                assert_eq!(builds_when_released, 0, "a required server is waited for before the first request");
                let first: (String, bool) = eval(&h, "return mare._builds[1][1], mare._builds[1][2]").await;
                assert_eq!(first, ("connected".to_string(), true), "its tools are in the first request");
            } else {
                // the connect is parked for the whole turn: the prompt must reach the provider anyway
                wait_until(WAIT_SECS, "the server to start connecting", || async { starts(&log) == 1 }).await;
                let result = h.prompt("hi", serde_json::json!({})).await.unwrap();
                assert!(result["error"].is_null() && result["text"] == "first", "{result}");
                assert!(!release.exists(), "the turn finished while the connect was still parked");
                let first: (String, bool) = eval(&h, "return mare._builds[1][1], mare._builds[1][2]").await;
                assert_eq!(first, ("connecting".to_string(), false), "the first request went out without the tools");
                // a turn is running when the connect finishes: its tools wait for the boundary
                exec(&h, r#"C.cordis:get("bus"):emit("step/started", { turn_id = "t-1", step_id = "s-1", turn = 1 })"#).await;
                std::fs::write(&release, "go").unwrap();
                let during: bool = eval(
                    &h,
                    r#"local deadline = mare.task.clock() + 60000
                       while C.cordis:get("mcp"):list_servers().data[1].state ~= "connected" do
                         assert(mare.task.clock() < deadline, "the server never connected")
                         mare.task.sleep(20)
                       end
                       return T:get('mcp__fake__echo') ~= nil"#,
                )
                .await;
                assert!(!during, "tools wait for the turn boundary");
                let after: bool = eval(
                    &h,
                    r#"C.cordis:get("bus"):emit("step/completed", { turn_id = "t-1", step_id = "s-1", status = "completed" })
                       return T:get('mcp__fake__echo') ~= nil and C.cordis:get("mcp"):list_servers().data[1].state == "connected""#,
                )
                .await;
                assert!(after, "tools declared from the next turn");
                // and the next prompt's request carries them
                let result = h.prompt("again", serde_json::json!({})).await.unwrap();
                assert!(result["error"].is_null() && result["text"] == "second", "{result}");
                let second: (String, bool) = eval(&h, "return mare._builds[2][1], mare._builds[2][2]").await;
                assert_eq!(second, ("connected".to_string(), true), "the server joined on the next turn");
            }
            h.shutdown().await;
        }
    })
    .await
}

/// Streamable HTTP through `mare.mcp` (rmcp): headers from the config reach the server, tools are
/// registered and callable. The server is a tiny mock (`mare-test-mcp http`) that answers JSON
/// (no SSE).
#[tokio::test]
async fn mcp_http_servers_tools_are_registered_and_callable() {
    local(async {
        let env = TestEnv::new().await;
        let mcp = FakeHttpMcp::spawn(env.home.path());
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "web": { "url": mcp.url(), "headers": { "x-team": "blue" }, "tier": "direct" } } } } });
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "the HTTP MCP tool to be registered", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__web__add') ~= nil").await }
        })
        .await;
        let out: String = eval(
            &h,
            r#"local r = T:execute({ id = "m1", name = "mcp__web__add", arguments = { a = 2, b = 3 } }, {})
               return type(r.content) == "table" and r.content[1].text or tostring(r.content)"#,
        )
        .await;
        h.shutdown().await;
        let headers = std::fs::read_to_string(mcp.headers_file()).unwrap();
        mcp.shutdown();
        assert_eq!(out, "5");
        assert!(headers.lines().all(|l| l == "blue"), "configured headers on every request");
    })
    .await
}

/// Lua for one `mare mcp <action> fake` request through the plugin's CLI event.
fn mcp_cli(action: &str) -> String {
    format!(
        r#"local req = {{ action = "{action}", server = "fake", lines = {{}}, code = 0 }}
           C.cordis:get("bus"):emit("mcp/cli", req) return req.code"#
    )
}

const SEARCH_ECHO: &str = r#"local r = T:execute({ id = "s", name = "tool_search", arguments = { query = "select:echo" } }, {})
   return type(r.content) == "table" and r.content[1].text or r.content"#;

/// A deferred server disabled while tool_search's first-use connect is still waiting for it
/// registers nothing when the connect completes, so the generic catalog cannot find its tools
/// (TOOL-DEFER: the catalog is the tools registry, so MCP must not register for a blocked server).
#[tokio::test]
async fn mcp_server_disabled_while_tool_search_connects_stays_unfindable() {
    local(async {
        let env = TestEnv::new().await;
        let (server, log) = tiered_mcp_server(&env);
        let release = env.home.path().join("tiered_release");
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": {
            "command": server, "args": ["tiered", log.to_string_lossy(), "0", release.to_string_lossy()] } } } } });
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        let (found, ()) = tokio::join!(eval::<String>(&h, SEARCH_ECHO), async {
            wait_until(15, "the deferred server to start", || async { starts(&log) > 0 }).await;
            assert_eq!(eval::<i64>(&h, &mcp_cli("disable")).await, 0, "disable while the connect is parked");
            std::fs::write(&release, "go").unwrap();
        });
        let registered: bool = eval(
            &h,
            r#"for _, d in ipairs(T:list()) do if d.name == "mcp__fake__echo" then return true end end
               return false"#,
        )
        .await;
        let state: String = eval(&h, r#"return C.cordis:get("mcp"):list_servers().data[1].state"#).await;
        h.shutdown().await;
        assert_eq!(state, "disabled", "a server disabled while it connected keeps no live client");
        assert!(!registered, "a disabled server's tool was registered after its connect finished");
        assert!(!found.contains("mcp__fake__echo"), "a disabled server was searchable: {found}");
    })
    .await
}

/// Disabling a deferred server withdraws its cached tools; enabling it again makes them findable
/// from the cache (without starting the server), as at startup.
#[tokio::test]
async fn mcp_reenabled_deferred_server_is_found_from_its_cache() {
    local(async {
        let env = TestEnv::new().await;
        let (server, log) = tiered_mcp_server(&env);
        let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": {
            "command": server, "args": ["tiered", log.to_string_lossy(), "0"] } } } } });
        // first run caches the tool list
        let h = env.host().builtins(&["mcp"]).config(cfg.clone()).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"T:execute({ id = "s1", name = "tool_search", arguments = { query = "echo" } }, {})
               C.cordis:get("bus"):emit("request/prepare", {})"#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(starts(&log), 1);
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        assert_eq!(eval::<i64>(&h, &mcp_cli("disable")).await, 0);
        let off: String = eval(&h, SEARCH_ECHO).await;
        assert!(!off.contains("mcp__fake__echo"), "a disabled server was searchable: {off}");
        assert_eq!(eval::<i64>(&h, &mcp_cli("enable")).await, 0);
        let on: String = eval(&h, SEARCH_ECHO).await;
        h.shutdown().await;
        assert!(on.contains("mcp__fake__echo"), "the re-enabled server's cached tool was not found: {on}");
        assert_eq!(starts(&log), 1, "found from the cache, the server did not start");
    })
    .await
}

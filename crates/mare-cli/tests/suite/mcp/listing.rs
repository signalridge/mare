//! Tool listing: pagination, names, status, the CLI list and direct tools.

use super::*;

/// tools/list is bounded: an endless cursor stops after 16 pages, and a list that never
/// answers fails within the startup timeout instead of hanging.
#[tokio::test]
async fn mcp_tools_list_is_capped_at_16_pages_and_bounded_by_the_startup_timeout() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "pages");
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        global(
            &h,
            "SPEC",
            mare_shared::lua::json_to_lua(h.lua(), &with(fake.entry(json!({ "pages": true })), json!({ "name": "p" })))
                .unwrap(),
        );
        let n: i64 = eval(&h, "local c = mare.mcp.connect(SPEC); local n = #c:tools(); c:close(); return n").await;
        assert_eq!(n, 16, "one tool per page, 16 pages");
        assert_eq!(fake.count("tools/list"), 16);

        let slow = Fake::new(&env, "slow");
        let spec = with(slow.entry(json!({ "list_delay": 30 })), json!({ "name": "s", "timeout_ms": 400 }));
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        let t0 = std::time::Instant::now();
        let err: String =
            eval(&h, "local ok, e = pcall(mare.mcp.connect, SPEC); assert(not ok); return tostring(e)").await;
        assert!(err.contains("did not initialize within 400ms"), "{err}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "bounded by the timeout");
        h.shutdown().await;
    })
    .await
}

/// Tool names: too long or colliding names get a short hash suffix; two servers whose names
/// sanitize alike are an error — the second never shadows the first.
#[tokio::test]
async fn mcp_tool_names_are_hashed_and_colliding_servers_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "names");
        let long = "x".repeat(80);
        let entry = fake.entry(json!({ "tools": ["echo", "a.b", "a_b", long] }));
        let cfg = servers(json!({
            "a-b": with(entry.clone(), json!({ "tier": "direct" })),
            "a_b": with(entry, json!({ "tier": "direct" })),
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "the a-b tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__a-b__echo') ~= nil").await }
        })
        .await;
        let names: Vec<String> = eval(
            &h,
            r#"local out = {}
               for _, d in ipairs(T:list()) do if d.name:match("^mcp__") then out[#out + 1] = d.name end end
               table.sort(out) return out"#,
        )
        .await;
        assert_eq!(names.len(), 4, "{names:?}");
        assert!(names.iter().all(|n| n.len() <= 64 && n.starts_with("mcp__a-b__")), "{names:?}");
        let hashed: Vec<&String> = names.iter().filter(|n| n.as_str() != "mcp__a-b__echo").collect();
        assert!(hashed.iter().all(|n| n.rsplit('_').next().unwrap().len() == 8), "hash suffixes: {hashed:?}");
        let out: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(out.contains("a_b") && out.contains("collides with a-b"), "{out}");
        assert_eq!(fake.count("start"), 1, "the rejected server never starts");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_264_mcp_tool_declarations_stable_across_connection_order() {
    local(async {
        async fn collect_order(alpha_first: bool) -> (Vec<String>, Vec<String>) {
            let env = TestEnv::new().await;
            let alpha = Fake::new(&env, "alpha");
            let beta = Fake::new(&env, "beta");
            let gate = env.home.path().join("second.gate");
            let alpha_opts = if alpha_first {
                json!({ "tools": ["echo"] })
            } else {
                json!({ "tools": ["echo"], "list_gate": gate })
            };
            let beta_opts = if alpha_first {
                json!({ "tools": ["echo"], "list_gate": gate })
            } else {
                json!({ "tools": ["echo"] })
            };
            let cfg = servers(json!({
                "alpha": with(alpha.entry(alpha_opts), json!({ "tier": "direct" })),
                "beta": with(beta.entry(beta_opts), json!({ "tier": "direct" })),
            }));
            let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
            expose(&h);
            let first = if alpha_first { "mcp__alpha__echo" } else { "mcp__beta__echo" };
            let second = if alpha_first { "mcp__beta__echo" } else { "mcp__alpha__echo" };
            wait_until(15, "first MCP server ready", || {
                let h = h.clone();
                let first = first.to_string();
                async move { eval::<bool>(&h, &format!("return T:get('{first}') ~= nil")).await }
            })
            .await;
            std::fs::write(&gate, "go").unwrap();
            wait_until(15, "second MCP server ready", || {
                let h = h.clone();
                let second = second.to_string();
                async move { eval::<bool>(&h, &format!("return T:get('{second}') ~= nil")).await }
            })
            .await;
            let defs: Vec<String> = eval(
                &h,
                r#"local out = {}
                   for _, d in ipairs(T:definitions()) do
                     if d.name:match("^mcp__") then out[#out + 1] = d.name end
                   end
                   return out"#,
            )
            .await;
            let snippets: Vec<String> = eval(
                &h,
                r#"local out = {}
                   for _, s in ipairs(T:snippets()) do
                     if type(s) == "table" and type(s.name) == "string" and s.name:match("^mcp__") then
                       out[#out + 1] = s.name
                     end
                   end
                   return out"#,
            )
            .await;
            h.shutdown().await;
            (defs, snippets)
        }

        let (defs_ab, snips_ab) = collect_order(true).await;
        let (defs_ba, snips_ba) = collect_order(false).await;
        assert_eq!(defs_ab, defs_ba, "MCP tool declaration order must not depend on server connection order");
        assert_eq!(snips_ab, snips_ba, "MCP tool snippet order must not depend on server connection order");
    })
    .await
}

/// The `mcp` service's status(): one row per server with state, tier, transport, tool count
/// and the last error, without connecting anything itself (deferred servers stay idle).
#[tokio::test]
async fn mcp_service_status_reports_state_tools_and_last_error() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "status");
        let cfg = servers(json!({
            "ok": with(fake.entry(json!({ "tools": ["echo", "crash"] })), json!({ "tier": "direct" })),
            "broken": { "command": "/nonexistent/mare-mcp-server", "tier": "direct" },
            "lazy": fake.entry(json!({})),
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        global(&h, "M", h.service_table("mcp").unwrap());
        wait_until(15, "both direct servers settle", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local by = {}
                       for _, s in ipairs(M:list_servers().data) do by[s.name] = s end
                       return by.ok.state == "connected" and by.broken.state == "error""#,
                )
                .await
            }
        })
        .await;
        let (rows, ok, broken, lazy): (String, String, String, String) = eval(
            &h,
            r#"local st = M:list_servers()
               local by, names = {}, {}
               for _, s in ipairs(st.data) do by[s.name] = s names[#names + 1] = s.name end
               local function row(s)
                 return table.concat({ s.state, s.tier, s.transport, tostring(s.tools), tostring(s.error ~= nil) }, " ")
               end
               return table.concat(names, ","), row(by.ok), row(by.broken), row(by.lazy)"#,
        )
        .await;
        assert_eq!(rows, "broken,lazy,ok", "sorted like /mcp");
        assert_eq!(ok, "connected direct stdio 2 false");
        assert_eq!(broken, "error direct stdio 0 true");
        assert_eq!(lazy, "idle deferred stdio 0 false");
        assert_eq!(fake.count("start"), 1, "status() never connected the deferred server");
        h.shutdown().await;
    })
    .await
}

/// `mare mcp list` (the binary): connects to the user's servers and prints their state.
#[test]
fn mcp_cli_list_prints_servers() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let log = home.path().join("srv.log");
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    let entry = json!({ "command": fake_mcp_bin(), "args": ["scripted", json!({ "log": log }).to_string()] });
    std::fs::write(home.path().join("config/mcp.json"), json!({ "mcpServers": { "cli": entry } }).to_string()).unwrap();
    let out = mare_cmd().args(["mcp", "list"]).current_dir(work.path()).env("MARE_HOME", home.path()).run_output();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("cli") && text.contains("connected") && text.contains("1 tools"), "{text}");
}

/// A deferred server's tools overridden to `direct` are declared once the background connect
/// finishes, the rest stay deferred.
#[tokio::test]
async fn mcp_direct_tools_of_a_deferred_server_are_declared_at_startup() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "mixed");
        let entry = with(fake.entry(json!({ "tools": ["echo", "other"] })), json!({ "tools": { "echo": "direct" } }));
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        let (echo, other_declared, other_exposure): (bool, bool, String) = eval(
            &h,
            r#"C.cordis:get("bus"):emit("request/prepare", {})
               local deadline = mare.task.clock() + 10000
               while not T:get('mcp__fake__echo') and mare.task.clock() < deadline do mare.task.sleep(25) end
               local declared = {}
               for _, d in ipairs(T:definitions({})) do declared[d.name] = true end
               local other_def = T:get('mcp__fake__other')
               return declared['mcp__fake__echo'] == true, declared['mcp__fake__other'] == true,
                 tostring(other_def and other_def.exposure or "")"#,
        )
        .await;
        assert!(echo && !other_declared && other_exposure == "deferred", "echo declared, other deferred");
        h.shutdown().await;
    })
    .await
}

/// `/mcp` and `mcp:list_servers()` order servers by source: project, user, env (then config).
#[tokio::test]
async fn mcp_servers_are_ordered_project_then_user_then_env() {
    local(async {
        let mut env = TestEnv::new().await;
        let fake = Fake::new(&env, "order");
        let entry = || fake.entry(json!({}));

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("mcp.json"), json!({ "mcpServers": { "m_user": entry() } }).to_string())
            .unwrap();

        let env_cfg = env.home.path().join("env-mcp.json");
        std::fs::write(&env_cfg, json!({ "mcpServers": { "a_env": entry() } }).to_string()).unwrap();
        env.set_var("MARE_MCP_CONFIG", env_cfg.to_string_lossy().as_ref());

        std::fs::write(env.cwd.join(".mcp.json"), json!({ "mcpServers": { "z_proj": entry() } }).to_string()).unwrap();

        let mut hb = env.host().builtins(&["mcp"]).config(servers(json!({ "b_cfg": entry() }))).no_session();
        hb.opts.project_trusted = true;
        let h = hb.build().await;
        global(&h, "M", h.service_table("mcp").unwrap());
        let names: Vec<String> = eval(
            &h,
            r#"local out = {}
               for _, s in ipairs(M:list_servers().data) do out[#out + 1] = s.name end
               return out"#,
        )
        .await;
        assert_eq!(
            names,
            vec!["z_proj", "m_user", "a_env", "b_cfg"],
            "MCP server listing must sort project -> user -> env -> config: {names:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// Wildcard `*` in `enabled_tools` filters registered tools, and the `/mcp` interactive
/// panel displays statuses, passes filter arguments, offers status-dependent actions, and shows
/// per-tool schemas in an overlay.
#[tokio::test]
async fn mcp_enabled_tools_wildcard_and_panel_states_actions_and_schemas() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "panel");
        let entry = with(
            fake.entry(json!({ "tools": ["echo", "echo_two", "other"] })),
            json!({ "tier": "direct", "enabled_tools": ["echo*"] }),
        );
        let cfg = servers(json!({
            "live": entry,
            "off": with(fake.entry(json!({ "tools": ["echo"] })), json!({ "enabled": false })),
            "oauth_srv": { "url": "http://127.0.0.1:1/mcp", "oauth": true, "tier": "direct" },
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());
        wait_until(15, "live tools and oauth needs_login settle", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local ok_live, ok_oauth = false, false
                       for _, s in ipairs(M:list_servers().data) do
                         if s.name == "live" and s.state == "connected" then ok_live = true end
                         if s.name == "oauth_srv" and s.state == "needs_login" then ok_oauth = true end
                       end
                       return ok_live and ok_oauth and T:get("mcp__live__echo") ~= nil and T:get("mcp__live__echo_two") ~= nil"#,
                )
                .await
            }
        })
        .await;

        // 1. Wildcard `enabled_tools: ["echo*"]` registered echo & echo_two, excluded other.
        let has_other: bool = eval(&h, r#"return T:get("mcp__live__other") ~= nil"#).await;
        assert!(!has_other, "tool not matching enabled_tools wildcard must be excluded");

        // 2. `/mcp <filter>` passes filter, shows status labels, status-dependent actions, and tool schema overlay.
        let (seen_filter, live_actions, off_actions, oauth_actions, overlay_title, overlay_body): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"local CM = C.cordis:get("commands")
               mare.ui.connected = function() return true end
               local got_filter, ov_title, ov_body = "", "", ""
               mare.ui.overlay = function(title, node)
                 ov_title = tostring(title)
                 ov_body = mare.json.encode(node)
               end
               local function probe_panel(target, arg, pick_tools)
                 local step, actions = 0, {}
                 mare.ui.select = function(opts)
                   step = step + 1
                   if step == 1 then
                     if arg then got_filter = tostring(opts.filter or "") end
                     for i, it in ipairs(opts.items or {}) do
                       if it.label:find("^" .. target .. "%s+%[") then return i end
                     end
                     return nil
                   else
                     for i, it in ipairs(opts.items or {}) do
                       actions[#actions + 1] = type(it) == "table" and it.label or tostring(it)
                     end
                     if pick_tools then
                       for i, lbl in ipairs(actions) do
                         if lbl == "Show tools and schemas" then return i end
                       end
                     end
                     return nil
                   end
                 end
                 CM:run(arg and ("/mcp " .. arg) or "/mcp")
                 return table.concat(actions, ",")
               end
               local a_live = probe_panel("live", "li", true)
               local a_off = probe_panel("off", nil, false)
               local a_oauth = probe_panel("oauth_srv", nil, false)
               return got_filter, a_live, a_off, a_oauth, ov_title, ov_body"#,
        )
        .await;
        assert_eq!(seen_filter, "li", "/mcp filter argument must populate picker filter");
        assert_eq!(live_actions, "Show tools and schemas,Reconnect,Disable");
        assert_eq!(off_actions, "Enable");
        assert_eq!(oauth_actions, "Login,Disable");
        assert_eq!(overlay_title, "MCP live tools");
        assert!(
            overlay_body.contains("mcp__live__echo") && overlay_body.contains("properties"),
            "tool schema overlay must show tool name and JSON schema: {overlay_body}"
        );
        h.shutdown().await;
    })
    .await
}

/// CLI subcommands `mare mcp list-tools`, `mare mcp disable`, and `mare mcp login` (error
/// on non-oauth server) work end-to-end through the real `mare` binary.
#[test]
fn mcp_cli_list_tools_disable_and_login_rejection() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let log = home.path().join("cli-sub.log");
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    let entry = json!({
        "command": fake_mcp_bin(),
        "args": ["scripted", json!({ "log": log, "tools": ["echo", "other"] }).to_string()]
    });
    std::fs::write(home.path().join("config/mcp.json"), json!({ "mcpServers": { "cli": entry } }).to_string()).unwrap();

    // 1. `mare mcp list-tools cli` prints the server's tools and schema.
    let list_tools = mare_cmd()
        .args(["mcp", "list-tools", "cli"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .run_output();
    let lt_out = String::from_utf8_lossy(&list_tools.stdout);
    assert!(list_tools.status.success(), "{lt_out}\n{}", String::from_utf8_lossy(&list_tools.stderr));
    assert!(lt_out.contains("echo") && lt_out.contains("other"), "{lt_out}");

    // 2. `mare mcp login cli` rejects a non-oauth server with a non-zero exit code.
    let login_bad =
        mare_cmd().args(["mcp", "login", "cli"]).current_dir(work.path()).env("MARE_HOME", home.path()).run_output();
    assert!(!login_bad.status.success(), "login on non-oauth server must fail");
    let login_txt =
        format!("{}{}", String::from_utf8_lossy(&login_bad.stdout), String::from_utf8_lossy(&login_bad.stderr));
    assert!(login_txt.contains("oauth"), "{login_txt}");

    // 3. `mare mcp disable cli` persists disabled state, reflected in subsequent `mare mcp list`.
    let disable =
        mare_cmd().args(["mcp", "disable", "cli"]).current_dir(work.path()).env("MARE_HOME", home.path()).run_output();
    assert!(disable.status.success(), "{}", String::from_utf8_lossy(&disable.stderr));
    let listed = mare_cmd().args(["mcp", "list"]).current_dir(work.path()).env("MARE_HOME", home.path()).run_output();
    let listed_out = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.status.success(), "{listed_out}");
    assert!(listed_out.contains("cli") && listed_out.contains("disabled"), "{listed_out}");
}

/// Servers that omit `capabilities.tools` (even while advertising other capabilities like
/// `prompts`) still get `tools/list` called (matching Codex `rmcp_client.rs:1374`) so their
/// tools are discovered and callable.
#[tokio::test]
async fn mcp_server_omitting_tools_capability_still_lists_tools() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "no-tools-cap");
        let entry = with(
            fake.entry(json!({ "capabilities": { "prompts": {} }, "tools": ["echo"] })),
            json!({ "tier": "direct" }),
        );
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());
        wait_until(15, "fake server settles", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local s = M:list_servers().data[1]
                       return s ~= nil and s.state ~= "connecting" and s.state ~= "idle""#,
                )
                .await
            }
        })
        .await;
        let (state, count, has_tool, call_out): (String, i64, bool, String) = eval(
            &h,
            r#"local s = M:list_servers().data[1]
               local t = T:get("mcp__fake__echo")
               local out = ""
               if t then
                 local r = T:execute({ id = "c1", name = "mcp__fake__echo", arguments = { text = "ok" } }, {})
                 out = type(r.content) == "table" and r.content[1].text or tostring(r.content)
               end
               return s.state, s.tools, t ~= nil, out"#,
        )
        .await;
        assert_eq!(state, "connected");
        assert_eq!(count, 1, "expected 1 tool when server omits capabilities.tools");
        assert!(has_tool, "mcp__fake__echo must be registered");
        assert_eq!(call_out, "echo: ok");
        assert!(fake.count("tools/list") >= 1, "client must call tools/list even without capabilities.tools");
        h.shutdown().await;
    })
    .await
}

/// Servers that omit `capabilities.tools` and return JSON-RPC `-32601 Method not found` to
/// `tools/list` (such as rmcp 3.12 default ServerHandler) still connect with 0 tools and show a
/// visible hint in `/mcp` / `mare mcp list`, whereas a server advertising `tools` that returns
/// `-32601` still fails.
#[test]
fn mcp_zero_tools_without_capability_connects_with_hint_and_advertised_method_not_found_fails() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let ok_log = home.path().join("ok.log");
    let bad_log = home.path().join("bad.log");
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    let ok_entry = json!({
        "command": fake_mcp_bin(),
        "args": [
            "scripted",
            json!({ "log": ok_log, "capabilities": { "prompts": {} }, "tools_method_not_found": true }).to_string()
        ]
    });
    let bad_entry = json!({
        "command": fake_mcp_bin(),
        "args": [
            "scripted",
            json!({ "log": bad_log, "capabilities": { "tools": {} }, "tools_method_not_found": true }).to_string()
        ]
    });
    std::fs::write(
        home.path().join("config/mcp.json"),
        json!({ "mcpServers": { "prompts_only": ok_entry, "broken_tools": bad_entry } }).to_string(),
    )
    .unwrap();

    let listed = mare_cmd().args(["mcp", "list"]).current_dir(work.path()).env("MARE_HOME", home.path()).run_output();
    let out = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.status.success(), "{out}\n{}", String::from_utf8_lossy(&listed.stderr));
    let prompts_line = out.lines().find(|l| l.contains("prompts_only")).expect("prompts_only row");
    assert!(
        prompts_line.contains("connected")
            && prompts_line.contains("0 tools")
            && prompts_line.contains("no tools exposed"),
        "expected connected + 0 tools + hint on prompts_only row:\n{out}"
    );
    let broken_line = out.lines().find(|l| l.contains("broken_tools")).expect("broken_tools row");
    assert!(
        broken_line.contains("error"),
        "server advertising capabilities.tools that rejects tools/list must fail:\n{out}"
    );

    let lt = mare_cmd()
        .args(["mcp", "list-tools", "prompts_only"])
        .current_dir(work.path())
        .env("MARE_HOME", home.path())
        .run_output();
    let lt_out = String::from_utf8_lossy(&lt.stdout);
    assert!(lt.status.success(), "{lt_out}\n{}", String::from_utf8_lossy(&lt.stderr));
    assert!(
        lt_out.contains("0 tools") && lt_out.contains("no tools exposed"),
        "expected connected 0-tools hint in list-tools:\n{lt_out}"
    );
}

/// File-backed JSON readers (MCP config, state.json, auth.json, permissions.json) strip at most
/// one leading UTF-8 BOM (`EF BB BF`), while `mare.json.decode*` keeps rejecting BOMs so network
/// and model JSON contracts stay unchanged.
#[tokio::test]
async fn file_backed_json_readers_strip_single_utf8_bom_while_json_decode_stays_strict() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "bom");
        let entry = with(fake.entry(json!({ "tools": ["ping"] })), json!({ "tier": "direct" }));
        let state_dir = mare_shared::paths::state_dir();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        // Single UTF-8 BOM on user mcp.json, state.json, auth.json, and permissions.json.
        std::fs::write(
            env.config_dir().join("mcp.json"),
            format!("\u{FEFF}{}", json!({ "mcpServers": { "bom_srv": entry } })),
        )
        .unwrap();
        std::fs::write(state_dir.join("state.json"), format!("\u{FEFF}{}", json!({ "version": 1, "effort": "high" })))
            .unwrap();
        mare_shared::paths::write_atomic(
            &mare_shared::paths::auth_file(),
            format!("\u{FEFF}{}", json!({ "version": 1, "openai": { "api_key": "sk-bom-test" } })).as_bytes(),
            mare_shared::paths::WriteMode::Private,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("permissions.json"),
            format!("\u{FEFF}{}", json!({ "version": 1, "rules": [] })),
        )
        .unwrap();

        let h = env.host().builtins(&["mcp", "llm", "permissions"]).no_session().build().await;
        expose(&h);
        wait_until(15, "bom_srv tool registered", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__bom_srv__ping') ~= nil").await }
        })
        .await;

        let (state_effort, auth_key, dec_ok, dec_u_ok, dec_l_ok): (String, String, bool, bool, bool) = eval(
            &h,
            r#"
            local st, st_err = mare.state.read()
            assert(st, "state.json with BOM must read: " .. tostring(st_err))
            local L = C.cordis:get("llm")
            local cred = L:auth_get("openai")
            local bom_json = "\239\187\191{}"
            local d1 = mare.json.decode(bom_json)
            local d2 = mare.json.decode_unique(bom_json)
            local d3 = mare.json.decode_lossless(bom_json)
            return tostring(st.effort or ""), tostring(cred and cred.api_key or ""),
                d1 ~= nil, d2 ~= nil, d3 ~= nil
            "#,
        )
        .await;
        assert_eq!(state_effort, "high");
        assert_eq!(auth_key, "sk-bom-test");
        assert!(!dec_ok, "mare.json.decode must still reject leading BOM");
        assert!(!dec_u_ok, "mare.json.decode_unique must still reject leading BOM");
        assert!(!dec_l_ok, "mare.json.decode_lossless must still reject leading BOM");
        h.shutdown().await;
        drop(env);

        // Double UTF-8 BOM on mcp.json must still be rejected as invalid JSON.
        let env2 = TestEnv::new().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(
            env2.config_dir().join("mcp.json"),
            format!("\u{FEFF}\u{FEFF}{}", json!({ "mcpServers": { "bad_bom": { "command": "echo" } } })),
        )
        .unwrap();
        let h2 = env2.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h2);
        let errs: String = eval(
            &h2,
            r#"
            local M = C.cordis:get("mcp")
            local status = M:list_servers()
            return mare.json.encode(status.errors or {})
            "#,
        )
        .await;
        assert!(errs.contains("invalid JSON"), "double BOM mcp.json must report invalid JSON: {errs}");
        h2.shutdown().await;
    })
    .await
}

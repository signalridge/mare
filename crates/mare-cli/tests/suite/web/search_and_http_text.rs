use super::*;

#[tokio::test]
async fn web_service_contract_runs_in_child_headless_and_rpc_hosts() {
    local(async {
        for mode in [mare_shared::ui::Mode::Child, mare_shared::ui::Mode::Print, mare_shared::ui::Mode::Rpc] {
            let env = TestEnv::new().await;
            let mut builder = env
                .host()
                .builtins(&["web"])
                .no_session()
                .config(json!({"plugins": {"web-fetch-http": {"enabled": false}}}));
            builder.opts.mode = mode;
            let h = builder.build().await;
            expose(&h);
            let (value, removed): (String, bool) = eval(
                &h,
                r#"
                local dispose = C.cordis:get('web'):register_fetch({
                    provider_id = "contract",
                    fetch = function(request, context)
                        assert(context.cancel and not context.cancel:is_cancelled())
                        return { requested_url = request.url, content = mare.mode }
                    end,
                })
                local tok = mare.task.token()
                local result = C.cordis:get('web'):fetch("https://example.test", { cancel = function() return tok end })
                dispose(); dispose()
                return result.content, C.cordis:get('web'):list_providers("fetch").reason == "unavailable"
            "#,
            )
            .await;
            assert_eq!(value, mode.as_str());
            assert!(removed);
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn web_queries_deduplicate_run_concurrently_and_merge_fairly() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["web"])
            .no_session()
            .config(json!({"plugins": {"web": {"search_max_results": 3}}}))
            .build()
            .await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local queries, running = {}, 0
            local both_tx, both_rx = mare.task.channel()
            local dispose = C.cordis:get('web'):register_search({
                provider_id = "fixture",
                search = function(req, call)
                    local q = req.queries[1]
                    queries[#queries + 1] = q
                    running = running + 1
                    if q == "legacy" then return { sources = {} } end
                    if q == "fail" then error("query failed") end
                    if q == "slow" then call.cancel:wait(); return { data = {} } end
                    if running == 2 then
                        both_tx:send(true)
                        both_tx:send(true)
                    end
                    both_rx:recv()
                    return { content = q, data = {{ url = q .. "1" }, { url = "shared" }, { url = q .. "3" }} }
                end,
            })
            local tok = mare.task.token()
            local opts = { cancel = function() return tok end }
            local r = C.cordis:get('web'):search({ "a", "b", "a" }, opts)
            assert(#queries == 2 and #r.data == 3 and r.truncated)
            assert(r.data[1].url == "a1" and r.data[2].url == "b1" and r.data[3].url == "shared")
            local ok, err = pcall(function() C.cordis:get('web'):search({ "slow", "fail" }, opts) end)
            assert(not ok and tostring(err):find("query failed", 1, true), tostring(err))
            local legacy_ok = pcall(function() C.cordis:get('web'):search({ "legacy" }, opts) end)
            assert(not legacy_ok, "a backend must return data, not the retired sources field")
            dispose()
            return true
        "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_search_formats_published_at_ms_including_negative_epochs_and_rejects_legacy_alias() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["web"]).no_session().build().await;
        expose(&h);
        let report_json: String = eval(
            &h,
            r#"
            local web = C.cordis:get("web")
            local dispose = web:register_search({
                provider_id = "ts-fixture",
                search = function(req)
                    local q = req.queries[1]
                    if q == "legacy-alias" then
                        return {
                            data = {
                                { url = "https://example.test/legacy", title = "Legacy", published_at = "2024-01-01T00:00:00Z" },
                            },
                        }
                    end
                    return {
                        data = {
                            { url = "https://example.test/pos", title = "Pos", snippet = "post-epoch", published_at_ms = 1704067200000 },
                            { url = "https://example.test/neg", title = "Neg", snippet = "pre-epoch", published_at_ms = -86400000 },
                            { url = "https://example.test/none", title = "None", snippet = "no-date" },
                        },
                    }
                end,
            })
            local tok = mare.task.token()
            local opts = { cancel = function() return tok end }
            local ok_res = web:search({ "dates" }, opts)
            local ok_legacy, err_legacy = pcall(function()
                return web:search({ "legacy-alias" }, opts)
            end)
            local tool_out = T:execute({
                id = "ws-ts-1",
                name = "web_search",
                arguments = { queries = { "dates" } },
            }, {})
            dispose()
            return mare.json.encode({
                pos_ms = ok_res.data[1].published_at_ms,
                neg_ms = ok_res.data[2].published_at_ms,
                none_ms = ok_res.data[3].published_at_ms,
                ok_legacy = ok_legacy,
                err_legacy = tostring(err_legacy),
                tool_is_error = tool_out.is_error == true,
                tool_content = tostring(tool_out.content or ""),
            })
            "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&report_json).unwrap();
        assert_eq!(report["pos_ms"], 1_704_067_200_000_i64, "{report}");
        assert_eq!(report["neg_ms"], -86_400_000_i64, "{report}");
        assert!(report["none_ms"].is_null(), "{report}");
        assert_eq!(report["ok_legacy"], false, "legacy published_at alias must be rejected: {report}");
        assert_eq!(report["tool_is_error"], false, "web_search tool must succeed on negative epoch ms: {report}");
        let content = report["tool_content"].as_str().unwrap();
        assert!(content.contains("(2024-01-01) post-epoch"), "positive published_at_ms formatting: {content}");
        assert!(content.contains("(1969-12-31) pre-epoch"), "negative published_at_ms formatting: {content}");
        assert!(content.contains("[None](https://example.test/none) no-date"), "omitted published_at_ms: {content}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_registry_selects_at_execution_and_revokes_inflight_calls() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins/web-probe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"return {
 name="web-probe",inject={"web","tools"}, apply=function(ctx)
  ctx:on("kernel/doctor",function(checks)
   local function check()
    assert(ctx.web:list_providers("fetch").reason=="unavailable")
    local events=0
    ctx:on("web/providers-changed",function() events=events+1 end)
    local dispose=ctx.web:register_fetch({provider_id="one",fetch=function(req) return {requested_url=req.url,content="one"} end})
    assert(not pcall(function() ctx.web:register_fetch({provider_id="one",fetch=function() end}) end))
    local tok=mare.task.token()
    local opts={cancel=function() return tok end}
    assert(ctx.web:fetch("test",opts).content=="one")
    local second=ctx.web:register_fetch({provider_id="two",fetch=function() return {} end})
    local amb=ctx.web:list_providers("fetch")
    assert(amb.reason=="ambiguous")
    assert(not amb.data[1].selected and not amb.data[2].selected)
    assert(not pcall(function() ctx.web:fetch("test",opts) end))
    second();second()
    local list1=ctx.web:list_providers("fetch").data
    assert(#list1==1 and list1[1].provider_id=="one" and list1[1].selected)
    dispose()
    local ready_tx,ready_rx=mare.task.channel()
    local slow_dispose=ctx.web:register_fetch({provider_id="slow",fetch=function(_,call)
     ready_tx:send(true);call.cancel:wait();error("backend cancelled during removal")
    end})
    local hold_tx, hold_rx = mare.task.channel()
    local winner, message=mare.task.select(function()
     local ok,err=pcall(function() ctx.web:fetch("test",opts) end)
     assert(not ok and tostring(err):find("WEB_PROVIDER_REMOVED",1,true),tostring(err))
     return "removed"
    end,function() ready_rx:recv();slow_dispose();hold_rx:recv() end)
    hold_tx:send(true)
    assert(winner==1 and message=="removed")
    assert(events==6,tostring(events))
   end
   local ok,err=pcall(check)
   checks[#checks+1]={name="web registry contracts",ok=ok,detail=tostring(err or "passed")}
  end)
 end}"#,
        )
        .unwrap();
        let h = env
            .host().builtins(&["web"])
            .user_plugins()
            .no_session()
            .config(json!({"plugins": {"web-fetch-http": {"enabled": false}}}))
            .build()
            .await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local checks={}
            C.cordis:get("bus"):emit("kernel/doctor",checks)
            for _,c in ipairs(checks) do if c.name=="web registry contracts" then assert(c.ok,c.detail);return true end end
            error("missing web check")
        "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_configured_selection_and_backend_reload_are_explicit() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins/web-choice");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            format!(
                r#"return {{
 name="web-choice",inject={{"web"}},apply=function(ctx)
  {LUA_SELECTED_ID}
  assert(ctx.web:list_providers("fetch").reason=="configured_missing")
  local ready=false
  ctx.web:register_fetch({{provider_id="chosen",available=function() return ready end,fetch=function() return {{content="chosen"}} end}})
  ctx.web:register_fetch({{provider_id="other",fetch=function() return {{content="wrong"}} end}})
  assert(ctx.web:list_providers("fetch").reason=="configured_unavailable")
  ready=true
  assert(selected_id(ctx.web:list_providers("fetch"))=="chosen")
 end}}"#
            ),
        )
        .unwrap();
        let h = env
            .host().builtins(&["web", "web-fetch-http"])
            .user_plugins()
            .no_session()
            .config(json!({"plugins": {"web": {"fetch_provider": "chosen", "web_search": "disabled"}}}))
            .build()
            .await;
        global(&h, "W", h.service_table("web").unwrap());
        let id: String = eval(&h, &format!("{LUA_SELECTED_ID} return selected_id(W:list_providers('fetch'))")).await;
        assert_eq!(id, "chosen");
        h.kernel.reload(Some("web-choice")).await.unwrap();
        let (count, search): (usize, String) =
            eval(&h, "return #W:list_providers('fetch').data, W:list_providers('search').reason").await;
        assert_eq!(count, 3);
        assert_eq!(search, "disabled");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_remote_backend_refused_inside_sandbox_execution() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["web", "web-fetch-http"])
            .no_session()
            .config(json!({"plugins": {"web": {"fetch_provider": "remote", "search_provider": "remote-search"}}}))
            .build()
            .await;
        expose(&h);
        let (ok, err, ok_search, err_search): (bool, String, bool, String) = eval(
            &h,
            r#"
            C.cordis:get('web'):register_fetch({
                provider_id = "remote",
                unconfined = true,
                fetch = function() error("must not run inside sandbox") end,
            })
            C.cordis:get('web'):register_search({
                provider_id = "remote-search",
                unconfined = true,
                search = function() error("search must not run inside sandbox") end,
            })
            return mare.exec.run({ sandbox = { level = "workspace-write", network = false } }, function()
                local tok = mare.task.token()
                local cancel = function() return tok end
                local ok_f, err_f = pcall(function() return C.cordis:get('web'):fetch("https://example.test", { cancel = cancel }) end)
                local ok_s, err_s = pcall(function() return C.cordis:get('web'):search({ "query" }, { cancel = cancel }) end)
                return ok_f, tostring(err_f), ok_s, tostring(err_s)
            end)
        "#,
        )
        .await;
        assert!(!ok && err.contains("WEB_UNCONFINED_DENIED"), "{err}");
        assert!(!ok_search && err_search.contains("WEB_UNCONFINED_DENIED"), "{err_search}");
        h.shutdown().await;
    })
    .await
}

#[test]
fn web_rpc_providers_selection_register_call_unregister_and_cancel() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(home.path().join("config/init.lua"), r#"mare.plugin("web-fetch-http", { enabled = false })"#)
        .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--no-session", "--yolo", "--rpc"])
        .env("MARE_HOME", home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init_no_cb = rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": false } }),
        |_| json!(null),
    );
    let methods = init_no_cb["result"]["capabilities"]["methods"].as_array().unwrap();
    for m in ["web.list_providers", "web.search", "web.fetch", "web.register_search", "web.register_fetch"] {
        assert!(methods.iter().any(|v| v == m), "missing {m}: {methods:?}");
    }
    for old in ["web.register", "web.unregister"] {
        assert!(!methods.iter().any(|v| v == old), "removed {old} still advertised: {methods:?}");
    }
    assert_eq!(init_no_cb["result"]["capabilities"]["callbacks"]["callbacks"], false);

    let no_cb_err =
        rpc.call_raw("web.register_fetch", json!({ "provider_id": "rpc-fetch", "callbacks": ["fetch"] }), |_| {
            json!(null)
        });
    assert_eq!(no_cb_err["error"]["data"]["code"], json!("unsupported"), "{no_cb_err}");

    let init = rpc.call_raw(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false } }),
        |_| json!(null),
    );
    assert_eq!(init["result"]["capabilities"]["callbacks"], true);

    assert_eq!(
        rpc.call_raw("web.list_providers", json!({ "kind": "fetch" }), |_| json!(null))["result"],
        json!({ "data": [], "reason": "unavailable" })
    );

    let confined_claim = rpc.call_raw(
        "web.register_fetch",
        json!({ "provider_id": "bad-confined", "unconfined": false, "callbacks": ["fetch"] }),
        |_| json!(null),
    );
    assert_eq!(confined_claim["error"]["data"]["code"], json!("invalid_arguments"), "{confined_claim}");

    let reg_fetch =
        rpc.call_raw("web.register_fetch", json!({ "provider_id": "rpc-fetch", "callbacks": ["fetch"] }), |_| {
            json!(null)
        });
    let fetch_reg_id = reg_fetch["result"]["registration_id"].as_str().expect("registration_id").to_string();
    let reg_search =
        rpc.call_raw("web.register_search", json!({ "provider_id": "rpc-search", "callbacks": ["search"] }), |_| {
            json!(null)
        });
    let search_reg_id = reg_search["result"]["registration_id"].as_str().expect("registration_id").to_string();

    let providers = rpc.call_raw("web.list_providers", json!({ "kind": "fetch" }), |_| json!(null))["result"].clone();
    assert_eq!(
        providers,
        json!({ "data": [{ "provider_id": "rpc-fetch", "available": true, "unconfined": true, "selected": true }] })
    );

    let fetched = rpc.call_raw("web.fetch", json!({ "url": "https://example.test/page" }), |req| {
        assert_eq!(req["method"], "callback.call");
        assert_eq!(req["params"]["registration_id"], fetch_reg_id);
        assert_eq!(req["params"]["callback"], "fetch");
        let url = req["params"]["arguments"]["request"]["url"].clone();
        json!({
            "requested_url": url,
            "url": "https://example.test/final",
            "status": 200,
            "content": "hello from rpc"
        })
    });
    assert_eq!(fetched["result"]["content"], "hello from rpc");
    assert_eq!(fetched["result"]["url"], "https://example.test/final");

    let searched = rpc.call_raw("web.search", json!({ "queries": ["rust", "lua"] }), |req| {
        assert_eq!(req["method"], "callback.call");
        assert_eq!(req["params"]["registration_id"], search_reg_id);
        assert_eq!(req["params"]["callback"], "search");
        let q = req["params"]["arguments"]["request"]["queries"][0].as_str().unwrap();
        json!({ "data": [{ "url": format!("https://example.test/{q}"), "title": q }] })
    });
    assert_eq!(searched["result"]["data"].as_array().unwrap().len(), 2);

    // $/cancel_request aborts an inflight web.fetch and cancels the pending callback.call.
    let slow_id = rpc.request("web.fetch", json!({ "url": "https://example.test/slow" }));
    let cb_id = loop {
        let msg = rpc.read();
        if msg["method"] == "callback.call" {
            break msg["id"].clone();
        }
    };
    rpc.notify("$/cancel_request", json!({ "id": slow_id }));
    let mut saw_cancel_cb = false;
    let cancel_reply = loop {
        let msg = rpc.read();
        if msg["method"] == "$/cancel_request" && msg["params"]["id"] == cb_id {
            saw_cancel_cb = true;
        } else if msg["id"] == slow_id {
            break msg;
        }
    };
    assert!(saw_cancel_cb, "expected $/cancel_request for callback {cb_id}");
    assert_eq!(cancel_reply["error"]["data"]["code"], json!("cancelled"), "{cancel_reply}");

    assert_eq!(
        rpc.call_raw("registrations.release", json!({ "registration_id": fetch_reg_id }), |_| json!(null))["result"],
        json!({})
    );
    assert_eq!(
        rpc.call_raw("web.list_providers", json!({ "kind": "fetch" }), |_| json!(null))["result"]["data"],
        json!([])
    );

    assert_eq!(rpc.call_raw("shutdown", json!({}), |_| json!(null))["result"], json!({}));
    rpc.wait_exit();
}

#[tokio::test]
async fn web_builtin_http_text_is_available_without_extras_and_extracts_content() {
    local(async {
        let server = MockServer::start().await;
        server.route("/plain", vec![resp_with(200, "text/plain; charset=utf-8", "hello 世界")]);
        server.route(
            "/html",
            vec![resp_with(
                200,
                "text/html; charset=utf-8",
                "<h1>Title</h1><script>secret</script><p hidden>hidden-p</p><p>visible</p><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>",
            )],
        );
        server.route("/binary", vec![resp_with(200, "application/octet-stream", "binary")]);
        server.route("/not-found", vec![resp_with(404, "text/plain", "nope")]);
        server.route("/long", vec![resp_with(200, "text/plain", "x".repeat(500))]);

        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["web", "web-fetch-http"])
            .no_session()
            .config(json!({"plugins": {"web-fetch-http": {"max_output_bytes": 100}}}))
            .build()
            .await;
        expose(&h);
        global(&h, "BASE", server.base.clone());
        let (sel_id, plain_content, html_content, bin_err, nf_err, long_len, long_trunc): (
            String,
            String,
            String,
            String,
            String,
            usize,
            bool,
        ) = eval(
            &h,
            &format!(
                r#"
            {LUA_SELECTED_ID}
            local sel_id = selected_id(C.cordis:get('web'):list_providers("fetch"))
            local tok = mare.task.token()
            local opts = {{ cancel = function() return tok end }}
            local plain = C.cordis:get('web'):fetch(BASE .. "/plain", opts)
            local html = C.cordis:get('web'):fetch(BASE .. "/html", opts)
            local ok_bin, err_bin = pcall(function() return C.cordis:get('web'):fetch(BASE .. "/binary", opts) end)
            assert(not ok_bin)
            local ok_nf, err_nf = pcall(function() return C.cordis:get('web'):fetch(BASE .. "/not-found", opts) end)
            assert(not ok_nf)
            local long = C.cordis:get('web'):fetch(BASE .. "/long", opts)
            return tostring(sel_id), plain.content, html.content, tostring(err_bin), tostring(err_nf), #long.content, long.truncated == true
        "#
            ),
        )
        .await;
        assert_eq!(sel_id, "http-text");
        assert_eq!(plain_content, "hello 世界");
        assert!(html_content.contains("Title") && html_content.contains("visible"), "{html_content}");
        assert!(html_content.contains("| A | B |") && html_content.contains("| 1 | 2 |"), "{html_content}");
        assert!(!html_content.contains("secret") && !html_content.contains("hidden-p"), "{html_content}");
        assert!(bin_err.contains("WEB_UNSUPPORTED_CONTENT_TYPE"), "{bin_err}");
        assert!(nf_err.contains("WEB_HTTP_STATUS"), "{nf_err}");
        assert_eq!(long_len, 100);
        assert!(long_trunc);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_builtin_http_text_respects_netauth_ssrf_and_offline_guard() {
    local(async {
        let server = MockServer::start().await;
        server.route("/plain", vec![resp_with(200, "text/plain; charset=utf-8", "loopback-ok")]);

        let env = TestEnv::new().await;
        let h = env.host().builtins(&["web", "web-fetch-http"]).no_session().build().await;
        expose(&h);
        global(&h, "BASE", server.base.clone());
        let (ok_priv, err_priv, ok_fake, err_fake, ok_allow, allow_content): (
            bool,
            String,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
                local tok = mare.task.token()
                local opts = { cancel = function() return tok end }
                local ok1, err1 = mare.exec.run(
                    { sandbox = { level = "workspace-write", network = { allow = { "127.0.0.1" } } } },
                    function()
                        return pcall(function()
                            return C.cordis:get('web'):fetch(BASE .. "/plain", opts)
                        end)
                    end
                )
                local ok2, err2 = mare.exec.run(
                    { sandbox = { level = "workspace-write", network = { allow = { "198.18.0.5" } } } },
                    function()
                        return pcall(function()
                            return C.cordis:get('web'):fetch("http://198.18.0.5/plain", opts)
                        end)
                    end
                )
                local ok3, res3 = mare.exec.run(
                    { sandbox = { level = "workspace-write", network = { allow = { "127.0.0.1" }, allow_local = true } } },
                    function()
                        return pcall(function()
                            return C.cordis:get('web'):fetch(BASE .. "/plain", opts)
                        end)
                    end
                )
                return ok1, tostring(err1), ok2, tostring(err2), ok3, type(res3) == "table" and res3.content or tostring(res3)
            "#,
        )
        .await;
        assert!(!ok_priv && err_priv.contains("WEB_BLOCKED_URL"), "{err_priv}");
        assert!(!ok_fake && err_fake.contains("WEB_BLOCKED_URL"), "{err_fake}");
        assert!(ok_allow && allow_content == "loopback-ok", "{allow_content}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_rpc_builtin_http_text_works_by_default() {
    let server = MockServer::start().await;
    server.route("/page", vec![resp_with(200, "text/html; charset=utf-8", "<h1>Hello</h1><p>World</p>")]);

    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--no-session", "--yolo", "--rpc"])
        .env("MARE_HOME", home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let _init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let providers = rpc.call_raw("web.list_providers", json!({ "kind": "fetch" }), |_| json!(null))["result"].clone();
    assert_eq!(
        providers,
        json!({ "data": [{ "provider_id": "http-text", "available": true, "unconfined": false, "selected": true }] })
    );

    let ok_fetch = rpc.call_raw("web.fetch", json!({ "url": format!("{}/page", server.base) }), |_| json!(null));
    let content = ok_fetch["result"]["content"].as_str().unwrap_or("");
    assert!(content.contains("Hello") && content.contains("World"), "{ok_fetch}");

    let blocked = rpc.call_raw("web.fetch", json!({ "url": "https://user:pass@127.0.0.1/secret" }), |_| json!(null));
    let err_msg = blocked["error"]["message"].as_str().unwrap_or("");
    assert!(err_msg.contains("WEB_BLOCKED_URL"), "{blocked}");

    assert_eq!(rpc.call_raw("shutdown", json!({}), |_| json!(null))["result"], json!({}));
    rpc.wait_exit();
}

#[test]
fn web_contract_validates_params_wraps_list_providers_and_maps_error_codes() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--no-session", "--rpc"]).env("MARE_HOME", &home).stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let methods = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for m in ["web.list_providers", "web.search", "web.fetch"] {
        assert!(methods.iter().any(|v| v == m), "missing {m}: {methods:?}");
    }
    for old in ["web.providers", "web.selection"] {
        assert!(!methods.iter().any(|v| v == old), "removed {old} must not be advertised: {methods:?}");
        let err = c.call_raw(old, json!({ "kind": "fetch" }), |_| json!(null));
        assert_eq!(err["error"]["code"], json!(-32601), "{old} must return method_not_found: {err}");
    }

    let providers = c.call_raw("web.list_providers", json!({ "kind": "fetch" }), |_| json!(null));
    let data = providers["result"]["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{providers}");
    assert_eq!(data[0]["provider_id"], json!("http-text"));
    assert_eq!(data[0]["selected"], json!(true));
    assert_eq!(data[0]["available"], json!(true));

    let bad_fetch = c.call_raw("web.fetch", json!({ "url": "https://example.test", "extra": 1 }), |_| json!(null));
    assert_eq!(bad_fetch["error"]["data"]["code"], json!("invalid_arguments"), "{bad_fetch}");

    let bad_timeout =
        c.call_raw("web.fetch", json!({ "url": "https://example.test", "timeout_ms": 0 }), |_| json!(null));
    assert_eq!(bad_timeout["error"]["data"]["code"], json!("invalid_arguments"), "{bad_timeout}");

    let bad_timeout_high =
        c.call_raw("web.fetch", json!({ "url": "https://example.test", "timeout_ms": 300_001 }), |_| json!(null));
    assert_eq!(bad_timeout_high["error"]["data"]["code"], json!("invalid_arguments"), "{bad_timeout_high}");

    let empty_queries = c.call_raw("web.search", json!({ "queries": [] }), |_| json!(null));
    assert_eq!(empty_queries["error"]["data"]["code"], json!("invalid_arguments"), "{empty_queries}");

    let missing_search = c.call_raw("web.search", json!({ "queries": ["hello"] }), |_| json!(null));
    assert_eq!(missing_search["error"]["data"]["code"], json!("unavailable"), "{missing_search}");

    c.shutdown_and_wait();

    // Case 1: without peer.allow granting `web.*`, a real PeerClient on the UNIX socket is refused.
    {
        let root = tempfile::tempdir().unwrap();
        let peer_home = root.path().join("home");
        std::fs::create_dir_all(peer_home.join("config")).unwrap();
        std::fs::write(peer_home.join("config/init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();
        std::fs::write(root.path().join("faux.json"), "[]").unwrap();
        let env = std::collections::BTreeMap::from([
            ("MARE_HOME".to_string(), peer_home.display().to_string()),
            ("MARE_FAUX_SCRIPT".to_string(), root.path().join("faux.json").display().to_string()),
        ]);
        let mut host = crate::common::rpc::RpcClient::spawn(
            &["--rpc", "--peer", "--yolo", "--no-session", "--model", "faux/echo"],
            &env,
            root.path(),
        );
        let init_host = host.call("initialize", json!({"protocol_version": "3.0"}));
        let mut peer = crate::common::rpc::PeerClient::connect(&peer_home, host.pid());
        let pinit = peer.call(
            "initialize",
            json!({
                "protocol_version": init_host["protocol_version"],
                "capabilities": { "callbacks": true },
            }),
        );
        assert!(pinit.get("result").is_some(), "peer initialize failed: {pinit}");
        let reg_denied =
            peer.call("web.register_fetch", json!({ "provider_id": "peer-denied", "callbacks": ["fetch"] }));
        assert_eq!(
            reg_denied["error"]["data"]["code"],
            json!("denied"),
            "unpermitted peer must be denied: {reg_denied}"
        );
        assert!(
            reg_denied["error"]["message"].as_str().unwrap_or("").contains("not allowed on this connection"),
            "{reg_denied}"
        );
        let fetch_denied = peer.call("web.fetch", json!({ "url": "https://example.test" }));
        assert_eq!(
            fetch_denied["error"]["data"]["code"],
            json!("denied"),
            "unpermitted peer fetch must be denied: {fetch_denied}"
        );
        assert!(
            fetch_denied["error"]["message"].as_str().unwrap_or("").contains("not allowed on this connection"),
            "{fetch_denied}"
        );
        drop(peer);
        host.shutdown_and_wait();
    }

    // Case 2: real PeerClient with `peer.allow = { "web.*", "services.call" }` can register/list/unregister
    // as its connection, yet both `services.call` and `PeerClient` `web.fetch` remain confined under
    // `--permission read-only` (running `run_host_query` and primary `approval.request`), and `WEB_INVALID_RESULT`
    // maps to `invalid_result`.
    {
        let root = tempfile::tempdir().unwrap();
        let peer_home = root.path().join("home");
        std::fs::create_dir_all(peer_home.join("config/plugins/web-err-probe")).unwrap();
        std::fs::write(
            peer_home.join("config/init.lua"),
            r#"mare.setup { model = "faux/echo", peer = { allow = { "web.register_fetch", "web.register_search", "web.list_providers", "web.fetch", "web.search", "registrations.release" } } }"#,
        )
        .unwrap();
        std::fs::write(
            peer_home.join("config/plugins/web-err-probe/init.lua"),
            r#"return {
    name = "web-err-probe",
    inject = { "web" },
    apply = function(ctx)
        ctx.web:register_search({
            provider_id = "err-probe",
            search = function()
                error("WEB_INVALID_RESULT: malformed search payload", 0)
            end,
        })
    end,
}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("faux.json"), "[]").unwrap();
        let env = std::collections::BTreeMap::from([
            ("MARE_HOME".to_string(), peer_home.display().to_string()),
            ("MARE_FAUX_SCRIPT".to_string(), root.path().join("faux.json").display().to_string()),
        ]);
        let mut host = crate::common::rpc::RpcClient::spawn(
            &[
                "--rpc",
                "--peer",
                "--permission",
                "read-only",
                "--approval-policy",
                "ask",
                "--no-session",
                "--model",
                "faux/echo",
            ],
            &env,
            root.path(),
        );
        let init_host =
            host.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }));

        let inv_res = host.call_err("web.search", json!({ "queries": ["bad-result"] }));
        assert_eq!(
            inv_res["data"]["code"],
            json!("invalid_result"),
            "WEB_INVALID_RESULT must map to invalid_result: {inv_res}"
        );

        let svc_err = host.call_err("web.fetch", json!({ "url": "http://93.184.216.34/plain" }));
        assert_eq!(host.approvals.len(), 1, "web.fetch must ask primary approval: {svc_err}");
        assert!(svc_err["message"].as_str().unwrap_or("").contains("WEB_BLOCKED_URL"), "{svc_err}");

        let mut peer = crate::common::rpc::PeerClient::connect(&peer_home, host.pid());
        let pinit = peer.call(
            "initialize",
            json!({
                "protocol_version": init_host["protocol_version"],
                "capabilities": { "callbacks": true },
            }),
        );
        assert!(pinit.get("result").is_some(), "peer initialize failed: {pinit}");
        let peer_sid = pinit["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();

        let reg = peer.call(
            "web.register_fetch",
            json!({ "session_id": peer_sid, "provider_id": "peer-owned", "callbacks": ["fetch"] }),
        );
        assert!(reg.get("error").is_none(), "permitted peer must be allowed to web.register_fetch: {reg}");
        let peer_reg_id = reg["result"]["registration_id"].as_str().expect("registration_id").to_string();
        let listed = peer.call("web.list_providers", json!({ "kind": "fetch" }));
        assert!(
            listed["result"]["data"].as_array().is_some_and(|d| d.iter().any(|p| p["provider_id"] == "peer-owned")),
            "peer registration missing from web.list_providers: {listed}"
        );
        let unreg = peer.call("registrations.release", json!({ "registration_id": peer_reg_id }));
        assert_eq!(unreg["result"], json!({}), "permitted peer must release its provider: {unreg}");

        let peer_ro_local = peer.call("web.fetch", json!({ "url": "http://127.0.0.1:1/plain" }));
        assert!(
            peer_ro_local["error"]["message"].as_str().unwrap_or("").contains("WEB_BLOCKED_URL"),
            "peer web.fetch must enforce read-only sandbox: {peer_ro_local}"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let peer_handle = std::thread::spawn(move || {
            let resp = peer.call("web.fetch", json!({ "url": "http://93.184.216.34/plain" }));
            let _ = tx.send((peer, resp));
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let peer_pub = loop {
            if let Ok((_, resp)) = rx.try_recv() {
                break resp;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for peer web.fetch approval flow");
            }
            if let Some(msg) = host.read_timeout(std::time::Duration::from_millis(100)) {
                host.dispatch(msg);
            }
        };
        peer_handle.join().unwrap();
        assert_eq!(host.approvals.len(), 2, "real PeerClient web.fetch must ask primary approval: {peer_pub}");
        assert!(
            peer_pub["error"]["message"].as_str().unwrap_or("").contains("WEB_BLOCKED_URL"),
            "denied peer web.fetch approval must block URL: {peer_pub}"
        );

        host.shutdown_and_wait();
    }
}

#[test]
fn web_rpc_error_limit_timeout_and_disconnect_mapping() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"
mare.plugin("web-fetch-http", { enabled = false })
mare.setup { model = "faux/echo", peer = { allow = { "web.register_fetch", "web.register_search", "web.list_providers", "web.fetch", "web.search", "registrations.release" } } }
"#,
    )
    .unwrap();
    std::fs::write(root.path().join("faux.json"), "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), root.path().join("faux.json").display().to_string()),
    ]);
    let mut host = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--yolo", "--no-session", "--model", "faux/echo"],
        &env,
        root.path(),
    );
    let init = host.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 120 }),
    );
    let proto_ver = init["protocol_version"].clone();

    // 1. Invalid provider id (`/` and >64 bytes) and duplicate provider id -> invalid_arguments.
    let bad_slash = host.call_err("web.register_fetch", json!({ "provider_id": "bad/id", "callbacks": ["fetch"] }));
    assert_eq!(bad_slash["data"]["code"], json!("invalid_arguments"), "{bad_slash}");
    let too_long_id = "x".repeat(65);
    let bad_long = host.call_err("web.register_fetch", json!({ "provider_id": too_long_id, "callbacks": ["fetch"] }));
    assert_eq!(bad_long["data"]["code"], json!("invalid_arguments"), "{bad_long}");

    let dup_first = host.call("web.register_fetch", json!({ "provider_id": "dup-fetch", "callbacks": ["fetch"] }));
    let dup_reg_id = dup_first["registration_id"].as_str().expect("registration_id").to_string();
    let dup_second = host.call_err("web.register_fetch", json!({ "provider_id": "dup-fetch", "callbacks": ["fetch"] }));
    assert_eq!(dup_second["data"]["code"], json!("invalid_arguments"), "{dup_second}");
    assert!(dup_second["message"].as_str().unwrap_or("").contains("WEB_DUPLICATE_PROVIDER"), "{dup_second}");

    // 2. Per-connection registration cap = 64; 65th fails until one is released.
    let mut limit_regs = vec![dup_reg_id];
    for idx in 2..=64 {
        let reg =
            host.call("web.register_search", json!({ "provider_id": format!("cap-{idx}"), "callbacks": ["search"] }));
        limit_regs.push(reg["registration_id"].as_str().expect("registration_id").to_string());
    }
    let over_cap = host.call_err("web.register_search", json!({ "provider_id": "cap-65", "callbacks": ["search"] }));
    assert_eq!(over_cap["data"]["code"], json!("invalid_arguments"), "{over_cap}");
    assert!(
        over_cap["message"].as_str().unwrap_or("").contains("connection registration limit exceeded"),
        "{over_cap}"
    );
    for reg_id in limit_regs {
        assert_eq!(host.call("registrations.release", json!({ "registration_id": reg_id })), json!({}));
    }

    // 3. Callback timeout -> timed_out (`WEB_CALLBACK_TIMEOUT`).
    let mut peer_to = crate::common::rpc::PeerClient::connect(&home, host.pid());
    let pinit = peer_to.call(
        "initialize",
        json!({ "protocol_version": proto_ver, "capabilities": { "callbacks": true }, "callback_timeout_ms": 120 }),
    );
    assert!(pinit.get("result").is_some(), "{pinit}");
    let pinit_sid = pinit["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();
    let reg_to = peer_to.call(
        "web.register_fetch",
        json!({ "session_id": pinit_sid, "provider_id": "peer-timeout", "callbacks": ["fetch"] }),
    );
    assert!(reg_to.get("error").is_none(), "{reg_to}");
    // Peer deliberately ignores `callback.call`, letting callbackTimeoutMs expire.
    let to_err = host.call_err("web.fetch", json!({ "url": "https://example.test/timeout" }));
    assert_eq!(to_err["data"]["code"], json!("timed_out"), "{to_err}");
    assert!(to_err["message"].as_str().unwrap_or("").contains("WEB_CALLBACK_TIMEOUT"), "{to_err}");
    let to_reg_id = reg_to["result"]["registration_id"].as_str().expect("registration_id");
    assert_eq!(peer_to.call("registrations.release", json!({ "registration_id": to_reg_id }))["result"], json!({}));
    drop(peer_to);

    // 4. Client disconnect while callback.call is in flight -> unavailable (`WEB_CALLBACK_DISCONNECTED`).
    let mut peer_dc = crate::common::rpc::PeerClient::connect(&home, host.pid());
    let pdc_init = peer_dc.call(
        "initialize",
        json!({ "protocol_version": proto_ver, "capabilities": { "callbacks": true }, "callback_timeout_ms": 10000 }),
    );
    assert!(pdc_init.get("result").is_some(), "{pdc_init}");
    let pdc_sid = pdc_init["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();
    let reg_dc = peer_dc
        .call("web.register_fetch", json!({ "session_id": pdc_sid, "provider_id": "peer-dc", "callbacks": ["fetch"] }));
    assert!(reg_dc.get("error").is_none(), "{reg_dc}");

    let req_id = host.request("web.fetch", json!({ "url": "https://example.test/disconnect" }));
    loop {
        let cb = peer_dc.read();
        if cb.get("method").and_then(serde_json::Value::as_str) == Some("callback.call") {
            break;
        }
    }
    drop(peer_dc);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let dc_reply = loop {
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for disconnect error after dropping callback peer");
        }
        if let Some(msg) = host.read_timeout(std::time::Duration::from_millis(100)) {
            if msg.get("id").and_then(serde_json::Value::as_u64) == Some(req_id) {
                break msg;
            }
            host.dispatch(msg);
        }
    };
    assert_eq!(dc_reply["error"]["data"]["code"], json!("unavailable"), "{dc_reply}");
    assert!(dc_reply["error"]["message"].as_str().unwrap_or("").contains("WEB_CALLBACK_DISCONNECTED"), "{dc_reply}");

    host.shutdown_and_wait();
}

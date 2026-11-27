//! Lossless numeric round-trip on the real RPC hook return path: a `hook.call` answer goes through
//! `restore_event_result` (`json_to_lua_lossless_bounded`). Exact numbers arrive exactly; numbers
//! Lua cannot hold are refused, never rounded or surfaced as serde_json's private number map.
use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;

#[tokio::test]
async fn rpc_hook_return_numbers_are_exact_or_refused() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":5000}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"hooks.subscribe","params":
                    {"scope":"all","event":"tool/rewrite","callbacks":["handle"]}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            let lua = host.lua();
            lua.globals().set("num_hook_ctx", host.kernel.host_ctx().unwrap()).unwrap();

            // (raw JSON number text, expected Lua result: Ok(rendered `math.type:value`) or Err(fragment))
            let cases: [(&str, Result<&str, &str>); 5] = [
                ("9007199254740993", Ok("integer:9007199254740993")),
                ("-42", Ok("integer:-42")),
                ("0.1", Ok("float:0.1")),
                ("1.00000000000000001", Err("cannot be represented losslessly")),
                ("18446744073709551616", Err("cannot be represented losslessly")),
            ];
            for (raw, expected) in cases {
                let fut = tokio::task::spawn_local({
                    let lua = lua.clone();
                    async move {
                        lua.load(
                            r#"
                            local ok, result = pcall(function()
                                return num_hook_ctx:get("policy"):rewrite("tool/rewrite", { name = "bash", arguments = { n = 0 } })
                            end)
                            if not ok then
                                if type(result) ~= "table" or result.code ~= "policy.rewrite_refused" then
                                    return nil, "unexpected error: " .. tostring(result)
                                end
                                return nil, tostring(result.message)
                            end
                            -- the remote rewriter answered the call; its arguments replace the call's
                            local n = result.n
                            if type(n) ~= "number" then return nil, "not a number: " .. type(n) end
                            return ("%s:%s"):format(math.type(n), math.type(n) == "integer" and tostring(n) or ("%.17g"):format(n)), nil
                            "#,
                        )
                        .eval_async::<(Option<String>, Option<String>)>()
                        .await
                        .unwrap()
                    }
                });
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" {
                        assert_eq!(msg["params"]["arguments"]["event"], "tool/rewrite");
                        let n: J = serde_json::from_str(raw).unwrap();
                        assert_eq!(n.to_string(), raw, "the client must send the exact number text");
                        rpc_send(
                            &mut cw,
                            json!({"jsonrpc":"2.0","id":msg["id"],
                                "result":{"value":{"name":"bash","arguments":{"n":n}}}}),
                        )
                        .await;
                        break;
                    }
                }
                let (got, err) = fut.await.unwrap();
                match expected {
                    Ok(want) => {
                        // %.17g renders 0.1 as 0.10000000000000001: compare the parsed float instead.
                        let got = got.unwrap_or_else(|| panic!("{raw}: unexpected error {err:?}"));
                        if let Some(f) = want.strip_prefix("float:") {
                            let (kind, val) = got.split_once(':').unwrap();
                            assert_eq!(kind, "float", "{raw}: {got}");
                            assert_eq!(val.parse::<f64>().unwrap(), f.parse::<f64>().unwrap(), "{raw}: {got}");
                        } else {
                            assert_eq!(got, want, "{raw}");
                        }
                    }
                    Err(fragment) => {
                        assert!(got.is_none(), "{raw}: must be refused, got {got:?}");
                        let err = err.unwrap_or_default();
                        assert!(err.contains(fragment), "{raw}: {err}");
                        assert!(!err.contains("$serde_json"), "{raw}: private number map leaked: {err}");
                    }
                }
            }

            drop(cw);
            drop(cr);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

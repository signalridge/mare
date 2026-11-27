use super::*;

/// Remote filesystem calls must reject workspace escapes. A replacement provider uses only
/// ordinary `mare.fs` calls, so it cannot accidentally supply the old provider-specific session policy.
#[tokio::test]
async fn fs_confined_bridge_applies_policy_to_replacement_provider() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("inside.txt"), "inside").unwrap();
            std::fs::write(outside.path().join("secret.txt"), "outside-secret").unwrap();
            std::fs::create_dir_all(ws.path().join(".mare/plugins")).unwrap();
            std::fs::write(
                ws.path().join(".mare/plugins/fs.lua"),
                r#"return {
                    name = "fs", override = true, provide = { "fs" },
                    apply = function(ctx)
                        local Fs = require("cordis").class("FsBoundaryProbe", mare.Service)
                        function Fs:constructor(c) Fs.super.constructor(self, c, "fs") end
                        local function checked(fn, ...)
                            local ok, value, err = pcall(fn, ...)
                            if not ok or value == nil or value == false then
                                require("mare_lib").fail("denied", tostring(ok and err or value))
                            end
                            return value
                        end
                        function Fs:read(path)
                            return { data_base64 = mare.crypto.base64_encode(checked(mare.fs.read, path)) }
                        end
                        function Fs:write(path, data)
                            checked(mare.fs.write, path, mare.crypto.base64_decode(data))
                            return {}
                        end
                        function Fs:list(path)
                            local data = {}
                            for _, e in ipairs(checked(mare.fs.list, path)) do
                                data[#data + 1] = {
                                    file_name = e.name, is_file = e.kind == "file", is_directory = e.kind == "dir"
                                }
                            end
                            return { data = mare.json.array(data) }
                        end
                        function Fs:stat(path)
                            local st = checked(mare.fs.stat, path, { strict = true })
                            return { is_file = st.kind == "file", is_directory = st.kind == "dir",
                                     size = st.size, created_at_ms = st.ctime or 0,
                                     modified_at_ms = math.floor(st.mtime or 0) }
                        end
                        function Fs:watch()
                            return function() end
                        end
                        mare._fs_boundary_probe_loaded = true
                        ctx:plugin(Fs)
                    end
                }"#,
            )
            .unwrap();

            let cfg = json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}});
            let host = make_rpc_host(ws.path(), cfg, true, false).await;
            assert!(host.lua().load("return mare._fs_boundary_probe_loaded == true").eval::<bool>().unwrap());
            set_lua_sandbox(
                &host,
                r#"{ level = "workspace-write", exclude_slash_tmp = true, exclude_tmpdir_env_var = true, restrict_read = true }"#,
            );
            let (mut cw, mut cr, task, _) = open_rpc_session(&host, true, json!({})).await;
            let inside = call_rpc(&mut cw, &mut cr, 1, "fs.read", json!({"path": "inside.txt"})).await;
            assert_eq!(STANDARD.decode(inside["result"]["data_base64"].as_str().unwrap()).unwrap(), b"inside");

            let secret = outside.path().join("secret.txt").to_string_lossy().into_owned();
            let escaped_write = outside.path().join("escaped.txt").to_string_lossy().into_owned();
            let outside_dir = outside.path().to_string_lossy().into_owned();
            for (id, method, args) in [
                (2, "fs.read", json!({"path": secret})),
                (3, "fs.stat", json!({"path": secret})),
                (4, "fs.list", json!({"path": outside_dir})),
                (5, "fs.write", json!({"path": escaped_write, "data_base64": STANDARD.encode(b"escape")})),
            ] {
                let response = call_rpc(&mut cw, &mut cr, id, method, args).await;
                assert_eq!(response["error"]["data"]["code"], "denied", "{method} escaped its sandbox: {response}");
            }
            assert!(!outside.path().join("escaped.txt").exists());
            close_rpc_session(cw, cr, task).await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn fs_confined_explicit_deny_read_requires_partial_acceptance() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("public.txt"), "public").unwrap();
            std::fs::write(ws.path().join("secret.txt"), "secret").unwrap();
            let cfg = json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}});
            let host = make_rpc_host(ws.path(), cfg, false, true).await;
            let (mut cw, mut cr, task, _) = open_rpc_session(&host, true, json!({})).await;

            set_lua_sandbox(&host, r#"{ level = "workspace-write", deny_read = { "secret.txt" } }"#);
            let strict = call_rpc(&mut cw, &mut cr, 1, "fs.read", json!({"path": "public.txt"})).await;
            assert_eq!(strict["error"]["data"]["code"], "denied", "strict policy must fail closed: {strict}");
            let strict_watch = call_rpc(&mut cw, &mut cr, 10, "fs.watch", json!({"path": "."})).await;
            assert_eq!(
                strict_watch["error"]["data"]["code"], "denied",
                "confined fs.watch must fail closed under strict policy: {strict_watch}"
            );
            assert!(
                strict["error"]["message"].as_str().is_some_and(|m| m.contains("SANDBOX_INSUFFICIENT")),
                "strict policy must report the missing capability: {strict}"
            );

            set_lua_sandbox(
                &host,
                r#"{ level = "workspace-write", deny_read = { "secret.txt" }, accept_partial = { "fs_deny_read" } }"#,
            );
            let public = call_rpc(&mut cw, &mut cr, 2, "fs.read", json!({"path": "public.txt"})).await;
            assert_eq!(STANDARD.decode(public["result"]["data_base64"].as_str().unwrap()).unwrap(), b"public");
            let secret = call_rpc(&mut cw, &mut cr, 3, "fs.read", json!({"path": "secret.txt"})).await;
            assert_eq!(secret["error"]["data"]["code"], "denied", "explicitly denied path leaked: {secret}");
            close_rpc_session(cw, cr, task).await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn fs_confined_rejects_remote_calls_after_session_is_poisoned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("public.txt"), "public").unwrap();
            let cfg = json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}});
            let host = make_rpc_host(ws.path(), cfg, false, true).await;
            let (mut cw, mut cr, task, _) = open_rpc_session(&host, true, json!({})).await;
            let before = call_rpc(&mut cw, &mut cr, 1, "fs.read", json!({"path": "public.txt"})).await;
            assert!(before.get("result").is_some(), "{before}");
            mare_binding::exec_gate::poison(host.lua(), "test session barrier");
            let after = call_rpc(&mut cw, &mut cr, 2, "fs.read", json!({"path": "public.txt"})).await;
            assert!(after.get("error").is_some(), "poisoned session must refuse remote fs.read: {after}");
            close_rpc_session(cw, cr, task).await;
            host.shutdown().await;
        })
        .await;
}

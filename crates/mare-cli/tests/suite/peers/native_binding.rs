use super::*;

/// Receiver plugin: takes over every inbound message and writes it to `MARE_TEST_INBOUND`.
const CAPTURE: &str = r#"return { name = 'capture', apply = function(ctx)
  ctx:on('peer/inbound', function(m)
    local f = assert(io.open(os.getenv('MARE_TEST_INBOUND'), 'w'))
    f:write(mare.json.encode(m)); f:close(); return true
  end)
end }"#;

#[tokio::test]
async fn native_effect_client_sends_current_session_and_closes_on_unload() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let record = tempfile::NamedTempFile::new().unwrap();
        let record_path = record.path().to_string_lossy().to_string();
        let receiver = session_env(
            json!([]),
            r#"name = "native-receiver""#,
            false,
            &[("MARE_TEST_INBOUND", &record_path)],
            &[("capture.lua", CAPTURE)],
        );
        let _watch = watchdog(receiver.pid(), 60);
        let directory = mare_binding::peers::dir();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(&receiver.json_path, directory.join(receiver.json_path.file_name().unwrap())).unwrap();
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.on('session/changed', function()
              assert(mare._native_client:send({text='during switch'}))
            end)"#,
        )
        .unwrap();
        let plugin = plugins.join("native_peer.lua");
        std::fs::write(
            &plugin,
            r#"return { name = 'native_peer', apply = function(ctx)
              ctx:effect(function()
                local client, err = mare.peers.connect('native-receiver')
                assert(client, err and err.message)
                mare._native_client = client
                return function() client:close() end
              end)
            end }"#,
        )
        .unwrap();
        let host = env.host().user_config(true).build().await;
        expose(&host);
        assert!(
            !mare_kernel::recent_events().iter().any(|event| event == "session/changed"),
            "initial peer identity must not emit a public session switch"
        );
        let first: String = eval(&host, "return S:info().session_id").await;
        // sent once the host has started (a startup effect's message carries no session yet)
        exec(&host, "assert(mare._native_client:send({text='before switch'}))").await;
        let received: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).unwrap()).unwrap();
        assert_eq!(received["text"], "before switch");
        assert_eq!(received["from"]["session_id"], first);
        let owned: mlua::AnyUserData = eval(&host, "return mare._native_client").await;
        let owned = owned.borrow::<mare_binding::peer_client::PeerClient>().unwrap().clone();

        exec(&host, "S:create()").await;
        let second: String = eval(&host, "return S:info().session_id").await;
        assert_ne!(first, second);
        let received: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).unwrap()).unwrap();
        assert_eq!(received["text"], "during switch");
        assert_eq!(received["from"]["session_id"], second);
        exec(&host, "assert(mare._native_client:send({text='after switch'}))").await;
        let received: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).unwrap()).unwrap();
        assert_eq!(received["text"], "after switch");
        assert_eq!(received["from"]["session_id"], second);
        std::fs::remove_file(&plugin).unwrap();
        host.reload_all().await.unwrap();
        assert!(owned.is_closed(), "effect cleanup must close the retained client");
        host.shutdown().await;
        assert_eq!(mare_binding::peers::session_cell(host.lua()).get(), None, "shutdown clears this host's session");
        receiver.shutdown();
    })
    .await;
}

/// lazy-audit 1534: ACP runs one Host per `session/new` in one process. Each host's clients name
/// that host's session, and a host that starts or shuts down later changes no other host's.
#[tokio::test]
async fn hosts_in_one_process_each_stamp_their_own_session() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let record = tempfile::NamedTempFile::new().unwrap();
        let record_path = record.path().to_string_lossy().to_string();
        let receiver = session_env(
            json!([]),
            r#"name = "multi-host-receiver""#,
            false,
            &[("MARE_TEST_INBOUND", &record_path)],
            &[("capture.lua", CAPTURE)],
        );
        let _watch = watchdog(receiver.pid(), 90);
        let directory = mare_binding::peers::dir();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(&receiver.json_path, directory.join(receiver.json_path.file_name().unwrap())).unwrap();
        let a = env.host().build().await;
        let b = env.host().build().await;
        expose(&a);
        expose(&b);
        let a_id: String = eval(&a, "return S:info().session_id").await;
        let b_id: String = eval(&b, "return S:info().session_id").await;
        assert_ne!(a_id, b_id);
        let connect = "CLIENT = assert(mare.peers.connect('multi-host-receiver'))";
        exec(&a, connect).await;
        exec(&b, connect).await;
        let sent = |text: &str| {
            let received: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).unwrap()).unwrap();
            assert_eq!(received["text"], text);
            received["from"]["session_id"].clone()
        };
        exec(&a, "assert(CLIENT:send({text='from a'}))").await;
        assert_eq!(sent("from a"), a_id, "A's client names A's session after B started");
        exec(&b, "assert(CLIENT:send({text='from b'}))").await;
        assert_eq!(sent("from b"), b_id, "B's client names B's session");
        exec(&b, "CLIENT:close()").await;
        b.shutdown().await;
        exec(&a, "assert(CLIENT:send({text='from a after b'}))").await;
        assert_eq!(sent("from a after b"), a_id, "B's shutdown leaves A's session in place");
        exec(&a, "CLIENT:close()").await;
        a.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

#[tokio::test]
async fn native_ctx_old_signature_reports_removed_owner() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("native_legacy.lua"),
            r#"return { name = 'native_legacy', apply = function(ctx)
              mare._native_legacy = function()
                local ok, err = pcall(mare.peers.connect, ctx, 'unused')
                return ok, tostring(err)
              end
            end }"#,
        )
        .unwrap();
        let host = env.host().user_plugins().build().await;
        let (ok, error): (bool, String) = eval(&host, "return mare._native_legacy()").await;
        assert!(!ok);
        assert!(error.contains("mare.peers.connect(target, opts): the owner argument is gone"), "{error}");
        host.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn lua_client_capacity_is_process_wide_and_close_releases_a_slot() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let receiver = session(json!([]), r#"name = "capacity-receiver""#);
        let _watch = watchdog(receiver.pid(), 90);
        let directory = mare_binding::peers::dir();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(&receiver.json_path, directory.join(receiver.json_path.file_name().unwrap())).unwrap();
        let host = env.host().build().await;
        let other = env.host().build().await;
        let entry: Value = serde_json::from_str(&std::fs::read_to_string(&receiver.json_path).unwrap()).unwrap();
        global(&host, "PEER_PID", entry["pid"].as_u64().unwrap());
        global(&host, "PEER_STARTED", entry["started_at_ms"].as_u64().unwrap());
        let legacy: String = eval(
            &host,
            r#"
            local ok, err = pcall(mare.peers.connect, {}, 'capacity-receiver')
            assert(not ok)
            CLIENTS = {}
            CLIENTS[1] = assert(mare.peers.connect({pid=PEER_PID, started_at_ms=PEER_STARTED}))
            for i = 2, 64 do
              local client, failure = mare.peers.connect('capacity-receiver')
              assert(client, failure and failure.message)
              CLIENTS[i] = client
            end
            return tostring(err)
            "#,
        )
        .await;
        let capacity: String = eval(
            &other,
            r#"
            CLIENTS = {}
            for i = 1, 64 do
              local client, failure = mare.peers.connect('capacity-receiver')
              assert(client, failure and failure.message)
              CLIENTS[i] = client
            end
            local overflow, failure = mare.peers.connect('capacity-receiver')
            assert(overflow == nil)
            return failure.code
            "#,
        )
        .await;
        exec(&host, "CLIENTS[1]:close()").await;
        exec(
            &other,
            r#"
            local replacement, replacement_error = mare.peers.connect('capacity-receiver')
            assert(replacement, replacement_error and replacement_error.message)
            replacement:close()
            for _, client in ipairs(CLIENTS) do client:close() end
            "#,
        )
        .await;
        exec(&host, "for _, client in ipairs(CLIENTS) do client:close() end").await;
        assert_eq!(capacity, "capacity");
        assert!(legacy.contains("mare.peers.connect(target, opts): the owner argument is gone"), "{legacy}");
        other.shutdown().await;
        host.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_client_survives_switch_and_closes_with_owner_reload() {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;

    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let receiver = session(json!([]), r#"name = "rpc-lifetime-receiver""#);
        let _receiver_watch = watchdog(receiver.pid(), 90);
        let proxy_dir = tempfile::tempdir().unwrap();
        let proxy_path = proxy_dir.path().join("proxy.sock");
        let listener = UnixListener::bind(&proxy_path).unwrap();
        let target_path = receiver.sock.clone();
        let (closed_tx, closed_rx) = mpsc::channel();
        let proxy = std::thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut remote = UnixStream::connect(target_path).unwrap();
            let mut from_remote = remote.try_clone().unwrap();
            let mut to_client = client.try_clone().unwrap();
            let response = std::thread::spawn(move || std::io::copy(&mut from_remote, &mut to_client));
            let result = std::io::copy(&mut client, &mut remote);
            let _ = remote.shutdown(std::net::Shutdown::Both);
            let _ = response.join();
            closed_tx.send(()).unwrap();
            result.unwrap();
        });
        let mut entry: Value = serde_json::from_str(&std::fs::read_to_string(&receiver.json_path).unwrap()).unwrap();
        entry["socket"] = json!(proxy_path);
        let directory = mare_binding::peers::dir();
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join(receiver.json_path.file_name().unwrap());
        std::fs::write(destination, entry.to_string()).unwrap();

        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let plugin = plugins.join("owner_lifetime.lua");
        std::fs::write(
            &plugin,
            r#"return { name = 'owner_lifetime', apply = function(ctx)
              ctx:effect(function()
                local client = assert(mare.peers.connect('rpc-lifetime-receiver'))
                mare._lifetime_client = client
                return function() client:close() end
              end)
            end }"#,
        )
        .unwrap();

        let host = env.host().user_plugins().build().await;
        expose(&host);
        exec(&host, "S:create()").await;
        let state: String = eval(&host, "return assert(mare._lifetime_client:status()).state").await;
        assert_eq!(state, "idle");
        assert!(closed_rx.try_recv().is_err(), "client closed during session switch");

        std::fs::remove_file(&plugin).unwrap();
        host.reload_all().await.unwrap();
        closed_rx.recv_timeout(Duration::from_secs(30)).expect("client did not close with owner reload");
        host.shutdown().await;
        proxy.join().unwrap();
        receiver.shutdown();
    })
    .await;
}

#[tokio::test]
async fn peer_registry_status_published_and_client_send_project_secrets() {
    const STATUS_SEC: &str = "PEER_STATUS_SEC_1";
    const PUB_SEC: &str = "PEER_PUB_SEC_2";
    const SEND_SEC: &str = "PEER_SEND_SEC_3";

    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let record = tempfile::NamedTempFile::new().unwrap();
        let record_path = record.path().to_string_lossy().to_string();
        let receiver = session_env(
            json!([]),
            r#"name = "peer-proj-receiver""#,
            false,
            &[("MARE_TEST_INBOUND", &record_path)],
            &[("capture.lua", CAPTURE)],
        );
        let _watch = watchdog(receiver.pid(), 60);

        // 1. Peer registry non-event fields (`current_status` + `mare.peers.publish`) are projected on disk.
        let publisher_plugin = format!(
            r#"return {{
                name = "status",
                override = true,
                provide = {{ "status" }},
                apply = function(ctx)
                    local cordis = require("cordis")
                    local Status = cordis.class("Status", mare.Service)
                    function Status:constructor(c) Status.super.constructor(self, c, "status") end
                    function Status:get()
                        return {{
                            state = "idle",
                            message = "{STATUS_SEC}",
                            session = {{ session_id = "s1", path = "/tmp/s1", resume = {{}} }},
                            cwd = "/tmp",
                            pid = 1,
                            seq = 1,
                        }}
                    end
                    KEEP_PEER_PUB_LEASES = {{
                        mare.projection.register("{STATUS_SEC}"),
                        mare.projection.register("{PUB_SEC}"),
                    }}
                    ctx:plugin(Status)
                    mare.peers.publish("probe", {{ token = "{PUB_SEC}" }})
                end
            }}"#
        );
        let publisher =
            session_env(json!([]), r#"name = "peer-proj-publisher""#, false, &[], &[("status.lua", &publisher_plugin)]);
        let _pub_watch = watchdog(publisher.pid(), 60);
        let reg_text = std::fs::read_to_string(&publisher.json_path).unwrap();
        assert!(!reg_text.contains(STATUS_SEC), "peer registry status leaked plaintext: {reg_text}");
        assert!(!reg_text.contains(PUB_SEC), "peer registry plugins leaked plaintext: {reg_text}");
        assert!(reg_text.matches("[[mare-value:").count() >= 2, "expected projected markers in registry: {reg_text}");
        publisher.shutdown();

        // 2. `mare.peers.connect():send` projects `text` before crossing the peer socket.
        let directory = mare_binding::peers::dir();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(&receiver.json_path, directory.join(receiver.json_path.file_name().unwrap())).unwrap();
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("peer_send_probe.lua"),
            format!(
                r#"return {{ name = 'peer_send_probe', apply = function(ctx)
                    KEEP_PEER_SEND_LEASE = mare.projection.register("{SEND_SEC}")
                    ctx:effect(function()
                        local client, err = mare.peers.connect('peer-proj-receiver')
                        assert(client, err and err.message)
                        mare._proj_peer_client = client
                        return function() client:close() end
                    end)
                end }}"#
            ),
        )
        .unwrap();
        let host = env.host().user_config(true).build().await;
        exec(&host, &format!(r#"assert(mare._proj_peer_client:send({{ text = "hi {SEND_SEC}" }}))"#)).await;
        let received: Value = serde_json::from_str(&std::fs::read_to_string(record.path()).unwrap()).unwrap();
        let msg_text = received["text"].as_str().unwrap();
        assert!(!msg_text.contains(SEND_SEC), "peer send leaked plaintext: {received}");
        assert!(msg_text.contains("[[mare-value:"), "expected projected peer message: {received}");
        host.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

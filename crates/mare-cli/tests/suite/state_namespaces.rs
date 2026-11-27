//! Isolated durable namespaces: public Lua/RPC plus real child/headless model tools.
use crate::common::*;
use serde_json::{Value, json};
use std::process::Stdio;

const PROBE: &str = r#"
return {name='namespace-probe',inject={'tools'},apply=function(ctx)
    ctx.tools:register('namespace_probe',{description='Check durable namespace state',side_effect='write',idempotent=false,
        parameters={type='object',properties={}},execute=function()
            local namespace=mare.agent_id and 'child.probe' or 'headless.probe'
            local before=mare.state.snapshot({namespace=namespace});assert(before.status=='ok')
            local data=assert(mare.json.decode_lossless('{"null":null,"object":{},"array":[]}'))
            local saved=mare.state.compare_exchange({namespace=namespace,expected_revision=before.snapshot.revision,value=data})
            assert(saved.status=='ok',mare.json.encode(saved))
            local actual=mare.state.snapshot({namespace=namespace});assert(actual.status=='ok')
            assert(mare.json.encode_strict(actual.snapshot.value)==mare.json.encode_strict(data))
            return {content='NAMESPACE_MODEL_OK'}
        end})
end}
"#;
fn script() -> Value {
    json!([{"tool_calls":[{"name":"namespace_probe","arguments":{}}]},{"text":"namespace complete"}])
}

#[tokio::test]
async fn namespace_public_lua_scope_cancel_and_real_child_tool() {
    local(async {
        let env=TestEnv::new().await;
        mare_plugin::runtime::ensure().unwrap();
        let plugins=env.config_dir().join("plugins");std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("namespace-probe.lua"),PROBE).unwrap();
        let h=env.host().faux(script()).no_session().build().await;
        let result:bool=eval(&h,r#"
            local initial=mare.state.snapshot({namespace='root.probe'});assert(initial.status=='ok' and initial.snapshot.revision==0)
            local value=assert(mare.json.decode_lossless('{"n":null,"o":{},"a":[]}'))
            local written=mare.state.compare_exchange({namespace='root.probe',expected_revision=0.0,value=value})
            assert(written.status=='ok' and written.snapshot.revision==1)
            local conflict=mare.state.compare_exchange({namespace='root.probe',expected_revision=0,value='wrong'})
            assert(conflict.status=='conflict' and conflict.snapshot.value.n==mare.json.null)
            local token=mare.task.token();token:cancel()
            local cancelled=mare.state.compare_exchange({namespace='cancel.probe',expected_revision=0,value=value},{cancel=token})
            assert(cancelled.status=='rejected' and cancelled.error.code=='cancelled')
            assert(not mare.state.snapshot({namespace='cancel.probe'}).snapshot.exists)
            local captured=mare.state.compare_exchange
            mare.exec.run({tool='namespace-inspect',lifetime='ephemeral-read-only',sandbox={level='read-only'}},function()
                local denied=captured({namespace='readonly.probe',expected_revision=0,value=value})
                assert(denied.status=='rejected' and denied.error.code=='read_only')
                mare.task.spawn(function()
                    local inherited=captured({namespace='readonly-child.probe',expected_revision=0,value=value})
                    assert(inherited.status=='rejected' and inherited.error.code=='read_only')
                end):wait()
            end)
            local callback_ran=false
            local rejected=mare.state.compare_exchange({namespace='callback.probe',expected_revision=0,value={callback=function() callback_ran=true end}})
            assert(rejected.status=='rejected' and not callback_ran)
            local options=setmetatable({},{__index=function() callback_ran=true end})
            local invalid_options=mare.state.compare_exchange({namespace='callback.probe',expected_revision=0,value=value},options)
            assert(invalid_options.status=='rejected' and not callback_ran)
            mare.exec.run({tool='namespace-deny',sandbox={level='read-only',deny_read={mare.cwd},accept_partial={'fs_deny_read'}}},function()
                local denied=captured({namespace='deny.probe',expected_revision=0,value=value})
                assert(denied.status=='rejected' and denied.error.code=='denied')
            end)
            local read=mare.state.snapshot({namespace='readonly.probe'});assert(not read.snapshot.exists)
            local artifact=mare.paths.runtime .. '/types/mare/state.lua'
            assert(mare.fs.read(artifact):find('function mare.state.compare_exchange',1,true))
            return true
        "#).await;
        assert!(result);
        let result:String=eval(&h,r#"local child=mare.subagent.start({prompt='Use namespace_probe then finish',tools={'namespace_probe'}})
            return mare.json.encode(child:wait())"#).await;
        assert!(result.contains("NAMESPACE_MODEL_OK"),"{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn namespace_owner_plugin_unload_cancels_a_waiting_write() {
    local(async {
        let env = TestEnv::new().await;
        let initial_host=env.host().faux(json!([{"text":"unused"}])).no_session().build().await;
        assert!(eval::<bool>(&initial_host,"return mare.state.compare_exchange({namespace='unload.probe',expected_revision=0,value='initial'}).status == 'ok'").await);
        initial_host.shutdown().await;
        let domains = env.home.path().join("state/domains");
        let data_file=std::fs::read_dir(&domains).unwrap().flatten().map(|e|e.path()).find(|p|
            p.extension().is_some_and(|e|e=="json") && std::fs::read(p).ok()
                .and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|entry|entry["namespace"]=="unload.probe")
        ).unwrap();
        let lock = mare_shared::private::open(
            std::fs::OpenOptions::new().create(true).write(true).truncate(false),
            &data_file.with_extension("lock"),
        )
        .unwrap();
        lock.try_lock().unwrap();
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("pending-state.lua"),
            r#"
            return {name='pending-state',inject={'task'},
                contracts={ns_test={owner='pending-state',events={waiting={doc='test waiting probe',mode='bail',expose='process'}}}},
                apply=function(ctx)
                local waiting=false
                ctx:on('ns_test/waiting',function() return waiting end)
                ctx.task:spawn(function()
                    waiting=true
                    mare.state.compare_exchange({namespace='unload.probe',expected_revision=1,value='must not publish'})
                end)
            end}
        "#,
        )
        .unwrap();
        let h = env.host().faux(json!([{"text":"unused"}])).no_session().user_plugins().build().await;
        expose(&h);
        wait_until(5, "state operation to enter its owner scope", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return C.cordis:bail('ns_test/waiting') == true").await }
        })
        .await;
        crate::common::unload_plugin(&h.kernel, "pending-state").await;
        drop(lock);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(eval::<String>(&h,"return mare.state.snapshot({namespace='unload.probe'}).snapshot.value").await,"initial","unloaded scope published state");
        h.shutdown().await;
    })
    .await
}

#[test]
fn namespace_rpc_headless_and_restart_preserve_exact_json() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config/plugins");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("namespace-probe.lua"), PROBE).unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, script().to_string()).unwrap();
    let launch = || {
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--yolo", "--no-session", "--model", "faux/echo"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(home.path())
            .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
        rpc
    };
    let mut rpc = launch();
    let value = json!({"n":null,"o":{},"a":[],"nested":[null,{},[]]});
    let result = rpc.call_raw(
        "state.compare_exchange",
        json!({"namespace":"rpc.probe","expected_revision":0,"value":value}),
        |_| json!(null),
    );
    assert_eq!(result["result"]["status"], "ok", "{result}");
    assert_eq!(result["result"]["snapshot"]["value"], value);
    let conflict = rpc.call_raw(
        "state.compare_exchange",
        json!({"namespace":"rpc.probe","expected_revision":0,"value":"wrong"}),
        |_| json!(null),
    );
    assert_eq!(conflict["result"]["status"], "conflict");
    let invalid = rpc.call_raw("state.snapshot", json!({"namespace":"../escape"}), |_| json!(null));
    assert_eq!(invalid["result"]["error"]["code"], "invalid_namespace");
    rpc.shutdown_and_wait();
    let mut rpc = launch();
    let restored = rpc.call_raw("state.snapshot", json!({"namespace":"rpc.probe"}), |_| json!(null));
    assert_eq!(restored["result"]["snapshot"]["value"], value);
    assert_eq!(restored["result"]["snapshot"]["revision"], 1);
    rpc.shutdown_and_wait();
    let output = mare_cmd()
        .args(["-p", "--json", "--no-session", "--model", "faux/echo", "--yolo", "Use namespace_probe then finish"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .current_dir(home.path())
        .run_output();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("NAMESPACE_MODEL_OK"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

//! Pure time queries through Lua, real child model tools and headless model tools
//! (RPC exposure is a service contract, not a hand-written method).
use crate::common::*;
use serde_json::{Value, json};
const PROBE: &str = r#"
return {name='time-probe',inject={'tools'},apply=function(ctx)
    ctx.tools:register('query_time_probe',{description='Check deterministic recurrence',side_effect='read',idempotent=true,
        parameters={type='object',properties={}},execute=function()
            local v,e=mare.time.query({action='resolve',pattern={kind='interval',anchor_ms=0,every_ms=60000},scheduled_ms=60000,accepted_ms=255000})
            assert(v,e); assert(v.occurrence_ms==240000 and v.next_ms==300000)
            return {content='TIME_QUERY_OK'}
        end})
end}
"#;
fn probe(env: &TestEnv) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("time-probe.lua"), PROBE).unwrap();
}
fn script() -> Value {
    json!([{"tool_calls":[{"name":"query_time_probe","arguments":{}}]},{"text":"time query complete"}])
}
#[tokio::test]
async fn time_query_is_public_in_root_and_real_child_runtime() {
    local(async {
        let env=TestEnv::new().await;
        probe(&env);
        let h=env.host().faux(script()).no_session().build().await;
        let root:bool=eval(&h,r#"local r=mare.time.query({action='instant',value='2026-01-01T00:00:00Z'}) return r.instant_ms==1767225600000"#).await;
        assert!(root);
        let encoded:String=eval(&h,r#"local child=mare.subagent.start({prompt='Use query_time_probe, then finish',tools={'query_time_probe'}})
            return mare.json.encode(child:wait())"#).await;
        let result:Value=serde_json::from_str(&encoded).unwrap();
        assert!(result["error"].is_null(),"{result}");
        assert!(encoded.contains("TIME_QUERY_OK"),"{encoded}");
        h.shutdown().await;
    }).await
}
#[test]
fn time_query_runs_in_a_headless_faux_model_tool() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config/plugins");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("time-probe.lua"), PROBE).unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, script().to_string()).unwrap();
    let output = mare_cmd()
        .args(["-p", "--json", "--no-session", "--model", "faux/echo", "--yolo", "Use query_time_probe then finish"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .current_dir(home.path())
        .run_output();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("TIME_QUERY_OK"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

use super::*;

#[test]
fn rpc_ui_multi_select_forwards_options_and_returns_all_indices() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("select_probe.lua"),
        r#"return { name = 'select-probe', inject = { 'commands', 'ui' }, apply = function(ctx)
          ctx.commands:register('select-probe', { run = function()
            local indices, tab, key, filter = ctx.ui:select({ title = 'Pick', multi = true,
              accept_label = 'apply', allow_search = false,
              items = { { label = 'one', checked = true }, 'two', 'three' } })
            assert(indices[1] == 1 and indices[2] == 3 and tab == 1 and key == nil and filter == 'red')
            ctx.ui:notify('selection complete', 'info')
          end })
        end }"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut client = RpcClient::spawn_cmd(&mut cmd);
    client.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    let command = client.request("commands.run", json!({"line": "/select-probe"}));
    let mut answered = false;
    let mut completed = false;
    loop {
        let message = client.read();
        if message["method"] == "ui.select" {
            let params = &message["params"];
            assert_eq!(params["multi"], true, "{params}");
            assert_eq!(params["accept_label"], "apply", "{params}");
            assert_eq!(params["allow_search"], false, "{params}");
            assert_eq!(params["items"][0]["checked"], true, "{params}");
            client.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {
                "index": 0, "selected_indices": [0, 2], "tab": 0, "filter": "red"
            }}));
            answered = true;
        } else if message["method"] == "ui.notify" && message["params"]["text"] == "selection complete" {
            completed = true;
        } else if message["id"] == command {
            assert!(message.get("error").is_none(), "{message}");
            break;
        }
    }
    assert!(answered && completed);
    client.shutdown_and_wait();
}

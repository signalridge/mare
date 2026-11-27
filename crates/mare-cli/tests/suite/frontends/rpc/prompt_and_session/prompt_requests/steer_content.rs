use super::*;

#[test]
fn rpc_steer_and_follow_up_keep_images_and_content() {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    use crate::common::rpc::RpcClient;

    // pi: packages/coding-agent/test/agent-session-queued-images.test.ts:65.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (req_tx, req_rx) = mpsc::channel::<Value>();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_srv = Arc::clone(&release);
    let server = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let req_tx = req_tx.clone();
            let release = Arc::clone(&release_srv);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    return;
                }
                if req_line.split_whitespace().nth(1) == Some("/__stop") {
                    return;
                }
                let mut content_len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" || h == "\n" || h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_len];
                reader.read_exact(&mut body).unwrap();
                let parsed: Value = serde_json::from_slice(&body).unwrap();
                let _ = req_tx.send(parsed);
                let (lock, cvar) = &*release;
                let mut guard = lock.lock().unwrap();
                guard = cvar.wait_timeout_while(guard, Duration::from_secs(15), |r| !*r).unwrap().0;
                assert!(*guard, "provider request gate timed out");
                drop(guard);
                let payload = concat!(
                    "event: response.output_text.delta\n",
                    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg\",\"delta\":\"ok\"}\n\n",
                    "event: response.output_item.done\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg\",\"type\":\"message\",",
                    "\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n",
                    "event: response.completed\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"test\",",
                    "\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.try_clone().unwrap().write_all(resp.as_bytes());
            });
        }
    });

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("OPENAI_API_KEY".to_string(), "test-key".to_string()),
    ]);
    let base_url = format!("plugins.provider-openai.base_url=http://127.0.0.1:{port}");
    let args = [
        "--rpc",
        "--yolo",
        "--no-session",
        "--model",
        "openai/gpt-6.1-terra",
        "--set",
        &base_url,
        "--set",
        "plugins.provider-openai.transport=http",
    ];
    let mut client = RpcClient::spawn(&args, &env, &work);
    let init = client.call("initialize", json!({ "protocol_version": "3.0" }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    assert_eq!(client.call("agent.prompt", json!({ "session_id": sid, "input": "hold turn" }))["status"], "started");
    let _first = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();

    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
    let gif = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    client.call(
        "agent.prompt",
        json!({
            "session_id": sid,
            "when_busy": "queue",
            "input": [
                { "type": "text", "text": "follow text" },
                { "type": "text", "text": "ordered part" },
                { "type": "image", "mime": "image/png", "data": png },
            ],
        }),
    );
    client.call(
        "agent.steer",
        json!({
            "session_id": sid,
            "input": [
                { "type": "text", "text": "steer text" },
                { "type": "image", "mime": "image/gif", "data": gif },
            ],
        }),
    );

    {
        let (lock, cvar) = &*release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }

    let is_disclosure = |item: &&Value| {
        item["content"]
            .as_str()
            .or_else(|| item["content"].as_array().and_then(|a| a.first()).and_then(|b| b["text"].as_str()))
            .is_some_and(|t| t.starts_with("<system-reminder>"))
    };
    let after_steer = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    let users: Vec<&Value> =
        after_steer["input"].as_array().unwrap().iter().filter(|i| i["role"] == "user" && !is_disclosure(i)).collect();
    assert_eq!(users.len(), 2, "{after_steer}");
    assert_eq!(
        users[1]["content"],
        json!([
            { "type": "input_text", "text": "steer text" },
            { "type": "input_image", "image_url": format!("data:image/gif;base64,{gif}"), "detail": "auto" },
        ]),
        "{after_steer}"
    );

    let after_follow = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    let users: Vec<&Value> =
        after_follow["input"].as_array().unwrap().iter().filter(|i| i["role"] == "user" && !is_disclosure(i)).collect();
    assert_eq!(users.len(), 3, "{after_follow}");
    assert_eq!(
        users[2]["content"],
        json!([
            { "type": "input_text", "text": "follow text" },
            { "type": "input_text", "text": "ordered part" },
            { "type": "input_image", "image_url": format!("data:image/png;base64,{png}"), "detail": "auto" },
        ]),
        "{after_follow}"
    );
    client.wait_event("turn/completed");
    client.shutdown_and_wait();
    let _ = std::net::TcpStream::connect(("127.0.0.1", port))
        .and_then(|mut s| s.write_all(b"GET /__stop HTTP/1.1\r\nHost: localhost\r\n\r\n"));
    drop(server);

    // Cancel before consumption returns queued steer/follow_up items with their images and content.
    let script = tmp.path().join("faux.json");
    std::fs::write(&script, r#"[{"text":"never reached"}]"#).unwrap();
    let faux_env = BTreeMap::from([
        ("MARE_HOME".to_string(), tmp.path().join("home2").display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut rpc = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &faux_env, &work);
    let held_hook: std::sync::Arc<std::sync::Mutex<Option<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
    let held_slot = std::sync::Arc::clone(&held_hook);
    rpc.on_method = Some(Box::new(move |_c, msg| {
        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/ready" {
            let mut slot = held_slot.lock().unwrap();
            if slot.is_none() {
                *slot = Some(msg["id"].clone());
                return true;
            }
        }
        false
    }));
    let init2 = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let sid2 = init2["session"]["session_id"].as_str().expect("session_id").to_string();
    rpc.call("hooks.subscribe", json!({ "scope": "all", "event": "request/ready", "callbacks": ["handle"] }));
    assert_eq!(rpc.call("agent.prompt", json!({ "session_id": sid2, "input": "hold abort" }))["status"], "started");
    while held_hook.lock().unwrap().is_none() {
        let msg = rpc.read();
        rpc.dispatch(msg);
    }
    rpc.call(
        "agent.prompt",
        json!({
            "session_id": sid2,
            "when_busy": "queue",
            "input": [
                { "type": "text", "text": "abort follow" },
                { "type": "image", "mime": "image/png", "data": png },
            ],
        }),
    );
    rpc.call(
        "agent.steer",
        json!({
            "session_id": sid2,
            "input": [
                { "type": "text", "text": "abort steer" },
                { "type": "image", "mime": "image/gif", "data": gif },
            ],
        }),
    );
    let aborted = rpc.call("agent.cancel", json!({ "session_id": sid2 }));
    let cancelled = aborted["removed"].as_array().unwrap();
    assert_eq!(cancelled.len(), 2, "{aborted}");
    assert_eq!(cancelled[0]["lane"], "steer", "{aborted}");
    assert_eq!(cancelled[0]["images"][0]["data"], gif, "{aborted}");
    assert_eq!(cancelled[1]["lane"], "follow_up", "{aborted}");
    assert_eq!(cancelled[1]["images"][0]["data"], png, "{aborted}");
    assert_eq!(cancelled[1]["content"][1]["data"], png, "{aborted}");
    let held_id = held_hook.lock().unwrap().take().unwrap();
    rpc.send(json!({ "id": held_id, "result": { "value": { "decision": "deny", "reason": "aborted" } } }));
    rpc.wait_event("turn/completed");
    rpc.shutdown_and_wait();
}

#[test]
fn rpc_steer_rejects_invalid_image_before_queueing() {
    use std::collections::BTreeMap;

    use crate::common::rpc::RpcClient;

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), tmp.path().join("home").display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut rpc = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, tmp.path());
    let init = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    let err =
        rpc.call_err("agent.steer", json!({ "session_id": sid, "input": [{ "type": "image", "mime": "image/png" }] }));
    assert_eq!(err["code"], -32602, "{err}");
    let audio_err = rpc.call_err(
        "agent.steer",
        json!({ "session_id": sid, "input": [{ "type": "audio", "mime": "audio/mp3", "data": "AAAA" }] }),
    );
    assert_eq!(audio_err["data"]["code"], "unsupported", "{audio_err}");
    let video_err = rpc.call_err(
        "agent.prompt",
        json!({ "session_id": sid, "when_busy": "queue", "input": [{ "type": "video", "mime": "video/mp4", "data": "AAAA" }] }),
    );
    assert_eq!(video_err["data"]["code"], "unsupported", "{video_err}");
    assert_eq!(rpc.call("agent.get", json!({ "session_id": sid }))["queued"], 0);
    rpc.shutdown_and_wait();
}

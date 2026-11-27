use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::{Value, json};

use super::helpers::*;

/// Scenario 5: real `herdr` lifecycle (`idle` -> `working` -> `blocked` on approval ->
/// `idle`), capture `mare`'s reported resume command, quit `mare` and restart isolated `herdr server`,
/// then launch the recorded `mare -r <session_id>` resume command and continue conversation.
#[tokio::test]
#[ignore = "daily acceptance (real model + herdr): run with --profile live --run-ignored only"]
async fn daily_scenario_5_herdr_states_and_restart_resume() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 5: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    require_herdr("DAILY SCENARIO 5");
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    // Use read-only preset so write tool triggers real approval -> `blocked` state in herdr.
    write_herdr_tui_init(home.path(), &model, "read-only", &req_log);

    let herdr = IsolatedHerdr::start();
    // Thin pass-through shim for HERDR_BIN_PATH that logs exact `-- mare -r <id>` argv sent by
    // examples/herdr/init.lua while forwarding every call to the real `herdr` binary.
    let hook_log = home.path().join("herdr-hook-args.log");
    let shim = home.path().join("herdr-shim.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nexec {:?} \"$@\"\n",
            hook_log.display().to_string(),
            herdr.real_herdr.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();

    let pane = herdr.create_workspace(work.path());
    let env_vars = herdr_env_for_pane(&herdr, &pane, Some(&shim));
    let wrapper = write_mare_wrapper(
        home.path(),
        "run-s5",
        home.path(),
        &key,
        &env_vars,
        &["--set", "plugins.onboarding.plugins=false"],
    );
    herdr.run_ok(&["pane", "run", &pane, wrapper.to_str().unwrap()]).unwrap();
    herdr.wait_output(&pane, "╰", "recent", Duration::from_secs(30));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    herdr.send_line(
        &pane,
        "Remember secret HERDR_TOKEN_8831, then use the write tool to create proof.txt containing HERDR_TOKEN_8831.",
    );
    herdr.wait_state(&pane, &["working"], Duration::from_secs(30));
    herdr.wait_output(&pane, "Allow ", "visible", Duration::from_secs(60));
    herdr.wait_state(&pane, &["blocked"], Duration::from_secs(20));

    // Approve once -> write finishes -> idle.
    herdr.send_keys(&pane, "enter");
    let proof_deadline = Instant::now() + Duration::from_secs(60);
    while !work.path().join("proof.txt").exists() {
        assert!(Instant::now() < proof_deadline, "proof.txt not written after approval");
        std::thread::sleep(Duration::from_millis(200)); // sleep-ok: wait for approved tool write
    }
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(60));

    // Extract the resume argv emitted by examples/herdr/init.lua (`-- mare -r <session_id>`).
    let hook_lines = std::fs::read_to_string(&hook_log).unwrap_or_default();
    let resume_id = hook_lines
        .lines()
        .filter(|l| l.contains("report-agent-session"))
        .find_map(|l| l.split(" -- mare -r ").nth(1))
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("missing `-- mare -r <id>` in herdr hook log:\n{hook_lines}"));

    herdr.send_keys(&pane, "ctrl+d");
    let release_deadline = Instant::now() + Duration::from_secs(15);
    while herdr.pane_info(&pane)["agent"].as_str() == Some("mare") {
        assert!(Instant::now() < release_deadline, "mare did not release pane on quit");
        std::thread::sleep(Duration::from_millis(200)); // sleep-ok: await clean quit release
    }

    // Restart the isolated herdr server, verify pane restoration, then execute `mare -r <id>`.
    let mut herdr = herdr;
    herdr.restart_server();
    assert_eq!(herdr.pane_info(&pane)["agent_status"].as_str(), Some("unknown"));

    let resume_wrapper = write_mare_wrapper(
        home.path(),
        "run-s5-resume",
        home.path(),
        &key,
        &herdr_env_for_pane(&herdr, &pane, None),
        &["-r", &resume_id, "--set", "plugins.onboarding.plugins=false"],
    );
    herdr.run_ok(&["pane", "run", &pane, resume_wrapper.to_str().unwrap()]).unwrap();
    herdr.wait_output(&pane, "╰", "recent", Duration::from_secs(30));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    herdr.send_line(
        &pane,
        "Take the number after HERDR_TOKEN_ from earlier, add 1 to it, and reply with only that sum.",
    );
    herdr.wait_output(&pane, "8832", "recent", Duration::from_secs(60));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    herdr.send_keys(&pane, "ctrl+d");
    let reqs = count_requests(&req_log);
    assert!(reqs >= 3, "expected >=3 real provider calls across pre-restart and post-restart resume, got {reqs}");
    eprintln!(
        "DAILY SCENARIO 5 (herdr states + restart resume): PASS in {:.1}s, real_api_calls={reqs}",
        t0.elapsed().as_secs_f64()
    );
}

/// Scenario 6: from-zero `mare login openai --device-auth` -> `mare login status` ->
/// expired token auto-refresh on real CLI binary `mare -p` rewriting `auth.json`, plus verification
/// that a live `opencode-go` request in the same `MARE_HOME` succeeds cleanly without auth leakage.
/// (Real interactive ChatGPT browser login against auth.openai.com still requires manual human confirmation.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "daily acceptance (CLI login/refresh + real model): run with --profile live --run-ignored only"]
async fn daily_scenario_6_subscription_login_and_expiry_refresh() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 6: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    let model = live_model();
    let t0 = Instant::now();

    // Base64url header.payload.sig where payload carries chatgpt_account_id="acct-daily" & email="daily@example.com".
    let payload_b64 = base64_url_nopad(
        br#"{"email":"daily@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct-daily"}}"#,
    );
    let access_jwt = format!("hdr.{payload_b64}.sig");
    let id_jwt = format!("hdr.{payload_b64}.sig");

    let server = MockServer::start().await;
    let base = server.base.clone();
    server.route(
        "/auth/api/accounts/deviceauth/usercode",
        vec![Resp::status(200, r#"{"device_auth_id":"dev-daily","user_code":"DAILY-9001","interval":"1"}"#)],
    );
    server.route(
        "/auth/api/accounts/deviceauth/token",
        vec![
            Resp::status(403, "{}"),
            Resp::status(
                200,
                r#"{"authorization_code":"code-daily","code_challenge":"c","code_verifier":"ver-daily"}"#,
            ),
        ],
    );
    server.route(
        "/auth/oauth/token",
        vec![
            Resp::status(
                200,
                format!(r#"{{"access_token":"{access_jwt}","id_token":"{id_jwt}","refresh_token":"rt-1","expires_in":3600}}"#),
            ),
            Resp::status(
                200,
                r#"{"access_token":"refreshed-access-2","refresh_token":"rt-2","expires_in":3600}"#,
            ),
        ],
    );
    let text_sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"LOGIN_REFRESH_OK\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"LOGIN_REFRESH_OK\",\"annotations\":[]}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":20,\"output_tokens\":5}}}\n\n",
    );
    server.route("/codex", vec![Resp::sse(text_sse)]);

    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            r#"mare.setup {{ model = {model:?} }}
mare.plugin("provider-openai", {{
  codex_base_url = {codex:?},
  auth_base_url = {auth:?},
}})
{counter}"#,
            codex = format!("{base}/codex"),
            auth = format!("{base}/auth"),
            counter = request_counter_lua(&req_log),
        ),
    )
    .unwrap();

    // 1. From-zero subscription device login via real `mare login openai --device-auth` CLI binary.
    let login = Command::new(mare_bin())
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .current_dir(work.path())
        .args(["login", "openai", "--device-auth"])
        .stdin(Stdio::null())
        .run_output();
    let login_out = String::from_utf8_lossy(&login.stdout);
    let login_err = String::from_utf8_lossy(&login.stderr);
    assert!(login.status.success(), "mare login failed:\n{login_err}\n{login_out}");
    assert!(login_out.contains("DAILY-9001"), "expected user code in login output: {login_out}");

    let status = Command::new(mare_bin())
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .current_dir(work.path())
        .args(["login", "status"])
        .stdin(Stdio::null())
        .run_output();
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.status.success() && status_out.contains("openai: ChatGPT ("),
        "login status unexpected: {status_out}"
    );

    // 2. Expire stored `auth.json` on disk (`expires = 0`), then run real `mare -p` against openai model:
    //    must transparently auto-refresh without manual login and update `auth.json` to `rt-2`.
    let auth_path = home.path().join("config/auth.json");
    let mut auth_doc: Value = serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
    auth_doc["openai-codex"]["expires"] = json!(0);
    std::fs::write(&auth_path, serde_json::to_string_pretty(&auth_doc).unwrap()).unwrap();

    let refreshed_run = Command::new(mare_bin())
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .current_dir(work.path())
        .args(["-p", "--json", "--model", "openai/gpt-6.1-terra", "--set", "plugins.onboarding.plugins=false", "hi"])
        .stdin(Stdio::null())
        .run_output();
    assert!(
        refreshed_run.status.success(),
        "mare -p failed during auto-refresh:\n{}\n{}",
        String::from_utf8_lossy(&refreshed_run.stderr),
        String::from_utf8_lossy(&refreshed_run.stdout)
    );
    let codex_auths: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/responses"))
        .filter_map(|r| r.header("authorization").map(str::to_string))
        .collect();
    assert_eq!(codex_auths, ["Bearer refreshed-access-2"]);
    let updated_auth: Value = serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
    assert_eq!(updated_auth["openai-codex"]["refresh"], "rt-2");
    assert_eq!(updated_auth["openai-codex"]["access"], "refreshed-access-2");

    // 3. Also verify a real `opencode-go` call succeeds cleanly in the same MARE_HOME with auth.json present.
    let before_reqs = count_requests(&req_log);
    let live_out = mare_cmd()
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .env("OPENCODE_API_KEY", &key)
        .current_dir(work.path())
        .args([
            "-p",
            "--json",
            "--permission",
            "auto",
            "--set",
            "plugins.onboarding.plugins=false",
            "--model",
            &model,
            "Reply with only: AUTH_ISOLATION_OK",
        ])
        .stdin(Stdio::null())
        .run_output();
    assert!(
        live_out.status.success(),
        "opencode-go live check failed:\n{}",
        redact(&String::from_utf8_lossy(&live_out.stderr), &key)
    );
    let live_stdout = redact(&String::from_utf8_lossy(&live_out.stdout), &key);
    assert!(live_stdout.contains("AUTH_ISOLATION_OK"), "unexpected live output: {live_stdout}");

    let total_reqs = count_requests(&req_log);
    assert!(total_reqs > before_reqs, "expected real opencode-go provider call to be recorded");
    eprintln!(
        "DAILY SCENARIO 6 (CLI subscription login + expiry refresh + live opencode-go): PASS in {:.1}s, live_opencode_calls={}, total_provider_calls={total_reqs}",
        t0.elapsed().as_secs_f64(),
        total_reqs - before_reqs
    );
}

/// Scenario 7: while working in a real `herdr` pane, switch terminal tab focus away (`FocusOut`)
/// and switch back (`FocusIn`); verify the working animation resumes immediately without a
/// multi-second freeze (measured via `MARE_FRAME_LOG`).
#[tokio::test]
#[ignore = "daily acceptance (real model + herdr): run with --profile live --run-ignored only"]
async fn daily_scenario_7_tab_switch_resumes_working_animation() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 7: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    require_herdr("DAILY SCENARIO 7");
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    let frame_log = home.path().join("frames.log");
    write_herdr_tui_init(home.path(), &model, "full-access", &req_log);

    let herdr = IsolatedHerdr::start();
    let pane = herdr.create_workspace(work.path());
    let frame_log_str = frame_log.display().to_string();
    let mut env_vars = herdr_env_for_pane(&herdr, &pane, None);
    env_vars.push(("MARE_FRAME_LOG", frame_log_str.as_str()));
    let wrapper = write_mare_wrapper(
        home.path(),
        "run-s7",
        home.path(),
        &key,
        &env_vars,
        &["--set", "plugins.onboarding.plugins=false"],
    );
    herdr.run_ok(&["pane", "run", &pane, wrapper.to_str().unwrap()]).unwrap();
    herdr.wait_output(&pane, "╰", "recent", Duration::from_secs(30));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    let read_working_ts = |path: &std::path::Path| -> Vec<f64> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let _took = parts.next()?;
                let at_us: f64 = parts.next()?.parse().ok()?;
                let working = parts.next()? == "1";
                working.then_some(at_us / 1000.0)
            })
            .collect()
    };

    herdr.send_line(
        &pane,
        "Run the bash tool with command `sleep 6` and after it finishes reply with only: TAB_RESUME_OK",
    );
    herdr.wait_state(&pane, &["working"], Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(600)); // sleep-ok: let initial focused working frames render

    // Simulate switching away to another terminal tab (`FocusOut`), staying away 1.5s, then returning (`FocusIn`).
    herdr.run_ok(&["pane", "send-text", &pane, "\x1b[O"]).unwrap();
    std::thread::sleep(Duration::from_millis(1500)); // sleep-ok: hold unfocused working window before switching back
    let before_focus_in = read_working_ts(&frame_log).len();
    herdr.run_ok(&["pane", "send-text", &pane, "\x1b[I"]).unwrap();

    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(60));
    herdr.send_keys(&pane, "ctrl+d");

    let all_ts = read_working_ts(&frame_log);
    let post = all_ts.get(before_focus_in..).unwrap_or(&[]);
    let post_gaps: Vec<f64> = post.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    let max_post_gap = post_gaps.iter().copied().fold(0.0_f64, f64::max);
    assert!(post.len() >= 3, "expected working frames after FocusIn, got {} (total={})", post.len(), all_ts.len());
    assert!(
        max_post_gap < 1000.0,
        "working animation stalled after FocusIn: max_post_gap={max_post_gap:.1}ms (gaps={post_gaps:?})"
    );
    eprintln!(
        "DAILY SCENARIO 7 (herdr tab-switch animation resume): PASS in {:.1}s, post_focus_frames={}, max_post_gap={max_post_gap:.1}ms",
        t0.elapsed().as_secs_f64(),
        post.len()
    );
}

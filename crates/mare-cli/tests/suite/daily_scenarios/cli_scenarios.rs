use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::Value;

use super::helpers::*;

/// Scenario 1: long session reaches compaction threshold, triggers auto compaction
/// on real `opencode-go`, persists `type = "compaction"` in `.jsonl`, and retains key context
/// on the subsequent resumed turn to complete a tool task.
#[tokio::test]
#[ignore = "daily acceptance (real model): run with --profile live --run-ignored only"]
async fn daily_scenario_1_long_session_auto_compaction() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 1: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            r#"mare.setup {{ model = {model:?} }}
mare.plugin("compact", {{
  auto = true,
  threshold_tokens = 7500,
  keep_recent_tokens = 600,
  recent_user_tokens = 600,
  stages = {{ "local" }},
  prune_min_saving_tokens = 999999,
}})
{counter}"#,
            counter = request_counter_lua(&req_log)
        ),
    )
    .unwrap();

    // Seed a ~3000-word document so reading + summarizing it pushes context comfortably past threshold_tokens=1200.
    let corpus = (1..=600)
        .map(|i| format!("record-{i:03}: alpha beta gamma delta epsilon zeta eta theta"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(work.path().join("corpus.txt"), corpus).unwrap();

    let out1 = mare_cmd()
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
            "Remember this project codename for later turns: PROJECT_CODENAME=AZURE-FALCON-742. Read corpus.txt using the read tool and briefly confirm both the last record number in corpus.txt and that you stored the codename.",
        ])
        .stdin(Stdio::null())
        .run_output();
    let s1_out = redact(&String::from_utf8_lossy(&out1.stdout), &key);
    let s1_err = redact(&String::from_utf8_lossy(&out1.stderr), &key);
    assert!(out1.status.success(), "turn 1 failed:\nstderr:\n{s1_err}\nstdout:\n{s1_out}");

    let sid = s1_out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "session")
        .and_then(|v| v["data"]["session_id"].as_str().map(str::to_string))
        .expect("missing session id from turn 1");

    // Turn 2 resumes the same session; pre-request context > 1200 triggers auto compaction via real model.
    let out2 = mare_cmd()
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .env("OPENCODE_API_KEY", &key)
        .current_dir(work.path())
        .args([
            "-r",
            &sid,
            "-p",
            "--json",
            "--permission",
            "auto",
            "--set",
            "plugins.onboarding.plugins=false",
            "--model",
            &model,
            "Write a file named answer.txt containing only the exact PROJECT_CODENAME from earlier, then reply with DONE and that codename.",
        ])
        .stdin(Stdio::null())
        .run_output();
    let s2_out = redact(&String::from_utf8_lossy(&out2.stdout), &key);
    let s2_err = redact(&String::from_utf8_lossy(&out2.stderr), &key);
    assert!(out2.status.success(), "turn 2 failed:\nstderr:\n{s2_err}\nstdout:\n{s2_out}");

    let sessions = find_session_files(home.path());
    assert!(!sessions.is_empty(), "expected persisted session file");
    let entries = read_jsonl(&sessions[0]);
    let compactions: Vec<_> = entries.iter().filter(|e| e["type"] == "compaction").collect();
    assert!(!compactions.is_empty(), "expected at least one persisted compaction entry in {}", sessions[0].display());
    assert_all_tool_calls_paired(&entries);

    let answer = std::fs::read_to_string(work.path().join("answer.txt")).unwrap_or_default();
    assert!(answer.contains("AZURE-FALCON-742"), "post-compaction answer.txt lacked codename: {answer:?}");

    let reqs = count_requests(&req_log);
    assert!(reqs >= 3, "expected at least 3 real provider calls (turn1 + compaction summary + turn2), got {reqs}");
    eprintln!(
        "DAILY SCENARIO 1 (long session + auto compaction): PASS in {:.1}s, real_api_calls={reqs}",
        t0.elapsed().as_secs_f64()
    );
}

/// Scenario 2: drive real `mare` TUI inside isolated `herdr`, queue a follow-up while
/// a slow shell command is executing, press `Esc` -> command stops, queued text returns to input
/// box, and sending the restored prompt completes normally on real `opencode-go`.
#[tokio::test]
#[ignore = "daily acceptance (real model + herdr): run with --profile live --run-ignored only"]
async fn daily_scenario_2_esc_interrupt_returns_queued_input() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 2: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    require_herdr("DAILY SCENARIO 2");
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    write_herdr_tui_init(home.path(), &model, "auto", &req_log);

    let herdr = IsolatedHerdr::start();
    let pane = herdr.create_workspace(work.path());
    let env_vars = herdr_env_for_pane(&herdr, &pane, None);
    let wrapper = write_mare_wrapper(
        home.path(),
        "run-s2",
        home.path(),
        &key,
        &env_vars,
        &["--yolo", "--set", "plugins.onboarding.plugins=false"],
    );
    herdr.run_ok(&["pane", "run", &pane, wrapper.to_str().unwrap()]).unwrap();

    herdr.wait_output(&pane, "╰", "recent", Duration::from_secs(30));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    herdr.send_line(
        &pane,
        "Run this exact shell command with the bash tool: touch esc_started.flag && sleep 30 && touch esc_never.flag",
    );
    herdr.wait_state(&pane, &["working"], Duration::from_secs(30));

    let flag_deadline = Instant::now() + Duration::from_secs(60);
    while !work.path().join("esc_started.flag").exists() {
        assert!(
            Instant::now() < flag_deadline,
            "bash tool never created esc_started.flag:\n{}",
            herdr.read_pane(&pane, "recent")
        );
        std::thread::sleep(Duration::from_millis(200)); // sleep-ok: wait for real bash tool start flag
    }

    // Queue a follow-up while the command is still running, then press Esc.
    let queued = "What is 400 plus 50? Reply with only that number.";
    herdr.send_line(&pane, queued);
    herdr.wait_output(&pane, "↳ What is 400 plus 50?", "recent", Duration::from_secs(15));

    herdr.send_keys(&pane, "esc");
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));
    assert!(!work.path().join("esc_never.flag").exists(), "command should have been stopped before completing");

    // Verify queued text moved back out of `↳` queue into the editor input box (bottom region).
    let screen_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let vis = herdr.read_pane(&pane, "visible");
        let in_bottom = vis.lines().rev().take(8).any(|l| l.contains("What is 400 plus 50?"));
        if in_bottom && !vis.contains("↳ What is 400 plus 50?") {
            break;
        }
        assert!(Instant::now() < screen_deadline, "queued message never returned to input:\n{vis}");
        std::thread::sleep(Duration::from_millis(100)); // sleep-ok: wait for restored editor render
    }

    // Submit the restored message cleanly and assert the computed answer `450`.
    herdr.send_keys(&pane, "enter");
    herdr.wait_output(&pane, "450", "recent", Duration::from_secs(60));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    herdr.send_keys(&pane, "ctrl+d");
    let reqs = count_requests(&req_log);
    assert!(reqs >= 2, "expected at least 2 real provider calls (interrupted turn + resumed prompt), got {reqs}");
    eprintln!(
        "DAILY SCENARIO 2 (herdr TUI Esc interrupt + queue return): PASS in {:.1}s, real_api_calls={reqs}",
        t0.elapsed().as_secs_f64()
    );
}

/// Scenario 3: kill real `mare` mid-execution of a side-effecting tool (`SIGKILL`),
/// resume with `mare -c`, verify missing/unknown result recovery in session JSONL and that the
/// real model inspects the workspace before completing.
#[tokio::test]
#[ignore = "daily acceptance (real model): run with --profile live --run-ignored only"]
async fn daily_scenario_3_sigkill_mid_tool_and_continue_session() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 3: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("init.lua"),
        format!("mare.setup {{ model = {model:?} }}\n{}", request_counter_lua(&req_log)),
    )
    .unwrap();

    let mut child_cmd = mare_cmd();
    child_cmd
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
            "Run this exact bash command now: printf 'side-effect-applied' > side_effect.txt && printf STARTED > crash.started && sleep 30",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut child_cmd).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while !work.path().join("crash.started").exists() {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("mare exited prematurely ({status}) before starting side-effect bash command");
        }
        assert!(Instant::now() < deadline, "timed out waiting for crash.started");
        std::thread::sleep(Duration::from_millis(100)); // sleep-ok: poll side-effect tool start marker
    }

    // Wait briefly so the initial tool_call entry is flushed to disk before SIGKILL.
    std::thread::sleep(Duration::from_millis(250)); // sleep-ok: ensure tool_call record reaches disk pre-SIGKILL
    drop(child);

    let sessions = find_session_files(home.path());
    assert!(!sessions.is_empty(), "expected persisted session file prior to SIGKILL");
    let pre_entries = read_jsonl(&sessions[0]);
    let has_call = pre_entries.iter().any(|e| {
        e["type"] == "message"
            && e["message"]["role"] == "assistant"
            && e["message"]["content"]
                .as_array()
                .is_some_and(|arr| arr.iter().any(|b| b["type"] == "tool_call" || b["type"] == "toolCall"))
    });
    assert!(has_call, "session JSONL must record the in-flight tool call before crash");

    let cont = mare_cmd()
        .env("MARE_TEST_ALLOW_LIVE", "1")
        .env("MARE_HOME", home.path())
        .env("OPENCODE_API_KEY", &key)
        .current_dir(work.path())
        .args([
            "-c",
            "-p",
            "--json",
            "--permission",
            "auto",
            "--set",
            "plugins.onboarding.plugins=false",
            "--model",
            &model,
            "The previous run crashed mid-tool. Inspect the workspace (read side_effect.txt if present) before making any assumptions, then write check_report.txt containing FOUND:<exact contents of side_effect.txt>.",
        ])
        .stdin(Stdio::null())
        .run_output();
    let c_out = redact(&String::from_utf8_lossy(&cont.stdout), &key);
    let c_err = redact(&String::from_utf8_lossy(&cont.stderr), &key);
    assert!(cont.status.success(), "mare -c failed:\nstderr:\n{c_err}\nstdout:\n{c_out}");

    let post_entries = read_jsonl(&sessions[0]);
    assert_all_tool_calls_paired(&post_entries);
    let recovered_unknown = post_entries.iter().any(|e| {
        e["type"] == "message"
            && matches!(e["message"]["role"].as_str(), Some("tool_result") | Some("toolResult"))
            && e["message"]["is_error"] == true
    });
    assert!(recovered_unknown, "resuming a crashed tool call must record an error/outcome-unknown tool_result");

    let report = std::fs::read_to_string(work.path().join("check_report.txt")).unwrap_or_default();
    assert!(report.contains("FOUND:side-effect-applied"), "unexpected check_report.txt: {report:?}");

    let reqs = count_requests(&req_log);
    assert!(reqs >= 2, "expected at least 2 real provider calls (pre-crash + mare -c), got {reqs}");
    eprintln!(
        "DAILY SCENARIO 3 (SIGKILL mid-tool + mare -c): PASS in {:.1}s, real_api_calls={reqs}",
        t0.elapsed().as_secs_f64()
    );
}

/// Scenario 4: real >120s command without `timeout_ms` override hits the default 120s
/// foreground limit in real `herdr` TUI, automatically moves to a background job, goes `idle`, and
/// notifies/wakes the agent on completion.
#[tokio::test]
#[ignore = "daily acceptance (real model + herdr, >120s): run with --profile live --run-ignored only"]
async fn daily_scenario_4_over_120s_command_backgrounds_and_wakes() {
    let Some(key) = runtime_live_key() else {
        eprintln!("DAILY SCENARIO 4: skipped ({KEY_ENV} / OPENCODE_API_KEY unavailable)");
        return;
    };
    require_herdr("DAILY SCENARIO 4");
    let model = live_model();
    let t0 = Instant::now();
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let req_log = home.path().join("live-requests.log");
    write_herdr_tui_init(home.path(), &model, "auto", &req_log);

    let herdr = IsolatedHerdr::start();
    let pane = herdr.create_workspace(work.path());
    let env_vars = herdr_env_for_pane(&herdr, &pane, None);
    let wrapper = write_mare_wrapper(
        home.path(),
        "run-s4",
        home.path(),
        &key,
        &env_vars,
        &["--yolo", "--set", "plugins.onboarding.plugins=false"],
    );
    herdr.run_ok(&["pane", "run", &pane, wrapper.to_str().unwrap()]).unwrap();
    herdr.wait_output(&pane, "╰", "recent", Duration::from_secs(30));
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(30));

    let prompt_sent = Instant::now();
    herdr.send_line(
        &pane,
        "Run this bash command (set run_in_background=false and do not set timeout_ms). If it moves to a background job, stop immediately and reply ONLY WAITING_BG without calling job_output or job_wait: i=0; while [ $i -lt 14 ]; do sleep 10; i=$((i+1)); echo tick-$i; done; echo BG_FINISHED_774 && printf 'BG_FINISHED_774' > bg_done.txt",
    );
    herdr.wait_state(&pane, &["working"], Duration::from_secs(30));

    // Wait for the ~120s foreground handoff message ("moved to background job bash-") before the 140s loop finishes.
    herdr.wait_output(&pane, "moved to background job bash-", "recent", Duration::from_secs(160));
    let handoff_secs = prompt_sent.elapsed().as_secs_f64();
    assert!(
        handoff_secs >= 115.0,
        "command should have waited for real ~120s foreground timeout, handoff took {handoff_secs:.1}s"
    );
    assert!(!work.path().join("bg_done.txt").exists(), "background command finished too early before handoff");
    herdr.wait_state(&pane, &["idle", "done"], Duration::from_secs(45));

    // Wait for the 140s loop to complete naturally and wake the idle agent -> working -> idle.
    let done_deadline = Instant::now() + Duration::from_secs(60);
    while !work.path().join("bg_done.txt").exists() {
        assert!(Instant::now() < done_deadline, "background job never wrote bg_done.txt");
        std::thread::sleep(Duration::from_millis(500)); // sleep-ok: await background loop completion
    }
    // Wait for the wake-up turn to settle back to idle and log the completion notification + reply.
    let wake_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let info = herdr.pane_info(&pane);
        let st = info["agent_status"].as_str().unwrap_or("");
        let sessions = find_session_files(home.path());
        let session_entries = sessions.first().map(|s| read_jsonl(s)).unwrap_or_default();
        let notif_idx = session_entries.iter().position(|e| {
            e["type"] == "message"
                && e["message"]["notification"] == true
                && (e.to_string().contains("BG_FINISHED_774") || e.to_string().contains("[background job bash-"))
        });
        let has_wakeup = notif_idx.is_some_and(|i| {
            session_entries[i + 1..].iter().any(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
        });
        if has_wakeup && (st == "idle" || st == "done") {
            break;
        }
        assert!(
            Instant::now() < wake_deadline,
            "agent did not wake up and respond after background completion; state={st}\nscreen:\n{}\nentries:\n{:?}",
            herdr.read_pane(&pane, "recent"),
            session_entries
                .iter()
                .map(|e| (e["type"].clone(), e["message"]["role"].clone(), e["message"]["notification"].clone()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(300)); // sleep-ok: wait for background job wake-up turn
    }

    herdr.send_keys(&pane, "ctrl+d");
    let reqs = count_requests(&req_log);
    assert!(reqs >= 3, "expected >=3 real provider calls (tool call, backgrounded reply, wake-up reply), got {reqs}");
    eprintln!(
        "DAILY SCENARIO 4 (>120s auto-background + wake-up): PASS in {:.1}s (handoff at {handoff_secs:.1}s), real_api_calls={reqs}",
        t0.elapsed().as_secs_f64()
    );
}

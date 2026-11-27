//! Built-in `bash` persistent shell and `command` tool tests.

use super::host;
#[cfg(unix)]
use super::{call, text};
use crate::common::*;
use serde_json::json;

#[cfg(unix)]
#[tokio::test]
async fn bash_reports_a_nonzero_exit_as_an_error_with_output() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "echo out; echo err >&2; exit 3" })).await;
        assert_eq!(r["is_error"], true, "{r}");
        let t = text(&r);
        assert!(t.contains("out") && t.contains("err") && t.contains('3'), "{t}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn command_tools_are_not_readonly_by_default() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let readonly: bool = eval(
            &h,
            r#"T:command({ name = "fmt", description = "format", command = { "true" } }) return T:readonly("fmt")"#,
        )
        .await;
        assert!(!readonly);
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_in_one_persistent_shell_that_resets_on_exit() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("sub")).unwrap();
        let h = host(&env).await;
        // cwd, exported variables and functions carry over
        let r = call(&h, "bash", json!({ "command": "cd sub && export E2_X=kept && f() { echo fn; }" })).await;
        let t = text(&r);
        assert!(t.contains("working directory is now") && t.contains("[Command finished with exit code 0]"), "{t}");
        let r = call(&h, "bash", json!({ "command": "pwd -P; echo $E2_X; f" })).await;
        let t = text(&r);
        assert!(t.contains("/sub") && t.contains("kept") && t.contains("fn"), "{t}");
        // a non-zero status reports it and keeps the shell
        let r = call(&h, "bash", json!({ "command": "false" })).await;
        assert!(text(&r).contains("[Command finished with exit code 1]") && r["is_error"] == true, "{r}");
        // an explicit workdir runs in a temporary shell and leaves the persistent one alone
        let r = call(&h, "bash", json!({ "command": "pwd -P", "workdir": "/" })).await;
        assert_eq!(text(&r).lines().next().unwrap_or(""), "/", "{r}");
        let r = call(&h, "bash", json!({ "command": "echo ${E2_X:-gone}" })).await;
        assert!(text(&r).contains("kept"), "{r}");
        // exit: reset, not repaired; the model is told; the next call starts fresh
        let r = call(&h, "bash", json!({ "command": "exit 3" })).await;
        let t = text(&r);
        assert!(t.contains("[shell exited: code 3]") && t.contains("was reset"), "{t}");
        let r = call(&h, "bash", json!({ "command": "echo ${E2_X:-fresh}" })).await;
        assert!(text(&r).contains("fresh"), "{r}");
        // the directory disappears: back to the starting one, and the model is told
        std::fs::remove_dir(env.cwd.join("sub")).unwrap();
        let r = call(&h, "bash", json!({ "command": "pwd -P" })).await;
        let t = text(&r);
        assert!(t.contains("no longer exists") && !t.lines().next().unwrap_or("").ends_with("/sub"), "{t}");
        h.shutdown().await;
    })
    .await
}

/// An interrupted command in the persistent shell gets SIGINT first —
/// the hard stop's SIGTERM comes only after a second — and the shell is reset (
/// cancellation resets, never repairs): the next call starts in the last directory with a fresh
/// environment.
#[cfg(unix)]
#[tokio::test]
async fn an_interrupted_persistent_shell_command_gets_sigint_and_the_next_call_keeps_the_directory() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("sub")).unwrap();
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "cd sub && export ESC_X=kept" })).await;
        assert!(text(&r).contains("[Command finished with exit code 0]"), "{r}");
        // the command answers SIGINT only: a stop that starts with SIGTERM never writes the file
        // `started` is written only once the traps are in place: a cancel that lands before them
        // would end the shell on the default SIGINT and never write `got_int` (a race under load)
        let cmd = "sh -c 'trap \"echo int > got_int; exit 0\" INT; trap \"\" TERM; touch started; while :; do sleep 0.05; done'";
        global(
            &h,
            "CALL",
            mare_shared::lua::json_to_lua(h.lua(), &json!({ "id": "t2", "name": "bash", "arguments": { "command": cmd } }))
                .unwrap(),
        );
        exec(
            &h,
            r#"TOK = mare.task.token(); RESULT = nil
               mare.task.spawn(function() RESULT = T:execute(CALL, { cancel = TOK }) end)"#,
        )
        .await;
        let started = env.cwd.join("sub/started");
        wait_until(WAIT_SECS, "the command to start", || {
            let p = started.clone();
            async move { p.exists() }
        })
        .await;
        exec(&h, "TOK:cancel()").await;
        wait_until(WAIT_SECS, "the interrupted call to end", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return RESULT ~= nil").await }
        })
        .await;
        assert!(env.cwd.join("sub/got_int").exists(), "the interrupt reached the command as SIGINT");
        let v: mlua::Value = eval(&h, "return RESULT").await;
        let t = text(&mare_shared::lua::lua_to_json(h.lua(), v).unwrap());
        assert!(t.contains("[command aborted]") && t.contains("was reset"), "{t}");
        let r = call(&h, "bash", json!({ "command": "pwd -P; echo ${ESC_X:-fresh}" })).await;
        let t = text(&r);
        assert!(t.lines().next().unwrap_or("").ends_with("/sub") && t.contains("fresh"), "{t}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn the_persistent_shell_sees_this_sessions_current_mare_variables() {
    local(async {
        let env = TestEnv::new().await;
        // an outer mare's value must never reach children (scrubbed, then injected)
        // SAFETY: env-dependent tests hold TestEnv's global lock
        unsafe { std::env::set_var("MARE_SESSION_MODEL", "outer/model") };
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "echo \"model=$MARE_SESSION_MODEL\"" })).await;
        let first = text(&r).lines().next().unwrap_or("").to_string();
        assert!(first.starts_with("model=faux/") && !first.contains("outer"), "{r}");
        // the shell outlives a model switch; the next command sees the new value
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"L:set_model("faux/echo2", { save = false })"#).await;
        let r = call(&h, "bash", json!({ "command": "echo \"model=$MARE_SESSION_MODEL\"" })).await;
        assert_eq!(text(&r).lines().next().unwrap_or(""), "model=faux/echo2", "{r}");
        unsafe { std::env::remove_var("MARE_SESSION_MODEL") };
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_commands_never_inherit_credential_shaped_variables() {
    local(async {
        let env = TestEnv::new().await;
        // SAFETY: env-dependent tests hold TestEnv's global lock
        unsafe {
            std::env::set_var("E2_TEST_TOKEN", "leak");
            std::env::set_var("E2_PLAIN", "kept");
        }
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "echo \"${E2_TEST_TOKEN:-scrubbed} $E2_PLAIN\"" })).await;
        assert!(text(&r).contains("scrubbed kept"), "{r}");
        // an explicitly forwarded credential still arrives
        let v: String = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "sh", args = { "-c", "echo ${E2_TEST_TOKEN:-none}" },
                 env = { E2_TEST_TOKEN = "given" } }); return r.stdout"#,
        )
        .await;
        assert_eq!(v.trim(), "given");
        let v: String = eval(&h, "return mare.proc.env().E2_TEST_TOKEN or 'absent'").await;
        assert_eq!(v, "absent");
        unsafe {
            std::env::remove_var("E2_TEST_TOKEN");
            std::env::remove_var("E2_PLAIN");
        }
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn long_output_says_which_lines_are_shown_and_where_the_rest_is() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "seq 1 5000" })).await;
        let t = text(&r);
        assert!(t.contains("[output truncated: showing lines 1-1000 and "), "{t}");
        let file = t.split("full output").nth(1).and_then(|s| s.split(": ").nth(1)).and_then(|s| s.split(']').next());
        let file = file.expect("names the full-output file");
        assert_eq!(std::fs::read_to_string(file).unwrap().lines().count(), 5000);
        h.shutdown().await;
    })
    .await
}

/// The full-output file of a long command is paged back with `read` (agent-loop corpus;
/// reference implementation toolExecutor.test.ts:1209, :1620; reference implementation
/// packages/spill/spill-policy/tests/spill-policy.spec.ts:262): offset/limit pages put together
/// give the command's output line for line, and no page is itself cut and spilled again (no
/// read → spill → read loop).
#[cfg(unix)]
#[tokio::test]
async fn the_full_output_file_is_paged_back_with_read() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "seq 1 5000" })).await;
        let t = text(&r);
        let file = t.split("full output").nth(1).and_then(|s| s.split(": ").nth(1)).and_then(|s| s.split(']').next());
        let file = file.expect("names the full-output file").to_string();
        let mut lines = Vec::new();
        let mut offset = 1;
        while offset <= 5000 {
            let page = call(&h, "read", json!({ "path": file, "offset": offset, "limit": 1000 })).await;
            let p = text(&page);
            assert!(page["is_error"] != true, "read failed at offset {offset}: {page}");
            assert!(!p.contains("full output"), "a read page was spilled again at offset {offset}: {p}");
            let got: Vec<String> = p
                .lines()
                .map(|l| l.split_once('\t').map_or(l, |(_, rest)| rest).trim().to_string())
                .filter(|l| l.chars().all(|c| c.is_ascii_digit()) && !l.is_empty())
                .collect();
            assert!(!got.is_empty(), "an empty page at offset {offset}: {p}");
            offset += got.len();
            lines.extend(got);
        }
        let want: Vec<String> = (1..=5000).map(|n| n.to_string()).collect();
        assert_eq!(lines, want, "the pages do not give the command's output back");
        h.shutdown().await;
    })
    .await
}

/// "命令在等输入": stdin is empty (a reader gets EOF, the shell's control channel is
/// untouched), and a silent command comes back as "still running" with a hint when its output
/// ends like a question.
#[cfg(unix)]
#[tokio::test]
async fn commands_waiting_for_input_get_empty_stdin_and_a_stall_hint() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "stall_ms": 600 } } }))
            .build()
            .await;
        expose(&h);
        let r = call(&h, "bash", json!({ "command": "read x; echo rc=$?" })).await;
        assert!(text(&r).contains("rc=1"), "a stdin read gets EOF: {r}");
        let r = call(&h, "bash", json!({ "command": "echo still-works" })).await;
        assert!(text(&r).contains("still-works"), "the shell's control channel survived: {r}");
        let r = call(&h, "bash", json!({ "command": "printf 'Overwrite? [y/N] '; sleep 30" })).await;
        let t = text(&r);
        assert!(t.contains("no output for") && t.contains("background job"), "{t}");
        assert!(t.contains("waiting for input") && t.contains("stdin is empty"), "{t}");
        h.shutdown().await;
    })
    .await
}

/// a flood in the persistent shell stays bounded in memory, the complete output is in the spill
/// file, and the end marker is still found (the shell keeps working)
#[cfg(unix)]
#[tokio::test]
async fn a_flooded_persistent_shell_stays_bounded() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "shell_buffer_bytes": 65536 } } }))
            .build()
            .await;
        expose(&h);
        let r =
            call(&h, "bash", json!({ "command": "yes 0123456789 | head -c 3000000; echo; echo END-OF-FLOOD" })).await;
        let t = text(&r);
        let out = r["details"]["output"].as_str().unwrap();
        assert!(out.len() < 80_000 && out.contains("bytes omitted"), "bounded in memory: {}", out.len());
        assert!(
            t.contains("END-OF-FLOOD"),
            "the marker after the flood was found: {}",
            &t[t.len().saturating_sub(400)..]
        );
        let spill =
            t.split("full output").nth(1).and_then(|s| s.split(": ").nth(1)).map(|s| s.split(']').next().unwrap());
        let size = spill.and_then(|s| std::fs::metadata(s.trim()).ok()).map(|m| m.len()).unwrap_or(0);
        assert!(size >= 3_000_000, "the spill file holds everything: {size} ({spill:?})");
        let r = call(&h, "bash", json!({ "command": "echo still-here" })).await;
        assert!(text(&r).contains("still-here") && text(&r).contains("exit code 0"), "{r}");
        h.shutdown().await;
    })
    .await
}

/// spawned commands get a UTF-8 locale when the user's environment sets none, never overriding
/// one that is set. Checked on the environment mare hands the command, not on what the command
/// prints: commands run in a login shell, and a profile that exports LANG (the GitHub macOS and
/// Ubuntu runners do) rightly wins over both mare's default and the inherited value.
#[cfg(unix)]
#[tokio::test]
async fn commands_get_a_utf8_locale_only_when_none_is_set() {
    local(async {
        let mut env = TestEnv::new().await;
        // SAFETY: env-dependent tests hold TestEnv's global lock
        unsafe {
            for k in ["LANG", "LC_ALL", "LC_CTYPE"] {
                std::env::remove_var(k);
            }
        }
        let locale = |h: std::sync::Arc<mare_host::Host>| async move {
            let v: (Option<String>, Option<String>) =
                eval(&h, "local e = T:process_env(); return e.LANG, e.LC_CTYPE").await;
            h.shutdown().await;
            v
        };
        let want = if cfg!(target_os = "macos") { "en_US.UTF-8" } else { "C.UTF-8" };
        assert_eq!(locale(host(&env).await).await, (Some(want.into()), Some(want.into())));
        // the default reaches the command (GH_PAGER is set alongside it, and no profile touches it)
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "echo \"gh=$GH_PAGER\"", "workdir": "." })).await;
        assert!(text(&r).contains("gh=cat"), "{r}");
        h.shutdown().await;
        for k in ["LANG", "LC_ALL", "LC_CTYPE"] {
            env.set_var(k, "fr_FR.ISO8859-1");
            assert_eq!(locale(host(&env).await).await, (None, None), "{k} set: inherited, not overridden");
            // SAFETY: as above
            unsafe { std::env::remove_var(k) };
        }
    })
    .await
}

/// the persistent shell's internal markers never reach the output. `set -x` /
/// `set -v` left on by one command made the shell trace mare's wrapper line on its stderr —
/// `__MARE_BASH_START_…__` showed up and the END marker was missed (30 s stall, then a job).
#[cfg(unix)]
#[tokio::test]
async fn shell_tracing_never_leaks_the_persistent_shells_markers() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        for (on, off) in [("set -x", "set +x"), ("set -v", "set +v")] {
            call(&h, "bash", json!({ "command": on })).await;
            let t0 = std::time::Instant::now();
            let r = call(&h, "bash", json!({ "command": "echo traced-ok" })).await;
            let t = text(&r);
            assert!(!t.contains("__MARE_BASH"), "{on}: marker in the output:\n{t}");
            assert!(t.contains("traced-ok") && t.contains("[Command finished with exit code 0]"), "{on}: {t}");
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "{on}: the END marker was missed: {t}");
            call(&h, "bash", json!({ "command": off })).await;
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn command_tools_apply_tools_env_include_and_exclude() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("CMD_TOOL_KEEP", "kept");
        env.set_var("CMD_TOOL_DROP", "dropped");
        env.set_var("CMD_TOOL_API_KEY", "included-secret");
        let h = env
            .host()
            .builtins(&["jobs"])
            .config(json!({
                "plugins": {
                    "tools": {
                        "env": {
                            "include": ["CMD_TOOL_API_KEY"],
                            "exclude": ["CMD_TOOL_DROP"],
                        }
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let (content, is_error): (String, bool) = eval(
            &h,
            r#"
            T:command({
                name = "dump_env",
                description = "print env vars",
                readonly = true,
                command = { "env" },
            })
            local r = T:execute({ id = "cmd-env-1", name = "dump_env", arguments = {} }, {})
            return tostring(r.content), r.is_error == true
            "#,
        )
        .await;
        assert!(!is_error, "env command failed: {content}");
        assert!(content.contains("CMD_TOOL_KEEP=kept"), "{content}");
        assert!(content.contains("CMD_TOOL_API_KEY=included-secret"), "{content}");
        assert!(!content.contains("CMD_TOOL_DROP=dropped"), "{content}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn bash_validates_timeout_and_hides_jobs_when_jobs_disabled() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({
                "plugins": { "jobs": { "enabled": false } }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let (desc, has_bg_prop, err_bg, err_zero, err_huge): (String, bool, bool, bool, bool) = eval(
            &h,
            r#"
            local defs = T:definitions({ name = "m" })
            local bash_def
            for _, d in ipairs(defs) do
                if d.name == "bash" then bash_def = d break end
            end
            local has_bg = bash_def.parameters.properties.run_in_background ~= nil
            local r_bg = T:execute({ id = "b1", name = "bash", arguments = { command = "echo hi", run_in_background = true } }, {})
            local r_zero = T:execute({ id = "b2", name = "bash", arguments = { command = "echo hi", timeout_ms = 0 } }, {})
            local r_huge = T:execute({ id = "b3", name = "bash", arguments = { command = "echo hi", timeout_ms = 999999999 } }, {})
            return bash_def.description, has_bg, r_bg.is_error == true, r_zero.is_error == true, r_huge.is_error == true
            "#,
        )
        .await;
        assert!(!desc.contains("background job") && !desc.contains("run_in_background"), "{desc}");
        assert!(!has_bg_prop, "run_in_background property must be omitted when jobs service is absent");
        assert!(err_bg, "run_in_background=true must be rejected without jobs service");
        assert!(err_zero, "timeout_ms=0 must be rejected");
        assert!(err_huge, "timeout_ms above max must be rejected");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn command_parser_accepts_question_dot_and_non_ascii_names_but_ignores_paths() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (q_res, dot_res, cjk_res, path_handled, slash_handled): (String, String, String, bool, bool) = eval(
            &h,
            r#"
            local cmd = C.cordis:get("commands")
            local log = {}
            cmd:register("?", { description = "q", run = function(a) log.q = a end })
            cmd:register("foo.bar", { description = "d", run = function(a) log.dot = a end })
            cmd:register("帮助", { description = "cjk", run = function(a) log.cjk = a end })
            local h1 = cmd:run("/? topic")
            local h2 = cmd:run("/foo.bar arg1")
            local h3 = cmd:run("/帮助 测试")
            local h_path = pcall(cmd.run, cmd, "/usr/bin/foo")
            local h_slash = pcall(cmd.run, cmd, "//comment")
            assert(h1 and h2 and h3)
            return log.q, log.dot, log.cjk, h_path, h_slash
            "#,
        )
        .await;
        assert_eq!(q_res, "topic");
        assert_eq!(dot_res, "arg1");
        assert_eq!(cjk_res, "测试");
        assert!(!path_handled, "/usr/bin/foo must not be treated as a slash command");
        assert!(!slash_handled, "//comment must not be treated as a slash command");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn startup_spill_cleanup_only_removes_bash_pid_logs() {
    local(async {
        let env = TestEnv::new().await;
        let spill_dir = env.home.path().join("state/spill");
        std::fs::create_dir_all(&spill_dir).unwrap();
        let stale_bash = spill_dir.join("bash-999999-s1-20260101-000000-abc123.log");
        let other_prefix = spill_dir.join("other-999999-s1-20260101-000000-abc123.log");
        let wrong_ext = spill_dir.join("bash-999999-s1-20260101-000000-abc123.txt");
        std::fs::write(&stale_bash, "stale bash spill").unwrap();
        std::fs::write(&other_prefix, "other caller file").unwrap();
        std::fs::write(&wrong_ext, "non-log file").unwrap();

        let h = host(&env).await;
        wait_until(WAIT_SECS, "stale bash spill log removed", || {
            let p = stale_bash.clone();
            async move { !p.exists() }
        })
        .await;
        assert!(other_prefix.exists(), "startup cleanup must not delete non-bash prefix files");
        assert!(wrong_ext.exists(), "startup cleanup must not delete non-.log files");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_stall_defaults_to_30s_zero_disables_and_yes_no_prompt_hints() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().no_session();
        b.opts.load_init = true;
        let h = b.build().await;
        let default_stall: i64 =
            eval(&h, r#"return mare.config_describe({ "plugins.tools.stall_ms" })["plugins.tools.stall_ms"].default"#)
                .await;
        assert_eq!(default_stall, 30_000, "default stall_ms must be 30s");
        h.shutdown().await;
        drop(env);

        // stall_ms = 0 disables silence handoff so a quiet command completes in the foreground.
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "stall_ms": 0 } } }))
            .build()
            .await;
        expose(&h);
        let r = call(&h, "bash", json!({ "command": "sleep 0.5; echo finished-after-silence" })).await;
        let t = text(&r);
        assert!(t.contains("finished-after-silence") && !t.contains("background job"), "{t}");
        h.shutdown().await;
        drop(env);

        // A `(yes/no)` prompt followed by silence triggers the stall/input-wait hint.
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs"])
            .no_session()
            .config(json!({ "plugins": { "tools": { "stall_ms": 500 } } }))
            .build()
            .await;
        expose(&h);
        let r = call(&h, "bash", json!({ "command": "printf 'Continue (yes/no)? '; sleep 30" })).await;
        let t = text(&r);
        assert!(t.contains("no output for") && t.contains("background job"), "{t}");
        assert!(t.contains("waiting for input") && t.contains("stdin is empty"), "{t}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_strips_ansi_sequences_end_to_end() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "printf '\\033[31mred\\033[0m \\033]0;win\\007plain\\n'" })).await;
        let t = text(&r);
        assert!(t.contains("red plain"), "{t:?}");
        assert!(!t.contains('\x1b'), "ANSI escapes must be stripped before reaching the model: {t:?}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_startup_cleans_dead_pid_spill_files_and_keeps_live_ones() {
    local(async {
        let env = TestEnv::new().await;
        let spill_dir = env.home.path().join("state/spill");
        std::fs::create_dir_all(&spill_dir).unwrap();
        // Spawn and reap a short child to guarantee a dead PID that cannot be alive.
        let mut dead_child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead_child.id();
        let _ = dead_child.wait();
        let dead_file = spill_dir.join(format!("bash-{dead_pid}-s1-20260101-000000-dead01.log"));
        let live_file = spill_dir.join(format!("bash-{}-20260101-000000-live01.log", std::process::id()));
        std::fs::write(&dead_file, "stale output\n").unwrap();
        std::fs::write(&live_file, "live output\n").unwrap();

        let h = host(&env).await;
        wait_until(WAIT_SECS, "startup spill cleanup to remove dead PID file", || {
            let dead = dead_file.clone();
            async move { !dead.exists() }
        })
        .await;
        assert!(live_file.exists(), "live PID spill file must not be removed");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_persistent_shell_reports_signal_kill_and_fresh_mode_isolates_state() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": "kill -9 $$" })).await;
        let t = text(&r);
        assert!(t.contains("[shell killed by signal:") && t.contains("was reset"), "{t}");
        let r2 = call(&h, "bash", json!({ "command": "echo recovered-ok" })).await;
        assert!(text(&r2).contains("recovered-ok"), "{r2}");
        h.shutdown().await;
        drop(env);

        // Fresh mode runs each bash call in a clean shell with isolated env and cwd.
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("sub")).unwrap();
        let h = env
            .host()
            .no_session()
            .config(json!({ "plugins": { "tools": { "shell_session": "fresh" } } }))
            .build()
            .await;
        expose(&h);
        let r1 = call(&h, "bash", json!({ "command": "cd sub && export FRESH_ONLY=leaked && pwd -P" })).await;
        assert!(text(&r1).contains("/sub"), "{r1}");
        let r2 = call(&h, "bash", json!({ "command": "echo var=${FRESH_ONLY:-isolated}; pwd -P" })).await;
        let t2 = text(&r2);
        assert!(t2.contains("var=isolated") && !t2.contains("/sub"), "fresh mode must isolate state: {t2}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn bash_and_file_guard_handle_timeout_background_children_and_stale_edits() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("target.txt"), "hello world\n").unwrap();
        std::fs::write(env.cwd.join("mid_race.txt"), "before edit\n").unwrap();
        let h = env
            .host()
            .builtins(&["jobs"])
            .config(json!({ "plugins": { "tools": { "exclude": ["grep"] } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "CWD_PATH", env.cwd.display().to_string());
        let ok: bool = eval(
            &h,
            r#"
            -- 1. Unknown, excluded, unadmitted, undeclared, and dynamic-schema tools are refused by pipeline_guard.
            local unk = T:execute({ id = "u1", name = "no_such_tool_xyz", arguments = {} }, {})
            assert(unk.is_error and tostring(unk.content):find("Unknown tool", 1, true), tostring(unk.content))
            local excl = T:execute({ id = "u2", name = "grep", arguments = { pattern = "a" } }, {})
            assert(excl.is_error and tostring(excl.content):find("Unknown tool", 1, true), tostring(excl.content))
            local undecl = T:execute({ id = "u3", name = "read", arguments = { path = "target.txt" } }, { declarations = {} })
            assert(undecl.is_error and tostring(undecl.content):find("not declared in this request", 1, true), tostring(undecl.content))

            -- Cancelled tool call waiting on a held file lock covers pipeline_guard's lock-cancel path.
            local hold_tx, hold_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:spawn(function()
                T:with_file_lock(CWD_PATH .. "/target.txt", function()
                    hold_tx:send(true)
                    release_rx:recv()
                end)
            end)
            assert(hold_rx:recv() == true)
            local cancel_lk = mare.task.token()
            cancel_lk:cancel()
            local lk_res = T:execute(
                { id = "u5", name = "edit", arguments = { path = "target.txt", old_string = "hello", new_string = "bye" } },
                { cancel = cancel_lk }
            )
            release_tx:send(true)
            assert(lk_res.is_error and lk_res.cancelled == true, tostring(lk_res.content))

            -- 1b. Approval callback with source="user" (without human evidence) and unavailable=true covers pipeline_guard L557 + L576-579.
            local off_pv = C.cordis:get("policy"):voter("tool/before", function(call)
                if call and call.id == "pv1" then
                    return { decision = "ask", reason = "test ask" }
                end
            end)
            local pv1 = T:execute(
                { id = "pv1", name = "read", arguments = { path = "target.txt" } },
                { approve = function() return { decision = "deny", source = "user", unavailable = true, reason = "approval unavailable" } end }
            )
            off_pv()
            assert(pv1.is_error, tostring(pv1.content))

            -- 2. file_guard reports missing file, stale external edit, mid-edit race, and unread write.
            local miss = T:execute({ id = "fg1", name = "edit", arguments = { path = "missing.txt", old_string = "a", new_string = "b" } }, {})
            assert(miss.is_error and tostring(miss.content):lower():find("file not found", 1, true), tostring(miss.content))

            local rd = T:execute({ id = "fg2", name = "read", arguments = { path = "target.txt" } }, {})
            assert(not rd.is_error, tostring(rd.content))
            assert(mare.fs.write(CWD_PATH .. "/target.txt", "changed externally\n"))
            local stale = T:execute({ id = "fg3", name = "edit", arguments = { path = "target.txt", old_string = "hello", new_string = "bye" } }, {})
            assert(stale.is_error and tostring(stale.content):find("stale", 1, true), tostring(stale.content))

            -- Mutate the file during fs/edit-intent to trigger commit-time stale check in file_guard.
            local rd_mid = T:execute({ id = "fg_mid_rd", name = "read", arguments = { path = "mid_race.txt" } }, {})
            assert(not rd_mid.is_error, tostring(rd_mid.content))
            local mutated = false
            local off_mid = C.cordis:get("policy"):voter("fs/edit-intent", function()
                if not mutated then
                    mutated = true
                    mare.fs.write(CWD_PATH .. "/mid_race.txt", "changed mid-edit!\n")
                end
                return { decision = "allow" }
            end)
            local mid_res = T:execute({ id = "fg_mid_ed", name = "edit", arguments = { path = "mid_race.txt", old_string = "before", new_string = "after" } }, {})
            off_mid()
            assert(mid_res.is_error and tostring(mid_res.content):find("changed while editing", 1, true), tostring(mid_res.content))

            local miss_rd = T:execute({ id = "fg4", name = "read", arguments = { path = "race.txt" } }, {})
            assert(miss_rd.is_error)
            assert(mare.fs.write(CWD_PATH .. "/race.txt", "created by someone else\n"))
            local raced = T:execute({ id = "fg5", name = "write", arguments = { path = "race.txt", content = "overwrite" } }, {})
            assert(raced.is_error and tostring(raced.content):find("read it first", 1, true), tostring(raced.content))

            -- 3. bash invalid timeout_ms and timeout move-to-background + job_output/wait/job_kill on persistent shell.
            local bad_to = T:execute({ id = "b_bad_to", name = "bash", arguments = { command = "echo hi", timeout_ms = 999999999 } }, {})
            assert(bad_to.is_error and tostring(bad_to.content):find("timeout_ms", 1, true), tostring(bad_to.content))

            local timed = T:execute({ id = "b_to", name = "bash", arguments = { command = "echo slow-start; sleep 5", timeout_ms = 80 } }, {})
            local timed_text = tostring(timed.content)
            assert(timed_text:find("moved to background job", 1, true), timed_text)
            local timed_job = (timed.details and timed.details.job_id) or timed_text:match("background job ([%w_%-]+)")
            assert(timed_job, "expected background job id in: " .. timed_text)
            local jout = T:execute({ id = "b_to_out", name = "job_output", arguments = { job_id = timed_job, offset = 0, max_bytes = 128 } }, {})
            assert(not jout.is_error, tostring(jout.content))
            local jsvc = C.cordis:get("jobs")
            if jsvc and jsvc.wait then
                pcall(jsvc.wait, jsvc, timed_job, { timeout_ms = 25 })
                local wc = mare.task.token()
                wc:cancel()
                pcall(jsvc.wait, jsvc, timed_job, { cancel = wc })
            end
            T:execute({ id = "b_to_kill", name = "job_kill", arguments = { job_id = timed_job } }, {})

            -- 4. bash abort via cancelled token.
            local cancel = mare.task.token()
            cancel:cancel()
            local ab = T:execute({ id = "b_ab", name = "bash", arguments = { command = "echo never" } }, { cancel = cancel })
            assert(ab.is_error or tostring(ab.content):find("aborted", 1, true) or tostring(ab.content):find("cancelled", 1, true), tostring(ab.content))

            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
        drop(env);

        // 5. apply_patch with move under active permissions exercises exec_ctx write checks in text/edit.rs.
        let env_p = TestEnv::new().await;
        std::fs::write(env_p.cwd.join("src_move.txt"), "old line\n").unwrap();
        let h_p = env_p
            .host()
            .builtins(&["tools-apply-patch"])
            .config(json!({ "plugins": { "tools": { "edit_mode": "apply_patch" } } }))
            .no_session()
            .build()
            .await;
        expose(&h_p);
        let ok_p: bool = eval(
            &h_p,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write" })
            local rd = T:execute({ id = "ap_rd", name = "read", arguments = { path = "src_move.txt" } }, { sandbox = { level = "read-only" } })
            assert(not rd.is_error, tostring(rd.content))
            local patch = "*** Begin Patch\n*** Update File: src_move.txt\n*** Move to: dst_move.txt\n@@\n-old line\n+new line\n*** End Patch"
            local ap = T:execute({ id = "ap_mv", name = "apply_patch", arguments = { input = patch } }, {})
            assert(not ap.is_error, tostring(ap.content))
            return true
            "#,
        )
        .await;
        assert!(ok_p);
        assert!(env_p.cwd.join("dst_move.txt").exists());
        h_p.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn fresh_bash_reports_timeout_abort_and_surviving_background_child() {
    local(async {
        // 1. Direct fresh-mode execution (jobs disabled) covers timeout, abort, and untracked background-child notes.
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({
                "plugins": {
                    "jobs": { "enabled": false },
                    "tools": { "shell_session": "fresh" }
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local bad_to = T:execute({ id = "fb0", name = "bash", arguments = { command = "echo hi", timeout_ms = 999999999 } }, {})
            assert(bad_to.is_error and tostring(bad_to.content):find("timeout_ms", 1, true), tostring(bad_to.content))

            local no_bg = T:execute({ id = "fb0b", name = "bash", arguments = { command = "echo hi", run_in_background = true } }, {})
            assert(no_bg.is_error and tostring(no_bg.content):find("jobs service is not loaded", 1, true), tostring(no_bg.content))

            local to = T:execute({ id = "fb1", name = "bash", arguments = { command = "sleep 5", timeout_ms = 80 } }, {})
            assert(tostring(to.content):find("command timed out", 1, true), tostring(to.content))

            -- Mid-command cancellation exercises the [command aborted] note in bash_run.lua.
            local cancel = mare.task.token()
            C.cordis:spawn(function()
                mare.task.sleep(40)
                cancel:cancel()
            end)
            local ab = T:execute({ id = "fb2", name = "bash", arguments = { command = "sleep 5", timeout_ms = 5000 } }, { cancel = cancel })
            assert(tostring(ab.content):find("aborted", 1, true) or ab.is_error, tostring(ab.content))

            local bg_no_jobs = T:execute({
                id = "fb2b",
                name = "bash",
                arguments = { command = "sleep 15 & echo fresh-bg-nojobs; exit", timeout_ms = 5000 },
            }, {})
            assert(tostring(bg_no_jobs.content):find("background processes it started are still running", 1, true), tostring(bg_no_jobs.content))
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
        drop(env);

        // 2. Fresh-mode with jobs enabled tracks surviving background child.
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host()
            .builtins(&["jobs"])
            .config(json!({ "plugins": { "tools": { "shell_session": "fresh" } } }))
            .no_session()
            .build()
            .await;
        expose(&h2);
        global(&h2, "J", h2.service_table("jobs").unwrap());
        let ok2: bool = eval(
            &h2,
            r#"
            local bg = T:execute({
                id = "fb3",
                name = "bash",
                arguments = { command = "sleep 30 & echo fresh-bg; exit", timeout_ms = 5000 },
            }, {})
            local bg_text = tostring(bg.content)
            assert(bg_text:find("background processes it started are still running", 1, true), bg_text)
            local jid = bg.details and bg.details.job_id
            if jid then
                pcall(J.cancel, J, jid, "test cleanup")
            end
            return true
            "#,
        )
        .await;
        assert!(ok2);
        h2.shutdown().await;
        drop(env2);

        // 3. Persistent shell covers cwd-record failure warning (bash_run.lua) and shell start/spawn failures (bash_persistent.lua).
        let env3 = TestEnv::new().await;
        std::fs::create_dir_all(env3.cwd.join("sub_cwd")).unwrap();
        let h3 = env3.host().build().await;
        expose(&h3);
        let ok3: bool = eval(
            &h3,
            r#"
            -- 3a. Persistent bash cd with failing S.record covers bash_run's working-directory warning path.
            mare.ui.notify = function() end
            local orig_rec = S.record
            S.record = function() return nil, "simulated record failure" end
            local cd_res = T:execute({ id = "ps_cd", name = "bash", arguments = { command = "cd sub_cwd && pwd" } }, { on_state = function() return true end })
            S.record = orig_rec
            assert(not cd_res.is_error, tostring(cd_res.content))

            -- 3b. Spawn failure on fresh start covers bash_persistent.lua L63 + L466, and shell_config error covers L37.
            local orig_start = mare.proc.start
            mare.proc.start = function() error("simulated spawn error") end
            local r_spawn = T:execute({ id = "ps_sp", name = "bash", arguments = { command = "echo hi" } }, { on_state = function() return true end, sandbox = { level = "read-only" } })
            mare.proc.start = orig_start
            assert(r_spawn.is_error and tostring(r_spawn.content):find("Could not start", 1, true), tostring(r_spawn.content))
            return true
            "#,
        )
        .await;
        assert!(ok3);
        h3.shutdown().await;
        drop(env3);

        // 4. Missing shell_config error on first bash call covers bash_persistent.lua L37.
        let env4 = TestEnv::new().await;
        let h4 = env4.host().no_session().build().await;
        expose(&h4);
        let ok4: bool = eval(
            &h4,
            r#"
            local orig_sh = mare.proc.shell
            mare.proc.shell = function() return nil, "no usable shell" end
            local r_sh = T:execute({ id = "ps_nosh", name = "bash", arguments = { command = "echo hi" } }, {})
            mare.proc.shell = orig_sh
            assert(r_sh.is_error and tostring(r_sh.content):find("Could not start", 1, true), tostring(r_sh.content))
            return true
            "#,
        )
        .await;
        assert!(ok4);
        h4.shutdown().await;
    })
    .await
}

/// Output containing `"PTY_POLICY"` from a command that exits or aborts the persistent shell must
/// never trigger the one-shot fallback runner (which would execute non-idempotent commands twice).
/// Only a structured startup refusal (`policy_refused = true`) falls back to one-shot execution.
#[cfg(unix)]
#[tokio::test]
async fn persistent_shell_fallback_uses_structured_policy_refusal_not_output_text() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let counter = env.cwd.join("runs.log");

        // 1. Command prints `PTY_POLICY`, appends a line, and exits the persistent shell -> runs ONCE.
        let r = call(&h, "bash", json!({ "command": "echo PTY_POLICY; echo ran >> runs.log; exit 0" })).await;
        let content = text(&r);
        assert!(content.contains("PTY_POLICY"), "expected command output preserved: {content}");
        assert!(content.contains("[shell exited: code 0]"), "expected shell exit note: {content}");
        let lines: Vec<String> = std::fs::read_to_string(&counter).unwrap().lines().map(ToOwned::to_owned).collect();
        assert_eq!(lines.len(), 1, "command printing PTY_POLICY and exiting must not run twice: {lines:?}");

        // 2. Command prints `PTY_POLICY` and is interrupted by cancel -> also runs ONCE (no one-shot replay).
        let abort_counter = env.cwd.join("abort_runs.log");
        let abort_started = env.cwd.join("abort_started");
        let abort_cmd =
            "echo PTY_POLICY; echo ran >> abort_runs.log; touch abort_started; while :; do sleep 0.05; done";
        global(
            &h,
            "ABORT_CALL",
            mare_shared::lua::json_to_lua(
                h.lua(),
                &json!({ "id": "pty_abort", "name": "bash", "arguments": { "command": abort_cmd } }),
            )
            .unwrap(),
        );
        exec(
            &h,
            r#"ABORT_TOK = mare.task.token(); ABORT_RES = nil
               mare.task.spawn(function() ABORT_RES = T:execute(ABORT_CALL, { cancel = ABORT_TOK }) end)"#,
        )
        .await;
        wait_until(WAIT_SECS, "abort command started", || {
            let p = abort_started.clone();
            async move { p.exists() }
        })
        .await;
        exec(&h, "ABORT_TOK:cancel()").await;
        wait_until(WAIT_SECS, "abort command finished", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return ABORT_RES ~= nil").await }
        })
        .await;
        let abort_lines: Vec<String> =
            std::fs::read_to_string(&abort_counter).unwrap().lines().map(ToOwned::to_owned).collect();
        assert_eq!(abort_lines.len(), 1, "interrupted command printing PTY_POLICY must not replay: {abort_lines:?}");

        // 3. Genuine `PTY_POLICY` startup refusal from `mare.proc.start` still falls back to one-shot runner.
        let ok_fallback: bool = eval(
            &h,
            r#"
            local orig_start = mare.proc.start
            local start_calls = 0
            mare.proc.start = function(opts)
                start_calls = start_calls + 1
                if opts and opts.on_policy_change == "stop" then
                    error("PTY_POLICY: per-call elevated terminals are not persistent authority")
                end
                return orig_start(opts)
            end
            local res = T:execute({ id = "pty_fb", name = "bash", arguments = { command = "echo fallback-ok" } }, {})
            mare.proc.start = orig_start
            assert(start_calls >= 1, "expected persistent start attempt")
            assert(not res.is_error and tostring(res.content):find("fallback-ok", 1, true), tostring(res.content))
            return true
            "#,
        )
        .await;
        assert!(ok_fallback);

        h.shutdown().await;
    })
    .await
}

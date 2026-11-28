//! Process-level Lua plugin regression tests migrated from `scripts/test_lua_regressions.py`.
//! Each test drives a real `mare --rpc` child process through `initialize` and `shutdown`.

use std::io::{BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{ChildGroup, RpcClient, mare_cmd};

struct ChildGuard(Option<ChildGroup>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

fn spawn_drain<R: Read + Send + 'static>(mut reader: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

fn wait_child_with_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("try_wait failed: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("child process did not exit within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_mare_app_ready(plugin_code: &str, faux_script: Option<Value>) -> Value {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(cwd.join(".mare/plugins")).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let out_path = tmp.path().join("result.json");
    let out_escaped = out_path.to_str().unwrap().replace('\\', "\\\\");
    let plugin_code = plugin_code.replace("__OUT_PATH__", &out_escaped);
    std::fs::write(cwd.join(".mare/plugins/repro.lua"), plugin_code).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--trust", "-m", "faux/echo", "-C", cwd.to_str().unwrap()])
        .env("HOME", &home)
        .env("MARE_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(script) = faux_script {
        let script_path = tmp.path().join("faux.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        cmd.env("MARE_FAUX_SCRIPT", &script_path);
    }

    let mut child = ChildGuard(Some(ChildGroup::spawn(&mut cmd).unwrap()));
    let inner = &mut child.0.as_mut().unwrap().0;
    let stderr_rx = spawn_drain(inner.stderr.take().unwrap());
    let mut rpc = RpcClient::new(BufReader::new(inner.stdout.take().unwrap()), inner.stdin.take().unwrap());

    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    assert!(init.get("result").is_some(), "initialize failed: {init}");

    let shutdown = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    assert!(shutdown.get("error").is_none(), "shutdown failed: {shutdown}");

    let stdout_reader = rpc.into_reader().unwrap();
    let _stdout_rx = spawn_drain(stdout_reader);

    let status = wait_child_with_timeout(&mut child.0.as_mut().unwrap().0, Duration::from_secs(15));
    let stderr = stderr_rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let status = match status {
        Ok(status) => {
            child.0.take();
            status
        }
        Err(err) => panic!("mare --rpc {err}; stderr={stderr}"),
    };
    assert!(status.success(), "mare --rpc exited with {:?}; stderr={stderr}", status.code());
    assert!(out_path.exists(), "missing output file {}; stderr={stderr}", out_path.display());

    let raw = std::fs::read_to_string(&out_path).unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn wait_child_timeout_kills_and_reaps_hanging_process() {
    if std::env::var_os("MARE_TEST_SLEEP_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", "suite::lua_regressions::wait_child_timeout_kills_and_reaps_hanging_process", "--nocapture"])
        .env("MARE_TEST_SLEEP_CHILD", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let start = Instant::now();
    let err = wait_child_with_timeout(&mut child.0, Duration::from_millis(100)).unwrap_err();
    assert!(err.contains("did not exit within"), "unexpected error: {err}");
    assert!(start.elapsed() < Duration::from_secs(2), "timeout took too long: {:?}", start.elapsed());
    assert!(child.0.try_wait().unwrap().is_some(), "child should be reaped after timeout");
}

#[test]
fn lz120_subagent_utf8_cut() {
    let plugin = r#"
return {
	name = "repro",
	inject = { "agent", "tools" },
	apply = function(ctx)
		-- Subagents inherit parent workspace trust (#73 / #30) and load `.mare/plugins/*.lua`;
		-- keep this harness plugin on the parent VM so the child does not recursively spawn itself.
		if mare.mode == "child" then
			return
		end
		local tx, rx = mare.task.channel()
		ctx:on("item/completed", function(ev)
			local m = ev and ev.item and ev.item.message
			local txt = m and m.content and m.content[1] and m.content[1].text
			if type(txt) == "string" and txt:find("[subagent ", 1, true) then
				tx:send(txt)
			end
		end)
		ctx:on("app/ready", function()
			local started = ctx.agent:prompt("run subagent", {})
			local r = ctx.agent:wait(started.turn_id)
			assert(not r.error, tostring(r.error))
			local msg = rx:recv()
			local ok = utf8.len(msg) ~= nil
			mare.fs.write("__OUT_PATH__", mare.json.encode({
				ok = ok,
				has_trunc = msg:find("[truncated]", 1, true) ~= nil,
			}))
		end)
	end,
}
"#;
    let long_utf8 = format!("{}中z", "a".repeat(4095));
    let faux = json!([
        {
            "tool_calls": [
                {
                    "id": "call_sub_1",
                    "name": "subagent",
                    "arguments": { "prompt": "go", "wait": false }
                }
            ]
        },
        { "text": long_utf8 },
        { "text": "parent-done" },
        { "text": "after-notify" }
    ]);
    let res = run_mare_app_ready(plugin, Some(faux));
    assert_eq!(res["ok"], json!(true), "{res}");
    assert_eq!(res["has_trunc"], json!(true), "{res}");
}

#[test]
fn lz133_archive_rollback_on_write_failure() {
    let plugin = r#"
return {
	name = "repro",
	inject = { "session", "agent", "commands" },
	apply = function(ctx)
		ctx:on("app/ready", function()
			local notices = {}
			mare.ui.notify = function(msg) notices[#notices + 1] = tostring(msg) end
			local orig_open = mare.store.open
			mare.store.open = function(path, opts)
				local f = orig_open(path, opts)
				if not opts or (not opts.auxiliary and not opts.create_new and not opts.existing) then
					return setmetatable({}, {
						__index = function(_, k)
							if k == "append" then
								return function(_, entry, append_opts)
									if type(entry) == "table" and entry.type == "archive" then
										return nil, "injected archive write failure"
									end
									return f:append(entry, append_opts)
								end
							end
							local v = f[k]
							if type(v) == "function" then
								return function(_, ...)
									return v(f, ...)
								end
							end
							return v
						end,
					})
				end
				return f
			end
			local started = ctx.agent:prompt("hello before archive", {})
			local p = ctx.agent:wait(started.turn_id)
			mare.store.open = orig_open
			assert(not p.error, tostring(p.error))
			local before = ctx.session:info()
			ctx.commands:run("/archive")
			local after = ctx.session:info()
			local last_notice = notices[#notices] or ""
			mare.fs.write("__OUT_PATH__", mare.json.encode({
				res_nil = last_notice:find("Not archived:", 1, true) ~= nil,
				err = last_notice,
				same_id = after.session_id == before.session_id,
				same_path = after.path == before.path,
			}))
		end)
	end,
}
"#;
    let res = run_mare_app_ready(plugin, None);
    assert_eq!(res["res_nil"], json!(true), "{res}");
    assert_eq!(res["same_id"], json!(true), "{res}");
    assert_eq!(res["same_path"], json!(true), "{res}");
}

#[test]
fn lz161_archive_rollback_bypasses_before_switch_policy() {
    let plugin = r#"
return {
	name = "repro",
	inject = { "session", "agent", "policy", "commands" },
	apply = function(ctx)
		ctx:on("app/ready", function()
			local notices = {}
			mare.ui.notify = function(msg) notices[#notices + 1] = tostring(msg) end
			local orig_open = mare.store.open
			mare.store.open = function(path, opts)
				local f = orig_open(path, opts)
				if not opts or (not opts.auxiliary and not opts.create_new and not opts.existing) then
					return setmetatable({}, {
						__index = function(_, k)
							if k == "append" then
								return function(_, entry, append_opts)
									if type(entry) == "table" and entry.type == "archive" then
										return nil, "injected archive write failure"
									end
									return f:append(entry, append_opts)
								end
							end
							local v = f[k]
							if type(v) == "function" then
								return function(_, ...)
									return v(f, ...)
								end
							end
							return v
						end,
					})
				end
				return f
			end
			local started = ctx.agent:prompt("hello before archive", {})
			local p = ctx.agent:wait(started.turn_id)
			mare.store.open = orig_open
			assert(not p.error, tostring(p.error))
			local before = ctx.session:info()
			ctx.policy:voter("session/before-switch", function(ev)
				if ev and ev.reason == "resume" then
					return { decision = "deny", reason = "policy blocks resume" }
				end
				return { decision = "allow" }
			end)
			ctx.commands:run("/archive")
			local after = ctx.session:info()
			local err = notices[#notices] or ""
			mare.fs.write("__OUT_PATH__", mare.json.encode({
				res_nil = err:find("Not archived:", 1, true) ~= nil,
				err = err,
				no_rollback_err = not err:find("rollback failed", 1, true),
				same_id = after.session_id == before.session_id,
				same_path = after.path == before.path,
			}))
		end)
	end,
}
"#;
    let res = run_mare_app_ready(plugin, None);
    assert_eq!(res["res_nil"], json!(true), "{res}");
    assert_eq!(res["no_rollback_err"], json!(true), "{res}");
    assert_eq!(res["same_id"], json!(true), "{res}");
    assert_eq!(res["same_path"], json!(true), "{res}");
}

#[test]
fn lz57_handoff_recovery_tool_call_persisted_before_execution() {
    let plugin = r#"
return {
	name = "repro",
	inject = { "session", "agent" },
	apply = function(ctx)
		ctx:on("app/ready", function()
			-- `mare --rpc` attaches the multi-runtime orchestrator before `app/ready`, so
			-- in-VM switch-and-reload recovery exercises `session:switch` directly.
			ctx.session:switch({ kind = "new" })
			local info = ctx.session:info()
			local tid = "tr-lz57"
			local turn_id = "transition:" .. tid
			assert(ctx.session:add_record("session/transition-initial", {
				transition_id = tid,
				turn_id = turn_id,
				mode = "new",
				status = "pending",
				baseline_messages = 0,
				target = { session_id = info.session_id, session_epoch = info.session_epoch },
				initial = { kind = "input", content = "run handoff", source = "handoff" },
			}, { sync = true }))
			local path = ctx.session:info().path
			ctx.session:switch({ kind = "new" })
			local f = mare.store.open(path, { existing = true, auxiliary = true })
			f:append({
				type = "message",
				entry_id = "m_u1",
				transition_id = tid,
				message = { role = "user", content = { { type = "text", text = "run handoff" } }, timestamp = mare.task.now() },
			}, { sync = true })
			f:append({
				type = "message",
				entry_id = "m_a1",
				parent_entry_id = "m_u1",
				message = {
					role = "assistant",
					content = { { type = "tool_call", id = "call_lz57", name = "echo_tool", arguments = {} } },
					timestamp = mare.task.now(),
				},
			}, { sync = true })
			f:append({ type = "state", state = "closed", created_at_ms = math.floor(mare.task.now() * 1000) }, { sync = true })
			f:close()

			local tx, rx = mare.task.channel()
			local ev_got
			ctx:on("session/transition-completed", function(ev)
				ev_got = { kind = "finished", ev = ev }
				tx:send(true)
			end)
			ctx:on("session/transition-failed", function(ev)
				ev_got = { kind = "failed", ev = ev }
				tx:send(true)
			end)
			assert(ctx.session:switch({ kind = "load", path = path }))
			rx:recv()
			local recs = ctx.session:list_records("session/transition-initial").data
			local all_msgs = ctx.agent:list_messages().data
			local msgs = {}
			for _, m in ipairs(all_msgs) do
				if not m.context_disclosure then
					table.insert(msgs, m)
				end
			end
			mare.fs.write("__OUT_PATH__", mare.json.encode({
				ev_kind = ev_got.kind,
				initial_status = ev_got.ev and ev_got.ev.initial_status,
				last_status = recs[#recs] and recs[#recs].status,
				msg_count = #msgs,
				last_role = msgs[#msgs] and msgs[#msgs].role,
				last_text = msgs[#msgs] and msgs[#msgs].content and msgs[#msgs].content[1] and msgs[#msgs].content[1].text,
			}))
		end)
	end,
}
"#;
    let faux = json!([{ "text": "resumed-after-tool-call" }]);
    let res = run_mare_app_ready(plugin, Some(faux));
    assert_eq!(res["ev_kind"], json!("finished"), "{res}");
    assert_eq!(res["last_status"], json!("settled"), "{res}");
    assert_eq!(res["msg_count"], json!(4), "{res}");
    assert_eq!(res["last_text"], json!("resumed-after-tool-call"), "{res}");
}

#[test]
fn lz57_handoff_recovery_interrupted_assistant_not_settled() {
    let plugin = r#"
return {
	name = "repro",
	inject = { "session", "agent" },
	apply = function(ctx)
		ctx:on("app/ready", function()
			-- `mare --rpc` attaches the multi-runtime orchestrator before `app/ready`, so
			-- in-VM switch-and-reload recovery exercises `session:switch` directly.
			ctx.session:switch({ kind = "new" })
			local info = ctx.session:info()
			local tid = "tr-lz57-int"
			local turn_id = "transition:" .. tid
			assert(ctx.session:add_record("session/transition-initial", {
				transition_id = tid,
				turn_id = turn_id,
				mode = "new",
				status = "pending",
				baseline_messages = 0,
				target = { session_id = info.session_id, session_epoch = info.session_epoch },
				initial = { kind = "input", content = "run handoff", source = "handoff" },
			}, { sync = true }))
			local path = ctx.session:info().path
			ctx.session:switch({ kind = "new" })
			local f = mare.store.open(path, { existing = true, auxiliary = true })
			f:append({
				type = "message",
				entry_id = "m_u1",
				transition_id = tid,
				message = { role = "user", content = { { type = "text", text = "run handoff" } }, timestamp = mare.task.now() },
			}, { sync = true })
			f:append({
				type = "message",
				entry_id = "m_a1",
				parent_entry_id = "m_u1",
				message = {
					role = "assistant",
					interrupted = true,
					content = { { type = "text", text = "partial" } },
					timestamp = mare.task.now(),
				},
			}, { sync = true })
			f:append({ type = "state", state = "closed", created_at_ms = math.floor(mare.task.now() * 1000) }, { sync = true })
			f:close()

			local tx, rx = mare.task.channel()
			local ev_got
			ctx:on("session/transition-completed", function(ev)
				ev_got = { kind = "finished", ev = ev }
				tx:send(true)
			end)
			ctx:on("session/transition-failed", function(ev)
				ev_got = { kind = "failed", ev = ev }
				tx:send(true)
			end)
			assert(ctx.session:switch({ kind = "load", path = path }))
			rx:recv()
			local recs = ctx.session:list_records("session/transition-initial").data
			mare.fs.write("__OUT_PATH__", mare.json.encode({
				ev_kind = ev_got.kind,
				last_status = recs[#recs] and recs[#recs].status,
			}))
		end)
	end,
}
"#;
    let res = run_mare_app_ready(plugin, None);
    assert_eq!(res["ev_kind"], json!("failed"), "{res}");
    assert_eq!(res["last_status"], json!("failed"), "{res}");
}

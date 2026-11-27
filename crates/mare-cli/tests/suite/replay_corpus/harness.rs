use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::common::cassette::{Cassette, MatchMode};
use crate::common::*;
use serde_json::Value;

pub(super) fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cassettes/mare-record").join(name)
}

pub(super) fn publish_recorded_cassette(cassette: &Path, target: &Path) -> Result<(), String> {
    let bytes = std::fs::read(cassette).map_err(|_| "failed to read recorded cassette".to_string())?;
    validate_recorded_cassette(&bytes)?;
    let sanitized = sanitize_cassette_host_paths(&bytes);
    let staged = target.with_extension("json.new");
    std::fs::write(&staged, sanitized).map_err(|_| "failed to stage recorded cassette".to_string())?;
    std::fs::rename(&staged, target).map_err(|_| "failed to replace recorded cassette".to_string())
}

fn sanitize_cassette_host_paths(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut text) = String::from_utf8(bytes.to_vec()) else {
        return bytes.to_vec();
    };
    let cwd_re = regex::Regex::new(r"<cwd>\\n([^\n\\]+)\\n</cwd>").expect("valid cwd regex");
    let cwds: Vec<String> = cwd_re
        .captures_iter(&text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .filter(|cwd| cwd != "/tmp/mare-record/workspace")
        .collect();
    let sess_re =
        regex::Regex::new(r#"[^"\\\s]+/state/sessions/--[^"\\/]+--/([^"\\\s]+)"#).expect("valid session regex");
    text = sess_re
        .replace_all(&text, "/tmp/mare-record/home/state/sessions/--tmp-mare-record-workspace--/$1")
        .into_owned();
    for cwd in cwds {
        if !cwd.is_empty() {
            text = text.replace(&cwd, "/tmp/mare-record/workspace");
        }
    }
    text.into_bytes()
}

pub(super) fn validate_recorded_cassette(bytes: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "recorded cassette is not valid JSON".to_string())?;
    let interactions = value["interactions"]
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(|| "recorded cassette has no interactions".to_string())?;
    for (index, interaction) in interactions.iter().enumerate() {
        if !interaction.get("request").is_some_and(Value::is_object) {
            return Err(format!("recorded interaction {index} has no request object"));
        }
    }
    let phases =
        value["metadata"]["phases"].as_array().ok_or_else(|| "recorded cassette has no phase list".to_string())?;
    if phases.len() != interactions.len() || phases.iter().any(|phase| !phase.is_string()) {
        return Err("recorded cassette must have one phase per interaction".to_string());
    }
    for required in ["turn_1", "compact", "turn_2"] {
        if !phases.iter().any(|phase| phase.as_str() == Some(required)) {
            return Err(format!("recorded cassette is missing phase {required}"));
        }
    }
    let cassette: Cassette =
        serde_json::from_value(value).map_err(|_| "recorded cassette has invalid structure".to_string())?;
    if cassette.version != 1 || cassette.interactions.iter().any(|it| it.transport != "http") {
        return Err("recorded cassette has an unsupported version or transport".to_string());
    }
    if cassette.mode() != MatchMode::RequestAndResponse {
        return Err("recorded cassette requires a request for every interaction".to_string());
    }
    Ok(())
}

pub(super) async fn run_recording_child(mut command: tokio::process::Command) -> Result<(), String> {
    let status = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|_| "failed to start recording child".to_string())?;
    if status.success() { Ok(()) } else { Err(format!("recording child failed ({status}); diagnostics suppressed")) }
}

/// The recorded interactions of one session phase, as `MockServer` responses.
pub(super) fn phase(c: &Cassette, name: &str) -> Vec<Resp> {
    let phases = c.metadata["phases"].as_array().expect("metadata.phases");
    assert_eq!(phases.len(), c.interactions.len(), "cassette: one phase per interaction");
    let out: Vec<Resp> = c
        .interactions
        .iter()
        .zip(phases)
        .filter(|(_, p)| p.as_str() == Some(name))
        .map(|(it, _)| it.response.to_resp())
        .collect();
    assert!(!out.is_empty(), "cassette: no interactions for requested phase");
    out
}

/// Finds a byte offset inside `s` that lands in the middle of a multi-byte UTF-8 sequence
/// (specifically inside a Chinese character) so `MockServer` splits the SSE stream across a
/// UTF-8 codepoint boundary.
pub(super) fn mid_utf8_split_offset(s: &str) -> Vec<usize> {
    for (idx, ch) in s.char_indices() {
        if !ch.is_ascii() && ch.len_utf8() >= 3 && idx > 0 {
            return vec![idx + 1, idx + 2];
        }
    }
    let mid = s.len() / 2;
    if mid > 0 { vec![mid] } else { vec![] }
}

/// Prepares a deterministic workspace with a >5KB file containing multi-byte Chinese text.
pub(super) fn prepare_workspace(dir: &Path) {
    let mut content = String::from("中文跨字节边界测试：你好世界，协议回放验证。\n");
    for i in 1..=140 {
        let _ = writeln!(content, "line {i:03}: 边界数据_alpha_beta_gamma_0123456789abcdef");
    }
    std::fs::write(dir.join("corpus.txt"), content).unwrap();
}

/// Provide deterministic GNU-format `wc` and `od -c` shims on Unix so recorded replay cassettes
/// are sealed against host coreutils/BSD formatting differences (e.g. BSD `wc` 8-column padding).
#[cfg(unix)]
pub(super) fn install_gnu_tool_shims(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("replay-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wc = bin.join("wc");
    std::fs::write(
        &wc,
        r#"#!/usr/bin/env python3
import os, stat, sys

argv = sys.argv[1:]
show_l = show_w = show_m = show_c = show_L = False
files = []
i = 0
stop_opts = False
unsupported = False
while i < len(argv):
    a = argv[i]
    i += 1
    if not stop_opts and a == "--":
        stop_opts = True
        continue
    if not stop_opts and a.startswith("--"):
        if a == "--lines":
            show_l = True
        elif a == "--words":
            show_w = True
        elif a == "--chars":
            show_m = True
        elif a == "--bytes":
            show_c = True
        elif a == "--max-line-length":
            show_L = True
        else:
            unsupported = True
            break
    elif not stop_opts and a.startswith("-") and len(a) > 1:
        for ch in a[1:]:
            if ch == "l":
                show_l = True
            elif ch == "w":
                show_w = True
            elif ch == "m":
                show_m = True
            elif ch == "c":
                show_c = True
            elif ch == "L":
                show_L = True
            else:
                unsupported = True
                break
        if unsupported:
            break
    else:
        files.append(a)

if unsupported:
    os.execv("/usr/bin/wc", ["/usr/bin/wc", *argv])

if not (show_l or show_w or show_m or show_c or show_L):
    show_l = show_w = show_c = True

cols = []
if show_l:
    cols.append("l")
if show_w:
    cols.append("w")
if show_m:
    cols.append("m")
if show_c:
    cols.append("c")
if show_L:
    cols.append("L")

def count_bytes(data: bytes) -> dict:
    lines = data.count(b"\n")
    words = len(data.split())
    text = data.decode("utf-8", errors="surrogateescape")
    chars = len(text)
    max_len = 0
    for line in text.splitlines():
        if len(line) > max_len:
            max_len = len(line)
    return {"l": lines, "w": words, "m": chars, "c": len(data), "L": max_len}

inputs = files if files else [None]
rows = []
reg_size_sum = 0
min_width = 1
status = 0

for name in inputs:
    if name is None or name == "-":
        try:
            st = os.fstat(0)
            if stat.S_ISREG(st.st_mode):
                reg_size_sum += st.st_size
            else:
                min_width = 7
        except OSError:
            min_width = 7
        data = sys.stdin.buffer.read()
        rows.append((count_bytes(data), name))
    else:
        try:
            st = os.stat(name)
            if stat.S_ISREG(st.st_mode):
                reg_size_sum += st.st_size
            else:
                min_width = 7
            with open(name, "rb") as f:
                data = f.read()
            rows.append((count_bytes(data), name))
        except OSError as e:
            sys.stderr.write(f"wc: {name}: {e.strerror}\n")
            status = 1

if len(inputs) > 1:
    total = {
        "l": sum(r[0]["l"] for r in rows),
        "w": sum(r[0]["w"] for r in rows),
        "m": sum(r[0]["m"] for r in rows),
        "c": sum(r[0]["c"] for r in rows),
        "L": max((r[0]["L"] for r in rows), default=0),
    }
    rows.append((total, "total"))

if len(cols) == 1 and len(inputs) == 1:
    width = 1
else:
    width = max(min_width, len(str(reg_size_sum)))
    for counts, _ in rows:
        for c in cols:
            width = max(width, len(str(counts[c])))

for counts, name in rows:
    nums = " ".join(f"{counts[c]:>{width}}" for c in cols)
    if name is not None:
        print(f"{nums} {name}")
    else:
        print(nums)

sys.exit(status)
"#,
    )
    .unwrap();
    let od = bin.join("od");
    std::fs::write(
        &od,
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nif args and args[0] == '-c' and all(a == '-c' or not a.startswith('-') for a in args):\n    paths = args[1:]\n    data = sys.stdin.buffer.read() if not paths else b''.join(open(p, 'rb').read() for p in paths)\n    esc = {0: r'\\0', 7: r'\\a', 8: r'\\b', 9: r'\\t', 10: r'\\n', 11: r'\\v', 12: r'\\f', 13: r'\\r'}\n    for off in range(0, len(data), 16):\n        cells = ''.join(f\"{(esc[b] if b in esc else chr(b) if 0x20 <= b <= 0x7e else f'{b:03o}'):>4}\" for b in data[off:off+16])\n        print(f'{off:07o}{cells}')\n    print(f'{len(data):07o}')\nelse:\n    os.execv('/usr/bin/od', ['/usr/bin/od', *args])\n",
    )
    .unwrap();
    for path in [&wc, &od] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[cfg(unix)]
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(super) fn write_opencode_config(env: &TestEnv, base_url: &str, route: &str, compact_enabled: bool) {
    std::fs::create_dir_all(env.config_dir()).unwrap();
    let compact_cfg = if compact_enabled {
        "mare.plugin(\"compact\", { keep_recent_tokens = 1, recent_user_tokens = 0 })\n"
    } else {
        ""
    };
    #[cfg(unix)]
    let tools_cfg = {
        let bin = install_gnu_tool_shims(env.home.path());
        let quoted_bin = shell_single_quote(&bin.to_string_lossy());
        // Runs after login-shell profiles (`/etc/profile`, `~/.profile`, macOS `path_helper`),
        // keeping `replay-bin` first on PATH and forcing `LC_ALL=C` so GNU grep does not fail
        // with `Invalid collation character` on `[一-龥]` under glibc's `C.UTF-8` locale.
        let prelude = format!(
            "export PATH={quoted_bin}:\"$PATH\" LC_ALL=C LANG=C PYTHONUTF8=1; unset GREP_OPTIONS GREP_COLORS GREP_COLOR _POSIX2_VERSION POSIXLY_CORRECT; hash -r"
        );
        format!("mare.plugin(\"tools\", {{ powershell = false, prelude = {prelude:?} }})\n")
    };
    #[cfg(not(unix))]
    let tools_cfg = "mare.plugin(\"tools\", { powershell = false })\n".to_string();
    let init_lua = format!(
        r#"mare.plugin("permissions", {{ enabled = false }})
{tools_cfg}{compact_cfg}mare.plugin("provider-opencode-go", {{
  base_url = {base_url:?},
  default_route = {route:?},
}})
"#
    );
    std::fs::write(env.config_dir().join("init.lua"), init_lua).unwrap();
    // Recorded opencode-go cassettes contain no `subagent.models` disclosure message. Pin the
    // participant's visible baseline state to the ready catalog during `context/transform` and
    // strip the synthetic sentinel in `request/build` so replay stays deterministic while keeping
    // the real `subagent` tool, system prompt, and `subagent.models` participant active.
    let plugins_dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("replay_pin_subagent.lua"),
        r#"return {
	name = "replay-pin-subagent",
	inject = { "bus", "llm" },
	apply = function(ctx)
		ctx:on("context/transform", function(ev, next)
			local res = next() or ev
			local refs = mare.json.array()
			for _, m in ipairs(ctx.llm:list_models().data) do
				if m.available and m.ready and type(m.ref) == "string" then
					refs[#refs + 1] = m.ref
				end
			end
			table.sort(refs)
			local msgs = res.messages or {}
			msgs[#msgs + 1] = {
				role = "user",
				content = {},
				_replay_pin = true,
				context_disclosure = {
					namespace = "subagent.models",
					state = { refs = refs },
				},
			}
			res.messages = msgs
			return res
		end)
		ctx:on("request/build", function(req, next)
			local res = next() or req
			local kept = {}
			for _, m in ipairs(res.messages or {}) do
				if not m._replay_pin then
					kept[#kept + 1] = m
				end
			end
			res.messages = kept
			return res
		end)
	end,
}
"#,
    )
    .unwrap();
}

pub(super) fn find_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    let mut stack = vec![home.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| t.lines().next().map(str::to_string))
                    .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                    .is_some_and(|h| h["type"] == "session" && h["session_id"] == session_id)
            {
                return Some(p);
            }
        }
    }
    None
}

pub(super) fn value_text(v: &Value) -> String {
    fn push(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(arr) => {
                for item in arr {
                    push(item, out);
                }
            }
            Value::Object(map) => {
                for val in map.values() {
                    push(val, out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    push(v, &mut out);
    out
}

pub(super) fn verify_resume_request_history(
    route: &str,
    resume_body: &Value,
    prompt_turn1: &str,
    prompt_turn2: &str,
    text1: &str,
    expect_thinking_on_resume: bool,
) -> Result<(), String> {
    match route {
        "anthropic" => {
            let msgs = resume_body["messages"]
                .as_array()
                .ok_or_else(|| "anthropic resume request missing messages array".to_string())?;
            let u1_idx = msgs
                .iter()
                .position(|m| m["role"] == "user" && value_text(&m["content"]).contains(prompt_turn1))
                .ok_or_else(|| "anthropic resume request missing turn 1 user prompt".to_string())?;
            let u2_idx = msgs
                .iter()
                .enumerate()
                .position(|(i, m)| {
                    i > u1_idx && m["role"] == "user" && value_text(&m["content"]).contains(prompt_turn2)
                })
                .ok_or_else(|| "anthropic resume request missing turn 2 user prompt after turn 1".to_string())?;

            let (tc_idx, call_id) = msgs
                .iter()
                .enumerate()
                .find_map(|(i, m)| {
                    if i <= u1_idx || i >= u2_idx || m["role"] != "assistant" {
                        return None;
                    }
                    let blocks = m["content"].as_array()?;
                    blocks.iter().find_map(|b| {
                        let is_read = b["type"] == "tool_use"
                            && b["name"] == "read"
                            && b["input"].is_object()
                            && value_text(&b["input"]).contains("corpus.txt");
                        if !is_read {
                            return None;
                        }
                        let id = b["id"].as_str().filter(|s| !s.is_empty())?;
                        Some((i, id.to_string()))
                    })
                })
                .ok_or_else(|| "anthropic resume request missing assistant read tool_use block".to_string())?;

            let tr_idx = msgs
                .iter()
                .enumerate()
                .position(|(i, m)| {
                    i > tc_idx
                        && i < u2_idx
                        && m["role"] == "user"
                        && m["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|b| {
                                b["type"] == "tool_result"
                                    && b["tool_use_id"].as_str() == Some(call_id.as_str())
                                    && b["is_error"] != Value::Bool(true)
                                    && value_text(&b["content"]).contains("中文跨字节边界测试")
                            })
                        })
                })
                .ok_or_else(|| "anthropic resume request missing matching tool_result".to_string())?;

            let has_reply = msgs.iter().enumerate().any(|(i, m)| {
                i > tr_idx
                    && i < u2_idx
                    && m["role"] == "assistant"
                    && m["content"].as_array().is_some_and(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(|t| t.contains(text1)))
                    })
            });
            if !has_reply {
                return Err("anthropic resume request missing turn 1 assistant text reply".to_string());
            }

            if expect_thinking_on_resume {
                let has_thinking = msgs.iter().any(|m| {
                    m["role"] == "assistant"
                        && m["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|b| {
                                b["type"] == "thinking" && b["signature"].as_str().is_some_and(|s| !s.is_empty())
                            })
                        })
                });
                if !has_thinking {
                    return Err("anthropic resume request missing signed thinking block".to_string());
                }
            }
            Ok(())
        }
        "responses" => {
            let items = resume_body["input"]
                .as_array()
                .ok_or_else(|| "responses resume request missing input array".to_string())?;
            let u1_idx = items
                .iter()
                .position(|item| item["role"] == "user" && value_text(&item["content"]).contains(prompt_turn1))
                .ok_or_else(|| "responses resume request missing turn 1 user prompt".to_string())?;
            let u2_idx = items
                .iter()
                .enumerate()
                .position(|(i, item)| {
                    i > u1_idx && item["role"] == "user" && value_text(&item["content"]).contains(prompt_turn2)
                })
                .ok_or_else(|| "responses resume request missing turn 2 user prompt after turn 1".to_string())?;

            let (tc_idx, call_id) = items
                .iter()
                .enumerate()
                .find_map(|(i, item)| {
                    if i <= u1_idx || i >= u2_idx {
                        return None;
                    }
                    if item["type"] != "function_call" || item["name"] != "read" {
                        return None;
                    }
                    let id = item["call_id"].as_str().filter(|s| !s.is_empty())?;
                    let args_str = item["arguments"].as_str()?;
                    let args_json: Value = serde_json::from_str(args_str).ok()?;
                    if !args_json.is_object() || !args_str.contains("corpus.txt") {
                        return None;
                    }
                    Some((i, id.to_string()))
                })
                .ok_or_else(|| "responses resume request missing assistant read function_call".to_string())?;

            let tr_idx = items
                .iter()
                .enumerate()
                .position(|(i, item)| {
                    i > tc_idx
                        && i < u2_idx
                        && item["type"] == "function_call_output"
                        && item["call_id"].as_str() == Some(call_id.as_str())
                        && value_text(&item["output"]).contains("中文跨字节边界测试")
                })
                .ok_or_else(|| "responses resume request missing matching function_call_output".to_string())?;

            let has_reply = items.iter().enumerate().any(|(i, item)| {
                i > tr_idx
                    && i < u2_idx
                    && item["type"] == "message"
                    && item["role"] == "assistant"
                    && item["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            b["type"] == "output_text" && b["text"].as_str().is_some_and(|t| t.contains(text1))
                        })
                    })
            });
            if !has_reply {
                return Err("responses resume request missing turn 1 assistant output_text reply".to_string());
            }

            if expect_thinking_on_resume && !items.iter().any(|item| item["type"] == "reasoning") {
                return Err("responses resume request missing reasoning item".to_string());
            }
            Ok(())
        }
        "openai-completions" => {
            let msgs = resume_body["messages"]
                .as_array()
                .ok_or_else(|| "openai-completions resume request missing messages array".to_string())?;
            let u1_idx = msgs
                .iter()
                .position(|m| m["role"] == "user" && value_text(&m["content"]).contains(prompt_turn1))
                .ok_or_else(|| "openai-completions resume request missing turn 1 user prompt".to_string())?;
            let u2_idx = msgs
                .iter()
                .enumerate()
                .position(|(i, m)| {
                    i > u1_idx && m["role"] == "user" && value_text(&m["content"]).contains(prompt_turn2)
                })
                .ok_or_else(|| "openai-completions resume request missing turn 2 user prompt".to_string())?;

            let (tc_idx, call_id) = msgs
                .iter()
                .enumerate()
                .find_map(|(i, m)| {
                    if i <= u1_idx || i >= u2_idx || m["role"] != "assistant" {
                        return None;
                    }
                    let calls = m["tool_calls"].as_array()?;
                    calls.iter().find_map(|tc| {
                        if tc["function"]["name"] != "read" {
                            return None;
                        }
                        let args_str = tc["function"]["arguments"].as_str()?;
                        if !args_str.contains("corpus.txt") {
                            return None;
                        }
                        let id = tc["id"].as_str().filter(|s| !s.is_empty())?;
                        Some((i, id.to_string()))
                    })
                })
                .ok_or_else(|| "openai-completions resume request missing assistant read tool_call".to_string())?;

            let tr_idx = msgs
                .iter()
                .enumerate()
                .position(|(i, m)| {
                    i > tc_idx
                        && i < u2_idx
                        && m["role"] == "tool"
                        && m["tool_call_id"].as_str() == Some(call_id.as_str())
                        && value_text(&m["content"]).contains("中文跨字节边界测试")
                })
                .ok_or_else(|| "openai-completions resume request missing matching tool result".to_string())?;

            let has_reply = msgs.iter().enumerate().any(|(i, m)| {
                i > tr_idx && i < u2_idx && m["role"] == "assistant" && value_text(&m["content"]).contains(text1)
            });
            if !has_reply {
                return Err("openai-completions resume request missing turn 1 assistant reply".to_string());
            }
            Ok(())
        }
        _ => Err("unexpected route".to_string()),
    }
}

/// Child stderr may contain encoded or otherwise unrecognizable credentials.
pub(super) const SUPPRESSED_CHILD_STDERR: &str = "[child stderr suppressed]";

#[cfg(test)]
mod record_guard_tests {
    use super::*;

    fn valid_cassette() -> Value {
        let interaction = serde_json::json!({
            "transport": "http",
            "request": { "method": "POST", "url": "https://example.invalid/v1/messages", "body": "{}" },
            "response": { "status": 200, "body": "ok" }
        });
        serde_json::json!({
            "version": 1,
            "metadata": { "name": "offline-recording", "phases": ["turn_1", "compact", "turn_2"] },
            "interactions": [interaction.clone(), interaction.clone(), interaction]
        })
    }

    #[test]
    fn recorded_cassette_guard_refuses_incomplete_inputs_before_replacement() {
        // reference implementation checks record write-back.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("candidate.json");
        let target = dir.path().join("existing.json");
        let staged = target.with_extension("json.new");
        let mut cases = Vec::new();
        let mut empty = valid_cassette();
        empty["interactions"] = serde_json::json!([]);
        cases.push(("empty", empty));
        let mut response_only = valid_cassette();
        for interaction in response_only["interactions"].as_array_mut().unwrap() {
            interaction.as_object_mut().unwrap().remove("request");
        }
        cases.push(("response-only", response_only));
        let mut mixed = valid_cassette();
        mixed["interactions"][1].as_object_mut().unwrap().remove("request");
        cases.push(("mixed", mixed));
        let mut non_object = valid_cassette();
        non_object["interactions"][1]["request"] = serde_json::json!("fake-secret-not-an-object");
        cases.push(("request-not-object", non_object));
        let mut invalid_request = valid_cassette();
        invalid_request["interactions"][1]["request"].as_object_mut().unwrap().remove("url");
        cases.push(("request-missing-url", invalid_request));
        let mut short_phases = valid_cassette();
        short_phases["metadata"]["phases"] = serde_json::json!(["turn_1", "turn_2"]);
        cases.push(("phase-count", short_phases));
        let mut missing_compact = valid_cassette();
        missing_compact["metadata"]["phases"] = serde_json::json!(["turn_1", "turn_1", "turn_2"]);
        cases.push(("missing-compact", missing_compact));
        let mut missing_turn = valid_cassette();
        missing_turn["metadata"]["phases"] = serde_json::json!(["turn_1", "compact", "compact"]);
        cases.push(("missing-turn-2", missing_turn));

        let mut bad = Vec::new();
        for (label, candidate) in cases {
            std::fs::write(&source, candidate.to_string()).unwrap();
            std::fs::write(&target, "existing fixture").unwrap();
            let result = publish_recorded_cassette(&source, &target);
            if result.is_ok() || std::fs::read_to_string(&target).unwrap() != "existing fixture" || staged.exists() {
                bad.push(label);
            }
        }
        assert!(bad.is_empty(), "invalid recordings replaced or staged fixtures: {bad:?}");
    }

    #[test]
    fn recorded_cassette_guard_accepts_complete_recording() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("candidate.json");
        let target = dir.path().join("existing.json");
        let input = valid_cassette().to_string();
        std::fs::write(&source, &input).unwrap();
        std::fs::write(&target, "old fixture").unwrap();
        publish_recorded_cassette(&source, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), input);
    }

    #[test]
    fn replay_history_diagnostic_does_not_echo_untrusted_call_id() {
        let secret = "fake-call-id-secret-123";
        let body = serde_json::json!({ "messages": [
            { "role": "user", "content": "first prompt" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "name": "read", "id": secret, "input": { "path": "corpus.txt" } }
            ] },
            { "role": "user", "content": "second prompt" }
        ] });
        let error = verify_resume_request_history("anthropic", &body, "first prompt", "second prompt", "reply", false)
            .unwrap_err();
        assert!(error.contains("missing matching tool_result"), "expected a missing tool result error");
        assert!(!error.contains(secret), "history diagnostic leaked a fake credential");
    }

    #[tokio::test]
    async fn recording_child_failure_does_not_forward_untrusted_output() {
        const TEST: &str = "suite::replay_corpus::harness::record_guard_tests::recording_child_failure_does_not_forward_untrusted_output";
        const STAGE: &str = "MARE_TEST_RECORD_DIAGNOSTIC_STAGE";
        const SECRET: &str = "fake-record-child-secret-123";
        const ENCODED: &str = "ZmFrZS1yZWNvcmQtY2hpbGQtc2VjcmV0LTEyMw==";
        const FORM: &str = "fake%2Drecord%2Dchild%2Dsecret%2D123";
        match std::env::var(STAGE).ok().as_deref() {
            Some("emit") => {
                println!("{SECRET} {FORM}");
                eprintln!("{ENCODED}");
                std::process::exit(42);
            }
            Some("wrapper") => {
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", TEST, "--nocapture"]).env(STAGE, "emit").env_remove("OPENCODE_API_KEY");
                let error = run_recording_child(command).await.expect_err("emitter must fail");
                eprintln!("{error}");
            }
            _ => {
                let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(STAGE, "wrapper")
                    .env_remove("OPENCODE_API_KEY")
                    .output()
                    .await
                    .unwrap();
                assert!(output.status.success(), "diagnostic wrapper failed");
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "diagnostic wrapper ran no test");
                let diagnostic =
                    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                assert!(diagnostic.contains("recording child failed"), "missing failure diagnostic");
                for secret in [SECRET, ENCODED, FORM] {
                    assert!(!diagnostic.contains(secret), "child output exposed a fake credential");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn replay_wc_shim_matches_gnu_column_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install_gnu_tool_shims(dir.path());
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        prepare_workspace(&ws);
        std::fs::write(ws.join("small.txt"), "hi\n").unwrap();

        let wc = bin.join("wc");
        let run_wc = |args: &[&str], stdin_bytes: Option<&[u8]>| -> String {
            let mut cmd = std::process::Command::new(&wc);
            cmd.args(args).current_dir(&ws);
            if stdin_bytes.is_some() {
                cmd.stdin(std::process::Stdio::piped());
            } else {
                cmd.stdin(std::process::Stdio::null());
            }
            cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            if let Some(bytes) = stdin_bytes {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(bytes).unwrap();
            }
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "wc {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };

        assert_eq!(run_wc(&["-l", "corpus.txt"], None), "141 corpus.txt\n");
        assert_eq!(run_wc(&["-c", "corpus.txt"], None), "8047 corpus.txt\n");
        assert_eq!(run_wc(&["-l", "-c", "corpus.txt"], None), " 141 8047 corpus.txt\n");
        assert_eq!(run_wc(&["-c", "-l", "corpus.txt"], None), " 141 8047 corpus.txt\n");
        assert_eq!(run_wc(&["-lc", "corpus.txt"], None), " 141 8047 corpus.txt\n");
        assert_eq!(run_wc(&["corpus.txt"], None), " 141  421 8047 corpus.txt\n");
        assert_eq!(
            run_wc(&["-l", "-c", "corpus.txt", "small.txt"], None),
            " 141 8047 corpus.txt\n   1    3 small.txt\n 142 8050 total\n"
        );
        let corpus_bytes = std::fs::read(ws.join("corpus.txt")).unwrap();
        assert_eq!(run_wc(&["-l"], Some(&corpus_bytes)), "141\n");
        assert_eq!(run_wc(&["-l", "-c"], Some(&corpus_bytes)), "    141    8047\n");
    }

    #[cfg(unix)]
    #[test]
    fn replay_c_locale_pipeline_matches_recorded_cassette() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install_gnu_tool_shims(dir.path());
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        prepare_workspace(&ws);

        let quoted_bin = shell_single_quote(&bin.to_string_lossy());
        let prelude = format!(
            "export PATH={quoted_bin}:\"$PATH\" LC_ALL=C LANG=C PYTHONUTF8=1; unset GREP_OPTIONS GREP_COLORS GREP_COLOR _POSIX2_VERSION POSIXLY_CORRECT; hash -r"
        );
        let pipeline = "wc -l -c corpus.txt; echo '---'; grep -n '[一-龥]' corpus.txt | head -20; echo '---'; tail -n 3 corpus.txt";
        let script = format!("{prelude}\n{pipeline}");

        let mut expected = String::from(" 141 8047 corpus.txt\n---\n1:中文跨字节边界测试：你好世界，协议回放验证。\n");
        for i in 1..=19 {
            let _ = writeln!(expected, "{}:line {i:03}: 边界数据_alpha_beta_gamma_0123456789abcdef", i + 1);
        }
        expected.push_str("---\n");
        for i in 138..=140 {
            let _ = writeln!(expected, "line {i:03}: 边界数据_alpha_beta_gamma_0123456789abcdef");
        }

        // Start from hostile C.UTF-8 + GREP_OPTIONS + _POSIX2_VERSION in a login shell to prove
        // the prelude's `LC_ALL=C` + PATH pinning overrides host/profile settings.
        let shell = if Path::new("/bin/bash").is_file() { "/bin/bash" } else { "/bin/sh" };
        let flag = if shell == "/bin/bash" { "-lc" } else { "-c" };
        let out = std::process::Command::new(shell)
            .args([flag, &script])
            .current_dir(&ws)
            .env("LC_ALL", "C.UTF-8")
            .env("LANG", "C.UTF-8")
            .env("LC_COLLATE", "C.UTF-8")
            .env("GREP_OPTIONS", "--color=always")
            .env("_POSIX2_VERSION", "200112")
            .env("POSIXLY_CORRECT", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "C-locale pipeline failed: stderr={}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stderr.is_empty(), "unexpected stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
    }
}

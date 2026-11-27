//! Native forced exit must kill the registered tool/MCP groups, including TERM-ignoring descendants.
use super::headless_pressure::Pressure;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Groups(Vec<PathBuf>);
impl Groups {
    fn members(&self) -> Vec<(i32, i32, i32)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        self.0
            .iter()
            .map(|path| {
                let value: Value = loop {
                    if let Ok(data) = std::fs::read(path)
                        && let Ok(parsed) = serde_json::from_slice(&data)
                    {
                        break parsed;
                    }
                    assert!(Instant::now() < deadline, "timed out waiting for group file {}", path.display());
                    std::thread::sleep(Duration::from_millis(10));
                };
                (
                    value["pid"].as_i64().unwrap() as i32,
                    value["grandchild"].as_i64().unwrap() as i32,
                    value["pgid"].as_i64().unwrap() as i32,
                )
            })
            .collect()
    }
}
impl Drop for Groups {
    fn drop(&mut self) {
        let mut owned = Vec::new();
        for path in &self.0 {
            if let Ok(data) = std::fs::read(path)
                && let Ok(value) = serde_json::from_slice::<Value>(&data)
                && let (Some(root), Some(grandchild), Some(pgid)) =
                    (value["pid"].as_i64(), value["grandchild"].as_i64(), value["pgid"].as_i64())
            {
                unsafe {
                    libc::kill(-(pgid as i32), libc::SIGKILL);
                }
                owned.extend([root as i32, grandchild as i32]);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !owned.is_empty() && Instant::now() < deadline {
            owned.retain(|&pid| unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == 0);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn forced_tree_exit(second: bool) {
    // Test-only reaping: after mare exits these orphaned descendants belong to this test,
    // allowing an exact ESRCH group assertion rather than mistaking zombies for running tools.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }, 0);
    let mut pressure = Pressure::configured(false, |home| {
        let script = home.join("ignore-term.py");
        std::fs::write(&script,r#"
import json, os, signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGINT, signal.SIG_IGN)
grandchild=os.fork()
if grandchild == 0:
    while True: time.sleep(1)
tmp = sys.argv[2] + '.tmp'
with open(tmp, 'w') as f:
    json.dump({'pid':os.getpid(),'grandchild':grandchild,'pgid':os.getpgrp()},f)
os.replace(tmp, sys.argv[2])
if sys.argv[1] == 'tool':
    while True: time.sleep(1)
while True:
    line=sys.stdin.readline()
    if not line:
        time.sleep(.01)
        continue
    message=json.loads(line)
    if 'id' not in message: continue
    if message.get('method') == 'initialize':
        result={'protocolVersion':message['params']['protocolVersion'],'capabilities':{},'serverInfo':{'name':'ignore-term','version':'1'}}
    elif message.get('method') == 'tools/list': result={'tools':[]}
    else: result={}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}),flush=True)
"#).unwrap();
        let tool = json!({"cmd":"python3","args":[script,"tool",home.join("tool.json")]});
        let mcp = json!({"name":"ignore-term","command":"python3","args":[script,"mcp",home.join("mcp.json")]});
        // JSON is data, converted through the public primitive (no plugin-specific lifecycle).
        format!(
            "held = {{ mare.proc.spawn(mare.json.decode({})), mare.mcp.connect(mare.json.decode({})) }}; io.write(string.rep('native-blocked-output',32768)); io.flush()",
            json!(tool.to_string()),
            json!(mcp.to_string())
        )
    });
    let mut groups = Groups(vec![pressure._home.path().join("tool.json"), pressure._home.path().join("mcp.json")]);
    pressure.blocked();
    let members = groups.members();
    for &(root, grandchild, pgid) in &members {
        assert_eq!(root, pgid);
        assert_eq!(unsafe { libc::getpgid(root) }, pgid, "root must be alive in its registered group");
        assert_eq!(unsafe { libc::getpgid(grandchild) }, pgid, "grandchild must be alive before forced exit");
    }
    let start = Instant::now();
    assert_eq!(unsafe { libc::kill(pressure.child.0.id() as i32, libc::SIGINT) }, 0);
    if second {
        assert_eq!(unsafe { libc::kill(pressure.child.0.id() as i32, libc::SIGTERM) }, 0);
    }
    let limit = if second { Duration::from_secs(2) } else { Duration::from_secs(7) };
    let status = loop {
        if let Some(status) = pressure.child.0.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < limit, "native shutdown failed to exit through blocked stdout");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(matches!(status.code(), Some(130 | 143)), "{status}");
    if !second {
        assert!(start.elapsed() >= Duration::from_secs(4), "first signal must allow its drain deadline");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    for ((root, grandchild, pgid), owned_path) in members.into_iter().zip(groups.0.clone()) {
        loop {
            for pid in [root, grandchild] {
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
            if unsafe { libc::kill(-pgid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                groups.0.retain(|path| path != &owned_path);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "forced exit left process group {pgid} alive (root {root}, grandchild {grandchild})"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn first_signal_deadline_reaps_term_ignoring_tool_mcp_and_grandchildren() {
    forced_tree_exit(false);
}
#[test]
fn second_signal_reaps_term_ignoring_tool_mcp_and_grandchildren() {
    forced_tree_exit(true);
}

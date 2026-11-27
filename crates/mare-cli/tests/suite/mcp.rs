//! The builtin `mcp` plugin and the `mare.mcp` primitive against a scripted stdio server:
//! listing bounds, tool-name hashing and collisions, reconnect after a crash,
//! list_changed refresh (debounced), the environment policy, stderr capture, cancellation,
//! approvals, the per-project disabled state and variable expansion.

use crate::common::*;
use serde_json::{Value, json};

mod approval;
mod codemode;
mod human_wait;
mod listing;
mod login_cli;
mod refresh;
mod required;
mod results;
mod rpc;
mod server_process;

/// A stdio MCP server driven by a JSON config (`mare-test-mcp scripted <config>`,
/// `tests/fake_mcp/scripted.rs`); every message it receives is appended to `log` as `<method>`
/// (`notify <method> <requestId>` for notifications), and it writes `start` on each launch.
/// Options: `tools` (names), `pages` (always another cursor), `list_delay` (seconds before
/// answering tools/list), `list_gate` (a file: tools/list is answered once it exists), `stderr` (lines printed at start), `env_dump` (file to write its
/// environment to), `grandchild` (file for the pid of a child it starts with its stdio). Tools:
/// `echo`, `crash` (exits), `slow` (answers after 10 min: never, within a test), `change` (switches to `changed_tools` and sends three list_changed), `big` (a 60 KB text,
/// a resource link, audio and structured content). `rich` adds a resource, a template and a `greet`
/// prompt; `instructions` is sent in the initialize result.
pub(crate) struct Fake {
    log: std::path::PathBuf,
}

impl Fake {
    pub(crate) fn new(env: &TestEnv, name: &str) -> Self {
        Self { log: env.home.path().join(format!("{name}.log")) }
    }
    /// A server entry (`command`, `args`) with this config merged over `{log}`.
    pub(crate) fn entry(&self, opts: Value) -> Value {
        let mut c = json!({ "log": self.log });
        merge(&mut c, opts);
        json!({ "command": fake_mcp_bin(), "args": ["scripted", c.to_string()] })
    }
    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(str::to_string).collect()
    }
    fn count(&self, what: &str) -> usize {
        self.lines().iter().filter(|l| l.as_str() == what).count()
    }
}

fn servers(map: Value) -> Value {
    json!({ "plugins": { "mcp": { "servers": map } } })
}

fn with(mut entry: Value, extra: Value) -> Value {
    merge(&mut entry, extra);
    entry
}

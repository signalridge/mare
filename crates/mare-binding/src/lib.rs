//! Primitives exposed to Lua as `mare.*` (Yazi `yazi-binding` style). Each module installs
//! itself into the `mare` table; `mare-plugin` assembles the runtime.

pub mod activity;
pub mod approval;
pub mod artifacts;
pub mod blocking;
pub mod cacerts;
pub mod clipboard;
pub mod codec;
pub mod crashguard;
pub mod danger;
pub mod exec_ctx;
pub mod exec_gate;
pub mod exec_lifetime;
pub mod fileindex;
pub mod filelock;
pub mod fs;
pub mod headtail;
pub mod http;
mod http_record;
pub mod http_serve;
mod human_wait;
pub mod image;
pub mod job;
pub mod mcp;
pub mod net_unix;
pub mod netauth;
pub mod netproxy;
pub mod ossandbox;
pub mod outdecode;
pub mod peer_client;
pub mod peers;
pub mod pgroups;
pub mod platform;
pub mod proc;
pub mod projection;
pub mod proxyenv;
pub mod pty;
pub mod ptybridge;
mod realpath;
mod relpath;
pub mod sandbox;
mod session_filters;
mod session_lineage;
mod session_query;
mod session_surface;
pub mod shell;
pub mod shellparse;
pub mod snapshot;
pub mod spill;
pub mod sse;
pub mod state_namespace;
pub mod statefile;
pub mod stopping;
pub mod store;
pub mod streaming;
pub mod subenv;
pub mod task;
pub mod text;
pub mod time;
pub mod trees;
pub mod ui;
mod ui_node;
mod ui_questions;
mod ui_select;
pub mod ui_wire;
pub mod winsandbox;
pub mod workspace;
pub mod ws;
pub mod yaml;

pub use artifacts::ARTIFACTS;
pub use ossandbox::{deny_read, read_mask};
pub use winsandbox::enhanced as winsandbox_user;

mod ui_image_resource;

#[cfg(test)]
mod test_home {
    pub(crate) fn assert_xdg(root: &std::path::Path) {
        for (name, directory) in [
            ("XDG_STATE_HOME", "xdg-state"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
        ] {
            assert_eq!(std::env::var_os(name).map(std::path::PathBuf::from), Some(root.join(directory)), "{name}");
        }
    }
}

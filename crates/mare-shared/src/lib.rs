//! Shared building blocks for mare crates: paths, styled text, the UI protocol, Lua helpers.

pub mod ansi;
pub mod artifact;

pub use artifact::ARTIFACTS;
pub mod bindings;
pub mod cancel;
pub mod carry;
pub mod config_env;
pub mod config_schema;
pub mod errors;
pub mod exec_ctx;
pub mod flags;
pub mod git;
pub mod keymap_contexts;
pub mod logfile;
pub mod lua;
pub mod lua_error;
pub mod markdown;
pub mod once;
pub mod paths;
pub mod private;
pub mod profile;
pub mod project;
pub mod redact;
pub mod schema;
pub mod schema_redaction;
pub mod session_barrier;
pub mod style;
pub mod txn_scope;
pub mod ui;
pub mod wire_projection;

pub mod image_resource;
pub mod layout;

/// Plugin definition table field for declarative static web assets.
pub const WEB_ASSETS_DEF_FIELD: &str = "web";

/// Configuration key, Lua `mare.plugins` namespace, and directory name for plugins.
pub const PLUGINS_KEY: &str = "plugins";

/// Table field name for option registries (`mare.options`) and model-parameter option lists.
pub const OPTIONS_KEY: &str = "options";

/// Canonical service name for the active agent service.
pub const AGENT_SERVICE_NAME: &str = "agent";

/// Canonical service and built-in plugin name for the active session service.
pub const SESSION_SERVICE_NAME: &str = "session";

/// Canonical service name for the active LLM service.
pub const LLM_SERVICE_NAME: &str = "llm";

/// Canonical LLM service method name for switching models.
pub const LLM_SET_MODEL_METHOD: &str = "set_model";

/// Canonical LLM service method name for changing model effort.
pub const LLM_SET_EFFORT_METHOD: &str = "set_effort";

/// Canonical service name for the active approvals service.
pub const APPROVALS_SERVICE_NAME: &str = "approvals";

/// Canonical service name for the host-provided options service.
pub const OPTIONS_SERVICE_NAME: &str = "options";

/// Canonical service name for the host-provided kernel service.
pub const KERNEL_SERVICE_NAME: &str = "kernel";

/// Canonical service name for the host-provided workspace service.
pub const WORKSPACE_SERVICE_NAME: &str = "workspace";

/// Canonical service name for the active permissions policy service.
pub const PERMISSIONS_SERVICE_NAME: &str = "permissions";

/// Canonical service name for the active tools registry service.
pub const TOOLS_SERVICE_NAME: &str = "tools";

/// Canonical service name for the active context assembly service.
pub const CONTEXT_SERVICE_NAME: &str = "context";

/// Scope identifier for session-scoped service methods and permission grants.
pub const SESSION_SCOPE: &str = "session";

/// Field key for structured error/notice/transcript message payloads and transcript message entries.
pub const MESSAGE_KEY: &str = "message";

/// Field key for tool definition lists in prompts and projected event tables.
pub const TOOLS_KEY: &str = "tools";

/// Field key for token usage summaries on agent and sub-agent results.
pub const USAGE_KEY: &str = "usage";

/// Field key for status entries in plugin introspection and sub-agent completion payloads.
pub const STATUS_KEY: &str = "status";

/// Field key for permission profiles on policy votes.
pub const PERMISSIONS_KEY: &str = "permissions";

/// Field key for attached images in submitted input events.
pub const IMAGES_KEY: &str = "images";

/// Namespace key for the `mare.ui` Lua API table.
pub const UI_NAMESPACE_KEY: &str = "ui";

/// Trust change source tag used when trust applies to the current session only.
pub const SESSION_TRUST_SOURCE: &str = "session";

/// Trust change source tag used when trust applies to the workspace.
pub const WORKSPACE_TRUST_SOURCE: &str = "workspace";

/// Strip at most one leading UTF-8 BOM (`EF BB BF`) from file text before parsing JSON.
pub fn strip_utf8_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Strip at most one leading UTF-8 BOM (`EF BB BF`) from file bytes before parsing JSON.
pub fn strip_utf8_bom_bytes(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

#[cfg(test)]
mod bom_tests {
    use super::{strip_utf8_bom, strip_utf8_bom_bytes};

    #[test]
    fn strip_utf8_bom_removes_at_most_one_leading_bom() {
        assert_eq!(strip_utf8_bom("\u{FEFF}{}"), "{}");
        assert_eq!(strip_utf8_bom("\u{FEFF}\u{FEFF}{}"), "\u{FEFF}{}");
        assert_eq!(strip_utf8_bom("{}"), "{}");
        assert_eq!(strip_utf8_bom(""), "");
        assert_eq!(strip_utf8_bom_bytes(b"\xEF\xBB\xBF{}"), b"{}");
        assert_eq!(strip_utf8_bom_bytes(b"\xEF\xBB\xBF\xEF\xBB\xBF{}"), b"\xEF\xBB\xBF{}");
        assert_eq!(strip_utf8_bom_bytes(b"{}"), b"{}");
    }
}

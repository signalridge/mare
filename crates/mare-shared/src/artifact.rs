//! Persistence artifact registry.
//!
//! Each owner crate exports `pub const ARTIFACTS: &[Artifact]` referencing its own version
//! constant (avoiding reverse dependencies). Top-level crates aggregate every slice via `all()`
//! for contract tests, scenario disk closure checks, and table checks.

use crate::logfile::KEEP as LOG_KEEP;
use crate::paths::WriteMode;

/// Stable identifier of a built-in persistence artifact.
///
/// Contract tests perform an exhaustive `match` over this enum so adding a new row fails
/// compilation until its production read/write contract test is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArtifactId {
    SessionJournal,
    SessionLock,
    SessionRollback,
    SessionCommit,
    SessionIndex,
    SessionAttachment,
    SessionSnapshot,
    StateFile,
    NamespaceState,
    Auth,
    Permissions,
    Trust,
    McpApprovals,
    McpDisabled,
    McpOauth,
    McpToolsCache,
    ModelsCache,
    SkillsCache,
    CrashGuard,
    PackageLock,
    PackageExtras,
    PackageCheckoutReferences,
    SpillResults,
    BashSpill,
    Logs,
    HostToken,
    LuacCache,
    News,
}

impl ArtifactId {
    /// Stable display identifier for diagnostics, assertions, and tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionJournal => "session.journal",
            Self::SessionLock => "session.lock",
            Self::SessionRollback => "session.rollback",
            Self::SessionCommit => "session.commit",
            Self::SessionIndex => "session.index",
            Self::SessionAttachment => "session.attachment",
            Self::SessionSnapshot => "session.snapshot",
            Self::StateFile => "state.json",
            Self::NamespaceState => "state.namespace",
            Self::Auth => "auth.json",
            Self::Permissions => "permissions.json",
            Self::Trust => "trust.json",
            Self::McpApprovals => "mcp.approvals",
            Self::McpDisabled => "mcp.disabled",
            Self::McpOauth => "mcp.oauth",
            Self::McpToolsCache => "mcp.tools_cache",
            Self::ModelsCache => "llm.models_cache",
            Self::SkillsCache => "context.skills_cache",
            Self::CrashGuard => "crash_guard",
            Self::PackageLock => "pkg.lock",
            Self::PackageExtras => "pkg.extras",
            Self::PackageCheckoutReferences => "pkg.checkout_refs",
            Self::SpillResults => "spill.results",
            Self::BashSpill => "spill.bash",
            Self::Logs => "logs",
            Self::HostToken => "host.token",
            Self::LuacCache => "kernel.luac_cache",
            Self::News => "news.json",
        }
    }
}

/// Base directory root under `MARE_HOME` / XDG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    Config,
    State,
    Data,
    Cache,
    /// User-configurable path with a built-in default location under `state`.
    UserPath,
}

impl Root {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "Config",
            Self::State => "State",
            Self::Data => "Data",
            Self::Cache => "Cache",
            Self::UserPath => "UserPath",
        }
    }
}

/// On-disk encoding format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jsonl,
    JsonDoc,
    JsonEnvelope,
    Sqlite,
    Text,
    Binary,
}

impl Format {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "Jsonl",
            Self::JsonDoc => "JsonDoc",
            Self::JsonEnvelope => "JsonEnvelope",
            Self::Sqlite => "Sqlite",
            Self::Text => "Text",
            Self::Binary => "Binary",
        }
    }
}

/// Write mechanism used to mutate the artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Atomic,
    Append,
    Stream,
    Sqlite,
    OpenExclusive,
}

impl WriteKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Atomic => "Atomic",
            Self::Append => "Append",
            Self::Stream => "Stream",
            Self::Sqlite => "Sqlite",
            Self::OpenExclusive => "OpenExclusive",
        }
    }
}

/// Cross-process locking discipline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    Lease,
    Flock,
    FlockTimeoutMs(u64),
    FlockBlocking,
    None,
}

impl LockKind {
    pub fn display(self) -> String {
        match self {
            Self::Lease => "Lease".into(),
            Self::Flock => "Flock".into(),
            Self::FlockTimeoutMs(ms) => format!("Flock({ms}ms)"),
            Self::FlockBlocking => "FlockBlocking".into(),
            Self::None => "None".into(),
        }
    }
}

/// Behavior when reading an existing artifact hits format corruption or an unsupported version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorruptPolicy {
    Journal,
    Refuse,
    Quarantine,
    Discard,
    WriteOnly,
}

impl CorruptPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Journal => "Journal",
            Self::Refuse => "Refuse",
            Self::Quarantine => "Quarantine",
            Self::Discard => "Discard",
            Self::WriteOnly => "WriteOnly",
        }
    }
}

/// Retention / size bound declared for the artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Files(u32),
    Bytes(u64),
    Count(u32),
    SoftCount(u32),
    AgeDays(u32),
    WithParent,
    PerProcess,
    UserOwned,
}

impl Bound {
    pub fn display(self) -> String {
        match self {
            Self::Files(n) => format!("Files({n})"),
            Self::Bytes(n) => format!("Bytes({n})"),
            Self::Count(n) => format!("Count({n})"),
            Self::SoftCount(n) => format!("SoftCount({n})"),
            Self::AgeDays(n) => format!("AgeDays({n})"),
            Self::WithParent => "WithParent".into(),
            Self::PerProcess => "PerProcess".into(),
            Self::UserOwned => "UserOwned".into(),
        }
    }
}

/// Sensitivity / redaction classification of stored content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentClass {
    Secret,
    UserContent,
    Redacted,
    Plain,
}

impl ContentClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "Secret",
            Self::UserContent => "UserContent",
            Self::Redacted => "Redacted",
            Self::Plain => "Plain",
        }
    }
}

/// Metadata row describing one built-in persistence artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub id: ArtifactId,
    pub root: Root,
    pub pattern: &'static str,
    pub sidecars: &'static [&'static str],
    pub owner: &'static str,
    pub format: Format,
    pub version: Option<u32>,
    pub write: WriteKind,
    pub lock: LockKind,
    pub corrupt: CorruptPolicy,
    pub mode: WriteMode,
    pub bound: Bound,
    pub content: ContentClass,
}

impl Artifact {
    /// Whether `rel_path` (slash-separated, relative to `root`) matches either `pattern` or a
    /// declared sidecar pattern. `UserPath` defaults under `State` are also matched when `root`
    /// is `Root::State`.
    pub fn matches_rel(&self, root: Root, rel_path: &str) -> bool {
        let root_ok = self.root == root || (self.root == Root::UserPath && root == Root::State);
        if !root_ok {
            return false;
        }
        glob_match(self.pattern, rel_path) || self.sidecars.iter().any(|s| glob_match(s, rel_path))
    }
}

/// Match a slash-separated path against an artifact glob pattern (`*` matches within a single
/// path component; `**` matches across any number of components).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p_segs: Vec<&str> = pattern.split('/').collect();
    let s_segs: Vec<&str> = path.split('/').collect();
    match_segments(&p_segs, &s_segs)
}

fn match_segments(p_segs: &[&str], s_segs: &[&str]) -> bool {
    match (p_segs.first(), s_segs.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            if match_segments(&p_segs[1..], s_segs) {
                return true;
            }
            !s_segs.is_empty() && match_segments(p_segs, &s_segs[1..])
        }
        (Some(pat), Some(seg)) => segment_match(pat, seg) && match_segments(&p_segs[1..], &s_segs[1..]),
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn segment_match(pat: &str, seg: &str) -> bool {
    let p = pat.as_bytes();
    let s = seg.as_bytes();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star_pi, mut star_si) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_si = si;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Persistence artifacts owned directly by `mare-shared`.
pub const ARTIFACTS: &[Artifact] = &[Artifact {
    id: ArtifactId::Logs,
    root: Root::State,
    pattern: "log/mare.log",
    sidecars: &["log/mare.log.*"],
    owner: "mare-shared::logfile",
    format: Format::Text,
    version: None,
    write: WriteKind::Stream,
    lock: LockKind::None,
    corrupt: CorruptPolicy::WriteOnly,
    mode: WriteMode::Private,
    bound: Bound::Files((LOG_KEEP as u32) + 1),
    content: ContentClass::Redacted,
}];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_handles_single_and_recursive_wildcards() {
        assert!(glob_match("sessions/*/*.jsonl", "sessions/--work--/2026_abc.jsonl"));
        assert!(!glob_match("sessions/*/*.jsonl", "sessions/--work--/nested/2026_abc.jsonl"));
        assert!(glob_match("sessions/*/agents/*/*.jsonl", "sessions/--work--/agents/r1/000-worker.jsonl"));
        assert!(glob_match(
            "sessions/*/agents/*/*.jsonl.lock.commit",
            "sessions/--work--/agents/r1/000-worker.jsonl.lock.commit"
        ));
        assert!(glob_match("sessions/*/*.jsonl.snapshots/**", "sessions/a/b.jsonl.snapshots/index.jsonl"));
        assert!(glob_match("sessions/*/*.jsonl.snapshots/**", "sessions/a/b.jsonl.snapshots/blobs/deadbeef"));
        assert!(glob_match("mcp-oauth/*-*.json", "mcp-oauth/srv-0123456789abcdef.json"));
        assert!(!glob_match("mcp-oauth/*-*.json", "mcp-oauth/srv.json"));
    }

    #[test]
    fn user_path_permissions_also_matches_default_state_path() {
        let perms = Artifact {
            id: ArtifactId::Permissions,
            root: Root::UserPath,
            pattern: "permissions.json",
            sidecars: &["permissions.json.lock"],
            owner: "permissions/rules_file.lua",
            format: Format::JsonDoc,
            version: Some(1),
            write: WriteKind::Atomic,
            lock: LockKind::FlockTimeoutMs(5000),
            corrupt: CorruptPolicy::Refuse,
            mode: WriteMode::Private,
            bound: Bound::Files(1),
            content: ContentClass::Plain,
        };
        assert!(perms.matches_rel(Root::State, "permissions.json"));
        assert!(perms.matches_rel(Root::State, "permissions.json.lock"));
        assert!(!perms.matches_rel(Root::Config, "permissions.json"));
    }
}

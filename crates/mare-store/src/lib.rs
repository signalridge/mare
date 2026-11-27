//! Session storage: append-only JSONL. Every entry is one JSON object per line.
//! The tree structure (id / parent) is interpreted by the `session` plugin; the store
//! guarantees durable appends and *strict* reads:
//!
//! * a trailing entry without '\n' is a torn write: readers ignore it, the writer truncates it
//!   before its next append;
//! * a '\n'-terminated line that does not parse is corruption: the file is never opened for
//!   writing or rewritten;
//! * readers take no lock: they freeze the end offset when they start and read only complete
//!   lines up to it, with a stat before and after;
//! * one writer, holding a non-blocking flock on the sibling `<session>.lock`, re-checked
//!   against the inode at that path after locking; within one process a
//!   session is open at most once (an in-process registry).
//!
//! Layout: `lock` (the writer's lock and private files), `stable` (stat-checked lock-free
//! reads), `validate` (strict line scans), `format` (version validation), `entries`
//! (readers), `session_file` (the writer), `files` (a session's files, listing, deletion).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

pub mod bounded;
pub mod index;
pub mod record;
pub mod reverse;
pub mod search;

mod entries;
mod files;
mod format;
mod lock;
mod session_file;
mod stable;
mod validate;

pub use entries::{
    check_file, read_entries, read_entries_bound, read_entries_lenient, read_entries_lenient_stable,
    read_entries_stable, read_fork_snapshot, read_header,
};
pub use files::{SessionInfo, cwd_slug, delete_session, list_sessions, list_sessions_with_unsupported, session_files};
pub use format::{CURRENT_VERSION, check_header};
pub use lock::{LockGuard, commit_path, file_identity, lock_path, lock_session, private_dir, rollback_path, sync_dir};
pub use session_file::{SessionFile, SessionSnapshot};
pub use stable::{FileStamp, STABLE_READ_TRIES, stable_read, stamp};
pub(crate) use validate::parse_line;
pub use validate::{Checkpoint, OpenStats};

/// `mare-store`-owned persistence artifacts.
pub const ARTIFACTS: &[mare_shared::artifact::Artifact] = &[
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::SessionJournal,
        root: mare_shared::artifact::Root::State,
        pattern: "sessions/*/*.jsonl",
        sidecars: &["sessions/*/agents/*/*.jsonl"],
        owner: "mare-store",
        format: mare_shared::artifact::Format::Jsonl,
        version: Some(CURRENT_VERSION as u32),
        write: mare_shared::artifact::WriteKind::Append,
        lock: mare_shared::artifact::LockKind::Lease,
        corrupt: mare_shared::artifact::CorruptPolicy::Journal,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::UserOwned,
        content: mare_shared::artifact::ContentClass::UserContent,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::SessionLock,
        root: mare_shared::artifact::Root::State,
        pattern: "sessions/*/*.jsonl.lock",
        sidecars: &["sessions/*/agents/*/*.jsonl.lock"],
        owner: "mare-store::lock",
        format: mare_shared::artifact::Format::Binary,
        version: None,
        write: mare_shared::artifact::WriteKind::OpenExclusive,
        lock: mare_shared::artifact::LockKind::Lease,
        corrupt: mare_shared::artifact::CorruptPolicy::Journal,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::WithParent,
        content: mare_shared::artifact::ContentClass::Plain,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::SessionRollback,
        root: mare_shared::artifact::Root::State,
        pattern: "sessions/*/*.jsonl.lock.rollback",
        sidecars: &["sessions/*/agents/*/*.jsonl.lock.rollback"],
        owner: "mare-store::lock",
        format: mare_shared::artifact::Format::Binary,
        version: None,
        write: mare_shared::artifact::WriteKind::Atomic,
        lock: mare_shared::artifact::LockKind::Lease,
        corrupt: mare_shared::artifact::CorruptPolicy::Journal,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::WithParent,
        content: mare_shared::artifact::ContentClass::Plain,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::SessionCommit,
        root: mare_shared::artifact::Root::State,
        pattern: "sessions/*/*.jsonl.lock.commit",
        sidecars: &["sessions/*/agents/*/*.jsonl.lock.commit"],
        owner: "mare-store::lock",
        format: mare_shared::artifact::Format::Binary,
        version: None,
        write: mare_shared::artifact::WriteKind::Atomic,
        lock: mare_shared::artifact::LockKind::Lease,
        corrupt: mare_shared::artifact::CorruptPolicy::Journal,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::WithParent,
        content: mare_shared::artifact::ContentClass::Plain,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::SessionIndex,
        root: mare_shared::artifact::Root::State,
        pattern: "sessions/index.sqlite",
        sidecars: &[
            "sessions/index.sqlite.lock",
            "sessions/index.sqlite-wal",
            "sessions/index.sqlite-shm",
            "sessions/index.sqlite-journal",
            "sessions/index.sqlite.bad-*",
        ],
        owner: "mare-store::index",
        format: mare_shared::artifact::Format::Sqlite,
        version: Some(index::SCHEMA_VERSION as u32),
        write: mare_shared::artifact::WriteKind::Sqlite,
        lock: mare_shared::artifact::LockKind::Flock,
        corrupt: mare_shared::artifact::CorruptPolicy::Quarantine,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::WithParent,
        content: mare_shared::artifact::ContentClass::UserContent,
    },
];

/// Error marker for a session file locked by another process (one writer per
/// session). Frontends offer "fork into a new session" or "read-only view".
pub const LOCKED: &str = "session locked by another process";

/// Error marker for a second open of a session this process already has open (in-process
/// registry — the second open is refused, never interleaved).
pub const OPEN_HERE: &str = "session is already open in this process";

/// Error marker for a session file with a '\n'-terminated line that does not parse.
/// The file is left byte-for-byte untouched.
pub const CORRUPT: &str = "session file is corrupt";

/// Error marker for an unsupported or missing session format version.
pub const UNSUPPORTED_VERSION: &str = "unsupported session format version";

/// Error marker for a malformed core record rejected before its batch is written.
pub const INVALID_RECORD: &str = "session.invalid_record";

/// Error marker for resuming a session whose file no longer exists (never re-created empty).
pub const NOT_FOUND: &str = "session file not found";
pub(crate) const ROLLBACK_IN_PROGRESS: &str = "session rollback in progress";

/// Tests that fork child processes and tests that may escalate an index flock must not overlap: a
/// forked child holds copies of every fd (close-on-exec only closes them at exec).
#[cfg(test)]
pub(crate) static FORK_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests;

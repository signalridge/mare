//! Package manager (lazy.nvim style): packages are git repos (or a subdirectory of
//! one) declared in init.lua:
//!
//! ```lua
//! mare.plugins.add {
//!   "someone/mare-git-tools",                         -- newest tag, else the default branch
//!   { "signalridge/mare-plugins", only = { "todo" }, version = "^1.2", opts = { todo = {} } },
//!   "me/monorepo:mare",                               -- a subdirectory (Yazi's syntax)
//!   { dir = "~/code/mare-myplugin" },                 -- local, never installed
//!   { import = "packages" },                          -- ~/.config/mare/lua/packages/*.lua
//! }
//! ```
//!
//! The Lua side (`mare.plugins.add` in mare-config) normalizes declarations — short forms, subdir
//! syntax, merging repeated declarations, `opts`, `enabled` functions, `import`; this crate
//! resolves versions, installs, and keeps `~/.config/mare/mare-lock.json`:
//! - `sync` installs missing packages at the locked commit (init.lua wins when its constraint
//!   changed), `update` moves to the newest revision allowed (and lists the new commits),
//!   `restore` puts checkouts back on the lock, `clean` removes checkouts no longer declared and
//!   stale lock entries (lazy.nvim `manage/`, Yazi `ya pkg`);
//! - every package succeeds or fails on its own and the lock is written as soon as one is done
//!   (Yazi `package.rs`); fetches run in parallel up to `mare.o.plugins.concurrency` (lazy.nvim
//!   `concurrency`, default 8); a disabled package keeps its lock entry (lazy `lock.lua`);
//! - missing packages are installed at startup before plugins load ([`install_missing`]);
//! - a lock entry records a content hash of the checkout ([`content_hash`], Yazi `hash.rs`),
//!   checked by `restore` and at startup; an older lock without one is accepted;
//! - besides `plugins/`, a package's `skills/`, `extensions/`, `prompts/`, `themes/` and `types/`
//!   (or custom paths declared in `mare-plugin.toml`) are found through [`content_dirs`] (`types/`
//!   mirrored for LuaLS by [`refresh_types`]);
//! - a package's `mare-plugin.toml` may say `requires = ">=0.4"`; an unsatisfied requirement keeps
//!   it from loading ([`problems`] says why); its `dependencies` order the load;
//! - extras (after LazyVim): the plugins of `signalridge/mare-plugins` (and
//!   of `mare.o.plugins.sources`) are the extras catalog, enabled in
//!   `~/.config/mare/plugins.json`.
//!
//! Sources: short names expand through `mare.o.plugins.url_format`, full
//! addresses follow git's rules, only https / ssh / file transports are used ([`source`]); git
//! never prompts and every call has a timeout and can be cancelled ([`git`], [`with_cancel`]);
//! checkouts live in `<data>/packages/<host>/<path>` and are cloned into a temporary directory
//! first, then renamed into place (lazy.nvim `task/git.lua` clone flags).
//!
//! A checkout with local edits is flagged, never refused — and never deleted by `clean`.
//!
//! Layout, bottom to top: `spec` (declarations → `PackageSpec`, options) and `lock` (the lock
//! file) next to `git` / `source`; `repo/` (one checkout: git operations, content hash,
//! manifest); `extras/` (plugins.json, the catalogs, which extras are on);
//! `effective/` (the enabled packages in load order, their health, their contents); `ops/` (sync,
//! update, restore, clean, reports). Each layer uses only the ones before it.

mod git;
pub mod source;

mod effective;
mod extras;
mod lock;
mod ops;
mod repo;
mod spec;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub use git::{DEFAULT_TIMEOUT, cancelled, with_cancel};
pub use source::{DEFAULT_URL_FORMAT, checkout_dir};

pub use effective::contents::{CONTENT_KINDS, content_dirs, package_dirs, refresh_types, resource_dirs, types_dir};
pub use effective::health::{diagnostics, installed, needs_install, problems, unavailable};
pub use effective::specs::{DependencyIssues, dependency_issues, disabled_specs, specs};
pub use extras::catalog::{CatalogEntry, catalog, catalog_for, fetch_catalog, fetch_catalog_for};
pub use extras::enabled::{Extra, extras, extras_for, install_plugins, plugin_source, set_extra, uninstall_plugins};
pub use extras::state::{ExtrasFile, PluginSelection, extras_path, migrate_extras, migrate_extras_for, read_extras};
pub use lock::{LockEntry, LockFile, PROJECT_SCOPE_KEY, lock_path, read_lock, set_project_scope};
pub use ops::clean::{clean_only, remove_unused};
pub use ops::report::{Status, UpdateCheck, check, diff_of, doctor, doctor_for, list, log_of, status};
pub use ops::restore::{repair_lock, restore, restore_only};
pub use ops::sync::{install_missing, install_missing_with_progress, sync, sync_only};
pub use ops::update::update;
pub use repo::hash::content_hash;
pub use repo::manifest::{
    Manifest, ManifestDiagnostic, PluginDependencies, PluginPlatform, PluginProvides, Recommended, WebManifest,
    manifest,
};
pub use spec::{InstalledPackage, Log, PackageSpec, PkgOptions, Target};

/// The extras catalog: its plugins are the official optional plugins.
pub const DEFAULT_SOURCE: &str = "signalridge/mare-plugins";

/// lazy.nvim `core/config.lua` `concurrency` default for git operations.
pub const DEFAULT_CONCURRENCY: usize = 8;

/// `plugins.json` format version this mare writes (older files are migrated when written back).
pub const EXTRAS_VERSION: u32 = 2;

/// `mare-plugin-manager`-owned persistence artifacts.
pub const ARTIFACTS: &[mare_shared::artifact::Artifact] = &[
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::PackageLock,
        root: mare_shared::artifact::Root::Config,
        pattern: "mare-lock.json",
        sidecars: &["mare-lock.json.lock"],
        owner: "mare-plugin-manager::lock",
        format: mare_shared::artifact::Format::JsonDoc,
        version: Some(lock::LOCK_VERSION),
        write: mare_shared::artifact::WriteKind::Atomic,
        lock: mare_shared::artifact::LockKind::Flock,
        corrupt: mare_shared::artifact::CorruptPolicy::Refuse,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::Files(1),
        content: mare_shared::artifact::ContentClass::Plain,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::PackageExtras,
        root: mare_shared::artifact::Root::Config,
        pattern: "plugins.json",
        sidecars: &["plugins.json.lock"],
        owner: "mare-plugin-manager::extras",
        format: mare_shared::artifact::Format::JsonDoc,
        version: Some(EXTRAS_VERSION),
        write: mare_shared::artifact::WriteKind::Atomic,
        lock: mare_shared::artifact::LockKind::Flock,
        corrupt: mare_shared::artifact::CorruptPolicy::Refuse,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::Files(1),
        content: mare_shared::artifact::ContentClass::Plain,
    },
    mare_shared::artifact::Artifact {
        id: mare_shared::artifact::ArtifactId::PackageCheckoutReferences,
        root: mare_shared::artifact::Root::Data,
        pattern: "packages/.locks/*.refs/*",
        sidecars: &[],
        owner: "mare-plugin-manager::lock",
        format: mare_shared::artifact::Format::JsonDoc,
        version: Some(lock::CHECKOUT_REFERENCE_VERSION),
        write: mare_shared::artifact::WriteKind::Atomic,
        lock: mare_shared::artifact::LockKind::Flock,
        corrupt: mare_shared::artifact::CorruptPolicy::Refuse,
        mode: mare_shared::paths::WriteMode::Private,
        bound: mare_shared::artifact::Bound::UserOwned,
        content: mare_shared::artifact::ContentClass::Plain,
    },
];

#[cfg(test)]
mod tests;

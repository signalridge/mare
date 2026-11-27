//! `mare.fs.index(dir, opts)`: the `@` completion index ( "`@` 文件补全的索引：第一次
//! 输入 `@` 时建立，之后靠文件变更增量更新"). One gitignore-aware walk builds it; after that a
//! refresh only stats the directories it knows and re-lists the ones whose mtime changed (an
//! entry added, removed or renamed in a directory changes that directory's mtime), so the cost
//! of keeping it current is one `stat` per directory, not a rescan of the tree. No file watcher
//! (no new dependency, nothing to leak across `/reload`, works the same on network mounts).
//!
//! * a directory whose mtime is too close to when it was listed is listed again next time
//!   ("racy" timestamps: a change within the filesystem's mtime granularity, as git does);
//! * `.gitignore` / `.ignore` of a known directory are stat'ed too: a changed rule re-walks that
//!   directory's subtree (an edit in place doesn't touch the directory's mtime);
//! * a new directory is walked in full, a vanished one drops its subtree;
//! * the total stays under `limit` entries, and a refresh runs at most every `min_interval_ms`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use mlua::{Lua, Table, UserData, UserDataMethods};

use mare_shared::lua::rt_err;

const IGNORE_FILES: [&str; 2] = [".gitignore", ".ignore"];
/// mtimes this close to the listing may hide a later change in the same tick
const RACY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Opts {
    pub hidden: bool,
    pub gitignore: bool,
    pub dirs: bool,
    pub limit: usize,
}

#[derive(Default)]
struct Dir {
    mtime: Option<SystemTime>,
    listed: Option<SystemTime>,
    listed_at: Option<Instant>,
    /// direct children, relative to the index root ("a/b.rs"); directories as "a/b/"
    files: Vec<String>,
    subdirs: Vec<String>,
    ignores: Vec<(&'static str, Option<SystemTime>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PolicyReadSig {
    restrict_read: bool,
    reads_pinned: bool,
    readable: Vec<PathBuf>,
    writable: Vec<PathBuf>,
    deny_read: Vec<crate::ossandbox::DenyReadPath>,
}

impl PolicyReadSig {
    fn from_policy(p: &crate::ossandbox::Policy) -> Self {
        Self {
            restrict_read: p.restrict_read,
            reads_pinned: p.reads_pinned,
            readable: p.readable.clone(),
            writable: p.writable.clone(),
            deny_read: p.deny_read.clone(),
        }
    }
}

#[derive(Default)]
struct State {
    /// directory (relative, "" = root) → its listing
    dirs: BTreeMap<String, Dir>,
    generation: u64,
    checked: Option<Instant>,
    entries: usize,
    truncated: bool,
    policy_sig: Option<PolicyReadSig>,
}

fn is_racy(mtime: Option<SystemTime>, listed: Option<SystemTime>, listed_at: Option<Instant>) -> bool {
    let (Some(m), Some(l)) = (mtime, listed) else {
        return true;
    };
    match l.duration_since(m) {
        Ok(age) => age < RACY,
        // Future mtime (e.g. NTP rollback or future-dated directory): fall back to monotonic
        // elapsed time since listing so the directory does not re-list forever.
        Err(_) => listed_at.is_none_or(|t| t.elapsed() < RACY),
    }
}

pub struct Index {
    root: PathBuf,
    opts: Opts,
    state: Arc<Mutex<State>>,
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn ignores_of(dir: &Path) -> Vec<(&'static str, Option<SystemTime>)> {
    IGNORE_FILES.iter().map(|n| (*n, mtime_of(&dir.join(n)))).collect()
}

fn rel_of(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

impl Index {
    pub fn new(root: PathBuf, opts: Opts) -> Self {
        Self { root, opts, state: Arc::default() }
    }

    fn walker(&self, from: &Path, depth: Option<usize>, policy: Option<&crate::ossandbox::Policy>) -> ignore::Walk {
        let g = self.opts.gitignore;
        let mut b = ignore::WalkBuilder::new(from);
        // parents(true) (the default): rules of the directories above `from` still apply
        b.hidden(!self.opts.hidden).git_ignore(g).git_global(g).git_exclude(g).ignore(g);
        b.max_depth(depth);
        let pol = policy.cloned();
        b.filter_entry(move |e| e.file_name() != ".git" && pol.as_ref().is_none_or(|p| p.can_read(e.path())));
        b.build()
    }

    /// Walk `rel` (and everything below it) into `st`.
    fn walk_into(&self, st: &mut State, rel: &str, policy: Option<&crate::ossandbox::Policy>) {
        let from = self.root.join(rel);
        let now = SystemTime::now();
        let now_mono = Instant::now();
        st.dirs.insert(
            rel.to_string(),
            Dir {
                mtime: mtime_of(&from),
                listed: Some(now),
                listed_at: Some(now_mono),
                ignores: ignores_of(&from),
                ..Default::default()
            },
        );
        for e in self.walker(&from, None, policy).flatten() {
            if e.depth() == 0 {
                continue;
            }
            if st.entries >= self.opts.limit {
                st.truncated = true;
                break;
            }
            let p = e.path();
            let r = rel_of(&self.root, p);
            let parent = p.parent().map(|q| rel_of(&self.root, q)).unwrap_or_default();
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if is_dir {
                st.dirs.insert(
                    r.clone(),
                    Dir {
                        mtime: mtime_of(p),
                        listed: Some(now),
                        listed_at: Some(now_mono),
                        ignores: ignores_of(p),
                        ..Default::default()
                    },
                );
            }
            let Some(d) = st.dirs.get_mut(&parent) else { continue };
            if is_dir {
                d.subdirs.push(r);
                if self.opts.dirs {
                    st.entries += 1;
                }
            } else {
                d.files.push(r);
                st.entries += 1;
            }
        }
    }

    fn count(&self, d: &Dir) -> usize {
        d.files.len() + if self.opts.dirs { d.subdirs.len() } else { 0 }
    }

    /// Drop `rel` and everything below it.
    fn drop_tree(&self, st: &mut State, rel: &str) {
        let prefix = format!("{rel}/");
        let gone: Vec<String> =
            st.dirs.keys().filter(|k| *k == rel || (rel.is_empty() || k.starts_with(&prefix))).cloned().collect();
        for k in gone {
            if let Some(d) = st.dirs.remove(&k) {
                st.entries = st.entries.saturating_sub(self.count(&d));
            }
        }
    }

    /// Re-list one directory's direct children; walk new subdirectories, drop vanished ones.
    fn relist(&self, st: &mut State, rel: &str, policy: Option<&crate::ossandbox::Policy>) {
        let from = self.root.join(rel);
        let (mut files, mut subdirs) = (vec![], vec![]);
        for e in self.walker(&from, Some(1), policy).flatten() {
            if e.depth() == 0 {
                continue;
            }
            let r = rel_of(&self.root, e.path());
            if e.file_type().is_some_and(|t| t.is_dir()) { subdirs.push(r) } else { files.push(r) }
        }
        let old = st.dirs.remove(rel).unwrap_or_default();
        st.entries = st.entries.saturating_sub(self.count(&old));
        for s in &old.subdirs {
            if !subdirs.contains(s) {
                self.drop_tree(st, s);
            }
        }
        // within the limit: whatever doesn't fit is left out, as in the first walk
        let room = self.opts.limit.saturating_sub(st.entries);
        if self.opts.dirs && subdirs.len() > room {
            subdirs.truncate(room);
            st.truncated = true;
        }
        let d_count = if self.opts.dirs { subdirs.len() } else { 0 };
        let file_room = room.saturating_sub(d_count);
        if files.len() > file_room {
            files.truncate(file_room);
            st.truncated = true;
        }
        let fresh: Vec<String> = subdirs.iter().filter(|s| !st.dirs.contains_key(*s)).cloned().collect();
        let d = Dir {
            mtime: mtime_of(&from),
            listed: Some(SystemTime::now()),
            listed_at: Some(Instant::now()),
            files,
            subdirs,
            ignores: ignores_of(&from),
        };
        st.entries += self.count(&d);
        st.dirs.insert(rel.to_string(), d);
        for s in fresh {
            if st.entries >= self.opts.limit {
                st.truncated = true;
                break;
            }
            // walk_into re-inserts the directory itself; its entry in `subdirs` is already counted
            self.walk_into(st, &s, policy);
        }
    }

    /// Build on first use, else bring the index up to date. Returns the generation (changes
    /// whenever the file list did).
    #[cfg_attr(not(test), allow(dead_code))]
    fn refresh(&self, min_interval: Duration) -> u64 {
        self.refresh_with_policy(min_interval, None)
    }

    fn refresh_with_policy(&self, min_interval: Duration, policy: Option<&crate::ossandbox::Policy>) -> u64 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let sig = policy.map(PolicyReadSig::from_policy);
        if st.checked.is_none() || st.policy_sig != sig {
            st.dirs.clear();
            st.entries = 0;
            st.truncated = false;
            st.policy_sig = sig;
            self.walk_into(&mut st, "", policy);
            st.generation += 1;
            st.checked = Some(Instant::now());
            return st.generation;
        }
        if st.checked.is_some_and(|t| t.elapsed() < min_interval) {
            return st.generation;
        }
        st.checked = Some(Instant::now());
        let mut changed = false;
        let keys: Vec<String> = st.dirs.keys().cloned().collect();
        for rel in keys {
            let Some(d) = st.dirs.get(&rel) else { continue }; // dropped with a parent meanwhile
            let path = self.root.join(&rel);
            let now_mtime = mtime_of(&path);
            if now_mtime.is_none() {
                self.drop_tree(&mut st, &rel);
                changed = true;
                continue;
            }
            let racy = is_racy(d.mtime, d.listed, d.listed_at);
            if ignores_of(&path) != d.ignores {
                // other rules for the whole subtree: walk it again
                self.drop_tree(&mut st, &rel);
                self.walk_into(&mut st, &rel, policy);
                changed = true;
            } else if now_mtime != d.mtime || racy {
                let before = (d.files.clone(), d.subdirs.clone());
                self.relist(&mut st, &rel, policy);
                let after = st.dirs.get(&rel).map(|d| (d.files.clone(), d.subdirs.clone()));
                changed |= after.as_ref() != Some(&before);
            }
        }
        if st.truncated && st.entries < self.opts.limit {
            st.dirs.clear();
            st.entries = 0;
            st.truncated = false;
            self.walk_into(&mut st, "", policy);
            changed = true;
        }
        if changed {
            st.generation += 1;
        }
        st.generation
    }

    fn files(&self) -> Vec<String> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::with_capacity(st.entries.min(self.opts.limit));
        for d in st.dirs.values() {
            if self.opts.dirs {
                out.extend(d.subdirs.iter().map(|s| format!("{s}/")));
            }
            out.extend(d.files.iter().cloned());
        }
        out.truncate(self.opts.limit);
        out
    }

    #[cfg(test)]
    fn dirs_known(&self) -> usize {
        self.state.lock().unwrap().dirs.len()
    }
}

/// The handle Lua holds: `refresh` runs off the runtime thread.
pub struct LuaIndex(Arc<Index>);

impl UserData for LuaIndex {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // refresh{min_interval_ms = 1000} -> generation
        m.add_async_method("refresh", |lua, this, opts: Option<Table>| {
            let allowed = crate::exec_ctx::check(&lua, "read", &this.0.root.to_string_lossy())
                .and_then(|()| crate::exec_ctx::refuse_deny_read(&lua, "file index refresh"));
            let policy = crate::exec_ctx::active(&lua);
            async move {
                allowed?;
                let policy = policy?.map(|a| a.policy);
                let ms = opts.and_then(|o| o.get::<Option<u64>>("min_interval_ms").ok().flatten()).unwrap_or(1000);
                let idx = this.0.clone();
                tokio::task::spawn_blocking(move || idx.refresh_with_policy(Duration::from_millis(ms), policy.as_ref()))
                    .await
                    .map_err(rt_err)
            }
        });
        // files() -> relative paths (directories end in "/" with `dirs`)
        m.add_method("files", |lua, this, ()| {
            crate::exec_ctx::check(lua, "read", &this.0.root.to_string_lossy())?;
            crate::exec_ctx::refuse_deny_read(lua, "file index files")?;
            Ok(this.0.files())
        });
    }
}

pub fn create(_lua: &Lua, root: PathBuf, opts: Opts) -> LuaIndex {
    LuaIndex(Arc::new(Index::new(root, opts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Opts {
        Opts { hidden: false, gitignore: true, dirs: true, limit: 1000 }
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    /// Back-date every directory so the next refresh doesn't treat it as racy.
    fn settle(root: &Path) {
        let old = SystemTime::now() - Duration::from_secs(60);
        for e in walkdir(root) {
            set_mtime(&e, old).unwrap();
        }
    }

    /// Set a file's or directory's mtime. Windows needs the handle opened for
    /// FILE_WRITE_ATTRIBUTES (a plain `File::open` is read-only: "Access is denied"), and a
    /// directory only opens with FILE_FLAG_BACKUP_SEMANTICS.
    fn set_mtime(p: &Path, t: SystemTime) -> std::io::Result<()> {
        let mut o = std::fs::File::options();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            o.access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        #[cfg(not(windows))]
        o.read(true);
        o.open(p)?.set_modified(t)
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = vec![root.to_path_buf()];
        let mut i = 0;
        while i < out.len() {
            if let Ok(rd) = std::fs::read_dir(&out[i]) {
                for e in rd.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        out.push(e.path());
                    }
                }
            }
            i += 1;
        }
        out
    }

    #[test]
    fn incremental_refresh_tracks_adds_removes_and_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap(); // a repository:.gitignore applies
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("src/deep/b.rs"), "").unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("src/x.log"), "").unwrap();
        settle(root);
        let idx = Index::new(root.to_path_buf(), opts());
        let g1 = idx.refresh(Duration::ZERO);
        assert_eq!(sorted(idx.files()), ["README.md", "src/", "src/a.rs", "src/deep/", "src/deep/b.rs"]);
        // nothing changed: same generation, nothing re-listed
        assert_eq!(idx.refresh(Duration::ZERO), g1);

        // a new file deep down, a new directory, a removed file
        std::fs::write(root.join("src/deep/c.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("lib/inner")).unwrap();
        std::fs::write(root.join("lib/inner/d.rs"), "").unwrap();
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        let g2 = idx.refresh(Duration::ZERO);
        assert!(g2 > g1);
        assert_eq!(
            sorted(idx.files()),
            [
                "README.md",
                "lib/",
                "lib/inner/",
                "lib/inner/d.rs",
                "src/",
                "src/deep/",
                "src/deep/b.rs",
                "src/deep/c.rs"
            ]
        );
        // the root's ignore rules still apply below it after a re-list
        std::fs::write(root.join("lib/inner/e.log"), "").unwrap();
        idx.refresh(Duration::ZERO);
        assert!(!idx.files().iter().any(|f| f.ends_with(".log")), "{:?}", idx.files());

        // a rule edited in place (the directory's mtime may not change): the subtree is re-walked
        settle(root);
        idx.refresh(Duration::ZERO);
        std::fs::write(root.join(".gitignore"), "*.log\nlib/\n").unwrap();
        set_mtime(&root.join(".gitignore"), SystemTime::now() + Duration::from_secs(5)).unwrap();
        idx.refresh(Duration::ZERO);
        assert!(!idx.files().iter().any(|f| f.starts_with("lib")), "{:?}", idx.files());

        // a removed directory drops its subtree; the throttle holds between refreshes
        std::fs::remove_dir_all(root.join("src/deep")).unwrap();
        let g = idx.refresh(Duration::from_secs(3600));
        assert!(idx.files().iter().any(|f| f.starts_with("src/deep")), "throttled: not checked yet");
        assert!(idx.refresh(Duration::ZERO) > g);
        assert_eq!(sorted(idx.files()), ["README.md", "src/"]);
        assert_eq!(idx.dirs_known(), 2);
    }

    #[test]
    fn the_limit_caps_the_index() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(dir.path().join(format!("f{i}")), "").unwrap();
        }
        let idx = Index::new(dir.path().to_path_buf(), Opts { limit: 10, ..opts() });
        idx.refresh(Duration::ZERO);
        assert_eq!(idx.files().len(), 10);
        std::fs::write(dir.path().join("new"), "").unwrap();
        idx.refresh(Duration::ZERO);
        assert!(idx.files().len() <= 10);
    }

    #[test]
    fn limit_truncation_recovers_siblings_when_quota_frees_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        for i in 0..8 {
            std::fs::write(root.join(format!("a/f{i}.txt")), "").unwrap();
            std::fs::write(root.join(format!("b/g{i}.txt")), "").unwrap();
        }
        settle(root);

        let idx = Index::new(root.to_path_buf(), Opts { dirs: false, limit: 5, ..opts() });
        idx.refresh(Duration::ZERO);
        let initial = idx.files();
        assert_eq!(initial.len(), 5);
        // Whichever directory filled the initial 5-entry quota, clear it so the untouched sibling must be recovered.
        let filled = if initial.iter().all(|f| f.starts_with("a/")) { "a" } else { "b" };
        let missing = if filled == "a" { "b" } else { "a" };
        for i in 0..8 {
            let name = if filled == "a" { format!("a/f{i}.txt") } else { format!("b/g{i}.txt") };
            std::fs::remove_file(root.join(name)).unwrap();
        }
        idx.refresh(Duration::ZERO);
        let files = sorted(idx.files());
        assert!(
            files.iter().any(|f| f.starts_with(&format!("{missing}/"))),
            "deleting entries in `{filled}/` must allow previously truncated sibling `{missing}/` to appear: {files:?}"
        );
    }

    #[test]
    fn policy_change_invalidates_cached_index_and_bypasses_throttle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("extra")).unwrap();
        std::fs::write(root.join("allowed.txt"), "").unwrap();
        std::fs::write(root.join("extra/opened.txt"), "").unwrap();
        settle(root);

        let canon_root = dunce::canonicalize(root).unwrap();
        let make_policy = |readable: Vec<PathBuf>| crate::ossandbox::Policy {
            level: crate::ossandbox::Level::ReadOnly,
            root: canon_root.clone(),
            extra: vec![],
            root_writable: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            writable: vec![],
            readable,
            deny_read: vec![],
            restrict_read: true,
            reads_pinned: true,
            network: crate::ossandbox::Network::Open,
            profile: None,
            unix_sockets: vec![],
            accept_partial: vec![],
            project_protected: vec![],
        };
        let restrictive = make_policy(vec![canon_root.join("allowed.txt")]);
        let open = make_policy(vec![canon_root.clone()]);

        let idx = Index::new(canon_root, opts());
        idx.refresh_with_policy(Duration::ZERO, Some(&restrictive));
        assert!(!idx.files().iter().any(|f| f == "extra/opened.txt"), "initial restrictive policy hides extra/");

        // Policy expands without touching filesystem mtimes, even under a long throttle window.
        idx.refresh_with_policy(Duration::from_secs(3600), Some(&open));
        assert!(
            idx.files().iter().any(|f| f == "extra/opened.txt"),
            "expanding read policy must invalidate cached dirs and bypass throttle: {:?}",
            idx.files()
        );
    }

    #[test]
    fn future_mtime_from_clock_rollback_does_not_relist_forever() {
        let now = SystemTime::now();
        let future_mtime = now + Duration::from_secs(3600);
        // Immediately after listing, a future mtime is treated as racy.
        assert!(is_racy(Some(future_mtime), Some(now), Some(Instant::now())));
        // Once the monotonic window elapses, it settles instead of triggering endless re-lists.
        let settled_mono = Instant::now().checked_sub(RACY + Duration::from_secs(1));
        if let Some(old_mono) = settled_mono {
            assert!(!is_racy(Some(future_mtime), Some(now), Some(old_mono)));
        }
        // Normal past mtimes still honor the wall-clock RACY window.
        assert!(is_racy(Some(now), Some(now), Some(Instant::now())));
        assert!(!is_racy(Some(now - RACY - Duration::from_secs(1)), Some(now), Some(Instant::now())));
    }
}

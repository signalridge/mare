//! Project resource root lookup.

use std::path::{Path, PathBuf};

/// Result of resolving the project resource root from `cwd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRoot {
    /// Canonicalized start directory (fallback: `cwd` as given).
    pub start: PathBuf,
    /// Nearest directory in `start..=bound` containing a valid `.mare/` directory or `.mcp.json`
    /// file (never the user config directory).
    pub root: Option<PathBuf>,
    /// Git worktree root bounding the upward walk (`None` outside a repository).
    pub bound: Option<PathBuf>,
    /// Further ancestor markers between `root` and `bound` that were ignored (single-layer rule).
    pub ignored: Vec<PathBuf>,
    /// `.mare` or `.mcp.json` entries ignored because they are symlinks pointing outside their
    /// candidate directory.
    pub rejected: Vec<PathBuf>,
}

/// Resolve the project resource root for `cwd`.
pub fn resource_root(cwd: &Path) -> ResourceRoot {
    resource_root_with_config(cwd, &crate::paths::config_dir())
}

fn resource_root_with_config(cwd: &Path, config_dir: &Path) -> ResourceRoot {
    let start = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let bound = crate::git::git_root(&start);
    let cfg_dir = dunce::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());

    let mut candidates = Vec::new();
    for dir in start.ancestors() {
        candidates.push(dir.to_path_buf());
        match &bound {
            Some(b) if dir == b.as_path() => break,
            None => break,
            Some(_) => {}
        }
    }

    let mut root = None;
    let mut ignored = Vec::new();
    let mut rejected = Vec::new();

    for c in candidates {
        let c_canon = dunce::canonicalize(&c).unwrap_or_else(|_| c.clone());
        if c_canon == cfg_dir {
            continue;
        }
        let mare_path = c.join(".mare");
        if std::fs::symlink_metadata(&mare_path).is_ok_and(|m| m.file_type().is_symlink())
            && dunce::canonicalize(&mare_path).ok().as_ref() == Some(&cfg_dir)
        {
            continue;
        }
        let has_mare = inspect_marker(&mare_path, &c_canon, true, &mut rejected);
        let has_mcp = inspect_marker(&c.join(".mcp.json"), &c_canon, false, &mut rejected);
        if has_mare || has_mcp {
            if root.is_none() {
                root = Some(c);
            } else {
                ignored.push(c);
            }
        }
    }

    ResourceRoot { start, root, bound, ignored, rejected }
}

fn inspect_marker(path: &Path, candidate_canon: &Path, expect_dir: bool, rejected: &mut Vec<PathBuf>) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        match dunce::canonicalize(path) {
            Ok(target) if target.starts_with(candidate_canon) => {
                if expect_dir {
                    target.is_dir()
                } else {
                    target.is_file()
                }
            }
            Ok(_) | Err(_) => {
                rejected.push(path.to_path_buf());
                false
            }
        }
    } else if expect_dir {
        meta.is_dir()
    } else {
        meta.is_file()
    }
}

impl ResourceRoot {
    /// Path to `<root>/.mare` when `root` is present and `.mare` was not rejected as a symlink
    /// pointing outside `root`.
    pub fn mare_dir(&self) -> Option<PathBuf> {
        let m = self.root.as_ref()?.join(".mare");
        (!self.rejected.contains(&m)).then_some(m)
    }
}

/// Directory on which project trust is recorded and checked: `root -> bound -> start`.
pub fn trust_subject(r: &ResourceRoot) -> &Path {
    r.root.as_deref().or(r.bound.as_deref()).unwrap_or(&r.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.join(".git/refs")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn subdir_start_finds_repo_root_mare_as_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let sub = repo.join("a/b");
        init_git(&repo);
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        let r = resource_root(&sub);
        assert_eq!(r.start, sub);
        assert_eq!(r.bound.as_deref(), Some(repo.as_path()));
        assert_eq!(r.root.as_deref(), Some(repo.as_path()));
        assert!(r.ignored.is_empty());
        assert!(r.rejected.is_empty());
    }

    #[test]
    fn no_git_uses_only_start_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = dunce::canonicalize(tmp.path()).unwrap().join("parent");
        let sub = parent.join("sub");
        std::fs::create_dir_all(parent.join(".mare")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        // Outside a git repo, parent/.mare is never used when starting in sub.
        let r_sub = resource_root(&sub);
        assert_eq!(r_sub.bound, None);
        assert_eq!(r_sub.root, None);
        // When start itself has a marker without git, start becomes the root.
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();
        let r_marked = resource_root(&sub);
        assert_eq!(r_marked.bound, None);
        assert_eq!(r_marked.root.as_deref(), Some(sub.as_path()));
    }

    #[test]
    fn nested_repo_and_gitfile_bound_search_at_inner_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let outer = base.join("outer");
        init_git(&outer);
        std::fs::create_dir_all(outer.join(".mare")).unwrap();

        // Nested repo without its own .mare stops at inner git root.
        let nested = outer.join("nested");
        let nested_sub = nested.join("src");
        init_git(&nested);
        std::fs::create_dir_all(&nested_sub).unwrap();
        let r_nested = resource_root(&nested_sub);
        assert_eq!(r_nested.bound.as_deref(), Some(nested.as_path()));
        assert_eq!(r_nested.root, None);

        // Linked worktree / submodule gitfile also stops at the inner dir.
        let main_git = base.join("main/.git");
        init_git(base.join("main").as_path());
        let wt_git = main_git.join("worktrees/wt1");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = outer.join("worktree");
        let wt_sub = wt.join("dir");
        std::fs::create_dir_all(&wt_sub).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let r_wt = resource_root(&wt_sub);
        assert_eq!(r_wt.bound.as_deref(), Some(wt.as_path()));
        assert_eq!(r_wt.root, None);
        std::fs::create_dir_all(wt.join(".mare")).unwrap();
        let r_wt_marked = resource_root(&wt_sub);
        assert_eq!(r_wt_marked.root.as_deref(), Some(wt.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn mare_symlink_outside_candidate_is_absent_and_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let repo = base.join("repo");
        let sub = repo.join("sub");
        let outside = base.join("outside-mare");
        init_git(&repo);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, sub.join(".mare")).unwrap();

        let r = resource_root(&sub);
        assert_eq!(r.root, None);
        assert_eq!(r.mare_dir(), None);
        assert_eq!(r.rejected, vec![sub.join(".mare")]);
    }

    #[test]
    fn second_marker_further_up_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let sub = repo.join("sub");
        let leaf = sub.join("leaf");
        init_git(&repo);
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(sub.join(".mcp.json"), "{}").unwrap();

        let r = resource_root(&leaf);
        assert_eq!(r.root.as_deref(), Some(sub.as_path()));
        assert_eq!(r.ignored, vec![repo]);
    }

    #[test]
    fn config_dir_is_never_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let cfg = repo.join("cfg-home");
        init_git(&repo);
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(cfg.join(".mare")).unwrap();

        // Direct start in config_dir skips config_dir marker and finds repo above.
        let r = resource_root_with_config(&cfg, &cfg);
        assert_eq!(r.root.as_deref(), Some(repo.as_path()));
        assert!(r.ignored.is_empty());
        assert!(r.rejected.is_empty());

        // Without repo marker above, config_dir alone yields None.
        std::fs::remove_dir_all(repo.join(".mare")).unwrap();
        let r_none = resource_root_with_config(&cfg, &cfg);
        assert_eq!(r_none.root, None);

        // A candidate whose `.mare` symlinks to config_dir is also never a root.
        #[cfg(unix)]
        {
            let linked = repo.join("linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink(&cfg, linked.join(".mare")).unwrap();
            let r_link = resource_root_with_config(&linked, &cfg);
            assert_eq!(r_link.root, None);
            assert!(r_link.ignored.is_empty());
        }
    }

    #[test]
    fn trust_subject_falls_back_root_then_bound_then_start() {
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let repo = base.join("repo");
        let sub = repo.join("sub/leaf");
        init_git(&repo);
        std::fs::create_dir_all(&sub).unwrap();

        // Inside a git repo without markers, trust_subject falls back to the git bound.
        let r_no_marker = resource_root(&sub);
        assert_eq!(r_no_marker.root, None);
        assert_eq!(r_no_marker.bound.as_deref(), Some(repo.as_path()));
        assert_eq!(trust_subject(&r_no_marker), repo.as_path());

        // Outside a git repo without markers, trust_subject falls back to start.
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let r_plain = resource_root(&plain);
        assert_eq!(r_plain.root, None);
        assert_eq!(r_plain.bound, None);
        assert_eq!(trust_subject(&r_plain), plain.as_path());
    }
}

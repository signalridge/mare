//! Directory listing, walking, and glob matching for `mare.fs`.

use std::path::Path;

use mare_shared::lua::rt_err;

pub(super) struct WalkOpts {
    pub(super) hidden: bool,
    pub(super) gitignore: bool,
    pub(super) max_depth: Option<usize>,
    pub(super) limit: usize,
    pub(super) glob: Option<globset::GlobSet>,
    pub(super) dirs: bool,
}

pub(super) fn walk(root: &Path, o: &WalkOpts, policy: Option<&crate::ossandbox::Policy>) -> Vec<String> {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(!o.hidden).git_ignore(o.gitignore).git_global(o.gitignore).git_exclude(o.gitignore).ignore(o.gitignore);
    b.max_depth(o.max_depth);
    let pol = policy.cloned();
    b.filter_entry(move |e| e.file_name() != ".git" && pol.as_ref().is_none_or(|p| p.can_read(e.path())));
    let mut out = Vec::new();
    for e in b.build().flatten() {
        if e.depth() == 0 {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && !o.dirs {
            continue;
        }
        let rel = e.path().strip_prefix(root).unwrap_or(e.path());
        if let Some(g) = &o.glob
            && !g.is_match(rel)
        {
            continue;
        }
        let rel_s = rel.to_string_lossy().into_owned();
        out.push(if is_dir { format!("{rel_s}/") } else { rel_s });
        if out.len() >= o.limit {
            break;
        }
    }
    out
}

pub(super) fn build_glob(patterns: &[String]) -> mlua::Result<globset::GlobSet> {
    let mut gb = globset::GlobSetBuilder::new();
    for p in patterns {
        let g = globset::GlobBuilder::new(p).literal_separator(false).build().map_err(rt_err)?;
        gb.add(g);
    }
    gb.build().map_err(rt_err)
}

/// The host runtime secret directory is hidden from listings: skipped
/// silently, never reported as a policy denial that would reveal it exists.
fn host_protected(policies: &[crate::ossandbox::Policy], path: &Path) -> bool {
    policies.iter().any(|p| p.is_host_protected_path(path))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EntryKind {
    Dir,
    File,
    Other,
}

impl EntryKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct ListDirOpts {
    pub(super) strict: bool,
    pub(super) no_follow: bool,
    pub(super) filter_denied: bool,
    pub(super) max_entries: Option<usize>,
}

pub(super) fn list_dir_entries_checked(
    dir: &Path,
    resolved_dir: Option<&Path>,
    policies: &[crate::ossandbox::Policy],
    policy_cwd: &Path,
    opts: ListDirOpts,
) -> std::io::Result<(Vec<(String, EntryKind)>, bool)> {
    let ListDirOpts { strict, no_follow, filter_denied, max_entries } = opts;
    let cap = max_entries.unwrap_or(usize::MAX);
    let mut v = Vec::new();
    let mut truncated = false;

    if no_follow {
        let dir_file = super::open_dir_no_follow(dir, None)?;
        let opened_dir = super::opened_path(&dir_file)?;
        super::read::check_policies_read(policies, &opened_dir, policy_cwd)?;
        #[cfg(unix)]
        {
            return list_dir_entries_from_fd(dir_file, &opened_dir, policies, policy_cwd, strict, filter_denied, cap);
        }
        #[cfg(not(unix))]
        {
            let rd = match std::fs::read_dir(&opened_dir) {
                Ok(rd) => rd,
                Err(e) if strict => return Err(e),
                Err(_) => return Ok((Vec::new(), false)),
            };
            for entry in rd {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) if strict => return Err(e),
                    Err(_) => continue,
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_path = opened_dir.join(&name);
                if host_protected(policies, &child_path) {
                    continue;
                }
                if super::read::check_read_with_symlink(policies, &child_path, policy_cwd).is_err() {
                    if strict && !filter_denied {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!("read_dir blocked by policy for {}", child_path.display()),
                        ));
                    }
                    continue;
                }
                if v.len() >= cap {
                    truncated = true;
                    break;
                }
                let kind = match entry.file_type() {
                    Ok(ft) if ft.is_dir() => EntryKind::Dir,
                    Ok(ft) if ft.is_file() => EntryKind::File,
                    _ => EntryKind::Other,
                };
                v.push((name, kind));
            }
            return Ok((v, truncated));
        }
    }

    // The directory itself must be readable, as on the no-follow path (including symlink target).
    let owned_physical;
    let physical_dir = match resolved_dir {
        Some(physical) => {
            if let Err(e) = super::read::check_read_resolved(policies, dir, physical, policy_cwd) {
                return if strict { Err(e) } else { Ok((Vec::new(), false)) };
            }
            physical
        }
        None => match super::read::check_read_with_symlink(policies, dir, policy_cwd) {
            Ok(p) => {
                owned_physical = p;
                &owned_physical
            }
            Err(e) => return if strict { Err(e) } else { Ok((Vec::new(), false)) },
        },
    };
    let rd = match std::fs::read_dir(physical_dir) {
        Ok(rd) => rd,
        Err(e) if strict => return Err(e),
        Err(_) => return Ok((Vec::new(), false)),
    };
    for entry in rd {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if strict => return Err(e),
            Err(_) => continue,
        };
        let path = entry.path();
        match list_entry_allowed(policies, &path, policy_cwd) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                if strict && !filter_denied {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("read_dir blocked by policy for {}", path.display()),
                    ));
                }
                continue;
            }
        }
        if v.len() >= cap {
            truncated = true;
            break;
        }
        let kind = match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => EntryKind::Dir,
            Ok(m) if m.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        v.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    Ok((v, truncated))
}

fn lexical_dangling_target(path: &Path, raw_target: std::path::PathBuf) -> std::path::PathBuf {
    let joined = if raw_target.is_absolute() {
        raw_target
    } else if let Some(parent) = path.parent() {
        parent.join(raw_target)
    } else {
        raw_target
    };
    let lexical = crate::realpath::lexical(&joined);
    if let (Some(parent), Some(name)) = (lexical.parent(), lexical.file_name())
        && let Ok(resolved_parent) = crate::realpath::resolve(parent)
    {
        return resolved_parent.join(name);
    }
    lexical
}

fn list_entry_allowed(policies: &[crate::ossandbox::Policy], path: &Path, policy_cwd: &Path) -> std::io::Result<bool> {
    if host_protected(policies, path) {
        return Ok(false);
    }
    match super::read::check_read_with_symlink(policies, path, policy_cwd) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return Ok(false);
            };
            if !meta.file_type().is_symlink() {
                return Ok(false);
            }
            let Ok(raw_target) = std::fs::read_link(path) else {
                return Ok(false);
            };
            let target = lexical_dangling_target(path, raw_target);
            if host_protected(policies, &target) {
                return Ok(false);
            }
            super::read::check_policies_read(policies, path, policy_cwd)?;
            super::read::check_policies_read(policies, &target, policy_cwd)?;
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn list_dir_entries_from_fd(
    dir_file: std::fs::File,
    opened_dir: &Path,
    policies: &[crate::ossandbox::Policy],
    policy_cwd: &Path,
    strict: bool,
    filter_denied: bool,
    cap: usize,
) -> std::io::Result<(Vec<(String, EntryKind)>, bool)> {
    use std::ffi::CStr;
    use std::os::fd::IntoRawFd;

    let raw_fd = dir_file.into_raw_fd();
    let dirp = unsafe { libc::fdopendir(raw_fd) };
    if dirp.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(raw_fd) };
        if strict {
            return Err(err);
        }
        return Ok((Vec::new(), false));
    }

    struct DirGuard(*mut libc::DIR);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let guard = DirGuard(dirp);
    let mut v = Vec::new();
    let mut truncated = false;
    loop {
        unsafe { *errno_location() = 0 };
        let dent = unsafe { libc::readdir(guard.0) };
        if dent.is_null() {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error().unwrap_or(0) != 0 && strict {
                return Err(err);
            }
            break;
        }
        let cname = unsafe { CStr::from_ptr((*dent).d_name.as_ptr()) };
        let bytes = cname.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(bytes).into_owned();
        let child_path = opened_dir.join(&name);
        match list_entry_allowed(policies, &child_path, policy_cwd) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                if strict && !filter_denied {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("read_dir blocked by policy for {}", child_path.display()),
                    ));
                }
                continue;
            }
        }
        if v.len() >= cap {
            truncated = true;
            break;
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatat(libc::dirfd(guard.0), cname.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        let kind = if rc == 0 {
            match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR => EntryKind::Dir,
                libc::S_IFREG => EntryKind::File,
                _ => EntryKind::Other,
            }
        } else {
            EntryKind::Other
        };
        v.push((name, kind));
    }
    Ok((v, truncated))
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[allow(dead_code)]
pub(crate) fn list_dir_entries(
    dir: &Path,
    policy: Option<&crate::ossandbox::Policy>,
    strict: bool,
) -> std::io::Result<Vec<(String, EntryKind)>> {
    let policies: Vec<crate::ossandbox::Policy> = policy.cloned().into_iter().collect();
    list_dir_entries_checked(dir, None, &policies, Path::new("/"), ListDirOpts { strict, ..Default::default() })
        .map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_filters_with_glob_keeps_dir_suffix_and_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/sub/mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let glob = build_glob(&["**/*.rs".to_string(), "src".to_string()]).unwrap();
        let mut got = walk(
            dir.path(),
            &WalkOpts { hidden: true, gitignore: false, max_depth: None, limit: 10, glob: Some(glob), dirs: true },
            None,
        );
        got.sort();
        let mut expected = vec![
            format!("{}/", std::path::PathBuf::from("src").display()),
            std::path::PathBuf::from("src").join("lib.rs").to_string_lossy().into_owned(),
            std::path::PathBuf::from("src").join("sub").join("mod.rs").to_string_lossy().into_owned(),
        ];
        expected.sort();
        assert_eq!(got, expected);

        let limited = walk(
            dir.path(),
            &WalkOpts { hidden: false, gitignore: false, max_depth: None, limit: 1, glob: None, dirs: false },
            None,
        );
        assert_eq!(limited.len(), 1);
    }
}

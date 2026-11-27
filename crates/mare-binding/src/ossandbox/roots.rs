//! The root sets a policy is built from: validated extra roots, temp areas, system read roots.

use std::path::{Path, PathBuf};

use super::SANDBOX_INVALID_ROOT;
use crate::realpath::lexical;

/// An explicit extra writable directory: must exist, be a directory, be absolute and be its own
/// physical path (no symlink anywhere in it). Checked again at every spawn.
pub fn validate_root(raw: &Path) -> Result<PathBuf, String> {
    let bad =
        |why: String| Err(format!("{SANDBOX_INVALID_ROOT}: invalid additional writable directory {raw:?}: {why}"));
    if !raw.is_absolute() {
        return bad("expected an absolute path".into());
    }
    let meta = match std::fs::symlink_metadata(raw) {
        Ok(m) => m,
        Err(e) => return bad(format!("cannot read the directory: {e}")),
    };
    if meta.file_type().is_symlink() {
        return bad("symlinked grants are not supported".into());
    }
    if !meta.is_dir() {
        return bad("expected an existing directory".into());
    }
    // dunce: std's canonicalize gives `\\?\C:\…` on Windows, which never equals the plain form
    let physical = match dunce::canonicalize(raw) {
        Ok(p) => p,
        Err(e) => return bad(e.to_string()),
    };
    if let Some(why) = linked_component(raw, &physical) {
        return bad(format!("{why}use the physical directory path {physical:?}; symlinked grants are not supported"));
    }
    Ok(physical)
}

/// A scoped read grant is already normalized when approved. A changed symlink must not
/// redirect that stored grant to a different target on a later tool call.
pub fn validate_read_grant(raw: &Path) -> Result<PathBuf, String> {
    let physical = crate::realpath::resolve(raw).map_err(|e| format!("cannot resolve read grant: {e}"))?;
    if !raw.is_absolute() || linked_component(raw, &physical).is_some() {
        return Err(format!(
            "{SANDBOX_INVALID_ROOT}: read grant {raw:?} is no longer its approved physical path; request permission again"
        ));
    }
    Ok(physical)
}

/// Whether an absolute extra writable root is definitely absent below a valid physical parent.
/// Errors other than ENOENT, and missing paths below symlinked / non-directory ancestors, are
/// left for `validate_root` to reject.
pub(super) fn root_missing(raw: &Path) -> bool {
    root_missing_with(raw, |path| std::fs::symlink_metadata(path))
}

fn root_missing_with(raw: &Path, mut metadata: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>) -> bool {
    if !raw.is_absolute() {
        return false;
    }
    match metadata(raw) {
        Ok(_) => false,
        Err(e) if is_missing_error(&e) => {
            let mut parent = raw.parent();
            while let Some(path) = parent {
                match metadata(path) {
                    Ok(meta) => {
                        if !meta.is_dir() {
                            return false;
                        }
                        let Ok(physical) = dunce::canonicalize(path) else {
                            return false;
                        };
                        return linked_component(path, &physical).is_none();
                    }
                    Err(e) if is_missing_error(&e) => parent = path.parent(),
                    Err(_) => return false,
                }
            }
            false
        }
        Err(_) => false,
    }
}

fn is_missing_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// Why `raw` isn't its own physical path (a prefix for the refusal), or `None` when it is.
/// Unix: any spelling other than the canonical one (a symlinked ancestor such as macOS `/var`).
/// Windows: an ancestor that is a symlink or junction (both name-surrogate reparse points, which
/// std's `is_symlink` reports). Spelling alone isn't compared there: case, `\\?\` and 8.3 short
/// names (`C:\Users\RUNNER~1\…`, the CI runners' `%TEMP%`) differ without any link.
fn linked_component(raw: &Path, physical: &Path) -> Option<String> {
    let lex = lexical(raw);
    if cfg!(windows) {
        lex.ancestors()
            .find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
            .map(|a| format!("{a:?} is a symlink or junction; "))
    } else if cfg!(target_os = "macos") {
        if let Some(a) =
            lex.ancestors().find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
        {
            return Some(format!("{a:?} is a symlink; "));
        }
        if physical == lex {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if physical.to_string_lossy().eq_ignore_ascii_case(&lex.to_string_lossy())
                && let (Ok(lm), Ok(pm)) = (std::fs::symlink_metadata(&lex), std::fs::symlink_metadata(physical))
                && (lm.dev(), lm.ino()) == (pm.dev(), pm.ino())
            {
                return None;
            }
        }
        Some(String::new())
    } else {
        (physical != lex).then(String::new)
    }
}

pub(super) fn temp_roots(exclude_env: bool, exclude_slash_tmp: bool) -> Vec<PathBuf> {
    if cfg!(windows) {
        return if exclude_env { vec![] } else { crate::winsandbox::session_temp_dir().into_iter().collect() };
    }
    let slash_tmp = dunce::canonicalize("/tmp").ok();
    let mut roots = vec![];
    if !exclude_slash_tmp {
        roots.push(PathBuf::from("/tmp"));
    }
    if !exclude_env {
        // A real environment entry is independent of the /tmp default, even if both
        // spell the same path. An absent TMPDIR must not re-add /tmp via std's fallback.
        if let Some(value) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                roots.push(path);
            }
        }
        let standard = std::env::temp_dir();
        if standard.is_absolute() && dunce::canonicalize(&standard).ok() != slash_tmp {
            roots.push(standard);
        }
        #[cfg(target_os = "macos")]
        if let Some(user_temp) = darwin_user_temp_dir()
            && dunce::canonicalize(&user_temp).ok() != slash_tmp
        {
            roots.push(user_temp);
        }
    }
    let mut roots: Vec<_> = roots.into_iter().filter_map(|path| dunce::canonicalize(path).ok()).collect();
    roots.sort();
    roots.dedup();
    roots
}

/// `confstr(_CS_DARWIN_USER_TEMP_DIR)`: the per-user temp dir under /var/folders.
#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: buf is valid for buf.len() bytes; confstr writes a NUL-terminated string
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    String::from_utf8(buf).ok().map(PathBuf::from)
}

/// Roots every process needs to read when reads are restricted (binaries, libraries, config).
pub(crate) const SYSTEM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library",
    "/Applications",
    "/opt",
    "/nix",
    "/run/current-system/sw",
    "/etc",
    "/private/etc",
    "/private/var/db",
    "/var/db",
    "/dev",
    "/lib",
    "/lib64",
    "/proc",
    "/sys",
];

pub(super) const WRITE_DEVICES: &[&str] =
    &["/dev/null", "/dev/zero", "/dev/stdout", "/dev/stderr", "/dev/dtracehelper", "/dev/tty"];

pub(crate) fn is_write_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    WRITE_DEVICES.contains(&s.as_ref()) || s.starts_with("/dev/fd/") || s.starts_with("/dev/tty")
}

pub(crate) fn dedup(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v.dedup();
    v
}

/// Never expose any askpass directory read-write to sandboxed commands.
#[cfg(target_os = "linux")]
pub(crate) fn passthrough_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{root_missing, root_missing_with};
    use std::io::ErrorKind;

    #[test]
    fn only_missing_roots_under_physical_directory_ancestors_are_omittable() {
        let temp = tempfile::tempdir().unwrap();
        // macOS `/var` is a symlink to `/private/var`; build the missing-root fixture from the
        // physical temp directory so this case tests only the missing final component.
        let physical = dunce::canonicalize(temp.path()).unwrap().join("physical");
        std::fs::create_dir(&physical).unwrap();
        assert!(root_missing(&physical.join("gone")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let child = physical.join("child");
            std::fs::create_dir(&child).unwrap();
            let link = temp.path().join("link");
            symlink(&physical, &link).unwrap();
            assert!(!root_missing(&link.join("gone")));
            assert!(!root_missing(&link.join("child").join("gone")));

            let file = temp.path().join("file");
            std::fs::write(&file, "x").unwrap();
            let not_a_directory = file.join("gone");
            let error = std::fs::symlink_metadata(&not_a_directory).unwrap_err();
            assert_ne!(error.kind(), ErrorKind::NotFound);
            assert!(!root_missing(&not_a_directory));
        }
    }

    #[test]
    fn permission_denied_is_not_treated_as_a_missing_root() {
        let temp = tempfile::tempdir().unwrap();
        let physical = dunce::canonicalize(temp.path()).unwrap();
        let inaccessible = physical.join("unreadable");
        let error = std::io::Error::from(ErrorKind::PermissionDenied);
        assert!(!root_missing_with(&inaccessible, |path| {
            if path == inaccessible {
                Err(std::io::Error::new(error.kind(), error.to_string()))
            } else {
                std::fs::symlink_metadata(path)
            }
        }));
    }
}

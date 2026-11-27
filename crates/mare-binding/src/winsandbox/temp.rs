//! The private temp directory and how it gets its write access.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(windows)]
use super::win;

/// Is a process with this pid still running? (Also the peer registry's check on Windows.)
pub(crate) fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        win::pid_alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Process creation time token on Windows (`GetProcessTimes` FILETIME as `u64`).
#[cfg(windows)]
pub(crate) fn pid_start_time(pid: u32) -> Option<u64> {
    win::pid_start_time(pid)
}

/// This process's private temp directory (`<temp>/mare-sbx/<pid>-<random>`), created on first
/// use. Directories left by processes that are gone are removed then (with them goes the grant
/// their ACL carried).
pub fn session_temp_dir() -> Result<PathBuf, String> {
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::temp_dir().join("mare-sbx");
        std::fs::create_dir_all(&base).map_err(|e| format!("create {}: {e}", base.display()))?;
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let pid = name.split('-').next().and_then(|p| p.parse::<u32>().ok());
                if pid.is_some_and(|p| p != std::process::id() && !pid_alive(p))
                    && let Err(error) = std::fs::remove_dir_all(e.path())
                {
                    eprintln!("mare: remove stale sandbox temp {}: {error}", e.path().display());
                }
            }
        }
        let dir = base.join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let dir = dunce_canonical(&dir)?;
        <crate::platform::Process as crate::platform::ProcessBackend>::at_exit(remove_session_temp);
        Ok(dir)
    })
    .clone()
}

extern "C" fn remove_session_temp() {
    if let Ok(dir) = session_temp_dir() {
        remove_session_dir(&dir);
    }
}

fn remove_session_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        eprintln!("mare: remove sandbox temp {}: {error}", dir.display());
        return;
    }
    // Another process may own a child: only remove an empty shared container.
    if let Some(base) = dir.parent()
        && let Err(error) = std::fs::remove_dir(base)
        && !matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty)
    {
        eprintln!("mare: remove sandbox temp container {}: {error}", base.display());
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn session_exit_callback_removes_private_directory_and_container() {
        const CHILD: &str = "MARE_TEST_SESSION_TEMP_EXIT";
        if std::env::var_os(CHILD).is_some() {
            let directory = session_temp_dir().unwrap();
            std::fs::write(directory.join("owned"), "cleanup at exit").unwrap();
            if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("exit")) {
                crate::platform::exit(0);
            }
            return;
        }
        for exit_mode in ["return", "exit"] {
            let root = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "winsandbox::temp::cleanup_tests::session_exit_callback_removes_private_directory_and_container",
                    "--nocapture",
                ])
                .env(CHILD, exit_mode)
                .env("TMPDIR", root.path())
                .env("TEMP", root.path())
                .env("TMP", root.path())
                .output()
                .unwrap();
            assert!(result.status.success(), "child failed: {result:?}");
            assert!(
                std::fs::read_dir(root.path()).unwrap().next().is_none(),
                "exit callback left temp resources: {result:?}"
            );
            root.close().unwrap();
        }
    }

    #[test]
    fn session_cleanup_preserves_other_owners_and_removes_empty_container() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("mare-sbx");
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("owned"), "keep").unwrap();
        remove_session_dir(&first);
        assert!(!first.exists());
        assert_eq!(std::fs::read_to_string(second.join("owned")).unwrap(), "keep");
        remove_session_dir(&second);
        assert!(!base.exists());
        temp.close().unwrap();
    }
}

pub(super) fn dunce_canonical(p: &Path) -> Result<PathBuf, String> {
    // the one path resolver (symlinks resolved per component, `\\?\` stripped)
    crate::realpath::resolve(p).map_err(|e| format!("cannot resolve sandbox path: {e}"))
}

/// `inner` is `outer` or inside it, by canonical identity (case, `\\?\` and 8.3 short names
/// don't count as differences; only real links and junctions do).
fn within(inner: &Path, outer: &Path) -> bool {
    let (Ok(ki), Ok(ko)) = (crate::realpath::canonical_path_key(inner), crate::realpath::canonical_path_key(outer))
    else {
        return false;
    };
    ki == ko || ki.starts_with(&format!("{}/", ko.trim_end_matches('/')))
}

/// The private temp directory and a writable root must not nest either way: ACE inheritance
/// would hand one's capability to the other.
pub fn disjoint(a: &Path, b: &Path) -> bool {
    crate::realpath::resolve(a).is_ok() && crate::realpath::resolve(b).is_ok() && !within(a, b) && !within(b, a)
}

/// What `/permission caps`, `mare doctor` and the sandbox report say when a writable root
/// contains %TEMP%.
pub const TEMP_NOT_ISOLATED: &str = "temp directory not separately isolated (a writable root contains %TEMP%: the \
     private temp directory uses that root's write access, not its own revocable grant)";

/// How the private temp directory gets its write access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempGrant {
    /// its own revocable identity ([`temp_write_sid`]), disjoint from every writable root
    ///
    /// [`temp_write_sid`]: crate::winsandbox::temp_write_sid
    Isolated,
    /// this writable root contains %TEMP% (mare started in the home folder, at `C:\` or in
    /// %TEMP%): the private temp directory is already writable through the root's grant, so it
    /// gets no identity of its own
    Shared(PathBuf),
}

/// Decide [`TempGrant`] for writable `roots`, the shared temp directory `temp_base` (%TEMP%) and
/// the private one under it (`<temp_base>\mare-sbx\<pid>-<id>`). A root that contains
/// `temp_base` shares; any other overlap with the private directory (a root at `mare-sbx` or
/// inside the private directory) is refused as before.
pub fn temp_grant(roots: &[PathBuf], temp_base: &Path, private: &Path) -> Result<TempGrant, String> {
    for path in roots.iter().map(PathBuf::as_path).chain([temp_base, private]) {
        crate::realpath::resolve(path).map_err(|e| format!("cannot resolve sandbox path: {e}"))?;
    }
    if let Some(r) = roots.iter().find(|r| within(temp_base, r)) {
        return Ok(TempGrant::Shared(r.clone()));
    }
    if let Some(r) = roots.iter().find(|r| !disjoint(r, private)) {
        return Err(format!(
            "sandbox: the private temp directory {} and the writable root {} overlap (ACL inheritance \
             would merge their capabilities); set TEMP outside the workspace",
            private.display(),
            r.display()
        ));
    }
    Ok(TempGrant::Isolated)
}

/// [`temp_grant`] for this process's %TEMP% and private temp directory.
pub fn session_temp_grant(roots: &[PathBuf]) -> Result<TempGrant, String> {
    temp_grant(roots, &dunce_canonical(&std::env::temp_dir())?, &session_temp_dir()?)
}

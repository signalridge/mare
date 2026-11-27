//! Single registry of host runtime secret paths protected from confined processes.

use std::path::PathBuf;

use super::SANDBOX_INSUFFICIENT;
use super::backend::backend;
use super::caps::Cap;

/// Filesystem shape of a registered host secret path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostShape {
    Dir,
    File,
}

/// What to do when the active sandbox backend cannot enforce protection for a registered path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapAction {
    Refuse,
    Report,
}

/// One registered host secret path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    pub path: PathBuf,
    pub shape: HostShape,
    pub on_gap: GapAction,
}

/// The host runtime token directory (`<state>/host`).
pub fn host_token_dir() -> PathBuf {
    let token_file = mare_shared::paths::host_token_file();
    token_file.parent().unwrap_or(&token_file).to_path_buf()
}

/// All host paths that confined processes must not read (single source of truth).
pub fn host_protected() -> Vec<HostPath> {
    vec![HostPath { path: host_token_dir(), shape: HostShape::Dir, on_gap: GapAction::Refuse }]
}

/// Tri-state status of a registered host secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretStatus {
    /// Confirmed absent (directory is readable and empty, or confirmed `NotFound`).
    Absent,
    /// Directory contains at least one entry (e.g. `token` or `.token.*.tmp`), or file exists.
    Present,
    /// Cannot be determined (permission denied, I/O error, not a directory, or is a symlink).
    /// Must be treated conservatively as `Present`.
    Unknown,
}

impl SecretStatus {
    pub fn is_present_or_unknown(self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// Ensure all registered `Dir` entries exist with private `0700` permissions and are real
/// directories (not symlinks or files).
pub fn ensure_host_protected_dirs() -> Result<(), String> {
    for entry in host_protected() {
        if entry.shape == HostShape::Dir {
            mare_shared::private::create_dir_all(&entry.path)
                .map_err(|e| format!("cannot create protected host dir {}: {e}", entry.path.display()))?;
            let meta = std::fs::symlink_metadata(&entry.path)
                .map_err(|e| format!("cannot stat protected host dir {}: {e}", entry.path.display()))?;
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                return Err(format!("protected host dir {} is not a real directory", entry.path.display()));
            }
        }
    }
    Ok(())
}

/// Tri-state status of `entry` without using `.exists()`.
/// Any permission, I/O, non-directory, or symlink condition returns `SecretStatus::Unknown`.
pub fn secret_status(entry: &HostPath) -> SecretStatus {
    let meta = match std::fs::symlink_metadata(&entry.path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SecretStatus::Absent,
        Err(_) => return SecretStatus::Unknown,
    };
    if meta.file_type().is_symlink() {
        return SecretStatus::Unknown;
    }
    match entry.shape {
        HostShape::Dir => {
            if !meta.file_type().is_dir() {
                return SecretStatus::Unknown;
            }
            let mut rd = match std::fs::read_dir(&entry.path) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SecretStatus::Absent,
                Err(_) => return SecretStatus::Unknown,
            };
            match rd.next() {
                None => SecretStatus::Absent,
                Some(Ok(_)) => SecretStatus::Present,
                Some(Err(_)) => SecretStatus::Unknown,
            }
        }
        HostShape::File => {
            if meta.file_type().is_file() {
                SecretStatus::Present
            } else {
                SecretStatus::Unknown
            }
        }
    }
}

/// Whether any `Refuse` `Dir` entry in [`host_protected`] resolves under `/proc` or `/dev`
/// where bubblewrap cannot mount a private tmpfs mask.
pub fn host_protected_unmaskable() -> bool {
    host_protected().into_iter().any(|entry| {
        entry.shape == HostShape::Dir && entry.on_gap == GapAction::Refuse && {
            crate::realpath::resolve(&entry.path)
                .map_or(true, |resolved| resolved.starts_with("/proc") || resolved.starts_with("/dev"))
        }
    })
}

/// Whether the active sandbox backend can enforce `fs_host_protect` for the current host paths.
pub fn host_protect_available() -> bool {
    let b = backend();
    #[cfg(target_os = "linux")]
    {
        b.caps().contains(&Cap::FsHostProtect) && !(b == super::Backend::Bwrap && host_protected_unmaskable())
    }
    #[cfg(not(target_os = "linux"))]
    {
        b.caps().contains(&Cap::FsHostProtect)
    }
}

/// Producer-side gate: refuse creating or enabling the host runtime token when the active
/// backend cannot hide it from sandboxed commands, unless `accepted_partial` is true.
pub fn producer_gate(accepted_partial: bool) -> Result<(), String> {
    if host_protect_available() || accepted_partial {
        return Ok(());
    }
    let b = backend();
    Err(format!(
        "{SANDBOX_INSUFFICIENT}: {} cannot hide the remote token from sandboxed commands; install bubblewrap (Linux), \
         complete enhanced sandbox setup (Windows), or explicitly accept fs_host_protect \
         (note: full-access, disabled permissions, approved unconfined escalations, custom static tokens, \
         raw Seatbelt profiles, and pre-existing processes on partial backends are not protected)",
        b.name()
    ))
}

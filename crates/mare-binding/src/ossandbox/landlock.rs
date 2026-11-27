//! Linux Landlock: the ABI probe and a policy's grants.

use std::path::PathBuf;

use super::Policy;

#[cfg(target_os = "linux")]
pub fn landlock_abi() -> Option<i64> {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION) → ABI version
    let v = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, 1u32) };
    (v >= 1).then_some(v)
}
#[cfg(all(unix, not(target_os = "linux")))]
pub fn landlock_abi() -> Option<i64> {
    None
}

impl Policy {
    /// Landlock grants: read everywhere (or only the allowed roots under restricted reads,
    /// [`crate::read_mask::landlock_grants`]), write `/dev/null` (+ `/tmp` and the roots under
    /// workspace-write).
    pub fn landlock_grants(&self) -> crate::read_mask::LandlockGrants {
        let mut rw: Vec<PathBuf> = vec!["/dev/null".into()];
        rw.extend(self.writable.iter().cloned());
        crate::read_mask::landlock_grants(self, rw)
    }
}

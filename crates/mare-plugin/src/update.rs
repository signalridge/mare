//! Mare release asset matrix and self-update orchestration built on `mare_binding::update`
//! and `crate::install`.

use std::path::Path;

use anyhow::{Context, Result, bail};
pub use mare_binding::update::{
    LatestRelease, REEXEC_ENV, ReleaseAsset, ReleaseSource, ReleaseTarget, SelectedArchive, StagedUpdate, SwapApplied,
    staging_dir,
};

use crate::install::{InstallChannel, InstallInfo, RELEASE_API_URL, RELEASE_DOWNLOAD_BASE_URL};

/// Candidate release targets in preference order (musl first on Linux with GNU fallback for
/// older releases; mirrors `install.sh` / `install.ps1` and `.github/workflows/release.yaml`).
pub fn release_targets_for(os: &str, arch: &str) -> Result<Vec<ReleaseTarget>> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(vec![
            ReleaseTarget { triple: "x86_64-unknown-linux-musl", archive_ext: "tar.gz", binary_name: "mare" },
            ReleaseTarget { triple: "x86_64-unknown-linux-gnu", archive_ext: "tar.gz", binary_name: "mare" },
        ]),
        ("linux", "aarch64") => Ok(vec![
            ReleaseTarget { triple: "aarch64-unknown-linux-musl", archive_ext: "tar.gz", binary_name: "mare" },
            ReleaseTarget { triple: "aarch64-unknown-linux-gnu", archive_ext: "tar.gz", binary_name: "mare" },
        ]),
        ("macos", "aarch64") => {
            Ok(vec![ReleaseTarget { triple: "aarch64-apple-darwin", archive_ext: "tar.gz", binary_name: "mare" }])
        }
        ("macos", "x86_64") => {
            Ok(vec![ReleaseTarget { triple: "x86_64-apple-darwin", archive_ext: "tar.gz", binary_name: "mare" }])
        }
        ("windows", "x86_64") => {
            Ok(vec![ReleaseTarget { triple: "x86_64-pc-windows-msvc", archive_ext: "zip", binary_name: "mare.exe" }])
        }
        ("windows", "aarch64") => {
            Ok(vec![ReleaseTarget { triple: "aarch64-pc-windows-msvc", archive_ext: "zip", binary_name: "mare.exe" }])
        }
        _ => bail!("unsupported platform {os}/{arch} for standalone binary releases"),
    }
}

/// Primary platform release target matching current Mare releases.
pub fn release_target_for(os: &str, arch: &str) -> Result<ReleaseTarget> {
    release_targets_for(os, arch).map(|v| v[0])
}

/// Mare's release source configuration for a resolved API URL.
pub fn mare_release_source(api_url: &str) -> ReleaseSource<'_> {
    ReleaseSource {
        api_url,
        default_api_url: RELEASE_API_URL,
        default_download_base_url: RELEASE_DOWNLOAD_BASE_URL,
        asset_prefix: "mare",
        binary_stem: "mare",
    }
}

/// Resolve Mare's GitHub release API URL (honoring `MARE_UPDATE_URL`, then config, then default).
pub fn resolve_api_url(config_url: Option<&str>) -> String {
    mare_binding::update::resolve_api_url(config_url, RELEASE_API_URL)
}

/// Fetch latest Mare release metadata using `mare_shared::version::is_newer_release`.
pub async fn check_latest_release(api_url: &str, current_version: &str) -> Result<LatestRelease> {
    mare_binding::update::check_latest_release(api_url, current_version).await
}

/// Select the preferred Mare release archive listed in `checksums_txt` (and `assets` when provided).
pub fn select_release_archive(
    tag: &str,
    os: &str,
    arch: &str,
    api_url: &str,
    assets: &[ReleaseAsset],
    checksums_txt: &str,
) -> Result<SelectedArchive> {
    let candidates = release_targets_for(os, arch)?;
    mare_binding::update::select_release_archive(&mare_release_source(api_url), tag, &candidates, assets, checksums_txt)
}

/// Download, verify checksum, extract, smoke-check, and stage a standalone Mare release.
pub async fn stage_release(
    release: &LatestRelease,
    api_url: &str,
    staging_dir_path: &Path,
    target_exe: &Path,
    channel: InstallChannel,
) -> Result<StagedUpdate> {
    if channel.is_managed() {
        bail!("refusing to stage self-update for managed install channel `{}`", channel.as_str());
    }
    let candidates = release_targets_for(std::env::consts::OS, std::env::consts::ARCH)?;
    mare_binding::update::stage_release(
        &mare_release_source(api_url),
        &candidates,
        release,
        staging_dir_path,
        target_exe,
        channel.is_managed(),
    )
    .await
}

/// Apply any pending staged standalone Mare update for the current executable on startup.
pub fn apply_staged_update() -> Result<Option<SwapApplied>> {
    if std::env::var_os(REEXEC_ENV).is_some() {
        return Ok(None);
    }
    let dir = staging_dir();
    if !dir.join(mare_binding::update::STAGED_FILE_NAME).is_file() {
        return Ok(None);
    }
    let info = InstallInfo::detect_current();
    let exe = std::env::current_exe().context("resolving current_exe")?;
    mare_binding::update::apply_staged_update_with(
        &dir,
        &exe,
        env!("CARGO_PKG_VERSION"),
        info.managed,
        mare_binding::update::smoke_check_binary,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_linux_musl_for_both_arches_with_gnu_fallback() {
        let sha_x86_musl = "1".repeat(64);
        let sha_arm_musl = "2".repeat(64);
        let sha_x86_gnu = "3".repeat(64);
        let checksums = format!(
            "{sha_x86_musl}  mare-v1.2.0-x86_64-unknown-linux-musl.tar.gz\n\
             {sha_arm_musl}  mare-v1.2.0-aarch64-unknown-linux-musl.tar.gz\n\
             {sha_x86_gnu}  mare-v1.2.0-x86_64-unknown-linux-gnu.tar.gz\n"
        );
        let x86 = select_release_archive("v1.2.0", "linux", "x86_64", RELEASE_API_URL, &[], &checksums).unwrap();
        assert_eq!(x86.archive_name, "mare-v1.2.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(x86.expected_sha, sha_x86_musl);

        let arm = select_release_archive("v1.2.0", "linux", "aarch64", RELEASE_API_URL, &[], &checksums).unwrap();
        assert_eq!(arm.archive_name, "mare-v1.2.0-aarch64-unknown-linux-musl.tar.gz");
        assert_eq!(arm.expected_sha, sha_arm_musl);

        let gnu_only = format!("{sha_x86_gnu}  mare-v1.0.0-x86_64-unknown-linux-gnu.tar.gz\n");
        let fallback = select_release_archive("v1.0.0", "linux", "x86_64", RELEASE_API_URL, &[], &gnu_only).unwrap();
        assert_eq!(fallback.archive_name, "mare-v1.0.0-x86_64-unknown-linux-gnu.tar.gz");
    }
}

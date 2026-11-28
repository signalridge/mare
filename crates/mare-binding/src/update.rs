//! Generic release lookup, checksum verification, archive extraction, staging, and
//! next-start binary swap primitives (product-specific release matrices and channel rules
//! belong to the update component, e.g. `mare_plugin::update` and `mare_plugin::install`).

use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::{DeflateDecoder, GzDecoder};
use mare_shared::version::{is_newer_release, normalize_tag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Environment variable guarding against recursive re-exec after swapping a staged binary.
pub const REEXEC_ENV: &str = "MARE_UPDATE_REEXEC";

/// Optional override URL for testing self-update against a local mock release server.
pub const UPDATE_URL_ENV: &str = "MARE_UPDATE_URL";

/// Optional override base URL for downloading release archives and checksums.
pub const UPDATE_DOWNLOAD_URL_ENV: &str = "MARE_UPDATE_DOWNLOAD_URL";

pub const STAGED_FILE_NAME: &str = "staged.json";
const SWAP_LOCK_FILE_NAME: &str = "swap.lock";
const MAX_EXTRACT_BYTES: u64 = 256 * 1024 * 1024;
const SWAP_MUTEX_STALE_SECS: u64 = 120;
const STAGED_PUBLISH_GRACE_SECS: u64 = 10;
const SMOKE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Platform target descriptor supplied by the product's release matrix.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ReleaseTarget {
    pub triple: &'static str,
    pub archive_ext: &'static str,
    pub binary_name: &'static str,
}

/// Product release endpoint and archive naming parameters supplied by the caller.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ReleaseSource<'a> {
    pub api_url: &'a str,
    pub default_api_url: &'a str,
    pub default_download_base_url: &'a str,
    pub asset_prefix: &'a str,
    pub binary_stem: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LatestRelease {
    pub tag: String,
    pub version: String,
    pub current: String,
    pub newer: bool,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct StagedUpdate {
    pub version: String,
    pub tag: String,
    pub target_exe: PathBuf,
    pub exe_file_name: String,
    pub sha256: String,
    pub exe_size: u64,
    pub staged_at: String,
}

pub fn staging_dir() -> PathBuf {
    mare_shared::paths::state_dir().join("update-staging")
}

/// Resolve the release API URL (honoring `MARE_UPDATE_URL` env override, else caller config, else `default_api_url`).
pub fn resolve_api_url(config_url: Option<&str>, default_api_url: &str) -> String {
    if let Ok(u) = std::env::var(UPDATE_URL_ENV)
        && !u.trim().is_empty()
    {
        return u.trim().to_string();
    }
    config_url.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(default_api_url).to_string()
}

/// Fetch release metadata JSON from `api_url` and evaluate release-channel SemVer precedence via
/// [`mare_shared::version::is_newer_release`]. Honors the offline test guard (`MARE_TEST_OFFLINE`).
pub async fn check_latest_release(api_url: &str, current_version: &str) -> Result<LatestRelease> {
    if let Err(reason) = crate::http::check_offline_url(api_url) {
        bail!("{reason}");
    }
    #[derive(Deserialize)]
    struct ReleasePayload {
        tag_name: String,
        #[serde(default)]
        assets: Vec<ReleaseAsset>,
    }
    let client = crate::http::shared_client()?;
    let resp = client
        .get(api_url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("fetching {api_url}"))?;
    if !resp.status().is_success() {
        bail!("release metadata request failed: HTTP {}", resp.status());
    }
    let payload: ReleasePayload = resp.json().await.context("decoding release JSON")?;
    let version = normalize_tag(&payload.tag_name).to_string();
    let newer = is_newer_release(&payload.tag_name, current_version);
    Ok(LatestRelease {
        tag: payload.tag_name,
        version,
        current: current_version.to_string(),
        newer,
        assets: payload.assets,
    })
}

async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    if let Err(reason) = crate::http::check_offline_url(url) {
        bail!("{reason}");
    }
    let resp = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    if !resp.status().is_success() {
        bail!("download {url} failed: HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await.with_context(|| format!("reading body from {url}"))?;
    Ok(bytes.to_vec())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

/// Parse standard `sha256sum` / `shasum -a 256` format (`<64-hex>  <filename>`) for `filename`.
pub fn parse_checksum_for(checksums_txt: &str, filename: &str) -> Option<String> {
    for line in checksums_txt.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let hex = parts.next()?.trim();
        let rest = parts.next()?.trim().trim_start_matches('*');
        let rest = rest.strip_prefix("./").unwrap_or(rest);
        if rest == filename && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

fn entry_matches_bin(entry_path: &str, bin_name: &str) -> bool {
    let clean = entry_path.trim_end_matches('/');
    if clean.is_empty() || clean.starts_with('/') || clean.split('/').any(|c| c == "..") {
        return false;
    }
    Path::new(clean).file_name().and_then(|s| s.to_str()) == Some(bin_name)
}

/// Extract `bin_name` (by safe relative basename) from a `.tar.gz` release archive.
pub fn extract_binary_from_tar_gz(archive_bytes: &[u8], bin_name: &str) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    loop {
        match decoder.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading tar header"),
        }
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let typeflag = header[156];
        let size_field = std::str::from_utf8(&header[124..136]).unwrap_or("").trim_end_matches(['\0', ' ']);
        let size = u64::from_str_radix(size_field, 8).context("invalid tar entry size")?;
        if size > MAX_EXTRACT_BYTES {
            bail!("tar entry exceeds maximum allowed size ({MAX_EXTRACT_BYTES} bytes)");
        }
        let name_raw = read_cstr(&header[0..100]);
        let prefix = read_cstr(&header[345..500]);
        let raw_name = if let Some(ln) = long_name.take() {
            ln
        } else if !prefix.is_empty() {
            format!("{prefix}/{name_raw}")
        } else {
            name_raw
        };
        let mut content = vec![0u8; size as usize];
        decoder.read_exact(&mut content).context("reading tar entry payload")?;
        let pad = (512 - (size % 512)) % 512;
        if pad > 0 {
            let mut sink = [0u8; 512];
            decoder.read_exact(&mut sink[..pad as usize]).context("reading tar entry padding")?;
        }
        if typeflag == b'L' {
            long_name = Some(String::from_utf8_lossy(&content).trim_end_matches('\0').to_string());
            continue;
        }
        if matches!(typeflag, b'0' | 0) && entry_matches_bin(&raw_name, bin_name) {
            return Ok(content);
        }
    }
    bail!("binary {bin_name} not found in tar.gz archive")
}

fn read_cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Extract `bin_name` (by safe relative basename) from a `.zip` release archive (stored or deflate).
pub fn extract_binary_from_zip(archive_bytes: &[u8], bin_name: &str) -> Result<Vec<u8>> {
    fn le_u16(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }
    fn le_u32(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    if archive_bytes.len() < 22 {
        bail!("invalid zip archive (too short)");
    }
    let min_offset = archive_bytes.len().saturating_sub(65_557);
    let eocd_pos = (min_offset..=archive_bytes.len() - 22)
        .rev()
        .find(|&i| &archive_bytes[i..i + 4] == b"PK\x05\x06")
        .context("zip end-of-central-directory not found")?;
    let total_entries = le_u16(&archive_bytes[eocd_pos + 10..eocd_pos + 12]) as usize;
    let cd_offset = le_u32(&archive_bytes[eocd_pos + 16..eocd_pos + 20]) as usize;
    let mut pos = cd_offset;
    for _ in 0..total_entries {
        if pos + 46 > archive_bytes.len() || &archive_bytes[pos..pos + 4] != b"PK\x01\x02" {
            bail!("corrupt zip central directory");
        }
        let method = le_u16(&archive_bytes[pos + 10..pos + 12]);
        let comp_size = le_u32(&archive_bytes[pos + 20..pos + 24]) as usize;
        let uncomp_size = le_u32(&archive_bytes[pos + 24..pos + 28]) as usize;
        let name_len = le_u16(&archive_bytes[pos + 28..pos + 30]) as usize;
        let extra_len = le_u16(&archive_bytes[pos + 30..pos + 32]) as usize;
        let comment_len = le_u16(&archive_bytes[pos + 32..pos + 34]) as usize;
        let local_header_offset = le_u32(&archive_bytes[pos + 42..pos + 46]) as usize;
        let name_start = pos + 46;
        let name_end = name_start.checked_add(name_len).context("zip name overflow")?;
        if name_end > archive_bytes.len() {
            bail!("truncated zip entry name");
        }
        let name = std::str::from_utf8(&archive_bytes[name_start..name_end]).unwrap_or("");
        pos = name_end + extra_len + comment_len;

        if !entry_matches_bin(name, bin_name) {
            continue;
        }
        if uncomp_size as u64 > MAX_EXTRACT_BYTES {
            bail!("zip entry exceeds maximum allowed size");
        }
        if local_header_offset + 30 > archive_bytes.len()
            || &archive_bytes[local_header_offset..local_header_offset + 4] != b"PK\x03\x04"
        {
            bail!("invalid zip local file header");
        }
        let l_name_len = le_u16(&archive_bytes[local_header_offset + 26..local_header_offset + 28]) as usize;
        let l_extra_len = le_u16(&archive_bytes[local_header_offset + 28..local_header_offset + 30]) as usize;
        let data_start = local_header_offset + 30 + l_name_len + l_extra_len;
        let data_end = data_start.checked_add(comp_size).context("zip payload overflow")?;
        if data_end > archive_bytes.len() {
            bail!("truncated zip payload");
        }
        let compressed = &archive_bytes[data_start..data_end];
        let out = match method {
            0 => compressed.to_vec(),
            8 => {
                let mut dec = DeflateDecoder::new(Cursor::new(compressed));
                let mut out = Vec::with_capacity(uncomp_size);
                dec.by_ref().take(MAX_EXTRACT_BYTES + 1).read_to_end(&mut out)?;
                if out.len() as u64 > MAX_EXTRACT_BYTES {
                    bail!("deflated entry exceeds maximum allowed size");
                }
                out
            }
            m => bail!("unsupported zip compression method {m}"),
        };
        return Ok(out);
    }
    bail!("binary {bin_name} not found in zip archive")
}

fn find_asset_url<'a>(assets: &'a [ReleaseAsset], filename: &str) -> Option<&'a str> {
    assets.iter().find(|a| a.name == filename).map(|a| a.browser_download_url.as_str())
}

fn default_asset_url(source: &ReleaseSource<'_>, tag: &str, filename: &str) -> String {
    if let Ok(base) = std::env::var(UPDATE_DOWNLOAD_URL_ENV)
        && !base.trim().is_empty()
    {
        return format!("{}/{tag}/{filename}", base.trim().trim_end_matches('/'));
    }
    if source.api_url != source.default_api_url
        && let Ok(parsed) = url::Url::parse(source.api_url)
    {
        let origin = parsed.origin().ascii_serialization();
        return format!("{origin}/download/{tag}/{filename}");
    }
    format!("{}/{tag}/{filename}", source.default_download_base_url.trim_end_matches('/'))
}

fn staged_exe_filename(binary_stem: &str, version: &str, bin_name: &str) -> String {
    if bin_name.ends_with(".exe") { format!("{binary_stem}-{version}.exe") } else { format!("{binary_stem}-{version}") }
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod +x {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Verify extracted binary works via `<bin> --version` containing `version`.
pub fn smoke_check_binary(bin_path: &Path, expected_version: &str) -> Result<()> {
    let mut cmd = std::process::Command::new(bin_path);
    cmd.arg("--version")
        .env(REEXEC_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("spawning {} --version", bin_path.display()))?;
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("staged binary {} --version exited with {status}", bin_path.display());
            }
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_string(&mut out);
            }
            if !out.contains(expected_version) {
                bail!(
                    "staged binary --version output {:?} did not contain expected version {expected_version}",
                    out.trim()
                );
            }
            return Ok(());
        }
        if start.elapsed() >= SMOKE_CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("staged binary --version timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SelectedArchive {
    pub archive_name: String,
    pub archive_url: String,
    pub expected_sha: String,
    pub ext: &'static str,
    pub bin_name: &'static str,
}

/// Select the preferred release archive listed in `checksums_txt` (and `assets` when provided)
/// from caller-supplied `candidates`.
pub fn select_release_archive(
    source: &ReleaseSource<'_>,
    tag: &str,
    candidates: &[ReleaseTarget],
    assets: &[ReleaseAsset],
    checksums_txt: &str,
) -> Result<SelectedArchive> {
    for target in candidates {
        let archive_name = format!("{}-{tag}-{}.{}", source.asset_prefix, target.triple, target.archive_ext);
        let Some(expected_sha) = parse_checksum_for(checksums_txt, &archive_name) else {
            continue;
        };
        let asset_url = find_asset_url(assets, &archive_name);
        if !assets.is_empty() && asset_url.is_none() {
            continue;
        }
        let archive_url =
            asset_url.map(str::to_string).unwrap_or_else(|| default_asset_url(source, tag, &archive_name));
        return Ok(SelectedArchive {
            archive_name,
            archive_url,
            expected_sha,
            ext: target.archive_ext,
            bin_name: target.binary_name,
        });
    }
    let tried: Vec<String> =
        candidates.iter().map(|t| format!("{}-{tag}-{}.{}", source.asset_prefix, t.triple, t.archive_ext)).collect();
    bail!("no matching release asset in checksums.txt (tried: {})", tried.join(", "))
}

/// Download, verify checksum, extract, smoke-check, and stage a standalone release for next start.
pub async fn stage_release(
    source: &ReleaseSource<'_>,
    candidates: &[ReleaseTarget],
    release: &LatestRelease,
    staging_dir_path: &Path,
    target_exe: &Path,
    managed: bool,
) -> Result<StagedUpdate> {
    if managed {
        bail!("refusing to stage self-update for a package-manager-managed installation");
    }
    if !release.newer {
        bail!("release {} is not newer than current version {}", release.tag, release.current);
    }
    let checksums_url = find_asset_url(&release.assets, "checksums.txt")
        .map(str::to_string)
        .unwrap_or_else(|| default_asset_url(source, &release.tag, "checksums.txt"));

    let client = crate::http::shared_client()?;
    let checksums_bytes = download_bytes(client, &checksums_url).await?;
    let checksums_txt = String::from_utf8(checksums_bytes).context("decoding checksums.txt")?;
    let selected = select_release_archive(source, &release.tag, candidates, &release.assets, &checksums_txt)?;

    let archive_bytes = download_bytes(client, &selected.archive_url).await?;
    let actual_archive_sha = sha256_hex(&archive_bytes);
    if actual_archive_sha != selected.expected_sha {
        bail!(
            "SHA-256 mismatch for {}: expected {}, got {actual_archive_sha}",
            selected.archive_name,
            selected.expected_sha
        );
    }

    let bin_bytes = if selected.ext == "zip" {
        extract_binary_from_zip(&archive_bytes, selected.bin_name)?
    } else {
        extract_binary_from_tar_gz(&archive_bytes, selected.bin_name)?
    };
    stage_binary_bytes(
        &bin_bytes,
        source.binary_stem,
        &release.tag,
        &release.version,
        selected.bin_name,
        staging_dir_path,
        target_exe,
        managed,
        smoke_check_binary,
    )
}

/// Write verified binary bytes into `staging_dir_path` and atomically publish `staged.json`.
#[allow(clippy::too_many_arguments)]
pub fn stage_binary_bytes<F>(
    bin_bytes: &[u8],
    binary_stem: &str,
    tag: &str,
    version: &str,
    bin_name: &str,
    staging_dir_path: &Path,
    target_exe: &Path,
    managed: bool,
    smoke_fn: F,
) -> Result<StagedUpdate>
where
    F: FnOnce(&Path, &str) -> Result<()>,
{
    if managed {
        bail!("refusing to stage binary for a package-manager-managed installation");
    }
    if bin_bytes.is_empty() {
        bail!("extracted binary is empty");
    }
    std::fs::create_dir_all(staging_dir_path)?;
    let exe_file_name = staged_exe_filename(binary_stem, version, bin_name);
    let temp_name = format!("{exe_file_name}.part.{}", std::process::id());
    let temp_path = staging_dir_path.join(&temp_name);
    let final_path = staging_dir_path.join(&exe_file_name);
    {
        let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(&temp_path)?;
        f.write_all(bin_bytes)?;
        f.sync_all()?;
    }
    set_executable(&temp_path)?;
    if let Err(e) = smoke_fn(&temp_path, version) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }
    std::fs::rename(&temp_path, &final_path)?;
    let sha256 = sha256_hex(bin_bytes);
    let canonical_target = dunce::canonicalize(target_exe).unwrap_or_else(|_| target_exe.to_path_buf());
    let staged_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let staged = StagedUpdate {
        version: version.to_string(),
        tag: tag.to_string(),
        target_exe: canonical_target,
        exe_file_name,
        sha256,
        exe_size: bin_bytes.len() as u64,
        staged_at,
    };
    let json = serde_json::to_vec_pretty(&staged)?;
    write_atomic(&staging_dir_path.join(STAGED_FILE_NAME), &json)?;
    Ok(staged)
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SwapApplied {
    pub from_version: String,
    pub to_version: String,
    pub exe_path: PathBuf,
}

struct SwapLockGuard {
    path: PathBuf,
}

impl Drop for SwapLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_swap_lock(dir: &Path) -> Option<SwapLockGuard> {
    std::fs::create_dir_all(dir).ok()?;
    let lock_path = dir.join(SWAP_LOCK_FILE_NAME);
    for attempt in 0..2 {
        match OpenOptions::new().create_new(true).write(true).open(&lock_path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return Some(SwapLockGuard { path: lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mt| SystemTime::now().duration_since(mt).ok())
                    .is_some_and(|age| age > Duration::from_secs(SWAP_MUTEX_STALE_SECS));
                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                return None;
            }
            Err(_) => return None,
        }
    }
    None
}

fn is_plain_filename(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).components().count() == 1
        && Path::new(name).file_name().and_then(|s| s.to_str()) == Some(name)
}

fn cleanup_stale_backups(exe_dir: &Path, exe_base: &str) {
    let Ok(entries) = std::fs::read_dir(exe_dir) else { return };
    let prefix = format!("{exe_base}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix(&prefix).and_then(|s| s.strip_suffix(".bak"))
            && (rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn rename_with_retry<R>(src: &Path, dst: &Path, rename_fn: &mut R) -> std::io::Result<()>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 0..MAX_ATTEMPTS {
        match rename_fn(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < MAX_ATTEMPTS && is_transient_lock_error(&e) => {
                // sleep-ok: bounded Windows sharing-violation / AV lock retry backoff
                std::thread::sleep(Duration::from_millis(25 * (attempt as u64 + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn is_transient_lock_error(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock)
        || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// Apply any valid staged update for `exe_path` atomically (with `.bak` rollback).
/// Never swaps when `managed` is `true`.
pub fn apply_staged_update_with<F>(
    staging_dir_path: &Path,
    exe_path: &Path,
    current_version: &str,
    managed: bool,
    smoke_fn: F,
) -> Result<Option<SwapApplied>>
where
    F: FnOnce(&Path, &str) -> Result<()>,
{
    apply_staged_update_with_rename(staging_dir_path, exe_path, current_version, managed, smoke_fn, |a, b| {
        std::fs::rename(a, b)
    })
}

/// Testable entry point allowing callers to inject custom rename behavior (e.g. Windows sharing lock simulation).
pub fn apply_staged_update_with_rename<F, R>(
    staging_dir_path: &Path,
    exe_path: &Path,
    current_version: &str,
    managed: bool,
    smoke_fn: F,
    mut rename_fn: R,
) -> Result<Option<SwapApplied>>
where
    F: FnOnce(&Path, &str) -> Result<()>,
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if managed {
        return Ok(None);
    }
    let state_file = staging_dir_path.join(STAGED_FILE_NAME);
    if !state_file.is_file() {
        return Ok(None);
    }
    let Some(_lock) = acquire_swap_lock(staging_dir_path) else {
        return Ok(None);
    };
    let claimed_path = staging_dir_path.join(format!("{STAGED_FILE_NAME}.swap.{}", std::process::id()));
    let young = std::fs::metadata(&state_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .is_some_and(|age| age <= Duration::from_secs(STAGED_PUBLISH_GRACE_SECS));
    if std::fs::rename(&state_file, &claimed_path).is_err() {
        return Ok(None);
    }

    let raw = match std::fs::read(&claimed_path) {
        Ok(b) => b,
        Err(_) => {
            if young {
                let _ = std::fs::rename(&claimed_path, &state_file);
            } else {
                let _ = std::fs::remove_file(&claimed_path);
            }
            return Ok(None);
        }
    };
    let staged: StagedUpdate = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::fs::remove_file(&claimed_path);
            return Ok(None);
        }
    };

    if !is_plain_filename(&staged.exe_file_name)
        || staged.exe_size == 0
        || !is_newer_release(&staged.version, current_version)
    {
        let _ = std::fs::remove_file(&claimed_path);
        return Ok(None);
    }
    let canon_exe = dunce::canonicalize(exe_path).unwrap_or_else(|_| exe_path.to_path_buf());
    let canon_target = dunce::canonicalize(&staged.target_exe).unwrap_or_else(|_| staged.target_exe.clone());
    if canon_exe != canon_target {
        // Staged for a different binary; restore state file cleanly.
        let _ = std::fs::rename(&claimed_path, &state_file);
        return Ok(None);
    }

    let staged_bin = staging_dir_path.join(&staged.exe_file_name);
    let meta = match std::fs::metadata(&staged_bin) {
        Ok(m) if m.len() == staged.exe_size => m,
        _ => {
            let _ = std::fs::remove_file(&claimed_path);
            let _ = std::fs::remove_file(&staged_bin);
            return Ok(None);
        }
    };
    let _ = meta;
    let actual_sha = match sha256_file(&staged_bin) {
        Ok(h) => h,
        Err(_) => {
            let _ = std::fs::remove_file(&claimed_path);
            return Ok(None);
        }
    };
    if actual_sha != staged.sha256 {
        let _ = std::fs::remove_file(&claimed_path);
        let _ = std::fs::remove_file(&staged_bin);
        bail!("staged binary SHA-256 mismatch (expected {}, got {actual_sha})", staged.sha256);
    }

    if let Err(e) = smoke_fn(&staged_bin, &staged.version) {
        let _ = std::fs::remove_file(&claimed_path);
        let _ = std::fs::remove_file(&staged_bin);
        return Err(e);
    }

    let exe_dir = exe_path.parent().context("executable has no parent directory")?;
    let exe_base = exe_path.file_name().and_then(|s| s.to_str()).unwrap_or("binary");
    cleanup_stale_backups(exe_dir, exe_base);

    let bak_path = exe_dir.join(format!("{exe_base}.{}.bak", std::process::id()));
    let next_tmp = exe_dir.join(format!("{exe_base}.staged.{}", std::process::id()));
    // Copy instead of moving out of staging so failure keeps staged_bin + staged.json intact for retry.
    if let Err(e) =
        std::fs::copy(&staged_bin, &next_tmp).and_then(|_| set_executable(&next_tmp).map_err(std::io::Error::other))
    {
        let _ = std::fs::remove_file(&next_tmp);
        let _ = std::fs::rename(&claimed_path, &state_file);
        return Err(e.into());
    }

    let had_existing = exe_path.exists();
    if had_existing && let Err(e) = rename_with_retry(exe_path, &bak_path, &mut rename_fn) {
        let _ = std::fs::remove_file(&next_tmp);
        let _ = std::fs::rename(&claimed_path, &state_file);
        return Err(e).with_context(|| format!("backing up {} to {}", exe_path.display(), bak_path.display()));
    }
    if let Err(e) = rename_with_retry(&next_tmp, exe_path, &mut rename_fn) {
        let _ = std::fs::remove_file(&next_tmp);
        if had_existing {
            let _ = rename_with_retry(&bak_path, exe_path, &mut rename_fn);
        }
        let _ = std::fs::rename(&claimed_path, &state_file);
        return Err(e).with_context(|| format!("moving staged binary into {}", exe_path.display()));
    }

    let _ = std::fs::remove_file(&claimed_path);
    let _ = std::fs::remove_file(&staged_bin);
    if had_existing {
        let _ = std::fs::remove_file(&bak_path);
    }

    Ok(Some(SwapApplied {
        from_version: current_version.to_string(),
        to_version: staged.version,
        exe_path: exe_path.to_path_buf(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::{DeflateEncoder, GzEncoder};

    fn make_tar_gz(entry_name: &str, content: &[u8]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        let mut header = [0u8; 512];
        let name_bytes = entry_name.as_bytes();
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_oct = format!("{:011o}\0", content.len());
        header[124..136].copy_from_slice(size_oct.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(cksum.as_bytes());
        tar_buf.extend_from_slice(&header);
        tar_buf.extend_from_slice(content);
        let pad = (512 - (content.len() % 512)) % 512;
        tar_buf.resize(tar_buf.len() + pad, 0);
        tar_buf.resize(tar_buf.len() + 1024, 0);

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_buf).unwrap();
        gz.finish().unwrap()
    }

    fn make_zip(entry_name: &str, content: &[u8]) -> Vec<u8> {
        let mut def = DeflateEncoder::new(Vec::new(), Compression::default());
        def.write_all(content).unwrap();
        let compressed = def.finish().unwrap();

        let mut out = Vec::new();
        // Local file header
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes()); // deflate
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(entry_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(entry_name.as_bytes());
        out.extend_from_slice(&compressed);

        let cd_offset = out.len() as u32;
        // Central directory header
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(entry_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        out.extend_from_slice(entry_name.as_bytes());

        let cd_size = (out.len() as u32) - cd_offset;
        // EOCD
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn extracts_binary_from_tar_gz_and_zip_and_rejects_traversal() {
        let payload = b"#!/bin/sh\necho app 1.0.2\n";
        let tgz = make_tar_gz("app-v1.0.2-x86_64-unknown-linux-gnu/app", payload);
        assert_eq!(extract_binary_from_tar_gz(&tgz, "app").unwrap(), payload);

        let zip_bytes = make_zip("app-v1.0.2-x86_64-pc-windows-msvc/app.exe", payload);
        assert_eq!(extract_binary_from_zip(&zip_bytes, "app.exe").unwrap(), payload);

        let bad_tgz = make_tar_gz("../evil/app", payload);
        assert!(extract_binary_from_tar_gz(&bad_tgz, "app").is_err());
    }

    #[test]
    fn checksum_parser_matches_exact_filename() {
        let hash = "a".repeat(64);
        let txt = format!("{hash}  ./app-v1.0.2-linux.tar.gz\n");
        assert_eq!(parse_checksum_for(&txt, "app-v1.0.2-linux.tar.gz").as_deref(), Some(hash.as_str()));
        assert_eq!(parse_checksum_for(&txt, "other.tar.gz"), None);
    }

    #[test]
    fn stage_and_apply_swap_respects_managed_flag_checksum_and_semver_release_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("app");
        std::fs::write(&exe, b"old-binary").unwrap();

        // Managed install must refuse staging
        assert!(
            stage_binary_bytes(b"new", "app", "v1.0.2", "1.0.2", "app", &staging, &exe, true, |_, _| Ok(())).is_err()
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");

        // Stable 1.0.0 must NOT apply a staged prerelease 1.0.1-rc.1 (shared SemVer release policy)
        stage_binary_bytes(b"rc-binary", "app", "v1.0.1-rc.1", "1.0.1-rc.1", "app", &staging, &exe, false, |_, _| {
            Ok(())
        })
        .unwrap();
        assert!(apply_staged_update_with(&staging, &exe, "1.0.0", false, |_, _| Ok(())).unwrap().is_none());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");

        // Prerelease 1.0.0-rc.1 DOES apply stable 1.0.0
        stage_binary_bytes(b"stable-100", "app", "v1.0.0", "1.0.0", "app", &staging, &exe, false, |_, _| Ok(()))
            .unwrap();
        let promoted = apply_staged_update_with(&staging, &exe, "1.0.0-rc.1", false, |_, _| Ok(())).unwrap().unwrap();
        assert_eq!(promoted.to_version, "1.0.0");
        assert_eq!(std::fs::read(&exe).unwrap(), b"stable-100");
        std::fs::write(&exe, b"old-binary").unwrap();

        // Stage for standalone
        let staged = stage_binary_bytes(
            b"new-binary-payload",
            "app",
            "v1.0.2",
            "1.0.2",
            "app",
            &staging,
            &exe,
            false,
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(staged.version, "1.0.2");

        // Managed flag on swap refuses and preserves binary
        assert!(apply_staged_update_with(&staging, &exe, "1.0.0", true, |_, _| Ok(())).unwrap().is_none());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");

        // Corrupt checksum fails and clears stage without touching exe
        let staged_file = staging.join(&staged.exe_file_name);
        std::fs::write(&staged_file, b"tampered-bytes!!!!").unwrap();
        assert!(apply_staged_update_with(&staging, &exe, "1.0.0", false, |_, _| Ok(())).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");
        assert!(!staging.join(STAGED_FILE_NAME).exists());

        // Re-stage & apply succeeds cleanly
        stage_binary_bytes(
            b"new-binary-payload",
            "app",
            "v1.0.2",
            "1.0.2",
            "app",
            &staging,
            &exe,
            false,
            |_, _| Ok(()),
        )
        .unwrap();
        let applied = apply_staged_update_with(&staging, &exe, "1.0.0", false, |_, _| Ok(())).unwrap().unwrap();
        assert_eq!(applied.to_version, "1.0.2");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary-payload");
        assert!(!staging.join(STAGED_FILE_NAME).exists());
    }

    #[test]
    fn windows_swap_failure_retries_restores_discoverable_stage_and_cleans_stale_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("app.exe");
        std::fs::write(&exe, b"old-binary").unwrap();
        // Stale leftover .bak from a previous locked Windows swap
        let stale_bak = bin_dir.join("app.exe.99999.bak");
        std::fs::write(&stale_bak, b"stale").unwrap();

        let staged = stage_binary_bytes(
            b"new-binary-payload",
            "app",
            "v1.0.2",
            "1.0.2",
            "app.exe",
            &staging,
            &exe,
            false,
            |_, _| Ok(()),
        )
        .unwrap();

        // 1. Transient sharing error on first two backup-rename attempts recovers cleanly on attempt 3
        let mut attempts = 0usize;
        let transient = apply_staged_update_with_rename(
            &staging,
            &exe,
            "1.0.0",
            false,
            |_, _| Ok(()),
            |a, b| {
                attempts += 1;
                if attempts <= 2 {
                    return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sharing violation"));
                }
                std::fs::rename(a, b)
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(transient.to_version, "1.0.2");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary-payload");
        assert!(!stale_bak.exists(), "stale .bak must be swept during swap");

        // 2. Permanent failure on target-rename after backup rolls back exe AND restores discoverable stage
        std::fs::write(&exe, b"old-binary").unwrap();
        stage_binary_bytes(
            b"new-binary-payload",
            "app",
            "v1.0.2",
            "1.0.2",
            "app.exe",
            &staging,
            &exe,
            false,
            |_, _| Ok(()),
        )
        .unwrap();
        let mut calls = 0usize;
        let err = apply_staged_update_with_rename(
            &staging,
            &exe,
            "1.0.0",
            false,
            |_, _| Ok(()),
            |a, b| {
                calls += 1;
                // Call 1: backup rename succeeds; subsequent target rename fails cleanly; rollback call succeeds.
                if calls == 1 || b == exe && a.to_string_lossy().ends_with(".bak") {
                    return std::fs::rename(a, b);
                }
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked target"))
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("moving staged binary"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary", "original binary must be rolled back");
        assert!(staging.join(STAGED_FILE_NAME).is_file(), "staged.json must remain discoverable for retry");
        assert!(staging.join(&staged.exe_file_name).is_file(), "staged binary must remain intact for retry");

        // Next start without lock now picks up the restored staged state cleanly
        let retried = apply_staged_update_with(&staging, &exe, "1.0.0", false, |_, _| Ok(())).unwrap().unwrap();
        assert_eq!(retried.to_version, "1.0.2");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary-payload");
    }
}

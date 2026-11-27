//! Session file version validation.

use super::*;

/// Current session format version (written in the header by the session plugin).
pub const CURRENT_VERSION: u64 = 3;

/// Reject unknown versions before interpreting any entries or writing to the file.
pub fn check_header(header: &Value) -> Result<()> {
    if header.get("type").and_then(Value::as_str) != Some("session") {
        anyhow::bail!("{CORRUPT}: missing session header");
    }
    let version = header.get("version");
    if version.and_then(Value::as_u64) != Some(CURRENT_VERSION) {
        anyhow::bail!(
            "{UNSUPPORTED_VERSION}: found {}, expected {CURRENT_VERSION}",
            version.map_or_else(|| "missing".to_string(), Value::to_string)
        );
    }
    Ok(())
}

/// Check the first complete line, including on checkpoint and reverse-read paths.
pub(crate) fn check_file_header(path: &Path) -> Result<()> {
    check_file_header_held(&mut File::open(path)?, path)
}

pub(crate) fn check_file_header_held(file: &mut File, path: &Path) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut line = String::new();
    let mut reader = BufReader::new(file);
    reader.read_line(&mut line)?;
    if !line.ends_with('\n') {
        anyhow::bail!("{CORRUPT}: missing complete session header in {}", path.display());
    }
    let header: Value = serde_json::from_str(&line)
        .with_context(|| format!("{CORRUPT}: invalid session header in {}", path.display()))?;
    check_header(&header)
}

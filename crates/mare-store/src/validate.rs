//! Strict validation: streaming the complete lines of a file, scanning that each parses, and
//! the writer's validation pass from a resume `Checkpoint`.

use super::*;

/// A resume checkpoint: the
/// session file had its first `len` bytes validated, and had this modification time then.
/// An optimization, not an integrity check: it assumes only mare appends to session files.
/// A matching checkpoint trusts the prefix, including when the file has grown. Its path, file
/// identity, and rollback generation must all still match before the prefix is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub len: u64,
    /// modification time when the checkpoint was taken (ns since the epoch): with an unchanged
    /// length it tells an in-place edit apart
    pub mtime_ns: u64,
}

/// How a writer's open validated the file (observable for tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenStats {
    /// byte offset the strict scan started from: the checkpoint length when the checkpoint
    /// matched, 0 for a full scan
    pub scanned_from: u64,
    /// a checkpoint was given but did not match (file changed outside mare)
    pub checkpoint_mismatch: bool,
    /// bytes of a torn final write found (truncated before the next append)
    pub torn_bytes: u64,
}

/// Parse one complete line; None when it isn't a JSON value.
pub(crate) fn parse_line(line: &[u8]) -> Option<Value> {
    serde_json::from_slice(line).ok()
}

/// Call `f(line_no, line)` for every complete ('\n'-terminated) line of the file from byte
/// `from` up to its length when opened (frozen end), streaming in 64 KiB reads so memory stays
/// bounded by the longest line, not the file. Returns the
/// length up to and including the last '\n' and the frozen end; bytes between them are a torn
/// final write. `line_no` counts from 1 at `from`.
pub(crate) fn complete_lines(path: &Path, from: u64, f: impl FnMut(usize, &[u8]) -> Result<()>) -> Result<(u64, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    complete_lines_held(&mut file, from, f)
}

pub(crate) fn complete_lines_held(
    file: &mut File,
    from: u64,
    mut f: impl FnMut(usize, &[u8]) -> Result<()>,
) -> Result<(u64, u64)> {
    use std::io::Read;
    let end = file.metadata()?.len();
    file.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::with_capacity(64 * 1024, file.take(end.saturating_sub(from)));
    let mut line = Vec::new();
    let (mut complete, mut line_no) = (from, 0usize);
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        line_no += 1;
        f(line_no, &line[..n - 1])?;
        complete += n as u64;
    }
    Ok((complete, end.max(from)))
}

pub(crate) fn blank(line: &[u8]) -> bool {
    line.iter().all(|b| b.is_ascii_whitespace())
}

/// Strict validation of the file from `start` (a line boundary): every '\n'-terminated line must
/// parse; bytes after the last '\n' are a torn write. Uses `IgnoredAny`, so it costs a fast scan,
/// not a full decode, and streams (no whole-file buffer). Returns (complete length, file length).
pub(crate) fn scan_file(path: &Path, start: u64) -> Result<(u64, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    scan_file_held(&mut file, path, start)
}

fn scan_file_held(file: &mut File, path: &Path, start: u64) -> Result<(u64, u64)> {
    use serde::de::IgnoredAny;
    complete_lines_held(file, start, |n, line| {
        if !blank(line) && serde_json::from_slice::<IgnoredAny>(line).is_err() {
            anyhow::bail!("{CORRUPT}: line {n} of {} does not parse", path.display());
        }
        Ok(())
    })
}

/// One validation pass for [`SessionFile::open_with`]: from the checkpoint when its anchor holds
/// (reading only what follows it), otherwise the whole file. Returns (complete length, start of
/// a torn tail, stats).
pub(crate) fn validate(
    file: &mut File,
    path: &Path,
    checkpoint: Option<&Checkpoint>,
) -> Result<(u64, Option<u64>, OpenStats)> {
    use std::io::Read;
    let mut stats = OpenStats::default();
    let now = crate::stable::stamp_file(file)?;
    if let Some(c) = checkpoint {
        let k = c.len;
        // the anchor: not shorter, and — at the same length — not modified since (an in-place
        // edit keeps the length but moves the mtime); growing is the normal case (appends)
        let generation_matches = crate::lock::file_id(file)
            .ok()
            .is_some_and(|file_id| crate::index::checkpoint_is_current(path, c, file_id).unwrap_or(false));
        if generation_matches && k > 0 && now.len >= k && (now.len > k || now.mtime_ns == c.mtime_ns) {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::Start(k - 1))?;
            file.read_exact(&mut last)?;
            if last[0] == b'\n' {
                let (len, end) = scan_file_held(file, path, k)?;
                stats.scanned_from = k;
                let torn = len < end;
                if torn {
                    stats.torn_bytes = end - len;
                }
                return Ok((len, torn.then_some(len), stats));
            }
        }
        stats.checkpoint_mismatch = true;
    }
    let (len, end) = scan_file_held(file, path, 0)?;
    let torn = len < end;
    if torn {
        stats.torn_bytes = end - len;
    }
    Ok((len, torn.then_some(len), stats))
}

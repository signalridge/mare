//! Reading session files from the end (reverse chunked JSONL scanner, Apache-2.0).
//!
//! Resuming a long session only needs the entries from the latest compaction onward (plus the
//! messages it keeps), so we scan backwards in fixed-size chunks and stop early instead of
//! parsing the whole file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

const CHUNK: usize = 64 * 1024;

/// Yields complete lines from the end of a file towards the start.
pub struct ReverseLines {
    file: File,
    pos: u64,
    /// Segments of a line that started before the current chunk, stored tail-first.
    carry_segments: Vec<Vec<u8>>,
    pending: Vec<Vec<u8>>,
    done: bool,
}

fn assemble_carried_line(head: &[u8], carry_segments: &mut Vec<Vec<u8>>) -> Option<Vec<u8>> {
    if head.is_empty() {
        if carry_segments.is_empty() {
            return None;
        }
        if carry_segments.len() == 1 {
            let seg = carry_segments.pop()?;
            return (!seg.is_empty()).then_some(seg);
        }
    }
    let carry_len: usize = carry_segments.iter().map(Vec::len).sum();
    let total_len = head.len() + carry_len;
    if total_len == 0 {
        carry_segments.clear();
        return None;
    }
    let mut line = Vec::with_capacity(total_len);
    line.extend_from_slice(head);
    for seg in carry_segments.drain(..).rev() {
        line.extend_from_slice(&seg);
    }
    Some(line)
}

impl ReverseLines {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_file(File::open(path)?)
    }

    fn from_file(file: File) -> Result<Self> {
        let pos = file.metadata()?.len();
        Ok(Self { file, pos, carry_segments: Vec::new(), pending: Vec::new(), done: false })
    }

    /// Byte offset of the start of the region not yet returned.
    pub fn position(&self) -> u64 {
        self.pos
    }

    fn fill(&mut self) -> Result<()> {
        while self.pending.is_empty() && !self.done {
            if self.pos == 0 {
                self.done = true;
                if let Some(line) = assemble_carried_line(&[], &mut self.carry_segments) {
                    self.pending.push(line);
                }
                break;
            }
            let start = self.pos.saturating_sub(CHUNK as u64);
            let len = (self.pos - start) as usize;
            let mut buf = vec![0u8; len];
            self.file.seek(SeekFrom::Start(start))?;
            self.file.read_exact(&mut buf)?;
            self.pos = start;

            let Some(first_nl) = buf.iter().position(|&b| b == b'\n') else {
                // No newline in this chunk: push the whole chunk once without re-scanning carry.
                self.carry_segments.push(buf);
                continue;
            };
            let last_nl = buf.iter().rposition(|&b| b == b'\n').unwrap_or(first_nl);
            if last_nl > first_nl {
                for p in buf[first_nl + 1..last_nl].split(|&b| b == b'\n') {
                    if !p.is_empty() {
                        self.pending.push(p.to_vec());
                    }
                }
            }
            if let Some(line) = assemble_carried_line(&buf[last_nl + 1..], &mut self.carry_segments) {
                self.pending.push(line);
            }
            if first_nl > 0 {
                self.carry_segments.push(buf[..first_nl].to_vec());
            }
        }
        Ok(())
    }
}

/// Items are `Err` when a read fails: an I/O error is never
/// mistaken for the start of the file, which would return a cut-short tail marked complete.
/// After an error the iterator ends.
impl Iterator for ReverseLines {
    type Item = Result<Vec<u8>>;
    fn next(&mut self) -> Option<Result<Vec<u8>>> {
        if self.pending.is_empty()
            && let Err(e) = self.fill()
        {
            self.done = true;
            self.pending.clear();
            return Some(Err(e));
        }
        self.pending.pop().map(Ok)
    }
}

/// Result of a tail read.
pub struct Tail {
    /// entries in file order
    pub entries: Vec<Value>,
    /// true when the read stopped before the start of the file
    pub partial: bool,
    /// false when the file still changed on the re-read ([`crate::STABLE_READ_TRIES`]) (a writer
    /// appending or rolling back a batch): show as "being written", never fork from it
    pub stable: bool,
}

/// Read backwards until the most recent `compaction` entry *and* the message it keeps from
/// (`first_kept_entry_id`) have been seen. Without a compaction the whole file is read.
///
/// Lock-free and frozen: the end offset is taken
/// once at the start, and an entry after the last '\n' (still being written, or torn) never
/// counts. Strict like [`crate::read_entries`]: any complete line that does not parse within the
/// region read is [`crate::CORRUPT`].
pub fn read_from_compaction(path: &Path) -> Result<Tail> {
    let (mut tail, stable) = crate::stable_read(path, || read_from_compaction_once(path))?;
    tail.stable = stable;
    Ok(tail)
}

fn read_from_compaction_once(path: &Path) -> Result<Tail> {
    crate::format::check_file_header(path)?;
    tail_from_lines(ReverseLines::open(path)?, path)
}

/// Read the header and reverse tail from the same already-open file handle.
pub(crate) fn read_from_compaction_held(file: &mut File, path: &Path) -> Result<(Tail, Value)> {
    use std::io::BufRead;
    file.seek(SeekFrom::Start(0))?;
    let mut header_line = String::new();
    std::io::BufReader::new(&mut *file).read_line(&mut header_line)?;
    if !header_line.ends_with('\n') {
        anyhow::bail!("{}: missing complete session header", crate::CORRUPT);
    }
    let header: Value = serde_json::from_str(&header_line)?;
    crate::format::check_header(&header)?;
    let tail = tail_from_lines(ReverseLines::from_file(file.try_clone()?)?, path)?;
    Ok((tail, header))
}

fn tail_from_lines(mut lines: ReverseLines, path: &Path) -> Result<Tail> {
    // drop an unterminated final entry: back the frozen end up to just after the last '\n'
    lines.pos = complete_end(&mut lines.file, lines.pos)?;
    let mut out: Vec<Value> = Vec::new();
    let mut need: Option<String> = None;
    let mut found_compaction = false;
    let mut stopped = false;
    for raw in lines.by_ref() {
        let raw = raw?;
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let Some(v) = crate::parse_line(&raw) else {
            anyhow::bail!("{}: a line of {} does not parse or fails its checksum", crate::CORRUPT, path.display());
        };
        let is_compaction = crate::record::CoreRecordType::of(&v) == Some(crate::record::CoreRecordType::Compaction);
        let id = crate::record::entry_id(&v).map(str::to_string);
        if !found_compaction && is_compaction {
            found_compaction = true;
            need = crate::record::core::<crate::record::Compaction>(&v).and_then(|c| c.first_kept_entry_id);
            out.push(v);
            if need.is_none() {
                stopped = true;
                break;
            }
            continue;
        }
        out.push(v);
        if found_compaction && need.is_some() && id == need {
            stopped = true;
            break;
        }
    }
    // partial = earlier records exist that were not returned: either unread bytes before the
    // current chunk, or lines already buffered from the chunk (a file smaller than one chunk
    // is read in one go, so position() is 0 even though the scan stopped early)
    let partial = stopped && (lines.position() > 0 || !lines.pending.is_empty());
    out.reverse();
    Ok(Tail { entries: out, partial, stable: true })
}

/// Offset just after the last '\n' at or before `end` (0 when there is none).
fn complete_end(file: &mut File, end: u64) -> Result<u64> {
    let mut pos = end;
    let mut buf = vec![0u8; 4096];
    while pos > 0 {
        let start = pos.saturating_sub(buf.len() as u64);
        let n = (pos - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        pos = start;
    }
    Ok(0)
}

/// The last `n` parseable entries (file order). Lenient — index previews only, never used to
/// load a session for writing.
pub fn read_tail(path: &Path, n: usize) -> Result<Vec<Value>> {
    crate::format::check_file_header(path)?;
    let mut out = Vec::with_capacity(n);
    for raw in ReverseLines::open(path)? {
        if let Some(v) = crate::parse_line(&raw?) {
            out.push(v);
            if out.len() >= n {
                break;
            }
        }
    }
    out.reverse();
    Ok(out)
}

/// The first `n` parseable entries. Lenient — index previews only.
pub fn read_head(path: &Path, n: usize, max_bytes: usize) -> Result<Vec<Value>> {
    crate::format::check_file_header(path)?;
    use std::io::BufRead;
    let f = File::open(path)?;
    let mut r = std::io::BufReader::new(f);
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut line = String::new();
    while out.len() < n && used < max_bytes {
        line.clear();
        let k = r.read_line(&mut line)?;
        if k == 0 {
            break;
        }
        used += k;
        if let Some(v) = crate::parse_line(line.trim_end_matches(['\n', '\r']).as_bytes()) {
            out.push(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn stops_at_compaction_and_first_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = std::io::BufWriter::new(File::create(&p).unwrap());
        writeln!(f, "{}", json!({"type":"session","session_id":"s","version":3})).unwrap();
        // lots of old history (> one chunk)
        for i in 0..3000 {
            writeln!(f, "{}", json!({"type":"message","entry_id":format!("m{i}"),"message":{"pad":"x".repeat(40)}}))
                .unwrap();
        }
        writeln!(f, "{}", json!({"type":"compaction","entry_id":"c1","first_kept_entry_id":"m2990"})).unwrap();
        writeln!(f, "{}", json!({"type":"message","entry_id":"n1"})).unwrap();
        drop(f);
        let t = read_from_compaction(&p).unwrap();
        assert!(t.partial);
        let ids: Vec<&str> = t.entries.iter().filter_map(|e| e["entry_id"].as_str()).collect();
        assert_eq!(ids.first(), Some(&"m2990"));
        assert_eq!(ids.last(), Some(&"n1"));
        assert_eq!(t.entries.len(), 10 + 1 + 1);
        assert_eq!(read_tail(&p, 2).unwrap()[1]["entry_id"], "n1");
        assert_eq!(read_head(&p, 2, 1 << 20).unwrap()[0]["type"], "session");
    }

    /// A read failing mid-scan (here: the file cut short under the frozen end) is an error, never
    /// a short tail that looks complete.
    #[test]
    fn an_io_error_mid_scan_is_an_error_not_the_start_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = std::io::BufWriter::new(File::create(&p).unwrap());
        for i in 0..4000 {
            writeln!(f, "{}", json!({"type":"message","entry_id":format!("m{i}"),"pad":"x".repeat(40)})).unwrap();
        }
        drop(f);
        let mut lines = ReverseLines::open(&p).unwrap();
        assert!(lines.next().unwrap().is_ok(), "the first chunk reads");
        std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_len(100).unwrap();
        let rest: Vec<_> = lines.collect();
        assert!(rest.iter().any(|r| r.is_err()), "the failed read is reported");
        assert!(rest.last().unwrap().is_err(), "and ends the scan");
    }

    #[test]
    fn multi_chunk_lines_around_64kib_boundaries_and_256kib_record() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("long.jsonl");

        let header = json!({"type":"session","session_id":"s-long","version":3}).to_string();
        // Place newlines before, exactly on, and after a 64 KiB boundary, plus a >256 KiB record.
        let line_before =
            json!({"type":"message","entry_id":"m-before","message":{"pad":"a".repeat(CHUNK - header.len() - 70)}})
                .to_string();
        assert!(header.len() + 1 + line_before.len() + 1 < CHUNK);
        let pad_to_exact = CHUNK - ((header.len() + 1 + line_before.len() + 1) % CHUNK);
        let line_on =
            json!({"type":"message","entry_id":"m-on","message":{"pad":"b".repeat(pad_to_exact.saturating_sub(60))}})
                .to_string();
        let mut expected_lines = vec![header.clone(), line_before, line_on];

        // Pad until a newline falls right on CHUNK boundary and right after CHUNK+1.
        let mut prefix_len: usize = expected_lines.iter().map(|l| l.len() + 1).sum();
        let rem = prefix_len % CHUNK;
        let to_boundary = if rem == 0 { 0 } else { CHUNK - rem };
        let prefix_exact = "{\"type\":\"message\",\"entry_id\":\"m-exact\",\"pad\":\"";
        let suffix = "\"}";
        let exact_overhead = prefix_exact.len() + suffix.len() + 1;
        let exact_filler_len = if to_boundary >= exact_overhead { to_boundary } else { to_boundary + CHUNK };
        let exact_line = format!("{prefix_exact}{}{suffix}", "c".repeat(exact_filler_len - exact_overhead));
        assert_eq!((prefix_len + exact_line.len() + 1) % CHUNK, 0);
        prefix_len += exact_line.len() + 1;
        expected_lines.push(exact_line);

        let prefix_after = "{\"type\":\"message\",\"entry_id\":\"m-after\",\"pad\":\"";
        let after_overhead = prefix_after.len() + suffix.len() + 1;
        let after_line = format!("{prefix_after}{}{suffix}", "d".repeat(CHUNK + 1 - after_overhead));
        assert_eq!((prefix_len + after_line.len() + 1) % CHUNK, 1);
        expected_lines.push(after_line);

        let huge_kept = json!({
            "type": "message",
            "entry_id": "m-huge",
            "message": { "role": "user", "content": "e".repeat(280 * 1024) }
        })
        .to_string();
        assert!(huge_kept.len() > 256 * 1024);
        expected_lines.push(huge_kept);

        let compaction = json!({"type":"compaction","entry_id":"c1","first_kept_entry_id":"m-huge"}).to_string();
        expected_lines.push(compaction);

        let huge_tail = json!({
            "type": "message",
            "entry_id": "m-tail",
            "message": { "role": "assistant", "content": "f".repeat(260 * 1024) }
        })
        .to_string();
        assert!(huge_tail.len() > 256 * 1024);
        expected_lines.push(huge_tail);

        {
            let mut f = std::io::BufWriter::new(File::create(&p).unwrap());
            for l in &expected_lines {
                writeln!(f, "{l}").unwrap();
            }
        }

        let got_rev: Vec<String> =
            ReverseLines::open(&p).unwrap().map(|r| String::from_utf8(r.unwrap()).unwrap()).collect();
        let want_rev: Vec<String> = expected_lines.iter().cloned().rev().collect();
        assert_eq!(got_rev, want_rev);

        let tail = read_from_compaction(&p).unwrap();
        assert!(tail.partial);
        let ids: Vec<&str> = tail.entries.iter().filter_map(|e| e["entry_id"].as_str()).collect();
        assert_eq!(ids, vec!["m-huge", "c1", "m-tail"]);
    }

    #[test]
    fn multi_chunk_corrupt_line_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("corrupt.jsonl");
        {
            let mut f = std::io::BufWriter::new(File::create(&p).unwrap());
            writeln!(f, "{}", json!({"type":"session","session_id":"s-bad","version":3})).unwrap();
            writeln!(f, "{}", json!({"type":"compaction","entry_id":"c1","first_kept_entry_id":"m1"})).unwrap();
            writeln!(f, "{{\"broken\": \"{}\"", "x".repeat(270 * 1024)).unwrap();
        }
        let err = read_from_compaction(&p).err().expect("corrupt multi-chunk line must fail");
        assert!(err.to_string().contains(crate::CORRUPT), "{err}");
    }
}

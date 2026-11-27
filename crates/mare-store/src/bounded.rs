//! Bounded, descriptor-bound strict snapshots for untrusted history consumers.
use anyhow::{Result, bail};
use serde_json::Value;
use std::{fs::File, io::Read, path::Path};

/// Maximum encoded bytes accepted by history consumers (8 MiB).
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// True when `path` is a gzip-compressed JSONL rollout (`.jsonl.gz`).
pub fn is_gzip(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".jsonl.gz"))
}
#[cfg(test)]
thread_local! {
    static BEFORE_VERIFY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Read only a bounded complete header from one unchanged descriptor. Unknown format fails.
/// The caller authorizes the path; O_NOFOLLOW keeps the leaf from changing into a link.
pub fn header(path: &Path, cancelled: impl Fn() -> bool) -> Result<Value> {
    use std::io::BufRead;
    if cancelled() {
        bail!("QUERY_CANCELLED: query cancelled");
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("session source is not a regular file");
    }
    let before = crate::stable::fork_stamp_file(&file)?;
    let mut line = Vec::new();
    if is_gzip(path) {
        let decoder = flate2::read::GzDecoder::new((&file).take(MAX_BYTES));
        std::io::BufReader::new(decoder.take(64 * 1024)).read_until(b'\n', &mut line)?;
    } else {
        std::io::BufReader::new((&file).take(64 * 1024)).read_until(b'\n', &mut line)?;
    }
    if line.last() != Some(&b'\n') {
        bail!("session header exceeds bound or is incomplete");
    }
    let header: Value = serde_json::from_slice(&line)?;
    crate::format::check_header(&header)?;
    if before != crate::stable::fork_stamp_file(&file)? {
        bail!("session header changed");
    }
    Ok(header)
}

/// One descriptor, one frozen end, complete lines only. Refuses a changing source and never
/// rewrites/migrates. The caller must authorize the path before opening it.
pub fn read(path: &Path, max_bytes: u64) -> Result<Vec<Value>> {
    read_with_cancel(path, max_bytes, || false)
}

/// Like read, but checks cooperative cancellation between bounded chunks and parsed records.
pub fn read_with_cancel(path: &Path, max_bytes: u64, cancelled: impl Fn() -> bool) -> Result<Vec<Value>> {
    if cancelled() {
        bail!("QUERY_CANCELLED: query cancelled");
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let before = crate::stable::fork_stamp_file(&file)?;
    if !file.metadata()?.is_file() || before.len > max_bytes.min(MAX_BYTES) {
        bail!("session snapshot exceeds byte limit");
    }
    let mut bytes = Vec::with_capacity(before.len as usize);
    let mut reader = (&file).take(before.len);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            bail!("QUERY_CANCELLED: query cancelled");
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let raw = if is_gzip(path) {
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]).take(MAX_BYTES + 1);
        let mut out = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if cancelled() {
                bail!("QUERY_CANCELLED: query cancelled");
            }
            let n = decoder.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            if out.len() as u64 > MAX_BYTES {
                bail!("decompressed session snapshot exceeds byte limit");
            }
        }
        out
    } else {
        bytes.clone()
    };
    let mut entries = Vec::new();
    for line in raw.split_inclusive(|b| *b == b'\n') {
        if cancelled() {
            bail!("QUERY_CANCELLED: query cancelled");
        }
        if line.last() != Some(&b'\n') {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        entries.push(serde_json::from_slice::<Value>(line)?);
    }
    crate::format::check_header(entries.first().ok_or_else(|| anyhow::anyhow!("missing header"))?)?;
    #[cfg(test)]
    BEFORE_VERIFY.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    if before != crate::stable::fork_stamp_file(&file)? || bytes.len() as u64 != before.len {
        bail!("session snapshot changed during read");
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bound_strict_complete_and_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = format!("{{\"type\":\"session\",\"version\":{}}}\n", crate::CURRENT_VERSION);
        let source = format!("{header}{{\"type\":\"message\"}}\n{{\"torn\"");
        std::fs::write(&path, &source).unwrap();
        assert_eq!(read(&path, MAX_BYTES).unwrap().len(), 2);
        assert!(read(&path, 4).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::write(&path, format!("{header}bad\n")).unwrap();
        assert!(read(&path, MAX_BYTES).is_err());
        std::fs::write(&path, "{\"type\":\"session\",\"version\":999}\n").unwrap();
        assert!(read(&path, MAX_BYTES).is_err());
    }
    #[test]
    fn refuses_a_source_that_changes_during_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, format!("{{\"type\":\"session\",\"version\":{}}}\n", crate::CURRENT_VERSION)).unwrap();
        let changed = path.clone();
        BEFORE_VERIFY.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                use std::io::Write;
                File::options().append(true).open(changed).unwrap().write_all(b"{}\n").unwrap();
            }))
        });
        assert!(read(&path, MAX_BYTES).unwrap_err().to_string().contains("changed"));
    }
    #[test]
    fn cooperative_cancel_stops_read_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let header = format!("{{\"type\":\"session\",\"version\":{}}}\n", crate::CURRENT_VERSION);
        let source = header + &" ".repeat(200_000);
        std::fs::write(&path, &source).unwrap();
        let calls = std::cell::Cell::new(0);
        let error = read_with_cancel(&path, MAX_BYTES, || {
            let n = calls.get() + 1;
            calls.set(n);
            n > 2
        })
        .unwrap_err();
        assert!(error.to_string().contains("QUERY_CANCELLED"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }
    #[cfg(unix)]
    #[test]
    fn header_refuses_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe.jsonl");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path owned by this test.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(header(&path, || false).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("link");
        std::os::unix::fs::symlink("target", &p).unwrap();
        assert!(read(&p, MAX_BYTES).is_err());
    }
    #[test]
    fn reads_compressed_jsonl_gz_and_rejects_bomb() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl.gz");
        let header_line =
            format!("{{\"type\":\"session\",\"version\":{},\"session_id\":\"gz1\"}}\n", crate::CURRENT_VERSION);
        let msg_line =
            "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"user\",\"content\":\"gzipped needle\"}}\n";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(header_line.as_bytes()).unwrap();
        enc.write_all(msg_line.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        std::fs::write(&path, gz).unwrap();
        let h = header(&path, || false).unwrap();
        assert_eq!(h["session_id"], "gz1");
        let entries = read(&path, MAX_BYTES).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["message"]["content"], "gzipped needle");

        // Bomb: > MAX_BYTES decompressed from small compressed payload must fail.
        let bomb_path = dir.path().join("bomb.jsonl.gz");
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(header_line.as_bytes()).unwrap();
        let pad = vec![b' '; (MAX_BYTES as usize) + 1024];
        enc.write_all(&pad).unwrap();
        std::fs::write(&bomb_path, enc.finish().unwrap()).unwrap();
        assert!(read(&bomb_path, MAX_BYTES).is_err());
    }
}

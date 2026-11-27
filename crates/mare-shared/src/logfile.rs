//! The log file: `<state>/log/mare.log`, rotated by size to `mare.log.1` … `.N`
//! (file-rotate), files 0600 and the directory 0700 (Windows: a private directory, whose DACL the
//! files inherit — [`crate::private`]). The level comes from `MARE_LOG`.

use std::path::Path;

/// Size limit of one log file (10 MB) and how many rotated files are kept (5).
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
pub const KEEP: usize = 5;

pub type Writer = file_rotate::FileRotate<file_rotate::suffix::AppendCount>;

pub fn writer(dir: &Path, max_bytes: usize, keep: usize) -> Writer {
    let _ = crate::private::create_dir_all(dir);
    let mut open = std::fs::OpenOptions::new();
    open.read(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    file_rotate::FileRotate::new(
        dir.join("mare.log"),
        file_rotate::suffix::AppendCount::new(keep),
        file_rotate::ContentLimit::BytesSurpassed(max_bytes),
        file_rotate::compression::Compression::None,
        Some(open),
    )
}

/// A log writer that creates the directory and opens the rotating file on the first write, not
/// at startup (the open scans the directory for rotated files — off the startup path).
pub struct Lazy {
    dir: std::path::PathBuf,
    max_bytes: usize,
    keep: usize,
    inner: Option<Writer>,
}

impl Lazy {
    pub fn new(dir: &Path, max_bytes: usize, keep: usize) -> Self {
        Self { dir: dir.to_path_buf(), max_bytes, keep, inner: None }
    }
    fn get(&mut self) -> &mut Writer {
        let (dir, max, keep) = (&self.dir, self.max_bytes, self.keep);
        self.inner.get_or_insert_with(|| writer(dir, max, keep))
    }
}

impl std::io::Write for Lazy {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.get().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.inner.as_mut() {
            Some(w) => w.flush(),
            None => Ok(()),
        }
    }
}

/// Passes every log write through the unified redactor: tracing formats one
/// event per write, so a credential-looking value in a log line never reaches the file.
pub struct Redacting<W>(pub W);

impl<W: std::io::Write> std::io::Write for Redacting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0.write_all(crate::redact::redact_str(&text).as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    #[test]
    fn the_log_rotates_by_size_and_keeps_a_bounded_number_of_files() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        crate::private::testing::open_to_everyone(tmp.path());
        let dir = tmp.path().join("log");
        let dir = dir.as_path();
        let mut w = super::writer(dir, 100, 2);
        for i in 0..20 {
            writeln!(w, "line {i:03} {}", "x".repeat(40)).unwrap();
        }
        w.flush().unwrap();
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["mare.log", "mare.log.1", "mare.log.2"], "rotated, at most 2 kept");
        assert!(std::fs::metadata(dir.join("mare.log")).unwrap().len() <= 200);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join("mare.log")), 0o600);
            assert_eq!(mode(dir), 0o700);
        }
        #[cfg(windows)]
        for p in [dir.join("mare.log"), dir.join("mare.log.1"), dir.to_path_buf()] {
            assert_eq!(crate::private::check(&p), Ok(()));
        }
    }

    #[test]
    fn log_writes_are_redacted() {
        use std::io::Write;
        let mut w = super::Redacting(Vec::new());
        w.write_all(b"request with Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123 sk-ABCDEFGHIJKLMNOPQRSTUVWX\n")
            .unwrap();
        let out = String::from_utf8(w.0).unwrap();
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz0123") && !out.contains("sk-ABCDEFGH"), "{out}");
    }
}

//! Codex `apply_patch` format: parser, fuzzy context seeking and application.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path
//! +line
//! *** Update File: path
//! *** Move to: new/path
//! @@ optional context line
//!  context
//! -old
//! +new
//! *** End of File
//! *** Delete File: path
//! *** End Patch
//! ```

use std::path::{Path, PathBuf};

use thiserror::Error;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CONTEXT_MARKER: &str = "@@";

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("invalid patch: {0}")]
    Invalid(String),
    #[error("invalid hunk at line {line}: {message}")]
    InvalidHunk { message: String, line: usize },
    #[error("{0}")]
    Apply(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateChunk {
    /// A line (usually a function/class header) to seek to before matching `old_lines`.
    pub context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Hunk {
    Add { path: PathBuf, contents: String },
    Delete { path: PathBuf },
    Update { path: PathBuf, move_to: Option<PathBuf>, chunks: Vec<UpdateChunk> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub move_to: Option<PathBuf>,
    pub kind: ChangeKind,
    pub old: Option<String>,
    pub new: Option<String>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn check_boundaries<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str], PatchError> {
    let strict = |ls: &'a [&'a str]| -> Result<&'a [&'a str], PatchError> {
        let first = ls.first().map(|l| l.trim());
        let last = ls.last().map(|l| l.trim());
        match (first, last) {
            (Some(f), Some(l)) if f == BEGIN_PATCH && l == END_PATCH && ls.len() >= 2 => Ok(ls),
            (Some(f), _) if f != BEGIN_PATCH => {
                Err(PatchError::Invalid("the first line of the patch must be '*** Begin Patch'".into()))
            }
            _ => Err(PatchError::Invalid("the last line of the patch must be '*** End Patch'".into())),
        }
    };
    match strict(lines) {
        Ok(l) => Ok(l),
        Err(e) => {
            // Lenient: the model wrapped the patch in a heredoc (`<<'EOF' ... EOF`).
            if lines.len() >= 4 {
                let first = lines[0].trim();
                let last = lines[lines.len() - 1].trim();
                if (first == "<<EOF" || first == "<<'EOF'" || first == "<<\"EOF\"") && last.ends_with("EOF") {
                    return strict(&lines[1..lines.len() - 1]);
                }
            }
            Err(e)
        }
    }
}

pub fn parse_patch(text: &str) -> Result<Vec<Hunk>, PatchError> {
    let all: Vec<&str> = text.trim().lines().collect();
    let lines = check_boundaries(&all)?;
    // offset of `lines` within `all` for error line numbers
    let base = if lines.len() != all.len() { 1 } else { 0 };
    let body = &lines[1..lines.len() - 1];
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line_no = base + i + 2;
        let line = body[i].trim_end();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        let line = line.trim_start();
        if let Some(path) = line.strip_prefix(ADD_FILE) {
            let mut contents = String::new();
            i += 1;
            while i < body.len() {
                if let Some(rest) = body[i].strip_prefix('+') {
                    contents.push_str(rest);
                    contents.push('\n');
                    i += 1;
                } else {
                    break;
                }
            }
            hunks.push(Hunk::Add { path: PathBuf::from(path.trim()), contents });
        } else if let Some(path) = line.strip_prefix(DELETE_FILE) {
            hunks.push(Hunk::Delete { path: PathBuf::from(path.trim()) });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE_FILE) {
            let path = PathBuf::from(path.trim());
            i += 1;
            let mut move_to = None;
            if i < body.len()
                && let Some(dest) = body[i].trim_start().strip_prefix(MOVE_TO)
            {
                move_to = Some(PathBuf::from(dest.trim()));
                i += 1;
            }
            let mut chunks = Vec::new();
            while i < body.len() {
                let l = body[i];
                if l.trim().is_empty() {
                    // skip blank lines separating chunks (and directly after the header)
                    i += 1;
                    continue;
                }
                if l.starts_with("***") && l.trim_end() != EOF_MARKER {
                    break;
                }
                let (chunk, used) = parse_chunk(&body[i..], base + i + 2, chunks.is_empty())?;
                chunks.push(chunk);
                i += used;
            }
            if chunks.is_empty() {
                return Err(PatchError::InvalidHunk {
                    message: format!("update hunk for '{}' is empty", path.display()),
                    line: line_no,
                });
            }
            hunks.push(Hunk::Update { path, move_to, chunks });
        } else {
            return Err(PatchError::InvalidHunk {
                message: format!(
                    "'{line}' is not a valid hunk header. Valid headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                ),
                line: line_no,
            });
        }
    }
    Ok(hunks)
}

fn parse_chunk(
    lines: &[&str],
    line_no: usize,
    allow_missing_context: bool,
) -> Result<(UpdateChunk, usize), PatchError> {
    let mut i = 0;
    let context = if lines[0].trim_end() == EMPTY_CONTEXT_MARKER {
        i = 1;
        None
    } else if let Some(ctx) = lines[0].strip_prefix(CONTEXT_MARKER) {
        i = 1;
        Some(ctx.to_string())
    } else {
        if !allow_missing_context {
            return Err(PatchError::InvalidHunk {
                message: format!("expected '@@' context marker, got: '{}'", lines[0]),
                line: line_no,
            });
        }
        None
    };
    let mut chunk = UpdateChunk { context, old_lines: vec![], new_lines: vec![], is_end_of_file: false };
    let mut changed = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.trim_end() == EOF_MARKER {
            if changed == 0 {
                return Err(PatchError::InvalidHunk { message: "update chunk has no lines".into(), line: line_no + i });
            }
            chunk.is_end_of_file = true;
            i += 1;
            break;
        }
        if changed > 0 && (l.starts_with("@@") || l.starts_with("***")) {
            break;
        }
        if l.starts_with("***") {
            break;
        }
        match l.chars().next() {
            None => {
                chunk.old_lines.push(String::new());
                chunk.new_lines.push(String::new());
            }
            Some(' ') => {
                chunk.old_lines.push(l[1..].to_string());
                chunk.new_lines.push(l[1..].to_string());
            }
            Some('+') => chunk.new_lines.push(l[1..].to_string()),
            Some('-') => chunk.old_lines.push(l[1..].to_string()),
            _ => {
                if changed == 0 {
                    return Err(PatchError::InvalidHunk {
                        message: format!(
                            "unexpected line in update hunk: '{l}'. Every line must start with ' ' (context), '+' or '-'"
                        ),
                        line: line_no + i,
                    });
                }
                break;
            }
        }
        changed += 1;
        i += 1;
    }
    if changed == 0 {
        return Err(PatchError::InvalidHunk { message: "update chunk has no lines".into(), line: line_no + i });
    }
    // Trailing blank context lines (often an artifact of trailing newlines) are harmless but drop
    // pure-empty trailing pairs to improve matching.
    Ok((chunk, i))
}

// ---------------------------------------------------------------------------
// Seeking
// ---------------------------------------------------------------------------

fn normalise(s: &str) -> String {
    s.trim().chars().map(crate::norm_char).collect()
}

/// Find `pattern` in `lines` at or after `start`, with decreasing strictness:
/// exact, trailing-whitespace-insensitive, trim-insensitive, then unicode-punctuation-normalized.
/// With `eof`, match only at the end of the file.
pub fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let search_start = if eof { last } else { start };
    type Eq = dyn Fn(&str, &str) -> bool;
    let passes: [&Eq; 4] =
        [&|a, b| a == b, &|a, b| a.trim_end() == b.trim_end(), &|a, b| a.trim() == b.trim(), &|a, b| {
            normalise(a) == normalise(b)
        }];
    for eq in passes {
        for i in search_start..=last {
            if pattern.iter().enumerate().all(|(k, p)| eq(&lines[i + k], p)) {
                return Some(i);
            }
        }
    }
    // an `*** End of File` hunk only matches at the end — never falls back to mid-file
    None
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

fn resolve(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

fn compute_replacements(
    original: &[String],
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, PatchError> {
    let mut reps = Vec::new();
    let mut line_index = 0;
    for chunk in chunks {
        if let Some(ctx) = &chunk.context {
            match seek_sequence(original, std::slice::from_ref(ctx), line_index, false) {
                Some(idx) => line_index = idx + 1,
                None => {
                    return Err(PatchError::Apply(format!("failed to find context '{ctx}' in {}", path.display())));
                }
            }
        }
        if chunk.old_lines.is_empty() {
            // pure addition: at the end, or just before a trailing empty line
            let at = if original.last().is_some_and(String::is_empty) { original.len() - 1 } else { original.len() };
            reps.push((at, 0, chunk.new_lines.clone()));
            continue;
        }
        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original, pattern, line_index, chunk.is_end_of_file);
        }
        match found {
            Some(idx) => {
                reps.push((idx, pattern.len(), new_slice.to_vec()));
                line_index = idx + pattern.len();
            }
            None => {
                return Err(PatchError::Apply(format!(
                    "failed to find expected lines in {}:\n{}",
                    path.display(),
                    chunk.old_lines.join("\n")
                )));
            }
        }
    }
    reps.sort_by_key(|r| r.0);
    Ok(reps)
}

fn derive_update(original: &str, path: &Path, chunks: &[UpdateChunk]) -> Result<String, PatchError> {
    // a UTF-8 BOM is not part of the first line: match without it, put it back
    let (bom, original) = match original.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", original),
    };
    // every line keeps its own ending (mixed LF/CRLF files stay mixed);
    // new lines take the ending of the line they replace (or the line before an insertion)
    let def: &'static str = crate::line_ending_at(original, 0);
    let mut lines: Vec<String> = Vec::new();
    let mut ends: Vec<&'static str> = Vec::new();
    for seg in original.split_inclusive('\n') {
        match seg.strip_suffix("\r\n") {
            Some(body) => {
                lines.push(body.to_string());
                ends.push("\r\n");
            }
            None => match seg.strip_suffix('\n') {
                Some(body) => {
                    lines.push(body.to_string());
                    ends.push("\n");
                }
                None => {
                    lines.push(seg.to_string());
                    ends.push("");
                }
            },
        }
    }
    let reps = compute_replacements(&lines, path, chunks)?;
    for (start, old_len, new) in reps.iter().rev() {
        let end = (*start + *old_len).min(lines.len());
        let pick = |e: &'static str| if e.is_empty() { def } else { e };
        // context lines are part of the range: lines equal at the start / end of the old and new
        // slices keep their own endings; the changed middle takes the ending of its first old line
        // (or of the line before an insertion)
        let old = &lines[*start..end];
        let pre = old.iter().zip(new.iter()).take_while(|(a, b)| a == b).count();
        let post = old[pre..].iter().rev().zip(new[pre..].iter().rev()).take_while(|(a, b)| a == b).count();
        let mid_end = if *start + pre < end {
            pick(ends[*start + pre])
        } else if *start + pre > 0 {
            pick(ends[*start + pre - 1])
        } else {
            ends.first().copied().map(pick).unwrap_or(def)
        };
        let new_ends: Vec<&'static str> = (0..new.len())
            .map(|k| {
                if k < pre {
                    pick(ends[*start + k])
                } else if k >= new.len() - post {
                    pick(ends[end - (new.len() - k)])
                } else {
                    mid_end
                }
            })
            .collect();
        // inserting after an unterminated last line: that line needs a line break now
        if *old_len == 0 && *start > 0 && *start == lines.len() && ends[*start - 1].is_empty() {
            ends[*start - 1] = def;
        }
        lines.splice(*start..end, new.iter().cloned());
        ends.splice(*start..end, new_ends);
    }
    let mut s = String::from(bom);
    for (l, e) in lines.iter().zip(&ends) {
        s.push_str(l);
        // the file always ends with a line break
        s.push_str(if e.is_empty() { def } else { e });
    }
    Ok(s)
}

/// Default maximum file size read by `apply_patch` (matches `tools.edit_max_bytes`, 10 MiB).
pub const DEFAULT_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;

fn read(path: &Path, what: &str) -> Result<String, PatchError> {
    use std::io::Read;
    let map_io = |source: std::io::Error| PatchError::Io {
        context: format!("failed to read file to {what} {}", path.display()),
        source,
    };
    let meta = std::fs::metadata(path).map_err(map_io)?;
    if !meta.is_file() {
        return Err(PatchError::Io {
            context: format!("cannot {what} {}: not a regular file", path.display()),
            source: std::io::Error::other("not a regular file"),
        });
    }
    if meta.len() > DEFAULT_FILE_MAX_BYTES {
        return Err(PatchError::Io {
            context: format!(
                "cannot {what} {}: {} bytes exceeds {}-byte limit (too_large)",
                path.display(),
                meta.len(),
                DEFAULT_FILE_MAX_BYTES
            ),
            source: std::io::Error::other("file too large"),
        });
    }
    let file = std::fs::File::open(path).map_err(map_io)?;
    let fmeta = file.metadata().map_err(map_io)?;
    if !fmeta.is_file() {
        return Err(PatchError::Io {
            context: format!("cannot {what} {}: not a regular file", path.display()),
            source: std::io::Error::other("not a regular file"),
        });
    }
    let mut buf = Vec::with_capacity(fmeta.len().min(DEFAULT_FILE_MAX_BYTES) as usize);
    file.take(DEFAULT_FILE_MAX_BYTES + 1).read_to_end(&mut buf).map_err(map_io)?;
    if buf.len() as u64 > DEFAULT_FILE_MAX_BYTES {
        return Err(PatchError::Io {
            context: format!(
                "cannot {what} {}: content exceeds {}-byte limit (too_large)",
                path.display(),
                DEFAULT_FILE_MAX_BYTES
            ),
            source: std::io::Error::other("file too large"),
        });
    }
    String::from_utf8(buf).map_err(|_| PatchError::Io {
        // a non-UTF-8 text file is a typed refusal, not an I/O mystery
        context: format!("cannot {what} {}: invalid UTF-8 text (not_text)", path.display()),
        source: std::io::Error::from(std::io::ErrorKind::InvalidData),
    })
}

/// Dry-run: parse the patch and compute every file's old/new contents without writing.
///
/// Hunks apply in order against an in-memory view of the files,
/// so a later hunk sees the effect of an earlier one on the same path.
pub fn compute_changes(text: &str, cwd: &Path) -> Result<Vec<FileChange>, PatchError> {
    let hunks = parse_patch(text)?;
    if hunks.is_empty() {
        return Err(PatchError::Invalid("patch contains no file operations".into()));
    }
    // path -> current contents (None = deleted by an earlier hunk)
    let mut view: std::collections::HashMap<PathBuf, Option<String>> = std::collections::HashMap::new();
    let current =
        |view: &std::collections::HashMap<PathBuf, Option<String>>, abs: &Path, what: &str| match view.get(abs) {
            Some(Some(s)) => Ok(s.clone()),
            Some(None) => Err(PatchError::Io {
                context: format!("failed to read file to {what} {}", abs.display()),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }),
            None => read(abs, what),
        };
    let mut out = Vec::new();
    for h in hunks {
        match h {
            Hunk::Add { path, contents } => {
                let abs = resolve(cwd, &path);
                let old = match view.get(&abs) {
                    Some(v) => v.clone(),
                    None => match std::fs::metadata(&abs) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        _ => read(&abs, "overwrite").ok(),
                    },
                };
                if let Err(e) = std::fs::metadata(&abs) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(PatchError::Io {
                            context: format!("failed to inspect file to overwrite {}", abs.display()),
                            source: e,
                        });
                    }
                } else if old.is_none() {
                    read(&abs, "overwrite")?;
                }
                view.insert(abs.clone(), Some(contents.clone()));
                out.push(FileChange { path: abs, move_to: None, kind: ChangeKind::Add, old, new: Some(contents) });
            }
            Hunk::Delete { path } => {
                let abs = resolve(cwd, &path);
                let old = current(&view, &abs, "delete")?;
                view.insert(abs.clone(), None);
                out.push(FileChange { path: abs, move_to: None, kind: ChangeKind::Delete, old: Some(old), new: None });
            }
            Hunk::Update { path, move_to, chunks } => {
                let abs = resolve(cwd, &path);
                let old = current(&view, &abs, "update")?;
                let new = derive_update(&old, &path, &chunks)?;
                let dest = move_to.map(|m| resolve(cwd, &m));
                match &dest {
                    Some(d) if d != &abs => {
                        view.insert(abs.clone(), None);
                        view.insert(d.clone(), Some(new.clone()));
                    }
                    _ => {
                        view.insert(abs.clone(), Some(new.clone()));
                    }
                }
                out.push(FileChange {
                    path: abs,
                    move_to: dest,
                    kind: ChangeKind::Update,
                    old: Some(old),
                    new: Some(new),
                });
            }
        }
    }
    Ok(out)
}

fn write(path: &Path, data: &str) -> Result<(), PatchError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| PatchError::Io {
            context: format!("failed to create parent directories for {}", path.display()),
            source,
        })?;
    }
    std::fs::write(path, data)
        .map_err(|source| PatchError::Io { context: format!("failed to write {}", path.display()), source })
}

/// Compute all changes first (so a failing hunk writes nothing), then write them.
pub fn apply_patch(text: &str, cwd: &Path) -> Result<Vec<FileChange>, PatchError> {
    let changes = compute_changes(text, cwd)?;
    for c in &changes {
        match c.kind {
            ChangeKind::Add => write(&c.path, c.new.as_deref().unwrap_or_default())?,
            ChangeKind::Delete => std::fs::remove_file(&c.path).map_err(|source| PatchError::Io {
                context: format!("failed to delete {}", c.path.display()),
                source,
            })?,
            ChangeKind::Update => {
                let data = c.new.as_deref().unwrap_or_default();
                if let Some(dest) = &c.move_to {
                    write(dest, data)?;
                    if dest != &c.path {
                        std::fs::remove_file(&c.path).map_err(|source| PatchError::Io {
                            context: format!("failed to remove original {}", c.path.display()),
                            source,
                        })?;
                    }
                } else {
                    write(&c.path, data)?;
                }
            }
        }
    }
    Ok(changes)
}

/// Summary of changes: `A path`, `M path`, `D path`.
pub fn summarize(changes: &[FileChange]) -> String {
    let mut out = String::from("Success. Updated the following files:\n");
    for c in changes {
        let (tag, p) = match c.kind {
            ChangeKind::Add => ("A", &c.path),
            ChangeKind::Delete => ("D", &c.path),
            ChangeKind::Update => ("M", c.move_to.as_ref().unwrap_or(&c.path)),
        };
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{tag} {}\n", p.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn wrap(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[test]
    fn later_hunks_see_earlier_ones() {
        // Hunks apply one after another; two updates of one file must both land
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let p = wrap(
            "*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: a.txt\n@@\n-three\n+THREE\n\
             *** Add File: b.txt\n+x\n*** Update File: b.txt\n@@\n-x\n+y",
        );
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ONE\ntwo\nTHREE\n");
        assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "y\n");
    }

    #[test]
    fn blank_line_between_chunks_after_eof_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let p = wrap("*** Update File: a.txt\n@@\n-b\n+B\n*** End of File\n\n@@\n+last");
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a\nB\nlast\n");
    }

    #[test]
    fn parse_all_kinds() {
        let p = wrap(
            "*** Add File: a.txt\n+x\n+y\n*** Delete File: d.txt\n*** Update File: u.py\n*** Move to: v.py\n@@ def f():\n-    pass\n+    return 1",
        );
        let h = parse_patch(&p).unwrap();
        assert_eq!(h[0], Hunk::Add { path: "a.txt".into(), contents: "x\ny\n".into() });
        assert_eq!(h[1], Hunk::Delete { path: "d.txt".into() });
        assert_eq!(
            h[2],
            Hunk::Update {
                path: "u.py".into(),
                move_to: Some("v.py".into()),
                chunks: vec![UpdateChunk {
                    context: Some("def f():".into()),
                    old_lines: vec!["    pass".into()],
                    new_lines: vec!["    return 1".into()],
                    is_end_of_file: false
                }]
            }
        );
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(parse_patch("bad"), Err(PatchError::Invalid(_))));
        assert!(matches!(parse_patch("*** Begin Patch\nbad"), Err(PatchError::Invalid(_))));
        assert!(matches!(parse_patch(&wrap("*** Frobnicate: x")), Err(PatchError::InvalidHunk { .. })));
        assert!(matches!(parse_patch(&wrap("*** Update File: x")), Err(PatchError::InvalidHunk { .. })));
    }

    #[test]
    fn heredoc_lenient() {
        let p = format!("<<'EOF'\n{}\nEOF", wrap("*** Add File: a\n+1"));
        assert_eq!(parse_patch(&p).unwrap().len(), 1);
    }

    #[test]
    fn add_update_delete_move() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("u.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(d.join("gone.txt"), "bye\n").unwrap();
        fs::write(d.join("m.txt"), "a\nb\n").unwrap();
        let p = wrap(
            "*** Add File: sub/new.txt\n+hello\n*** Update File: u.txt\n@@\n one\n-two\n+TWO\n three\n*** Delete File: gone.txt\n*** Update File: m.txt\n*** Move to: moved/m2.txt\n@@\n-a\n+A",
        );
        let ch = apply_patch(&p, d).unwrap();
        assert_eq!(fs::read_to_string(d.join("sub/new.txt")).unwrap(), "hello\n");
        assert_eq!(fs::read_to_string(d.join("u.txt")).unwrap(), "one\nTWO\nthree\n");
        assert!(!d.join("gone.txt").exists());
        assert!(!d.join("m.txt").exists());
        assert_eq!(fs::read_to_string(d.join("moved/m2.txt")).unwrap(), "A\nb\n");
        let s = summarize(&ch);
        assert!(s.contains("A ") && s.contains("M ") && s.contains("D "));
    }

    #[test]
    fn multiple_chunks_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.py");
        fs::write(&f, "def a():\n    x = 1\n\ndef b():\n    x = 1\n").unwrap();
        let p = wrap("*** Update File: f.py\n@@ def a():\n-    x = 1\n+    x = 2\n@@ def b():\n-    x = 1\n+    x = 3");
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "def a():\n    x = 2\n\ndef b():\n    x = 3\n");
    }

    #[test]
    fn bom_is_kept_and_ignored_for_matching() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), "\u{feff}a\r\nb\r\n").unwrap();
        let ch = compute_changes("*** Begin Patch\n*** Update File: f\n@@\n-a\n+A\n*** End Patch", dir.path()).unwrap();
        assert_eq!(ch[0].new.as_deref(), Some("\u{feff}A\r\nb\r\n"));
    }

    #[test]
    fn line_endings_preserved() {
        // CRLF stays CRLF (the patch itself is LF), LF files stay LF
        let dir = tempfile::tempdir().unwrap();
        let (w, u) = (dir.path().join("w.txt"), dir.path().join("u.txt"));
        fs::write(&w, "a\r\nb\r\nc\r\n").unwrap();
        fs::write(&u, "a\nb\nc\n").unwrap();
        let p = wrap("*** Update File: w.txt\n@@\n a\n-b\n+B\n*** Update File: u.txt\n@@\n a\n-b\n+B");
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&w).unwrap(), "a\r\nB\r\nc\r\n");
        assert_eq!(fs::read_to_string(&u).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn end_of_file_marker() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "x\ny\nx\n").unwrap();
        let p = wrap("*** Update File: f.txt\n@@\n-x\n+z\n*** End of File");
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "x\ny\nz\n");
    }

    #[test]
    fn fuzzy_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "  foo   \n\u{201C}q\u{201D}\n").unwrap();
        let p = wrap("*** Update File: f.txt\n@@\n-foo\n-\"q\"\n+bar");
        apply_patch(&p, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "bar\n");
    }

    #[test]
    fn missing_lines_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, "a\n").unwrap();
        let p = wrap("*** Add File: n.txt\n+x\n*** Update File: f.txt\n@@\n-zzz\n+y");
        let e = apply_patch(&p, dir.path()).unwrap_err();
        assert!(e.to_string().contains("failed to find expected lines"));
        assert!(!dir.path().join("n.txt").exists());
    }

    /// an `*** End of File` hunk matches at the end only, never mid-file
    #[test]
    fn eof_hunks_match_only_at_the_end() {
        let lines: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(seek_sequence(&lines, &["a".to_string()], 0, true), None);
        assert_eq!(seek_sequence(&lines, &["c".to_string()], 0, true), Some(2));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\nend\nx\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: f.txt\n@@\n-end\n+END\n*** End of File\n*** End Patch";
        assert!(compute_changes(p, dir.path()).is_err(), "the mid-file 'end' must not match an EOF hunk");
    }

    /// a pure addition goes before a trailing empty line
    #[test]
    fn pure_additions_go_before_a_trailing_empty_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: f.txt\n@@\n+b\n*** End Patch";
        let ch = compute_changes(p, dir.path()).unwrap();
        assert_eq!(ch[0].new.as_deref(), Some("a\nb\n\n"));
    }

    /// mixed line endings: untouched lines keep theirs, new lines take the replaced line's;
    /// a file without a final newline gets one (CRLF files get CRLF)
    #[test]
    fn line_endings_follow_the_edited_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.txt"), "one\r\ntwo\nthree\r\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: m.txt\n@@\n one\n-two\n+TWO\n+2b\n three\n*** End Patch";
        let ch = compute_changes(p, dir.path()).unwrap();
        assert_eq!(ch[0].new.as_deref(), Some("one\r\nTWO\n2b\nthree\r\n"));
        std::fs::write(dir.path().join("n.txt"), "a\nb").unwrap();
        std::fs::write(dir.path().join("w.txt"), "a\r\nb").unwrap();
        let p = |f: &str| format!("*** Begin Patch\n*** Update File: {f}\n@@\n a\n-b\n+B\n*** End Patch");
        assert_eq!(compute_changes(&p("n.txt"), dir.path()).unwrap()[0].new.as_deref(), Some("a\nB\n"));
        assert_eq!(compute_changes(&p("w.txt"), dir.path()).unwrap()[0].new.as_deref(), Some("a\r\nB\r\n"));
    }

    /// two sections for the same path chain in memory (the second sees the first's result)
    #[test]
    fn same_path_sections_chain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let p =
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+A\n*** Update File: f.txt\n@@\n-c\n+C\n*** End Patch";
        let ch = compute_changes(p, dir.path()).unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[1].old.as_deref(), Some("A\nb\nc\n"));
        assert_eq!(ch[1].new.as_deref(), Some("A\nb\nC\n"));
    }

    /// a non-UTF-8 text file is a typed `not_text` refusal
    #[test]
    fn non_utf8_files_are_not_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("l.txt"), b"caf\xe9\n").unwrap();
        let p = "*** Begin Patch\n*** Update File: l.txt\n@@\n-x\n+y\n*** End Patch";
        let e = compute_changes(p, dir.path()).unwrap_err().to_string();
        assert!(e.contains("not_text"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_and_oversized_files_are_rejected_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.txt");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success(), "mkfifo failed");
        let p_fifo = "*** Begin Patch\n*** Update File: pipe.txt\n@@\n-x\n+y\n*** End Patch";
        let err_fifo = compute_changes(p_fifo, dir.path()).unwrap_err().to_string();
        assert!(err_fifo.contains("not a regular file"), "{err_fifo}");

        let big = dir.path().join("big.txt");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(DEFAULT_FILE_MAX_BYTES + 1).unwrap();
        let p_big = "*** Begin Patch\n*** Update File: big.txt\n@@\n-x\n+y\n*** End Patch";
        let err_big = compute_changes(p_big, dir.path()).unwrap_err().to_string();
        assert!(err_big.contains("too_large"), "{err_big}");
    }

    #[test]
    fn invalid_hunks_and_missing_context_fail_with_structured_errors() {
        // Empty patch contains no file operations.
        let dir = tempfile::tempdir().unwrap();
        let err = compute_changes("*** Begin Patch\n\n*** End Patch", dir.path()).unwrap_err().to_string();
        assert!(err.contains("no file operations"), "{err}");

        // Second chunk missing @@ marker must error.
        let missing_at = wrap("*** Update File: a.txt\n@@\n-a\n+b\nbad_line");
        assert!(matches!(parse_patch(&missing_at), Err(PatchError::InvalidHunk { .. })));

        // Update chunk with no +/-/context lines must error.
        let empty_chunk = wrap("*** Update File: a.txt\n@@\n*** End Patch");
        assert!(matches!(parse_patch(&empty_chunk), Err(PatchError::InvalidHunk { .. })));
        let empty_eof = wrap("*** Update File: a.txt\n@@\n*** End of File");
        assert!(matches!(parse_patch(&empty_eof), Err(PatchError::InvalidHunk { .. })));

        // Unexpected prefix in first line of update chunk must error.
        let bad_prefix = wrap("*** Update File: a.txt\n@@\n?unexpected");
        assert!(matches!(parse_patch(&bad_prefix), Err(PatchError::InvalidHunk { .. })));

        // Empty blank line inside an update chunk counts as context line.
        fs::write(dir.path().join("blank.txt"), "a\n\nb\n").unwrap();
        let blank_ctx = wrap("*** Update File: blank.txt\n@@\n a\n\n-b\n+B");
        apply_patch(&blank_ctx, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("blank.txt")).unwrap(), "a\n\nB\n");

        // Missing @@ context target returns PatchError::Apply.
        let missing_ctx = wrap("*** Update File: blank.txt\n@@ fn nonexistent()\n-a\n+A");
        let err = apply_patch(&missing_ctx, dir.path()).unwrap_err().to_string();
        assert!(err.contains("failed to find context"), "{err}");

        // Deleting a file then updating it in the same patch fails with NotFound.
        let del_then_upd = wrap("*** Delete File: blank.txt\n*** Update File: blank.txt\n@@\n-a\n+A");
        assert!(compute_changes(&del_then_upd, dir.path()).is_err());

        // Overwriting an existing file via Add File after Delete File in the same patch works.
        let del_then_add = wrap("*** Delete File: blank.txt\n*** Add File: blank.txt\n+fresh");
        let ch = compute_changes(&del_then_add, dir.path()).unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[1].new.as_deref(), Some("fresh\n"));

        // seek_sequence handles empty pattern and oversized pattern cleanly.
        let lines = vec!["one".to_string(), "two".to_string()];
        assert_eq!(seek_sequence(&lines, &[], 1, false), Some(1));
        assert_eq!(seek_sequence(&lines, &["a".into(), "b".into(), "c".into()], 0, false), None);

        // Trailing empty line in pattern falls back cleanly when file has no extra blank line.
        fs::write(dir.path().join("tail.txt"), "first\nsecond\n").unwrap();
        let tail_patch = wrap("*** Update File: tail.txt\n@@\n-second\n+SECOND\n ");
        apply_patch(&tail_patch, dir.path()).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("tail.txt")).unwrap(), "first\nSECOND\n");
    }
}

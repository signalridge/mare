use super::*;
use serde_json::{Value, json};
use std::io::Write;

/// Own every cached index opened below a test directory before deleting its files.
struct OwnedTempDir(Option<tempfile::TempDir>);

impl OwnedTempDir {
    fn new() -> Self {
        Self(Some(tempfile::tempdir().unwrap()))
    }

    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }

    fn cleanup(&mut self) -> Result<()> {
        let Some(directory) = self.0.take() else { return Ok(()) };
        let roots: Vec<_> = OPEN
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(root, _)| root.starts_with(directory.path()))
            .map(|(root, _)| root.clone())
            .collect();
        let mut errors = Vec::new();
        for root in roots {
            if let Err(error) = close_idle(&root) {
                errors.push(format!("close test index {}: {error}", root.display()));
            }
        }
        let path = directory.path().to_path_buf();
        if let Err(error) = directory.close() {
            errors.push(format!("remove test directory {}: {error}", path.display()));
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
        Ok(())
    }

    fn close(mut self) -> Result<()> {
        self.cleanup()
    }
}

impl Drop for OwnedTempDir {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            if std::thread::panicking() {
                eprintln!("{error:#}");
            } else {
                panic!("{error:#}");
            }
        }
    }
}

fn append(p: &Path, lines: &[Value]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn header(id: &str, cwd: &str) -> Value {
    json!({"type":"session","version":3,"session_id":id,"cwd":cwd,"created_at_ms":1})
}

fn user(id: &str, text: &str, ts: i64) -> Value {
    json!({"type":"message","entry_id":id,"created_at_ms":ts,"message":{"role":"user","content":[{"type":"text","text":text}]}})
}

fn ids(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.session_id.as_str()).collect()
}

fn all(ix: &Index) -> Vec<Row> {
    ix.list(&Query { kinds: Kinds::Any, ..Default::default() }).unwrap().0
}

fn checkpoint(path: &Path) -> crate::Checkpoint {
    let mut file = crate::SessionFile::open_existing(path, None).unwrap();
    file.checkpoint().unwrap().unwrap()
}

mod lifecycle_and_concurrency;
mod reconcile;
mod rollback;
mod schema_and_errors;

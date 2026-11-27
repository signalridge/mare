//! CLI `mare session <subcommand>` handlers, sharing the session service and address resolver.

use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use mare_host::{Host, HostOptions};
use mare_shared::ui::Mode;
use serde_json::{Value as J, json};

use crate::cli::SessionCmd;
use crate::rpc::call_service_checked as call_service;
use crate::session_addr::{SelectorError, resolve_session};

pub(crate) async fn run(action: &SessionCmd, cwd: &Path, name: Option<String>) -> Result<i32> {
    match action {
        SessionCmd::New => session_new(cwd, name).await,
        SessionCmd::List { all, archived, limit, json } => session_list(cwd, *all, *archived, *limit, *json).await,
        SessionCmd::Fork { session } => session_fork(cwd, session, name).await,
        SessionCmd::Archive { session } => session_archive(cwd, session).await,
        SessionCmd::Unarchive { session } => session_unarchive(cwd, session).await,
        SessionCmd::Delete { session, yes } => session_delete(cwd, session, *yes).await,
    }
}

async fn session_host(cwd: &Path, args: J) -> Result<Arc<Host>> {
    Host::new(HostOptions {
        cwd: cwd.to_path_buf(),
        mode: Mode::Print,
        config: json!({}),
        load_init: false,
        project_trusted: false,
        args,
        no_user_plugins: true,
    })
    .await
}

async fn with_resolved_session<F, Fut>(cwd: &Path, session: &str, include_archived: bool, op: F) -> Result<i32>
where
    F: FnOnce(Arc<Host>, String) -> Fut,
    Fut: Future<Output = Result<i32>>,
{
    let host = session_host(cwd, json!({})).await?;
    let path = match resolve_session(&host, session, true, include_archived).await {
        Ok(p) => p,
        Err(err) => {
            host.shutdown().await;
            if let Some(sel) = err.downcast_ref::<SelectorError>() {
                eprintln!("mare: {}", sel.detail);
                return Ok(2);
            }
            return Err(err);
        }
    };
    let res = op(Arc::clone(&host), path).await;
    host.shutdown().await;
    res
}

/// `mare session new`: the session plugin creates the session file (header only) and indexes it;
/// its id goes to stdout.
async fn session_new(cwd: &Path, name: Option<String>) -> Result<i32> {
    let host = session_host(cwd, json!({ "session_new": true, "name": name })).await?;
    let mut id = None;
    if let Ok(info) = call_service(&host, "session", "info", vec![]).await
        && info.get("path").and_then(J::as_str).is_some()
    {
        id = info.get("session_id").and_then(J::as_str).map(str::to_owned);
    }
    host.shutdown().await;
    match id {
        Some(id) => {
            println!("{id}");
            Ok(0)
        }
        None => {
            eprintln!("mare: no session created (sessions are off?)");
            Ok(1)
        }
    }
}

fn format_updated_ms(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .ok()
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

struct PageDisplayIndex {
    all_ids: Vec<String>,
    by_key: HashMap<String, mare_store::index::Row>,
}

fn page_display_index(
    root: &Path,
    page_ids: &[String],
    need_row_ids: &[String],
    need_row_paths: &[String],
) -> PageDisplayIndex {
    if let Ok(ix) = mare_store::index::ready(root) {
        let index = ix.lock().unwrap_or_else(|e| e.into_inner());
        let all_ids = if page_ids.len() <= 32 {
            let mut ids = Vec::with_capacity(page_ids.len() * 3);
            let mut ok = true;
            for id in page_ids {
                if id.is_empty() {
                    continue;
                }
                match index.neighbor_session_ids(id) {
                    Ok((prev, next)) => {
                        if let Some(p) = prev {
                            ids.push(p);
                        }
                        ids.push(id.clone());
                        if let Some(n) = next {
                            ids.push(n);
                        }
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok { ids } else { index.session_ids().unwrap_or_default() }
        } else {
            index.session_ids().unwrap_or_default()
        };
        let id_refs: Vec<&str> = need_row_ids.iter().map(String::as_str).collect();
        let path_refs: Vec<&str> = need_row_paths.iter().map(String::as_str).collect();
        let rows = index.rows_by_keys(&id_refs, &path_refs).unwrap_or_default();
        let mut by_key = HashMap::with_capacity(rows.len() * 2);
        for r in rows {
            if !r.session_id.is_empty() {
                by_key.insert(r.session_id.clone(), r.clone());
            }
            if !r.path.is_empty() {
                by_key.insert(r.path.clone(), r);
            }
        }
        return PageDisplayIndex { all_ids, by_key };
    }

    let q = mare_store::index::Query {
        kinds: mare_store::index::Kinds::Sessions,
        archived: None,
        nonempty: false,
        ..Default::default()
    };
    let rows = mare_store::index::scan_list(root, &q).map(|(r, _)| r).unwrap_or_default();
    let mut all_ids = Vec::with_capacity(rows.len());
    let mut by_key = HashMap::with_capacity(rows.len() * 2);
    for r in rows {
        if !r.session_id.is_empty() {
            all_ids.push(r.session_id.clone());
            by_key.insert(r.session_id.clone(), r.clone());
        }
        if !r.path.is_empty() {
            by_key.insert(r.path.clone(), r);
        }
    }
    PageDisplayIndex { all_ids, by_key }
}

fn unique_short_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let mut sorted: Vec<String> = ids.into_iter().filter(|s| !s.is_empty()).map(str::to_owned).collect();
    sorted.sort();
    sorted.dedup();
    let common = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let mut out = HashMap::with_capacity(sorted.len());
    for (i, id) in sorted.iter().enumerate() {
        let mut need = 8;
        if i > 0 {
            need = need.max(common(id, &sorted[i - 1]) + 1);
        }
        if i + 1 < sorted.len() {
            need = need.max(common(id, &sorted[i + 1]) + 1);
        }
        let end = id.char_indices().map(|(idx, _)| idx).find(|&idx| idx >= need).unwrap_or(id.len());
        out.insert(id.clone(), id[..end].to_string());
    }
    out
}

async fn session_list(cwd: &Path, all: bool, archived: bool, limit: Option<usize>, json_out: bool) -> Result<i32> {
    if limit == Some(0) {
        eprintln!("mare: --limit must be a positive integer");
        return Ok(2);
    }
    let host = session_host(cwd, json!({})).await?;
    let list =
        call_service(&host, "session", "list", vec![json!({ "all": all, "archived": archived, "limit": limit })]).await;
    host.shutdown().await;
    let list = list?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(0);
    }

    let entries = list.get("data").and_then(J::as_array).map(Vec::as_slice).unwrap_or(&[]);
    if entries.is_empty() {
        return Ok(0);
    }
    let page_ids: Vec<String> =
        entries.iter().filter_map(|e| e.get("session_id").and_then(J::as_str).map(str::to_owned)).collect();
    let mut need_row_ids = Vec::new();
    let mut need_row_paths = Vec::new();
    for entry in entries {
        let has_name = entry
            .get("name")
            .and_then(J::as_str)
            .map(mare_store::index::normalize_title)
            .is_some_and(|t| !t.is_empty());
        let has_updated = entry.get("updated_at_ms").and_then(J::as_i64).is_some();
        let has_cwd = !all || entry.get("cwd").and_then(J::as_str).is_some_and(|s| !s.is_empty());
        if !has_name || !has_updated || !has_cwd {
            if let Some(id) = entry.get("session_id").and_then(J::as_str).filter(|s| !s.is_empty()) {
                need_row_ids.push(id.to_owned());
            }
            if let Some(path) = entry.get("path").and_then(J::as_str).filter(|s| !s.is_empty()) {
                need_row_paths.push(path.to_owned());
            }
        }
    }
    let root = mare_shared::paths::sessions_dir();
    let display =
        tokio::task::spawn_blocking(move || page_display_index(&root, &page_ids, &need_row_ids, &need_row_paths))
            .await?;
    let by_key = display.by_key;
    let short_ids = unique_short_ids(
        display
            .all_ids
            .iter()
            .map(String::as_str)
            .chain(entries.iter().filter_map(|e| e.get("session_id").and_then(J::as_str))),
    );
    for entry in entries {
        let id = entry.get("session_id").and_then(J::as_str).unwrap_or("");
        let path = entry.get("path").and_then(J::as_str).unwrap_or("");
        let row = (!id.is_empty()).then(|| by_key.get(id)).flatten().or_else(|| by_key.get(path));
        let short: &str = short_ids.get(id).map(String::as_str).unwrap_or("-");
        let updated_ms =
            entry.get("updated_at_ms").and_then(J::as_i64).or_else(|| row.map(|r| r.updated_at_ms)).unwrap_or(0);
        let updated = format_updated_ms(updated_ms);
        let title = row
            .and_then(|r| r.title.as_deref().or(r.preview.as_deref()))
            .or_else(|| entry.get("name").and_then(J::as_str))
            .map(mare_store::index::normalize_title)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "(untitled)".to_string());
        if all {
            let entry_cwd = entry.get("cwd").and_then(J::as_str).or_else(|| row.map(|r| r.cwd.as_str())).unwrap_or("");
            if entry_cwd.is_empty() {
                println!("{short}  {updated}  {title}");
            } else {
                println!("{short}  {updated}  {title}  {entry_cwd}");
            }
        } else {
            println!("{short}  {updated}  {title}");
        }
    }
    Ok(0)
}

async fn session_fork(cwd: &Path, session: &str, name: Option<String>) -> Result<i32> {
    with_resolved_session(cwd, session, false, |host, path| async move {
        let loaded = call_service(&host, "session", "load", vec![json!({ "path": path })]).await?;
        let sid = loaded
            .get("session_id")
            .and_then(J::as_str)
            .ok_or_else(|| anyhow::anyhow!("loaded session has no session_id"))?;
        let mut opts = json!({ "session_id": sid });
        if let Some(n) = name {
            opts["name"] = J::String(n);
        }
        let forked = call_service(&host, "session", "fork", vec![opts]).await?;
        let id =
            forked.get("session_id").and_then(J::as_str).ok_or_else(|| anyhow::anyhow!("forked session has no id"))?;
        println!("{id}");
        Ok(0)
    })
    .await
}

async fn session_archive(cwd: &Path, session: &str) -> Result<i32> {
    let target = session.to_string();
    with_resolved_session(cwd, session, false, |host, path| async move {
        let _ = call_service(&host, "session", "archive", vec![J::String(path)]).await?;
        println!("archived {target}");
        Ok(0)
    })
    .await
}

async fn session_unarchive(cwd: &Path, session: &str) -> Result<i32> {
    let target = session.to_string();
    with_resolved_session(cwd, session, true, |host, path| async move {
        let _ = call_service(&host, "session", "unarchive", vec![J::String(path)]).await?;
        println!("unarchived {target}");
        Ok(0)
    })
    .await
}

fn confirm_delete(session: &str, yes: bool) -> Result<Option<i32>> {
    if yes {
        return Ok(None);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("mare: session delete requires --yes when stdin is not a terminal");
        return Ok(Some(2));
    }
    eprint!("Delete session `{session}`? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let reply = answer.trim().to_ascii_lowercase();
    if reply != "y" && reply != "yes" {
        eprintln!("cancelled");
        return Ok(Some(1));
    }
    Ok(None)
}

async fn session_delete(cwd: &Path, session: &str, yes: bool) -> Result<i32> {
    let target = session.to_string();
    with_resolved_session(cwd, session, true, |host, path| async move {
        if let Some(code) = confirm_delete(&target, yes)? {
            return Ok(code);
        }
        let _ = call_service(&host, "session", "delete", vec![J::String(path)]).await?;
        println!("deleted {target}");
        Ok(0)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn session_list_limit_one_disambiguates_global_and_archived_without_full_stat_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let w1 = root.join("--ws1--");
        let w2 = root.join("--ws2--");
        std::fs::create_dir_all(&w1).unwrap();
        std::fs::create_dir_all(&w2).unwrap();

        let p_active = w1.join("2026-01-03_abcd12340001.jsonl");
        let p_arch = w1.join("2026-01-02_abcd12340002.jsonl");
        let p_other_ws = w2.join("2026-01-01_abcd12340003.jsonl");

        let write_sess = |p: &Path, id: &str, cwd: &str, archived: bool| {
            let mut f = std::fs::File::create(p).unwrap();
            writeln!(
                f,
                "{}",
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":id,"cwd":cwd,"created_at_ms":10})
            )
            .unwrap();
            writeln!(
                f,
                "{}",
                json!({"type":"message","entry_id":"u1","created_at_ms":20,"message":{"role":"user","content":"hello"}})
            )
            .unwrap();
            if archived {
                writeln!(f, "{}", json!({"type":"archive","archived":true,"created_at_ms":30})).unwrap();
            }
        };
        write_sess(&p_active, "abcd12340001", "/ws1", false);
        write_sess(&p_arch, "abcd12340002", "/ws1", true);
        write_sess(&p_other_ws, "abcd12340003", "/ws2", false);

        let ix = mare_store::index::ready(&root).unwrap();
        // Remove the other-workspace file AFTER indexing: a full `list_checked` sweep would delete
        // its row, whereas the lightweight SQLite-only page lookup leaves it alone and still
        // preserves global collision disambiguation for `--limit 1`!
        std::fs::remove_file(&p_other_ws).unwrap();
        std::fs::remove_file(&p_arch).unwrap();

        let page_ids = vec!["abcd12340001".to_string()];
        let need_paths = vec![p_active.to_string_lossy().into_owned()];
        let display = page_display_index(&root, &page_ids, &page_ids, &need_paths);
        let short_ids = unique_short_ids(display.all_ids.iter().map(String::as_str));

        // Collides on first 8 chars ("abcd1234") with archived and other-workspace sessions.
        assert_eq!(short_ids.get("abcd12340001").map(String::as_str), Some("abcd12340001"));
        assert!(display.by_key.contains_key("abcd12340001"));
        assert!(!display.by_key.contains_key("abcd12340002"));
        assert!(!display.by_key.contains_key("abcd12340003"));

        // Confirm the unrelated workspace/archived rows were NOT stat-swept or removed.
        let index = ix.lock().unwrap();
        assert_eq!(index.session_ids().unwrap().len(), 3);
    }
}

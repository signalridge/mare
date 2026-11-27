//! Shared session-address resolution for RPC (`session.resume`) and CLI (`mare session`).
//!
//! Resolves a session selector in order:
//! 1. an existing `.jsonl` file path (when `allow_path` is true);
//! 2. an exact session id;
//! 3. a unique session id prefix or case-insensitive normalized session title/name.
//!
//! Ambiguous matches and path substrings are rejected; archived sessions are rejected unless
//! `include_archived` is true.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::Result;
use mare_host::Host;
use serde_json::Value as J;

struct SessionCandidate {
    path: String,
    id: String,
    title: Option<String>,
    archived: bool,
}

/// A session selector the caller must fix (no match, ambiguous, archived, empty).
#[derive(Debug)]
pub(crate) struct SelectorError {
    pub(crate) detail: String,
    pub(crate) redacted: String,
}

impl SelectorError {
    fn same(message: String) -> Self {
        Self { detail: message.clone(), redacted: message }
    }

    fn not_found(message: String) -> Self {
        Self { detail: message.clone(), redacted: message }
    }

    fn scoped(detail: String, redacted: String) -> Self {
        Self { detail, redacted }
    }

    pub(crate) fn for_caller(&self, can_list: bool) -> &str {
        if can_list { &self.detail } else { &self.redacted }
    }
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SelectorError {}

struct SessionHit {
    path: String,
    id: String,
    title: Option<String>,
    archived: bool,
}

fn check_archived(id: &str, exact_match: bool, archived: bool, include_archived: bool) -> Result<()> {
    if archived && !include_archived {
        let short: String = id.chars().take(8).collect();
        let detail = format!("session `{short}` is archived — unarchive it first");
        if exact_match {
            return Err(SelectorError::same(detail).into());
        }
        return Err(SelectorError::scoped(detail, "that session is archived — unarchive it first".into()).into());
    }
    Ok(())
}

/// Resolve a session selector against candidates: exact id wins, otherwise unique id prefix or
/// case-insensitive normalized session name; ambiguous matches and path substrings are rejected,
/// and archived sessions are rejected unless `include_archived` is true.
fn resolve_session_candidates<I>(candidates: I, s: &str, include_archived: bool) -> Result<String>
where
    I: IntoIterator<Item = SessionCandidate>,
{
    if s.is_empty() {
        return Err(SelectorError::same("no session given".into()).into());
    }
    let norm_query = mare_store::index::normalize_title(s).to_lowercase();
    let mut hits: Vec<SessionHit> = Vec::new();
    let mut seen_paths = HashSet::new();

    for c in candidates {
        if c.path.is_empty() {
            continue;
        }
        if !c.id.is_empty() && c.id == s {
            check_archived(&c.id, true, c.archived, include_archived)?;
            return Ok(c.path);
        }
        let by_id = !c.id.is_empty() && c.id.starts_with(s);
        let by_name = !norm_query.is_empty() && c.title.as_deref().is_some_and(|t| t.to_lowercase() == norm_query);
        if (by_id || by_name) && seen_paths.insert(c.path.clone()) {
            hits.push(SessionHit { path: c.path, id: c.id, title: c.title, archived: c.archived });
        }
    }

    match hits.len() {
        0 => Err(SelectorError::not_found(format!("no session matches `{s}`")).into()),
        1 => {
            let hit = hits.into_iter().next().expect("len == 1");
            check_archived(&hit.id, hit.id == s, hit.archived, include_archived)?;
            Ok(hit.path)
        }
        n => {
            let desc: Vec<String> = hits
                .iter()
                .take(5)
                .map(|h| {
                    let short: String = h.id.chars().take(8).collect();
                    match h.title.as_deref() {
                        Some(t) if !short.is_empty() => format!("{t} ({short})"),
                        Some(t) => t.to_string(),
                        None => short,
                    }
                })
                .collect();
            let suffix = if n > 5 { ", …" } else { "" };
            let detail = format!("`{s}` matches {n} sessions: {}{suffix} — give more of the id", desc.join(", "));
            let redacted = format!("`{s}` matches more than one session — give more of the id");
            Err(SelectorError::scoped(detail, redacted).into())
        }
    }
}

fn resolve_session_from_service_entries(entries: &[J], s: &str, include_archived: bool) -> Result<String> {
    resolve_session_candidates(
        entries.iter().map(|e| {
            let id = e.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let path = e.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let title = e
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .map(mare_store::index::normalize_title);
            let archived = e.get("archived").and_then(|v| v.as_bool()).unwrap_or(false);
            SessionCandidate { path, id, title, archived }
        }),
        s,
        include_archived,
    )
}

fn is_path_like_session_selector(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.ends_with(".jsonl") || Path::new(s).is_absolute()
}

async fn list_all_session_pages(host: &Host, archived: bool) -> Result<Vec<J>> {
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = serde_json::Map::new();
        params.insert("all".into(), J::Bool(true));
        params.insert("limit".into(), J::from(200));
        if archived {
            params.insert("archived".into(), J::Bool(true));
        }
        if let Some(ref c) = cursor {
            params.insert("cursor".into(), J::String(c.clone()));
        }
        let val = crate::rpc::call_service(host, "session", "list", vec![J::Object(params)]).await?;
        let (page_entries, next_cursor) = match val {
            J::Object(mut obj) => {
                let data = obj.remove("data").and_then(|d| d.as_array().cloned()).unwrap_or_default();
                let next = obj.remove("next_cursor").and_then(|v| v.as_str().map(str::to_owned));
                (data, next)
            }
            _ => (Vec::new(), None),
        };
        entries.extend(page_entries);
        match next_cursor {
            Some(next) if !next.is_empty() && cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }
    Ok(entries)
}

/// Resolve a session address to its `.jsonl` path: an existing file path (when `allow_path` is
/// true), or an exact id, case-insensitive normalized session name, or unique id prefix resolved
/// through the `session.list` service contract.
pub(crate) async fn resolve_session(host: &Host, s: &str, allow_path: bool, include_archived: bool) -> Result<String> {
    if s.is_empty() {
        return Err(SelectorError::same("no session given".into()).into());
    }
    if is_path_like_session_selector(s) {
        if !allow_path {
            return Err(SelectorError::same(format!("no session matches `{s}`")).into());
        }
        let p = s.to_string();
        if tokio::task::spawn_blocking(move || Path::new(&p).is_file()).await? {
            return Ok(s.to_string());
        }
    }
    let mut entries = list_all_session_pages(host, false).await?;
    let archived = list_all_session_pages(host, true).await?;
    let mut seen: BTreeSet<String> =
        entries.iter().filter_map(|e| e.get("path").and_then(J::as_str).map(str::to_owned)).collect();
    for entry in archived {
        let path = entry.get("path").and_then(J::as_str).unwrap_or("");
        if path.is_empty() || seen.insert(path.to_owned()) {
            entries.push(entry);
        }
    }
    resolve_session_from_service_entries(&entries, s, include_archived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_session_from_entries_matches_id_prefix_and_name_and_rejects_ambiguity() {
        let entries = vec![
            json!({ "session_id": "aaaa1111-0000-0000-0000-000000000001", "path": "/s/one.jsonl", "name": "bbbb2222-0000-0000-0000-000000000002" }),
            json!({ "session_id": "bbbb2222-0000-0000-0000-000000000002", "path": "/s/two.jsonl", "name": "Alpha" }),
            json!({ "session_id": "aaaa9999-0000-0000-0000-000000000003", "path": "/s/three.jsonl", "name": "alpha" }),
            json!({ "session_id": "cccc3333-0000-0000-0000-000000000004", "path": "/s/four.jsonl", "name": "old-work", "archived": true }),
            json!({ "session_id": "dddd4444-0000-0000-0000-000000000005", "path": "/s/five.jsonl", "name": "dddd4444", "archived": true }),
        ];

        // Exact id wins over colliding title on entry 0.
        assert_eq!(
            resolve_session_from_service_entries(&entries, "bbbb2222-0000-0000-0000-000000000002", false).unwrap(),
            "/s/two.jsonl"
        );
        // Unique id prefix succeeds.
        assert_eq!(resolve_session_from_service_entries(&entries, "aaaa9", false).unwrap(), "/s/three.jsonl");
        // Ambiguous id prefix is rejected.
        let amb_id = resolve_session_from_service_entries(&entries, "aaaa", false).unwrap_err().to_string();
        assert!(amb_id.contains("matches 2 sessions"), "{amb_id}");
        // Ambiguous case-insensitive name is rejected.
        let amb_name = resolve_session_from_service_entries(&entries, "ALPHA", false).unwrap_err().to_string();
        assert!(amb_name.contains("matches 2 sessions"), "{amb_name}");
        // Archived session matched only by name is rejected when include_archived is false.
        let arch_err = resolve_session_from_service_entries(&entries, "old-work", false).unwrap_err().to_string();
        assert!(arch_err.contains("archived"), "{arch_err}");
        // Archived session whose name overlaps its own id prefix is also rejected when include_archived is false.
        let arch_overlap = resolve_session_from_service_entries(&entries, "dddd4444", false).unwrap_err().to_string();
        assert!(arch_overlap.contains("archived"), "{arch_overlap}");
        // include_archived = true resolves archived sessions by name and by id prefix.
        assert_eq!(resolve_session_from_service_entries(&entries, "OLD-WORK", true).unwrap(), "/s/four.jsonl");
        assert_eq!(resolve_session_from_service_entries(&entries, "dddd4444", true).unwrap(), "/s/five.jsonl");
        // Path substring must not match.
        let path_sub = resolve_session_from_service_entries(&entries, "one.jsonl", false).unwrap_err().to_string();
        assert!(path_sub.contains("no session matches"), "{path_sub}");
        // Empty input is rejected.
        assert!(resolve_session_from_service_entries(&entries, "", false).is_err());
    }

    #[test]
    fn is_path_like_session_selector_identifies_paths_and_jsonl_files() {
        assert!(is_path_like_session_selector("/etc/passwd"));
        assert!(is_path_like_session_selector("sub/session.txt"));
        assert!(is_path_like_session_selector(r"sub\session.txt"));
        assert!(is_path_like_session_selector("session.jsonl"));
        assert!(!is_path_like_session_selector("Peer-Target"));
        assert!(!is_path_like_session_selector("aaaa1111-0000-0000-0000-000000000001"));
        assert!(!is_path_like_session_selector("aaaa1111"));
    }

    #[tokio::test]
    async fn resolve_session_uses_session_list_even_for_builtin_provider() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let host = mare_host::Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();

                assert!(matches!(
                    host.kernel.service_provider("session"),
                    Some((ref name, origin)) if name == "session" && format!("{origin:?}") == "Builtin"
                ));

                let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let called_c = called.clone();
                let spy = host
                    .lua()
                    .create_function(move |lua, (_self, opts): (mlua::Value, Option<mlua::Table>)| {
                        called_c.store(true, std::sync::atomic::Ordering::SeqCst);
                        let want_archived =
                            opts.and_then(|t| t.get::<Option<bool>>("archived").ok().flatten()).unwrap_or(false);
                        let rows = if want_archived {
                            json!({ "data": [] })
                        } else {
                            json!({
                                "data": [{
                                    "session_id": "11111111-1111-1111-1111-111111111111",
                                    "path": "/spy/one.jsonl",
                                    "name": "spy-one",
                                    "archived": false
                                }]
                            })
                        };
                        mare_shared::lua::json_to_lua(lua, &rows)
                    })
                    .unwrap();
                host.service_table("session").expect("builtin session service").set("list", spy).unwrap();

                let resolved = resolve_session(&host, "spy-one", false, false).await;
                assert!(
                    called.load(std::sync::atomic::Ordering::SeqCst),
                    "resolve_session must route through session.list even for the builtin provider"
                );
                assert_eq!(resolved.unwrap(), "/spy/one.jsonl");

                host.shutdown().await;
            })
            .await;
    }
}

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use rusqlite::types::Value as Sql;

use super::scan;
use super::{Row, session_files};

/// Which kinds of session a [`Query`] returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kinds {
    /// what the picker lists: main sessions and forks
    #[default]
    Sessions,
    Main,
    Fork,
    Agent,
    Any,
}

/// A listing: newest first (`seq` descending).
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub kinds: Kinds,
    /// `Some(false)` unarchived only, `Some(true)` archived only, `None` both
    pub archived: Option<bool>,
    pub cwd: Option<String>,
    pub project_root: Option<String>,
    /// only sessions with at least one message
    pub nonempty: bool,
    pub session_id: Option<String>,
    pub session_id_prefix: Option<String>,
    /// sessions whose parent (fork origin / sub-agent owner) is this id
    pub parent_session_id: Option<String>,
    pub limit: Option<i64>,
    pub offset: i64,
}

impl Query {
    pub(super) fn filter(&self) -> (String, Vec<Sql>) {
        let mut w = String::from(" WHERE 1=1");
        let mut p: Vec<Sql> = vec![];
        w.push_str(match self.kinds {
            Kinds::Sessions => " AND kind IN ('main', 'fork')",
            Kinds::Main => " AND kind = 'main'",
            Kinds::Fork => " AND kind = 'fork'",
            Kinds::Agent => " AND kind = 'agent'",
            Kinds::Any => "",
        });
        match self.archived {
            Some(false) => w.push_str(" AND archived_at_ms IS NULL"),
            Some(true) => w.push_str(" AND archived_at_ms IS NOT NULL"),
            None => {}
        }
        let mut eq = |col: &str, v: &Option<String>| {
            if let Some(v) = v {
                w.push_str(" AND ");
                w.push_str(col);
                w.push_str(" = ?");
                p.push(Sql::Text(v.clone()));
            }
        };
        eq("cwd", &self.cwd);
        eq("project_root", &self.project_root);
        eq("session_id", &self.session_id);
        eq("parent_session_id", &self.parent_session_id);
        if let Some(pre) = &self.session_id_prefix {
            w.push_str(" AND substr(session_id, 1, ?) = ?");
            p.push(Sql::Integer(pre.chars().count() as i64));
            p.push(Sql::Text(pre.clone()));
        }
        if self.nonempty {
            w.push_str(" AND message_count > 0");
        }
        (w, p)
    }

    /// The same filter in memory (the degraded listing without a database).
    fn matches(&self, r: &Row) -> bool {
        let kind_ok = match self.kinds {
            Kinds::Sessions => r.kind != "agent",
            Kinds::Main => r.kind == "main",
            Kinds::Fork => r.kind == "fork",
            Kinds::Agent => r.kind == "agent",
            Kinds::Any => true,
        };
        let eq = |want: &Option<String>, have: Option<&str>| want.as_deref().is_none_or(|w| Some(w) == have);
        kind_ok
            && self.archived.is_none_or(|a| a == r.archived_at_ms.is_some())
            && eq(&self.cwd, Some(&r.cwd))
            && eq(&self.project_root, r.project_root.as_deref())
            && eq(&self.session_id, Some(&r.session_id))
            && eq(&self.parent_session_id, r.parent_session_id.as_deref())
            && self.session_id_prefix.as_deref().is_none_or(|p| r.session_id.starts_with(p))
            && (!self.nonempty || r.message_count > 0)
    }
}

/// The listing without a database (it can't be opened): every session file parsed, filtered
/// in memory, newest activity first. Slower; for the degraded mode only.
pub fn scan_list(root: &Path, q: &Query) -> Result<(Vec<Row>, i64)> {
    let (rows, total, _) = scan_list_with_unsupported(root, q)?;
    Ok((rows, total))
}

/// Scan without an index, retaining the paths skipped for an unsupported session version.
pub fn scan_list_with_unsupported(root: &Path, q: &Query) -> Result<(Vec<Row>, i64, Vec<String>)> {
    let mut rows = Vec::new();
    let mut unsupported = BTreeSet::new();
    for path in session_files(root) {
        match scan::parse_full(&path) {
            Ok(Some(row)) if q.matches(&row) => rows.push(row),
            Err(e) if e.to_string().contains(crate::UNSUPPORTED_VERSION) => {
                unsupported.insert(path.to_string_lossy().into_owned());
            }
            _ => {}
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at_ms));
    let total = rows.len() as i64;
    let rows =
        rows.into_iter().skip(q.offset.max(0) as usize).take(q.limit.map_or(usize::MAX, |l| l as usize)).collect();
    Ok((rows, total, unsupported.into_iter().collect()))
}

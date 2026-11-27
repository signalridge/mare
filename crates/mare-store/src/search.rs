//! Policy-independent FTS5 execution over caller-supplied, already-authorized documents.
//! Uses SQLite FTS5 unicode61 tokenization and ranking.
use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use std::sync::Arc;

const START: &str = "\u{fdd0}";
const END: &str = "\u{fdd1}";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortOrder {
    #[default]
    Relevance,
    CreatedDesc,
    CreatedAsc,
    UpdatedDesc,
    UpdatedAsc,
}

pub struct Document {
    pub session_id: String,
    pub seq: usize,
    pub text: String,
    pub timestamp: i64,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Debug)]
pub struct Hit {
    pub document: usize,
    pub snippet: String,
    pub text_offset: usize,
    pub match_count: usize,
}
#[derive(Debug)]
pub struct Page {
    pub hits: Vec<Hit>,
    pub more: bool,
}

fn sanitize(text: &str) -> String {
    text.replace(['\u{fdd0}', '\u{fdd1}'], "\u{fffd}")
}
fn excerpt(marked: &str) -> (String, usize) {
    let mut plain = String::new();
    let mut start = None;
    let mut end = None;
    let mut count: usize = 0;
    for c in marked.chars() {
        if c == '\u{fdd0}' {
            if start.is_none() {
                start = Some(count);
            }
            continue;
        }
        if c == '\u{fdd1}' {
            if end.is_none() {
                end = Some(count);
            }
            continue;
        }
        plain.push(c);
        count += 1;
    }
    let start = start.unwrap_or(0);
    let end = end.unwrap_or(start);
    (plain.chars().skip(start.saturating_sub(40)).take(end - start + 80).collect(), start)
}
/// No filesystem/index access. Query text is bound as one quoted phrase, never FTS syntax.
/// The ephemeral index is disposed on return; cancellation interrupts SQLite VM work too.
pub fn page(
    docs: &[Document],
    query: &str,
    grouped: bool,
    offset: usize,
    limit: usize,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Page> {
    page_sorted(docs, query, grouped, SortOrder::Relevance, offset, limit, cancelled)
}

/// FTS5 search with configurable sort order across sessions/events.
pub fn page_sorted(
    docs: &[Document],
    query: &str,
    grouped: bool,
    order: SortOrder,
    offset: usize,
    limit: usize,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Page> {
    if cancelled() {
        bail!("QUERY_CANCELLED: query cancelled");
    }
    let mut db = Connection::open_in_memory()?;
    let interrupted = Arc::clone(&cancelled);
    db.progress_handler(1000, Some(move || interrupted()))?;
    db.execute_batch("CREATE VIRTUAL TABLE docs USING fts5(text, ordinal UNINDEXED, session_id UNINDEXED, seq UNINDEXED, event_time UNINDEXED, codepoint_length UNINDEXED, created_at UNINDEXED, updated_at UNINDEXED, tokenize='unicode61')")?;
    {
        let transaction = db.transaction()?;
        let mut insert = transaction.prepare("INSERT INTO docs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?;
        for (ordinal, doc) in docs.iter().enumerate() {
            if cancelled() {
                bail!("QUERY_CANCELLED: query cancelled");
            }
            insert.execute(params![
                sanitize(&doc.text),
                ordinal as i64,
                doc.session_id,
                doc.seq as i64,
                doc.timestamp,
                doc.text.chars().count() as i64,
                doc.created_at,
                doc.updated_at
            ])?;
        }
        drop(insert);
        transaction.commit()?;
    }
    let phrase = format!("\"{}\"", sanitize(query).replace('"', "\"\""));
    let order_by = match order {
        SortOrder::Relevance => {
            "match_count DESC,codepoint_length ASC,event_time DESC,session_id ASC,seq DESC,ordinal ASC"
        }
        SortOrder::CreatedDesc => "created_at DESC,match_count DESC,session_id ASC,seq DESC,ordinal ASC",
        SortOrder::CreatedAsc => "created_at ASC,match_count DESC,session_id ASC,seq DESC,ordinal ASC",
        SortOrder::UpdatedDesc => "updated_at DESC,match_count DESC,session_id ASC,seq DESC,ordinal ASC",
        SortOrder::UpdatedAsc => "updated_at ASC,match_count DESC,session_id ASC,seq DESC,ordinal ASC",
    };
    let base = "WITH matched AS (SELECT ordinal,session_id,seq,event_time,codepoint_length,created_at,updated_at,highlight(docs,0,?1,?2) AS marked FROM docs WHERE docs MATCH ?3), scored AS (SELECT *, (length(CAST(marked AS BLOB))-length(CAST(replace(marked,?1,'') AS BLOB)))/length(CAST(?1 AS BLOB)) AS match_count FROM matched)";
    let select = if grouped {
        format!(
            ", ranked AS (SELECT *,ROW_NUMBER() OVER(PARTITION BY session_id ORDER BY match_count DESC,codepoint_length ASC,event_time DESC,seq DESC) AS event_rank FROM scored) SELECT ordinal,marked,match_count FROM ranked WHERE event_rank=1 ORDER BY {order_by} LIMIT ?4 OFFSET ?5"
        )
    } else {
        format!(" SELECT ordinal,marked,match_count FROM scored ORDER BY {order_by} LIMIT ?4 OFFSET ?5")
    };
    let mut statement = db.prepare(&(base.to_owned() + &select))?;
    let mut rows = statement.query(params![
        START,
        END,
        phrase,
        i64::try_from(limit.saturating_add(1))?,
        i64::try_from(offset)?
    ])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        if cancelled() {
            bail!("QUERY_CANCELLED: query cancelled");
        }
        let marked: String = row.get(1)?;
        let (snippet, text_offset) = excerpt(&marked);
        hits.push(Hit {
            document: row.get::<_, i64>(0)? as usize,
            snippet,
            text_offset,
            match_count: row.get::<_, i64>(2)? as usize,
        });
    }
    let more = hits.len() > limit;
    hits.truncate(limit);
    Ok(Page { hits, more })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn doc(id: &str, seq: usize, text: &str) -> Document {
        Document { session_id: id.into(), seq, text: text.into(), timestamp: 0, created_at: 0, updated_at: 0 }
    }
    #[test]
    fn grouped_best_hit_is_not_first_and_query_syntax_is_inert() {
        let docs =
            vec![doc("a", 1, "needle"), doc("a", 2, "needle needle"), doc("b", 1, "needle enormous unrelated text")];
        let page = page(&docs, "needle", true, 0, 20, Arc::new(|| false)).unwrap();
        assert_eq!(page.hits.len(), 2);
        assert_eq!(page.hits[0].document, 1);
        assert_eq!(page.hits[0].match_count, 2);
        assert!(super::page(&docs, "needle OR absent", true, 0, 20, Arc::new(|| false)).unwrap().hits.is_empty());
    }
    #[test]
    fn sqlite_work_is_cooperatively_interrupted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let docs: Vec<_> = (0..5000).map(|n| doc("one", n, "needle")).collect();
        let checks = Arc::new(AtomicUsize::new(0));
        let observed = checks.clone();
        let result =
            page(&docs, "needle", true, 0, 20, Arc::new(move || observed.fetch_add(1, Ordering::SeqCst) > 5050));
        assert!(result.is_err());
        assert!(
            checks.load(Ordering::SeqCst) > 5050,
            "more checks than insertion/returned rows alone requires SQLite progress callbacks"
        );
    }
    #[test]
    fn accents_highlights_and_cancellation() {
        let docs = vec![doc("a", 1, "Before Café Needle After")];
        let hit = page(&docs, "cafe needle", false, 0, 20, Arc::new(|| false)).unwrap().hits.remove(0);
        assert_eq!(hit.text_offset, 7);
        assert!(hit.snippet.contains("Café Needle"));
        assert!(
            page(&docs, "needle", false, 0, 20, Arc::new(|| true)).unwrap_err().to_string().contains("QUERY_CANCELLED")
        );
    }
    #[test]
    fn sorts_by_created_and_updated_timestamps() {
        let docs = vec![
            Document {
                session_id: "older-created-newer-updated".into(),
                seq: 1,
                text: "needle".into(),
                timestamp: 10,
                created_at: 100,
                updated_at: 500,
            },
            Document {
                session_id: "newer-created-older-updated".into(),
                seq: 1,
                text: "needle needle".into(),
                timestamp: 10,
                created_at: 200,
                updated_at: 300,
            },
        ];
        let u_desc = page_sorted(&docs, "needle", true, SortOrder::UpdatedDesc, 0, 20, Arc::new(|| false)).unwrap();
        assert_eq!(u_desc.hits.iter().map(|h| h.document).collect::<Vec<_>>(), vec![0, 1]);
        let u_asc = page_sorted(&docs, "needle", true, SortOrder::UpdatedAsc, 0, 20, Arc::new(|| false)).unwrap();
        assert_eq!(u_asc.hits.iter().map(|h| h.document).collect::<Vec<_>>(), vec![1, 0]);
        let c_desc = page_sorted(&docs, "needle", true, SortOrder::CreatedDesc, 0, 20, Arc::new(|| false)).unwrap();
        assert_eq!(c_desc.hits.iter().map(|h| h.document).collect::<Vec<_>>(), vec![1, 0]);
        let c_asc = page_sorted(&docs, "needle", true, SortOrder::CreatedAsc, 0, 20, Arc::new(|| false)).unwrap();
        assert_eq!(c_asc.hits.iter().map(|h| h.document).collect::<Vec<_>>(), vec![0, 1]);
    }
}

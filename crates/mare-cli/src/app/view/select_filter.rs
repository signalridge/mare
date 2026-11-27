//! Select popup filtering and heading skipping.

use std::cell::RefCell;
use std::sync::Arc;

use crate::app::*;

#[derive(Default)]
pub(in crate::app) struct SelectFilterCache {
    version: u64,
    inner: RefCell<Option<CachedFilter>>,
}

struct CachedFilter {
    version: u64,
    tab: usize,
    filter: String,
    visible: Arc<Vec<usize>>,
}

impl SelectFilterCache {
    pub(in crate::app) fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
        *self.inner.borrow_mut() = None;
    }

    pub(in crate::app) fn get(&self, items: &[SelectItem], tab: usize, filter: &str) -> Arc<Vec<usize>> {
        if let Some(c) = self.inner.borrow().as_ref()
            && c.version == self.version
            && c.tab == tab
            && c.filter == filter
        {
            return Arc::clone(&c.visible);
        }
        let visible = Arc::new(filter_items(items, filter));
        *self.inner.borrow_mut() = Some(CachedFilter {
            version: self.version,
            tab,
            filter: filter.to_string(),
            visible: Arc::clone(&visible),
        });
        visible
    }
}

/// The nearest selectable position from `at` in `visible`, searching down (or up) first, then
/// the other way; `at` itself when nothing is selectable.
pub(crate) fn skip_unselectable(items: &[SelectItem], visible: &[usize], at: usize, down: bool) -> usize {
    let ok = |p: usize| visible.get(p).is_some_and(|&i| items[i].is_selectable());
    if ok(at) || visible.is_empty() {
        return at;
    }
    let fwd = (at..visible.len()).find(|&p| ok(p));
    let back = (0..at.min(visible.len())).rev().find(|&p| ok(p));
    if down { fwd.or(back) } else { back.or(fwd) }.unwrap_or(at)
}

fn fill_hay<'a>(buf: &'a mut String, it: &'a SelectItem) -> &'a str {
    match &it.detail {
        Some(d) => {
            buf.clear();
            buf.push_str(&it.label);
            buf.push(' ');
            buf.push_str(d);
            buf.as_str()
        }
        None => it.label.as_str(),
    }
}

/// Popup filtering: fuzzy (nucleo, like fzf) over label + detail, best matches first; ties keep
/// the original order. Friendly model names live in `detail`, so they match too.
/// Rows matching the search text. Fuzzy by default; `re:<regex>` is a case-insensitive regular
/// expression and `"exact phrase"` a case-insensitive substring (the /resume and
/// /tree searches). Group headings only show without a search.
pub(crate) fn filter_items(items: &[SelectItem], filter: &str) -> Vec<usize> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};
    if filter.trim().is_empty() {
        return (0..items.len()).collect();
    }
    let mut hay_buf = String::new();
    if let Some(re) = filter.strip_prefix("re:") {
        // an unfinished pattern (`re:foo(`) matches nothing rather than everything
        let Ok(re) = regex::RegexBuilder::new(re).case_insensitive(true).build() else { return vec![] };
        return items
            .iter()
            .enumerate()
            .filter(|(_, it)| !it.header && re.is_match(fill_hay(&mut hay_buf, it)))
            .map(|(i, _)| i)
            .collect();
    }
    if let Some(q) = filter.strip_prefix('"') {
        let q = q.strip_suffix('"').unwrap_or(q).to_lowercase();
        return items
            .iter()
            .enumerate()
            .filter(|(_, it)| !it.header && fill_hay(&mut hay_buf, it).to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
    }
    let pat = Pattern::parse(filter, CaseMatching::Smart, Normalization::Smart);
    let mut m = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| !it.header)
        .filter_map(|(i, it)| {
            let hay = fill_hay(&mut hay_buf, it);
            pat.score(Utf32Str::new(hay, &mut buf), &mut m).map(|s| (s, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_filter_cache_hits_and_invalidates_on_query_tab_and_version() {
        let items = vec![
            SelectItem { label: "alpha".into(), detail: Some("first".into()), ..Default::default() },
            SelectItem { label: "beta".into(), detail: Some("second".into()), ..Default::default() },
            SelectItem { label: "gamma".into(), detail: Some("third".into()), ..Default::default() },
        ];
        let mut cache = SelectFilterCache::default();
        let a1 = cache.get(&items, 0, "a");
        let a2 = cache.get(&items, 0, "a");
        assert!(Arc::ptr_eq(&a1, &a2), "same (version, tab, filter) must reuse cached Arc");
        // Tab change misses and caches new tab.
        let b1 = cache.get(&items, 1, "a");
        assert!(!Arc::ptr_eq(&a1, &b1));
        // Filter change misses.
        let c1 = cache.get(&items, 0, "bet");
        assert_eq!(*c1, vec![1]);
        // Version bump invalidates.
        let prev = cache.get(&items, 0, "bet");
        assert!(Arc::ptr_eq(&c1, &prev));
        cache.bump();
        let after = cache.get(&items, 0, "bet");
        assert!(!Arc::ptr_eq(&c1, &after));
        assert_eq!(*after, vec![1]);
    }

    #[test]
    fn filter_items_supports_regex_quoted_phrase_and_skips_headers() {
        let items = vec![
            SelectItem { label: "Today".into(), header: true, ..Default::default() },
            SelectItem { label: "Fix parser".into(), detail: Some("alpha beta".into()), ..Default::default() },
            SelectItem { label: "Add tests".into(), detail: Some("exact phrase here".into()), ..Default::default() },
            SelectItem { label: "Refactor".into(), detail: Some("gamma-42".into()), ..Default::default() },
        ];
        // Empty filter preserves headers and all rows.
        assert_eq!(filter_items(&items, ""), vec![0, 1, 2, 3]);
        // Regex (`re:`) is case-insensitive and skips headers.
        assert_eq!(filter_items(&items, "re:^fix|gamma-\\d+$"), vec![1, 3]);
        assert_eq!(filter_items(&items, "re:today"), Vec::<usize>::new());
        // Invalid/unfinished regex matches nothing rather than everything.
        assert_eq!(filter_items(&items, "re:foo("), Vec::<usize>::new());
        // Quoted phrase (`"..."`, closed or unclosed) is a case-insensitive substring match skipping headers.
        assert_eq!(filter_items(&items, "\"EXACT PHRASE\""), vec![2]);
        assert_eq!(filter_items(&items, "\"alpha beta"), vec![1]);
        assert_eq!(filter_items(&items, "\"today\""), Vec::<usize>::new());
    }
}

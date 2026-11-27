//! General byte decoding and bounded HTML conversion, independent of retrieval providers.
use mare_shared::lua::rt_err;
use mlua::{Lua, Table};

const MAX_INPUT: usize = 5_000_000;
const MAX_HTML_INPUT: usize = MAX_INPUT * 4;
const MAX_OUTPUT: usize = 1_000_000;
const MAX_TAGS: usize = 100_000;
const MAX_NODES: usize = 50_000;
const MAX_DEPTH: usize = 128;
static HTML_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn resolve_encoding(charset: &str) -> Result<&'static encoding_rs::Encoding, &'static str> {
    if let Some(encoding) = encoding_rs::Encoding::for_label(charset.as_bytes()) {
        return Ok(encoding);
    }
    if charset
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .any(|part| encoding_rs::Encoding::for_label(part.as_bytes()) == Some(encoding_rs::UTF_8))
    {
        return Ok(encoding_rs::UTF_8);
    }
    Err("unknown charset")
}

fn decode(bytes: &[u8], charset: &str, final_chunk: bool) -> Result<String, &'static str> {
    if bytes.len() > MAX_INPUT {
        return Err("text input exceeds 5000000 bytes");
    }
    let encoding = resolve_encoding(charset)?;
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let capacity =
        decoder.max_utf8_buffer_length_without_replacement(bytes.len()).ok_or("decoded text exceeds capacity")?;
    let mut text = String::with_capacity(capacity);
    let (result, read) = decoder.decode_to_string_without_replacement(bytes, &mut text, final_chunk);
    if result != encoding_rs::DecoderResult::InputEmpty || read != bytes.len() {
        return Err("malformed text for declared charset");
    }
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

// Tags whose subtrees are unconditionally omitted when converting HTML to Markdown.
const ALWAYS_NON_VISIBLE_TAGS: &[&str] =
    &["script", "style", "head", "noscript", "template", "iframe", "object", "embed"];

// Elements omitted when converting HTML to Markdown.
fn non_visible(node: &htmd::node::Node<'_>) -> bool {
    let Some(tag) = node.tag_name() else { return false };
    if ALWAYS_NON_VISIBLE_TAGS.contains(&tag) {
        return true;
    }
    node.attrs().iter().any(|(key, value)| {
        key == "hidden"
            || (key == "aria-hidden" && value.eq_ignore_ascii_case("true"))
            || (tag == "input" && key == "type" && value.eq_ignore_ascii_case("hidden"))
            || (key == "style"
                && value.split(';').any(|declaration| {
                    let Some((property, value)) = declaration.split_once(':') else { return false };
                    let property = property.trim();
                    let value = value.trim().to_ascii_lowercase();
                    let value = value.strip_suffix("!important").unwrap_or(&value).trim_end();
                    (property.eq_ignore_ascii_case("display") && value == "none")
                        || (property.eq_ignore_ascii_case("visibility") && matches!(value, "hidden" | "collapse"))
                }))
    })
}

fn table_alignment(cell: &htmd::node::Node<'_>) -> &'static str {
    let mut align = None;
    for (key, value) in cell.attrs().iter() {
        if key == "align" {
            align = Some(value.trim().to_ascii_lowercase());
            break;
        }
        if key == "style" {
            for decl in value.split(';') {
                if let Some((prop, val)) = decl.split_once(':')
                    && prop.trim().eq_ignore_ascii_case("text-align")
                {
                    let val = val.trim().to_ascii_lowercase();
                    let val = val.strip_suffix("!important").unwrap_or(&val).trim_end().to_owned();
                    align = Some(val);
                }
            }
        }
    }
    match align.as_deref() {
        Some("left") => ":---",
        Some("right") => "---:",
        Some("center") => ":---:",
        _ => "---",
    }
}

fn format_table_cell(content: &str) -> String {
    let trimmed = content.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_cr = false;
    for ch in trimmed.chars() {
        match ch {
            '\r' => {
                out.push_str("<br>");
                prev_cr = true;
            }
            '\n' => {
                if !prev_cr {
                    out.push_str("<br>");
                }
                prev_cr = false;
            }
            '|' => {
                out.push_str("\\|");
                prev_cr = false;
            }
            _ => {
                out.push(ch);
                prev_cr = false;
            }
        }
    }
    out
}

struct TableBudget {
    remaining: std::sync::atomic::AtomicUsize,
    truncated: std::sync::atomic::AtomicBool,
    cells_written: std::sync::atomic::AtomicUsize,
}

fn append_bounded_row<'a>(
    out: &mut String,
    cols: usize,
    limit: usize,
    cells_written: &std::sync::atomic::AtomicUsize,
    mut cell_at: impl FnMut(usize) -> &'a str,
) -> bool {
    out.push('|');
    for i in 0..cols {
        if out.len() >= limit {
            return false;
        }
        out.push(' ');
        out.push_str(cell_at(i));
        out.push_str(" |");
        cells_written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    out.push('\n');
    out.len() < limit
}

fn extract_tr_cells(
    handlers: &dyn htmd::element_handler::Handlers,
    tr: &htmd::node::Node<'_>,
) -> (Vec<String>, Vec<&'static str>, bool) {
    let mut cells = Vec::new();
    let mut aligns = Vec::new();
    let mut all_th = true;
    for child in tr.children().iter() {
        if non_visible(&child) {
            continue;
        }
        let Some(tag) = child.tag_name() else {
            continue;
        };
        if tag != "th" && tag != "td" {
            continue;
        }
        if tag != "th" {
            all_th = false;
        }
        aligns.push(table_alignment(&child));
        let rendered =
            handlers.handle(&child, htmd::element_handler::Context::BLOCK).map(|r| r.content).unwrap_or_default();
        cells.push(format_table_cell(&rendered));
    }
    let header_only = all_th && !cells.is_empty();
    (cells, aligns, header_only)
}

fn render_gfm_table(
    handlers: &dyn htmd::element_handler::Handlers,
    element: &htmd::Element<'_>,
    budget: &TableBudget,
) -> Option<htmd::element_handler::HandlerResult> {
    if non_visible(element.node) {
        return None;
    }
    let limit = budget.remaining.load(std::sync::atomic::Ordering::Relaxed);
    if limit == 0 {
        budget.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
        return Some(String::new().into());
    }
    let mut captions = Vec::new();
    let mut rows: Vec<(Vec<String>, Vec<&'static str>, bool)> = Vec::new();
    for child in element.node.children().iter() {
        if non_visible(&child) {
            continue;
        }
        let Some(tag) = child.tag_name() else {
            continue;
        };
        match tag {
            "caption" => {
                if let Some(res) = handlers.handle(&child, htmd::element_handler::Context::BLOCK) {
                    let cap = res.content.trim().to_owned();
                    if !cap.is_empty() {
                        captions.push(cap);
                    }
                }
            }
            "thead" | "tbody" | "tfoot" => {
                let in_thead = tag == "thead";
                for row_node in child.children().iter() {
                    if !non_visible(&row_node) && row_node.tag_name() == Some("tr") {
                        let (cells, aligns, all_th) = extract_tr_cells(handlers, &row_node);
                        if !cells.is_empty() {
                            rows.push((cells, aligns, in_thead || all_th));
                        }
                    }
                }
            }
            "tr" => {
                let (cells, aligns, all_th) = extract_tr_cells(handlers, &child);
                if !cells.is_empty() {
                    rows.push((cells, aligns, all_th));
                }
            }
            _ => {}
        }
    }
    if rows.is_empty() && captions.is_empty() {
        return None;
    }
    let cols = rows.iter().map(|(c, _, _)| c.len()).max().unwrap_or(0);
    if cols == 0 {
        let fallback = captions.join("\n");
        return if fallback.is_empty() { None } else { Some(format!("\n\n{fallback}\n\n").into()) };
    }
    let mut out = String::from("\n\n");
    for cap in captions {
        if out.len() >= limit {
            budget.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
            break;
        }
        out.push_str(&cap);
        out.push('\n');
    }
    let has_header = rows.first().is_some_and(|(_, _, is_header)| *is_header);
    let mut hit_limit = false;
    for (idx, (cells, aligns, _)) in rows.iter().enumerate() {
        if !append_bounded_row(&mut out, cols, limit, &budget.cells_written, |i| {
            cells.get(i).map(String::as_str).unwrap_or("")
        }) {
            hit_limit = true;
            break;
        }
        if idx == 0
            && has_header
            && !append_bounded_row(&mut out, cols, limit, &budget.cells_written, |i| {
                aligns.get(i).copied().unwrap_or("---")
            })
        {
            hit_limit = true;
            break;
        }
    }
    if hit_limit {
        budget.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
    } else {
        out.push('\n');
    }
    let used = out.len();
    let _ = budget.remaining.try_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |cur| Some(cur.saturating_sub(used)),
    );
    Some(out.into())
}

#[cfg_attr(not(test), allow(dead_code))]
struct HtmlStats {
    pre_cut_len: usize,
    table_cells_written: usize,
}

fn html(
    input: &str,
    max_input: usize,
    max_output: usize,
    max_tags: usize,
    max_nodes: usize,
    max_depth: usize,
) -> Result<(String, bool), String> {
    let (md, truncated, _) = html_with_stats(input, max_input, max_output, max_tags, max_nodes, max_depth)?;
    Ok((md, truncated))
}

const EXPOSED_TEMPLATE_TAG: &str = "x-mare-template";

fn check_dom_budget(
    root: &htmd::node::Node<'_>,
    max_nodes: usize,
    max_depth: usize,
    exposed_template_tag: Option<&str>,
) -> Result<(bool, std::collections::BTreeSet<String>), String> {
    let mut pending = vec![(root.clone(), 0usize)];
    let mut count = 0usize;
    let mut has_template = false;
    let mut hidden_tags = std::collections::BTreeSet::new();
    while let Some((node, depth)) = pending.pop() {
        count += 1;
        if count > max_nodes {
            return Err("HTML exceeds max_nodes complexity budget".into());
        }
        if depth > max_depth {
            return Err("HTML exceeds max_depth complexity budget".into());
        }
        let mut child_depth = depth + 1;
        if let Some(tag) = node.tag_name() {
            if tag == "template" {
                has_template = true;
            }
            if exposed_template_tag == Some(tag) {
                // Each `<template>` owns a DocumentFragment node at `depth + 1`, with its
                // parsed contents starting at `depth + 2`.
                count += 1;
                if count > max_nodes {
                    return Err("HTML exceeds max_nodes complexity budget".into());
                }
                if child_depth > max_depth {
                    return Err("HTML exceeds max_depth complexity budget".into());
                }
                child_depth += 1;
            }
            if exposed_template_tag.is_none()
                && !ALWAYS_NON_VISIBLE_TAGS.contains(&tag)
                && tag != "table"
                && non_visible(&node)
            {
                hidden_tags.insert(tag.to_owned());
            }
        }
        let mut child = node.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            pending.push((c, child_depth));
        }
    }
    Ok((has_template, hidden_tags))
}

// htmd 0.6's ego-tree backend stores `<template>` contents in a private DocumentFragment
// unreachable through `Node::children()`. Rewrite `<template...>` / `</template...>` tags
// solely for a budget-counting parse (counting each template's DocumentFragment node and
// extra depth level) so template subtrees match real DOM budgets; Markdown conversion always
// runs on the original unmodified tree.
fn expose_templates(input: &str) -> String {
    const TAG: &[u8] = b"template";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len() + 32);
    let mut i = 0;
    let mut last = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let mut j = i + 1;
            if bytes.get(j) == Some(&b'/') {
                j += 1;
            }
            if bytes.get(j..j + TAG.len()).is_some_and(|s| s.eq_ignore_ascii_case(TAG))
                && matches!(
                    bytes.get(j + TAG.len()).copied(),
                    None | Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
                )
            {
                out.push_str(&input[last..j]);
                out.push_str("x-mare-");
                last = j;
                i = j + TAG.len();
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&input[last..]);
    out
}

fn html_with_stats(
    input: &str,
    max_input: usize,
    max_output: usize,
    max_tags: usize,
    max_nodes: usize,
    max_depth: usize,
) -> Result<(String, bool, HtmlStats), String> {
    if input.len() > max_input {
        return Err("HTML input exceeds max_bytes".into());
    }
    // Coarse parser work budget before DOM allocation. Counting all '<' also counts
    // text/comments is conservative; actual depth is checked on the parsed DOM below.
    if input.bytes().filter(|b| *b == b'<').take(max_tags + 1).count() > max_tags {
        return Err("HTML exceeds max_tags complexity budget".into());
    }
    let parser = htmd::HtmlToMarkdown::new();
    let tree = parser.html_to_tree(input).map_err(|e| e.to_string())?;
    // Count the entire parsed tree first: hidden subtrees do not evade resource
    // ceilings. Collect tag names that actually carry non-visible attributes so the
    // immutable ego-tree walk can skip those subtrees without regex over HTML.
    let (has_template, hidden_tags) = check_dom_budget(&tree.root(), max_nodes, max_depth, None)?;
    if has_template {
        let exposed = expose_templates(input);
        let exposed_tree = parser.html_to_tree(&exposed).map_err(|e| e.to_string())?;
        check_dom_budget(&exposed_tree.root(), max_nodes, max_depth, Some(EXPOSED_TEMPLATE_TAG))?;
    }
    let budget = std::sync::Arc::new(TableBudget {
        remaining: std::sync::atomic::AtomicUsize::new(max_output.saturating_add(1)),
        truncated: std::sync::atomic::AtomicBool::new(false),
        cells_written: std::sync::atomic::AtomicUsize::new(0),
    });
    let budget_handler = budget.clone();
    let mut builder = htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ..Default::default()
        })
        .skip_tags(ALWAYS_NON_VISIBLE_TAGS.to_vec())
        .add_handler(
            vec!["table"],
            move |handlers: &dyn htmd::element_handler::Handlers, element: &htmd::Element<'_>| {
                render_gfm_table(handlers, element, &budget_handler)
            },
        );
    if !hidden_tags.is_empty() {
        let tag_refs: Vec<&str> = hidden_tags.iter().map(String::as_str).collect();
        builder = builder.add_handler(
            tag_refs,
            |handlers: &dyn htmd::element_handler::Handlers, element: &htmd::Element<'_>| {
                if non_visible(element.node) { None } else { handlers.fallback(element) }
            },
        );
    }
    let converter = builder.build();
    let output = converter.convert(&tree).map_err(|e| e.to_string())?;
    let pre_cut_len = output.len();
    let table_cells_written = budget.cells_written.load(std::sync::atomic::Ordering::Relaxed);
    let truncated = pre_cut_len > max_output || budget.truncated.load(std::sync::atomic::Ordering::Relaxed);
    Ok((
        String::from_utf8_lossy(super::cut(output.as_bytes(), max_output, false, false)).into_owned(),
        truncated,
        HtmlStats { pre_cut_len, table_cells_written },
    ))
}

pub(super) fn install(lua: &Lua, t: &Table) -> mlua::Result<()> {
    t.set(
        "decode",
        lua.create_function(|_, (bytes, charset, opts): (mlua::LuaString, Option<String>, Option<Table>)| {
            let final_chunk = opts.map(|o| o.get::<Option<bool>>("final")).transpose()?.flatten().unwrap_or(true);
            decode(&bytes.as_bytes(), charset.as_deref().unwrap_or("utf-8"), final_chunk).map_err(rt_err)
        })?,
    )?;
    t.set(
        "html_to_markdown",
        lua.create_async_function(|_, (input, opts): (String, Option<Table>)| {
            let get = |key, default, ceiling| -> mlua::Result<usize> {
                let n = opts.as_ref().map(|o| o.get::<Option<usize>>(key)).transpose()?.flatten().unwrap_or(default);
                if n == 0 || n > ceiling {
                    return Err(rt_err(format!("{key} must be in 1..={ceiling}")));
                }
                Ok(n)
            };
            let limits = (
                get("max_bytes", MAX_HTML_INPUT, MAX_HTML_INPUT),
                get("max_output_bytes", MAX_OUTPUT, MAX_OUTPUT),
                get("max_tags", MAX_TAGS, MAX_TAGS),
                get("max_nodes", MAX_NODES, MAX_NODES),
                get("max_depth", MAX_DEPTH, MAX_DEPTH),
            );
            async move {
                let (a, b, c, d, e) = (limits.0?, limits.1?, limits.2?, limits.3?, limits.4?);
                let permit = HTML_WORKERS.acquire().await.map_err(rt_err)?;
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    html(&input, a, b, c, d, e)
                })
                .await
                .map_err(rt_err)?
                .map_err(rt_err)
            }
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_text_decode_is_strict_and_supports_declared_charsets() {
        assert_eq!(decode(b"caf\xe9", "windows-1252", true).unwrap(), "café");
        assert_eq!(decode(b"\xff\xfeh\0i\0", "utf-16le", true).unwrap(), "hi");
        assert!(decode(b"\xff", "utf-8", true).is_err());
        assert!(decode(b"hi", "unknown", true).is_err());
    }
    #[test]
    fn web_html_filters_declared_hidden_subtrees_without_interpreting_visible_text() {
        let input = r#"<main>
            <p>visible before</p><section hidden="false"><p>hidden boolean child</p></section>
            <aside aria-hidden="TRUE">hidden aria</aside><input type="HIDDEN" value="hidden input">
            <div style="DISPLAY : NoNe !IMPORTANT">hidden display</div>
            <p style="color:red; visibility: hidden">hidden visibility</p>
            <span style="visibility:COLLAPSE!important">hidden collapse</span>
            <iframe>hidden frame</iframe><object>hidden object</object><embed src="hidden embed">
            <p aria-hidden="false" style="display:block;visibility:visible">visible after</p>
            <pre>&lt;p hidden&gt;visible literal&lt;/p&gt;</pre>
        </main>"#;
        let (markdown, truncated) = html(input, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        for visible in ["visible before", "visible after", "visible literal"] {
            assert!(markdown.contains(visible), "missing {visible}: {markdown}");
        }
        for hidden in [
            "hidden boolean child",
            "hidden aria",
            "hidden input",
            "hidden display",
            "hidden visibility",
            "hidden collapse",
            "hidden frame",
            "hidden object",
            "hidden embed",
        ] {
            assert!(!markdown.contains(hidden), "leaked {hidden}: {markdown}");
        }
        assert!(!truncated);
        let hidden_flood = format!("<section hidden>{}</section>", "<p>hidden</p>".repeat(100));
        assert!(html(&hidden_flood, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 50, MAX_DEPTH).unwrap_err().contains("max_nodes"));
        let template_flood = format!("<template>{}</template>", "<p>hidden</p>".repeat(100));
        assert!(
            html(&template_flood, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 50, MAX_DEPTH).unwrap_err().contains("max_nodes")
        );
    }
    #[test]
    fn web_html_removes_hidden_content_preserves_tables_and_bounds_work() {
        let (text, truncated) = html("<h1>Title</h1><script>secret</script><style>css</style><table><tr><th>A</th></tr><tr><td>B</td></tr></table>", MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert!(text.contains("Title") && text.contains("| A |") && text.contains("| B |"), "{text}");
        assert!(!text.contains("secret") && !text.contains("css") && !truncated);
        assert!(html(&"<div>".repeat(257), MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).is_err());
        assert_eq!(html("界界", MAX_INPUT, 4, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap(), ("界".into(), true));
        assert!(!html("text", MAX_INPUT, 4, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap().1);
    }
}

#[cfg(test)]
mod broad_pages {
    use super::*;
    #[test]
    fn web_html_accepts_wide_pages_and_rejects_node_floods() {
        let page = format!("<main>{}</main>", "<p>ordinary paragraph</p>".repeat(2000));
        let (md, cut) = html(&page, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert_eq!(md.matches("ordinary paragraph").count(), 2000);
        assert!(!cut);
        assert!(html(&page, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 100, MAX_DEPTH).unwrap_err().contains("max_nodes"));
    }
}

#[cfg(test)]
mod decoding_prefixes {
    use super::*;
    #[test]
    fn web_decode_partial_tail_is_not_a_malformed_sequence() {
        for (bytes, encoding, expected) in [
            (b"a\xe7\x95".as_slice(), "utf-8", "a"),
            (b"a\0b".as_slice(), "utf-16le", "a"),
            (b"a\0\0\xd8".as_slice(), "utf-16le", "a"),
            (b"A\x82".as_slice(), "shift_jis", "A"),
        ] {
            assert_eq!(decode(bytes, encoding, false).unwrap(), expected);
            assert!(decode(bytes, encoding, true).is_err());
        }
        for (bytes, encoding) in [
            (b"\xe0\x80".as_slice(), "utf-8"),
            (b"\0\xd8A\0".as_slice(), "utf-16le"),
            (b"\x82 ".as_slice(), "shift_jis"),
        ] {
            assert!(decode(bytes, encoding, false).is_err());
        }
        assert_eq!(decode("café".as_bytes(), "utf-8, latin1", true).unwrap(), "café");
        assert!(decode(b"\xff", "utf-8, latin1", true).is_err());
    }

    #[test]
    fn web_html_table_and_max_tags_parity() {
        let table = r#"<table>
            <thead><tr><th align="left">Col1</th><th style="text-align: right">Col2</th><th align="center">Col3</th></tr></thead>
            <tbody>
                <tr><td colspan="999999">a|b</td><td>line1<br>line2</td><td>mid</td></tr>
                <tr style="display:none"><td>hidden</td><td>row</td><td>x</td></tr>
            </tbody>
        </table>
        <table><tr><td>headerless-1</td><td>headerless-2</td></tr></table>
        <table><tr><th>Key</th><td>Val</td></tr></table>
        <ul><li>item</li></ul>"#;
        let (md, truncated) = html(table, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert!(!truncated);
        assert!(md.contains("| Col1 | Col2 | Col3 |"), "{md}");
        assert!(md.contains("| :--- | ---: | :---: |"), "{md}");
        assert!(md.contains("| a\\|b |"), "{md}");
        assert!(md.contains("line1") && md.contains("line2"), "{md}");
        assert!(!md.contains("hidden"), "{md}");
        assert!(md.contains("| headerless-1 | headerless-2 |"), "{md}");
        assert!(md.contains("| Key | Val |"), "{md}");
        assert!(md.contains("- item"), "{md}");
        assert!(
            html("<p>a</p><p>b</p>", MAX_INPUT, MAX_OUTPUT, 3, MAX_NODES, MAX_DEPTH).unwrap_err().contains("max_tags")
        );
    }

    #[test]
    fn wide_sparse_html_table_respects_output_budget() {
        // ~324 KB page: 1 row with 12,000 empty cells + 12,000 rows with 1 empty cell.
        // Well under MAX_INPUT (5 MB), MAX_TAGS (100k), MAX_NODES (50k), but naive
        // rectangular padding to max_cols would allocate ~432 MB.
        let wide_row = format!("<tr>{}</tr>", "<td></td>".repeat(12_000));
        let narrow_rows = "<tr><td></td></tr>".repeat(12_000);
        let page = format!("<table>{wide_row}{narrow_rows}</table>");
        assert!(page.len() < 400_000, "unexpected page size: {}", page.len());

        let (md, truncated, stats) =
            html_with_stats(&page, MAX_INPUT, 100_000, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();

        assert!(truncated, "wide sparse table must be marked truncated");
        assert!(md.len() <= 100_000, "output exceeded budget: {}", md.len());
        assert!(
            stats.pre_cut_len <= 102_000,
            "intermediate markdown exceeded generation budget before cut: {}",
            stats.pre_cut_len
        );
        assert!(
            stats.table_cells_written <= 40_000,
            "table cell work exceeded generation budget: {}",
            stats.table_cells_written
        );
    }

    #[test]
    fn multibyte_expansion_under_raw_limit_succeeds() {
        // 3 MB of Windows-1252 0xe9 ('é') is under MAX_INPUT (5 MB) raw bytes,
        // but expands to ~6 MB in UTF-8 after decoding.
        let mut raw = b"<p>".to_vec();
        raw.extend(std::iter::repeat_n(0xe9u8, 3_000_000));
        raw.extend_from_slice(b"</p>");
        let decoded = decode(&raw, "windows-1252", true).unwrap();
        assert!(decoded.len() > MAX_INPUT, "expected UTF-8 expansion > MAX_INPUT, got {}", decoded.len());
        let (md, truncated) = html(&decoded, MAX_HTML_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert!(truncated);
        assert!(md.starts_with("é"));
    }

    #[test]
    fn web_html_filters_non_visible_elements_inside_tables_and_custom_tags() {
        let input = r#"
        <table hidden><tr><th>HiddenTable</th></tr><tr><td>SecretTableCell</td></tr></table>
        <table>
            <caption hidden>Secret Caption</caption>
            <caption>Sales <span style="display:none">secret-cap</span> Summary</caption>
            <thead style="display:none"><tr><th>BadHead</th></tr></thead>
            <thead>
                <tr>
                    <th align="left">Item</th>
                    <th hidden>HiddenCol</th>
                    <th style="text-align: right !important">Qty</th>
                    <th style="text-align: center">Note</th>
                </tr>
            </thead>
            <tbody aria-hidden="true"><tr><td>HiddenTbody</td></tr></tbody>
            <tbody>
                <tr>
                    <td>A|B</td>
                    <td style="visibility:hidden">drop-cell</td>
                    <td>10</td>
                    <td>ok <em hidden>secret-inline</em><em>shown</em></td>
                </tr>
                <tr style="display:none"><td>HiddenRow</td><td>99</td><td>x</td></tr>
            </tbody>
            <tfoot>
                <tr>
                    <td>Total</td>
                    <td>10</td>
                    <td><span style="visibility:collapse">secret-foot</span>done</td>
                </tr>
            </tfoot>
        </table>
        <custom-banner hidden>secret-custom</custom-banner>
        <custom-banner>visible-custom</custom-banner>
        <p><em hidden>secret-em</em><em>kept-em</em> <span style="display:none">secret-span</span><span>kept-span</span></p>
        "#;
        let (md, truncated) = html(input, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert!(!truncated);
        for visible in [
            "Sales",
            "Summary",
            "| Item | Qty | Note |",
            "| :--- | ---: | :---: |",
            "| A\\|B | 10 | ok *shown* |",
            "| Total | 10 | done |",
            "visible-custom",
            "*kept-em*",
            "kept-span",
        ] {
            assert!(md.contains(visible), "missing {visible} in:\n{md}");
        }
        for hidden in [
            "HiddenTable",
            "SecretTableCell",
            "Secret Caption",
            "secret-cap",
            "BadHead",
            "HiddenCol",
            "HiddenTbody",
            "drop-cell",
            "secret-inline",
            "HiddenRow",
            "secret-foot",
            "secret-custom",
            "secret-em",
            "secret-span",
        ] {
            assert!(!md.contains(hidden), "leaked {hidden} in:\n{md}");
        }
    }

    #[test]
    fn web_html_template_depth_and_utf8_boundaries_are_enforced() {
        let utf8_with_template = "<p>日本語</p><template><template><p>秘密</p></template></template><p>表示</p>";
        let (md, truncated) = html(utf8_with_template, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, MAX_DEPTH).unwrap();
        assert!(!truncated);
        assert!(md.contains("日本語") && md.contains("表示"), "{md}");
        assert!(!md.contains("秘密"), "{md}");

        // Exact DocumentFragment accounting:
        // Document(0) -> html(1) -> {head(2), body(2) -> template(3) -> frag(4) -> p(5) -> text(6)}
        // = 8 nodes, max depth 6.
        let single_tpl = "<body><template><p>x</p></template></body>";
        assert!(html(single_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 7, MAX_DEPTH).unwrap_err().contains("max_nodes"));
        assert!(html(single_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 8, MAX_DEPTH).is_ok());
        assert!(html(single_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, 5).unwrap_err().contains("max_depth"));
        assert!(html(single_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, 6).is_ok());

        // Nested templates add one DocumentFragment node and one depth level per template:
        // Document(0) -> html(1) -> {head(2), body(2) -> tpl(3) -> frag(4) -> tpl(5) -> frag(6) -> p(7) -> text(8)}
        // = 10 nodes, max depth 8.
        let nested_tpl = "<body><template><template><p>x</p></template></template></body>";
        assert!(html(nested_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 9, MAX_DEPTH).unwrap_err().contains("max_nodes"));
        assert!(html(nested_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, 10, MAX_DEPTH).is_ok());
        assert!(html(nested_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, 7).unwrap_err().contains("max_depth"));
        assert!(html(nested_tpl, MAX_INPUT, MAX_OUTPUT, MAX_TAGS, MAX_NODES, 8).is_ok());
    }
}

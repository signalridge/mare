//! Lua → `Node` conversion for `mare.ui`: styles, spans, lines and component trees built by the
//! prelude (`ui.Line`, `ui.Span`, ...). Split out of `ui.rs` (module size rule); `ui` re-exports
//! the public entry points.

use mlua::{Table, Value};

use mare_shared::style::{Color, Line, Span, Style, Theme, theme_name_id};
use mare_shared::ui::Node;

// Keep in sync with the renderer's defensive limit in mare-tui/src/render.rs.
const MAX_GAP_LINES: usize = 1_000;

// ----------------------------------------------------------------------
// Lua → Node conversion
// ----------------------------------------------------------------------

fn color_ref(v: Value) -> Option<Color> {
    match v {
        Value::String(s) => {
            let name = s.to_str().ok()?;
            Some(Color::parse(&name).unwrap_or_else(|| Color::Theme(theme_name_id(&name))))
        }
        Value::Integer(n) => Some(Color::Ansi(n as u8)),
        _ => None,
    }
}

pub fn style_from(_theme: &Theme, v: &Value) -> Style {
    let mut st = Style::default();
    let Value::Table(t) = v else {
        if let Value::String(s) = v
            && let Ok(s) = s.to_str()
        {
            // a named style ("MareToolName", or a short alias like "accent")
            return Style::named(&s);
        }
        return st;
    };
    // {hl = "MareX", bold = true, ...}: start from a named style, then patch
    if let Ok(Some(name)) = t.get::<Option<String>>("hl") {
        st = Style::named(&name);
    }
    if let Some(c) = t.get::<Value>("fg").ok().and_then(color_ref) {
        st.fg = Some(c);
    }
    if let Some(c) = t.get::<Value>("bg").ok().and_then(color_ref) {
        st.bg = Some(c);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("bold") {
        st = st.with_bold(v);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("dim") {
        st = st.with_dim(v);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("italic") {
        st = st.with_italic(v);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("underline") {
        st = st.with_underline(v);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("strike") {
        st = st.with_strike(v);
    }
    if let Ok(Some(v)) = t.get::<Option<bool>>("reverse") {
        st = st.with_reverse(v);
    }
    // link = "https://…": store validated http(s) URLs unconditionally; Style::resolve gates
    // emission on the live `theme.md.osc8` so late terminal capability probes take effect.
    if let Ok(Some(url)) = t.get::<Option<String>>("link") {
        st = st.link(&url);
    }
    st
}

/// Text from Lua → spans. Untrusted text may carry SGR colors: they are parsed into styled spans
/// (layered on the span's style); every other escape is removed here.
fn text_spans(text: &str, style: Style) -> Vec<Span> {
    let text = mare_shared::style::expand_tabs(text);
    if !text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return vec![Span::styled(text, style)];
    }
    let clean = mare_shared::ansi::sanitize(&text, true);
    mare_shared::ansi::sgr_spans(&clean, style)
}

fn span_from(theme: &Theme, v: Value) -> mlua::Result<Vec<Span>> {
    Ok(match v {
        Value::String(s) => text_spans(&s.to_str()?, Style::default()),
        Value::Integer(n) => vec![Span::raw(n.to_string())],
        Value::Number(n) => vec![Span::raw(n.to_string())],
        Value::Table(t) => {
            let text: String = t.get::<Option<String>>("text")?.unwrap_or_default();
            let style = style_from(theme, &t.get::<Value>("style")?);
            text_spans(&text, style)
        }
        _ => vec![],
    })
}

pub(crate) fn spans_from(theme: &Theme, v: Value) -> mlua::Result<Vec<Span>> {
    match v {
        Value::Table(t) if t.get::<Option<String>>("type")?.is_none() => {
            let mut out = vec![];
            for v in t.sequence_values::<Value>() {
                out.extend(span_from(theme, v?)?);
            }
            Ok(out)
        }
        Value::Table(t) if t.get::<Option<String>>("type")?.as_deref() == Some("line") => {
            spans_from(theme, t.get("spans")?)
        }
        v => span_from(theme, v),
    }
}

fn line_from(theme: &Theme, t: &Table) -> mlua::Result<Line> {
    let mut line = Line::new(spans_from(theme, t.get("spans")?)?);
    let base = style_from(theme, &t.get::<Value>("style")?);
    if base != Style::default() {
        line = line.base_style(base);
    }
    if let Some(bg) = t.get::<Value>("fill").ok().and_then(color_ref) {
        line.fill_bg = Some(bg);
    }
    Ok(line)
}

/// One row from Lua: a `ui.Line`, a list of spans, or a plain string (`ui.List` items, a select
/// item's `lines`).
pub(crate) fn line_value(theme: &Theme, v: Value) -> mlua::Result<Line> {
    Ok(match v {
        Value::Table(it) if it.get::<Option<String>>("type")?.as_deref() == Some("line") => line_from(theme, &it)?,
        Value::Table(it) => Line::new(spans_from(theme, Value::Table(it))?),
        Value::String(s) => Line::raw(s.to_str()?.to_string()),
        _ => Line::empty(),
    })
}

pub(crate) fn new_image_id() -> mlua::Result<u32> {
    mare_shared::ui::next_image_id().ok_or_else(|| mlua::Error::external("image identity space is exhausted"))
}

pub fn node_from(theme: &Theme, v: Value) -> mlua::Result<Node> {
    let t = match v {
        Value::Nil => return Ok(Node::Empty),
        Value::String(s) => {
            return Ok(Node::Paragraph {
                line: Line::new(text_spans(&s.to_str()?, Style::default())),
                indent: vec![],
                hard_wrap: false,
            });
        }
        Value::Table(t) => t,
        _ => return Ok(Node::Empty),
    };
    let ty: Option<String> = t.get("type")?;
    let Some(ty) = ty else {
        // plain array of nodes
        let mut out = vec![];
        for v in t.sequence_values::<Value>() {
            out.push(node_from(theme, v?)?);
        }
        return Ok(Node::Lines(out));
    };
    Ok(match ty.as_str() {
        "span" => Node::Line(Line::new(span_from(theme, Value::Table(t))?)),
        "line" => {
            let line = line_from(theme, &t)?;
            if t.get::<Option<bool>>("wrap")?.unwrap_or(false) {
                let indent = spans_from(theme, t.get("indent")?)?;
                let hard_wrap = t.get::<Option<bool>>("hard_wrap")?.unwrap_or(false);
                Node::Paragraph { line, indent, hard_wrap }
            } else {
                Node::Line(line)
            }
        }
        "paragraph" => {
            let line = line_from(theme, &t)?;
            let hard_wrap = t.get::<Option<bool>>("hard_wrap")?.unwrap_or(false);
            Node::Paragraph { line, indent: spans_from(theme, t.get("indent")?)?, hard_wrap }
        }
        "lines" => {
            let mut out = vec![];
            if let Some(items) = t.get::<Option<Table>>("items")? {
                for v in items.sequence_values::<Value>() {
                    out.push(node_from(theme, v?)?);
                }
            }
            Node::Lines(out)
        }
        "markdown" => Node::Markdown(t.get::<Option<String>>("text")?.unwrap_or_default()),
        "presented_markdown" => {
            let text: String = t.get("text")?;
            let valid = mare_shared::markdown::fences(&text);
            let mut blocks = std::collections::BTreeMap::new();
            for entry in t.get::<Table>("blocks")?.sequence_values::<Table>() {
                let entry = entry?;
                let start: usize = entry.get("start")?;
                let end: Option<usize> = entry.get("end")?;
                if !valid.iter().any(|block| block.start == start && end.is_none_or(|end| block.end == end))
                    || blocks.contains_key(&start)
                {
                    return Err(mlua::Error::external("invalid or duplicate fenced-block start"));
                }
                blocks.insert(start, node_from(theme, entry.get("node")?)?);
            }
            Node::PresentedMarkdown { text, blocks }
        }
        "code" => Node::Code {
            code: t.get::<Option<String>>("code")?.unwrap_or_default(),
            lang: t.get::<Option<String>>("lang")?.unwrap_or_default(),
        },
        "diff" => match t.get::<Option<String>>("unified")? {
            Some(diff) => Node::Unified { diff, path: t.get("path")?, lines: t.get("lines")? },
            None => Node::Diff {
                old: t.get::<Option<String>>("old")?.unwrap_or_default(),
                new: t.get::<Option<String>>("new")?.unwrap_or_default(),
                path: t.get("path")?,
            },
        },
        "indent" => {
            let first = spans_from(theme, t.get("first")?)?;
            let rest = match t.get::<Value>("rest")? {
                Value::Nil => {
                    let w: usize = first.iter().map(|s| s.width()).sum();
                    vec![Span::raw(" ".repeat(w))]
                }
                v => spans_from(theme, v)?,
            };
            Node::Indent { first, rest, child: Box::new(node_from(theme, t.get("child")?)?) }
        }
        "gap" => {
            let n = t.get::<Option<usize>>("n")?.unwrap_or(1);
            if n > MAX_GAP_LINES {
                return Err(mlua::Error::runtime(format!("ui gap.n exceeds {MAX_GAP_LINES} lines")));
            }
            Node::Gap(n)
        }
        "image" => {
            if t.get::<Option<Table>>("resource")?.is_some() && (t.contains_key("bytes")? || t.contains_key("path")?) {
                return Err(mlua::Error::external("image resource cannot retain inline bytes or path"));
            }
            Node::Image {
                fallback: t.get::<Option<Value>>("fallback")?.map(|v| node_from(theme, v).map(Box::new)).transpose()?,
                resource: t
                    .get::<Option<Table>>("resource")?
                    .map(|r| {
                        mare_shared::image_resource::Reference::new(
                            r.get("resolver_id")?,
                            r.get("key")?,
                            r.get("width")?,
                            r.get("height")?,
                        )
                        .map_err(mlua::Error::external)
                    })
                    .transpose()?,
                image_id: match t.get::<Option<u32>>("image_id")? {
                    Some(0) => return Err(mlua::Error::external("image_id must be nonzero")),
                    Some(id) => id,
                    None => new_image_id()?,
                },
                path: t.get("path")?,
                bytes: t.get::<Option<mlua::LuaString>>("bytes")?.map(|b| b.as_bytes().to_vec().into()),
                max_rows: t.get::<Option<usize>>("max_rows")?.unwrap_or(16),
                max_cols: t.get::<Option<usize>>("max_cols")?.unwrap_or(80),
            }
        }
        "spinner" => Node::Spinner(Line::new(spans_from(theme, t.get("label")?)?)),
        "frames" => {
            let mut frames = vec![];
            if let Some(items) = t.get::<Option<Table>>("frames")? {
                for v in items.sequence_values::<Value>() {
                    frames.push(node_from(theme, v?)?);
                }
            }
            let still = match t.get::<Value>("still")? {
                Value::Nil => frames.first().cloned().unwrap_or(Node::Empty),
                v => node_from(theme, v)?,
            };
            Node::Frames { frames, ms: t.get::<Option<u64>>("ms")?.unwrap_or(100).max(16), still: Box::new(still) }
        }
        "row" => {
            let mut out = vec![];
            if let Some(items) = t.get::<Option<Table>>("items")? {
                for v in items.sequence_values::<Value>() {
                    out.push(node_from(theme, v?)?);
                }
            }
            Node::Row(out)
        }
        "block" => Node::Block {
            title: match t.get::<Value>("title")? {
                Value::String(s) => vec![Span::styled(s.to_str()?.to_string(), Style::named("MareTitle"))],
                v => spans_from(theme, v)?,
            },
            footer: t.get("footer")?,
            border_style: t.get::<Option<String>>("border_style")?.unwrap_or_else(|| "MareBorder".into()),
            border: t.get("border")?,
            child: Box::new(node_from(theme, t.get("child")?)?),
        },
        "list" => {
            let mut items = vec![];
            if let Some(list) = t.get::<Option<Table>>("items")? {
                for v in list.sequence_values::<Value>() {
                    items.push(line_value(theme, v?)?);
                }
            }
            Node::List {
                // Lua indices are 1-based
                selected: t.get::<Option<usize>>("selected")?.and_then(|i| i.checked_sub(1)),
                focused: t.get::<Option<bool>>("focused")?.unwrap_or(true),
                height: t.get::<Option<usize>>("height")?.unwrap_or(items.len().max(1)),
                items,
            }
        }
        "split" => Node::Split {
            left: Box::new(node_from(theme, t.get("left")?)?),
            right: Box::new(node_from(theme, t.get("right")?)?),
            ratio: t.get::<Option<f32>>("ratio")?.unwrap_or(0.4),
            stack_below: t.get::<Option<usize>>("stack_below")?.unwrap_or(84),
        },
        "clip" => Node::Clip {
            max: t.get::<Option<usize>>("max")?.unwrap_or(5),
            tail: t.get::<Option<bool>>("tail")?.unwrap_or(false),
            child: Box::new(node_from(theme, t.get("child")?)?),
        },
        _ => Node::Empty,
    })
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn gap_nodes_preserve_small_values_and_reject_oversized_counts() {
        let lua = mlua::Lua::new();
        let theme = Theme::default();
        let gap = lua.create_table().unwrap();
        gap.set("type", "gap").unwrap();
        assert!(matches!(node_from(&theme, Value::Table(gap.clone())).unwrap(), Node::Gap(1)));
        gap.set("n", 0).unwrap();
        assert!(matches!(node_from(&theme, Value::Table(gap.clone())).unwrap(), Node::Gap(0)));
        gap.set("n", 1_000).unwrap();
        assert!(matches!(node_from(&theme, Value::Table(gap.clone())).unwrap(), Node::Gap(1_000)));
        gap.set("n", 1_001).unwrap();
        let error = node_from(&theme, Value::Table(gap.clone())).unwrap_err().to_string();
        assert!(error.contains("gap") && error.contains("1000"), "{error}");
        gap.set("n", -1).unwrap();
        assert!(node_from(&theme, Value::Table(gap)).is_err());
    }

    #[test]
    fn list_height_rejects_negative_values_but_accepts_large_windows() {
        let lua = mlua::Lua::new();
        let theme = Theme::default();
        let list = lua.create_table().unwrap();
        list.set("type", "list").unwrap();
        list.set("height", -1).unwrap();
        assert!(node_from(&theme, Value::Table(list.clone())).is_err());
        list.set("height", 1_000_000_000).unwrap();
        assert!(matches!(node_from(&theme, Value::Table(list)).unwrap(), Node::List { height: 1_000_000_000, .. }));
    }

    #[test]
    fn presentation_offsets_are_validated_and_binary_images_remain_bytes() {
        let lua = mlua::Lua::new();
        let source = "```drawing\nx\n```";
        let t = lua.create_table().unwrap();
        t.set("type", "presented_markdown").unwrap();
        t.set("text", source).unwrap();
        let entries = lua.create_table().unwrap();
        let entry = lua.create_table().unwrap();
        entry.set("start", 0).unwrap();
        let image = lua.create_table().unwrap();
        image.set("type", "image").unwrap();
        image.set("bytes", lua.create_string([0, 255, 137, 80, 78, 71]).unwrap()).unwrap();
        entry.set("node", image).unwrap();
        entries.set(1, entry.clone()).unwrap();
        t.set("blocks", entries.clone()).unwrap();
        let theme = Theme::default();
        let node = node_from(&theme, Value::Table(t.clone())).unwrap();
        let Node::PresentedMarkdown { text, blocks } = node else { panic!("wrong node") };
        assert_eq!(text, source);
        assert!(
            matches!(blocks.get(&0), Some(Node::Image { bytes: Some(bytes), .. }) if bytes.as_ref() == [0, 255, 137, 80, 78, 71])
        );
        entry.set("start", 1).unwrap();
        assert!(node_from(&theme, Value::Table(t.clone())).is_err());
        entry.set("start", 0).unwrap();
        entries.set(2, entry).unwrap();
        assert!(node_from(&theme, Value::Table(t)).is_err());
    }
}

//! Lua-rendered TUI components and their caches.

use crate::app::*;

/// A list overlay's frame as described by `mare.ui.Select:frame` (the core adds rows, search
/// text and the count).
#[derive(Clone)]
pub(in crate::app) struct SelectFrame {
    pub(super) title: Vec<Span>,
    pub(super) border: String,
    pub(super) search: Vec<Span>,
    pub(super) cursor: Vec<Span>,
    pub(super) empty: Line,
    pub(super) footer_style: String,
    pub(super) preview_title: Vec<Span>,
    pub(super) preview_border: String,
}

#[derive(Clone)]
pub(in crate::app) struct EditorChromeSpec {
    pub(super) prompt: String,
    pub(super) prompt_style: Option<String>,
    pub(super) placeholder: String,
    pub(super) placeholder_style: Option<String>,
    pub(super) border: Option<String>,
}

#[derive(Clone)]
pub(in crate::app) struct ModalFrameSpec {
    pub(super) title: Vec<Span>,
    pub(super) border: String,
    pub(super) footer_style: String,
    pub(super) search: Vec<Span>,
    pub(super) cursor: Vec<Span>,
    pub(super) empty: Line,
}

type Memo<T> = std::cell::RefCell<Option<(String, Option<T>)>>;
type NoticeRows = std::collections::HashMap<String, Option<Vec<Line>>>;

pub(super) struct CompletionPopupCache {
    theme_generation: u64,
    title: String,
    commands: bool,
    items: Vec<CompletionItem>,
    selected: usize,
    width: usize,
    height: usize,
    max_height: Option<usize>,
    footer: String,
    lines: Option<Vec<Line>>,
}

#[derive(Default)]
pub(in crate::app) struct ComponentCaches {
    pub(super) hints: Memo<Line>,
    pub(super) working: Memo<Node>,
    pub(super) frame: Memo<SelectFrame>,
    pub(super) confirm: Memo<Node>,
    pub(super) confirm_has_anim: std::cell::Cell<bool>,
    pub(super) notice_card: std::cell::RefCell<Option<(u64, NoticeRows)>>,
    pub(super) completion_popup: std::cell::RefCell<Option<CompletionPopupCache>>,
    pub(super) which_key: Memo<(Vec<Line>, usize)>,
    pub(super) editor_chrome: Memo<EditorChromeSpec>,
    pub(super) select_row: std::cell::RefCell<std::collections::HashMap<String, Vec<Line>>>,
    pub(super) select_row_failed: std::cell::RefCell<std::collections::HashSet<String>>,
    pub(super) select_row_heights: std::cell::RefCell<std::collections::HashMap<String, usize>>,
    pub(super) select_row_ptr: std::cell::Cell<usize>,
    pub(super) select_row_gen: std::cell::Cell<u64>,
    pub(super) modal_frame: Memo<ModalFrameSpec>,
    pub(super) failed_components: std::cell::RefCell<std::collections::HashSet<(String, String)>>,
    pub(super) component_notices: std::cell::RefCell<Vec<String>>,
}

/// The value cached for `key`, or `make()`'s (cached too, a failure included, so a broken
/// component costs one call per input change, not one per frame).
pub(super) fn cached<T: Clone>(
    cell: &std::cell::RefCell<Option<(String, Option<T>)>>,
    key: String,
    make: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if let Some((k, v)) = cell.borrow().as_ref()
        && *k == key
    {
        return v.clone();
    }
    let v = make();
    *cell.borrow_mut() = Some((key, v));
    cell.borrow().as_ref().and_then(|(_, v)| v.clone())
}

/// The spans of a one-line node (a Line or Span from Lua), with deferred theme styles resolved.
pub(super) fn node_spans(n: Node, theme: &Theme) -> Vec<Span> {
    match n {
        Node::Line(l) => l.resolve(theme).spans,
        Node::Paragraph { line, .. } => line.resolve(theme).spans,
        Node::Lines(items) => items.into_iter().flat_map(|item| node_spans(item, theme)).collect(),
        _ => vec![],
    }
}

impl App {
    /// Call a Lua shell component: `mare.ui.<comp>:<method>(args…)` (the key hint
    /// line, tabs, select / confirm popups are Lua components; Lua describes, the core draws).
    /// `None` when it's missing or fails (logged): the caller draws its built-in default.
    pub(super) fn lua_component(&self, comp: &str, method: &str, args: &[serde_json::Value]) -> Option<mlua::Value> {
        let lua = self.host.lua();
        let call = || -> mlua::Result<mlua::Value> {
            let ui: mlua::Table = lua.globals().get::<mlua::Table>("mare")?.get("ui")?;
            let c: mlua::Table = ui.get(comp)?;
            let f: mlua::Function = c.get(method)?;
            let mut a = mlua::MultiValue::new();
            a.push_back(mlua::Value::Table(c));
            for v in args {
                a.push_back(mare_shared::lua::json_to_lua(lua, v)?);
            }
            f.call(a)
        };
        match call() {
            Ok(v) if !v.is_nil() => Some(v),
            Ok(_) => None,
            Err(e) => {
                self.record_component_failure(comp, method, &e.to_string());
                None
            }
        }
    }

    pub(super) fn clear_component_failure(&self, comp: &str, method: &str) {
        self.component_caches.failed_components.borrow_mut().remove(&(comp.to_string(), method.to_string()));
    }

    pub(super) fn record_component_failure(&self, comp: &str, method: &str, err: &str) {
        tracing::warn!("mare.ui.{comp}:{method}: {err}");
        if self.component_caches.failed_components.borrow_mut().insert((comp.to_string(), method.to_string())) {
            self.component_caches.component_notices.borrow_mut().push(format!("mare.ui.{comp}:{method} failed: {err}"));
        }
    }

    pub(super) fn flush_component_notices(&mut self) {
        let msgs = std::mem::take(&mut *self.component_caches.component_notices.borrow_mut());
        for msg in msgs {
            self.push_notice((msg, "warning".into(), Instant::now()));
        }
    }

    /// Return the decoded node too so Confirm can cache it and render each animation frame.
    fn lua_render_node(
        &self,
        comp: &str,
        props: serde_json::Value,
        width: usize,
        theme: &Theme,
        frame: usize,
        unbounded_line: bool,
    ) -> Option<(Node, Vec<Line>)> {
        let value = match self.lua_component(comp, "render", &[props]) {
            Some(value) => value,
            None => {
                // lua_component already records call errors; a nil return needs the same notice.
                let failed = self.component_caches.failed_components.borrow().contains(&(comp.into(), "render".into()));
                if !failed {
                    self.record_component_failure(comp, "render", "returned nil");
                }
                return None;
            }
        };
        let node = match mare_binding::ui::node_from(theme, value) {
            Ok(node) => node,
            Err(error) => {
                self.record_component_failure(comp, "render", &error.to_string());
                return None;
            }
        };
        let render_width = if unbounded_line && matches!(node, Node::Line(_)) { usize::MAX } else { width };
        let lines = render_node(&node, render_width, theme, frame);
        if lines.is_empty() || lines.iter().all(|line| line.spans.is_empty()) {
            self.record_component_failure(comp, "render", "returned empty node");
            return None;
        }
        self.clear_component_failure(comp, "render");
        Some((node, lines))
    }

    /// The working line through `mare.ui.Working:render(info)` (single Lua path),
    /// including the `stopping` state (`info.stopping = true`). When Lua returns `nil` or errors,
    /// the caller renders a generic `Fallback::Lines` text fallback.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn working_node(
        &self,
        label: &str,
        detail: Option<&str>,
        secs: u64,
        hints: &[String],
        interrupt_key: Option<&str>,
        still: bool,
        stopping: bool,
        theme: &Theme,
    ) -> Option<Node> {
        let color = match self.renderer.color {
            ColorMode::TrueColor => "truecolor",
            ColorMode::Ansi256 => "256",
            ColorMode::Ansi16 => "16",
            ColorMode::NoColor => "none",
        };
        let key = format!(
            "{}|{color}|{label}|{detail:?}|{secs}|{hints:?}|{interrupt_key:?}|{still}|{stopping}",
            mare_shared::style::theme_gen()
        );
        cached(&self.component_caches.working, key, || {
            let info = serde_json::json!({
                "label": label,
                "detail": detail,
                "secs": secs,
                "hints": hints,
                "interrupt_key": interrupt_key,
                "color": color,
                "still": still,
                "stopping": stopping,
                "stop_hint": stopping.then_some(STOP_HINT),
            });
            let v = self.lua_component("Working", "render", &[info])?;
            match mare_binding::ui::node_from(theme, v) {
                Ok(n) => {
                    self.clear_component_failure("Working", "render");
                    Some(n)
                }
                Err(e) => {
                    self.record_component_failure("Working", "render", &e.to_string());
                    None
                }
            }
        })
    }

    /// The status bar's key hints through `mare.ui.KeyHints:render`.
    pub(super) fn hints_line(&self, hints: &[(String, String)], theme: &Theme) -> Option<Line> {
        let key = format!("{}|{hints:?}", mare_shared::style::theme_gen());
        cached(&self.component_caches.hints, key, || {
            let list: Vec<serde_json::Value> =
                hints.iter().map(|(k, d)| serde_json::json!({ "key": k, "desc": d })).collect();
            let v = self.lua_component("KeyHints", "render", &[serde_json::Value::Array(list)])?;
            match mare_binding::ui::node_from(theme, v) {
                Ok(n) => {
                    self.clear_component_failure("KeyHints", "render");
                    Some(Line::new(node_spans(n, theme)))
                }
                Err(e) => {
                    self.record_component_failure("KeyHints", "render", &e.to_string());
                    None
                }
            }
        })
    }

    /// A list overlay's frame through `mare.ui.Select:frame` (built-in default without it).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select_frame(
        &self,
        title: &str,
        tabs: &[String],
        tab: usize,
        searching: bool,
        empty: bool,
        width: usize,
        theme: &Theme,
    ) -> SelectFrame {
        let key = format!("{}|{title}|{tabs:?}|{tab}|{searching}|{empty}|{width}", mare_shared::style::theme_gen());
        let lua = cached(&self.component_caches.frame, key, || {
            let s = serde_json::json!({ "title": title, "tabs": tabs, "tab": tab + 1, "searching": searching, "empty": empty, "width": width });
            let v = self.lua_component("Select", "frame", &[s])?;
            let mlua::Value::Table(t) = v else {
                self.record_component_failure("Select", "frame", "expected table");
                return None;
            };
            self.clear_component_failure("Select", "frame");
            let spans = |k: &str| {
                t.get::<mlua::Value>(k)
                    .ok()
                    .and_then(|v| mare_binding::ui::node_from(theme, v).ok())
                    .map(|n| node_spans(n, theme))
            };
            let style = |k: &str, d: &str| t.get::<Option<String>>(k).ok().flatten().unwrap_or_else(|| d.into());
            Some(SelectFrame {
                title: spans("title").unwrap_or_default(),
                border: style("border", "MareBorderActive"),
                search: spans("search").unwrap_or_default(),
                cursor: spans("cursor").unwrap_or_default(),
                empty: Line::new(spans("empty").unwrap_or_default()),
                footer_style: style("footer_style", "MareDim"),
                preview_title: spans("preview_title").unwrap_or_default(),
                preview_border: style("preview_border", "MareBorder"),
            })
        });
        lua.unwrap_or_else(|| {
            // built-in default (the same look as the Lua component's)
            let title = if tabs.len() > 1 {
                let mut t = vec![];
                let active = tab.min(tabs.len() - 1);
                let mut prefix = if title.is_empty() { String::new() } else { format!("{title}  ") };
                let available = width.saturating_sub(6).max(1);
                if mare_shared::style::str_width(&prefix) + mare_shared::style::str_width(&tabs[active]) > available {
                    prefix.clear();
                }
                if !prefix.is_empty() {
                    t.push(Span::styled(prefix.clone(), theme.style("MareMuted")));
                }
                let available = available.saturating_sub(mare_shared::style::str_width(&prefix));
                let (mut first, mut last) = (active, active);
                let required = |first: usize, last: usize| {
                    let names: usize = tabs[first..=last].iter().map(|name| mare_shared::style::str_width(name)).sum();
                    names + (last - first) * 3 + usize::from(first > 0) * 2 + usize::from(last + 1 < tabs.len()) * 2
                };
                loop {
                    if first > 0 && required(first - 1, last) <= available {
                        first -= 1;
                    } else if last + 1 < tabs.len() && required(first, last + 1) <= available {
                        last += 1;
                    } else {
                        break;
                    }
                }
                if first > 0 {
                    t.push(Span::styled("‹ ", theme.style("MareBorder")));
                }
                for (i, name) in tabs.iter().enumerate().take(last + 1).skip(first) {
                    if i > first {
                        t.push(Span::styled(" │ ", theme.style("MareBorder")));
                    }
                    let st = if i == active { theme.style("MareTitle") } else { theme.style("MareDim") };
                    let room =
                        available.saturating_sub(usize::from(first > 0) * 2 + usize::from(last + 1 < tabs.len()) * 2);
                    let name =
                        if i == active { mare_shared::style::truncate_str(name, room, "…") } else { name.clone() };
                    t.push(Span::styled(name, st));
                }
                if last + 1 < tabs.len() {
                    t.push(Span::styled(" ›", theme.style("MareBorder")));
                }
                t
            } else {
                vec![Span::styled(title.to_string(), theme.style("MareTitle"))]
            };
            SelectFrame {
                title,
                border: if searching { "MareBorderSearch" } else { "MareBorderActive" }.into(),
                search: vec![Span::styled("/ ", theme.style("MareBorderSearch"))],
                cursor: vec![Span::styled("▏", theme.style("MareBorderSearch"))],
                empty: Line::styled(if empty { "(empty)" } else { "(no matches)" }, theme.fg("MareDim")),
                footer_style: "MareDim".into(),
                preview_title: vec![Span::styled("Preview", theme.style("MareMuted"))],
                preview_border: "MareBorder".into(),
            }
        })
    }

    /// The confirm dialog's rows (`pop_w` wide) through `mare.ui.Confirm:render`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn confirm_rows(
        &self,
        prompt: &str,
        yes: &str,
        no: &str,
        selected: usize,
        pop_w: usize,
        theme: &Theme,
        frame: usize,
    ) -> Option<Vec<Line>> {
        let still = frame == STATIC_FRAME;
        let key = format!("{}|{pop_w}|{selected}|{prompt}|{yes}|{no}|{still}", mare_shared::style::theme_gen());
        let node = cached(&self.component_caches.confirm, key, || {
            let c = serde_json::json!({
                "prompt": prompt, "yes": yes, "no": no, "selected": selected.min(1) + 1, "width": pop_w,
            });
            self.lua_render_node("Confirm", c, pop_w, theme, frame, false).map(|(node, _)| node)
        });
        self.component_caches.confirm_has_anim.set(node.as_ref().is_some_and(Node::has_animation));
        let node = node?;
        Some(render_node(&node, pop_w, theme, frame))
    }

    /// Notice card through `mare.ui.NoticeCard:render(p)`, cached per theme generation.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn notice_card_lines(
        &self,
        text: &str,
        level: &str,
        icon: &str,
        title: &str,
        style: &str,
        is_first: bool,
        card_w: usize,
        max_rows: usize,
        theme: &Theme,
        frame: usize,
    ) -> Option<Vec<Line>> {
        let theme_generation = mare_shared::style::theme_gen();
        let key = format!("{card_w}|{max_rows}|{is_first}|{style}|{icon}|{title}|{level}|{text}");
        {
            let cache = self.component_caches.notice_card.borrow();
            if let Some((g, map)) = cache.as_ref()
                && *g == theme_generation
                && let Some(cached) = map.get(&key)
            {
                return cached.clone();
            }
        }
        let props = serde_json::json!({
            "text": text,
            "level": level,
            "icon": icon,
            "title": title,
            "style": style,
            "width": card_w,
            "is_first": is_first,
            "max_rows": max_rows,
        });
        let mut lines = self.lua_render_node("NoticeCard", props, card_w, theme, frame, false).map(|(_, lines)| lines);
        if let Some(ref mut box_lines) = lines
            && box_lines.len() > max_rows + 2
            && let Some(bottom) = box_lines.pop()
        {
            box_lines.truncate(max_rows + 1);
            box_lines.push(bottom);
        }
        let mut cache = self.component_caches.notice_card.borrow_mut();
        if cache.as_ref().is_none_or(|(g, _)| *g != theme_generation) {
            *cache = Some((theme_generation, std::collections::HashMap::new()));
        }
        if let Some((_, map)) = cache.as_mut() {
            if map.len() > 64 {
                map.clear();
            }
            map.insert(key, lines.clone());
        }
        lines
    }

    /// Completion popup through `mare.ui.CompletionPopup:render(p)`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn completion_popup_lines(
        &self,
        title: &str,
        commands: bool,
        items: &[CompletionItem],
        selected: usize,
        cw: usize,
        height: usize,
        max_height: Option<usize>,
        footer: &str,
        theme: &Theme,
        frame: usize,
    ) -> Option<Vec<Line>> {
        let theme_generation = mare_shared::style::theme_gen();
        if let Some(cache) = self.component_caches.completion_popup.borrow().as_ref()
            && cache.theme_generation == theme_generation
            && cache.title == title
            && cache.commands == commands
            && cache.items.as_slice() == items
            && cache.selected == selected
            && cache.width == cw
            && cache.height == height
            && cache.max_height == max_height
            && cache.footer == footer
        {
            return cache.lines.clone();
        }
        let items_json: Vec<serde_json::Value> = items
            .iter()
            .map(|it| {
                serde_json::json!({
                    "label": it.label,
                    "detail": it.detail,
                    "hint": it.hint,
                    "icon": it.icon,
                    "style": it.style,
                })
            })
            .collect();
        let props = serde_json::json!({
            "title": title,
            "items": items_json,
            "selected": selected + 1,
            "total": items.len(),
            "width": cw,
            "height": height,
            "commands": commands,
            "footer": footer,
        });
        let val = self.lua_render_node("CompletionPopup", props, cw, theme, frame, false).map(|(_, mut out)| {
            if let Some(mh) = max_height {
                out.truncate(mh);
            }
            out
        });
        *self.component_caches.completion_popup.borrow_mut() = Some(CompletionPopupCache {
            theme_generation,
            title: title.to_string(),
            commands,
            items: items.to_vec(),
            selected,
            width: cw,
            height,
            max_height,
            footer: footer.to_string(),
            lines: val.clone(),
        });
        val
    }

    /// Which-key popup through `mare.ui.WhichKey:render(p)`.
    pub(super) fn which_key_lines(
        &self,
        prefix: &str,
        bindings: &[(String, String)],
        footer: Option<&str>,
        pop_w: usize,
        theme: &Theme,
        frame: usize,
    ) -> Option<(Vec<Line>, usize)> {
        let key = format!("{}|{pop_w}|{prefix}|{bindings:?}|{footer:?}", mare_shared::style::theme_gen());
        if let Some((k, v)) = self.component_caches.which_key.borrow().as_ref()
            && *k == key
        {
            return v.clone();
        }
        let entries: Vec<serde_json::Value> =
            bindings.iter().map(|(k, a)| serde_json::json!({ "key": k, "action": a })).collect();
        let props = serde_json::json!({ "prefix": prefix, "bindings": entries, "width": pop_w, "footer": footer });
        let lines = self.lua_render_node("WhichKey", props, pop_w, theme, frame, false).map(|(_, lines)| lines);
        let res = lines.map(|l| (l, pop_w));
        *self.component_caches.which_key.borrow_mut() = Some((key, res.clone()));
        res
    }

    /// Editor input chrome through `mare.ui.EditorChrome:render(s)`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn editor_chrome_opts(
        &self,
        prompt: &str,
        placeholder: &str,
        border: Option<&str>,
        shell: u8,
        working: bool,
        empty: bool,
    ) -> Option<EditorChromeSpec> {
        let key =
            format!("{}|{shell}|{working}|{empty}|{prompt}|{placeholder}|{border:?}", mare_shared::style::theme_gen());
        cached(&self.component_caches.editor_chrome, key, || {
            let props = serde_json::json!({
                "prompt": prompt,
                "placeholder": placeholder,
                "border": border,
                "shell": shell,
                "working": working,
                "empty": empty,
            });
            let v = self.lua_component("EditorChrome", "render", &[props])?;
            let mlua::Value::Table(t) = v else {
                self.record_component_failure("EditorChrome", "render", "expected table");
                return None;
            };
            self.clear_component_failure("EditorChrome", "render");
            let get_opt_str = |k: &str| t.get::<Option<String>>(k).ok().flatten();
            Some(EditorChromeSpec {
                prompt: get_opt_str("prompt").unwrap_or_else(|| prompt.to_string()),
                prompt_style: get_opt_str("prompt_style"),
                placeholder: get_opt_str("placeholder").unwrap_or_else(|| placeholder.to_string()),
                placeholder_style: get_opt_str("placeholder_style"),
                border: get_opt_str("border").or_else(|| border.map(str::to_string)),
            })
        })
    }

    /// Single list row through `mare.ui.SelectRow:render(item)`.
    pub(in crate::app) fn reset_select_row_failures(&self) {
        self.component_caches.select_row_failed.borrow_mut().clear();
    }

    pub(super) fn sync_select_row_state(&self) -> (bool, bool) {
        let lua = self.host.lua();
        let meta = (|| -> mlua::Result<(usize, bool)> {
            let ui: mlua::Table = lua.globals().get::<mlua::Table>("mare")?.get("ui")?;
            let c: mlua::Table = ui.get("SelectRow")?;
            let f: mlua::Function = c.get("render")?;
            let ptr = f.to_pointer() as usize;
            let is_preset = f.info().source.as_deref() == Some("@preset/ui_components.lua");
            Ok((ptr, is_preset))
        })();
        match meta {
            Ok((ptr, is_preset)) => {
                if ptr != self.component_caches.select_row_ptr.get() {
                    self.component_caches.select_row_ptr.set(ptr);
                    self.component_caches.select_row_gen.set(self.component_caches.select_row_gen.get() + 1);
                    self.component_caches.select_row.borrow_mut().clear();
                    self.component_caches.select_row_failed.borrow_mut().clear();
                    self.component_caches.select_row_heights.borrow_mut().clear();
                    self.clear_component_failure("SelectRow", "render");
                }
                (is_preset, true)
            }
            Err(_) => {
                if self.component_caches.select_row_ptr.get() != 0 {
                    self.component_caches.select_row_ptr.set(0);
                    self.component_caches.select_row_gen.set(self.component_caches.select_row_gen.get() + 1);
                    self.component_caches.select_row.borrow_mut().clear();
                    self.component_caches.select_row_failed.borrow_mut().clear();
                    self.component_caches.select_row_heights.borrow_mut().clear();
                }
                (false, false)
            }
        }
    }

    pub(super) fn select_row_key(
        &self,
        item: &SelectItem,
        index: usize,
        selected: bool,
        filter: &str,
        inner: usize,
    ) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{:?}|{}",
            self.component_caches.select_row_gen.get(),
            mare_shared::style::theme_gen(),
            inner,
            index,
            item.header,
            selected,
            item.label,
            item.detail,
            filter
        )
    }

    /// Single list row through `mare.ui.SelectRow:render(item)`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select_row_lines(
        &self,
        item: &SelectItem,
        index: usize,
        selected: bool,
        filter: &str,
        inner: usize,
        theme: &Theme,
        frame: usize,
    ) -> Option<Vec<Line>> {
        let key = self.select_row_key(item, index, selected, filter, inner);
        if let Some(cached) = self.component_caches.select_row.borrow().get(&key) {
            return Some(cached.clone());
        }
        if self.component_caches.select_row_failed.borrow().contains(&key) {
            return None;
        }
        let props = serde_json::json!({
            "label": item.label,
            "detail": item.detail,
            "header": item.header,
            "selected": selected,
            "filter": filter,
            "index": index + 1,
        });
        // Let `frame::list_items` truncate single-line rows after filter highlighting.
        let lines = self.lua_render_node("SelectRow", props, inner, theme, frame, true).map(|(_, out)| {
            let hl = theme.style("MareAccent").bold();
            if filter.is_empty() {
                out
            } else {
                out.into_iter().map(|l| frame::highlight_line(&l, filter, hl)).collect()
            }
        });
        if let Some(lines) = lines {
            let mut cache = self.component_caches.select_row.borrow_mut();
            if cache.len() >= 512
                && let Some(evict) = cache.keys().next().cloned()
            {
                cache.remove(&evict);
            }
            cache.insert(key, lines.clone());
            Some(lines)
        } else {
            let mut failed = self.component_caches.select_row_failed.borrow_mut();
            if failed.len() < 2048 {
                failed.insert(key);
            }
            None
        }
    }
}

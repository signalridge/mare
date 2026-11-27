//! Transcript blocks: patching and (streaming) rendering.

use crate::app::*;

pub(in crate::app) struct Block {
    pub(super) id: u64,
    pub(in crate::app) kind: String,
    pub(super) node: Option<Node>,
    pub(super) has_anim: bool,
    pub(super) title: Option<String>,
    pub(super) md: Option<StreamingMarkdown>,
    pub(super) expanded: Option<Node>,
    pub(super) expanded_has_anim: bool,
    pub(in crate::app) finished: bool,
    /// for "Thought for N s" (thinking blocks)
    pub(super) started: Instant,
    pub(super) ended: Option<Instant>,
    /// lines already committed to scrollback while still streaming
    pub(in crate::app) partial: usize,
    /// streaming commit pacing + the committed source position
    pub(super) sc: mare_tui::stream::StreamCommit,
}

impl Block {
    pub(super) fn apply(&mut self, p: BlockPatch) {
        if let Some(k) = p.kind {
            self.kind = k;
        }
        if let Some(n) = p.node {
            self.has_anim = n.has_animation();
            self.node = Some(n);
        }
        if let Some(t) = p.title {
            self.title = Some(t);
        }
        if let Some(e) = p.expanded {
            self.expanded_has_anim = e.has_animation();
            self.expanded = Some(e);
        }
        if let Some(m) = p.markdown {
            let mut sm = StreamingMarkdown::new();
            sm.set_text(&m);
            self.md = Some(sm);
        }
        if let Some(a) = p.append {
            self.md.get_or_insert_with(StreamingMarkdown::new).push(&a);
        }
    }

    /// The markdown source received so far (streaming answers), if this block has one.
    pub(in crate::app) fn source(&self) -> Option<&str> {
        self.md.as_ref().map(|md| md.text())
    }

    fn effective_expanded(&self, expanded: bool, show_reasoning: bool) -> bool {
        expanded || (self.kind == "reasoning" && show_reasoning)
    }

    pub(super) fn has_images(&self, expanded: bool, show_reasoning: bool) -> bool {
        let expanded = self.effective_expanded(expanded, show_reasoning);
        if expanded { self.expanded.as_ref().or(self.node.as_ref()) } else { self.node.as_ref() }
            .is_some_and(Node::contains_images)
            || self.md.as_ref().is_some_and(|md| mare_shared::markdown::has_images(md.text()))
    }

    /// Materialize only the visible tail of a possibly large live/committing block. Historical
    /// references retain their layout; the owned transcript viewport can revisit older rows.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn render_tail(
        &mut self,
        width: usize,
        theme: &Theme,
        frame: usize,
        expanded: bool,
        show_reasoning: bool,
        height: usize,
        expand_key: Option<&str>,
    ) -> Vec<Line> {
        if !self.has_images(expanded, show_reasoning) {
            return self.render(width, theme, frame, expanded, show_reasoning, expand_key);
        }
        let measured = {
            let _measure = mare_tui::image::measure_only();
            self.render(width, theme, frame, expanded, show_reasoning, expand_key)
        };
        let _window = mare_tui::image::window(measured.len().saturating_sub(height), measured.len());
        self.render(width, theme, frame, expanded, show_reasoning, expand_key)
    }

    pub(in crate::app) fn render(
        &mut self,
        width: usize,
        theme: &Theme,
        frame: usize,
        expanded: bool,
        show_reasoning: bool,
        expand_key: Option<&str>,
    ) -> Vec<Line> {
        // /show-reasoning: finished reasoning shows its expanded form instead of "Thought for N s"
        let expanded = self.effective_expanded(expanded, show_reasoning);
        if expanded && let Some(e) = &self.expanded {
            return render_node(e, width, theme, frame);
        }
        if let Some(n) = &self.node {
            return render_node(n, width, theme, frame);
        }
        let Some(md) = self.md.as_mut() else { return vec![] };
        match self.kind.as_str() {
            "reasoning" => {
                let style = theme.fg("MareReasoning");
                let show = expanded;
                // finished reasoning folds to one line; /show-reasoning or
                // the expanded view shows the text
                if self.finished && !show {
                    let secs = self.ended.unwrap_or_else(Instant::now).duration_since(self.started).as_secs().max(1);
                    let mut spans =
                        vec![Span::styled("◇ ", style), Span::styled(format!("Thought for {secs}s"), style)];
                    if let Some(key) = expand_key {
                        spans.push(Span::styled(format!("  ({key} to expand)"), theme.fg("dim")));
                    }
                    return vec![Line::new(spans)];
                }
                let inner = md.render(width.saturating_sub(2), theme);
                let mut lines: Vec<Line> = inner.into_iter().map(|l| l.base_style(style)).collect();
                if !show && lines.len() > 6 {
                    lines = lines.split_off(lines.len() - 6);
                }
                prefix_lines(lines, Span::styled("◇ ", style), Span::raw("  "))
            }
            _ => {
                let inner = md.render(width.saturating_sub(2), theme);
                if let Some(title) = &self.title {
                    let mut lines = vec![
                        Line::new(vec![
                            Span::styled("• ", theme.fg("MareAssistantMarker")),
                            Span::styled(title.clone(), theme.fg("text").bold()),
                        ]),
                        Line::default(),
                    ];
                    lines.extend(prefix_lines(inner, Span::raw("  "), Span::raw("  ")));
                    lines
                } else {
                    prefix_lines(inner, Span::styled("● ", theme.fg("MareAssistantMarker")), Span::raw("  "))
                }
            }
        }
    }
}

impl Block {
    /// A streaming answer's rows from row `skip` on: only the uncommitted part is built (the
    /// rows already in the scrollback aren't copied again every frame). Other blocks: the full
    /// render with the first `skip` rows dropped.
    pub(super) fn render_from(
        &mut self,
        width: usize,
        theme: &Theme,
        frame: usize,
        skip: usize,
        show_reasoning: bool,
    ) -> Vec<Line> {
        if self.kind == "assistant"
            && self.node.is_none()
            && self.expanded.is_none()
            && let Some(md) = self.md.as_mut()
        {
            let inner = md.render_from(width.saturating_sub(2), theme, skip);
            let first = Span::styled("● ", theme.fg("MareAssistantMarker"));
            let rest = Span::raw("  ");
            return inner
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    let fill = l.fill_bg;
                    let mut nl = l.prefixed(vec![if i == 0 && skip == 0 { first.clone() } else { rest.clone() }]);
                    nl.fill_bg = fill;
                    nl
                })
                .collect();
        }
        self.render(width, theme, frame, false, show_reasoning, None).into_iter().skip(skip).collect()
    }
    pub(super) fn has_animation(&self, expanded: bool, show_reasoning: bool) -> bool {
        if self.effective_expanded(expanded, show_reasoning) && self.expanded.is_some() {
            self.expanded_has_anim
        } else {
            self.has_anim
        }
    }
}

fn prefix_lines(lines: Vec<Line>, first: Span, rest: Span) -> Vec<Line> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let fill = l.fill_bg;
            let mut nl = l.prefixed(vec![if i == 0 { first.clone() } else { rest.clone() }]);
            nl.fill_bg = fill;
            nl
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_reasoning_uses_expanded_node_for_has_images_detection() {
        let blk = Block {
            id: 1,
            kind: "reasoning".into(),
            node: Some(Node::Line(Line::raw("Thought for 1s"))),
            has_anim: false,
            title: None,
            md: None,
            expanded: Some(Node::Image {
                resource: None,
                fallback: None,
                image_id: 42,
                path: None,
                bytes: None,
                max_rows: 4,
                max_cols: 20,
            }),
            expanded_has_anim: false,
            finished: true,
            started: Instant::now(),
            ended: Some(Instant::now()),
            partial: 0,
            sc: Default::default(),
        };
        assert!(!blk.has_images(false, false));
        assert!(blk.has_images(false, true), "show_reasoning must align has_images(false) with expanded render");
    }
}

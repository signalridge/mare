//! UI protocol shared by the binding layer (`mare.ui`) and the frontends: renderable nodes,
//! UI commands and the shared UI state.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use unicode_segmentation::UnicodeSegmentation;

use crate::style::{Line, Span, Theme};

/// How mare is running; plugins adapt (e.g. no UI in print mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Tui,
    Print,
    Json,
    Rpc,
    Acp,
    Child,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Print => "print",
            Self::Json => "json",
            Self::Rpc => "rpc",
            Self::Acp => "acp",
            Self::Child => "child",
        }
    }
}

/// Renderable node, already converted from Lua.
#[derive(Clone, Debug)]
pub enum Node {
    Empty,
    Line(Line),
    /// A line that is word-wrapped; continuation lines are prefixed with `indent`.
    Paragraph {
        line: Line,
        indent: Vec<Span>,
    },
    Lines(Vec<Self>),
    Markdown(String),
    /// Cached presentations keyed by the parser's fenced-block start byte. Source remains the
    /// truth; absent entries render ordinary code. No callbacks or IO on the draw path.
    PresentedMarkdown {
        text: String,
        blocks: std::collections::BTreeMap<usize, Self>,
    },
    Code {
        code: String,
        lang: String,
    },
    Diff {
        old: String,
        new: String,
        path: Option<String>,
    },
    /// An existing unified diff. `path` (when given) names the file for every row's highlight
    /// instead of the `+++` headers; `lines` (when given) is the longer side's line count, so the
    /// line-number gutter is as wide as [`Node::Diff`]'s for the same texts.
    Unified {
        diff: String,
        path: Option<String>,
        lines: Option<usize>,
    },
    /// Prefix the first rendered line with `first`, the rest with `rest`.
    Indent {
        first: Vec<Span>,
        rest: Vec<Span>,
        child: Box<Self>,
    },
    Gap(usize),
    /// An image: `path` or `bytes`, fitted into `max_rows` × `max_cols` cells; the
    /// renderer picks the terminal's protocol or a one-line text placeholder.
    Image {
        /// Stable occurrence identity, independent of cached source bytes.
        image_id: u32,
        resource: Option<crate::image_resource::Reference>,
        fallback: Option<Box<Self>>,
        path: Option<String>,
        bytes: Option<std::sync::Arc<[u8]>>,
        max_rows: usize,
        max_cols: usize,
    },
    /// Animated spinner glyph followed by a label (animated by the core).
    Spinner(Line),
    /// A frame animation drawn by the core: `frames` shown `ms` apart by wall-clock time (a
    /// slower redraw skips frames, it never slows the motion); `still` under reduce_motion.
    /// What the frames contain is the plugin's: glyphs, colour sweeps, anything.
    Frames {
        frames: Vec<Self>,
        ms: u64,
        still: Box<Self>,
    },
    /// Children laid side by side on one row (the first line of each), e.g. an animated glyph,
    /// a label and a plain tail.
    Row(Vec<Self>),
    /// Truncate to at most `max` lines with a "… N more lines" hint.
    Clip {
        max: usize,
        tail: bool,
        child: Box<Self>,
    },
    /// A bordered panel: title embedded in the top border, footer (`3 of 42`) at
    /// the right of the bottom border; `border_style` is a named style (focus / search colors are
    /// chosen by the Lua component), the charset comes from the theme unless `border` is set.
    Block {
        title: Vec<Span>,
        footer: Option<String>,
        border_style: String,
        border: Option<String>,
        child: Box<Self>,
    },
    /// A virtually scrolled list: only `height` rows around `selected` are drawn; selection is a
    /// full-width background when `focused`, bold otherwise. Inside a Block it adds a scrollbar.
    List {
        items: Vec<Line>,
        selected: Option<usize>,
        focused: bool,
        height: usize,
    },
    /// Two panes side by side (list + preview, lazygit); stacked when narrower than `stack_below`.
    Split {
        left: Box<Self>,
        right: Box<Self>,
        /// share of the width for the left pane (0..1)
        ratio: f32,
        stack_below: usize,
    },
}

/// Image occurrences occupy a different id range from legacy 24-bit content hashes.
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(0x0100_0000);

/// Allocate a process-wide terminal image identity. Exhaustion fails rather than reusing a
/// live occurrence. The component description retains it across layout/descriptor copies.
pub fn next_image_id() -> Option<u32> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut current = NEXT_IMAGE_ID.load(Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match NEXT_IMAGE_ID.compare_exchange_weak(current, next, Relaxed, Relaxed) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

impl Node {
    /// Whether any presentation branch depends on an image artifact/capability.
    pub fn contains_images(&self) -> bool {
        match self {
            Self::Image { .. } => true,
            Self::Lines(items) | Self::Row(items) => items.iter().any(Self::contains_images),
            Self::Markdown(text) => crate::markdown::has_images(text),
            Self::PresentedMarkdown { text, blocks } => {
                crate::markdown::has_images(text) || blocks.values().any(Self::contains_images)
            }
            Self::Indent { child, .. } | Self::Clip { child, .. } | Self::Block { child, .. } => {
                child.contains_images()
            }
            Self::Frames { frames, still, .. } => frames.iter().any(Self::contains_images) || still.contains_images(),
            Self::Split { left, right, .. } => left.contains_images() || right.contains_images(),
            _ => false,
        }
    }

    /// Whether any branch of this node tree contains an active animated node (`Spinner` or non-empty `Frames`).
    pub fn has_animation(&self) -> bool {
        match self {
            Self::Spinner(_) => true,
            Self::Frames { frames, still, .. } => {
                if frames.is_empty() {
                    still.has_animation()
                } else if frames.len() == 1 {
                    frames[0].has_animation()
                } else {
                    true
                }
            }
            Self::Lines(items) | Self::Row(items) => items.iter().any(Self::has_animation),
            Self::PresentedMarkdown { blocks, .. } => blocks.values().any(Self::has_animation),
            Self::Indent { child, .. } | Self::Clip { child, .. } | Self::Block { child, .. } => child.has_animation(),
            Self::Split { left, right, .. } => left.has_animation() || right.has_animation(),
            Self::Image { fallback, .. } => fallback.as_deref().is_some_and(Self::has_animation),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BlockPatch {
    pub node: Option<Node>,
    pub title: Option<String>,
    pub markdown: Option<String>,
    pub append: Option<String>,
    pub expanded: Option<Node>,
    pub kind: Option<String>,
}

#[derive(Default)]
pub struct SelectItem {
    pub label: String,
    pub detail: Option<String>,
    /// shown in a preview pane next to the list
    pub preview: Option<Node>,
    /// a group heading (`── Today ──`): shown, never selected; hidden while filtering
    pub header: bool,
    /// shown and searchable, but never highlighted or returned by a select
    pub disabled: bool,
    /// Initial and current selection state for a multi-select.
    pub checked: bool,
    /// what identifies the item across [`UiCmd::SelectUpdate`]s (else its label)
    pub item_id: Option<String>,
    /// drawn instead of `label detail` when not empty, one row each;
    /// the search still matches `label` + `detail` only
    pub lines: Vec<Line>,
}

impl SelectItem {
    pub fn is_selectable(&self) -> bool {
        !self.header && !self.disabled
    }
}

/// One tab of a tabbed select (`Running │ All`): its own items and selection.
pub struct SelectTab {
    pub name: String,
    pub items: Vec<SelectItem>,
    pub selected: usize,
}

/// Why a modal ended without an answer: `Interrupted` only for the reserved Ctrl+C;
/// Esc, `q`, a cancel token, a closed front end and a dropped modal are `Cancelled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dismissed {
    Cancelled,
    Interrupted,
}

impl Dismissed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

/// A select's highlighted item, checked items, and active tab (all indices are 0-based).
#[derive(Clone, Debug, Default)]
pub struct SelectReply {
    pub index: usize,
    /// All checked indices in the active tab (0-based).
    pub selected_indices: Vec<usize>,
    pub tab: usize,
    /// one of the select's extra `keys` pressed on the item, instead of Enter
    pub key: Option<char>,
    /// the search text at that moment, so a picker re-opened after an action keeps it
    pub filter: String,
}

/// How a select's list and its preview pane share a wide popup (`mare.ui.select{ split = {
/// ratio, min } }`): the list takes `ratio` of the width, at least `min` columns (the preview
/// keeps the rest). The default is the shell's 2/5 and 24.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectSplit {
    pub ratio: f32,
    pub min: usize,
}

impl Default for SelectSplit {
    fn default() -> Self {
        Self { ratio: 0.4, min: 24 }
    }
}

/// One entry of a plugin transcript view's header help: the actions (their keys joined by `/`)
/// and what they do, e.g. `{["agents.prev", "agents.next"], "agent"}` → `Alt+←/Alt+→ agent`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewKey {
    pub actions: Vec<String>,
    pub label: String,
}

/// How the input looks in shell mode: set by the plugin that
/// owns shell mode through `mare.ui.editor_config{ shell = … }`.
#[derive(Debug, Clone, Default)]
pub struct ShellStyle {
    pub placeholder: String,
    pub private_placeholder: String,
    pub border: Option<String>,
}

pub enum UiCmd {
    BlockPush {
        id: u64,
        kind: String,
        patch: BlockPatch,
    },
    BlockUpdate {
        id: u64,
        patch: BlockPatch,
    },
    BlockFinish {
        id: u64,
    },
    BlockRemove {
        id: u64,
    },
    Widget {
        id: String,
        slot: String,
        order: i64,
        node: Option<Node>,
    },
    Status {
        id: String,
        align: String,
        order: i64,
        node: Option<Node>,
        /// a left segment that stays whole on a narrow terminal (the rest gives way first)
        keep: bool,
    },
    Working {
        label: Option<String>,
        detail: Option<String>,
        /// a shell command the background key can move aside (the core words the key hint)
        background: bool,
    },
    Notify {
        text: String,
        level: String,
        id: Option<String>,
    },
    Select {
        title: String,
        /// embedded at the right of the top border (`Sort: Updated · Filter: Cwd`'s
        /// picker header); cut first when the width runs out
        title_right: Vec<Span>,
        /// how a list shares the width with its preview pane (`mare.ui.select{split=…}`)
        split: SelectSplit,
        items: Vec<SelectItem>,
        selected: usize,
        /// Toggle multiple items in place before confirming.
        multi: bool,
        /// Label for the accept hint; an empty label hides it.
        accept_label: Option<String>,
        /// Whether the list can be searched.
        allow_search: bool,
        /// initial filter text (`/model son` opens the picker already filtered)
        filter: String,
        /// tabbed views (empty = no tabs; then `items` is the list)
        tabs: Vec<SelectTab>,
        tab: usize,
        /// "list" | "confirm" (centered dialog with [Y]es [N]o buttons)
        kind: String,
        /// extra action keys `(key, label)`: pressed (while not searching) they return the
        /// highlighted item with that key (e.g. `w` = write to file in /copy's picker)
        keys: Vec<(char, String)>,
        /// explanatory lines shown above the options (e.g. the trust overlay's directory and
        /// resources)
        body: Vec<String>,
        /// vim-style navigation while not searching: j/k (with a count prefix), h/l switch
        /// tabs, g/G first/last; letters then don't start a search (`/` does)
        vim: bool,
        /// told `(tab, item, list_rev)` whenever the highlighted item changes: the owner answers
        /// with [`UiCmd::SelectPreview`] (lazily loaded preview panes)
        highlight: Option<UnboundedSender<(usize, usize, u64)>>,
        reply: oneshot::Sender<Result<SelectReply, Dismissed>>,
        /// names this select for [`UiCmd::SelectUpdate`] (0: no updates)
        token: u64,
        /// unique modal id for [`UiCmd::CancelModal`] (0: none)
        modal_id: u64,
        /// Tool-approval modal (`mare.ui.approve`): waits for typing idle, ignores decision keys
        /// until drawn at least once, and disables digit shortcuts.
        approval: bool,
    },
    /// replace the items of the open select `token` in place, keeping its search text, searching
    /// state and highlighted item (by id, else label); `tabs` non-empty: every tab's items
    SelectUpdate {
        token: u64,
        items: Vec<SelectItem>,
        tabs: Vec<SelectTab>,
    },
    /// the preview pane of one item of the open select (an answer to its `highlight`)
    SelectPreview {
        modal_id: u64,
        revision: u64,
        tab: usize,
        index: usize,
        node: Node,
    },
    Input {
        title: String,
        placeholder: String,
        default: String,
        secret: bool,
        reply: oneshot::Sender<Result<String, Dismissed>>,
        modal_id: u64,
    },
    /// Close an open modal if its `modal_id` matches (e.g. `cancel` token fired or caller dropped).
    CancelModal {
        modal_id: u64,
    },
    Overlay {
        title: String,
        node: Node,
        context: Option<String>,
        cursor: Option<(usize, usize)>,
    },
    /// full-screen transcript viewer with plugin-supplied content (e.g. a sub-session file);
    /// sending the same title again refreshes it in place, keeping the scroll position
    TranscriptView {
        title: String,
        node: Node,
        /// the view's own keys for its header (`ui.transcript(t, node, { keys = … })`): each names
        /// one or more actions of the transcript layer and what they do; the header shows the keys
        /// bound to them now (the keymap's reverse lookup), so a rebind changes the text
        keys: Vec<ViewKey>,
    },
    /// close the plugin transcript view — only while it shows `title`, when given (a stale task
    /// never closes a view the user opened since); the Ctrl+T transcript isn't a plugin view
    TranscriptClose {
        title: Option<String>,
    },
    SetEditor {
        text: String,
        /// Image blocks restored with the text, in marker order.
        images: Vec<serde_json::Value>,
        /// Exact submitted marker identities and byte offsets, parallel to images.
        image_markers: Vec<serde_json::Value>,
        /// leave a non-empty input alone (/tree, /fork and /undo never
        /// overwrite a draft)
        keep_draft: bool,
        /// put the text before a non-empty input instead of replacing it (an interrupt gives
        /// queued messages back above the draft)
        prepend: bool,
    },
    EditorConfig {
        prompt: Option<String>,
        placeholder: Option<String>,
        busy_placeholder: Option<String>,
        border: Option<String>,
        /// `Some(Some(style))` turns shell mode (`!` / `!!` on an empty input) on with that look,
        /// `Some(None)` turns it off; without a plugin that sets it there is no shell mode
        shell: Option<Option<ShellStyle>>,
    },
    EditorHint {
        text: String,
    },
    /// Frontend display flag (`show_reasoning`, `expand_all`, …) set from Lua.
    Flag {
        name: String,
        value: bool,
    },
    /// `key`'s whole binding stack in `context` (bottom first; empty unbinds it)
    SetBindings {
        context: String,
        key: String,
        entries: Vec<crate::bindings::Entry>,
    },
    /// Batch of `(context, key, entries)` binding-stack updates (e.g. `mare.keymap.set_many`).
    SetBindingsBatch {
        updates: Vec<(String, String, Vec<crate::bindings::Entry>)>,
    },
    /// Register (`Some(parent)`) or remove (`None`) a plugin keymap context.
    SetContext {
        name: String,
        parent: Option<String>,
        swallow_printable: bool,
        active: bool,
    },
    /// Toggle whether a declared plugin keymap subcontext is active.
    SetContextActive {
        name: String,
        active: bool,
    },
    /// Synchronous editor mutation when no `EditorBridge` was attached yet (headless / unit test).
    EditorOp(EditorSyncOp),
    /// Notification to `App` that `EditorBridge::apply` modified the live editor.
    SyncEditorChanged,
    /// A registered completion source was released or disposed; retract its items from any open popup.
    CompletionSourceRemoved {
        id: u64,
    },
    ThemeChanged,
    /// Wake the TUI event loop to redraw after a shared atomic state change (e.g. queue count/mode).
    Redraw,
    /// Updated terminal color depth (`terminal.color` / capability resolution).
    ColorMode(crate::style::ColorMode),
    Print {
        node: Node,
    },
    /// desktop notification via OSC 9 / 777 / 99, or just BEL
    NotifyOs {
        text: String,
        /// "auto" | "osc9" | "osc777" | "osc99" | "bel" | "native"
        protocol: String,
        /// also ring the bell after the OSC sequence
        bell: bool,
    },
    /// terminal window title (OSC 0/2)
    Title {
        text: String,
    },
    Clear,
    /// a user asked to leave (`/exit`, the quit keys): quits unless an exit guard is set, in
    /// which case the frontend emits `app/exit-requested` and whoever set the guard decides
    Exit,
    /// leave now, past any exit guard (the guard's owner confirmed)
    ExitForce,
    /// on: user-initiated exits go through `app/exit-requested` (the jobs plugin sets it while
    /// background jobs run)
    ExitGuard {
        on: bool,
    },
    /// Updated stacked region layout configuration (`ui.layout`).
    Layout(crate::layout::LayoutConfig),
}

/// Snapshot of the main editor state exposed synchronously to Lua (`mare.ui.get_editor` /
/// `mare.ui.editor_state` / `editor/changed`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditorStateSnapshot {
    pub text: String,
    pub cursor: (usize, usize),
    pub selection: Option<((usize, usize), (usize, usize))>,
    pub anchor: Option<(usize, usize)>,
    pub head: Option<(usize, usize)>,
    pub selected_text: Option<String>,
    pub empty: bool,
    pub revision: u64,
}

/// Synchronous mutation requested by `mare.ui` editor manipulation APIs.
#[derive(Clone, Debug)]
pub enum EditorSyncOp {
    SetText(String),
    SetCursor { row: usize, col: usize },
    SetSelection { anchor: Option<(usize, usize)>, head: Option<(usize, usize)> },
    Insert(String),
    Replace { start: Option<(usize, usize)>, end: Option<(usize, usize)>, text: String },
}

/// Bridge allowing `mare-binding` to read/mutate the live TUI `Editor` synchronously when attached.
pub trait EditorBridge: Send + Sync {
    fn snapshot(&self) -> EditorStateSnapshot;
    fn apply(&self, op: EditorSyncOp) -> Result<EditorStateSnapshot, String>;
    fn take_changed(&self) -> bool;
}

/// Metadata for a registered completion source (`mare.ui.completion.register`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionSourceMeta {
    pub id: u64,
    pub name: String,
    pub triggers: Vec<String>,
    pub min_prefix: usize,
    pub score_offset: i32,
    pub timeout_ms: u64,
}

pub struct UiShared {
    pub responder: AtomicBool,
    pub theme: RwLock<Theme>,
    pub width: AtomicU16,
    pub height: AtomicU16,
    pub tx: RwLock<Option<UnboundedSender<UiCmd>>>,
    pub next_id: AtomicU64,
    /// key bindings by layer, kept in step with `mare.keymap` (doctor, `mare.ui.keymap()`)
    pub keymap: RwLock<crate::bindings::Keymap>,
    /// latest snapshot of the live editor (updated on every edit or bridge op)
    pub editor_snapshot: RwLock<EditorStateSnapshot>,
    /// live synchronous editor bridge when a TUI `App` is attached
    pub editor_bridge: RwLock<Option<Arc<dyn EditorBridge>>>,
    /// active completion source descriptors (`mare.ui.completion.register`)
    pub completion_sources: RwLock<Vec<CompletionSourceMeta>>,
    notification_history: RwLock<VecDeque<(String, String)>>,
    /// terminal focus (tracked via focus-change reporting); meaningful only once `focus_known`
    pub focused: AtomicBool,
    /// the terminal has reported a focus change at least once: until then focus is unknown (many
    /// terminals never report it), and "only while unfocused" must not swallow everything
    pub focus_known: AtomicBool,
    /// messages waiting in the agent's queue (set by the ui plugin): with an empty input, ↑ selects
    /// a queued message and Enter offers send-now / steer / interrupt
    pub queued: std::sync::atomic::AtomicUsize,
    /// the queue editing state (set by the ui plugin, assumed at once by the key handler):
    /// 0 nothing selected, 1 a queued message selected, 2 one being edited (`queue_keys`)
    pub queue_mode: std::sync::atomic::AtomicU8,
    /// title of the plugin-supplied transcript view while it is open (`mare.ui.transcript_title`)
    pub transcript_title: RwLock<Option<String>>,
    /// terminal capabilities (static table + startup probe + `ui/caps` listeners): `mare.ui.caps()`
    pub caps: RwLock<serde_json::Value>,
    /// validated stacked region layout (`ui.layout`)
    pub layout: RwLock<crate::layout::LayoutConfig>,
}

/// The TUI layer's functions, installed once by the binary (or a test harness) that links the
/// TUI, so lower crates (host, bindings) can render nodes / normalize keys without depending on it.
static HOOKS: std::sync::OnceLock<UiHooks> = std::sync::OnceLock::new();

/// Install the TUI layer's hooks (first call wins). Without them nodes render to nothing and key
/// names are only lower-cased.
pub fn install_hooks(hooks: UiHooks) {
    let _ = HOOKS.set(hooks);
}

/// Outcome of writing an OSC 52 clipboard sequence through the TUI write exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Osc52Status {
    /// Sequence queued to the terminal write exit.
    Written,
    /// Payload exceeded the OSC 52 byte cap.
    TooLarge,
    /// Terminal is currently handed off to an external program; no bytes were written.
    HandedOff,
}

/// Functions provided by the TUI layer.
#[derive(Clone, Copy)]
pub struct UiHooks {
    pub render: fn(&Node, usize, &Theme) -> Vec<Line>,
    pub normalize_key: fn(&str) -> String,
    /// OSC 52 through the TUI's single write exit; only writes while the TUI owns the terminal.
    pub osc52: fn(&str) -> Osc52Status,
}

impl UiShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            responder: AtomicBool::new(true),
            theme: RwLock::new(Theme::default()),
            width: AtomicU16::new(100),
            height: AtomicU16::new(40),
            tx: RwLock::new(None),
            next_id: AtomicU64::new(1),
            keymap: RwLock::new(crate::bindings::Keymap::default()),
            editor_snapshot: RwLock::new(EditorStateSnapshot::default()),
            editor_bridge: RwLock::new(None),
            completion_sources: RwLock::new(Vec::new()),
            notification_history: RwLock::new(VecDeque::new()),
            focused: AtomicBool::new(true),
            focus_known: AtomicBool::new(false),
            queued: std::sync::atomic::AtomicUsize::new(0),
            queue_mode: std::sync::atomic::AtomicU8::new(0),
            transcript_title: RwLock::new(None),
            caps: RwLock::new(serde_json::Value::Object(Default::default())),
            layout: RwLock::new(crate::layout::LayoutConfig::default()),
        })
    }
    pub fn send(&self, cmd: UiCmd) -> bool {
        match self.tx.read().unwrap().as_ref() {
            Some(tx) => tx.send(cmd).is_ok(),
            None => false,
        }
    }

    /// Record recent TUI notices without publishing them to the event stream.
    pub fn record_notification(&self, text: &str, level: &str) {
        let mut history = self.notification_history.write().unwrap();
        if history.len() == 200 {
            history.pop_front();
        }
        history.push_back((text.to_owned(), level.to_owned()));
    }

    /// Recent TUI notices, oldest first.
    pub fn notification_history(&self) -> Vec<(String, String)> {
        self.notification_history.read().unwrap().iter().cloned().collect()
    }
    pub fn can_respond(&self) -> bool {
        self.tx.read().unwrap().as_ref().is_some_and(|tx| !tx.is_closed())
            && self.responder.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn connected(&self) -> bool {
        self.tx.read().unwrap().is_some()
    }
    /// Render a node with the injected renderer (plain fallback without a TUI layer).
    pub fn render(&self, node: &Node, width: usize) -> Vec<Line> {
        let theme = self.theme.read().unwrap().clone();
        match HOOKS.get() {
            Some(h) => (h.render)(node, width, &theme),
            None => vec![],
        }
    }
    /// OSC 52 through the write exit; `None` when no TUI layer is attached.
    pub fn osc52(&self, text: &str) -> Option<Osc52Status> {
        HOOKS.get().map(|h| (h.osc52)(text))
    }
    /// Canonical key name ("<C-t>" → "ctrl+t") via the injected normalizer.
    pub fn normalize_key(&self, k: &str) -> String {
        match HOOKS.get() {
            Some(h) => (h.normalize_key)(k),
            None => {
                let trimmed = k.trim();
                let inner = if trimmed.len() >= 2 && trimmed.starts_with('<') && trimmed.ends_with('>') {
                    &trimmed[1..trimmed.len() - 1]
                } else {
                    trimmed
                };
                let mut s = inner.to_lowercase();
                let mut prefix = String::new();
                loop {
                    if let Some(rest) = s.strip_prefix("c-").or_else(|| s.strip_prefix("ctrl+")) {
                        prefix.push_str("ctrl+");
                        s = rest.to_string();
                    } else if let Some(rest) = s.strip_prefix("m-").or_else(|| s.strip_prefix("alt+")) {
                        prefix.push_str("alt+");
                        s = rest.to_string();
                    } else if let Some(rest) = s.strip_prefix("s-").or_else(|| s.strip_prefix("shift+")) {
                        prefix.push_str("shift+");
                        s = rest.to_string();
                    } else {
                        break;
                    }
                }
                let key = match s.as_str() {
                    "lt" => "<",
                    "gt" => ">",
                    other => other,
                };
                format!("{prefix}{key}")
            }
        }
    }

    pub fn install_editor_bridge(&self, bridge: Arc<dyn EditorBridge>) {
        let snap = bridge.snapshot();
        *self.editor_snapshot.write().unwrap() = snap;
        *self.editor_bridge.write().unwrap() = Some(bridge);
    }

    pub fn clear_editor_bridge(&self) {
        *self.editor_bridge.write().unwrap() = None;
    }

    pub fn read_editor_snapshot(&self) -> EditorStateSnapshot {
        let bridge = self.editor_bridge.read().unwrap().clone();
        if let Some(b) = bridge {
            let snap = b.snapshot();
            *self.editor_snapshot.write().unwrap() = snap.clone();
            snap
        } else {
            self.editor_snapshot.read().unwrap().clone()
        }
    }

    pub fn apply_editor_op(&self, op: EditorSyncOp) -> Result<EditorStateSnapshot, String> {
        let bridge = self.editor_bridge.read().unwrap().clone();
        if let Some(b) = bridge {
            let snap = b.apply(op)?;
            *self.editor_snapshot.write().unwrap() = snap.clone();
            self.send(UiCmd::SyncEditorChanged);
            Ok(snap)
        } else {
            let snap = self.apply_headless_editor_op(op);
            Ok(snap)
        }
    }

    fn apply_headless_editor_op(&self, op: EditorSyncOp) -> EditorStateSnapshot {
        let mut snap = self.editor_snapshot.write().unwrap();
        match &op {
            EditorSyncOp::SetText(t) => {
                snap.text = t.clone();
                snap.cursor = headless_byte_to_pos(&snap.text, snap.text.len());
                headless_clear_selection(&mut snap);
            }
            EditorSyncOp::SetCursor { row, col } => {
                snap.cursor = headless_clamp_pos(&snap.text, *row, *col);
                headless_clear_selection(&mut snap);
            }
            EditorSyncOp::SetSelection { anchor, head } => {
                if let (Some((ar, ac)), Some((hr, hc))) = (anchor, head) {
                    let a = headless_clamp_pos(&snap.text, *ar, *ac);
                    let h = headless_clamp_pos(&snap.text, *hr, *hc);
                    snap.anchor = Some(a);
                    snap.head = Some(h);
                    snap.cursor = h;
                    if a != h {
                        let (start, end) = if a <= h { (a, h) } else { (h, a) };
                        let b_start = headless_pos_to_byte(&snap.text, start);
                        let b_end = headless_pos_to_byte(&snap.text, end);
                        snap.selection = Some((start, end));
                        snap.selected_text = Some(snap.text[b_start..b_end].to_string());
                    } else {
                        snap.selection = None;
                        snap.selected_text = None;
                    }
                } else {
                    headless_clear_selection(&mut snap);
                }
            }
            EditorSyncOp::Insert(text) => {
                let (b_start, b_end) = headless_selection_bytes(&snap);
                headless_splice(&mut snap, b_start, b_end, text);
            }
            EditorSyncOp::Replace { start, end, text } => {
                let (b_start, b_end) = match (start, end) {
                    (Some((sr, sc)), Some((er, ec))) => {
                        let a = headless_clamp_pos(&snap.text, *sr, *sc);
                        let b = headless_clamp_pos(&snap.text, *er, *ec);
                        let (from, to) = if a <= b { (a, b) } else { (b, a) };
                        (headless_pos_to_byte(&snap.text, from), headless_pos_to_byte(&snap.text, to))
                    }
                    _ => headless_selection_bytes(&snap),
                };
                headless_splice(&mut snap, b_start, b_end, text);
            }
        }
        snap.empty = snap.text.is_empty();
        snap.revision += 1;
        let out = snap.clone();
        drop(snap);
        self.send(UiCmd::EditorOp(op));
        out
    }
}

/// Clamp a byte offset to the start of its grapheme, or to the end of the line.
pub fn clamp_line_grapheme(line: &str, col: usize) -> usize {
    let clamped = col.min(line.len());
    if clamped == line.len() {
        return clamped;
    }
    line.grapheme_indices(true).take_while(|(i, _)| *i <= clamped).last().map_or(0, |(i, _)| i)
}

fn headless_clamp_pos(text: &str, row: usize, col: usize) -> (usize, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let r = row.min(lines.len().saturating_sub(1));
    let line = lines.get(r).copied().unwrap_or("");
    (r, clamp_line_grapheme(line, col))
}

fn headless_pos_to_byte(text: &str, (row, col): (usize, usize)) -> usize {
    let mut offset = 0;
    for (idx, line) in text.split('\n').enumerate() {
        if idx == row {
            return offset + col.min(line.len());
        }
        offset += line.len() + 1;
    }
    text.len()
}

fn headless_byte_to_pos(text: &str, byte_idx: usize) -> (usize, usize) {
    let clamped = byte_idx.min(text.len());
    let prefix = &text[..clamped];
    let row = prefix.matches('\n').count();
    let col = prefix.rsplit('\n').next().map_or(0, str::len);
    (row, col)
}

fn headless_clear_selection(snap: &mut EditorStateSnapshot) {
    snap.selection = None;
    snap.anchor = None;
    snap.head = None;
    snap.selected_text = None;
}

fn headless_selection_bytes(snap: &EditorStateSnapshot) -> (usize, usize) {
    if let Some((start, end)) = snap.selection {
        (headless_pos_to_byte(&snap.text, start), headless_pos_to_byte(&snap.text, end))
    } else {
        let p = headless_pos_to_byte(&snap.text, snap.cursor);
        (p, p)
    }
}

fn headless_splice(snap: &mut EditorStateSnapshot, start_byte: usize, end_byte: usize, text: &str) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    snap.text.replace_range(start_byte..end_byte, &normalized);
    snap.cursor = headless_byte_to_pos(&snap.text, start_byte + normalized.len());
    headless_clear_selection(snap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_editor_clamps_rows_and_grapheme_byte_offsets() {
        assert_eq!(clamp_line_grapheme("", usize::MAX), 0);
        assert_eq!(clamp_line_grapheme("abc", 2), 2);
        assert_eq!(clamp_line_grapheme("a👩‍💻z", 5), 1);
        assert_eq!(clamp_line_grapheme("a👩‍💻z", "a👩‍💻".len()), "a👩‍💻".len());
        let ui = UiShared::new();
        ui.apply_editor_op(EditorSyncOp::SetText("xก้z\n界ﾞ!".into())).unwrap();
        for (row, col, expected) in [
            (0, 0, (0, 0)),
            (0, 3, (0, 1)),
            (0, 7, (0, 7)),
            (0, usize::MAX, (0, 8)),
            (1, 4, (1, 0)),
            (1, 6, (1, 6)),
            (usize::MAX, usize::MAX, (1, 7)),
        ] {
            let snapshot = ui.apply_editor_op(EditorSyncOp::SetCursor { row, col }).unwrap();
            assert_eq!(snapshot.cursor, expected, "row={row}, col={col}");
        }
        let selected =
            ui.apply_editor_op(EditorSyncOp::SetSelection { anchor: Some((0, 3)), head: Some((1, 4)) }).unwrap();
        assert_eq!(selected.selection, Some(((0, 1), (1, 0))));
        assert_eq!(selected.selected_text.as_deref(), Some("ก้z\n"));
    }
}

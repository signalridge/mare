---@meta _
-- LuaLS definitions: component constructors, styles, renderers, terminal UI, clipboard and images (entry: types/mare.lua)

---@alias mare.Color string  Theme slot ("accent", "muted", "dim", "error", ...) or "#rrggbb"

---@class mare.Style
---@field fg? mare.Color
---@field bg? mare.Color
---@field bold? boolean
---@field dim? boolean
---@field italic? boolean
---@field underline? boolean
---@field strike? boolean
---@field reverse? boolean

---@alias mare.StyleName string Named style, e.g. "MareToolName", "MareDim", "MareDiffAdd", "MareAccent"

---@class mare.Span
---@field type "span"
---@field text string
---@field style? mare.Style|mare.Color
local Span = {}
---@param c mare.Color
---@return mare.Span
function Span:fg(c) end
---@param c mare.Color
---@return mare.Span
function Span:bg(c) end
---@return mare.Span
function Span:bold() end
---@return mare.Span
function Span:dim() end
---@return mare.Span
function Span:italic() end
---@return mare.Span
function Span:underline() end
---@return mare.Span
function Span:strike() end
---@return mare.Span
function Span:reverse() end
--- An OSC 8 hyperlink (http/https only) where the terminal supports them.
---@param url string
---@return mare.Span
function Span:link(url) end
---@param s mare.Style
---@return mare.Span
function Span:with_style(s) end
--- Use a named style (Neovim highlight-group style).
---@param name mare.StyleName
---@return mare.Span
function Span:hl(name) end

---@class mare.Line : mare.Span
---@field type "line"
---@field spans (mare.Span|string)[]
local Line = {}
---@param indent? (mare.Span|string)[]
---@return mare.Line
function Line:wrapped(indent) end
---@param c mare.Color
---@return mare.Line
function Line:filled(c) end
---@param x mare.Span|string
---@return mare.Line
function Line:push(x) end

---@alias mare.Node mare.Line|mare.Span|table|string|nil

---@alias mare.NodeWire table|string|nil JSON-safe existing component descriptions; images use base64 data

-- ---------------------------------------------------------------------------
-- ui (component constructors + UI commands); also available as global `ui`
-- ---------------------------------------------------------------------------

---@class mare.ui
---@field icons table<string, table<string, string>>
ui = {}

---@param text any
---@param style? mare.Style|mare.Color
---@return mare.Span
function ui.Span(text, style) end
---@param spans (mare.Span|string)[]|string
---@return mare.Line
function ui.Line(spans) end
--- Word-wrapped line; continuation lines get `indent`.
---@param x mare.Line|string
---@param indent? (mare.Span|string)[]|integer
---@return mare.Node
function ui.Paragraph(x, indent) end
---@param items mare.Node[]
---@return mare.Node
function ui.Lines(items) end
ui.List = ui.Lines
---@param text string
---@return mare.Node
function ui.Markdown(text) end
---@param code string
---@param lang? string
---@return mare.Node
function ui.Code(code, lang) end
--- `{old=, new=, path=}` diffs two texts (3 lines of context); `{unified=}` renders an existing
--- unified diff, with `path` (highlighting for every row instead of the `+++` headers) and
--- `lines` (the longer side's line count, for the gutter width) it renders a 3-line-context
--- `mare.text.diff` exactly like `{old=, new=, path=}` of the same texts.
---@param t {old?: string, new?: string, path?: string, unified?: string, lines?: integer}
---@return mare.Node
function ui.Diff(t) end
---@param first (mare.Span|string)[]|string
---@param child mare.Node
---@param rest? (mare.Span|string)[]|string
---@return mare.Node
function ui.Indent(first, child, rest) end
---@param n? integer
---@return mare.Node
function ui.Gap(n) end
---@param label (mare.Span|string)[]
---@return mare.Node
function ui.Spinner(label) end
--- A frame animation the core plays by wall-clock time (`still` under reduce_motion).
---@param frames mare.Node[]
---@param ms integer
---@param still? mare.Node
---@return mare.Node
function ui.Frames(frames, ms, still) end
--- Nodes side by side on one row (the first line of each).
---@param items mare.Node[]
---@return mare.Node
function ui.Row(items) end
---@param child mare.Node
---@param max integer
---@param tail? boolean
---@return mare.Node
function ui.Clip(child, max, tail) end
--- Bordered panel: title in the top border, footer (`3 of 42`) in the bottom one.
--- `border_style` is a named style (MareBorder / MareBorderActive / MareBorderSearch); the
--- charset follows `mare.o.border` unless `border` is given.
---@param child mare.Node
---@param opts? {title?: string|(mare.Span|string)[], footer?: string, border_style?: string, border?: "rounded"|"single"|"double"|"thick"|"hidden"}
---@return mare.Node
function ui.Block(child, opts) end
--- Virtually scrolled list: only `height` rows around `selected` (1-based) are drawn; inside a
--- Block it gets a scrollbar. Selection = full-width MareSelection when focused, bold otherwise.
---@param items (mare.Node|string)[]
---@param opts? {selected?: integer, focused?: boolean, height?: integer}
---@return mare.Node
function ui.List(items, opts) end
--- Two panes (list + preview); stacked when narrower than `stack_below` (default 84) columns.
---@param left mare.Node
---@param right mare.Node
---@param opts? {ratio?: number, stack_below?: integer}
---@return mare.Node
function ui.Split(left, right, opts) end
--- Status capsules (Yazi): Powerline caps with a Nerd Font, padded background otherwise.
---@param list {text: string, style?: string}[]
---@param side? "left"|"right"
---@return mare.Span[]
function ui.Segments(list, side) end
---@param frac number 0..1
---@param width? integer
---@return string
function ui.Gauge(frac, width) end
--- Glyph for the active icon set (`mare.o.icons`): file, dir, command, branch, model, jobs,
--- workflow, info, warn, error, sep_left, sep_right. Sets live in `ui.icons[set][name]`.
---@param name string
---@return string
function ui.icon(name) end
--- The icon set for `icons = "auto"`; `caps` defaults to `mare.ui.caps()` (`emoji_wide = false`
--- → "ascii").
---@param caps? table
---@return "nerd"|"unicode"|"ascii"
function ui.detect_icons(caps) end
--- Terminal color depth: "truecolor" | "256" | "16" | "none" (env MARE_COLOR overrides; NO_COLOR
--- gives "none").
---@return "truecolor"|"256"|"16"|"none"
function ui.color_depth() end
--- Recompute and apply the terminal color depth from `option` (or `mare.o.terminal.color`),
--- environment variables, and probed capabilities, returning the active depth.
---@param option? "auto"|"truecolor"|"256"|"16"|"none"
---@return "truecolor"|"256"|"16"|"none"
function ui.set_color_mode(option) end
--- Interface-shell components the core draws its overlays with. Lua describes, the
--- core draws and caches: each method runs only when its input changes (an overlay opens, the
--- selection or a tab changes, a search starts), never per typed filter character.
---@class mare.ui.KeyHints
---@field lead string
---@field sep string
---@field key_style string
---@field desc_style string
---@field sep_style string
ui.KeyHints = {}
--- The status bar while an overlay is open.
---@param hints {key: string, desc: string}[]
---@return mare.Line
function ui.KeyHints:render(hints) end
---@class mare.ui.Tabs
---@field sep string
---@field sep_style string
---@field active_style string
---@field inactive_style string
---@field title_style string
ui.Tabs = {}
--- A panel's views as its border title (`Running │ All`).
---@param names string[]
---@param active integer 1-based
---@param title? string
---@return mare.Line
function ui.Tabs:render(names, active, title) end
---@class mare.ui.Select
---@field search_prompt string
---@field search_cursor string
---@field empty string
---@field preview string
ui.Select = {}
--- The frame of a list overlay; the core adds the rows, the search text and the count.
---@param s {title: string, tabs: string[], tab: integer, searching: boolean}
---@return {title: mare.Line, border: string, search: mare.Line, cursor: mare.Line, empty: mare.Line, footer_style: string, preview_title: mare.Line, preview_border: string}
function ui.Select:frame(s) end
---@class mare.ui.Confirm
---@field title string
---@field gap string between the buttons
ui.Confirm = {}
--- The yes / no dialog, drawn `c.width` cells wide (normally a ui.Block).
---@param c {prompt: string, yes: string, no: string, selected: 1|2, width: integer}
---@return mare.Node
function ui.Confirm:render(c) end
---@param s string "yes" → "[Y]es"
---@return string
function ui.Confirm.label(s) end
--- Startup logo component. Replace `bitmap` (rows; "x" = pixel) or the methods.
---@class mare.ui.Logo
---@field bitmap string[]
---@field min_width integer narrower terminals get the compact form
---@field hints {[1]: string, [2]: string}[] key hints under the logo: { key, what }
ui.Logo = {}
---@return mare.Node[] text rows of the half-block logo (body MareLogo, shadow MareLogoShadow)
function ui.Logo:lines() end
---@param info {version: string, cwd: string, model?: string}
---@return mare.Node
function ui.Logo:render(info) end
---@param info {version: string, cwd: string, model?: string}
---@return mare.Node
function ui.Logo:compact(info) end
---@return string
function ui.icon_set() end
--- Update interface-shell theme & glyph data (omitted fields stay unchanged; `glyphs = false`
--- restores the built-in spinner glyphs).
---@param opts? {border?: "rounded"|"single"|"double"|"thick"|"hidden", icons?: "nerd"|"unicode"|"ascii", glyphs?: false|string[]|{frames?: string[], still?: string}, markdown?: {glyphs?: table<string, string>, code_line_numbers?: boolean}}
function ui.set_theme_data(opts) end
--- The working line: `render(info)` returns a node (usually a Row of Frames) or nil
--- to engage the core's generic text fallback. info = { label, detail?, secs, hints = {…},
--- color = "truecolor"|"256"|"16"|"none", still, stopping? } (`still`: reduce_motion — only the still
--- frame of each ui.Frames is drawn; `stopping`: interrupted turn waiting for child stop).
---@class mare.ui.Working
---@field looks? table<string, table>
---@field frames? table<string, table>
---@field sweeps? table<string, table>
ui.Working = {}
---@param info {label: string, detail?: string, secs: integer, hints: string[], color: string, still: boolean, stopping?: boolean}
---@return mare.Node|nil
function ui.Working:render(info) end
--- An image node: the terminal's protocol (Kitty / iTerm2 / sixel) or a one-line
--- placeholder; fitted into max_rows × max_cols cells (defaults 16 × 80).
---@param opts {resource?:table, fallback?:mare.Node, path?: string, bytes?: string, image_id?: integer, max_rows?: integer, max_cols?: integer}
---@return table node
function ui.image(opts) end
--- Named style for a path by file type (Yazi filetype; rules in `ui.filetypes`).
---@param path string
---@return string
function ui.filetype_style(path) end
---@param path string
---@return string
function ui.file_icon(path) end
---@param items {text: string, done?: boolean, active?: boolean}[]
---@return mare.Node
function ui.Checklist(items) end

---@return boolean
function ui.connected() end
--- Whether this frontend currently has an interactive responder (including RPC UI hosts).
---@return boolean
function ui.can_respond() end
--- A directory for display: home as `~`, longer than `max_width` cells → `…/last/components`
--- (the rule the welcome line, the status bar and the trust question share). 0/nil = no limit.
---@param path string
---@param max_width? integer
---@return string
function ui.display_path(path, max_width) end
---@return integer
function ui.width() end
---@return integer
function ui.height() end
--- Push a transcript block; returns its id. kind: "user"|"assistant"|"reasoning"|"tool"|"info"|"error"|...
---@param kind string
---@param t? {node?: mare.Node, markdown?: string, expanded?: mare.Node}
---@return integer
function ui.push(kind, t) end
---@param id integer
---@param t {node?: mare.Node, markdown?: string, append?: string, expanded?: mare.Node, kind?: string}
function ui.update(id, t) end
--- Streaming markdown append (fast path).
---@param id integer
---@param text string
function ui.append(id, text) end
--- Mark a block finished: it is committed to terminal scrollback in order.
---@param id integer
function ui.finish(id) end
---@param id integer
function ui.remove(id) end
---@param node mare.Node
function ui.print(node) end
---@param id string
---@param slot "above-editor"|"below-editor"|nil
---@param node mare.Node
---@param order? integer
function ui.set_widget(id, slot, node, order) end
---@param id string
---@param align "left"|"right"|nil
---@param node mare.Node
---@param order? integer
---@param keep? boolean a left segment that stays whole on a narrow terminal while the rest gives way
function ui.set_status(id, align, node, order, keep) end
---@param label string|nil
---@param detail? string
---@param opts? { background?: boolean } `background`: a shell command the background key can move aside (the core shows the key hint)
function ui.working(label, detail, opts) end
---@param text string
---@param level? "info"|"success"|"warning"|"error"
---@param opts? {id?: string} Replace the visible notice with the same id.
function ui.notify(text, level, opts) end
--- Recent TUI notifications, oldest first (up to 200 entries).
---@return {text: string, level: string}[]
function ui.notification_history() end
---@async
--- Items may carry a `preview` node (shown in a pane next to the list); `tabs` gives tabbed
--- views (switch with Tab or [ ]); `kind = "confirm"` draws a centered [Y]es [N]o dialog.
--- `body` (a string or a list of lines) is shown above the options (e.g. the trust overlay).
--- `multi = true` returns an array of checked 1-based indices in the active tab: Space toggles
--- the highlighted item and Enter confirms. While typing a search, Space is part of the filter;
--- move to a result to resume toggling. Esc clears search before cancelling.
--- `accept_label` changes the accept hint (an empty string hides it); `allow_search = false`
--- disables searching.
--- `keys` adds action keys (shown in the hint bar): pressed while not searching they return the
--- highlighted item plus that key (e.g. `w` = write to file in /copy's picker); `"ctrl+d"`
--- keys also work while searching (so letters can always search). An item with `header = true`
--- is a group heading: shown, never selected, hidden while searching. The search is fuzzy;
--- `re:<regex>` and `"exact phrase"` (case-insensitive) narrow it. `vim = true`: j/k (with a
--- count prefix), g/G and h/l (tabs) move when typed before a search; ←/→ switch tabs anyway.
--- `on_highlight(index, tab) -> node|nil` builds the preview of the highlighted item lazily.
--- `title_right` (a string, drawn muted, or a `ui.Line`) ends the top border, e.g. the current
--- sort and filter; a narrow box shortens it, then leaves it out (the title always wins).
--- `split = { ratio, min }`: on a wide terminal (84 columns and more) with a preview pane, the
--- list takes `ratio` of the popup (default 0.4), at least `min` columns (default 24); the
--- preview keeps at least 20.
--- The 4th result is the search text then (re-open a picker after an action without losing it).
--- A `filter` given up front opens in searching mode: the first Esc clears the search, the next
--- one closes the select.
--- `on_open(sel)` is called once the select is open with its handle: `sel:update(items)` replaces
--- the items in place while the user is in it (an index that caught up, a list still loading),
--- keeping the search text, the searching state and the highlighted item — found again by the
--- item's `item_id`, else by its label. The index the select returns refers to the latest items.
--- Dismissal returns `nil, "cancelled"`; Ctrl+C returns `nil, "interrupted"`. A `cancel` token
--- returns `nil, "cancelled"`. When no UI responder is available, returns `nil, "unavailable"`.
--- Ctrl+C cannot be declared in `keys`.
---@param t {title?: string, title_right?: string|mare.Line, split?: {ratio?: number, min?: integer}, items?: mare.SelectItem[], selected?: integer, multi?: boolean, accept_label?: string, allow_search?: boolean, filter?: string, tabs?: {name: string, items: mare.SelectItem[], selected?: integer}[], tab?: integer, kind?: "list"|"confirm", keys?: (string|{key: string, label?: string})[], body?: string|string[], vim?: boolean, cancel?: mare.Token, on_highlight?: (fun(index: integer, tab: integer): mare.Node|nil), on_open?: fun(sel: mare.SelectHandle), wait_kind?: string, wait_title?: string}
---@return integer|integer[]|nil index, (integer|string)|nil tab_or_error, string|nil key, string|nil filter
function ui.select(t) end

--- Dedicated trusted human approval prompt: native code renders the title, bound arguments,
--- and fixed choice/key mapping, returning the chosen decision and a one-time native receipt.
---@param t {approval_binding: mare.ApprovalBinding, approval_arguments: table, tool: string, choices: ("allow"|"allow_session"|"allow_amend"|"deny"|"abort")[], amendment_description?: string, origin?: table, reason?: string, unconfined?: boolean, protected_arguments?: boolean, scoped?: boolean, preview?: mare.Node, wait_kind?: string, wait_title?: string}
---@return ("allow"|"allow_session"|"allow_amend"|"deny"|"abort")|nil decision, string? human_approval
function ui.approve(t) end

--- `header` rows are hidden while filtering; `disabled` rows remain searchable but cannot be selected.
--- `lines` (rows: `ui.Line`, spans or strings) are drawn instead of `label  detail`, so one item
--- can be a card of several rows; the search still matches `label` and `detail` only.
---@alias mare.SelectItem string|{label: string, detail?: string, preview?: mare.Node, header?: boolean, disabled?: boolean, checked?: boolean, item_id?: string|integer, lines?: (mare.Line|string)[]}

--- The handle of an open `ui.select` (its `on_open`). Only the TUI updates in place; elsewhere
--- (plain mode, RPC) `update` does nothing visible.
---@class mare.SelectHandle
local SelectHandle = {}
--- Replace the open select's items: a list of items, `{items = …}`, or `{tabs = {{name, items}, …}}`
--- for a tabbed select (each tab keeps its highlighted item). Returns false once the select has
--- closed (nothing is sent then).
---@param t mare.SelectItem[]|{items?: mare.SelectItem[], tabs?: {name: string, items: mare.SelectItem[]}[]}
---@return boolean open
function SelectHandle:update(t) end
--- Whether the select is still open.
---@return boolean
function SelectHandle:is_open() end
---@async
--- Returns `nil, "interrupted"` for Ctrl+C, `nil, "unavailable"` when no UI responder is available,
--- or `nil, "cancelled"` for other dismissals and `cancel`.
---@param t {title?: string, placeholder?: string, default?: string, secret?: boolean, cancel?: mare.Token}
---@return string|nil, string? error
function ui.input(t) end
---@async
--- Returns `nil, "interrupted"` for Ctrl+C, `nil, "unavailable"` when no UI responder is available,
--- or `nil, "cancelled"` for other dismissals and `cancel`.
---@param t {title?: string, prefill?: string, cancel?: mare.Token}
---@return string|nil, string? error
function ui.editor(t) end
---@class mare.UiQuestionOption
---@field label string
---@field description? string
---@field recommended? boolean
---@class mare.UiQuestionItem
---@field question_id string
---@field header? string
---@field question string
---@field options mare.UiQuestionOption[]
---@field multi_select? boolean
---@field allow_custom? boolean Defaults to true.
---@field allow_skip? boolean Defaults to true.
---@field preview? string
---@class mare.UiQuestionAnswer
---@field selected string[]
---@field custom? string
---@field skipped? boolean
---@class mare.UiQuestionsResult
---@field answers table<string, mare.UiQuestionAnswer>
---@field dismissed? boolean
---@field reason? "cancelled"|"interrupted"|"unavailable"
---@field note? string
---@async
--- Returns `{ answers = {}, dismissed = true, reason = "cancelled"|"interrupted" }` when dismissed,
--- `nil, "cancelled"` when cancelled via `cancel`, or `nil, "unavailable"` when no UI responder is available.
---@param t {title?: string, questions: mare.UiQuestionItem[], cancel?: mare.Token}
---@return mare.UiQuestionsResult|nil, string? error
function ui.questions(t) end
---@param title string
---@param node mare.Node
---@param opts? {context?: mare.KeyContext, cursor?: {row: integer, col: integer}}
function ui.overlay(title, node, opts) end
--- Full-screen transcript view (Ctrl+T style) showing `node`. Calling it again with the same
--- title while open refreshes it in place (keeps the scroll position, follows the end).
--- `opts.keys`: the view's own keys for its header, e.g. `{ { action = "agents.back", label =
--- "list" }, { action = { "agents.prev", "agents.next" }, label = "agent" } }` → `Esc list ·
--- Alt+←/Alt+→ agent · ↑/↓ PgUp/PgDn …`: the keys bound to those transcript-layer actions now
--- (an action without a key is left out), so a rebind changes the header.
---@param title string
---@param node mare.Node
---@param opts? {keys?: {action: string|string[], label: string}[]}
function ui.transcript(title, node, opts) end
--- Title of the plugin transcript view while it's open, else nil.
---@return string|nil
function ui.transcript_title() end
--- Close the plugin transcript view (e.g. Esc in a child's view going back to a list); with
--- `title`, only while it shows that title. The Ctrl+T transcript is left alone. Commands keep
--- their order: `ui.transcript_close(t); ui.select{…}` opens the list on the main screen.
---@param title? string
function ui.transcript_close(title) end
---@class mare.EditorPos
---@field row integer 0-based line index
---@field col integer 0-based UTF-8 byte offset on the line

---@class mare.EditorRange
---@field start mare.EditorPos
---@field end mare.EditorPos

---@class mare.EditorSelection
---@field start mare.EditorPos
---@field end mare.EditorPos
---@field anchor mare.EditorPos
---@field head mare.EditorPos
---@field text? string

---@class mare.EditorState
---@field text string
---@field cursor mare.EditorPos
---@field selection mare.EditorSelection|nil
---@field empty boolean
---@field revision integer

---@class mare.CompletionItem
---@field label string
---@field insert? string
---@field detail? string
---@field hint? string
---@field icon? string
---@field style? string
---@field auto_execute? boolean
---@field score? number
---@field source? string
---@field source_id? integer

---@class mare.CompletionContext
---@field token string
---@field start integer
---@field line_index integer
---@field text string
---@field force boolean
---@field shell_level integer
---@field shell_mode boolean

---@class mare.CompletionSource
---@field name string
---@field triggers? string[]
---@field complete fun(ctx: mare.CompletionContext): mare.CompletionItem[]|nil
---@field enabled? boolean|fun(ctx: mare.CompletionContext): boolean
---@field min_prefix? integer
---@field score_offset? number
---@field timeout_ms? integer

--- `keep_draft = true`: leave a non-empty input alone (the user's draft wins; /tree, /fork, /undo).
---@param text string
--- `prepend = true` restores before any newer draft. `images` contains attachment blocks in
--- marker order; colliding image markers are remapped while preserving the newer draft.
---@param opts? {keep_draft?: boolean, prepend?: boolean, images?: table[], image_markers?: {text:string,offset:integer}[]}
function ui.set_editor(text, opts) end
---@return string
function ui.get_editor() end
---@return mare.EditorState
function ui.editor_state() end
---@return mare.EditorPos
function ui.editor_cursor() end
---@param row integer|mare.EditorPos
---@param col? integer
function ui.set_editor_cursor(row, col) end
---@return mare.EditorSelection|nil
function ui.editor_selection() end
---@param anchor? mare.EditorPos|{anchor?: mare.EditorPos, head?: mare.EditorPos}
---@param head? mare.EditorPos
function ui.set_editor_selection(anchor, head) end
---@param text string
function ui.editor_insert(text) end
---@param range? mare.EditorRange
---@param text string
function ui.editor_replace(range, text) end

---@class mare.ui.completion
ui.completion = {}
---@param spec mare.CompletionSource
---@return fun() release
function ui.completion.register(spec) end
---@param ev mare.CompletionContext
---@return mare.CompletionItem[]
function ui.completion.collect(ev) end
---@return table[]
function ui.completion.list() end
--- Frontend display switch: "show_reasoning" (full reasoning instead of "Thought for N s"),
--- "expand_all" (the Ctrl+O compact/expanded view).
---@param name string
---@param value boolean
function ui.flag(name, value) end
--- How many messages wait in the agent queue (with an empty input, ↑ emits `input/queue-select`
--- and Enter emits `input/queue-menu`).
---@param n integer
function ui.set_queued(n) end
--- The queue editing state the key handler routes by: 0 nothing selected, 1 a queued message
--- selected (↑/↓ emit `input/queue-move`, Enter `input/queue-edit`, Delete `input/queue-remove`,
--- Esc `input/queue-cancel`), 2 one being edited (Esc emits `input/queue-cancel`).
---@param mode integer
function ui.set_queue_mode(mode) end
---@param text string
function ui.editor_hint(text) end
--- The input's key bindings, `{ key = action }` (each key's topmost binding; `mare.keymap.set` changes them).
---@return table<string, string>
function ui.keymap() end
--- The canonical name of a key or key sequence ("<C-t>" -> "ctrl+t", "<C-x>g" -> "ctrl+x g").
---@param key string
---@return string
function ui.normalize_key(key) end
---@param colors table<string, string>
function ui.theme(colors) end
function ui.clear() end
--- Leave mare. Without `force`, an exit guard (`ui.exit_guard`) gets to ask first.
---@param opts? {force?: boolean}
function ui.exit(opts) end
--- While on, a user's exit (quit keys, /exit) emits `app/exit-requested` instead of quitting;
--- the guard's owner calls `ui.exit{ force = true }` once confirmed (the jobs plugin does this
--- while background jobs run).
---@param on boolean
function ui.exit_guard(on) end
---@param node mare.Node
---@param width? integer
---@return string
function ui.render_text(node, width) end

--- The system clipboard: over SSH tmux's clipboard
--- (`tmux load-buffer -w -`) or OSC 52; locally the native clipboard, then PowerShell on WSL, then
--- the terminal. OSC 52 goes through the single write exit (or `/dev/tty` without a TUI) and is
--- capped at `max_osc52_bytes`; it can't be confirmed, so `verified = false` — never say "copied".
---@class mare.clipboard
mare.clipboard = {}
---@type integer
mare.clipboard.max_osc52_bytes = 100000
---@async
---@param text string
---@return {ok: boolean, method?: "native"|"tmux"|"osc52"|"wsl", verified?: boolean, bytes: integer, error?: string}
function mare.clipboard.copy(text) end

mare.ui = ui

-- ---------------------------------------------------------------------------
-- customizable UI
-- ---------------------------------------------------------------------------

--- Set (or with one argument, read) a named style.
---@param name mare.StyleName
---@param spec? mare.Style|string
---@return table|nil
function ui.style(name, spec) end
---@return mare.StyleName[]
function ui.styles() end
--- Set a theme default (not recorded as an explicit override; themes use this).
---@param name string
---@param spec table
function ui.theme_style(name, spec) end
--- Explicit `mare.ui.style` calls, which win over any theme.
---@return table<string, table>
function ui.style_overrides() end
--- Drop the explicit override of `name` and restore `spec` as its default.
---@param name string
---@param spec? table
function ui.reset_style(name, spec) end
--- Wrap or replace `select`, `input`, `confirm`, or `notify`; layers stack (`next` delegates down)
--- and `release()` removes this layer while preserving lower ones.
---@param name "select"|"input"|"confirm"|"notify"
---@param fn fun(opts: table, next: fun(opts?: table, ...): ...): ...
---@return fun() release
function ui.override(name, fn) end
--- Returns true for Yes, false for No, or nil and the dismissal reason (`"cancelled"`, `"interrupted"`, or `"unavailable"`).
---@param prompt string|{prompt?: string, title?: string, default?: boolean, cancel?: mare.Token}
---@param default? boolean
---@param opts? {cancel?: mare.Token}
---@return boolean|nil, string? error
function ui.confirm(prompt, default, opts) end
---@param t {prompt?: string, placeholder?: string, busy_placeholder?: string, border?: mare.StyleName, shell?: {placeholder?: string, private_placeholder?: string, border?: mare.StyleName}|false}
function ui.editor_config(t) end
--- Register a renderer. kind: "tool:<name>", "message:user", "message:assistant", "message:reasoning".
---@param kind string
---@param r mare.Renderer
function ui.renderer(kind, r) end
--- The topmost renderer registered for `kind`, or nil.
---@param kind string
---@return {compact?: function, expanded?: function}?
function ui.get_renderer(kind) end

---@class mare.Renderer
---@field compact? function (call, result) or (text) → mare.Node
---@field expanded? function same arguments, for Ctrl+T

---@class mare.EntryRenderer
---@field render? fun(entry: table, opts: {expanded: boolean}): mare.Node
---@field compact? fun(entry: table, opts: {expanded: boolean}): mare.Node
---@field expanded? fun(entry: table, opts: {expanded: boolean}): mare.Node

---@param kind string
---@param r mare.EntryRenderer|fun(entry: table, opts: {expanded: boolean}): mare.Node
---@return fun() remove
function ui.entry_renderer(kind, r) end

---@param kind string
---@return {compact?: function, expanded?: function}?
function ui.get_entry_renderer(kind) end

---@alias mare.MarkdownMessageType "user"|"assistant"|"assistant-reasoning"|"custom"|"summary"|"changelog"

---@class mare.MarkdownTransformContext
---@field message_type? mare.MarkdownMessageType
---@field is_streaming? boolean
---@field available_width? integer
---@field message_id? string
---@field created_at_ms? integer

---@alias mare.MarkdownTransformer fun(markdown: string, context: mare.MarkdownTransformContext): string

---@param fn mare.MarkdownTransformer
---@param opts? {message_types?: mare.MarkdownMessageType[]}
---@return fun() remove
function ui.markdown_transform(fn, opts) end

---@param message_type? mare.MarkdownMessageType
---@return boolean
function ui.has_markdown_transforms(message_type) end

---@param text string
---@param context? mare.MarkdownTransformContext
---@return string
function ui.transform_markdown(text, context) end

---@class mare.ui.UserMessage
ui.UserMessage = {}
---@param text string
---@return mare.Node
function ui.UserMessage:render(text) end

--- The dim line closing each turn ("worked 4s · 1.2k tokens · 02:46" / "interrupted after 4s").
---@class mare.ui.TurnEnd
ui.TurnEnd = {}
---@param info {secs: number, interrupted: boolean, failed: boolean, output_tokens: integer, clock: string}
---@return mare.Node? nil shows nothing
function ui.TurnEnd:render(info) end
--- `secs` as "42s" or "3m 05s".
---@param secs number
---@return string
function ui.TurnEnd.duration(secs) end
--- A token count as "512", "1.2k" or "3.4M".
---@param n number
---@return string
function ui.TurnEnd.tokens(n) end

---@class mare.ui.AssistantMessage
---@field render? fun(self, text: string): mare.Node replace the finished message (streaming stays native)
ui.AssistantMessage = {}
---@return string
function ui.AssistantMessage:marker() end

---@class mare.ui.Reasoning
ui.Reasoning = {}
---@param text string
---@param secs number
---@return mare.Node
function ui.Reasoning:render(text, secs) end
---@return mare.Node
function ui.Reasoning:expanded(text, secs) end

---@class mare.ui.Notice
ui.Notice = {}
---@return mare.Node
function ui.Notice:render(text, level) end

---@class mare.ToolCallInfo
---@field summary string e.g. "Read(src/main.rs)"
---@field tool? table tool definition
---@field text? string result text
---@field live? string streaming output while running
---@field source? string optional invocation source marker (e.g. "call_tool")

---@class mare.ui.ToolCall
---@field tenses table<string, string[]> lower-case tool name → { present, past } verb ("Reading", "Read")
ui.ToolCall = {}
--- The tool's verb for `state`: present while running, past when done; nil when it has none.
---@param name string
---@param state "running"|"ok"|"error"
---@return string?
function ui.ToolCall:verb(name, state) end
---@param summary string
---@param state "running"|"ok"|"error"
---@param source? string
---@return mare.Node
function ui.ToolCall:header(summary, state, source) end
---@param node mare.Node
---@return mare.Node
function ui.ToolCall:body(node) end
---@return mare.Node
function ui.ToolCall:result_text(text, is_error, expanded) end
---@param call {id: string, name: string, arguments: table}
---@param result? {content: any, is_error?: boolean, details?: table}
---@param expanded boolean
---@param info mare.ToolCallInfo
---@return mare.Node
function ui.ToolCall:render(call, result, expanded, info) end
--- The head of a running call: a spinner, or a steady pending dot when `still` is set.
---@param label mare.Span[]
---@return mare.Node
function ui.ToolCall:running(label) end
--- Whether `node` is, or holds, a diff (edit / write / apply_patch results).
---@param node any
---@return boolean
function ui.ToolCall:has_diff(node) end

--- Register or update a status bar segment; returns a release function.
---@param id string
---@param spec {align?: "left"|"right", order?: number, keep?: boolean, render: fun(width: integer): mare.Node|nil}
---@return fun() release
function ui.status(id, spec) end
--- Redraw one registered status segment (`id`) or all of them (`nil`).
---@param id? string
function ui.status_redraw(id) end

---@class mare.ui.Editor
---@field prompt string
---@field placeholder string
---@field busy_placeholder string
---@field border mare.StyleName
ui.Editor = {}
function ui.Editor:apply() end

---@class mare.ui.EditorChrome
---@field prompt_style mare.Color
---@field placeholder_style mare.Color
ui.EditorChrome = {}
---@param s {prompt: string, placeholder: string, border: mare.StyleName, shell: integer, working: boolean, empty: boolean}
---@return {prompt?: string, prompt_style?: mare.Color, placeholder?: string, placeholder_style?: mare.Color, border?: mare.StyleName}|nil
function ui.EditorChrome:render(s) end

---@class mare.ui.NoticeCard
ui.NoticeCard = {}
---@param p {text: string, level: string, icon: string, title: string, style: mare.StyleName, width: integer, is_first: boolean, max_rows: integer}
---@return mare.Node|nil
function ui.NoticeCard:render(p) end

---@class mare.ui.CompletionPopup
---@field border mare.StyleName
ui.CompletionPopup = {}
---@param p {title: string, items: {label: string, detail?: string, hint?: string, icon?: string, style?: mare.StyleName}[], selected: integer, total: integer, width: integer, height: integer, commands: boolean}
---@return mare.Node|nil
function ui.CompletionPopup:render(p) end

---@class mare.ui.WhichKey
---@field border mare.StyleName
---@field title_style mare.StyleName
---@field key_style mare.StyleName
---@field action_style mare.StyleName
ui.WhichKey = {}
---@param p {prefix: string, bindings: {key: string, action: string}[], width: integer}
---@return mare.Node|nil
function ui.WhichKey:render(p) end

---@class mare.ui.HelpModal
ui.HelpModal = {}
---@param s {title: string, tabs: string[], tab: integer, searching: boolean}
---@return table|nil
function ui.HelpModal:frame(s) end

---@class mare.ui.InfoModal
---@field border mare.StyleName
---@field title_style mare.StyleName
---@field footer_style mare.StyleName
ui.InfoModal = {}
---@param s {title: string, total: integer, shown: integer, scroll: integer}
---@return {title?: mare.Node|string, border?: mare.StyleName, footer_style?: mare.StyleName}|nil
function ui.InfoModal:frame(s) end

---@class mare.ui.InputModal
---@field border mare.StyleName
---@field title_style mare.StyleName
ui.InputModal = {}
--- Secret inputs use a protected core frame and do not call this method.
---@param s {title: string, placeholder: string, secret: boolean}
---@return {title?: mare.Node|string, border?: mare.StyleName}|nil
function ui.InputModal:frame(s) end

---@class mare.ui.SelectRow
---@field header_style mare.StyleName
---@field detail_style mare.StyleName
ui.SelectRow = {}
---@param item {label: string, detail?: string, header: boolean, selected: boolean, filter: string, index: integer}
---@return mare.Node|nil
function ui.SelectRow:render(item) end

---@class mare.ImageBlock
---@field type "image"
---@field data string base64
---@field mime string
---@field width integer
---@field height integer

---@class mare.image
mare.image = {}
--- Load, downscale (longest side ≤ max_dim) and encode an image file.
---@async
---@param path string
---@param opts? {max_dim?: integer}
---@return mare.ImageBlock|nil, string|nil err
function mare.image.load(path, opts) end
---@async
---@param opts? {max_dim?: integer}
---@return mare.ImageBlock|nil, string|nil err
function mare.image.clipboard(opts) end
---@param path string
---@return boolean
function mare.image.is_image(path) end
--- Decode base64-encoded image bytes within safety limits, downscale if oversized, and re-encode as PNG (or JPEG when large).
---@async
---@param data string
---@return {mime: string, data: string}|nil, string|nil err
function mare.image.to_png(data) end
--- Validate base64-encoded image bytes by magic bytes and bounded decode, returning the detected MIME type (`image/png`, `image/jpeg`, `image/gif`, or `image/webp`) and normalized base64.
---@async
---@param data string
---@return {mime: string, data: string}|nil, string|nil err
function mare.image.validate(data) end

--- Desktop notification (OSC 9 / 777 / kitty 99, tmux & screen passthrough, BEL fallback);
--- returns false when suppressed.
---@param text string
---@param opts? {only_unfocused?: boolean, protocol?: "auto"|"osc9"|"osc777"|"osc99"}
---@return boolean
function ui.notify_os(text, opts) end
--- Set the terminal window title (OSC 2); no-op without a TUI.
---@param text string
---@return boolean sent
function ui.set_title(text) end
---@return boolean
function ui.focused() end
--- Terminal capabilities: static table + startup probe (DECRQM 2026/2027, XTGETTCAP, kitty
--- keyboard, cursor position) + the `terminal_caps` option + `ui/caps` listeners. `width_method`
--- is the width rule ("unicode" = grapheme clusters, "wcwidth" = code point by code point; set it
--- through `terminal_caps` or `MARE_WIDTH_METHOD`); `emoji_wide = false` switches `icons = "auto"`
--- to the ASCII set.
---@return {sync: boolean, scroll_region: boolean, position_known: boolean, truecolor: boolean, grapheme: boolean, kitty_keyboard: boolean, probed: boolean, term: string, term_program: string, multiplexer: ""|"tmux"|"zellij"|"screen", images: "kitty"|"iterm2"|"sixel"|"none", hyperlinks: boolean, xtversion: string, da1: integer[], cell_px?: integer[], emoji_wide: boolean, terminal: string, width_method: "unicode"|"wcwidth"}
function ui.caps() end
--- Frames that took longer than 16ms, newest last (128 kept): {at_ms, ms, what}.
---@return {at_ms: integer, ms: number, what: string}[]
function ui.slow_frames() end

--- Validate and select a tmTheme XML document; nil restores background-based syntax defaults.
--- Invalid or >1 MiB documents throw without changing the active document.
---@param document? string
---@param validate_only? boolean Validate without publishing
function ui.syntax_theme(document, validate_only) end

---@class mare.MarkdownBlock
---@field start integer Zero-based UTF-8 source byte offset; identifies an occurrence.
---@field end integer Exclusive source byte offset.
---@field source string Parser-normalized code, without container prefixes or fence markers.
---@field language string First information-string word, ASCII lowercase.
---@field info string Full information string.
---@field closed boolean False while the closing fence is absent.

--- Native Markdown parser boundaries, shared by live/replayed code presentation. Indented
--- code and inline code are not fenced blocks. This function does not invoke renderers or IO.
---@param text string
---@return mare.MarkdownBlock[]
function ui.markdown_blocks(text) end

--- Pure cached presentation node. Invalid/duplicate block offsets are rejected; absent
--- offsets render the original code card. Images use the existing frontend image protocol.
--- The caller must retain source fallback when an artifact or image capability is absent.
---@param text string
---@param blocks {start: integer, node: mare.Node}[]
---@return mare.Node
function ui.PresentedMarkdown(text, blocks) end

--- Validate conversion to a cached frontend node. Returns false for nil/unsupported empty
--- descriptors, throws for invalid typed fields; performs no rendering or filesystem IO.
---@param node mare.Node
---@return boolean accepted, {images:boolean} info Native traversal includes images inside Markdown children
function ui.validate_node(node) end

--- Bounded immutable component-description copy, preserving binary image bytes. No callbacks,
--- userdata or cycles; <=64 nesting levels/50000 entries/32MiB content. Does not render or do IO.
---@param node mare.Node
---@return mare.Node
function ui.snapshot_node(node) end
--- Existing component descriptions as JSON-safe wire values. Images require bytes (<=8MiB
--- each), become canonical padded base64 `data`, and omit local paths. Plain text must be UTF-8.
--- Empty component arrays retain their array identity. No closures, handles or terminal IO.
---@param node mare.Node
---@return mare.NodeWire wire
function ui.node_to_wire(node) end
--- Decode that same description into Lua component tables. Image data is strictly bounded
--- canonical base64; incoming raw bytes/local image paths are refused. No image validity claim.
---@param wire mare.NodeWire
---@return mare.Node
function ui.node_from_wire(wire) end

--- Allocate a nonzero native image occurrence id (fails on exhaustion). ui.image allocates
--- once when omitted. Keep the id with that logical component across redraws; create a distinct
--- id per occurrence even for identical pixels. Content encoding caches remain shared.
--- Caller scope/document disposal removes components; ids are not authorization or durable
--- artifact handles. Wire ids belong to the source frontend namespace; node_from_wire allocates
--- fresh local ids, so keep the decoded description across redraws.
---@return integer
function ui.image_id() end

---@class mare.UiRegionOpts
---@field max_height? integer Maximum height in rows for this region (>= 1; >= 3 for `completion`).
---@field hidden? boolean Whether to hide this region (`working`, `above-editor`, `below-editor`).

---@class mare.UiLayoutOpts
---@field order? string[] Ordered list of stacked region names.
---@field regions? table<string, mare.UiRegionOpts> Per-region overrides.

---@class mare.TerminalProbeOpts
---@field enabled? boolean Enable post-first-frame terminal capability probe (default `true`).
---@field background? boolean Include OSC 11 background query in the terminal probe (default `true`).

---@class mare.TerminalOpts
---@field ambiwidth? "single"|"double" Width of East Asian ambiguous characters (`"single"` | `"double"`).
---@field color? "auto"|"truecolor"|"256"|"16"|"none" Terminal color depth override (default `"auto"`).
---@field probe? mare.TerminalProbeOpts Startup terminal probe controls.

---@class mare.UiWorkingOpts
---@field preset? string Working-line preset name (`"combined"|"mare"|"spinner"|"codex"|"cc"|"kimi"|"shimmer"|"pulse"|"dots"|"wave"|"foam"|"sail"|"sea"` or custom key in `presets`, default `"combined"`).
---@field presets? table<string, table> Named custom working-line presets (may declare `like = "<preset>"` to inherit from another preset).
---@field glyph? string Glyph animation name (`"swell"|"ember"|"braille"|"moon"|"phase"|"wave"|"sail"|"bullet"|"pulse"|"none"` or custom key in `glyphs`, default `"swell"`).
---@field label? "accent"|"shimmer"|"glint"|"foam"|"dots" Label animation treatment (default `"glint"`).
---@field elapsed? "auto"|"seconds"|"off" Elapsed time display mode (default `"auto"`).
---@field tokens? boolean Show detail/token count (`~1.2k` format, default `true`).
---@field interrupt_hint? boolean Show keymap-derived interrupt hint (`Esc to interrupt`, default `true`).
---@field tips? boolean|string[]|{items?: string[], delay_s?: integer, interval_s?: integer} Show occasional usage tips next to the working line (default `true`).
---@field sep? "dot"|"bullet" Tail separator (default `"dot"`).
---@field tools? "still"|"spin" Running tool-row indicator when `ui.tool_call.spinner` is unset (default `"still"`).
---@field interval_ms? integer Custom animation interval in milliseconds.
---@field text_throttle_ms? integer Streaming working-line update throttle in milliseconds (default `250`).
---@field frames? {frames: string[], ms?: integer, still?: string, ascii?: table, tint?: string, hold?: integer, render?: fun(info: table): mare.Node}
---@field verbs? table<string, string>
---@field glyphs? table<string, table>

---@class mare.UiSpinnerOpts
---@field spinner? boolean|string|{frames: string[], ms?: integer, still?: string, ascii?: table, tint?: string, hold?: integer, render?: fun(info: table): mare.Node}

---@class mare.UiOpts
---@field layout? string[]|mare.UiLayoutOpts Stacked region layout order and/or per-region overrides.
---@field working? mare.UiWorkingOpts Fine-grained working line appearance and verb overrides.
---@field tool_call? mare.UiSpinnerOpts Running tool-call spinner options.
---@field stopping? mare.UiSpinnerOpts Stopping indicator spinner options.

--- Configure the stacked region layout (`ui.layout`). Errors on invalid region names, missing
--- `editor`/`status`, duplicate regions, or invalid region options, keeping the previous layout.
--- Pass `{ persist = false }` to apply the layout without writing `mare.o.ui.layout`.
---@param layout? string[]|mare.UiLayoutOpts|nil
---@param opts? {persist?: boolean}
function ui.set_layout(layout, opts) end
--- Set the frontend viewport dimensions (each in `1..=10000`). Returns `true` if either dimension changed.
---@param width? integer
---@param height? integer
---@return boolean changed
function ui.set_viewport(width, height) end
--- Set the width rule for East Asian ambiguous characters (`"single"` or `"double"`).
---@param mode "single"|"double"
function ui.set_ambiwidth(mode) end

---@class mare.UiFrontend
---@field editor_is_empty fun(): boolean
---@field is_working fun(): boolean
---@field update_editor fun(text: string, opts?: table): table|nil
---@field send_editor fun(text?: string, mode: string, attachments?: table[], allow_empty?: boolean): table|nil
---@field set_external_editor fun(action: string, text?: string): table|nil
---@field get_select_preview fun(caller_id?: integer, select_id: string, index: integer, tab: integer): table|nil

--- RPC frontend bridge table when attached to an RPC host, or `nil` in local frontends.
---@type mare.UiFrontend?
ui.frontend = nil

--- Opaque process layout revision; changes on theme/style/width-rule writes. Pure observation,
--- not a session identity. Used to coalesce presentation invalidation without polling or IO.
---@return integer
function ui.layout_revision() end

--- Register a scoped, asynchronous image-byte resolver (64 namespaces maximum). Callback
--- executes in a fresh ephemeral read-only execution. Native workers queue at most 16
--- pending requests per resolver; reads settle within 30s. Disposal cancels loads and
--- evicts that registration's materialized bytes. Re-registration has a new identity.
---@param namespace string ASCII letters/digits/_/-; <=64 bytes
---@param load async fun(key:string):string raw image bytes, <=8MiB
---@return fun() dispose
function ui.image_resolver(namespace, load) end
--- Create a small immutable resource description without reading storage. key must include
--- the storage owner/session_epoch; the resolver must enforce actual storage authorization.
--- Wire descriptions omit native resolver identity and rebind only to a local namespace.
---@param namespace string
---@param key string opaque resolver payload, <=128KiB
---@param width integer 1..4096
---@param height integer 1..4096
---@return {namespace:string,key:string,width:integer,height:integer,resolver_id:integer} resource
function ui.image_resource(namespace, key, width, height) end
---@return {materialized_bytes:integer,entries:integer,budget_bytes:integer,max_image_bytes:integer,max_pending_per_resolver:integer}
function ui.image_cache_info() end
---@class mare.OutputImageKey
---@field session_id string Storage owner; validated against the originating session
---@field session_epoch string Presentation owner session epoch
---@field spill_id string Opaque output locator; never parse it or treat it as a path
---@field sha256 string Lowercase SHA-256 of the complete bytes

---@meta _
-- GENERATED from types/mare/services/contracts/ui.lua (service `ui`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.UiService
local M = {}

--- Register a named key action with optional default keys; removed when the caller is disposed.
---@param name string
---@param fn? fun(ev: {key?: string, context?: mare.KeyContext}): boolean
---@param opts? {contexts?: mare.KeyContext[]?, desc?: string?, keys?: table<mare.KeyContext, any>?, [string]: any}
---@return table
function M:action(name, fn, opts) end

--- Bind a saved output image artifact to the current session epoch and return a renderable `mare.Node`.
---@param artifact {height: integer, session_id: string, sha256: string, spill_id: string, width: integer, [string]: any}
---@param opts? {image_id?: integer, max_cols?: integer, max_rows?: integer, fallback?: mare.Node?, [string]: any}
---@return mare.Node
function M:artifact_image(artifact, opts) end

--- Dispatch a key sequence or named keymap action in the session runtime.
---@param context? string
---@param key? string
---@param action? string
---@return {action?: string, context?: string, declined: boolean, handled: boolean, masked: boolean, reason?: string}
function M:call_keymap(context, key, action) end

--- Cancel outstanding presenter work for a Markdown document and fall back to source.
---@param document_id string
---@return table
function M:cancel_markdown(document_id) end

--- Emit a durable, reflowable titled Markdown card in the transcript; returns block id/status or `nil, err`.
---@param opts {after_entry_id?: string, card_id?: string, kind?: string, markdown: string, meta?: table<string, any>, persist?: boolean, title: string, [string]: any}
---@return integer|boolean|nil
---@return string?
function M:card(opts) end

--- Clear the conversation scrollback in the connected UI frontend.
---@return table
function M:clear() end

--- Register a completion source for `/` commands, `@` mentions, or custom triggers; withdrawn on dispose.
---@param spec mare.CompletionSource
---@return fun() dispose releases this registration
function M:completion_register(spec) end

--- Present a yes/no confirmation prompt and return `boolean|nil, err?`.
---@param prompt string|table
---@param default? boolean
---@param opts? table
---@return boolean|nil
---@return string?
function M:confirm(prompt, default, opts) end

--- Register a plugin-scoped keymap subcontext; removed when the caller is disposed.
---@param name string
---@param opts {active?: boolean, parent: mare.KeyContext, swallow_printable?: boolean, [string]: any}
---@return {name: string, remove: fun(self?), set_active: (fun(self_or_active: any, active?: boolean): boolean), is_active: (fun(): boolean)}
function M:context(name, opts) end

--- Delete the topmost runtime keymap binding of a key sequence in a context.
---@param context string
---@param key string
---@return {context: string, key: string}
function M:delete_keymap(context, key) end

--- Remove a caller-owned Markdown document and cancel its pending work.
---@param document_id string
---@return table
function M:delete_markdown(document_id) end

--- Open an interactive multi-line editor dialog and return the edited text or `nil, err`.
---@param opts? {prefill?: string, title?: string, cancel?: mare.Token?, [string]: any}
---@return string|nil
---@return string?
function M:editor(opts) end

--- Configure editor appearance options (`prompt`, `placeholder`, `busy_placeholder`, `border`, `shell`).
---@param opts? {border?: string, busy_placeholder?: string, placeholder?: string, prompt?: string, shell?: table<string, any>|boolean, [string]: any}
---@return table
function M:editor_config(opts) end

--- Return the current 0-based editor cursor position `{row, col}` aligned to grapheme boundaries.
---@return mare.EditorPos
function M:editor_cursor() end

--- Set the transient hint text displayed beneath the editor.
---@param text string
---@return table
function M:editor_hint(text) end

--- Insert `text` at the current cursor (replacing any active selection) as an undoable edit.
---@param text string
---@return table
function M:editor_insert(text) end

--- Replace `range` (`{start, end}` or nil for selection/cursor) with `text` as an undoable edit.
---@param range? mare.EditorRange
---@param text string
---@return table
function M:editor_replace(range, text) end

--- Return the active editor selection `{start, end, anchor, head, text}`, or nil when collapsed.
---@return mare.EditorSelection|nil
function M:editor_selection() end

--- Return the full editor state snapshot (`text`, `cursor`, `selection`, `empty`, `revision`).
---@return mare.EditorState
function M:editor_state() end

--- Request the UI frontend to exit (`force = true` bypasses exit guards).
---@param opts? {force?: boolean}
---@return table
function M:exit(opts) end

--- Enable or disable the frontend exit guard.
---@param on boolean
---@return table
function M:exit_guard(on) end

--- Set a named boolean UI context flag used by keymap `when` expressions.
---@param name string
---@param value boolean
---@return table
function M:flag(name, value) end

--- Return the current raw editor input text (including leading `!`/`!!` in shell mode).
---@return string
function M:get_editor() end

--- Return the runtime keymap snapshot (contexts, actions, bindings, and essential conflicts).
---@param context? string
---@return {actions: {contexts?: string[], core?: boolean, desc?: string, essential?: boolean, keys?: string[], name: string}[], bindings: ({action: string|boolean, active: boolean, context: string, declines: boolean, desc?: string, key: string, key_label: string, rank: string, unbound: boolean, when: string[]})[], conflicts: {missing_essential: {action: string, context: string}[], unbound_essential: {action: string, context: string}[]}, contexts: {bindable: boolean, chain: string[], core: boolean, name: string, parents: string[]}[]}
function M:get_keymap(context) end

--- Latest rendered snapshot for a caller-owned Markdown document.
---@param document_id string
---@return mare.MarkdownSnapshot
function M:get_markdown(document_id) end

--- Evaluate the active `ui.select` preview callback for the highlighted item and tab.
---@param select_id string
---@param index integer
---@param tab? integer
---@return {preview?: string|mare.Node}
function M:get_select_preview(select_id, index, tab) end

--- Register an ordered handler for `input/submit` ("submit") or `key/interrupt` ("interrupt"); withdrawn with the caller.
---@param kind "submit"|"interrupt"
---@param name string
---@param opts {order: number}
---@param fn? fun(ev: table): boolean
---@return fun() dispose releases this registration
function M:handler(kind, name, opts, fn) end

--- Present a single-line input prompt and return the entered string or `nil, err`.
---@param opts? table<string, any>
---@return string|nil
---@return string?
function M:input(opts) end

--- Invalidate a rendered widget or status segment, or update viewport dimensions and trigger resize.
---@param component? string
---@param opts? {viewport?: {height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer}}
---@return table
function M:invalidate(component, opts) end

--- Invalidate cached Markdown presentations across active documents after a display or presenter state change.
---@return table
function M:invalidate_presentations() end

--- Return a summary map of active key bindings.
---@return table<string, string>
function M:keymap() end

--- Active fenced-code presenter registrations, sorted by language.
---@return {data: {language: string, preview: boolean, requires_artifact_images: boolean, timeout_ms: integer}[]}
function M:list_markdown_presenters() end

--- Register a fenced-code presenter for `language`; disposing the returned function removes it and cancels its work.
---@param language string
---@param def {render: async fun(request: mare.MarkdownRequest): (mare.Node|{compact: mare.Node, expanded?: mare.Node})?, preview?: boolean, timeout_ms?: integer, requires_artifact_images?: boolean}
---@return fun() dispose releases this registration
function M:markdown_presenter(language, def) end

--- Show a transient notification message with optional severity level (`info`, `success`, `warning`, `error`).
---@param text string
---@param level? "info"|"success"|"warning"|"error"
---@return table
function M:notify(text, level) end

--- Display a titled modal overlay containing `node`.
---@param title string
---@param node mare.Node
---@param opts? {cursor?: {col: integer, row: integer}, context?: mare.KeyContext?, [string]: any}
---@return table
function M:overlay(title, node, opts) end

--- Append a rendered `mare.Node` block to the conversation scrollback.
---@param node mare.Node
---@return table
function M:print(node) end

--- Present a structured multi-question prompt to the user and return the answers or `nil, err`.
---@param opts {questions: mare.UiQuestionItem[], title?: string, cancel?: mare.Token?, [string]: any}
---@return mare.UiQuestionsResult|nil
---@return string?
function M:questions(opts) end

--- Register a display-only custom entry renderer owned by the caller.
---@param kind string
---@param opts? {compact?: (fun(entry: table, opts?: { expanded?: boolean }): mare.Node), expanded?: (fun(entry: table, opts?: { expanded?: boolean }): mare.Node), render?: (fun(entry: table, opts?: { expanded?: boolean }): mare.Node)}
---@return fun() dispose releases this registration
function M:register_entry_renderer(kind, opts) end

--- Register a Markdown transformer for selected message surfaces, owned by the caller.
---@param opts? {message_types?: ("user"|"assistant"|"assistant-reasoning"|"custom"|"summary"|"changelog")[], transform?: (fun(text: string, ctx?: mare.MarkdownTransformContext): string|{ text: string })}
---@return fun() dispose releases this registration
function M:register_markdown_transform(opts) end

--- Wrap or replace `select`, `input`, `confirm`, or `notify`; released on dispose.
---@param name "select"|"input"|"confirm"|"notify"
---@param fn fun(opts: table, next: fun(opts?: table, ...): ...): ...
---@return fun() dispose releases this registration
function M:register_override(name, fn) end

--- Render fenced blocks in a caller-scoped Markdown document asynchronously via registered presenters.
---@param document_id string
---@param source string
---@param opts? {base_node?: mare.Node, replay?: boolean, viewport?: {height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer}, on_update?: fun(node: mare.Node, revision: integer, snapshot: mare.MarkdownSnapshot)}
---@return mare.MarkdownSnapshot
function M:render_markdown(document_id, source, opts) end

--- Render a `mare.Node` into plain text wrapped to optional column `width`.
---@param node mare.Node
---@param width? integer
---@return string
function M:render_text(node, width) end

--- Register a display renderer for a message or tool kind (`tool:<name>`, `message:user`, etc.); removed on dispose.
---@param kind string
---@param r mare.Renderer
---@return table
function M:renderer(kind, r) end

--- Present an interactive item picker and return `(index, tab_or_error, key, filter)`.
---@param opts? table<string, any>
---@return integer|nil
---@return (integer|string)|nil
---@return string|nil
---@return string|nil
function M:select(opts) end

--- Submit the current editor buffer (or override text) to the session's agent lane.
---@param text? string
---@param opts? {attachments?: table<string, any>[], mode?: "prompt"|"steer"|"follow_up"}
---@return {handled: boolean, result?: table<string, any>, revision: integer}
function M:send_editor(text, opts) end

--- Set or update the main input editor buffer text and optional draft/attachment options.
---@param text string
---@param opts? {image_markers?: {offset: integer, text: string, [string]: any}[], images?: table<string, any>[], keep_draft?: boolean, prepend?: boolean, [string]: any}
---@return table
function M:set_editor(text, opts) end

--- Move the editor cursor to `(row, col)` (or `{row, col}`), clamped to grapheme boundaries, and clear selection.
---@param row integer|mare.EditorPos
---@param col? integer
---@return table
function M:set_editor_cursor(row, col) end

--- Set the editor selection `{anchor, head}` or pass nil to clear it.
---@param anchor? (mare.EditorPos|{anchor?: mare.EditorPos, head?: mare.EditorPos})
---@param head? mare.EditorPos
---@return table
function M:set_editor_selection(anchor, head) end

--- Acquire, commit, or cancel the per-session external editor lock.
---@param action "start"|"commit"|"cancel"
---@param text? string
---@return {program?: string, revision: integer, running: boolean, text: string}
function M:set_external_editor(action, text) end

--- Bind or mask a key sequence in a context for the caller's lifetime.
---@param context string
---@param key string
---@param action string|boolean
---@param opts? {desc?: string, priority?: number, unique?: boolean, when?: string[]}
---@return fun() dispose releases this registration
function M:set_keymap(context, key, action, opts) end

--- Update the active queue input mode indicator in the UI.
---@param mode integer
---@return table
function M:set_queue_mode(mode) end

--- Update the displayed count of queued inputs in the UI.
---@param n integer
---@return table
function M:set_queued(n) end

--- Register a named statusline segment; returns a release disposer.
---@param status_id string
---@param def {align?: "left"|"right", order?: number, keep?: boolean, render: fun(width: integer, flush_gen?: integer): mare.Node|nil}
---@return fun() dispose releases this registration
function M:status(status_id, def) end

--- Set a named UI style override; the previous style returns when the caller is disposed.
---@param name mare.StyleName
---@param spec mare.Style|string
---@return table
function M:style(name, spec) end

--- Apply a color map to the connected UI frontend.
---@param colors table<string, string>
---@return table
function M:theme(colors) end

--- Open or update the live transcript view with `title` and `node`.
---@param title string
---@param node mare.Node
---@param opts? {keys?: ({action: string|any[], label: string, [string]: any})[], [string]: any}
---@return table
function M:transcript(title, node, opts) end

--- Close the open transcript view (optionally matching `title`).
---@param title? string
---@return table
function M:transcript_close(title) end

--- Return the title of the currently open transcript view, or nil.
---@return string|nil
function M:transcript_title() end

--- Run registered Markdown transformers for a message surface.
---@param text string
---@param opts? {available_width?: integer, created_at_ms?: integer, is_streaming?: boolean, message_id?: string, message_type?: "user"|"assistant"|"assistant-reasoning"|"custom"|"summary"|"changelog"}
---@return {text: string}
function M:transform_markdown(text, opts) end

--- Synchronize the external editor draft state and broadcast updated state to attached frontends.
---@param text string
---@param opts? {attachments?: {attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer}[], image_markers?: string[], offset?: integer, shell_mode?: boolean}
---@return {attachments?: {attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer}[], external_editor_running: boolean, image_markers?: string[], offset: integer, revision: integer, shell_mode: boolean, text: string}
function M:update_editor(text, opts) end

--- Register a named widget rendered above or below the editor; withdrawn when the caller is disposed.
---@param widget_id string
---@param def {slot?: "above-editor"|"below-editor", order?: integer, render: fun(width: integer, max_height: integer): mare.Node}
---@return table
function M:widget(widget_id, def) end

--- Return the current frontend viewport width in columns.
---@return integer
function M:width() end

--- Set or clear the active working indicator (`label = nil` clears it).
---@param label? string|nil
---@param detail? string
---@param opts? {background?: boolean}
---@return table
function M:working(label, detail, opts) end

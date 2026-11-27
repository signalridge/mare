---@meta _
-- LuaLS definitions: key bindings (entry: types/mare.lua)

---@alias mare.KeyAction "interrupt"|"transcript"|"quit-if-empty"|"quit"|"redraw"|"suspend"|"steer"|"history-search"|"external-editor"|"paste-image"|"follow-up"|"complete"|"newline"|"editor.submit"|"input.accept"|"input.cancel"|string|fun()|false
--- A keymap context: "global", "text", "editor", "editor.completion", "editor.queue",
--- "editor.history", "editor.selection", "input", "transcript", "transcript.search", "select",
--- "select.vim", "select.search", "confirm", "help", "info", or a plugin-declared subcontext
--- (`mare.keymap.context`).
---@alias mare.KeyContext "global"|"text"|"editor"|"editor.completion"|"editor.queue"|"editor.history"|"editor.selection"|"input"|"transcript"|"transcript.search"|"select"|"select.vim"|"select.search"|"confirm"|"help"|"info"|string
--- Predicates the core evaluates (no Lua per key); "!x" negates, a list means all of them.
---@alias mare.KeyWhen "editor_empty"|"completion_open"|"paste_burst"|"working"|string
---@class mare.KeymapResolveOpts
---@field editor_empty? boolean
---@field working? boolean
---@field completion_open? boolean
---@field paste_burst? boolean

---@class mare.KeymapResolveResult
---@field key string
---@field masked boolean
---@field candidates {context: mare.KeyContext, action: string}[]

---@class mare.KeymapSnapshot
---@field contexts {name: string, parents: string[], core: boolean, bindable: boolean, chain: string[]}[]
---@field actions {name: string, desc?: string, contexts?: string[], core: boolean, essential?: boolean, keys: string[]}[]
---@field bindings {context: string, key: string, key_label: string, action: string|false, unbound: boolean, when: string[], declines: boolean, rank: "default"|"plugin"|"user"|"project", desc?: string, active: boolean}[]
---@field conflicts {missing_essential: {action: string, context: string}[], unbound_essential: {context: string, action: string}[]}

---@class mare.KeymapListRow
---@field key string
---@field context mare.KeyContext
---@field desc string
---@field action string
---@field when? mare.KeyWhen|mare.KeyWhen[]
---@field layer? "default"|"plugin"|"user"|"project"
---@field priority? number
---@field owner? string

---@class mare.KeymapHandle
---@field key? string
---@field entry? table
---@field entries table[]
---@field records {key: string, entry: table}[]
---@field remove fun(self?)

---@class mare.keymap
mare.keymap = {}
--- Bind a key (Neovim `vim.keymap.set`). context: "editor" (or a list of contexts). key:
--- "<C-t>", "<M-CR>", "ctrl+t", ... `unique = true` raises when the key is already bound in that
--- context. `priority` (finite number, default 0) breaks ties within the same source layer
--- (`project > user > plugin > default`). `when`: the binding takes the key only while these hold,
--- otherwise the key goes on to the binding below it (then along the lookup chain). Anonymous
--- function actions require a non-empty `desc`. The handle undoes the binding (`handle:remove()` or
--- `mare.keymap.remove(handle)`).
--- `false` masks the caller's layer and all lower contexts along the chain; it accepts no `when` and ignores `unique`.
--- Services delegating on behalf of a caller may pass `owner` and `layer` (`"default"` or `"plugin"`).
---@param context mare.KeyContext|mare.KeyContext[]
---@param key string
---@param action mare.KeyAction
---@param opts? {desc?: string, unique?: boolean, priority?: number, when?: mare.KeyWhen|mare.KeyWhen[], owner?: string, layer?: "default"|"plugin"}
---@return mare.KeymapHandle
function mare.keymap.set(context, key, action, opts) end
--- Register multiple bindings in one atomic batch. Each item is either `{ context, key_or_keys,
--- action, opts? }` or `{ context?, key?, keys?, action, desc?, unique?, priority?, when? }`.
--- Flushes all changed stacks in a single host update; `handle:remove()` or
--- `mare.keymap.remove(handle)` removes the whole batch.
---@param specs table[]
---@param opts? {desc?: string, unique?: boolean, priority?: number, when?: mare.KeyWhen|mare.KeyWhen[], owner?: string, layer?: "default"|"plugin"}
---@return mare.KeymapHandle
function mare.keymap.set_many(specs, opts) end
--- Remove the topmost binding of `key` in `context` in the caller's layer (or `opts.owner` / `opts.layer`);
--- returns whether a binding was removed and the normalized key sequence.
---@param context mare.KeyContext
---@param key string
---@param opts? {owner?: string, layer?: "default"|"plugin"}
---@return boolean removed, string key
function mare.keymap.del(context, key, opts) end
--- Declare a plugin subcontext (`name` must be `parent .. "." .. seg` with `seg` matching
--- `^[a-z][a-z0-9_-]*$`). `handle:remove()` deletes the context, its descendants, and their bindings.
---@param name string
---@param opts {parent: mare.KeyContext, swallow_printable?: boolean, active?: boolean}
---@return {name: string, remove: fun(self?), set_active: fun(self?, active: boolean): boolean, is_active: fun(self?): boolean}
function mare.keymap.context(name, opts) end
--- Without `opts.snapshot = true`, returns every reachable unmasked binding of each key in each
--- context (optionally filtered by `opts.context`), ordered by source layer, `priority`, and binding
--- order. With `opts.snapshot = true`, returns the full keymap snapshot (`contexts`, `actions`,
--- `bindings`, `conflicts`).
---@overload fun(opts: {snapshot: true, context?: mare.KeyContext}): mare.KeymapSnapshot
---@param opts? {snapshot?: false, context?: mare.KeyContext}
---@return mare.KeymapListRow[]
function mare.keymap.list(opts) end
--- The keys that trigger `action` with focus at `context` (default "editor"), ordered by source
--- layer (`project > user > plugin > default`), then `priority`, then binding order.
---@param action string
---@param context? mare.KeyContext
---@return string[]
function mare.keymap.hint(action, context) end
--- Registered core and Lua actions (`core = true` for core actions).
---@return {name: string, desc?: string, contexts?: string[], core: boolean, essential?: boolean, declines?: boolean}[]
function mare.keymap.actions() end
--- A named Lua action, bound by name: `mare.keymap.set("editor", "<M-c>", "copy.last")`.
--- Built-in ones: "copy.last" (copy), "agents" (/agents; bound to ← with `when = "editor_empty"`),
--- "agents.back" / "agents.prev" / "agents.next" (Esc, Alt+←/→ in a sub-agent's view; they
--- decline in views the agents plugin didn't open).
--- `fn(ev)` runs as the key is handled, up to its first yield: returning exactly `false` declines
--- (the key goes on to the binding below, then the editor), anything else takes the key; decide
--- before waiting on anything. An error takes the key and shows a notice. The handle removes it
--- (`ctx.ui:action` does that when the plugin is disposed).
---@param name string
---@param fn fun(ev: {key?: string, context?: mare.KeyContext}): boolean?
---@param opts? {desc?: string, contexts?: mare.KeyContext[], priority?: number, owner?: string, layer?: "default"|"plugin", keys?: table<mare.KeyContext, string|{key: string, when?: string|string[], priority?: number}|(string|{key: string, when?: string|string[], priority?: number}|{[1]: string, [2]?: string|string[]})[]>}
---@return {name: string, remove: fun()}
function mare.keymap.action(name, fn, opts) end
--- Resolve a key or key sequence along `context`'s lookup chain under optional predicate flags.
---@param context mare.KeyContext
---@param seq string
---@param opts? mare.KeymapResolveOpts
---@return mare.KeymapResolveResult
function mare.keymap.resolve(context, seq, opts) end
--- Run a registered Lua action (named or `fn:N`). Returns `true` if handled, `false` if declined,
--- or `nil` if no such Lua action is registered.
---@param action string
---@param opts? {key?: string, context?: mare.KeyContext}
---@return boolean?
function mare.keymap.invoke(action, opts) end
--- Whether `key` (or any token in a key sequence) is reserved by the core.
---@param key string
---@return boolean
function mare.keymap.is_reserved(key) end

--- Remove every binding registered by a handle returned from `set` or `set_many`.
---@param handle mare.KeymapHandle
function mare.keymap.remove(handle) end
--- The Lua function bound under an action id (from `set`), or nil.
---@param id string
---@return function?
function mare.keymap.callback(id) end

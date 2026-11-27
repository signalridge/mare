# Key bindings

Bind and remove keys with mare.keymap: key notation, contexts and conditions, the keys shorthand, Lua actions and the default bindings.

## Bind a key

`mare.keymap.set(context, key, action, opts)` binds `key` in `context` to an action. The action is the name of a built-in action, the name of a Lua action, or a Lua function.

```lua
-- ~/.config/mare/init.lua
mare.keymap.set("editor", "<C-o>", "transcript", { desc = "open the transcript" })

mare.keymap.set("editor", "<M-n>", function()
  ui.notify("hello from a key")
end, { desc = "say hello" })
```

`opts` accepts:

| Field | Meaning |
|---|---|
| `desc` | Text shown in `/hotkeys` and the `?` help. |
| `unique` | When `true`, raise an error if the key is already bound in that context. Ignored for `false`. |
| `when` | A predicate or a list of predicates. The binding takes the key only while all hold. An unbinding (`false`) cannot have a condition. |

`set` returns a handle. `handle:remove()` or `mare.keymap.remove(handle)` undoes that binding.

To register several bindings atomically in one host update, use `mare.keymap.set_many(specs, opts?)`. Each entry is either `{ context, key_or_keys, action, opts? }` or `{ context?, key?, keys?, action, desc?, unique?, priority?, when? }`; `opts` supplies defaults shared across the batch. Validation runs before any binding is applied, and the returned handle uses the same `handle:remove()` / `mare.keymap.remove(handle)` interface to remove the whole batch.

```lua
local batch = mare.keymap.set_many({
  { "editor", "<F7>", "redraw" },
  { "editor", "<F8>", "suspend", { when = "editor_empty" } },
  { "text", { "<M-b>", "<M-Left>" }, "text.word_left" },
})
```

Bindings stack per key across four layers (`project > user > plugin > default`), regardless of load order: built-in defaults sit at `default`, third-party plugins at `plugin`, `~/.config/mare/init.lua` at `user`, and trusted `.mare/init.lua` at `project`. Within a layer, any mask (`false`) outranks every non-mask regardless of `priority` (for example, a priority-`0` mask beats a priority-`10` action in either registration order); non-mask bindings order by `priority` (default `0`) and then registration order. Removing a binding brings the one below back, and the editor's own handling sits at the bottom.

## Key notation

These spellings are all the same key, and `<C-t>`, `C-t`, `Ctrl+T` and `ctrl+t` normalise to `ctrl+t`:

| You write | Meaning |
|---|---|
| `<C-t>`, `ctrl+t`, `C-t` | Ctrl+T |
| `<M-CR>`, `alt+enter` | Alt+Enter |
| `<S-Tab>`, `shift+tab` | Shift+Tab |
| `<Esc>`, `<Space>`, `<Up>`, `<F5>` | Escape, Space, Up, F5 |

Modifiers: `ctrl` (`c`, `control`), `alt` (`m`, `meta`, `opt`, `option`), `shift` (`s`), `super` (`cmd`, `command`, `win`). Named keys: `esc`, `enter` (`return`, `cr`), `tab`, `backspace` (`bs`), `delete` (`del`), `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup` (`pgup`), `pagedown` (`pgdn`), `space`, `f1` to `f12`. A single character is itself.

A key sequence is several keys in one string: `"<C-x>g"`, `"<C-x> g"` and `"ctrl+x g"` are the same. After the first key mare shows the continuations. Sequences work in `editor`, `transcript`, and their subcontexts.

```lua
mare.keymap.set("editor", "<C-x>t", "transcript", { desc = "transcript (chord)" })
```

Which keys reach mare at all depends on your terminal. `mare doctor` explains known conflicts such as Alt+Enter in Windows Terminal, and the `key_conflicts` and `terminal_caps` options in [Options](options.md) extend what mare knows.

## Contexts

Keys are resolved along a context lookup chain determined by focus and state:

| Context | Lookup chain | Where it applies |
|---|---|---|
| `global` | `global` | Shared non-modal fallback for `editor` and `transcript`. |
| `text` | `text` | Line-editing context shared by text inputs (`editor`, `input`). |
| `editor` | `editor` → `text` → `global` | The prompt input. |
| `editor.completion`, `editor.queue`, `editor.history`, `editor.selection` | subcontext → `editor` → `text` → `global` | Active while completion is open, queue selection/editing is active, history is being browsed, or mouse selection is active. |
| `input` | `input` → `text` | The `mare.ui.input` text dialog (falls back to `text` for line-editing keys). |
| `transcript` | `transcript` → `global` | The transcript overlay (`Ctrl+T`) and views that plugins open, such as a sub-agent's view. |
| `transcript.search` | `transcript.search` → `transcript` → `global` | Active while typing a `/` search in the transcript (printable keys append to the query before `transcript` and `global`). |
| `select` | `select` | Selection dialogs (`mare.ui.select`); sequences are allowed. |
| `select.vim` | `select.vim` → `select` | Active in a selection dialog opened with `vim = true` while not searching. |
| `select.search` | `select.search` → `select` | Active while typing a `/` search filter in a selection dialog (printable keys append to the filter before `select`). |
| `confirm` | `confirm` | Confirmation dialogs (`mare.ui.confirm`). |
| `help` | `help` | Key help modals (`?`). |
| `info` | `info` | Scrollable info overlays (`mare.ui.overlay`). |

Printable rule: when a chain includes `text` and the key is a printable character without `Ctrl` or `Alt`, lookup stops after `text` so `global` bindings never steal typed text.

`context` may also be a list: `mare.keymap.set({ "editor", "transcript" }, "<F2>", "redraw")`. Plugins can declare subcontexts with `mare.keymap.context(name, { parent })` (where `name` is `parent .. "." .. seg` with `seg` matching `^[a-z][a-z0-9_-]*$`); `handle:remove()` removes the subcontext, its descendants, and all bindings in them. Any unknown context name is an error. The install screen and startup error screen do not use the keymap.

## Reserved key

`Ctrl+C` is reserved by the core and cannot be bound or unbound. It acts on the topmost focus:

| Focus | Effect |
|---|---|
| Modal | Close it with reason `interrupted`; Esc and other ordinary dismissals give `cancelled`. |
| Pending prefix or completion | Cancel the prefix or close the completion menu. |
| Transcript or plugin view | Cancel a search being typed; otherwise close the view. |
| Input | Copy a selection, clear a draft, interrupt work, or exit on a second press. |

## Conditions

`when` lets one key do different things. The predicates are fixed; plugins cannot add more. Prefix a name with `!` to negate it.

| Predicate | True when |
|---|---|
| `editor_empty` | The input has no text. |
| `completion_open` | The completion menu is open. |
| `paste_burst` | A fast run of keys is being treated as a paste. |
| `working` | The agent is running. |

```lua
-- F6 queues a follow-up, but only while the agent is busy
mare.keymap.set("editor", "<F6>", "follow-up", { when = "working" })

-- "?" opens the key help only on an empty input
mare.keymap.set("editor", "?", "keymap-help", { when = { "editor_empty", "!completion_open" } })
```

## Actions

A string action is looked up in this order: a core action, then a named Lua action. An unregistered action passes the key to the next binding or the editor. Core actions declare the contexts they support; binding a core action in an unsupported context is an error. `mare.keymap.actions()` lists registered core and Lua actions.

Core actions in the `editor` context:

| Action | Does |
|---|---|
| `interrupt` | Close completion, or stop the running work. Twice when idle: the rewind picker. |
| `quit-if-empty` | Quit when the input is empty. |
| `quit` | Quit. |
| `transcript` | Open the transcript overlay. |
| `toggle-expand` | Switch tool output between compact and expanded. |
| `steer` | Send the input into the running turn. |
| `follow-up` | Queue the input to run after the current turn. |
| `history-search` | Search the input history. |
| `external-editor` | Edit the input in your editor. |
| `paste-image` | Paste an image from the clipboard. |
| `complete` | Complete `@file` or `/command`. |
| `newline` | Insert a newline. |
| `redraw` | Redraw the screen. |
| `suspend` | Suspend mare (not on Windows). |
| `select-mode` | Select text in the live area with the mouse. |
| `keymap-help` | Open the searchable list of all bindings. |
| `editor.submit` | Send the input (insert a newline when preceded by `\`). |

In the `text` context (shared by `editor` and `input`), the line-editing actions are `text.newline`, `text.left`, `text.right`, `text.word_left`, `text.word_right`, `text.line_start`, `text.line_end`, `text.up`, `text.down`, `text.delete_back`, `text.delete_word_back`, `text.delete_forward`, `text.delete_word_forward`, `text.kill_line_start`, `text.kill_line_end`, `text.yank`, `text.yank_pop`, and `text.undo`.

In the `input` context, the dialog actions are `input.accept` (confirm the dialog text) and `input.cancel` (dismiss with `cancelled`).

In the `transcript` context the actions are `view.close`, `view.back`, `view.up`, `view.down`, `view.page_up`, `view.page_down`, `view.top`, `view.bottom`, `view.search`, `view.copy`, `view.next_match` and `view.prev_match`.

In modal contexts the actions and their default keys (registered by `keymap-default`) are:
- `select` (`select.vim` adds `k`/`j`/`g`/`<S-g>`/`l`/`h`): `select.up` (`<Up>`), `select.down` (`<Down>`), `select.page_up` (`<PageUp>`), `select.page_down` (`<PageDown>`), `select.top` (`<Home>`), `select.bottom` (`<End>`), `select.accept` (`<CR>`), `select.back` (`<Esc>`), `select.search` (`/`), `select.tab_next` (`<Tab>`, `<Right>`, `]`), `select.tab_prev` (`<S-Tab>`, `<Left>`, `[`), `select.help` (`?`), `select.preview_up`/`down`/`page_up`/`page_down`/`top`/`bottom` (`<S-Up>`/`<S-Down>`/`<S-PageUp>`/`<S-PageDown>`/`<S-Home>`/`<S-End>`), `select.body_up`/`down`/`page_up`/`page_down`/`top`/`bottom` (`<C-Up>`/`<C-Down>`/`<C-PageUp>`/`<C-PageDown>`/`<C-Home>`/`<C-End>`).
- `confirm`: `confirm.yes` (`y`, `<S-y>`), `confirm.no` (`n`, `<S-n>`), `confirm.toggle` (`<Left>`, `<Right>`, `<Tab>`, `<S-Tab>`, `h`, `l`), `confirm.accept` (`<CR>`), `confirm.cancel` (`<Esc>`, `q`), `confirm.scroll_up`/`scroll_down`/`page_up`/`page_down`/`top`/`bottom` (`<Up>`/`k`, `<Down>`/`j`, `<PageUp>`, `<PageDown>`, `<Home>`, `<End>`).
- `help`: `help.up` (`<Up>`), `help.down` (`<Down>`), `help.accept` (`<CR>`), `help.cancel` (`<Esc>`).
- `info`: `info.up` (`<Up>`, `k`), `info.down` (`<Down>`, `j`), `info.page_up` (`<PageUp>`), `info.page_down` (`<PageDown>`, `<Space>`), `info.top` (`<Home>`), `info.bottom` (`<End>`).

Plugins add more. The built-in `copy` plugin defines `copy.last` with no default key; the `agents` plugin defines `agents`, `agents.back`, `agents.prev` and `agents.next`.

```lua
mare.keymap.set("editor", "<M-c>", "copy.last", { desc = "copy the last response" })
```

### Lua actions

`mare.keymap.action(name, fn, opts)` registers a named action, so users can rebind it by name and hints can find its keys. `fn(ev)` receives `{ key, context }`. It runs at once, up to its first yield, so decide before waiting on anything. Returning exactly `false` declines: the key goes on to the binding below, then along the chain to the editor. Any other result takes the key.

`opts.keys` declares default keys by context: a key string, a record such as `{ key = "<F5>", when = "working" }`, or a list of strings and records. A single record needs a string `key`; an empty table declares no keys. Removing the action also removes its default bindings.

```lua
mare.keymap.action("greet", function(ev)
  if ev.context ~= "editor" then return false end   -- decline: the key falls through
  ui.notify("pressed " .. ev.key)
end, { desc = "show the key that ran this action" })

mare.keymap.set("editor", "<M-d>", "greet")
```

A function you pass directly to `set` behaves the same way.

## The keys shorthand

`mare.setup { keys = { ... } }` binds editor keys from a table. A value of `false` or `"none"` masks bindings registered earlier at the plugin layer (such as `keymap-default` or `commands`); to mask a plugin loaded later, use `mare.keymap.set` in `init.lua`.

```lua
mare.setup {
  keys = {
    ["<C-o>"] = "transcript",
    ["<M-p>"] = false,          -- free Alt+P (the model picker's default key)
  },
}
```

Writing one key two ways with different actions (`"<C-o>"` and `"ctrl+o"`) is an error and neither is bound. Use `mare.keymap.set` when you need a context other than `editor`, `when`, or a function.

## Remove and inspect bindings

| Call | Does |
|---|---|
| `mare.keymap.set(context, key, false)` | Mask bindings for the key in this context at the caller's layer and all contexts after it in the chain. Remove the returned handle to lift the mask. |
| `mare.keymap.del(context, key)` | Remove the newest binding of `key` in `context` in the caller's own layer. The one below comes back. |
| `mare.keymap.context(name, { parent })` | Declare a plugin subcontext under `parent`; `handle:remove()` deletes it, its descendants, and their bindings. |
| `mare.keymap.remove(handle)` | Remove the binding a `set` call returned. |
| `mare.keymap.list([opts])` | All reachable, unmasked bindings (multiple rows per context and key are possible, as with the two `Ctrl+R` entries below), as `{ key, context, desc, action, when }`. Supports `list({ context = "editor" })` and `list({ snapshot = true })`. |
| `mare.keymap.hint(action, context)` | The keys that can trigger `action` with focus at `context` (`project > user > plugin > default`, then `priority`, then binding order). |
| `mare.keymap.actions()` | Registered core and Lua actions (`{ name, desc, contexts, core, essential }`). |

```lua
mare.keymap.set("editor", "<C-z>", false) -- disable suspend
```

`/hotkeys` shows the same list in the TUI. Press `?` on an empty input for a searchable list. During `/reload config` and `/reload`, config reevaluation commits staged `init.lua` bindings first and then opens the plugin keymap reload guard around plugin reloads: if `init.lua` fails to evaluate, the previous config bindings stay intact; if a later plugin reload fails or is cancelled, the plugin guard handles plugin keymaps while already-committed config bindings can remain active.

If an essential action (`editor.submit`, `input.accept`, `input.cancel`) is left without any key (for example with `keymap-default` disabled or an explicit unbind), `/hotkeys` lists it under `No key` and `mare doctor` prints a warning instead of restoring a default key silently.

## Default bindings

Core action defaults come from the built-in `keymap-default` plugin, and plugin actions (`review.viewer`, `agents`, `model.picker`, `model.cycle`, `effort.down`, `effort.up`, …) register their own `default`-layer keys. `mare.plugin("keymap-default", { enabled = false })` disables the core set; bindings registered by other plugins remain, and the reserved `Ctrl+C` keeps working.

Rebind or disable any default key in `init.lua` with `mare.keymap.set`:

```lua
mare.keymap.set("editor", "<M-o>", "model.picker")
mare.keymap.set("editor", "<M-m>", false) -- unbind model.cycle
mare.keymap.set("editor", "<C-y>", "review.viewer")
mare.keymap.set("editor", "<M-a>", "agents", { when = "editor_empty" })
```

<!-- BEGIN GENERATED KEYMAPS (scripts/gen_docs.py) -->
| Context | Key | Action | When | Description |
|---|---|---|---|---|
| `editor` | `?` | `keymap-help` | `editor_empty`, `!completion_open`, `!paste_burst` | all key bindings (on an empty input) |
| `editor` | `Alt+,` | `effort.down` |  | lower reasoning effort (the model's levels) |
| `editor` | `Alt+.` | `effort.up` |  | raise reasoning effort (the model's levels) |
| `editor` | `Alt+Enter` | `follow-up` | `working` | queue the input as a follow-up |
| `editor` | `Alt+M` | `model.cycle` |  | next favorite / recent model |
| `editor` | `Alt+P` | `model.picker` |  | select model (keeps the input) |
| `editor` | `Alt+S` | `select-mode` |  | select text in the live area with the mouse, release copies |
| `editor` | `Alt+V` | `paste-image` |  | paste an image from the clipboard (Windows) |
| `editor` | `Ctrl+B` | `background` |  | move the running command to the background |
| `editor` | `Ctrl+D` | `quit-if-empty` |  | exit when the input is empty |
| `editor` | `Ctrl+G` | `external-editor` |  | edit the input in $EDITOR |
| `editor` | `Ctrl+L` | `redraw` |  | redraw the screen |
| `editor` | `Ctrl+O` | `toggle-expand` |  | toggle compact / expanded tool output and thinking |
| `editor` | `Ctrl+Q` | `follow-up` |  | queue the input as a follow-up (Windows) |
| `editor` | `Ctrl+R` | `history-search` |  | search input history |
| `editor` | `Ctrl+R` | `review.viewer` |  | review changes: mare's edits per turn, or git unstaged / staged / last commit |
| `editor` | `Ctrl+S` | `steer` |  | send the input into the running turn |
| `editor` | `Ctrl+T` | `transcript` |  | expanded transcript |
| `editor` | `Ctrl+V` | `paste-image` |  | paste an image from the clipboard (Unix / macOS) |
| `editor` | `Ctrl+Z` | `suspend` |  | suspend mare (resume with fg) |
| `editor` | `Enter` | `editor.submit` |  | send the input |
| `editor` | `Esc` | `interrupt` |  | interrupt the agent · twice when idle: rewind picker (/undo) |
| `editor` | `←` | `agents` | `editor_empty` | sub-agents and other sessions (/agents) |
| `editor` | `Shift+Tab` | `permissions.cycle` | `!completion_open` | cycle permission mode |
| `editor` | `Tab` | `complete` |  | complete @file or /command |
| `text` | `Alt+B` | `text.word_left` |  | one word left |
| `text` | `Alt+Backspace` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Alt+D` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Alt+delete` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Alt+Enter` | `text.newline` |  | new line |
| `text` | `Alt+F` | `text.word_right` |  | one word right |
| `text` | `Alt+←` | `text.word_left` |  | one word left |
| `text` | `Alt+→` | `text.word_right` |  | one word right |
| `text` | `Alt+Shift+Enter` | `text.newline` |  | new line |
| `text` | `Alt+Shift+←` | `text.select_word_left` |  | extend selection one word left |
| `text` | `Alt+Shift+→` | `text.select_word_right` |  | extend selection one word right |
| `text` | `Alt+Shift+Z` | `text.redo` |  | redo the last undone edit |
| `text` | `Alt+Y` | `text.yank_pop` |  | cycle older deletions |
| `text` | `Backspace` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Ctrl+_` | `text.undo` |  | undo the last edit |
| `text` | `Ctrl+A` | `text.line_start` |  | start of line |
| `text` | `Ctrl+B` | `text.left` |  | one character left |
| `text` | `Ctrl+Backspace` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Ctrl+D` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `Ctrl+delete` | `text.delete_word_forward` |  | delete the word after the cursor |
| `text` | `Ctrl+E` | `text.line_end` |  | end of line |
| `text` | `Ctrl+End` | `text.line_end` |  | end of line |
| `text` | `Ctrl+F` | `text.right` |  | one character right |
| `text` | `Ctrl+H` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Ctrl+Home` | `text.line_start` |  | start of line |
| `text` | `Ctrl+J` | `text.newline` |  | new line |
| `text` | `Ctrl+K` | `text.kill_line_end` |  | delete to the end of the line |
| `text` | `Ctrl+←` | `text.word_left` |  | one word left |
| `text` | `Ctrl+N` | `text.down` |  | line down |
| `text` | `Ctrl+P` | `text.up` |  | line up |
| `text` | `Ctrl+→` | `text.word_right` |  | one word right |
| `text` | `Ctrl+Shift+←` | `text.select_word_left` |  | extend selection one word left |
| `text` | `Ctrl+Shift+→` | `text.select_word_right` |  | extend selection one word right |
| `text` | `Ctrl+Shift+Z` | `text.redo` |  | redo the last undone edit |
| `text` | `Ctrl+U` | `text.kill_line_start` |  | delete to the start of the line |
| `text` | `Ctrl+W` | `text.delete_word_back` |  | delete the word before the cursor |
| `text` | `Ctrl+Y` | `text.yank` |  | paste the last deletion |
| `text` | `Ctrl+Z` | `text.undo` |  | undo the last edit |
| `text` | `delete` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `↓` | `text.down` |  | line down |
| `text` | `End` | `text.line_end` |  | end of line |
| `text` | `Home` | `text.line_start` |  | start of line |
| `text` | `←` | `text.left` |  | one character left |
| `text` | `→` | `text.right` |  | one character right |
| `text` | `Shift+Backspace` | `text.delete_back` |  | delete the character before the cursor |
| `text` | `Shift+delete` | `text.delete_forward` |  | delete the character after the cursor |
| `text` | `Shift+↓` | `text.select_down` |  | extend selection one line down |
| `text` | `Shift+End` | `text.select_line_end` |  | extend selection to end of line |
| `text` | `Shift+Enter` | `text.newline` |  | new line |
| `text` | `Shift+Home` | `text.select_line_start` |  | extend selection to start of line |
| `text` | `Shift+←` | `text.select_left` |  | extend selection one character left |
| `text` | `Shift+→` | `text.select_right` |  | extend selection one character right |
| `text` | `Shift+↑` | `text.select_up` |  | extend selection one line up |
| `text` | `↑` | `text.up` |  | line up |
| `input` | `Enter` | `input.accept` |  | confirm input |
| `input` | `Esc` | `input.cancel` |  | cancel input |
| `transcript` | `/` | `view.search` |  | search |
| `transcript` | `Alt+←` | `agents.prev` |  | the previous sub-agent |
| `transcript` | `Alt+→` | `agents.next` |  | the next sub-agent |
| `transcript` | `B` | `view.page_up` |  | scroll up a page |
| `transcript` | `Ctrl+T` | `view.close` |  | close the transcript |
| `transcript` | `↓` | `view.down` |  | scroll down a line |
| `transcript` | `End` | `view.bottom` |  | go to the bottom |
| `transcript` | `Esc` | `agents.back` |  | back to the agents list |
| `transcript` | `Esc` | `view.back` |  | clear the search, else close the transcript |
| `transcript` | `F` | `view.page_down` |  | scroll down a page |
| `transcript` | `G` | `view.top` |  | go to the top |
| `transcript` | `Home` | `view.top` |  | go to the top |
| `transcript` | `J` | `view.down` |  | scroll down a line |
| `transcript` | `K` | `view.up` |  | scroll up a line |
| `transcript` | `N` | `view.next_match` |  | next search match |
| `transcript` | `PageDown` | `view.page_down` |  | scroll down a page |
| `transcript` | `PageUp` | `view.page_up` |  | scroll up a page |
| `transcript` | `Q` | `view.close` |  | close the transcript |
| `transcript` | `Shift+G` | `view.bottom` |  | go to the bottom |
| `transcript` | `Shift+N` | `view.prev_match` |  | previous search match |
| `transcript` | `Space` | `view.page_down` |  | scroll down a page |
| `transcript` | `↑` | `view.up` |  | scroll up a line |
| `transcript` | `Y` | `view.copy` |  | copy the message at the reading position |
| `select` | `/` | `select.search` |  | search (fuzzy) |
| `select` | `?` | `select.help` |  | all keys |
| `select` | `[` | `select.tab_prev` |  | previous tab |
| `select` | `]` | `select.tab_next` |  | next tab |
| `select` | `Ctrl+↓` | `select.body_down` |  | scroll body down |
| `select` | `Ctrl+End` | `select.body_bottom` |  | scroll body to bottom |
| `select` | `Ctrl+Home` | `select.body_top` |  | scroll body to top |
| `select` | `Ctrl+P` | `select.toggle_preview` |  | toggle preview |
| `select` | `Ctrl+PageDown` | `select.body_page_down` |  | scroll body page down |
| `select` | `Ctrl+PageUp` | `select.body_page_up` |  | scroll body page up |
| `select` | `Ctrl+↑` | `select.body_up` |  | scroll body up |
| `select` | `↓` | `select.down` |  | next item |
| `select` | `End` | `select.bottom` |  | last item |
| `select` | `Enter` | `select.accept` |  | choose the selected item |
| `select` | `Esc` | `select.back` |  | clear search / close |
| `select` | `Home` | `select.top` |  | first item |
| `select` | `←` | `select.tab_prev` |  | previous tab |
| `select` | `PageDown` | `select.page_down` |  | page down |
| `select` | `PageUp` | `select.page_up` |  | page up |
| `select` | `→` | `select.tab_next` |  | next tab |
| `select` | `Shift+↓` | `select.preview_down` |  | scroll preview down |
| `select` | `Shift+End` | `select.preview_bottom` |  | scroll preview to bottom |
| `select` | `Shift+Home` | `select.preview_top` |  | scroll preview to top |
| `select` | `Shift+PageDown` | `select.preview_page_down` |  | scroll preview page down |
| `select` | `Shift+PageUp` | `select.preview_page_up` |  | scroll preview page up |
| `select` | `Shift+Tab` | `select.tab_prev` |  | previous tab |
| `select` | `Shift+↑` | `select.preview_up` |  | scroll preview up |
| `select` | `Space` | `select.toggle` |  | toggle selection |
| `select` | `Tab` | `select.tab_next` |  | next tab |
| `select` | `↑` | `select.up` |  | previous item |
| `select.vim` | `G` | `select.top` |  | first item |
| `select.vim` | `H` | `select.tab_prev` |  | previous tab |
| `select.vim` | `J` | `select.down` |  | next item |
| `select.vim` | `K` | `select.up` |  | previous item |
| `select.vim` | `L` | `select.tab_next` |  | next tab |
| `select.vim` | `Shift+G` | `select.bottom` |  | last item |
| `confirm` | `↓` | `confirm.scroll_down` |  | scroll down |
| `confirm` | `End` | `confirm.bottom` |  | scroll to bottom |
| `confirm` | `Enter` | `confirm.accept` |  | confirm choice |
| `confirm` | `Esc` | `confirm.cancel` |  | cancel |
| `confirm` | `H` | `confirm.toggle` |  | switch choice |
| `confirm` | `Home` | `confirm.top` |  | scroll to top |
| `confirm` | `J` | `confirm.scroll_down` |  | scroll down |
| `confirm` | `K` | `confirm.scroll_up` |  | scroll up |
| `confirm` | `L` | `confirm.toggle` |  | switch choice |
| `confirm` | `←` | `confirm.toggle` |  | switch choice |
| `confirm` | `N` | `confirm.no` |  | answer no |
| `confirm` | `PageDown` | `confirm.page_down` |  | scroll page down |
| `confirm` | `PageUp` | `confirm.page_up` |  | scroll page up |
| `confirm` | `Q` | `confirm.cancel` |  | cancel |
| `confirm` | `→` | `confirm.toggle` |  | switch choice |
| `confirm` | `Shift+N` | `confirm.no` |  | answer no |
| `confirm` | `Shift+Tab` | `confirm.toggle` |  | switch choice |
| `confirm` | `Shift+Y` | `confirm.yes` |  | answer yes |
| `confirm` | `Tab` | `confirm.toggle` |  | switch choice |
| `confirm` | `↑` | `confirm.scroll_up` |  | scroll up |
| `confirm` | `Y` | `confirm.yes` |  | answer yes |
| `help` | `↓` | `help.down` |  | next item |
| `help` | `Enter` | `help.accept` |  | run selected key |
| `help` | `Esc` | `help.cancel` |  | close |
| `help` | `↑` | `help.up` |  | previous item |
| `info` | `↓` | `info.down` |  | scroll down |
| `info` | `End` | `info.bottom` |  | scroll to bottom |
| `info` | `Esc` | `info.close` |  | close overlay |
| `info` | `Home` | `info.top` |  | scroll to top |
| `info` | `J` | `info.down` |  | scroll down |
| `info` | `K` | `info.up` |  | scroll up |
| `info` | `PageDown` | `info.page_down` |  | scroll page down |
| `info` | `PageUp` | `info.page_up` |  | scroll page up |
| `info` | `Space` | `info.page_down` |  | scroll page down |
| `info` | `↑` | `info.up` |  | scroll up |
<!-- END GENERATED KEYMAPS -->

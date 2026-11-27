# 键位映射

用 mare.keymap 绑定和移除按键：按键写法、上下文和条件、keys 简写、Lua 动作以及默认键位。

## 绑定按键

`mare.keymap.set(context, key, action, opts)` 在 `context` 中把 `key` 绑定到一个动作。动作可以是内置动作的名称、Lua 动作的名称，或一个 Lua 函数。

```lua
-- ~/.config/mare/init.lua
mare.keymap.set("editor", "<C-o>", "transcript", { desc = "open the transcript" })

mare.keymap.set("editor", "<M-n>", function()
  ui.notify("hello from a key")
end, { desc = "say hello" })
```

`opts` 接受：

| 字段 | 含义 |
|---|---|
| `desc` | 显示在 `/hotkeys` 和 `?` 帮助中的文本。 |
| `unique` | 为 `true` 时，如果该按键在该上下文中已被绑定，则抛出错误。解绑（`false`）时忽略此项。 |
| `when` | 一个谓词或谓词列表。只有全部成立时，这个绑定才会接管按键。解绑（`false`）不能带条件。 |

`set` 返回一个句柄。`handle:remove()` 或 `mare.keymap.remove(handle)` 撤销该绑定。

要在一个宿主更新中原子地批量注册多条绑定，请使用 `mare.keymap.set_many(specs, opts?)`。每条记录可以是 `{ context, key_or_keys, action, opts? }` 或 `{ context?, key?, keys?, action, desc?, unique?, priority?, when? }`，`opts` 为整批提供默认选项。所有条目会先完整校验再提交，返回的句柄与 `set` 形状一致，可通过 `handle:remove()` 或 `mare.keymap.remove(handle)` 一次撤销整批绑定。

```lua
local batch = mare.keymap.set_many({
  { "editor", "<F7>", "redraw" },
  { "editor", "<F8>", "suspend", { when = "editor_empty" } },
  { "text", { "<M-b>", "<M-Left>" }, "text.word_left" },
})
```

绑定按四个层级叠加（`project > user > plugin > default`），与加载顺序无关：内置默认键位在 `default` 层，第三方插件在 `plugin` 层，`~/.config/mare/init.lua` 在 `user` 层，受信任的 `.mare/init.lua` 在 `project` 层。同一层内，屏蔽项（`false`）无论 `priority` 高低都优先于普通绑定（例如 `priority = 0` 的屏蔽项无论先后注册都会覆盖同层 `priority = 10` 的动作）；普通绑定之间再按 `priority`（默认 `0`）和登记先后排序。移除顶层绑定会让下面那条恢复，编辑器自身的处理位于最底层。

## 按键写法

这些写法都是同一个按键，`<C-t>`、`C-t`、`Ctrl+T` 和 `ctrl+t` 都会规范化为 `ctrl+t`：

| 你写的 | 含义 |
|---|---|
| `<C-t>`、`ctrl+t`、`C-t` | Ctrl+T |
| `<M-CR>`、`alt+enter` | Alt+Enter |
| `<S-Tab>`、`shift+tab` | Shift+Tab |
| `<Esc>`、`<Space>`、`<Up>`、`<F5>` | Escape、Space、Up、F5 |

修饰键：`ctrl`（`c`、`control`）、`alt`（`m`、`meta`、`opt`、`option`）、`shift`（`s`）、`super`（`cmd`、`command`、`win`）。具名按键：`esc`、`enter`（`return`、`cr`）、`tab`、`backspace`（`bs`）、`delete`（`del`）、`insert`、`up`、`down`、`left`、`right`、`home`、`end`、`pageup`（`pgup`）、`pagedown`（`pgdn`）、`space`、`f1` 到 `f12`。单个字符就是它本身。

按键序列是一个字符串里的多个按键：`"<C-x>g"`、`"<C-x> g"` 和 `"ctrl+x g"` 是一样的。按下第一个键后，mare 会显示后续可按的键。序列在 `editor`、`transcript` 及其子上下文中有效。

```lua
mare.keymap.set("editor", "<C-x>t", "transcript", { desc = "transcript (chord)" })
```

哪些按键能到达 mare，取决于你的终端。`mare doctor` 会说明已知的冲突，例如 Windows Terminal 中的 Alt+Enter；[选项](options.md)中的 `key_conflicts` 和 `terminal_caps` 选项可以扩展 mare 已知的信息。

## 上下文

按键按焦点和状态决定的查找链解析：

| 上下文 | 查找顺序 | 适用范围 |
|---|---|---|
| `global` | `global` | `editor` 和 `transcript` 共用的非模态兜底上下文。 |
| `text` | `text` | 文本输入框（`editor`、`input`）共用的行编辑上下文。 |
| `editor` | `editor` → `text` → `global` | 提示词输入框。 |
| `editor.completion`、`editor.queue`、`editor.history`、`editor.selection` | 子上下文 → `editor` → `text` → `global` | 分别在补全弹窗打开、队列选择/编辑、浏览历史或鼠标选区激活时生效。 |
| `input` | `input` → `text` | `mare.ui.input` 文本输入弹窗（行编辑按键回落到 `text`）。 |
| `transcript` | `transcript` → `global` | 对话记录浮层（`Ctrl+T`）以及插件打开的视图，例如子智能体的视图。 |
| `transcript.search` | `transcript.search` → `transcript` → `global` | 在记录视图中输入 `/` 搜索词时生效（可打印字符先追加进搜索词，再查 `transcript` 和 `global`）。 |
| `select` | `select` | 选择弹窗（`mare.ui.select`），支持按键序列。 |
| `select.vim` | `select.vim` → `select` | 在 `vim = true` 的选择弹窗且未输入搜索时生效。 |
| `select.search` | `select.search` → `select` | 在选择弹窗中输入 `/` 搜索词时生效（可打印字符先追加进过滤词，再查 `select`）。 |
| `confirm` | `confirm` | 确认弹窗（`mare.ui.confirm`）。 |
| `help` | `help` | 按键帮助浮层（`?`）。 |
| `info` | `info` | 可滚动信息浮层（`mare.ui.overlay`）。 |

可打印字符规则：当查找链包含 `text` 且按键是不带 `Ctrl`/`Alt` 的可打印字符时，查找在 `text` 之后停止，这样 `global` 里的绑定不会抢走打字输入。

`context` 也可以是列表：`mare.keymap.set({ "editor", "transcript" }, "<F2>", "redraw")`。插件可以用 `mare.keymap.context(name, { parent })` 声明子上下文（`name` 为 `parent .. "." .. seg`，其中 `seg` 匹配 `^[a-z][a-z0-9_-]*$`）；`handle:remove()` 会连同后代子上下文及其绑定一起删除。任何未知的上下文名都是错误。安装画面和启动错误画面不走 keymap。

## 保留键

`Ctrl+C` 由核心保留，不能绑定或解绑。它只作用于最上层焦点：

| 焦点 | 作用 |
|---|---|
| 模态框 | 关闭并返回原因 `interrupted`；Esc 等普通取消返回 `cancelled`。 |
| 等待中的前缀或补全 | 取消前缀或关闭补全菜单。 |
| 对话记录或插件视图 | 取消正在输入的搜索；否则关闭视图。 |
| 输入框 | 复制选区、清除草稿、中断工作，或在第二次按下时退出。 |

## 条件

`when` 让同一个按键做不同的事。谓词是固定的；插件不能添加更多。在名称前加 `!` 表示取反。

| 谓词 | 成立条件 |
|---|---|
| `editor_empty` | 输入框中没有文本。 |
| `completion_open` | 补全菜单已打开。 |
| `paste_burst` | 一连串快速的按键被当作粘贴处理。 |
| `working` | 智能体正在运行。 |

```lua
-- F6 queues a follow-up, but only while the agent is busy
mare.keymap.set("editor", "<F6>", "follow-up", { when = "working" })

-- "?" opens the key help only on an empty input
mare.keymap.set("editor", "?", "keymap-help", { when = { "editor_empty", "!completion_open" } })
```

## 动作

字符串动作按以下顺序查找：先是核心动作，然后是具名的 Lua 动作。未注册的动作会将按键交给下一条绑定或编辑器。核心动作声明了自己支持的上下文；把核心动作绑定到不支持的上下文属于错误。`mare.keymap.actions()` 列出已登记的核心和 Lua 动作。

`editor` 上下文下的核心动作：

| 动作 | 作用 |
|---|---|
| `interrupt` | 关闭补全，或停止正在运行的工作。空闲时连按两次：打开回退选择器。 |
| `quit-if-empty` | 输入为空时退出。 |
| `quit` | 退出。 |
| `transcript` | 打开对话记录浮层。 |
| `toggle-expand` | 在紧凑和展开的工具输出之间切换。 |
| `steer` | 把输入送入正在运行的这一轮。 |
| `follow-up` | 把输入排队，在当前一轮之后运行。 |
| `history-search` | 搜索输入历史。 |
| `external-editor` | 在你的编辑器中编辑输入。 |
| `paste-image` | 粘贴剪贴板中的图片。 |
| `complete` | 补全 `@file` 或 `/command`。 |
| `newline` | 插入换行。 |
| `redraw` | 重绘屏幕。 |
| `suspend` | 挂起 mare（Windows 上不可用）。 |
| `select-mode` | 用鼠标选择动态区域中的文本。 |
| `keymap-help` | 打开所有绑定的可搜索列表。 |
| `editor.submit` | 发送输入（前有 `\` 时插入换行）。 |

在 `text` 上下文下（由 `editor` 和 `input` 共用），行编辑动作有 `text.newline`、`text.left`、`text.right`、`text.word_left`、`text.word_right`、`text.line_start`、`text.line_end`、`text.up`、`text.down`、`text.delete_back`、`text.delete_word_back`、`text.delete_forward`、`text.delete_word_forward`、`text.kill_line_start`、`text.kill_line_end`、`text.yank`、`text.yank_pop` 和 `text.undo`。

在 `input` 上下文下，对话框动作有 `input.accept`（确认输入文本）和 `input.cancel`（以 `cancelled` 取消）。

在 `transcript` 上下文下，动作有 `view.close`、`view.back`、`view.up`、`view.down`、`view.page_up`、`view.page_down`、`view.top`、`view.bottom`、`view.search`、`view.copy`、`view.next_match` 和 `view.prev_match`。

模态上下文的动作及默认按键（由 `keymap-default` 注册）：
- `select`（`select.vim` 补充 `k`/`j`/`g`/`<S-g>`/`l`/`h`）：`select.up`（`<Up>`）、`select.down`（`<Down>`）、`select.page_up`（`<PageUp>`）、`select.page_down`（`<PageDown>`）、`select.top`（`<Home>`）、`select.bottom`（`<End>`）、`select.accept`（`<CR>`）、`select.back`（`<Esc>`）、`select.search`（`/`）、`select.tab_next`（`<Tab>`、`<Right>`、`]`）、`select.tab_prev`（`<S-Tab>`、`<Left>`、`[`）、`select.help`（`?`）、`select.preview_up`/`down`/`page_up`/`page_down`/`top`/`bottom`（`<S-Up>`/`<S-Down>`/`<S-PageUp>`/`<S-PageDown>`/`<S-Home>`/`<S-End>`）、`select.body_up`/`down`/`page_up`/`page_down`/`top`/`bottom`（`<C-Up>`/`<C-Down>`/`<C-PageUp>`/`<C-PageDown>`/`<C-Home>`/`<C-End>`）。
- `confirm`：`confirm.yes`（`y`、`<S-y>`）、`confirm.no`（`n`、`<S-n>`）、`confirm.toggle`（`<Left>`、`<Right>`、`<Tab>`、`<S-Tab>`、`h`、`l`）、`confirm.accept`（`<CR>`）、`confirm.cancel`（`<Esc>`、`q`）、`confirm.scroll_up`/`scroll_down`/`page_up`/`page_down`/`top`/`bottom`（`<Up>`/`k`、`<Down>`/`j`、`<PageUp>`、`<PageDown>`、`<Home>`、`<End>`）。
- `help`：`help.up`（`<Up>`）、`help.down`（`<Down>`）、`help.accept`（`<CR>`）、`help.cancel`（`<Esc>`）。
- `info`：`info.up`（`<Up>`、`k`）、`info.down`（`<Down>`、`j`）、`info.page_up`（`<PageUp>`）、`info.page_down`（`<PageDown>`、`<Space>`）、`info.top`（`<Home>`）、`info.bottom`（`<End>`）。

插件会添加更多动作。内置的 `copy` 插件定义了没有默认按键的 `copy.last`；`agents` 插件定义了 `agents`、`agents.back`、`agents.prev` 和 `agents.next`。

```lua
mare.keymap.set("editor", "<M-c>", "copy.last", { desc = "copy the last response" })
```

### Lua 动作

`mare.keymap.action(name, fn, opts)` 注册一个具名动作，这样用户可以按名称重新绑定它，提示也能找到它的按键。`fn(ev)` 接收 `{ key, context }`。它会立即运行，直到第一次 yield 为止，所以请在等待任何东西之前做出决定。恰好返回 `false` 表示放弃：按键继续传给下面的绑定和查找链，然后传给编辑器。其他任何结果都会接管按键。

`opts.keys` 按上下文声明默认按键：值可以是按键字符串、类似 `{ key = "<F5>", when = "working" }` 的记录，或字符串和记录组成的列表。单条记录必须有字符串 `key`；空表表示不设置按键。移除动作时也会移除它的默认绑定。

```lua
mare.keymap.action("greet", function(ev)
  if ev.context ~= "editor" then return false end   -- decline: the key falls through
  ui.notify("pressed " .. ev.key)
end, { desc = "show the key that ran this action" })

mare.keymap.set("editor", "<M-d>", "greet")
```

直接传给 `set` 的函数行为相同。

## keys 简写

`mare.setup { keys = { ... } }` 从一张表中绑定编辑器按键。值为 `false` 或 `"none"` 会屏蔽此前在插件层登记的绑定（例如 `keymap-default` 或 `commands`）；要屏蔽更晚加载的插件，请在 `init.lua` 中使用 `mare.keymap.set`。

```lua
mare.setup {
  keys = {
    ["<C-o>"] = "transcript",
    ["<M-p>"] = false,          -- free Alt+P (the model picker's default key)
  },
}
```

用两种写法（`"<C-o>"` 和 `"ctrl+o"`）为同一个按键写下不同动作属于错误，两者都不会被绑定。需要 `editor` 以外的上下文、`when` 或函数时，请使用 `mare.keymap.set`。

## 移除和查看绑定

| 调用 | 作用 |
|---|---|
| `mare.keymap.set(context, key, false)` | 屏蔽此上下文中该按键在调用者所在层及查找链后续上下文里的绑定；移除返回的句柄可解除屏蔽。 |
| `mare.keymap.del(context, key)` | 只移除调用者所在层中 `context` 下 `key` 的最新绑定。下面的那个会恢复。 |
| `mare.keymap.context(name, { parent })` | 在 `parent` 下声明插件子上下文；`handle:remove()` 删除它、其后代以及里面的绑定。 |
| `mare.keymap.remove(handle)` | 移除某次 `set` 调用返回的绑定。 |
| `mare.keymap.list([opts])` | 返回所有仍可触发且未被屏蔽的绑定（同一上下文、同一按键可以有多行，如下表中的两个 `Ctrl+R`），形如 `{ key, context, desc, action, when }`。支持 `list({ context = "editor" })` 和 `list({ snapshot = true })`。 |
| `mare.keymap.hint(action, context)` | 当焦点在 `context` 时能触发 `action` 的按键（`project > user > plugin > default`，同层按 `priority` 与登记先后）。 |
| `mare.keymap.actions()` | 已登记的核心与 Lua 动作列表（`{ name, desc, contexts, core, essential }`）。 |

```lua
mare.keymap.set("editor", "<C-z>", false) -- disable suspend
```

`/hotkeys` 在 TUI 中显示同样的列表。在空输入上按 `?` 可以得到可搜索的列表。执行 `/reload config` 和 `/reload` 时，配置重评估会先提交暂存的 `init.lua` 绑定，随后再为插件重载开启插件键位守卫：若 `init.lua` 本身求值失败，原有配置绑定会完整保留；若后续插件重载失败或被取消，插件键位由守卫处理，但先前已提交的新配置绑定仍可能保持生效。

如果必要动作（`editor.submit`、`input.accept`、`input.cancel`）没有绑定任何按键（例如关闭了 `keymap-default` 或显式解绑），`/hotkeys` 会在 `No key` 分组下列出它，`mare doctor` 也会输出一条警告，而不会静默恢复默认键。

## 默认键位

核心动作的默认键由内置插件 `keymap-default` 登记，其余插件动作（`review.viewer`、`agents`、`model.picker`、`model.cycle`、`effort.down`、`effort.up` 等）由各自的内置插件登记在 `default` 层。`mare.plugin("keymap-default", { enabled = false })` 会关闭核心默认键；其他插件登记的按键不受影响，保留键 `Ctrl+C` 照常可用。

在 `init.lua` 中用 `mare.keymap.set` 改绑或禁用任意默认键：

```lua
mare.keymap.set("editor", "<M-o>", "model.picker")
mare.keymap.set("editor", "<M-m>", false) -- 禁用 model.cycle
mare.keymap.set("editor", "<C-y>", "review.viewer")
mare.keymap.set("editor", "<M-a>", "agents", { when = "editor_empty" })
```

<!-- BEGIN GENERATED KEYMAPS (scripts/gen_docs.py) -->
| 上下文 | 按键 | 动作 | 条件 | 说明 |
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

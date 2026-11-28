# 外观

改变 mare 的外观：命名样式、主题和风格、覆盖 UI 组件和工具渲染器、状态栏、边框以及其他外观选项。

## 样式

样式是一个名称（如 `MareAccent`）加上一份规格：前景色、背景色和文本属性。用 `mare.ui.style` 设置；只传一个参数调用则读取它。

```lua
mare.ui.style("MareAccent", { fg = "#7aa2f7", bold = true })
mare.ui.style("MareDim", "#5c6370")          -- a string is a foreground color
local spec = mare.ui.style("MareAccent")             -- read it back
```

规格接受 `fg`、`bg`（主题槽位名称或 `#rrggbb`）以及布尔值 `bold`、`dim`、`italic`、`underline`、`strike`、`reverse`。你显式设置的样式始终优先于主题，并且在主题变化时会被重新应用。

`theme` 选项可以一次设置多个样式。字符串值是前景色。

```lua
mare.setup {
  theme = {
    MareToolName = { fg = "#7aa2f7", bold = true },
    MareDim = "#5c6370",
  },
}
```

主要的样式名称：

| 名称 | 用途 |
|---|---|
| `MareText`、`MareMuted`、`MareDim`、`MareSubtle` | 正文及其较淡的层次。 |
| `MareAccent`、`MareTitle`、`MareHeading` | 强调、标题、Markdown 标题。 |
| `MareError`、`MareWarning`、`MareSuccess`、`MareInfo` | 状态颜色。 |
| `MareUserMessage`、`MareUserPrompt`、`MareReasoning` | 你的消息和模型的思考。 |
| `MareToolName`、`MareToolArgs`、`MareToolOk`、`MareToolError`、`MareToolPending`、`MareToolResult` | 工具调用行。 |
| `MareDiffAdd`、`MareDiffDelete`、`MareDiffAddWord`、`MareDiffDeleteWord` | 差异。 |
| `MareBorder`、`MareBorderActive`、`MareSelection` | 面板和选中的行。 |
| `MareModeReadOnly`、`MareModeAuto`、`MareModeFull`、`MareModeCustom` | 状态栏中的权限胶囊。 |

`mare.ui.styles()` 返回所有名称。在 TUI 中，`/lua mare.ui.styles()` 会把它们打印出来。

## 主题与风格

风格（flavor）是适用于深色或浅色终端的一整套样式。用下面这些选项选择：

| 选项 | 默认值 | 含义 |
|---|---|---|
| `flavor` | `auto` | `auto`、`dark`、`light`，或已安装主题的名称。 |
| `flavor_dark` | `dark` | `auto` 在深色或未知背景下选择的风格。 |
| `flavor_light` | `light` | `auto` 在浅色背景下选择的风格。 |

使用 `auto` 时，mare 会在第一帧之后向终端询问其背景色并据此切换。`/theme` 打开带实时预览的选择器；`/theme <name>` 直接选择一个。两者都会把选择保存到 `state.json`，而在 `init.lua` 中设置的 `flavor` 在下次启动时仍然优先。

```lua
mare.setup {
  flavor = "auto",
  flavor_dark = "night",     -- a theme you installed
  flavor_light = "light",
}
```

### 编写主题

在 `~/.config/mare/themes/` 中放一个文件 `<name>.lua`（如果只用于语法高亮，也可以是 `.tmTheme` 文档）。包可以在它的 `themes/` 目录中提供同样的文件。文件返回一个表。最简单的形式是修改深色或浅色基础主题的调色板：

```lua
-- ~/.config/mare/themes/night.lua
return {
  base = "dark",                       -- "dark" or "light"
  palette = {
    accent = "#7aa2f7",
    accent2 = "#ff9e64",
    selection = "#2a2f45",
  },
}
```

调色板的键有 `accent`、`accent2`、`text`、`muted`、`dim`、`subtle`、`border`、`selection`、`code_bg`、`green`、`yellow`、`red`、`blue`、`add_bg`、`del_bg`、`add_word`、`del_word` 和 `on_color`，每个都是 `#rrggbb`。mare 会根据调色板推导出各个命名样式。要直接控制样式，返回 `styles = { MareAccent = { fg = "#7aa2f7" } }` 来代替 `palette`。成对的主题给出 `variants = { dark = {...}, light = {...} }`；两者都必需，之后 `auto` 会跟随终端。

没有文件时，可以在 `theme-default` 插件的选项中定义风格：

```lua
mare.plugin("theme-default", {
  flavors = { night = { MareAccent = { fg = "#7aa2f7" } } },
  blend_background = false,   -- do not mix the palette with the terminal's background
})
```

`mare.plugin("theme-default", { dirs = { "~/themes" } })` 会替换被搜索的目录。

## 覆盖组件

对话记录的各个部分是 `mare.ui` 中的 Lua 表，其方法可以被替换。`init.lua` 在默认值安装之后运行，所以赋值会生效。

```lua
-- replace how a tool call header looks
function mare.ui.ToolCall:header(summary, state)
  local mark = state == "ok" and "+" or state == "error" and "x" or "."
  local hl = state == "error" and "MareToolError" or "MareToolOk"
  return ui.Line({ ui.Span(mark .. " "):hl(hl), ui.Span(summary):hl("MareToolName") })
end

-- wrap a component instead of replacing it
local render = mare.ui.UserMessage.render
function mare.ui.UserMessage:render(text)
  return ui.Lines({ render(self, text), ui.Gap() })
end
```

可以覆盖的组件包括 `UserMessage`、`AssistantMessage`（`render`、`marker`）、`Reasoning`、`TurnEnd`、`Notice`、`ToolCall`、`Status`、`Editor`、`Logo`、`Working`，以及浮层部件 `KeyHints`、`Tabs`、`Select` 和 `Confirm`。组件在消息开始或结束以及失效时运行，绝不会对每个流式 token 运行。

`ui.Span`、`ui.Line`、`ui.Lines`、`ui.Gap`、`ui.Paragraph`、`ui.Row`、`ui.Block`、`ui.Markdown` 等用于构建组件返回的节点；[Lua API](../extend/lua-api.md) 页面列出了它们。

### 工具渲染器

`mare.ui.renderer(kind, { compact = fn, expanded = fn })` 改变某个工具的最终结果的绘制方式。`kind` 是 `"tool:<name>"`。`compact` 用于回滚缓冲区，`expanded` 用于对话记录（`Ctrl+T`）；两者都接收 `(call, result)` 并返回一个节点。该函数返回一个移除函数。

```lua
mare.ui.renderer("tool:bash", {
  compact = function(call, result)
    local command = call.arguments and call.arguments.command or "bash"
    return ui.Line({ ui.Span("$ "):hl("MareAccent"), ui.Span(command) })
  end,
})
```

用户消息和思考消息以同样的方式接受 `message:user` 和 `message:reasoning` 渲染器，分别接收 `(text)` 和 `(text, secs)`。

在 `init.lua` 中设置的渲染器和组件会在 `/reload config` 时被移除并重新设置。

## 状态栏

`statusline` 插件绘制输入框下方的栏：权限胶囊、模型和强度、上下文仪表、目录、分支、拉取请求、已更改的文件、费用。`/statusline` 打开选择器来隐藏片段；选择保存在 `state.json` 中。在 `init.lua` 中：

```lua
mare.plugin("statusline", {
  hidden = { "pr", "cost" },     -- mode, model, effort, context, cwd, branch, pr, changes, cost, quota
  cwd_width = 32,
  gauge_width = 8,
  changed_files = true,
})
```

`layout` 选择不同的排布：`combined`（默认）、`classic`、`codex` 或 `kimi`。`layout_opts` 覆盖其中的单个元素，`looks` 定义你自己的排布。

```lua
mare.plugin("statusline", {
  layout = "combined",
  layout_opts = { sep = "space", flash = 0, cwd_segments = 3 },
})
```

look 是数据：`left` 和 `right` 列出片段，`drop` 列出终端变窄时这些片段让位的顺序（`hints:short` 会先缩短再丢弃），其他键选择每个元素的形式：`sep`（`dot` 或 `space`）、`mode`（`word`、`capsule`、`dim`、`off`）、`model`（`text`、`dim`、`muted`）、`effort`（`reasoning`、`suffix`、`off`）、`jobs`（`bracket`、`icon`）、`context`（`kimi`、`left`、`gauge`）、`context_color`、`hints`、`flash`（变化保持可见的秒数）和 `cwd_segments`。

要显示你自己脚本的输出，设置 `command`（通过 shell 运行的字符串，或 argv 表）。它在标准输入上收到一个 JSON 对象（会话 id、cwd、宽度、模型、上下文、费用），显示它输出的第一行。`command_replaces = true` 会隐藏内置片段。

```lua
mare.plugin("statusline", { command = "~/bin/statusline.sh", command_timeout_ms = 500 })
```

后台探测段：`changes`（`git status`）、`pr`（`gh pr view`）以及外部 `command` 输出会在启动就绪后（`ui/ready`）移到后台异步执行，并在每轮结束后刷新，因此启动首帧不会阻塞在子进程探测上（`branch` 在首帧同步读取 `.git/HEAD` 显示，并在每轮结束后于后台刷新）；外部 `command` 仅在 `ui/ready` 之后运行，因此在无 UI 或 RPC 会话中不会执行。探测结果到达后，这些段会就地更新内容。其余所有段（`mode`、`hints`、`flags`、`model`、`effort`、`context`、`cwd`、`branch`、`cost`、`quota`、`peer_allow`、`tips`、`jobs`、`workflow`、`mcp`）从首帧起同步绘制，与就绪状态一致。

要从你自己的插件添加片段，使用 `ui` 服务：`ctx.ui:status(id, { align = "right", order = 20, render = function() ... end })`。见[编写插件](../extend/writing-a-plugin.md)。

## 其他外观选项

| 选项 | 取值 | 效果 |
|---|---|---|
| `border` | `rounded`（默认）、`single`、`double`、`thick`、`hidden` | 面板和弹出窗口的边框。 |
| `icons` | `nerd`、`unicode`、`ascii` | 图标集（未设置时按终端自动检测）。启用 `nerd` 时，提示框与围栏代码块标题会显示 Nerd Font 图标（覆盖 70+ 种常见语言及别名，如 `rust`、`python`、`ts`、`go`、`lua`、`bash`、`sql`、`docker`、`yaml`）。 |
| `prompt` | 字符串，默认 `❯` | 输入提示符。 |
| `terminal.ambiwidth` | `single`、`double` | 东亚模糊宽度字符的宽度，与 Vim 相同。 |
| `spinner` | 字形列表，或 `{ frames = {...}, still = "•" }` | 旋转指示器的字形。 |
| `ui.working` | `{ glyph, label, elapsed, tokens, interrupt_hint, sep, tools, interval_ms, text_throttle_ms, frames, glyphs, verbs }` | 工作状态行细粒度外观（默认 `{ glyph = "ember", label = "glint", elapsed = "auto", tokens = true, interrupt_hint = true, sep = "dot", tools = "still", text_throttle_ms = 250 }`）。 |
| `ui.tool_call`、`ui.stopping` | `{ spinner = "braille" }` / `{ spinner = true }` | 工具调用与停止阶段的旋转指示器（设为 `false` 时显示静态圆点）。 |
| `markdown` | `{ glyphs = {...}, code_line_numbers = false }` | Markdown 渲染（当 `icons = "nerd"` 时，围栏代码块标题自动附带语言图标）。 |
| `logo` | `true`、`false`、`"compact"` | 启动标志。 |
| `reduce_motion` | 布尔值 | 用静态文本代替动画。 |
| `images_protocol` | `kitty`、`iterm2`、`sixel`、`none` | 终端图像协议。未设置时自动检测。 |
| `char_widths` | `{ from, to, width }` 或 `{ char, width }` 的列表 | 显示宽度覆盖。 |

```lua
mare.setup {
  border = "single",
  prompt = ">",
  icons = "nerd",
  ui = {
    working = {
      glyph = "ember",
      label = "glint",
      elapsed = "auto",
      tokens = true,
      interrupt_hint = true,
      sep = "dot",
      tools = "still",
      text_throttle_ms = 250,
    },
    tool_call = { spinner = "braille" },
  },
  markdown = { code_line_numbers = true },
  reduce_motion = true,
}
```

`ui.working` 的每一项外观都可以单独配置：`glyph`（`ember`、`swell`、`braille`、`moon`、`phase`、`wave`、`sail`、`bullet`、`pulse`、`none` 或 `glyphs` 中定义的名称）、`label`（`accent`、`shimmer`、`glint`、`foam`、`dots`）、`elapsed`（`auto`、`seconds`、`off`）、`tokens`、`interrupt_hint`、`sep`（`dot`、`bullet`）、`tools`（`still`、`spin`）、`interval_ms`、`text_throttle_ms`、内联 `frames` / 自定义 `glyphs` 以及重命名 `reasoning` / `responding` / `tool` / `shell` / `exploring` 各阶段标签的 `verbs`。在 `reduce_motion` 下一切都保持静止。[选项](options.md)页面有带默认值的完整列表。

影响对话记录的插件选项，例如 `preview_lines`（紧凑视图中每个工具调用显示的输出行数）和 `diff_fold_lines`（紧凑 diff 折叠前保留的行数，默认 `8`，设为 `false` 展开完整 diff），属于 `ui` 插件：

```lua
mare.plugin("ui", { preview_lines = 4, diff_fold_lines = 12 })
```

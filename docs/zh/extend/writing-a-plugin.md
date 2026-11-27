# 编写插件

一步步构建一个小型 mare 插件：一个工具、一个斜杠命令、一个事件监听器、一个配置选项和一个状态栏片段，然后测试并发布它。

请先阅读[插件](plugins.md)了解定义的字段。

## 你需要插件吗？

运行命令的工具不需要插件。在 `init.lua` 中声明即可：

```lua
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

`command` 中的每个 `{name}` 都由模型的参数填入。没有 `readonly = true` 时，mare 会认为该命令有副作用。需要 Lua 逻辑、事件、命令或界面部件时，再使用插件。

## 第 1 步：骨架

创建 `~/.config/mare/plugins/tally.lua`：

```lua
return {
  name = "tally",
  inject = { "tools", "commands", "ui" },
  apply = function(ctx, cfg)
    ctx.commands:register("tally", {
      description = "Show how many tool calls the current turn has made",
      run = function()
        ui.notify("tally is loaded")
      end,
    })
  end,
}
```

启动 mare，或在运行中的会话里执行 `/reload`，然后输入 `/tally`。会出现一条通知。`/plugins` 会列出来源为 `user` 的 `tally`。

`inject` 列出插件使用的服务。每个服务随后都是 `ctx` 的一个字段：`ctx.commands`、`ctx.tools`、`ctx.ui`。不带 `ctx.` 的 `ui` 是用于构建节点和通知的全局命名空间。

## 第 2 步：一个工具

用 `ctx.tools:register(name, def)` 注册工具。下面这个工具统计模型传入文本的行数、单词数和字符数。

```lua
ctx.tools:register("text_stats", {
  snippet = "Count lines and words in a piece of text",
  description = "Count the lines, words and characters of the given text.",
  parameters = {
    type = "object",
    properties = { text = { type = "string", description = "The text to measure" } },
    required = { "text" },
  },
  side_effect = "read",
  summary = function(args)
    return ("TextStats(%d chars)"):format(#(args.text or ""))
  end,
  execute = function(args)
    local text = args.text or ""
    local _, lines = text:gsub("\n", "\n")
    local words = 0
    for _ in text:gmatch("%S+") do words = words + 1 end
    return { content = ("lines: %d\nwords: %d\nchars: %d"):format(lines + 1, words, #text) }
  end,
})
```

该工具走与内置工具相同的流水线：`tool/rewrite`、参数检查、`tool/before` 投票、审批，然后是 `execute`。插件被销毁时，工具会被撤回。

工具定义的字段：

| 字段 | 含义 |
|---|---|
| `description`、`parameters` | 模型看到的内容。`parameters` 是 JSON Schema 对象。两者都可以是 `(model, mode)` 的函数。 |
| `execute(args, tctx)` | 运行调用。可以 yield。返回结果表或字符串。 |
| `snippet`、`guidelines` | 给系统提示词的一行说明和额外规则。 |
| `summary(args)` | 对话记录中显示的标题，如 `Read(src/main.rs)`。 |
| `render(args, result, expanded)` | 结果的自定义节点。 |
| `readonly` | 无副作用。连续的只读调用并行运行。 |
| `side_effect` | `"read"`、`"write"`（仅限本地文件）或 `"external"`。`readonly` 时默认为 `"read"`，否则为 `"external"`。 |
| `idempotent` | 重复运行是安全的。运行到一半崩溃的非幂等调用，恢复时视为结果未知，绝不会重放。 |
| `unconfined` | 在本进程之外运行，因此本地沙箱无法约束它。 |
| `enabled(model, edit_mode)` | 返回 `false` 则对某个模型隐藏该工具。 |
| `timeout_ms` | 调用的时间限制。 |

`tctx` 带有 `cancel`（传给阻塞调用的令牌）、用于进度的 `update(partial)`、`cwd` 和 `tool_call_id`。结果表有 `content`（字符串，或内容块列表），以及可选的 `is_error`、`details`、`display`（一个节点）、`no_truncate` 和 `max_line_chars`。

## 第 3 步：事件监听器

在 `item/completed` 中统计工具调用，并在每一轮开始时重置：

```lua
local calls = 0

ctx:on("turn/started", function() calls = 0 end)
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
  end
end)
```

插件被销毁时，监听器会被移除。修改 `/tally` 命令来报告这个数字：

```lua
run = function()
  ui.notify(("%d tool call(s) in this turn"):format(calls))
end,
```

事件名称以及返回值的作用见[服务与事件](services-and-events.md)。

## 第 4 步：配置选项

在 `config_schema` 中逐项声明选项。默认值加上用户的设置以 `cfg` 传入；类型不对或超出范围的值会被报告并保留默认值。

```lua
config_schema = {
  warn_after = { type = "integer", default = 25, min = 1, desc = "notify when one turn makes more tool calls than this" },
  show_in_status = { type = "boolean", default = true, desc = "show the count in the status line" },
},
```

在监听器中使用它：

```lua
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
    if calls == cfg.warn_after + 1 then
      ui.notify(("tally: more than %d tool calls in this turn"):format(cfg.warn_after), "warning")
    end
  end
end)
```

用户在 `init.lua` 中修改它。你没有声明的键会产生带建议的警告。

```lua
mare.plugin("tally", { warn_after = 10 })
```

选项变化时，`/reload config` 会重新加载该插件。

## 第 5 步：状态栏片段

`ctx.ui:status(id, def)` 添加一个片段。`render` 返回一个节点，返回 `nil` 则隐藏。内容变化时调用 `ctx.ui:invalidate(id)`。mare 只在被要求时才绘制。

```lua
if cfg.show_in_status then
  ctx.ui:status("tally", {
    align = "right",
    order = 20,
    render = function()
      if calls == 0 then return nil end
      return ui.Line({ ui.Span(("calls: %d"):format(calls)):hl("MareDim") })
    end,
  })
end
```

每次修改 `calls` 之后加上 `ctx.ui:invalidate("tally")`。`ctx.ui:widget(id, { slot = "above-editor", render = function(width, max_height) ... end })` 则在输入框的上方或下方绘制一个块。

上面所有的代码片段都按顺序放在 `apply` 里，`local calls = 0` 放在最前面。`inject` 和 `config_schema` 与 `apply` 一起放在返回的表中。最终结果是一个约 55 行的文件。

## 测试

- **重新加载。** 编辑文件，然后对此插件运行 `/reload tally`，或对全部运行 `/reload`。加载失败的插件会继续运行它之前的版本。
- **检查状态。** `/plugins` 显示它是否处于活动状态，失败时会打印带回溯的错误。失败也会在状态栏显示为 `plugin tally failed · /reload`。
- **Lua 控制台。** `/lua` 在运行中的会话里运行 Lua，例如 `/lua mare.plugins.status("tally")` 或 `/lua mare.o.border`。不带代码时会打开一个可输入多行的提示。它只存在于 TUI 中，无法通过 RPC 访问。
- **日志。** `ctx.logger:warn("...")` 写入 `~/.local/state/mare/log/` 下的日志。`/debug` 显示最近的几行。
- **不用 TUI。** 在无界面会话中加载插件并读取标准错误。无法加载的定义会被报告为 `mare: warning: plugin ...`：

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","client":{"name":"t"},"capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}' | mare --rpc >/dev/null
```

在前面加上 `MARE_HOME=$(mktemp -d)`，就可以针对空配置进行测试。

### 用 `mare test` 运行自动化测试

`mare test` 会在指定路径（默认当前目录，跳过 `.git`、`target`、`node_modules` 和 `snapshots`）下递归发现所有 `*_spec.lua` 文件，并在内置 `mare.test`（`require("mare.test")`）测试框架里执行。

用 `t.host()` 启动一个隔离的真实插件宿主（与正式运行相同的 Cordis 生命周期树、loader、事件总线与契约准入；可按需传入 `builtins`，或用 `host:provide(name, impl, opts?)` 提供同样经过契约校验的假服务）。

在 `plugins/tally.lua` 旁创建 `tests/tally_spec.lua`：

```lua
local t = require("mare.test")

t.describe("tally", function()
  t.it("counts lines, words, and chars via text_stats", function()
    local host = t.host({ builtins = { "tools", "commands", "ui" } })
    local plugin = dofile("plugins/tally.lua")
    host:load(plugin, { warn_after = 25, show_in_status = false })
    t.expect.settled(host)
    t.expect.eq(host:state("tally"), "ACTIVE")

    local tool = assert(host:get("tools"):get("text_stats"))
    local res = tool.execute({ text = "alpha beta\ngamma" })
    t.expect.eq(res.content, "lines: 2\nwords: 3\nchars: 16")
    t.expect.snapshot(res.content, "text_stats_sample")

    host:dispose()
    t.expect.eq(host:get("tools"), nil)
  end)
end)
```

因为 `host` 跑的是真实的插件生命周期，同一套夹具可以直接测缺依赖时停在 `"PENDING"`、通过 `host:provide(...)` 补齐依赖与契约守卫、`host:dispose()` 卸载清理以及 `host:reload(name, def, cfg)` 失败回滚。未显式调用 `dispose()` 的测试宿主会在当前 `t.it` 用例结束时自动回收。

由于 `tests/tally_spec.lua` 包含快照断言，首次运行时先用 `-u` 生成并检查基线快照，再在插件仓库根目录运行常规测试：

```sh
mare test -u tests/tally_spec.lua # 首次运行直接创建或更新 snapshots/*.snap（提交前检查内容）
mare test                         # 自动发现并运行全部 *_spec.lua 并比对已有快照
mare test tests/tally_spec.lua    # 运行单个 spec 文件
mare test -f "text_stats"         # 按完整用例名子串过滤
```

`expect.snapshot(value, name?)` 把快照写入 `<spec_dir>/snapshots/<spec_stem>__<name>.snap`。未传 `-u`（`--update-snapshots`）时，快照不匹配会判失败并写出 `.snap.new` 供比对。退出码为 `0`（全部通过）、`1`（有用例失败）或 `2`（参数、发现或加载/语法错误）。

如果你在 mare 仓库的检出副本中开发内置插件，`just run` 会启动 debug 构建，每次启动都会从磁盘读取 `crates/mare-plugin/preset/plugins/*.lua`。编辑文件后执行 `/reload` 即可，不需要重新编译。

## 作为包发布

包就是一个 git 仓库。把插件放进 `plugins/`：

```
mare-tally/
  plugins/tally.lua
  mare-plugin.toml        # optional
```

```toml
# mare-plugin.toml
name = "mare-tally"
description = "Count tool calls per turn"
version = "0.1.0"
requires = ">=0.1"     # a mare older than this will not load the package
```

推送它，并用 semver 标签给发布版本打标签：

```sh
git tag v0.1.0 && git push --tags
```

任何人只要在 `init.lua` 中声明它并运行 `mare plugin sync` 就能安装。选项放在插件名称下：

```lua
mare.plugins.add {
  { "you/mare-tally", opts = { tally = { warn_after = 10 } } },
}
```

一个仓库可以包含多个插件，用户可以用 `only = { "tally" }` 加载其中一部分。包也可以附带 `skills/`、`prompts/`、`themes/` 和 `types/`（Lua 类型定义）。见[包](../configure/packages.md)。

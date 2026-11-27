# 钩子、状态栏与通知

mare 核心本身不内置 shell 脚本钩子运行器。在 mare 里，钩子通常表现为 `init.lua` 和插件中的 Lua 监听器（`mare.on`、`ctx:on`、`mare.policy.voter`、`ctx.policy:rewriter`），或由 `statusline`、`notify`、外部钩子包调用的外部命令。

## `init.lua` 与插件中的生命周期钩子

在 `~/.config/mare/init.lua` 中使用 `mare.on` 订阅内核事件，或用 `mare.policy.voter` 在策略点投票：

```lua
-- ~/.config/mare/init.lua
mare.on("turn/started", function(ev)
  mare.log("info", "turn started: " .. tostring(ev.turn_id))
end)

mare.policy.voter("tool/before", function(call)
  if call.name == "bash" and (call.arguments.command or ""):find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

完整的插件级控制接口（`ctx:on`、`ctx.policy:rewriter`、`ctx.bus:waterfall`）见[服务与事件](../extend/services-and-events.md)。

### 常用生命周期与策略点

| 事件 / 策略点 | 类型 | 常见用途 |
|---|---|---|
| `app/ready`、`app/shutdown` | 通知 | 启动完成初始化，或退出前清理资源 |
| `session/started`、`session/loaded`、`session/changed` | 通知 | 响应会话新建、恢复与切换 |
| `input/submit`、`input/transform` | 通知 / 瀑布流 | 记录或改写用户提交的输入文本 |
| `request/prepare`、`request/ready`、`request/build` | 检查 / 策略 / 严格瀑布流 | 拦截或修改发往模型的请求 |
| `context/system`、`context/transform` | 瀑布流 | 动态拼接系统提示词或调整消息列表 |
| `tool/rewrite`、`tool/before`、`tool/approval`、`tool/after` | 改写 / 策略 / 瀑布流 | 清洗工具参数、拒绝危险调用或加工工具输出 |
| `turn/started`、`turn/completed`、`agent/settled` | 通知 | 跟踪轮次起止（先发 `turn/completed`，其监听器全部返回后再发携带 `busy` 的 `agent/settled`） |
| `option/changed`、`reload/completed` | 通知 | 响应运行时配置变更与热重载 |

### 出错处理

不同派发方式对异常的处理不同：

- **普通通知（`emit`）**：监听回调在轮次内抛出错误时，mare 会记录回溯并在当轮提示下方显示一行简短提示（`plugin <name> hook <event> failed: ...`），不会停用插件或中断其他监听器。
- **严格派发（`emit_checked`、`waterfall_checked`）**：异常会直接向上传播。`request/prepare` 或 `request/build` 抛错会在发往模型提供方之前直接中止本次请求。
- **策略投票**：若 `request/ready` 投票拒绝发送提示，提示文本会退回输入框；`tool/before` 或 `tool/approval` 投票者抛错或超时（`policy_timeout_ms`，默认 30 秒）时一律按 `deny` 处理。

### 兼容 Claude Code / Codex 的 Shell 钩子包（`mare-plugins`）

如果需要用声明式 JSON/shell 钩子对接类似 Claude Code 或 Codex 的事件集（如 `PreToolUse`、`PostToolUse`、`UserPromptSubmit`、`SessionStart`），可以从 `signalridge/mare-plugins` 通过 `/plugins` 或 `mare.plugins.add` 安装钩子包来执行外部命令，不必自己在 `init.lua` 里编写进程调度代码。

## 自定义状态栏（`statusline`）

`statusline` 插件绘制输入区下方的状态栏（包含 `mode`、`model`、`effort`、`context`、`cwd`、`branch`、`pr`、`changes`、`cost`、`quota`）。

### 内置样式与开关

在会话里运行 `/statusline` 可以开关单个状态段（选择会存入 `state.json`），也可以在 `init.lua` 里固定布局：

```lua
mare.plugin("statusline", {
  layout = "combined", -- "combined" | "classic" | "codex" | "kimi" | 自定义布局表
  hidden = { "pr", "cost" },
  cwd_width = 24,
  gauge_width = 6,
  changed_files = true,
  pr = true,
  layout_opts = { sep = "space", flash = 0 },
})
```

### 外部状态栏脚本

配置 `command` 后，mare 会在启动、切换模型及每轮结束时执行指定脚本，通过标准输入传入 JSON 对象（`session_id`、`transcript_path`、`cwd`、`width`、`model`、`context`、`cost`），并在状态栏显示输出的第一行：

```lua
mare.plugin("statusline", {
  command = "~/bin/mare-statusline.sh",
  command_timeout_ms = 1000,
  command_replaces = false, -- 设为 true 时隐藏所有内置分段
})
```

插件也可以用 `ctx.ui:status(id, { align = "right", order = 20, render = ... })` 注册自定义状态段。

## 通知（`notify`）

当单轮耗时达到 `min_turn_ms`（默认 5000 毫秒）完成，或 mare 等待用户输入时，`notify` 插件会发送终端通知（OSC 9、OSC 777、Kitty OSC 99、tmux/screen 透传或响铃；非 Windows Terminal 的 Windows 环境走原生桌面通知）。

```lua
mare.plugin("notify", {
  on_done = true,
  on_input = true,
  only_unfocused = true,
  min_turn_ms = 5000,
  protocol = "auto", -- "auto" | "bel" | "native"
  bell = false,
  dedupe_ms = 10000,
})
```

### 外部通知命令（`notify.cmd`）

配置 `cmd` 后，无论运行在 TUI、无界面还是 RPC 模式，每次轮次完成或等待输入都会把事件 JSON（`type` 为 `"agent-turn-complete"` 或 `"input-required"`，附带 `session-id`、`turn-id`、`cwd` 以及 `last-assistant-message` 或 `message`）送到该程序的标准输入：

```lua
mare.plugin("notify", {
  cmd = { "notify-send-mare" },
  cmd_timeout_ms = 10000,
})
```

## 相关页面

- [服务与事件](../extend/services-and-events.md)：编写 Lua 事件与策略钩子
- [事件参考](../reference/events.md)：由代码生成的公开协议事件 schema
- [外观](ui.md)：主题、边框与 UI 组件覆盖

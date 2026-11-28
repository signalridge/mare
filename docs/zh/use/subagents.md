# 子代理与临时旁路对话

在主对话之外处理分支任务，mare 提供两种方式：落盘可跟踪的子代理线程（`subagent`），以及从当前已提交历史分支、关闭后即丢弃的临时旁路对话（`/side`、`/btw`）。

## 子代理

`subagent` 插件允许模型拉起独立的子会话，各有独立的会话上下文；默认沿用父工作目录（可通过 `cwd` 指定目录），并按类型或调用参数选择工具白名单和模型覆盖。子线程由宿主的 `AgentRegistry`（`crates/mare-host`）统一管理，受核心 `agents` 配额限制，并保存在 `sessions/<project>/agents/<run>/<seq>-<label>.jsonl`。

### 暴露给模型的工具

| 工具 | 作用 |
|---|---|
| `subagent` | 启动子代理，参数包括 `prompt`、可选的 `type`、`fork`（`"none"`、`"all"` 或轮数）、`name`、`cwd`、`model`、`effort`、`tools`、`system` 和 `wait`（默认 `false`）。 |
| `subagent_wait` | 阻塞等待指定子代理完成（或到 `timeout_ms` 超时），返回最终输出。 |
| `subagent_steer` | 向运行中的子代理发送引导消息。 |
| `subagent_cancel` | 取消指定子代理及其后代线程。 |
| `subagent_list` | 列出当前会话派生的子代理及其 id、名称、状态和任务说明。 |

默认情况下，`subagent` 在后台启动子代理。结束后，mare 会向父会话投递一行完成摘要（`[subagent <id> finished: <status>...]`），仅在有输出正文或错误信息时附带 `<subagent-output agent="…">` 标签块。把 `wait = true` 传入就会同步等它跑完。

后台子代理属于启动它的 mare 进程。如果该进程在子代理完成前退出或崩溃，恢复父会话时未完成的子代理会被标为 `interrupted`：`subagent_list` 和 `subagent_wait` 可查到该状态，父会话会收到一条带子代理标识及其对话记录位置的通知，以便重新发起任务。

### 内置子代理类型

| 类型 | 用途 | 工具范围 |
|---|---|---|
| `general` | 常规委派实现或排查任务 | 继承父会话可用工具与权限上限 |
| `explore` | 检索代码、追踪行为但不改文件 | `read`、`ls`、`bash`（只读沙箱） |
| `plan` | 基于源码制定实施方案但不改文件 | `read`、`ls`、`bash`（只读沙箱） |

### 自定义子代理类型

在 `~/.config/mare/agents/*.md` 或受信任项目的 `.mare/agents/*.md` 里用带 YAML frontmatter 的 Markdown 文件定义子代理类型：

```markdown
---
name: security-auditor
description: 审查改动中的鉴权、路径穿越和密钥泄漏风险。
when_to_use: 在合并鉴权、RPC 或沙箱相关改动前使用。
tools:
  - read
  - ls
model: anthropic/claude-sonnet-5-5
effort: high
---

你是一名只读安全审阅员。只报告有确切文件和行号依据的问题。
```

Frontmatter 支持 `name`、`description`（必填）、`when_to_use`、`tools`、`disallowed_tools`、`model`、`effort`；正文作为该类型的系统提示词。

> `tools` 仅过滤工具名称，自定义子代理继承父会话的权限上限。要强制只读边界，省略 `bash` 等写入工具或使用 `mare --permission read-only --agent security-auditor`（内置 `explore`、`plan` 和 `/review` 均显式设置 `permission = "read-only"`）。

在 `init.lua` 里追加搜索目录（项目相对路径需要信任）：

```lua
mare.plugin("subagent", {
  extra_agent_dirs = { ".mare/custom-agents" },
  notify_inline_bytes = 4096,
})
```

### 并发限制与子代理审批

整棵子代理树共享根会话的限额，在 `mare.setup` 中配置：

```lua
mare.setup {
  agents = {
    max_active = 16, -- 整棵树同时存活的后代线程上限（设为 0 禁用子代理）
    max_depth = 4,   -- 从根会话算起的最大嵌套深度
  },
}
```

子代理触发超出沙箱的权限请求时，`agent-approvals` 插件会把审批弹窗转发到父会话 UI（`agent/approval-dialog`）。输入框下方会显示按到达顺序排队的待审批列表，弹窗标题会写明请求来自哪个子代理。

#### 权限继承与父会话切换规则

- **启动时继承**：子代理在启动瞬间继承父会话当前生效的权限预设、沙箱策略和审批策略。父会话先切到 `full-access`（`FULL`）后再启动 `general` 子代理，子代理会直接继承 `full-access` 且不附带旧沙箱上限；显式只读子代理（`explore`、`plan`、`/review` 或显式指定 `permission = "read-only"`）始终锁定为 `read-only`，不受父会话预设影响。
- **父会话收紧立即下推**：父会话在子代理运行期间切到更严格的沙箱策略（如 `full-access` → `auto` / `read-only`，或撤销项目信任）时，会立即收紧所有已挂载后代 VM 的沙箱上限，并在已预留后代挂载时应用，作用于其后续工具调用并撤销过期授权。已经进入执行中的工具调用会继续持有调用开始时的冻结策略直至结束；仅收紧审批行为但沙箱策略未变的预设切换（例如沙箱同为 `danger-full-access`、审批为 `like = "read-only"` 的自定义预设）会在新启动或续用（`subagent_steer`）时生效。
- **父会话提权规则**：
  - 对于未锁定权限的继承型子代理（如 `general`），在父会话切到更宽松预设（如 `full-access`）后通过 `subagent_steer` / `mare.subagent.steer` 续用时，会将其权限预设与沙箱上限重新同步到父会话当前预设，后续有效权限内的写入与 `request_permissions` 不再弹窗。
  - 若运行中的未锁定子代理在父会话切到更宽松预设后发出沙箱类审批请求，父端会结合自身当前预设判定：属于父当前预设允许范围的请求会直接放行并记录日志，不弹确认框；与主会话 `full-access` 行为一致，危险命令和显式 `prompt` 规则仍会询问，`forbidden` 规则仍会拒绝。
  - 显式锁定为只读的子代理（`explore`、`plan`、`/review` 或显式指定 `permission = "read-only"` / `permission_ceiling`）在续用或审批时均**不会**因父会话提权而放宽。

### 用 `/agents` 查看子代理

运行 `/agents`（或在空输入框按 `Left` 方向键）打开代理列表，可以看到当前会话的子代理、工作流成员、后台任务，以及 `Peers` 标签页下同机运行的其他 `mare` 会话。

- 在某个子代理上按 `Enter` 只读查看其实时对话记录。
- 按 `Alt+Left` / `Alt+Right` 在不同子代理视图间切换。
- 按 `Esc` 回到列表，或按 `Ctrl+G` 跳转到 `/resume`。

## 临时旁路对话（`/side` 与 `/btw`）

`conversations` 插件从当前会话已提交的历史分支出临时旁路对话，关闭后只保留主对话原貌与用量统计，旁路消息绝不写入会话 JSONL 文件。

```text
/side 为什么这个测试夹具跳过了第 42 行？
/btw 这里的 EADDRINUSE 报错通常是什么原因？
```

`/side` 和 `/btw` 入口完全一致：

| 命令 | 动作 |
|---|---|
| `/side` 或 `/btw` | 在编辑器上方打开空白旁路窗口 |
| `/side <问题>` / `/btw <问题>` | 打开旁路窗口并立即发送问题 |
| `/side toggle` | 显示或隐藏当前旁路窗口 |
| `/side back`、`/side close` 或 `Esc` | 关闭并丢弃临时旁路对话 |

旁路窗口打开期间，输入框发送的内容会进入旁路线程而不打扰主代理，窗口顶部会显示父代理的实时状态（`idle`、`working` 或 `blocked`）。输入框里只允许运行安全检查命令（`/side`、`/btw`、`/help`、`/status`、`/session`），旁路循环中的工具调用也会在 `ephemeral-read-only` 生命周期约束下执行。旁路请求消耗的 token 和费用仍会由 `usage` 插件计入当前会话统计。

## 相关页面

- [权限与沙箱](permissions.md)：子代理审批与沙箱继承规则
- [RPC 对等套接字](../integrate/rpc.md#对等套接字)：同机 peer 套接字与 `mare peer send`
- [选项](../configure/options.md)：`agents` 核心配置项

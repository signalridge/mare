# 概览

mare 是一个由小巧的 Rust 核心与 Lua 5.5 插件构成的 agent harness，内置插件均可阅读、包装或替换。

## mare 是什么

Rust 核心负责操作系统沙箱、持久存储、Lua 运行时以及终端渲染（`mare-tui` 与 `mare-cli`）；agent 循环、工具、模型 provider、上下文压缩、权限规则，以及 UI 组件、主题和状态栏均作为内置 Lua 插件随二进制一起发布。

![mare 分层架构图](../../assets/diagrams/architecture.svg)

| 层 | 使用语言 | 作用 |
| --- | --- | --- |
| 核心 | Rust | 插件内核（事件、服务、生命周期、看门狗）、Lua 运行时、OS 沙箱、会话存储、文件快照、项目信任、终端渲染器（`mare-tui`）、mare RPC 3.0（JSON-RPC 2.0） |
| 内置插件 | Lua 5.5 | agent 循环、工具、provider、压缩、权限、审批、会话、斜杠命令、UI 组件、主题、状态栏、审阅、后台任务、MCP 客户端 |
| 你的插件 | Lua 5.5 | 来自 `init.lua`、`~/.config/mare/plugins/`、`.mare/plugins/` 或已安装包的扩展与覆盖 |

运行 `mare runtime` 查看存放内置插件只读源码（`agent.lua`、`tools.lua` 等）和 Lua 类型定义的目录。

## 替换，而不是打补丁

- 配置插件选项：`mare.plugin("tools", { bash_timeout_ms = 120000 })`
- 在 `init.lua` 中添加工具、斜杠命令、键位或样式。
- 用同名且带 `override = true` 的插件替换任意内置插件（`/plugins` 会标明覆盖关系）。

核心边界：

- 核心安全边界：沙箱隔离、文件/进程权限校验、崩溃安全 JSONL 写入、项目信任与 Lua 看门狗（`watchdog_ms`）在 Rust 内核实现。替换 agent 循环不会削弱经由权限插件与受保护工具管线的调用约束。
- 受信任插件边界：插件属于受信任代码。禁用或替换 `permissions` 插件，或在工具调用外直接调用原语，不受上述检查限制。
- 故障隔离：普通可选插件或包失败会报告警告并跳过；关键服务、声明 `required = true` 的插件或必需包失败会阻断请求，直到修复并 `/reload`。`/plugins` 显示完整 traceback。

## 一套能力，五种前端

| 前端 | 启动命令 | 用途 |
| --- | --- | --- |
| 终端 UI | `mare` | 交互式终端会话（`mare-tui` 与 `mare-cli`） |
| 无界面模式 | `mare -p "prompt"` 或 `mare --json "prompt"` | 面向脚本与 CI 的单次运行和 JSON 事件流 |
| stdio RPC | `mare --rpc` | 通过 mare RPC 3.0（JSON-RPC 2.0）驱动会话（`mare rpc schema`） |
| HTTP / WebSocket 监听器 | `mare --web` | 带认证的 HTTP 路由与 WebSocket RPC（Web UI 由插件提供） |
| ACP | `mare acp` | 编辑器接入（Agent Client Protocol） |

进程内 Lua（`init.lua` 与插件）位于所有前端共享的扩展层。

## 范围与路线图

后续版本规划：`ralph`、`lsp`、`model-router`、`worktree` 插件，`/import`，以及 HTML/JSONL 会话导出（当前可通过第三方插件实现）。

## 接下来看什么

- [安装](installation.md)
- [快速上手](quickstart.md)
- [工作流](workflows.md)
- [插件](../extend/plugins.md)与[编写插件](../extend/writing-a-plugin.md)
- [RPC](../integrate/rpc.md) 与 [ACP](../integrate/acp.md)

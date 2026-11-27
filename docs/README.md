# mare documentation

Documentation is organized by audience and available in English, Simplified Chinese, and Japanese.

| Language | Start here | Reference |
|---|---|---|
| English | [Overview](en/get-started/overview.md) · [Quickstart](en/get-started/quickstart.md) | [CLI](en/reference/cli.md) · [Options](en/configure/options.md) · [Lua API](en/extend/lua-api.md) |
| 简体中文 | [概览](zh/get-started/overview.md) · [快速上手](zh/get-started/quickstart.md) | [命令行](zh/reference/cli.md) · [选项](zh/configure/options.md) · [Lua API](zh/extend/lua-api.md) |
| 日本語 | [概要](ja/get-started/overview.md) · [クイックスタート](ja/get-started/quickstart.md) | [CLI](ja/reference/cli.md) · [オプション](ja/configure/options.md) · [Lua API](ja/extend/lua-api.md) |

## For users

| Topic | English | 简体中文 | 日本語 |
|---|---|---|---|
| Install & setup | [Installation](en/get-started/installation.md) | [安装](zh/get-started/installation.md) | [インストール](ja/get-started/installation.md) |
| Workflows & troubleshooting | [Workflows](en/get-started/workflows.md) · [Troubleshooting](en/get-started/troubleshooting.md) | [工作流](zh/get-started/workflows.md) · [故障排查](zh/get-started/troubleshooting.md) | [ワークフロー](ja/get-started/workflows.md) · [トラブルシューティング](ja/get-started/troubleshooting.md) |
| Terminal UI & sessions | [TUI](en/use/tui.md) · [Sessions](en/use/sessions.md) | [终端界面](zh/use/tui.md) · [会话](zh/use/sessions.md) | [ターミナル UI](ja/use/tui.md) · [セッション](ja/use/sessions.md) |
| Tools, shell & jobs | [Tools and shell mode](en/use/tools.md) | [内置工具与 Shell](zh/use/tools.md) | [ツールとシェルモード](ja/use/tools.md) |
| Skills & prompt templates | [Skills & templates](en/use/skills.md) | [技能与提示词模板](zh/use/skills.md) | [スキルとテンプレート](ja/use/skills.md) |
| Subagents & side threads | [Subagents & `/side`](en/use/subagents.md) | [子代理与旁路对话](zh/use/subagents.md) | [サブエージェントとサイドスレッド](ja/use/subagents.md) |
| Snapshots, undo & review | [Checkpoints & review](en/use/checkpoints-and-review.md) | [文件快照与代码审阅](zh/use/checkpoints-and-review.md) | [スナップショットとレビュー](ja/use/checkpoints-and-review.md) |
| Usage, compaction & profile | [Usage & compaction](en/use/usage-and-compaction.md) | [用量统计与上下文压缩](zh/use/usage-and-compaction.md) | [使用量とコンパクション](ja/use/usage-and-compaction.md) |
| Safety & permissions | [Permissions & sandbox](en/use/permissions.md) | [权限与沙箱](zh/use/permissions.md) | [権限とサンドボックス](ja/use/permissions.md) |
| Models, context & MCP | [Models](en/use/models.md) · [Context](en/use/context.md) · [MCP](en/use/mcp.md) | [模型](zh/use/models.md) · [上下文](zh/use/context.md) · [MCP](zh/use/mcp.md) | [モデル](ja/use/models.md) · [コンテキスト](ja/use/context.md) · [MCP](ja/use/mcp.md) |
| Local WebSocket / HTTP & CI | [Web listener](en/use/web.md) · [Headless & CI](en/use/headless.md) | [Web 服务](zh/use/web.md) · [无界面与 CI](zh/use/headless.md) | [Web リスナー](ja/use/web.md) · [ヘッドレスと CI](ja/use/headless.md) |
| Configuration & hooks | [Files](en/configure/files.md) · [Keymaps](en/configure/keymaps.md) · [Appearance](en/configure/ui.md) · [Hooks & notify](en/configure/hooks.md) · [Packages](en/configure/packages.md) | [配置文件](zh/configure/files.md) · [键位](zh/configure/keymaps.md) · [外观](zh/configure/ui.md) · [钩子与通知](zh/configure/hooks.md) · [包](zh/configure/packages.md) | [設定ファイル](ja/configure/files.md) · [キーバインド](ja/configure/keymaps.md) · [外観](ja/configure/ui.md) · [フックと通知](ja/configure/hooks.md) · [パッケージ](ja/configure/packages.md) |
| Environment variables | [Environment](en/reference/environment.md) | [环境变量](zh/reference/environment.md) | [環境変数](ja/reference/environment.md) |
| Runtime files | [Runtime files](en/reference/files.md) | [运行时文件](zh/reference/files.md) | [ランタイムファイル](ja/reference/files.md) |

## For plugin authors

| Topic | English | 简体中文 | 日本語 |
|---|---|---|---|
| Plugin model | [Plugins](en/extend/plugins.md) | [插件](zh/extend/plugins.md) | [プラグイン](ja/extend/plugins.md) |
| Step-by-step guide | [Writing a plugin](en/extend/writing-a-plugin.md) | [编写插件](zh/extend/writing-a-plugin.md) | [プラグインを書く](ja/extend/writing-a-plugin.md) |
| Services & events | [Services and events](en/extend/services-and-events.md) | [服务与事件](zh/extend/services-and-events.md) | [サービスとイベント](ja/extend/services-and-events.md) |
| Runtime guarantees | [Runtime guarantees](en/extend/runtime-guarantees.md) | [插件运行时承诺](zh/extend/runtime-guarantees.md) | [ランタイム保証](ja/extend/runtime-guarantees.md) |
| Host kernel & loader | [Kernel](en/extend/host-kernel.md) · [Loader](en/extend/host-loader.md) | [Kernel](zh/extend/host-kernel.md) · [Loader](zh/extend/host-loader.md) | [Kernel](ja/extend/host-kernel.md) · [Loader](ja/extend/host-loader.md) |
| Typed API & events | [Lua API](en/extend/lua-api.md) · [Event reference](en/reference/events.md) | [Lua API](zh/extend/lua-api.md) · [事件参考](zh/reference/events.md) | [Lua API](ja/extend/lua-api.md) · [イベントリファレンス](ja/reference/events.md) |

## For integrators

| Topic | English | 简体中文 | 日本語 |
|---|---|---|---|
| mare RPC 3.0 (JSON-RPC 2.0) | [RPC guide](en/integrate/rpc.md) · [RPC reference](en/reference/rpc.md) | [RPC 指南](zh/integrate/rpc.md) · [RPC 参考](zh/reference/rpc.md) | [RPC ガイド](ja/integrate/rpc.md) · [RPC リファレンス](ja/reference/rpc.md) |
| Agent Client Protocol | [ACP server](en/integrate/acp.md) | [ACP 服务器](zh/integrate/acp.md) | [ACP サーバー](ja/integrate/acp.md) |
| Event stream & exit codes | [JSON stream](en/integrate/json-stream.md) · [Exit codes](en/reference/exit-codes.md) | [JSON 事件流](zh/integrate/json-stream.md) · [退出码](zh/reference/exit-codes.md) | [JSON イベントストリーム](ja/integrate/json-stream.md) · [終了コード](ja/reference/exit-codes.md) |

## Architecture and contributor references

- [Subsystem design references](subsystems/README.md): crate boundaries, responsibilities, and sources of truth.
- [Channel and backpressure registry](channels.md): bounded channels, capacities, and full-queue rules.
- [Output-backed image resources](image-resources.md): artifact image caching, viewports, and RPC wire shape.
- [Unix sandbox acceptance matrix](unix-sandbox-matrix.md): platform backend checks for Seatbelt, bubblewrap, and Landlock.

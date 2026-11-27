# Overview

mare is an agent harness built as a small Rust core plus Lua 5.5 plugins that you can read, wrap, or replace.

## What mare is

The Rust core provides OS confinement, durable storage, the Lua runtime, and the terminal renderer (`mare-tui` and `mare-cli`). The agent loop, tools, model providers, compaction, permission rules, and UI components, themes, and statusline ship as built-in Lua plugins inside the binary.

![mare layered architecture](../../assets/diagrams/architecture.svg)

| Layer | Written in | What it does |
| --- | --- | --- |
| Core | Rust | Plugin kernel (events, services, lifecycle, watchdog), Lua runtime, OS sandbox, session storage, file snapshots, project trust, terminal renderer (`mare-tui`), mare RPC 3.0 (JSON-RPC 2.0) |
| Built-in plugins | Lua 5.5 | Agent loop, tools, providers, compaction, permissions, approvals, sessions, slash commands, UI components, themes, statusline, review, background jobs, MCP client |
| Your plugins | Lua 5.5 | Additions or overrides from `init.lua`, `~/.config/mare/plugins/`, `.mare/plugins/`, or installed packages |

Run `mare runtime` to print the read-only directory containing the built-in plugin sources (`agent.lua`, `tools.lua`, ...) and Lua type definitions.

## Replace, not patch

- Configure a plugin: `mare.plugin("tools", { bash_timeout_ms = 120000 })`
- Add a tool, slash command, key binding, or style in `init.lua`.
- Replace any built-in with a same-name plugin setting `override = true` (`/plugins` shows it as overriding the built-in).

Core boundaries:

- Core safety: Sandboxing, file/process permission checks, crash-safe JSONL writes, project trust, and the Lua watchdog (`watchdog_ms`) live in Rust. Replacing the agent loop preserves them for calls routed through the permissions plugin and guarded tool pipeline.
- Trusted plugins: Plugins run as trusted code. A plugin that disables or replaces `permissions`, or calls primitives directly outside a tool call, is not confined by those checks.
- Fault isolation: Failures in optional plugins or packages report a warning and are skipped; failures in essential services, plugins with `required = true`, or required packages block requests until fixed and `/reload`ed. `/plugins` shows tracebacks.

## One set of capabilities, five front ends

| Front end | Command | Purpose |
| --- | --- | --- |
| Terminal UI | `mare` | Interactive terminal sessions (`mare-tui` + `mare-cli`) |
| Headless | `mare -p "prompt"` or `mare --json "prompt"` | One-shot prompts and JSON streams for scripts and CI |
| stdio RPC | `mare --rpc` | Drive sessions over mare RPC 3.0 (JSON-RPC 2.0; `mare rpc schema`) |
| HTTP / WebSocket | `mare --web` | Authenticated HTTP routes and WebSocket RPC (web UI provided by plugins) |
| ACP | `mare acp` | Editor integration (Agent Client Protocol) |

In-process Lua (`init.lua` and plugins) runs in the extension layer shared by every front end.

## Scope and roadmap

Deferred to later releases: `ralph`, `lsp`, `model-router`, and `worktree` plugins, `/import`, and HTML/JSONL session export (implementable as third-party plugins in the meantime).

## Next steps

- [Installation](installation.md)
- [Quickstart](quickstart.md)
- [Workflows](workflows.md)
- [Plugins](../extend/plugins.md) and [Writing a plugin](../extend/writing-a-plugin.md)
- [RPC](../integrate/rpc.md) and [ACP](../integrate/acp.md)

# 斜杠命令

内置插件注册的斜杠命令。包和你自己的插件可以再加；在会话里用 `/commands` 查看当前可用的全部命令。

> 本页由 `scripts/gen_docs.py` 从代码生成，请勿手工修改。说明文字来自代码，保持英文。

| 命令 | 说明 | 别名 |
|---|---|---|
| `/add-dir` | Add a directory to the workspace (/add-dir &lt;path&gt;; no arg lists them) |  |
| `/agents` | Sub-agents, workflow members, jobs and other sessions |  |
| `/approval` | Independent approval policy: ask / never / untrusted |  |
| `/archive` | Archive this session and start a new one |  |
| `/btw` | Open or control an ephemeral side conversation (/btw [&lt;question&gt; \| back \| close]) |  |
| `/clone` | Duplicate this session into a new one |  |
| `/commands` | Browse slash commands; Enter runs one |  |
| `/compact` | Compact the conversation (/compact [focus]) |  |
| `/config` | Show options, their values and where each was set | `/settings` |
| `/context [all]` | Show what the context is made of (/context all: per item) |  |
| `/continue` | Continue an interrupted turn (after a crash or kill) |  |
| `/copy` | Copy the last response (/copy N: the Nth-last) — a code block or the whole Markdown |  |
| `/debug` | Show recent log lines and the latest crash report |  |
| `/delete [path]` | Delete this session, or delete a session file by path |  |
| `/diff` | File changes by turn |  |
| `/doctor` | Check the installation, credentials, plugins and MCP servers |  |
| `/effort [level]` | Set reasoning effort (/effort [level] [--session]; lists the model's levels) |  |
| `/exit` | Exit mare | `/q`, `/quit` |
| `/export` | Export the conversation as Markdown (/export [path]) | `/export-md` |
| `/fork [pick]` | Fork into a new session from your latest message (/fork pick: choose one) |  |
| `/help` | Show commands and keyboard shortcuts | `/?`, `/h` |
| `/hotkeys` | Show key bindings |  |
| `/init` | Analyze the codebase and create or update AGENTS.md |  |
| `/jobs` | Background jobs: view output or stop one |  |
| `/label` | Label the current point in the session tree (/label &lt;text&gt;; empty clears) |  |
| `/login` | Log in to a provider (/login [provider]) |  |
| `/logout` | Remove a provider's stored credentials (/logout [provider]) |  |
| `/lua [code]` | Run Lua in mare's running state (/lua [code]) (TUI local command) |  |
| `/mcp` | MCP servers (/mcp [filter], /mcp reconnect\|enable\|disable\|login\|logout\|tools &lt;server&gt;) |  |
| `/memory [add [--user] <text>]` | Show the instruction files (AGENTS.md) in effect; /memory add [--user] &lt;text&gt; |  |
| `/model [provider/id]` | Select model |  |
| `/name` | Name this session (/name &lt;title&gt;) | `/rename`, `/title` |
| `/new` | Start a new session | `/clear` |
| `/news` | Show the update notes of mare and the enabled plugins again |  |
| `/permission [read-only\|auto\|full-access\|log\|caps]` | Permission preset (read-only / auto / full-access) · /permission log · /permission caps | `/permissions` |
| `/plugins` | Manage plugins or show the runtime tree |  |
| `/profile` | Startup timeline and plugin load times |  |
| `/reload` | Reload plugins (/reload [plugin]) or the configuration (/reload config) |  |
| `/resume` | Resume a session (/resume all · /resume search &lt;text&gt;) | `/sessions` |
| `/retry` | Regenerate the last answer (the old one stays on its branch: /tree) |  |
| `/review [sha\|commit\|base\|changes\|instructions]` | Review code in a read-only session (/review [sha\|base &lt;branch&gt;\|&lt;instructions&gt;]) |  |
| `/rewind [turn]` | Restore files to before a turn (/rewind [turn]) |  |
| `/session` | Show session info (/session id copies the id) |  |
| `/show-reasoning` | Toggle showing full reasoning instead of “Thought for N s” |  |
| `/side` | Open or control an ephemeral side conversation (/side [&lt;question&gt; \| back \| close]) |  |
| `/skill:plugin-authoring` | Write a local Lua plugin tool for mare, register it in an a… |  |
| `/skills` | Browse skills; Enter loads one |  |
| `/status` | Show version, model, session and working directory |  |
| `/statusline` | Choose the status bar's segments (saved) |  |
| `/terminal-setup` | Check this terminal's keys (Shift+Enter, Alt+Enter) and show how to fix them |  |
| `/theme [flavor]` | Pick the theme flavor (auto / dark / light / installed flavors) |  |
| `/tree` | Navigate the session tree (branch from any earlier point) |  |
| `/undo` | Rewind the conversation to before a prompt (/undo picks one; /undo N: the Nth-last); files stay — /rewind restores them |  |
| `/usage` | Token usage, cost and subscription quota for this session | `/cost` |
| `/web` | List or switch search and fetch backends (/web [mode\|search\|fetch &lt;val&gt;]) |  |
| `/yolo` | Switch to full-access (no sandbox; explicit prompt rules and dangerous-command checks still ask) — /permission full-access |  |

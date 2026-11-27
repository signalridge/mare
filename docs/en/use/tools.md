# Tools and shell mode

Every built-in tool in mare is registered by Lua plugins (`tools`, `tools-apply-patch`, `jobs`, `web`, `subagent`, `permissions`) and executed through one guarded pipeline that validates arguments, checks policy, enforces the OS sandbox, and captures file snapshots for mare's file tools (`write`, `edit`, `apply_patch`; shell subprocess side effects are not snapshotted).

## Built-in tools

| Tool | Plugin | Purpose |
|---|---|---|
| `read` | `tools` | Read UTF-8 text files (with `offset`/`limit`), directory listings, or images (`png`, `jpg`, `jpeg`, `gif`, `webp`). |
| `write` | `tools` | Create or overwrite files, capturing a snapshot before overwrite. Writes atomically for ordinary files, and in place (`atomic = false`) when the file has multiple hard links so the other links stay linked. |
| `edit` | `tools` | Exact string replacement (`old_string`, `new_string`, `replace_all`). Active when `edit_mode = "replace"`, or under `"auto"` when neither the model's `edit_tool` nor the provider default selects `apply_patch`. |
| `apply_patch` | `tools-apply-patch` | Multi-file patch format (`*** Begin Patch` / `*** End Patch`). Active when `edit_mode = "apply_patch"`, or under `"auto"` when selected by the model's `edit_tool` or provider default (built-in matching covers GPT and Codex). Explicit `edit_mode` always wins. |
| `ls` | `tools` | List directory entries up to `ls_limit` (default `500`), including dotfiles. |
| `bash` | `tools` | Run commands in a persistent or job-tracked shell inside the OS sandbox. |
| `powershell` | `tools` | Run PowerShell commands on Windows (`powershell = "auto"` by default, or `"replace"` to swap in for `bash`). |
| `job_output`, `job_list`, `job_write`, `job_kill` | `jobs` | Read, list, feed stdin/EOF to, or stop background jobs. |
| `web_search`, `web_fetch` | `web` | Search the web or fetch HTTP(S) pages (fetched via `web-fetch-http` `http-text` backend or provider-native search). |
| `tool_search` | `tools` | Discover deferred tools (such as deferred MCP tools) and inspect their input schemas by keyword or `select:<name>`. |
| `call_tool` | `tools` | Invoke a deferred tool by name and arguments on models without native mid-conversation tool updates (`tool_update = "addition_only"`), keeping `tools[]` fixed for prompt caching. |
| `subagent`, `subagent_*` | `subagent` | Spawn and manage child agents (see [Subagents and side threads](subagents.md)). |
| `request_permissions` | `permissions` | Request additional read/write paths or network hosts with a justification and `turn` or `session` scope (see [Permissions and sandbox](permissions.md#requesting-additional-permissions)). |

### Read-before-edit and edit modes

The `tools` plugin enforces `read_before_edit = true` by default: `edit`, `apply_patch`, and overwriting `write` refuse to touch an existing file unless the session read it first, and fail if the file changed on disk since that read.

Switch edit tools in `init.lua` or on the command line:

```lua
mare.plugin("tools", {
  edit_mode = "auto",        -- "auto" | "apply_patch" | "replace"
  read_before_edit = true,
  bash_timeout_ms = 120000,
  max_lines = 2000,
  max_bytes = 50 * 1024,
})
```

### Disabling or adding command tools

Hide built-in tools or register shell-backed tools directly in `init.lua`, or exclude tools per invocation on the CLI with `--exclude-tools` (repeatable or comma-separated, supporting `*` globs, e.g. `mare --exclude-tools bash,write` or `mare --exclude-tools "mcp__*"`):

```lua
mare.tools.disable({ "ls" })

mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

Commands registered with `mare.tool.command` inherit the credential-scrubbed environment (`KEY`, `PASSWORD`, `SECRET`, `TOKEN`, and inherited `MARE_*` variables stripped) and honor the `tools` plugin's `env` settings: `env.include` keeps named inherited variables across the scrub, `env.exclude` drops additional names, and `env.set` assigns values after scrubbing. mare also injects runtime context variables (`MARE_CWD`, `MARE_SESSION_ID`, `MARE_SESSION_MODEL`, `MARE_SESSION_EFFORT`, and `MARE_PEER_SOCKET` when active). Without `readonly = true`, mare treats them as having side effects.

## Web search and fetch

The `web` and `web-fetch-http` plugins provide `web_search` and `web_fetch`. `web-fetch-http` blocks URLs with embedded credentials as well as private, loopback, link-local, and metadata IP addresses (even under `auto`), and returned content is wrapped with an untrusted-data notice before reaching the model.

- Inspect or switch backends inside a session with `/web`:
  - `/web` shows the current mode (`cached` by default) and available backends.
  - `/web mode disabled|cached|indexed|live` sets the search mode.
  - `/web search <id|auto>` and `/web fetch <id|auto>` pick a backend (`http-text` is built in for fetch).

Configure limits in `init.lua`:

```lua
mare.plugin("web", {
  web_search = "cached",
  search_timeout_ms = 30000,
  fetch_max_output_chars = 200000,
})
```

## `@file` references and image attachments

### `@file` completion (`file-ref`)

Type `@` in the prompt editor to fuzzy-complete paths across the working directory and any extra workspace roots added via `--add-dir <DIR>` at startup or `/add-dir` in a session (`@"path with spaces"` for paths containing spaces). `--add-dir` is repeatable and accepts existing absolute physical directories outside host-protected and project-protected paths (for ACP, pass `additionalDirectories` instead of `--add-dir`).

- Default limits: `100 KiB` per file (`max_bytes`) and `256 KiB` total per message (`max_total_bytes`).
- Directories attach as entry listings.
- Unreadable, binary, denied, or oversized paths are never dropped silently: mare attaches a one-line note telling the model why and pointing to `read`.

### Image attachments (`images`)

Paste an image from the clipboard with `Ctrl+V` (`Alt+V` on Windows) or let the model read an image file via `read`. On terminals supporting Kitty, iTerm2, or sixel protocols, the `images` plugin renders them inline; other terminals display `[image]`.

```lua
mare.plugin("images", {
  user = true,
  read = true,
  tools = true,
  max_rows = 12,
  max_cols = 60,
})
```

## Output spilling (`spill`)

- Shell command logs: When `bash` output exceeds `shell_buffer_bytes` (`1 MiB`) or the truncation limit (`2000` lines / `50 KiB`), full output streams to `~/.local/state/mare/spill/bash-*.log` and the truncated tool result cites that path. Logs from exited processes are cleaned up automatically.
- Session-scoped `spill` service: Tools, renderers, and RPC hosts store private text or binary blobs (up to `8 MiB` per object) via `spill.save` / `spill.read`, scoped to the owning `session_id` and pruned after `cleanup_period_days` (default `30`).

## `!` shell mode and background jobs

### Interactive shell mode (`shell-mode`)

Start a line with `!` on an empty input to run a command yourself using the same shell, `prelude`, and `MARE_*` environment as `bash`, without an approval prompt:

```text
!git status --short      # runs command; adds command and output to model context
!!cat .env.example       # runs command; shows output in scrollback, keeps it out of context
```

Press `Esc` or `Ctrl+C` to cancel a running `!` command, or `Backspace` on an empty shell line to leave shell mode. Model-visible output is middle-truncated to `2000` lines / `50 KiB`.

### Background jobs (`jobs`)

Long-running shell commands can run in the background without blocking your conversation:

- Press `Ctrl+B` while a foreground command runs to move it to the background.
- The model can start commands with `run_in_background = true` (and `tty = true` or `stdin = true` for interactive processes) and monitor them via `job_output` (including `until = "<regex>"` or `tail = N`), `job_write`, and `job_kill`.
- Run `/jobs` to list running and finished jobs, inspect their live tail (`view_tail_bytes`), or stop one. When a background job finishes, mare notifies the agent with up to `notify_inline_bytes` (2048 bytes) of output.

```lua
mare.plugin("jobs", {
  max_running = 8,
  max_jobs = 64,
  kill_grace_ms = 3000,
})
```

## Related

- [Permissions and sandbox](permissions.md) for OS confinement and `bash` rules
- [MCP servers](mcp.md) for external tools and `tool_search` deferred loading
- [Output-backed image resources](https://github.com/signalridge/mare/blob/main/docs/image-resources.md) for artifact image caching

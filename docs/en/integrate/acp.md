# ACP server

`mare acp` runs mare as an Agent Client Protocol server over stdio, so an editor that speaks ACP can start it and chat with it in a panel. This page shows how to register mare, what the server implements, and where it stops short of the full protocol.

## Register mare in an editor

An ACP client starts the agent as a subprocess and talks JSON-RPC over its stdin and stdout. Every client needs the same two things, a command and its arguments:

```json
{
  "command": "mare",
  "args": ["acp"]
}
```

Where that block goes depends on the editor. Look for its setting for custom or external ACP agents and enter `mare` as the command and `acp` as the argument. Make sure `mare` is on the `PATH` the editor sees; if it is not, give the full path.

`mare acp` waits for protocol messages on stdin and prints nothing until it gets one. `mare acp --help` lists its options.

## Options

`mare acp` accepts a subset of the startup options. Add them to `args`:

| Option | Effect |
|---|---|
| `-m`, `--model <MODEL>` | Model, for example `openai/gpt-6.1-terra`. |
| `--agent <NAME>` | Use a registered agent type for the initial session. |
| `-e`, `--effort <EFFORT>` | Reasoning effort. |
| `--tools <TOOLS>` | Enable only these tools (comma separated). |
| `--exclude-tools <TOOLS>` | Exclude these tools (repeatable or comma separated; supports `*`). |
| `--append-system-prompt <TEXT>` | Append text to the system prompt. |
| `--name <NAME>` | Name the session. |
| `--set <KEY=VALUE>` | Override a config value, such as `--set plugins.permissions.preset=read-only`. |
| `--strict-config` | Refuse to start when configuration warnings occur. |
| `--trust`, `--no-trust` | Override saved project trust for this run (saves nothing). |
| `--approve-mcps` | Approve project and `$MARE_MCP_CONFIG` MCP servers without prompting. |
| `--clean` | No user or project config, no user plugins, no packages. |
| `--startuptime <FILE>` | Write startup timing data to a file. |
| `-C`, `--cwd <CWD>` | Default working directory. |

For example, a read-only server pinned to one model:

```json
{ "command": "mare", "args": ["acp", "--model", "openai/gpt-6.1-terra", "--set", "plugins.permissions.preset=read-only"] }
```

The session flags (`-c`, `-r`, `--no-session`) and `--permission` are not options of `mare acp`. Use `--set plugins.permissions.preset=...` instead; the values are `read-only`, `auto` and `full-access` (see [Permissions](../use/permissions.md)).

## What is implemented

The server implements these messages:

| Message | Kind | Behavior |
|---|---|---|
| `initialize` | request | Returns the lower of the client's requested protocol version and the server's `ProtocolVersion::LATEST`; advertises HTTP MCP, image and embedded-context prompts, `loadSession`, and session list, resume and close. |
| `authenticate` | request | The server does not require authentication and returns an empty success response. |
| `session/new` | request | Starts a new mare session in the `cwd` the client sends and returns its id. |
| `session/load` | request | Reopens a saved, inactive session in its original `cwd` and replays its history via `session/update` before responding. For compacted sessions, only the compacted context is replayed. |
| `session/resume` | request | Reopens a saved, inactive session in its original `cwd`. |
| `session/list` | request | Lists saved, inactive sessions, optionally filtered by `cwd`. |
| `session/close` | request | Closes an active session. |
| `session/set_config_option` | request | Changes a session's model or reasoning effort. |
| `session/prompt` | request | Runs one turn and returns the stop reason when it ends. |
| `session/cancel` | notification | Aborts the running turn of that session. |
| `session/request_permission` | server request | Asks the client to approve a tool or network operation. Options are `allow-once`, `reject-once`, and `allow-always` when offered. |

Each `session/new` creates a separate mare host for the given `cwd`, so several sessions in different folders can run side by side. Project trust is decided per session workspace: ACP honors explicit `--trust` and `--no-trust` flags first, then falls back to the saved decision in `trust.json` or `MARE_TRUST_DEFAULT` (see [Permissions](../use/permissions.md)). ACP never prompts interactively for project trust. Until a folder is trusted, its project plugins, skills and prompts do not load. Record a decision once with `mare trust` in that folder, or pass `--trust` / `--no-trust` explicitly.

The `mcpServers` supplied with `session/new`, `session/load`, or `session/resume` are accepted for HTTP(S) and stdio transports; stdio commands must be absolute paths. SSE transport is not supported. See [MCP](../use/mcp.md) for mare's own MCP configuration.

### Prompts

Prompts accept text blocks, embedded text resources, resource links and images in their original order. Resource links are represented as text, and adjacent text is joined with a blank line. Audio and embedded binary resources are refused.

Image blocks are refused with an invalid-params error when the session's current model does not declare image input. `initialize` advertises image support for the connection because the model is selected per session and can change; image support is checked for each prompt containing images, since mare cannot decide this flag from a model at initialization before a session host exists.

A turn ends with one of these stop reasons:

| Stop reason | When |
|---|---|
| `end_turn` | The turn finished. |
| `cancelled` | `session/cancel` aborted it. |
| `max_tokens` | The last assistant message stopped because of length. |
| `max_turn_requests` | The turn reached `agent.max_turns`. |
| `refusal` | The provider refused or filtered the response. |

A provider error or persistence failure is returned as a JSON-RPC internal error, never as a successful `end_turn`. An unknown session id returns JSON-RPC error `-32002` (Resource not found).

### Streaming updates

While a turn runs, mare sends `session/update` notifications:

| ACP update | From |
|---|---|
| `agent_message_chunk` | Assistant text deltas. |
| `agent_thought_chunk` | Reasoning deltas. |
| `tool_call` | A tool call starting. `status` is `in_progress`, `title` is the tool name plus its `path` or `command` argument, `rawInput` is the arguments. |
| `tool_call_update` | A tool call finishing. `status` is `completed` or `failed`, and the content is the tool's text output. |
| `usage_update` | Emitted when `usage/changed` reports `last_context` and a positive `context_window`, carrying `used` and `size`. |

The `kind` of a tool call is derived from kernel tool metadata (`execute` when `command_tool` is set, `read` / `edit` from `side_effect`, and `other` otherwise or when metadata is unavailable during history replay).

## Limitations

- **No session modes.** The server does not advertise ACP session modes and does not implement `session/set_mode`.
- **Additional directories validation.** `session/new` accepts `additionalDirectories` (existing physical directories outside `cwd`, revalidated on resume). `session/load` and `session/resume` accept recorded `additionalDirectories` that pass revalidation and refuse unrecorded or invalid directories with `Invalid params`.
- **No audio prompts.** Audio blocks and embedded binary resources are refused.
- **No client file or terminal methods.** mare reads and writes files and runs commands itself. It does not call the editor's `fs/*` or `terminal/*` methods.
- **No authentication required.** `initialize` returns no auth methods, and `authenticate` returns an empty success response.

For programmatic control with approvals, host tools and the full session API, use the [RPC](rpc.md) interface.

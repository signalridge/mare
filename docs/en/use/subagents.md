# Subagents and side threads

mare supports two ways to isolate work from your main conversation: persistent child agents (`subagent`) that execute tasks in separate threads, and ephemeral side threads (`/side`, `/btw`) for quick questions that never enter the session log.

## Subagents

The `subagent` plugin lets the model spawn independent child sessions with their own session context; by default they inherit the parent working directory unless `cwd` is set, with per-type or per-call tool allowlist and model override. Child threads are tracked by the host's `AgentRegistry` (`crates/mare-host`), bounded by core `agents` limits, and persisted under `sessions/<project>/agents/<run>/<seq>-<label>.jsonl`.

### Tools exposed to the model

| Tool | Role |
|---|---|
| `subagent` | Start a child agent with `prompt`, optional `type`, `fork` (`"none"`, `"all"`, or turn count), `name`, `cwd`, `model`, `effort`, `tools`, `system`, and `wait` (`false` by default). |
| `subagent_wait` | Block until a child finishes (or until `timeout_ms`), returning its final output. |
| `subagent_steer` | Send a steering message into a running child agent. |
| `subagent_cancel` | Cancel a subagent and its descendants. |
| `subagent_list` | List subagents spawned in the current session with their ids, names, statuses, and tasks. |

By default, `subagent` starts the child in the background. When it finishes, mare delivers a one-line completion summary (`[subagent <id> finished: <status>...]`) to the parent, attaching a `<subagent-output agent="…">` block only when the child produced output or error text. Set `wait = true` to block on the result inline.

A background subagent belongs to the mare process that started it. If that process exits or crashes before the subagent finishes, resuming the session marks it `interrupted`: `subagent_list` and `subagent_wait` report that status, and the model gets one notice naming the subagent and its transcript so it can start the work again.

### Built-in subagent types

| Type | Purpose | Tool scope |
|---|---|---|
| `general` | General delegated implementation or investigation work | Inherits available tools and permissions from the parent |
| `explore` | Inspect code and locate evidence without modifying files | `read`, `ls`, `bash` (read-only sandbox) |
| `plan` | Design a concrete implementation plan without modifying files | `read`, `ls`, `bash` (read-only sandbox) |

### Custom subagent types

Define reusable agent types as Markdown files with YAML frontmatter in `~/.config/mare/agents/*.md` or (for trusted projects) `.mare/agents/*.md`:

```markdown
---
name: security-auditor
description: Audits changes for auth, path traversal, and secret leaks.
when_to_use: Use before merging changes to auth, RPC, or sandbox code.
tools:
  - read
  - ls
model: anthropic/claude-sonnet-5-5
effort: high
---

You are a read-only security reviewer. Report concrete findings with file and line numbers.
```

Frontmatter fields: `name`, `description` (required), `when_to_use`, `tools`, `disallowed_tools`, `model`, and `effort`. The Markdown body becomes the agent's system prompt.

> `tools` filters tool names only; custom subagents inherit the parent's permission ceiling. To enforce a read-only boundary, omit mutating tools such as `bash` or launch with `--permission read-only` (for example `mare --permission read-only --agent security-auditor`; built-in `explore`, `plan`, and `/review` set `permission = "read-only"`).

Add extra search paths in `init.lua` (project-relative directories require trust):

```lua
mare.plugin("subagent", {
  extra_agent_dirs = { ".mare/custom-agents" },
  notify_inline_bytes = 4096,
})
```

### Limits and approvals

Root-shared thread limits are configured in `mare.setup`:

```lua
mare.setup {
  agents = {
    max_active = 16, -- total live descendant threads across the tree (0 disables delegation)
    max_depth = 4,   -- maximum nesting depth from the root session
  },
}
```

When a subagent requests an approval that leaves its sandbox, the `agent-approvals` plugin forwards the request to the parent session's UI (`agent/approval-dialog`). A queue line below the input shows pending child approvals in arrival order, and the dialog names the requesting child.

#### Permission inheritance and parent changes

- **Spawn-time snapshot**: a subagent inherits the parent's live effective permission preset, sandbox policy, and approval policy at the moment it starts, and that snapshot becomes its upper bound (`ceiling`). A `general` subagent started after the parent switches to `full-access` (`FULL`) inherits `full-access` with no sandbox ceiling; explicit read-only agents (`explore`, `plan`, `/review`, or `permission = "read-only"`) always narrow to `read-only` regardless of the parent's preset.
- **Parent narrows while running**: switching the parent to a stricter preset or sandbox policy (or revoking project trust) immediately tightens every running descendant subagent and clears stale grants.
- **Parent elevates while running**: switching the parent to a wider preset (such as `auto` or `full-access`) does **not** automatically widen subagents that are already running. Already-running subagents keep the ceiling they started with and continue routing out-of-sandbox requests through the parent approval prompt; subagents started *after* the parent switches inherit the wider preset.

### Inspecting subagents with `/agents`

Run `/agents` (or press `Left` on an empty input) to open the agent list. It shows active subagents, workflow members, background jobs, and (in the Peers tab) other running `mare` sessions on the machine.

- Press `Enter` on a subagent to view its live transcript read-only.
- Press `Alt+Left` / `Alt+Right` to cycle between subagent views.
- Press `Esc` to return to the list, or `Ctrl+G` to jump to `/resume`.

## Side threads (`/side` and `/btw`)

The `conversations` plugin runs ephemeral, parent-isolated side conversations branched from the committed history of your current session. Side threads never write messages to the session JSONL file.

```text
/side why did the test fixture skip line 42?
/btw what does EADDRINUSE mean here?
```

`/side` and `/btw` share the same entrypoint:

| Command | Action |
|---|---|
| `/side` or `/btw` | Open an empty side thread widget above the editor |
| `/side <question>` / `/btw <question>` | Open a side thread and send the question immediately |
| `/side toggle` | Show or hide the active side thread |
| `/side back`, `/side close`, or `Esc` | Close and discard the side thread |

While a side thread is open, prompts you type in the input go to the side thread rather than the main agent, and the widget header displays the parent agent's live state (`idle`, `working`, or `blocked`). In the input box, only safe inspection commands (`/side`, `/btw`, `/help`, `/status`, `/session`) run inside a side thread, and any tool calls executed by the side loop run under the `ephemeral-read-only` tool execution lifetime. Token usage and cost from side requests are still counted by the `usage` plugin.

## Related

- [Permissions and sandbox](permissions.md) for how child approvals and sandbox inheritance work
- [RPC peer sockets](../integrate/rpc.md#peer-sockets) for peer sockets and `mare peer send`
- [Options](../configure/options.md) for `agents` configuration limits

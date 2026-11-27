# Permissions and sandbox

How mare decides what the agent may do: permission presets, approvals, rules, project trust and the operating-system sandbox, including what is not protected.

Two mechanisms work together. The sandbox is enforced by the operating system and limits what a spawned process can write, read and reach over the network. Approvals decide when mare asks you before a tool call that goes beyond the sandbox. Both are provided by the built-in `permissions` plugin; if you disable that plugin, mare runs with full access.

## Presets

A preset sets the sandbox level and the approval behaviour together. Choose one per session.

| Preset | Status bar | Sandbox | Asks |
| --- | --- | --- | --- |
| `read-only` | `READ` | Read everywhere. No writes. Network blocked (not on the default Windows tier, which cannot block it; see the sandbox section below). | Before a file write, and before anything that needs more than the sandbox gives. |
| `auto` (default) | `AUTO` | Write in the workspace and temporary directories. Network open unless you turn it off. | When the model asks to leave the sandbox, and for tools that are not confined by it. |
| `full-access` | `FULL` | None. | Explicit `prompt` rules, unanalyzable commands when a restrictive rule applies in scope, and dangerous commands (below). |

```sh
mare --permission read-only          # for this run
mare --yolo                          # same as --permission full-access
```

```lua
-- ~/.config/mare/init.lua
mare.plugin("permissions", { preset = "auto" })
```

Inside a session, `/permission` opens a picker and `/permission read-only` (or `auto`, `full-access`) switches directly. `/yolo` switches to `full-access`; mare asks you to confirm. `/permission log` shows what was decided and why, and `/permission caps` shows what the sandbox backend on this machine can enforce. The session records its preset, so resuming restores it.

In an untrusted project (see below), an explicit CLI flag preset (`--permission` or `--yolo`) still wins first; otherwise mare starts in `untrusted_preset`, whose default is `read-only`, instead of the configured `preset` (including values set via `--set plugins.permissions.preset=...`), and downgrades to that ceiling if project trust is revoked.

In `auto` and `read-only`, shell commands run inside the sandbox without asking. If a command fails because the sandbox denied something, the model can retry it with a request for the narrowest extra right that the denial names: network access (to a host), write access to a path, or everything. That retry asks you. An approval applies to that one call and is never retried automatically.

The default Windows tier cannot control the network, and mare accepts that gap without refusing. To keep the `read-only` promise as far as it can, mare asks before every shell or command tool (`info.command_tool == true`, covering `bash`, `powershell`, and `mare.tool.command` tools) unless that exact unrestricted access (`all` / `danger-full-access`) was already approved or allowed by rule. For strict refusal instead, set `accept_partial` explicitly (see below).

### Customizing `auto`

```lua
mare.plugin("permissions", {
  preset = "auto",
  writable = { "~/.cache/pip", "/data/scratch" },   -- extra writable directories (must exist; use physical paths, not symlinks)
  deny_read = { "/home/alice/.ssh" },               -- literal absolute paths the sandbox hides from reads
  network = { default = "deny", allow = { "github.com", "*.npmjs.org" } },
})
```

Relative extra writable directories are resolved against the working directory. A directory that does not exist, or is not a directory, is skipped with a notice and flagged by `mare doctor`. A directory that exists but is a symlink, or lies behind one, is refused by the sandbox policy instead of being skipped; use its physical path. `network` is `true` (open), `false` (blocked) or a table with `allow`, `deny`, `default` and `allow_local`. A table is enforced through a local filtering proxy; see network policy below.

## Approvals

When a tool call needs your decision, mare shows a dialog titled `Allow <tool>?` with the reason, the working directory and, for edits, a preview of the change.

| Choice | Key | Meaning |
| --- | --- | --- |
| Allow once | `y` | Run this call. |
| Allow for this session | `a` | Run this call, and the same call again without asking, for the rest of the session. For shell commands "the same" means the same command, directory and requested scope: approving `git push` does not approve `git push --force`. |
| Allow and remember | `A` | Save a rule and allow. For shell commands the rule is an argv prefix such as `git push`, saved in the rules file and used by every session. For other tools it allows all calls to that tool for this session. |
| Deny | `n` | Refuse this call. You are asked for an optional reason, which the model reads in the tool result. |
| Deny and stop | `Ctrl+C` | Refuse and end the turn. |

`Esc` denies the call and lets the turn continue. For a request for extra sandbox rights the choices are for this turn or for this session. Only one approval is shown at a time.

If the call comes from a sub-agent, the dialog names it. Sub-agents inherit the parent's effective preset and sandbox at spawn time: narrowing the parent tightens running sub-agents immediately, while widening the parent to a looser preset (such as `full-access`) resynchronizes unpinned sub-agents on continuation (`subagent_steer`) and auto-allows in-flight sandbox requests already covered by the parent's current preset, while explicit read-only sub-agents remain restricted (see [Subagents and side threads](subagents.md#permission-inheritance-and-parent-changes)).

### Requesting additional permissions

The `permissions` plugin also registers the built-in `request_permissions` tool (see [Tools and shell mode](tools.md)). Instead of failing on a sandbox denial and retrying, the model can call `request_permissions` up front with `additional_permissions` (`read` paths, existing physical `write` directories, or `network` boolean / host allowlist), a required `justification`, and `scope` (`"turn"` by default, or `"session"`).

You can grant all or only part of the request. The tool returns JSON (`{ permissions, scope }`) listing only newly approved or reused additional rights; access already present in the baseline preset is omitted, and a denied request grants nothing. Subsequent calls within that turn or session reuse the approved scope without prompting again. If the host sandbox baseline changes (`sandbox/changed`, for example after switching presets or updating the host policy), mare clears all transient turn and session grants and session approval caches so stale grants never carry over.

### Approval policy

The approval policy is separate from the preset. It decides what happens to a request that would ask:

```sh
mare --approval-policy never    # reject every request without prompting
```

- `ask` (default): prompt you.
- `never`: reject requests without prompting. The sandbox still applies.
- `untrusted`: ask before every tool call except those that only read, even where the sandbox would allow it.
- A table `{ mode = "granular", categories = { tool = true, network = true } }` allows asking only for the listed categories and rejects the rest.

Set it with `mare.plugin("approvals", { policy = "never" })` (RPC `options.set {session_id: "...", values: {"plugins.approvals.policy": "never"}}`, `--set plugins.approvals.policy=never`) or `/approval never`. A sub-agent can only tighten the policy it inherits.

### Dangerous commands

Some commands ask in every preset, including `full-access`: `rm -f` / `--force` and `find -delete` (found anywhere in the script, through wrappers), and on Windows `Remove-Item -Force`, `del /f` and `rd /s /q`. In headless runs nobody can answer, so they are denied. Only a rule that allows that exact command lets one pass without asking.

```lua
mare.plugin("permissions", {
  dangerous = { rules = { { "git", "push", "--force" } } },   -- add your own argv prefixes
})
```

## Rules

Rules fix a decision for matching commands. They are checked against the parsed command: plain words joined by `&&`, `||`, `;` or `|` are split into segments, and every segment must match an allow rule. A command with `$(...)`, backticks, expansions, subshells, redirections (except fixed sinks like `/dev/null`), `eval` or a nested shell cannot be analyzed, so no allow rule applies to it.

```lua
mare.plugin("permissions", {
  rules = {
    { argv = { "git", "status" }, decision = "allow" },
    { argv = { "git", "push" },   decision = "prompt" },
    { argv = { "rm", "-rf" },     decision = "forbidden", justification = "never delete recursively" },
    { tool = "my_unconfined_tool", decision = "allow" },
  },
})
```

`decision` is `allow`, `prompt` (always ask) or `forbidden` (always deny). When several rules match, the strictest wins: `forbidden`, then `prompt`, then `allow`. Optional fields are `cwd` (a glob such as `~/src/**`), `scope` (`network`, `write:<path>`, `all`), `deny_args` and `expires`. Rules saved with "Allow and remember" live in `~/.local/state/mare/permissions.json`, which several sessions share safely; edit it or set `rules_file` to move it.

## Project trust

A project can ship code and prompts that mare would load: `.mare/plugins`, `.mare/init.lua`, `.mare/skills`, `.mare/prompts`, `.mare/SYSTEM.md`, and `.mare/APPEND_SYSTEM.md` from the project resource root (nearest directory with `.mare/` or `.mcp.json`, up to the git root). Trust probing checks upward to the git root for `.agents/skills`; when loading skills, default relative skill directories are resolved once against `mare.project.root`. Project resources load only if you trusted the resource root (with no deny along the path to it). Decisions are stored in `~/.local/state/mare/trust.json`.

```sh
mare trust              # trust the current project; pass a directory with mare trust <dir>
mare trust --revoke     # stop trusting it
mare --trust          # trust it for this run only; saves nothing
mare --no-trust       # distrust it for this run, whatever was saved
```

The first time you open mare in a directory with such resources (or inside a git repository), the interactive TUI shows a trust overlay listing discovered resources by group (project config with a full-API warning for `.mare/init.lua`, plugins, skills, prompt templates, system prompt files, and sanitized MCP server names with executable basename or URL host only; press `e` to expand or collapse groups with more than six items). The overlay offers four choices: **Trust and load** (saved to `trust.json`), **Trust this session only** (loads project resources for the current session without saving), **Don't trust** (saved), and **Not now** (`Esc`, ask again next time). Headless and RPC runs never ask: untrusted resources stay unloaded and mare prints a note to stderr unless you pass `--trust` (trust for this run without saving) or saved trust already exists. The `trust_default` option (`ask`, `trust` or `distrust`) sets the default for directories with no saved decision.

Trust only covers loading project files. MCP servers from the project have their own approval; see [MCP](mcp.md).

## The operating-system sandbox

The sandbox confines the processes mare starts for tools (the `bash` tool, other command tools) and file access made by mare's own file primitives, using the session's current preset. There are three levels: `read-only`, `workspace-write` and `danger-full-access`. Inside `workspace-write` the writable roots are the working directory, extra workspace directories added with `--add-dir` or `/add-dir` (existing absolute physical directories that do not overlap host-protected or project-protected paths), the platform temp areas and `writable`. Inside the workspace, `.git` and `.mare` stay read-only for confined processes when the backend reports `fs_protect`; Landlock and the Windows tiers do not.

![Three-platform OS sandbox backends and probed capabilities](../../assets/diagrams/sandbox-backends.svg)

When a command escalates with `write:.git` inside a git linked worktree or submodule where `.git` is an existing pointer file (`gitdir: ...`), mare resolves the target `gitdir` strictly without symlinks instead of widening the surrounding workspace. For a valid linked worktree (verified `commondir`, `worktrees/` ancestry, `objects`/`refs`/`HEAD` layout, and matching `gitdir` backlink), `write:.git` grants write access to both the worktree's `gitdir` and the shared repository `commondir`; existing symlinked or malformed `.git` pointers and backlinks are rejected. If `.git` itself does not exist, `write:.git` follows the ordinary missing-file branch and requests write scope for its existing parent directory (which still requires the applicable approval).

| Platform | Backend | Notes |
| --- | --- | --- |
| Linux | bubblewrap (`bwrap`), then Landlock | `bwrap` must be installed and able to create namespaces. It mounts the host root read-only, with a private `/proc`, `/dev` and PID namespace, and binds the writable roots. Network is blocked with a network namespace; filtered networking uses a loopback-only namespace bridged to the proxy. If `bwrap` is unusable, Landlock is used: file writes and reads are restricted, and sockets are blocked with seccomp, but it cannot protect `.git` and `.mare` or provide domain filtering. |
| macOS | Seatbelt (`/usr/bin/sandbox-exec`) | A profile that denies file writes except the writable roots and can restrict reads, outbound network and Unix sockets. |
| Windows, default tier | Restricted token, Low integrity, ACL grants | Writes are confined. Reads and the network are not controlled, and mare reports this as partial. mare accepts the missing network control by default (see the paragraph on approvals above). |
| Windows, enhanced tier | Dedicated sandbox accounts, setup-time deny-read ACLs, firewall filters | Install once with `mare sandbox setup` (asks for elevation). Setup applies deny-read ACLs to credential and browser paths for the sandbox account, but `windows-user` does not declare per-call `fs_deny_read` (nor allowlist `fs_read`), so strict calls requesting explicit `deny_read` are refused unless partial protection is accepted. Network is filtered, but the filter's proxy ports are shared by all sandboxed calls, so per-call network isolation is not enforced and the sandbox does not report `net_block` or `net_proxy`. `mare sandbox status` runs a self-test; `--require-full` exits non-zero unless everything is enforced; because per-call network isolation is not, it currently exits non-zero even when the self-test passes. `mare sandbox setup --remove` undoes the setup. If the self-test fails, mare falls back to the default tier and reports why. |

mare probes each backend on demand and caches which capabilities actually work (`fs_read`, `fs_deny_read`, `fs_write`, `fs_protect`, `fs_host_protect`, `net_block`, `net_proxy`, `unix_socket`). A preset that needs a capability the backend lacks is refused with `SANDBOX_INSUFFICIENT`, naming it, instead of running weaker. If no backend works, commands are refused with `SANDBOX_UNAVAILABLE`; mare never runs them unconfined silently. The default exceptions are a missing `fs_protect`, which is accepted and reported as partial on every platform, and a missing network control on the default Windows tier. To accept other gaps explicitly:

```lua
mare.plugin("permissions", { accept_partial = { "network" } })
```

An explicit `accept_partial = {}` means strict refusal. Run `mare doctor` to see the backend and capabilities for your machine, or `/permission caps` in a session.

### Network policy

In `auto`, `network = true` leaves the network open for sandboxed commands (the built-in `web_fetch` HTTP client still blocks private, loopback, link-local, and metadata IPs by default). `false` blocks it. A table with `allow` / `deny` hosts and `default` sends traffic through a local proxy that filters by host; the sandbox lets the command connect only to that proxy. Unix sockets are blocked while the network is restricted; per-socket exceptions (`unix_sockets`) apply only on backends that support them (Linux's seccomp `AF_UNIX` path does not support per-socket allowlists). Setting proxy environment variables alone never counts as enforcement.

## What is not protected

- Reads are open by default: under `auto` and `read-only`, a confined command can read any file your user can (including `~/.ssh`) unless listed in `deny_read` or hidden by the Windows enhanced tier's defaults. Anything read can be sent to the model provider.
- `auto` allows outbound network unless you change `network`.
- `full-access` and approved unconfined tool calls run without a sandbox. Under `auto`, tools outside the local sandbox (such as MCP tools) ask before running.
- Landlock and the Windows default tier do not protect `.git` or `.mare`; the Windows default tier does not control reads or network.
- The sandbox is not a VM: it shares your kernel, user, and home directory. Approving `write:.git` lets a command edit git hooks that execute outside the sandbox.
- Direct `!cmd` shell commands bypass approval prompts.
- Prompt injection is not solved by sandboxing: read approval prompts before pressing `y`.
- Shell side effects cannot be reverted with `/rewind`; see [Sessions](sessions.md).

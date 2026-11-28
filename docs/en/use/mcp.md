# MCP servers

Connect Model Context Protocol servers to mare: where to configure them, how tools reach the model, how project servers are approved and OAuth login.

The `mcp` plugin is built in and on by default. It does nothing until you configure a server. It supports stdio servers (a command mare starts) and streamable HTTP servers (a URL). The legacy SSE transport is not supported; use the server's streamable HTTP endpoint, commonly `/mcp` instead of `/sse`.

## Configuring servers

Servers can come from five places. Later sources override earlier ones for a server of the same name.

| Order | Source | Notes |
| --- | --- | --- |
| 1 | `servers` in `mare.plugin("mcp", { ... })` in `init.lua` | |
| 2 | `~/.config/mare/mcp.json` | User file. |
| 3 | The file named by `$MARE_MCP_CONFIG` | Needs approval (below). |
| 4 | `.mcp.json` in the project resource root | Project file (nearest `.mare/` or `.mcp.json`, up to the git root; may be in an ancestor of the working directory); needs approval. |
| 5 | `.mare/mcp.json` in the project resource root | Project file in the same project resource root; needs approval. |

JSON files use the common layout, `{ "mcpServers": { ... } }` (`mcp_servers` and `servers` are also read), and accept the camelCase spellings used by other clients (`enabledTools`, `bearerTokenEnvVar`, `startupTimeoutMs` and so on).

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" }
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": { "x-team": "platform" }
    }
  }
}
```

The same in `init.lua`:

```lua
mare.plugin("mcp", {
  servers = {
    github = {
      command = "npx", args = { "-y", "@modelcontextprotocol/server-github" },
      env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" },
      tier = "direct",
    },
    linear = {
      url = "https://mcp.linear.app/mcp", oauth = true,
      tools = { delete_issue = "hidden" },
    },
  },
})
```

Server names may contain letters, digits, `_` and `-`. Strings expand `${VAR}`, `${env:VAR}` and `${VAR:-default}`; a variable that is not set is reported by `/mcp` and `mare doctor`.

A project entry that has no `command`, `url` or `type` is an override of the user's server with the same name: it can change only `enabled`, `tier`, `tools`, `enabled_tools`, `disabled_tools`, `description` and `required`.

### Server keys

| Key | Meaning |
| --- | --- |
| `command`, `args`, `env`, `env_file`, `cwd`, `env_include`, `env_exclude` | A stdio server. `env_file` is a dotenv file; `env` overrides it. |
| `url`, `headers`, `env_http_headers`, `bearer_token`, `bearer_token_env_var` | A streamable HTTP server. `env_http_headers` maps a header name to an environment variable. |
| `oauth` | `true`, or a table with `scopes`, `client_id`, `client_secret`, `client_name`, `callback_port`, `auth_server_metadata_url`. |
| `tls` | `{ ca_bundle = "..." }` for a private certificate authority. |
| `enabled` | `false` turns the server off. |
| `required` | `true`: a request waits for this server to connect, with no time limit; a failure denies the request with the reason. |
| `tier` | Exposure level; see below. |
| `tools` | Per-tool tier, for example `{ delete_issue = "hidden", ["search_*"] = "direct" }`. |
| `enabled_tools`, `disabled_tools` | Allow or deny lists, with `*` patterns. |
| `description` | Shown to the model to help it find the server's tools. |
| `startup_timeout_ms`, `call_timeout_ms` | Defaults 60,000 (at most 300,000) and 120,000. Progress notifications reset the call timeout. |

MCP plugin options are `tier` (default `deferred`), `startup_timeout_ms`, `call_timeout_ms` and `text_limit`. Set `plugins.tools.search_results` to change the default number of `tool_search` results (5). Results longer than `text_limit` (20 KiB) are cut in the middle, and the full text is saved to a private file named in the result.

## Approving project servers

A server from `$MARE_MCP_CONFIG`, `.mcp.json` or `.mare/mcp.json` does not start until you approve it, because opening a repository must not run its commands. The approval is remembered by server name plus a hash in `~/.local/state/mare/mcp-approvals.json`. The hash covers the server's execution, auth, transport, and exposure fields: `command`, `args`, `env`, `env_file`, `env_include`, `env_exclude`, `cwd`, `url`, `headers`, `env_http_headers`, `bearer_token`, `bearer_token_env_var`, `oauth`, `tls`, `type`, `tier`, `tools`, `enabled_tools`, `disabled_tools`, `enabled`, `required`, `description`, and any partial project overlays. Changing any of these fields invalidates the hash and asks for approval again. Stdio servers from a project also require the project to be [trusted](permissions.md).

- Interactive: `/mcp enable <server>`, or choose the server in `/mcp` and pick Enable.
- Headless: pass `--approve-mcps` to approve all of them. This writes the same remembered approvals to `mcp-approvals.json`, so later runs, headless or interactive, start those servers without asking again until the hash changes:

```sh
mare -p --trust --approve-mcps "list my open issues"
```

Without it, an unapproved server shows as `needs_approval` and its tools are not available.

## How tools reach the model

Each tool is registered as `mcp__<server>__<tool>`. A short hash is appended if the name is longer than 64 characters or collides with another tool. How many tools the model sees up front depends on the server's tier, which keeps the prompt small when you have many servers.

| Tier | Behavior |
| --- | --- |
| `deferred` (default) | The server is not connected at startup. The model discovers tools and their schemas with `tool_search`. On the first search, an uncached server connects to discover its tools; a cached list under `~/.local/state/mare/mcp-tools` avoids a connection until first use. Models with `tool_update = "addition_only"` (Anthropic Claude, OpenAI Responses `gpt-5.4` and newer, DeepSeek V4.1 Flash on Responses API, and Kimi K3) activate deferred tools mid-conversation and invoke them directly; all other models invoke discovered tools through the fixed `call_tool` proxy so the top-level `tools` array and prompt prefix cache stay byte-stable across turns. |
| `direct` | Connected in the background at startup, and tools are declared like built-in ones. A request never waits for it (unless `required`): tools that are still connecting join from the next turn. |
| `hidden` | Not available. |
| `codemode` | Callable only from a code-mode script. Needs a codemode plugin; otherwise treated as `deferred`. |

`tool_search` takes keywords (`github issues`) or exact names (`select:mcp__github__create_issue`, up to 32 tools per query), and returns matching tool schemas within a fixed result budget. The system prompt gets one line per server.

> **Prompt caching across all providers:** Deferred tool discovery keeps the system prompt and top-level `tools` array fixed across turns on every provider. Models with `tool_update = "addition_only"` (Anthropic Claude, OpenAI Responses `gpt-5.4`+, DeepSeek V4.1 Flash, Kimi K3) activate deferred tools natively; other endpoints (such as DeepSeek V4 Pro, GLM, Google Gemini, older OpenAI models, and Kimi K2) use the fixed `call_tool` proxy instead of mutating `tools[]`, preserving the prompt prefix cache. Set `tier = "direct"` (or mark individual tools `direct` in `tools`) if you prefer a small toolset to be declared directly up front.

If a server offers resources, the model also gets tools to list and read them, and server prompts appear as slash commands named `/mcp__<server>__<prompt>` (pass `name=value` arguments or positional words). Server elicitation requests show as a form in the TUI; headless runs always decline them.

### Approvals for MCP tools

MCP tools run in the server's process, outside mare's OS sandbox. In the `auto` preset every MCP tool call therefore asks first, including read-only ones. To skip the question for a tool you trust, add a rule:

```lua
mare.plugin("permissions", {
  rules = { { tool = "mcp__github__list_issues", decision = "allow" } },
})
```

Or choose "Allow and remember" in the dialog. See [Permissions](permissions.md).

## Managing servers

```sh
mare mcp list                 # connect to every enabled server; print state, tools and errors (exit 1 on required server problems)
mare mcp list-tools github    # a server's tools with their input schemas
mare mcp disable github       # turn a server off in this project
mare mcp enable github
mare mcp login linear         # OAuth login in the browser
```

`enable` and `disable` are remembered per project in `~/.local/state/mare/mcp-disabled.json`; they never edit your config files.

Inside a session, `/mcp` opens a panel of servers with their state (`connected`, `needs_login`, `needs_approval`, `disabled`, and others) and the actions that fit each one: show tools and schemas, reconnect, log in or out, enable or disable. The same actions work as subcommands: `/mcp login <server>`, `/mcp logout <server>`, `/mcp reconnect <server>`, `/mcp enable <server>`, `/mcp disable <server>` and `/mcp tools <server>`. `mare doctor` also checks MCP servers.

## OAuth

Set `oauth = true` (or a table of options) on an HTTP server, then log in once:

```sh
mare mcp login linear
```

mare opens a browser, completes the OAuth flow, stores the tokens, and reconnects the server with them. A server that needs a login shows `needs_login` until you do. `/mcp logout <server>` forgets the stored tokens. `login` applies only to HTTP servers configured with `oauth`.

mare finds the authorization server from the server's protected resource metadata, or from authorization server metadata at the server's origin. It refuses a login when that metadata is on another origin, when the server publishes none (set `auth_server_metadata_url` then), when the authorization server does not advertise PKCE `S256`, or when an authorization, token, registration or revocation endpoint is plain `http` on a host other than loopback.

For a server that takes a static token instead, use `bearer_token_env_var` so the token stays out of the file:

```json
{ "mcpServers": { "internal": { "url": "https://mcp.example.com/mcp", "bearer_token_env_var": "INTERNAL_MCP_TOKEN" } } }
```

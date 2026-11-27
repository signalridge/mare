# Quickstart

Log in to a provider, start `mare` in a repository, approve a tool call, and create `~/.config/mare/init.lua`. On first launch, you can pick optional plugins or skip and install them later via `/plugins` → Discover.

## Log in to a provider

mare needs one model provider. The built-in providers are:

| Provider name | Credential | Environment variable |
| --- | --- | --- |
| `openai` | ChatGPT login (browser or device code), or an API key | `OPENAI_API_KEY` |
| `anthropic` | API key | `ANTHROPIC_API_KEY` |
| `google` | API key | `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| `opencode-go` | API key | `OPENCODE_API_KEY` |
| `deepseek` | API key | `DEEPSEEK_API_KEY` |
| `kimi` | API key | `MOONSHOT_API_KEY` |
| `kimi-code` | API key | `KIMI_API_KEY` |
| `openrouter` | API key | `OPENROUTER_API_KEY` |
| `ollama` | Local server (`http://localhost:11434/v1`); no key required by default | — |
| `lmstudio` | Local server (`http://localhost:1234/v1`); no key required by default | — |

An API key in the environment variable works without logging in. `mare login` stores a credential in `~/.config/mare/auth.json` (mode `0600`); a stored login wins over the environment variable.

Log in to OpenAI with a ChatGPT account:

```sh
mare login openai
```

> Bare `mare login` tries the configured model's provider first. With no usable default, it opens a provider picker on a TTY and exits with code `2` without a TTY.

mare opens a browser. On a machine without one, for example over SSH, use a device code instead:

```sh
mare login openai --device-auth
```

For any provider that uses an API key, pipe the key in on stdin. mare never prompts for a key on the terminal:

```sh
printenv ANTHROPIC_API_KEY | mare login anthropic --with-api-key
```

See which providers are logged in and where each credential comes from:

```sh
mare login status
```

Remove credentials with `mare logout [provider]`.

You can also skip this step. When no provider has a credential, the interactive UI starts a guided first-run flow: pick a provider, log in and choose a default model. `/login [provider]` does the same from inside a session.

## Start in a project

```sh
cd ~/src/my-project
mare
```

Pass an initial prompt as arguments to start with a question already asked:

```sh
mare "what does this project do?"
```

### Trust a project

Some project files change what mare runs: `.mare/init.lua`, `.mare/plugins`, `.mare/skills`, `.mare/prompts`, `.mare/SYSTEM.md`, `.mare/APPEND_SYSTEM.md` and `.agents/skills`. mare loads them only in a trusted project. When you start in a directory that has any of them, or that is inside a git repository, and nothing is saved yet, a prompt appears:

| Choice | Effect |
| --- | --- |
| Trust and load | Remember this folder as trusted and load the project's files now |
| Trust this session only | Load the project's files for this session only; do not save to `trust.json` |
| Don't trust | Remember the decision; never load them |
| Not now (or Esc) | Load nothing this time and ask again next time |

The prompt groups discovered resources (project config with a full-API warning for `.mare/init.lua`, plugins, skills, prompt templates, system prompt files, and sanitized MCP server summaries). When any group has more than six items, press `e` to expand or collapse the full list. Trusting a directory permanently also trusts later changes to those files, so trust only code you would run. Decisions are stored in `~/.local/state/mare/trust.json` and apply to subdirectories. You can also decide from the shell:

```sh
mare trust            # trust the current project (or pass a directory: mare trust <dir>)
mare trust --revoke   # revoke it
mare --trust        # trust for this run only, save nothing
mare --no-trust     # distrust for this run only
```

## Ask a question

Type a question and press Enter. A good first one asks for a reading of the code:

```text
How is the configuration loaded? Point me at the files.
```

The agent reads files with its tools and answers. The tool calls appear in the transcript. Press Ctrl+O to expand or collapse tool output and thinking, and Ctrl+T for the full transcript.

Press Esc to interrupt a running turn. Press Enter while the agent is working to queue a message for when the turn finishes; see [Workflows](workflows.md#steer-or-queue-while-the-agent-works).

## Approve a tool call

The default permission preset is `auto`: the agent can write inside the workspace and temp directories without asking, and mare asks when a call would leave the sandbox. To see approvals right away, start with the `read-only` preset, where writing files needs your approval:

```sh
mare --permission read-only "add a one-line comment to the top of README.md"
```

> Outbound network access is blocked where the OS sandbox supports it. On the default Windows tier, which cannot block network access, mare asks before shell and other command tools unless that unrestricted access is already approved.

The approval box names the tool and offers these answers:

| Key | Answer |
| --- | --- |
| `y` | Allow once |
| `a` | Allow for this session |
| `A` | Allow and remember the proposed rule (shown when mare can propose one) |
| `n` | Deny, then tell the model why |
| Esc | Deny this call; the turn goes on |
| Ctrl+C | Deny and stop the turn |

Switch presets inside a session with `/permission` (or `/permission auto`). The three presets are `read-only`, `auto` and `full-access`; `--yolo` is shorthand for `--permission full-access`. See [Permissions](../use/permissions.md).

## Mention files and run commands

Type `@` and a path to attach a file to your message. Tab completes paths, and `@"a path with spaces"` quotes one:

```text
Explain the retry logic in @src/client.rs
```

Text files are attached up to 100 KiB each (256 KiB per message). A file that cannot be attached gets a one-line note saying why, and the agent can still read it with its read tool.

Start a line with `!` to run a shell command yourself. The command and its output go into the context, so the model sees them on its next turn. Use `!!` to run a command without adding anything to the context:

```text
!git status --short
!!ls -la
```

## Choose a model

`/model` opens a picker. Providers without credentials are grouped as needing login. Give a model reference directly to switch:

```text
/model openai/gpt-6.1-terra
```

`/effort` sets the reasoning effort for the active model. You can also pass `-m/--model` and `-e/--effort` at start-up (or set `MARE_EFFORT`). `effort` is invocation-only as a global override: global `effort` in `init.lua` is ignored, whereas `model` in `init.lua` overrides the choice saved in `~/.local/state/mare/state.json`.

`/status` shows the version, model, session, token use and each provider's login. `/help` lists every command.

## Resume a session

Every conversation is saved under `~/.local/state/mare/sessions`. Leave with Ctrl+D on an empty input, or press Ctrl+C twice. Then come back:

```sh
mare -c               # continue the most recent session in this directory
mare -r               # pick a session from a list
mare -r -2            # the second most recent session in this directory
mare -r my-name       # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original alone
```

Add `--all` to `-c` or `-r` to look at sessions from every directory. Inside the UI, `/resume`, `/new`, `/name <title>` and `/fork` do the corresponding things. More in [Workflows](workflows.md#continue-and-fork-sessions).

## Write your first init.lua

Run `mare init` to create `~/.config/mare/init.lua` with commented examples (an existing file is preserved), then edit it:

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6.1-terra",
}

-- give long shell commands more time
mare.plugin("tools", { bash_timeout_ms = 300000 })

-- /m is a shortcut for /model
mare.command.alias("m", "model")

-- a grep tool backed by ripgrep (grep and find are not built in)
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
}
```

Restart mare, or run `/reload config` inside a session. `mare.setup` warns about unknown option names and wrong types, with a "did you mean" hint; `/config` shows every option, its value and where it was set. A project's trusted `.mare/init.lua` runs after this file.

Next:

- [Workflows](workflows.md) for recipes.
- [Models](../use/models.md) and [Permissions](../use/permissions.md) for the choices you just made.
- [Options](../configure/options.md) and [Key maps](../configure/keymaps.md) for what else `init.lua` can set.

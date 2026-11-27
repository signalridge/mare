# CLI

Every `mare` command and flag, as printed by `--help`.

> This page is generated from the code by `scripts/gen_docs.py`. Do not edit it by hand.

## mare

```text
Agent harness (Rust core, Lua plugins)

Usage: mare [OPTIONS] [PROMPT]... [COMMAND]

Commands:
  plugin      Manage plugins
  acp         Run as an Agent Client Protocol server over stdio
  trust       Trust (or revoke trust for) a directory (default: current project root, git worktree root, or cwd)
  session     Sessions: create, list, fork, archive, unarchive, or delete saved sessions
  runtime     Print the runtime directory (read-only copies of builtin plugins and types)
  init        Create a default ~/.config/mare/init.lua and .luarc.json
  doctor      Check config, credentials, connectivity and plugins
  login       Log in to a provider; `mare login status` lists every provider
  logout      Remove a provider's stored credentials (all of them; tokens are revoked first)
  model       Model catalogue: `mare model refresh` re-reads every logged-in provider's model list
  completion  Print a shell completion script: `mare completion zsh > ~/.zfunc/_mare`
  mcp         MCP servers: list, list-tools, enable, disable, login
  peer        Talk to mare sessions over their peer sockets
  rpc         RPC protocol helpers
  sandbox     Windows enhanced sandbox: install, remove or inspect
  test        Run Lua *_spec.lua tests using the mare.test harness (cases in the same spec file share an isolated file-level Lua environment)
  token       Host runtime authentication token management
  web         Host HTTP/WebSocket listener helpers
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [PROMPT]...  Initial prompt (interactive) or the prompt to run (with -p)

Options:
  -p, --print
          Headless: run the prompt, print the final answer, exit
      --mode <MODE>
          Output mode: text (default), json (event stream), rpc (stdin/stdout JSON lines) [possible values: text, json, rpc]
      --json
          Shorthand for --mode json
      --stream-deltas
          With --json: also emit the streaming deltas (`item/delta`); without it the stream is item level (starts, messages, tool calls, usage)
      --output-schema <FILE>
          JSON Schema file that the final assistant output must conform to (headless runs: -p / --json)
      --rpc
          Bidirectional JSON-RPC 2.0 over stdin/stdout (same as --mode rpc; `mare rpc schema`)
  -m, --model <MODEL>
          Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>
          Use a registered agent type for the initial session
  -e, --effort <EFFORT>
          Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>
          Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>
          Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --add-dir <DIR>
          Add an extra directory to the workspace (repeatable; existing absolute physical directories only)
      --append-system-prompt <TEXT>
          Append text to the system prompt
  -c, --continue
          Continue the most recent session in this directory (with --all: in any directory)
  -r, --resume [<RESUME>]
          Resume a session: its id (or an unambiguous prefix), its name, -N (the Nth most recent) or a file path; without a value opens the picker
      --all
          With -c / --resume: look at sessions of every directory, not just this one
      --fork-session
          With -c / --resume: continue in a copy (a new session), leaving the original as it was
      --session-id <ID>
          Use this id for the new session (8-64 letters, digits, - or _; e.g. a UUID)
      --name <NAME>
          Name the session (also `mare session new --name …`)
      --no-session
          Do not persist this session
      --max-turns <N>
          Stop after this many model turns in one prompt (headless exit code 4)
      --timeout <DURATION>
          Stop the run after this long, e.g. 90s, 5m, 1h (headless exit code 4)
      --permission <PERMISSION>
          Permission preset for this session: read-only | auto | full-access [possible values: read-only, auto, full-access]
      --approval-policy <APPROVAL_POLICY>
          Independent approval policy; never rejects requests without prompting [possible values: ask, never, untrusted]
      --yolo
          Shorthand for --permission full-access
      --peer
          Headless / --rpc: also serve RPC on this session's peer socket and register it (interactive sessions always do unless `peer = false`)
      --web
          Enable the host HTTP/WebSocket RPC listener
      --web-listen <ADDR>
          Bind address (`HOST:PORT`) for the host HTTP/WebSocket RPC listener (`--web`)
      --set <KEY=VALUE>
          Override config values: --set plugins.tools.edit_mode=replace
      --strict-config
          Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust
          Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust
          Don't trust this project for this run, whatever was saved
      --approve-mcps
          Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean
          Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>
          Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup
          Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>
          Working directory
  -h, --help
          Print help
  -V, --version
          Print version
```

## mare plugin

```text
Manage plugins

Usage: mare plugin [OPTIONS] <COMMAND>

Commands:
  list       List declared and installed plugins
  tree       Print the composed runtime plugin tree
  sync       Install plugins declared in init.lua (respects mare-lock.json)
  update     Update plugins to their latest revision and rewrite mare-lock.json
  check      Check for updates without moving checkouts or changing the lock
  restore    Restore plugins to the commits recorded in mare-lock.json
  clean      Remove checkouts that are no longer declared
  install    Install one or more catalog plugins
  uninstall  Uninstall one or more catalog plugins
  help       Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin list

```text
List declared and installed plugins

Usage: mare plugin list [OPTIONS]

Options:
      --json                         Print the list as JSON
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin tree

```text
Print the composed runtime plugin tree

Usage: mare plugin tree [OPTIONS]

Options:
      --json                         Print the tree as JSON
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin sync

```text
Install plugins declared in init.lua (respects mare-lock.json)

Usage: mare plugin sync [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin update

```text
Update plugins to their latest revision and rewrite mare-lock.json

Usage: mare plugin update [OPTIONS] [NAME]

Arguments:
  [NAME]

Options:
      --stash                        Save local changes and untracked files in git stash before updating
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin check

```text
Check for updates without moving checkouts or changing the lock

Usage: mare plugin check [OPTIONS] [NAMES]...

Arguments:
  [NAMES]...

Options:
      --json                         Print structured results as JSON
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin restore

```text
Restore plugins to the commits recorded in mare-lock.json

Usage: mare plugin restore [OPTIONS] [NAME]

Arguments:
  [NAME]

Options:
      --repair                       Rebuild a missing or corrupt lock from clean installed checkouts
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin clean

```text
Remove checkouts that are no longer declared

Usage: mare plugin clean [OPTIONS]

Options:
      --force                        Allow removal of ignored files and saved git stashes
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin install

```text
Install one or more catalog plugins

Usage: mare plugin install [OPTIONS] <NAMES>...

Arguments:
  <NAMES>...

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare plugin uninstall

```text
Uninstall one or more catalog plugins

Usage: mare plugin uninstall [OPTIONS] <NAMES>...

Arguments:
  <NAMES>...

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare acp

```text
Run as an Agent Client Protocol server over stdio

Usage: mare acp [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare trust

```text
Trust (or revoke trust for) a directory (default: current project root, git worktree root, or cwd)

Usage: mare trust [OPTIONS] [DIR]

Arguments:
  [DIR]

Options:
      --revoke
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare session

```text
Sessions: create, list, fork, archive, unarchive, or delete saved sessions

Usage: mare session [OPTIONS] <COMMAND>

Commands:
  new        Create an empty session in this directory and print its id
  list       List saved sessions (newest first; current directory by default)
  fork       Fork a saved session (by id, prefix, name, or path) and print the new session id
  archive    Archive a saved session (by id, prefix, name, or path)
  unarchive  Restore an archived session (by id, prefix, name, or path)
  delete     Permanently delete a saved session (by id, prefix, name, or path)
  help       Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session new

```text
Create an empty session in this directory and print its id

Usage: mare session new [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session list

```text
List saved sessions (newest first; current directory by default)

Usage: mare session list [OPTIONS]

Options:
      --all                          Include sessions from every workspace
      --archived                     List archived sessions instead of active ones
      --limit <N>                    Maximum number of sessions to show
      --json                         Print JSON matching RPC `session.list`
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session fork

```text
Fork a saved session (by id, prefix, name, or path) and print the new session id

Usage: mare session fork [OPTIONS] <SESSION>

Arguments:
  <SESSION>  Session id, unique prefix, name, or path

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session archive

```text
Archive a saved session (by id, prefix, name, or path)

Usage: mare session archive [OPTIONS] <SESSION>

Arguments:
  <SESSION>  Session id, unique prefix, name, or path

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session unarchive

```text
Restore an archived session (by id, prefix, name, or path)

Usage: mare session unarchive [OPTIONS] <SESSION>

Arguments:
  <SESSION>  Session id, unique prefix, name, or path

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare session delete

```text
Permanently delete a saved session (by id, prefix, name, or path)

Usage: mare session delete [OPTIONS] <SESSION>

Arguments:
  <SESSION>  Session id, unique prefix, name, or path

Options:
      --yes                          Skip the interactive confirmation prompt (required when stdin is not a TTY)
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare runtime

```text
Print the runtime directory (read-only copies of builtin plugins and types)

Usage: mare runtime [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare init

```text
Create a default ~/.config/mare/init.lua and .luarc.json

Usage: mare init [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare doctor

```text
Check config, credentials, connectivity and plugins

Usage: mare doctor [OPTIONS]

Options:
      --fix                          Safe fixes only: create missing directories, tighten permissions (0700 / 0600)
      --json                         Print the checks as one JSON document
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare login

```text
Log in to a provider; `mare login status` lists every provider

Usage: mare login [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]  provider name, or `status`

Options:
      --with-api-key                 Read an API key from stdin: `printenv OPENAI_API_KEY | mare login openai --with-api-key`
      --device-auth                  Sign in with a device code (no browser here, e.g. over SSH)
      --oauth                        Sign in using browser OAuth
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare logout

```text
Remove a provider's stored credentials (all of them; tokens are revoked first)

Usage: mare logout [OPTIONS] [PROVIDER]

Arguments:
  [PROVIDER]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare model

```text
Model catalogue: `mare model refresh` re-reads every logged-in provider's model list

Usage: mare model [OPTIONS] <COMMAND>

Commands:
  refresh  Fetch the model lists (and models.dev metadata) now
  help     Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare model refresh

```text
Fetch the model lists (and models.dev metadata) now

Usage: mare model refresh [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare completion

```text
Print a shell completion script: `mare completion zsh > ~/.zfunc/_mare`

Usage: mare completion [OPTIONS] <SHELL>

Arguments:
  <SHELL>  [possible values: bash, elvish, fish, powershell, zsh]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare mcp

```text
MCP servers: list, list-tools, enable, disable, login

Usage: mare mcp [OPTIONS] <COMMAND>

Commands:
  list        Connect to every enabled server and print its state, tools and errors (exit 1 on problems)
  list-tools  Print a server's tools with their input schemas
  enable      Enable a server in this project (kept in the state directory, not the config file)
  disable     Disable a server in this project
  login       Log in to an OAuth server in the browser
  help        Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare mcp list

```text
Connect to every enabled server and print its state, tools and errors (exit 1 on problems)

Usage: mare mcp list [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare mcp list-tools

```text
Print a server's tools with their input schemas

Usage: mare mcp list-tools [OPTIONS] <SERVER>

Arguments:
  <SERVER>

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare mcp enable

```text
Enable a server in this project (kept in the state directory, not the config file)

Usage: mare mcp enable [OPTIONS] <SERVER>

Arguments:
  <SERVER>

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare mcp disable

```text
Disable a server in this project

Usage: mare mcp disable [OPTIONS] <SERVER>

Arguments:
  <SERVER>

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare mcp login

```text
Log in to an OAuth server in the browser

Usage: mare mcp login [OPTIONS] <SERVER>

Arguments:
  <SERVER>

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare peer

```text
Talk to mare sessions over their peer sockets

Usage: mare peer [OPTIONS] <COMMAND>

Commands:
  send  Send a message to a session: by default the one this process runs under ($MARE_PEER_SOCKET, set for tools and hooks); held until its user confirms
  help  Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare peer send

```text
Send a message to a session: by default the one this process runs under ($MARE_PEER_SOCKET, set for tools and hooks); held until its user confirms

Usage: mare peer send [OPTIONS] [TEXT]...

Arguments:
  [TEXT]...  the message (read from stdin when omitted)

Options:
      --to <TO>                      a session's socket path or registered name (default: $MARE_PEER_SOCKET)
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare rpc

```text
RPC protocol helpers

Usage: mare rpc [OPTIONS] <COMMAND>

Commands:
  schema  Print the JSON Schema of the RPC protocol
  help    Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare rpc schema

```text
Print the JSON Schema of the RPC protocol

Usage: mare rpc schema [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare sandbox

```text
Windows enhanced sandbox: install, remove or inspect

Usage: mare sandbox [OPTIONS] <COMMAND>

Commands:
  setup   Install the enhanced tier (elevates once): sandbox accounts, deny-read ACLs, WFP filters
  status  Show the installed setup and run the self-test
  help    Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare sandbox setup

```text
Install the enhanced tier (elevates once): sandbox accounts, deny-read ACLs, WFP filters

Usage: mare sandbox setup [OPTIONS]

Options:
      --remove                       Undo everything setup installed
      --deny-read <PATH>             Extra path that sandboxed commands must not read (repeatable; also `sandbox.deny_read`)
      --no-default-deny-read         Don't apply the built-in deny-read list (credentials, browser profiles)
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare sandbox status

```text
Show the installed setup and run the self-test

Usage: mare sandbox status [OPTIONS]

Options:
      --require-full                 Exit non-zero unless all sandbox isolation capabilities are fully enforced
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare test

```text
Run Lua *_spec.lua tests using the mare.test harness (cases in the same spec file share an isolated file-level Lua environment)

Usage: mare test [OPTIONS] [PATH]...

Arguments:
  [PATH]...  Spec files or directories to run (default: current directory)

Options:
  -f, --filter <PATTERN>             Only run test cases whose full name contains this substring
  -u, --update-snapshots             Update snapshot files directly instead of failing or writing .snap.new
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

## mare token

```text
Host runtime authentication token management

Usage: mare token [OPTIONS] <COMMAND>

Commands:
  rotate  Generate a new 32-byte host token (0600) and immediately invalidate the previous token
  help    Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare token rotate

```text
Generate a new 32-byte host token (0600) and immediately invalidate the previous token

Usage: mare token rotate [OPTIONS]

Options:
      --accept-partial <ACCEPT_PARTIAL>
          Accept missing sandbox protection explicitly (e.g. fs_host_protect)
  -m, --model <MODEL>
          Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>
          Use a registered agent type for the initial session
  -e, --effort <EFFORT>
          Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>
          Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>
          Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>
          Append text to the system prompt
      --name <NAME>
          Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>
          Override config values: --set plugins.tools.edit_mode=replace
      --strict-config
          Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust
          Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust
          Don't trust this project for this run, whatever was saved
      --approve-mcps
          Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean
          Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>
          Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup
          Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>
          Working directory
  -h, --help
          Print help
```

## mare web

```text
Host HTTP/WebSocket listener helpers

Usage: mare web [OPTIONS] <COMMAND>

Commands:
  url   Print the authenticated login URL for the running host webserver
  help  Print this message or the help of the given subcommand(s)

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

### mare web url

```text
Print the authenticated login URL for the running host webserver

Usage: mare web url [OPTIONS]

Options:
  -m, --model <MODEL>                Model, e.g. openai/gpt-6.1-terra
      --agent <NAME>                 Use a registered agent type for the initial session
  -e, --effort <EFFORT>              Reasoning effort (off, minimal, low, medium, high, xhigh, max)
      --tools <TOOLS>                Only enable these tools (comma separated), e.g. --tools read,bash
      --exclude-tools <TOOLS>        Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
      --append-system-prompt <TEXT>  Append text to the system prompt
      --name <NAME>                  Name the session (also `mare session new --name …`)
      --set <KEY=VALUE>              Override config values: --set plugins.tools.edit_mode=replace
      --strict-config                Refuse to start (exit code 2) when the configuration has warnings (unknown options, type errors, unparseable MARE_* values) or init.lua errors instead of continuing
      --trust                        Trust this project for this run (load its plugins, skills, prompts, system prompt); overrides the saved decision and saves nothing
      --no-trust                     Don't trust this project for this run, whatever was saved
      --approve-mcps                 Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and configuration hash) without asking — for headless runs
      --clean                        Like `nvim --clean`: no user/project config, user plugins, or packages
      --startuptime <FILE>           Write startup timing to a file (like Neovim's --startuptime)
      --profile-startup              Start the TUI, quit once it is ready, and print how long each startup stage took
  -C, --cwd <CWD>                    Working directory
  -h, --help                         Print help
```

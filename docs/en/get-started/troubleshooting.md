# Troubleshooting

How to find out why mare does not start, cannot reach a model, behaves differently than you configured, or draws wrongly in your terminal.

## Start with mare doctor

```sh
mare doctor
```

`mare doctor` runs without calling a model. It prints one line per check and exits `1` if any check fails (`0` otherwise). A check mark means pass, `!` is a warning that does not fail the run and a cross is a failure. `mare doctor --json` prints the same checks as one JSON document, and `mare doctor --fix` creates missing directories, tightens permissions, and attempts to repair a corrupt package lock file (the run itself still does its normal start-up work, such as preparing the session index and runtime files).

Inside a session, `/doctor` shows the checks the running session knows about, as an overlay.

| Check | What it tells you |
| --- | --- |
| `directories`, `permissions` | The config, state, data, sessions and log directories exist; `auth.json`, the state directory and the sessions directory are private (`0600` / `0700`), as are `.jsonl` files directly inside it. Per-project session files in subdirectories are not checked. `--fix` repairs both |
| `terminal capabilities` | Which terminal mare detected and whether it has truecolor, wide emoji, the kitty keyboard protocol and bracketed paste, plus known quirks of that terminal |
| `config`, `model`, `config options` | Whether `init.lua` loads, which model is set, and any unknown option names or type errors in `mare.setup`, `mare.o` and `mare.plugin` |
| `auth.json`, `auth.json mode` | A credentials file that does not parse is reported, never treated as "no credentials"; mare keeps a copy as `auth.json.bak` |
| `sandbox` and `sandbox ...` | The sandbox backend on this machine (for example `bwrap` or `landlock` on Linux, Seatbelt on macOS) and which capabilities it can enforce |
| `tmux clipboard`, `tmux keys` | Only inside tmux: whether `set-clipboard` and `extended-keys` are on |
| `session files`, `session index` | Every saved session parses; a half-written last entry is reported as a warning; the session index is consistent |
| `runtime`, `plugins` | The runtime directory is extracted; how many plugins are active and each plugin's problem, including which plugins override a built-in |
| `permission policy`, `key conflicts` | The permissions service in use and the session sandbox level; key bindings that your terminal or OS probably intercepts |
| `credential sources` | For each provider, where its credential comes from (a stored login, an environment variable, or none). Secrets are never printed |
| connectivity | Reachability of the providers you have configured, never a fixed list |
| `mcp ...`, `packages`, `skills`, `git` | Checks contributed by plugins: MCP servers, installed packages and the lock file, loaded skills, `git` on `PATH` |

On Windows, `mare doctor` also reports legacy console hosts and whether Windows Terminal forwards `Shift+Enter` and `Alt+Enter`.

Plugins add their own checks by listening to the `kernel/doctor` event; see [Services and events](../extend/services-and-events.md).

## Isolate the cause

These flags narrow a problem down. Combine them with a normal start.

| Flag | Effect |
| --- | --- |
| `--clean` | Skip `init.lua`, packages, user plugins and project plugins. Only the built-in plugins load, and the project is not trusted |
| `--strict-config` | Refuse to start (exit `2`) when the configuration has warnings, instead of printing them. Applies to headless and RPC runs |
| `--no-trust` | Do not trust this project for this run, whatever was saved |
| `--no-session` | Do not read or write a session |
| `--set KEY=VALUE` | Override one config value for this run, for example `--set plugins.tools.edit_mode=replace` |

If `mare --clean` works and `mare` does not, the problem is in `init.lua`, a package, or a plugin. Bisect by commenting out parts of `init.lua`. If the problem is in a project, `mare --no-trust` tells you whether its `.mare/` files are the cause.

To try mare with entirely fresh directories without touching your real ones, point `MARE_HOME` at an empty directory: `MARE_HOME=$(mktemp -d) mare doctor`.

## Slow start

```sh
mare --profile-startup            # start the UI, quit when ready, print the time of each stage
mare --startuptime /tmp/start.txt # normal start, write the timing to a file (like nvim --startuptime)
```

Inside a session, `/profile` shows the startup timeline and each plugin's load time, plus changes to the tool list between model requests (a changed tool list invalidates the provider's prompt cache from that point).

## Configuration errors

mare distinguishes warnings from errors.

- Warnings are unknown option names, wrong types, and unparseable `MARE_*` environment values. The option name comes with a "did you mean" hint when one is close. Headless and RPC runs print warnings to stderr; add `--strict-config` to make them refuse to start (exit `2`). `/config` lists every option, its value and where it was set.
- An error is an `init.lua` (or a trusted project's `.mare/init.lua`) that fails to run. The interactive UI shows a dialog with the file, the line and the first line of the error, and three choices: open the file in your editor at that line (the config runs again afterwards), exit, or continue. If you continue, the UI starts but every model request stays blocked until the file is fixed and `/reload` succeeds. Headless and RPC runs have no dialog and exit with `2`.
- An optional plugin or package that fails to load reports a warning and is skipped. `/plugins` lists every plugin with its origin and, below them, each failure with a traceback; the traceback stays until a full `/reload` clears it. Startup notices say "fix it and /reload (or start clean: mare --clean)".
- Failures in essential services, plugins marked `required = true`, or required packages (`required = true`) block model requests until fixed and `/reload`ed (or disabled). `mare plugin sync` installs the packages `init.lua` declares, `mare plugin list` shows them, and disabling a required package in `init.lua` stops it from blocking requests.

Reload after editing: `/reload` reloads plugins, `/reload <plugin>` one plugin and `/reload config` the configuration.

## Credentials and models

| Symptom | What to check |
| --- | --- |
| "X is not configured: run /login X or set ..." | `mare login status` shows each provider. Log in, or export the provider's variable (see the [Quickstart](quickstart.md#log-in-to-a-provider)) |
| A key in the environment is ignored | A stored login in `auth.json` wins over environment variables. `mare doctor` names a source that is set but not used. `mare logout <provider>` removes the stored one |
| `mare login anthropic` says to pass the key on stdin | API-key providers read the key from stdin only: `printenv ANTHROPIC_API_KEY \| mare login anthropic --with-api-key` |
| The ChatGPT browser login cannot open a browser | Over SSH, or on Linux without `DISPLAY` or `WAYLAND_DISPLAY`, use `mare login openai --device-auth` |
| `/model` shows a provider as "needs login" | That provider has no credential yet; `/login <provider>` |
| The model list is old or a model is missing | `mare model refresh` re-reads the model list of every logged-in provider |
| `auth.json` is corrupt | Saved credentials cannot be read or changed until it is fixed. Fix the file, or delete it and log in again; `auth.json.bak` holds the copy mare made |
| `mare logout` removed the login but the provider still works | The credential also comes from an environment variable or `init.lua`; `logout` tells you which one to remove |

The first-run flow starts by itself in the interactive UI whenever no provider has a credential.

## Terminal problems

- Run `/terminal-setup` to check whether `Shift+Enter` and `Alt+Enter` reach mare and to see how to fix them. Without `Shift+Enter`, type `\` and press Enter for a newline.
- Inside tmux, `Shift+Enter` arrives as Enter unless `set -g extended-keys on` is in `tmux.conf`, and copying only reaches the system clipboard with `set -g set-clipboard on`. `mare doctor` checks both.
- `TERM=dumb` selects a plain line-by-line mode with no cursor movement.
- `mare doctor` lists key bindings that terminals or the OS commonly intercept (`key conflicts`) with a fix. Rebind with `mare.keymap.set`; see [Key maps](../configure/keymaps.md).
- The screen is garbled after a program wrote to the terminal directly: press `Ctrl+L` to redraw. Output written to mare's stdout and stderr, including by child processes, is captured above the input box; only programs that open the terminal device themselves can disturb the screen.
- Images show as `[image]`: the terminal supports none of the Kitty, iTerm2 or sixel protocols. `mare doctor` shows the detected capabilities.
- On Windows, `Ctrl+V` is the terminal's text paste, so paste images with `Alt+V`, and `Ctrl+Z` cannot suspend mare.

## Sessions

- `session files ... damaged` in `mare doctor`: the named file cannot be parsed. Open it read-only or fork it instead of continuing it.
- `session torn writes`: the last entry was half written (usually after a crash). It is ignored and cut at the next resume.
- `session index ... degraded`: `/resume` falls back to scanning the session files.
- After a crash or kill in the middle of a turn, run `/continue` in the resumed session to continue the interrupted turn.

## Logs, crash reports and state

| What | Where |
| --- | --- |
| Log | `~/.local/state/mare/log/mare.log`, rotated at 10 MB with 5 old files kept, redacted, mode `0600` |
| Crash reports | `~/.local/state/mare/crashes/<time>/report.txt`, written only when a panic ends mare |
| Sessions | `~/.local/state/mare/sessions/` |
| Runtime choices (model, effort) | `~/.local/state/mare/state.json` |
| Trust decisions | `~/.local/state/mare/trust.json` |
| Credentials | `~/.config/mare/auth.json` |

The default log level is `info`. Raise it for one run with the `MARE_LOG` environment variable, which takes a `tracing` filter:

```sh
MARE_LOG=debug mare
MARE_LOG=info,mare_binding=trace mare
```

Inside a session, `/debug` shows the most recent log lines and the latest crash report. Directories move with `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `MARE_HOME`.

## Sandbox and permissions

`mare doctor` prints the sandbox backend and what it can and cannot enforce. A `!` on `sandbox deny-read` means that restricting reads is not available on this machine, so approved escalations leave the sandbox as approved. Inside a session, `/permission caps` shows the same capability summary, `/permission log` the permission decisions of the session, and `/permission` changes the preset. `--permission full-access` (`--yolo`) removes the sandbox and does not ask, except for dangerous commands, which still ask in every preset (and are denied in a headless run).

If a headless run exits with `3`, an approval was answered with "deny and stop" (`abort`) and that stopped the turn. An approval nobody could answer is only denied: the model gets a denial and the run can still exit `0`, with the denied calls listed on stderr. To avoid denials, choose a preset that does not need the approval (`--permission`) or set `--approval-policy never` so requests are rejected without prompting.

# Workflows

## Explore a codebase read-only

```sh
mare --permission read-only "Give me a map of this repository: the main modules, the entry points and how they connect."
```

`read-only` blocks writes and network access. On the default Windows tier (no network blocking), `mare` asks before each shell or command tool unless `all` / `danger-full-access` was approved or allowed by rule.

Follow up with narrower questions and attach files with `@`:

```text
Where is the retry policy decided? Compare @src/client.rs with @src/config.rs.
```

To limit what the model can call at all, name the tools. `read` and `bash` are two of the built-in tools (the others include `write` and `edit`):

```sh
mare --tools read,bash --permission read-only
```

Run `/init` once in a project to have the agent write an `AGENTS.md` with build commands, architecture notes and conventions. Later sessions read it as context.

## Fix a bug with approvals

Describe the symptom and ask for a failing test first:

```sh
mare "The CSV export drops the last row when the file has no trailing newline. Reproduce it with a failing test, then fix it."
```

In the default `auto` preset, edits inside the workspace go through and mare asks when a call would leave the sandbox. Answer the approval box with `y` (once), `a` (this session) or `n` (deny, then say why). To approve every write yourself, run `/permission read-only`. See [Permissions](../use/permissions.md) for the rules behind each answer.

Check what happened before you commit:

- `/diff` shows the files changed in each turn, newest first.
- `Ctrl+R` opens the change viewer: mare's own edits per turn, or git's unstaged, staged and last-commit diffs.
- Type `!git diff --stat` to run a command yourself and put the result in the context.

## Review changes

`/review` starts a separate read-only session that reviews code and shows its findings in the transcript. They never enter your conversation's context.

```text
/review                     pick a target from a list
/review uncommitted         staged, unstaged and untracked changes
/review <sha>               one commit
/review base main           the changes against a base branch
/review focus on error handling and concurrency
```

Any text that is not a target is taken as custom review instructions. `/review changes [path]` opens the change viewer instead of running a review.

## Run headless in a script or CI

`-p` runs one prompt, prints the final answer to stdout and exits:

```sh
mare -p "Summarize the changes in @CHANGELOG.md in three bullets"
```

mare also goes headless by itself when stdin or stdout is not a terminal. Piped input is appended to the prompt, or becomes the prompt when you pass `-` or no prompt:

```sh
git diff main... | mare -p "Review this diff for bugs"
echo "list the TODO comments in src/" | mare -p -
```

A headless run cannot ask you anything. Set the permission level up front, and bound the run:

```sh
mare -p --permission read-only --max-turns 20 --timeout 10m "Find unused exported functions"
```

| Exit code | Meaning |
| --- | --- |
| `0` | Completed |
| `1` | Error |
| `2` | Bad arguments or configuration |
| `3` | An approval was answered with "deny and stop" and the turn was stopped |
| `4` | `--max-turns` or `--timeout` reached |
| `130` | Interrupted |

For machine-readable output use `--json`, which prints one JSON object per line. Each line is `{"type": ..., "data": ...}`, a `session` record comes first and a `settled` record comes last with the final text, usage, `exit_code` and `reason`:

```sh
mare --json "Run the tests and report failures" | tail -n 1 | jq '.data | {exit_code, reason}'
```

Add `--stream-deltas` for token-level events. Headless runs never ask about project trust: a saved decision applies, and otherwise a project's `.mare/` files stay unloaded unless you pass `--trust`. MCP servers declared by the project need `--approve-mcps`. In CI, make runs reproducible with `--no-session` (the session is not saved or resumable; runtime files and tool writes still happen) and `--strict-config` (refuse to start on configuration warnings). See [Headless mode](../use/headless.md) and [JSON stream](../integrate/json-stream.md).

To run several prompts in one session from a script, create the session first:

```sh
id=$(mare session new --name nightly)
mare -p --resume "$id" "Step 1: list failing tests"
mare -p --resume "$id" "Step 2: fix the first one"
```

## Continue and fork sessions

```sh
mare -c                  # continue the most recent session in this directory
mare -r                  # open the session picker
mare -r -2               # the second most recent session in this directory
mare -r nightly          # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original unchanged
mare -c --all            # most recent session in any directory
```

Inside a session:

| Command | What it does |
| --- | --- |
| `/new` (alias `/clear`) | Start a new session |
| `/resume` (alias `/sessions`) | Pick a session; `/resume all` lists every directory, `/resume search <text>` searches |
| `/name <title>` (aliases `/rename`, `/title`) | Name the session so `-r <name>` finds it |
| `/fork` | Fork into a new session from your latest message; `/fork pick` lets you choose the message |
| `/tree` | Navigate the session tree and branch from any earlier point |
| `/retry` | Regenerate the last answer; the old one stays on its branch |
| `/btw <question>` | Ask a side question; the answer is not added to the conversation |
| `/export [path]` | Export the conversation as Markdown |

Sessions are JSONL files in `~/.local/state/mare/sessions`. See [Sessions](../use/sessions.md).

## Undo and rewind

There are two levels of undo, and they do different things.

- `/undo` rewinds the conversation to before a prompt. `/undo` alone picks one, `/undo N` takes the Nth-last. The files stay as they are. Pressing Esc twice while the agent is idle opens the same picker.
- `/rewind [turn]` puts files back the way they were before a turn. It can also rewind the conversation to that point (as a `/tree` branch, so the old one is kept) or fold the conversation from that turn into a summary.

mare saves the original of a file before any tool writes, removes or renames it, whichever tool does it. A file that changed outside mare since is listed and restored only after you confirm. Changes made by shell commands cannot be snapshotted, and the turn says so.

## Background jobs

For commands that take a while, the agent starts them in the background and keeps working. You are notified when they finish, with a short piece of the output.

- Ask for it: "Start the dev server in the background and check that it answers on port 8080."
- Press `Ctrl+B` while a command is running to move it to the background.
- `/jobs` lists jobs and opens a job's output or stops it. A line above the input box shows running jobs.

## Steer or queue while the agent works

You do not have to wait for a turn to end.

| Key | Effect while the agent works |
| --- | --- |
| Enter | Queue the message; it is sent when the turn finishes |
| `Ctrl+S` | Steer: send the input into the running turn |
| `Alt+Enter` | Queue it explicitly as a follow-up (`Ctrl+Q` on Windows) |
| Esc | Interrupt the turn |
| `Ctrl+C` | Interrupt, or clear the input; twice to exit |

Press Enter on an empty input while messages are queued to get a menu with three choices: steer now, interrupt and send, or keep them queued. Queued messages are shown above the input; press Up to select one and Enter to edit it in place.

## Work with images

- Press `Ctrl+V` in the input to paste an image from the clipboard (`Alt+V` on Windows, where the terminal owns `Ctrl+V`).
- Ask the agent to look at an image file: "Look at docs/mockup.png and list the UI elements." The read tool returns PNG, JPEG, GIF and WebP files as images.

On a terminal that supports the Kitty, iTerm2 or sixel image protocols, mare draws images inline in the transcript. Elsewhere the transcript shows `[image]`. The `images` plugin controls which images are drawn; see [UI](../configure/ui.md).

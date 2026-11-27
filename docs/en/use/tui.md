# Interactive interface

How the mare terminal UI works: the input editor, key bindings, slash commands, file mentions, shell commands, queued messages, transcript and notifications.

mare runs in the terminal's main screen, not in an alternate screen. Finished output (messages, tool calls, diffs) is written to your terminal's normal scrollback, so you scroll, search and copy with your terminal's own tools. Only the bottom of the screen is live: the streaming reply, widgets, the input box and the status bar. Start it with:

```sh
mare                       # new session
mare "explain this repo"   # new session, first prompt already sent
mare -c                    # continue the last session in this directory
```

## The input editor

Type a prompt and press Enter to send it.

| Key | Action |
| --- | --- |
| `Enter` | Send. A trailing `\` before Enter inserts a newline instead. |
| `Shift+Enter`, `Ctrl+J` | Insert a newline. |
| `Left` / `Right`, `Ctrl+B` / `Ctrl+F` | Move by character. |
| `Alt+Left` / `Alt+Right`, `Alt+B` / `Alt+F` | Move by word. |
| `Home` / `End`, `Ctrl+A` / `Ctrl+E` | Start / end of line. |
| `Up` / `Down`, `Ctrl+P` / `Ctrl+N` | Move between lines; at the first or last line, browse prompt history. |
| `Backspace`, `Ctrl+H`; `Delete`, `Ctrl+D` | Delete a character. `Ctrl+D` on an empty input exits. |
| `Ctrl+W`, `Alt+Backspace` | Delete the previous word. |
| `Alt+D` | Delete the next word. |
| `Ctrl+U` / `Ctrl+K` | Cut to the start / end of the line. |
| `Ctrl+Y`, `Alt+Y` | Paste the last cut text; cycle earlier cuts. |
| `Ctrl+_` | Undo in the input. |
| `Ctrl+G` | Edit the input in an external editor. |
| `Ctrl+R` after `Up` | Search the prompt history. |
| `Tab` | Complete a `@file` or `/command`. |

Large pastes are folded into a placeholder in the input and expanded when you send. Prompt history is stored in `~/.local/state/mare/history.jsonl` (or under `$XDG_STATE_HOME/mare`).

Not every terminal sends `Shift+Enter` and `Alt+Enter` distinctly. Run `/terminal-setup` to check yours and see how to fix it.

### External editor

`Ctrl+G` hands the input to an editor and puts the result back when the editor exits successfully. mare uses the `editor` option, then `$VISUAL`, then `$EDITOR`, then `vi`. The value may contain arguments, for example `code --wait`.

```lua
-- ~/.config/mare/init.lua
mare.setup { editor = "nvim" }
```

### Key bindings

The bindings below are the defaults. Press `?` on an empty input, or run `/hotkeys`, to see the live list, including your changes.

| Key | Action |
| --- | --- |
| `Esc` | Interrupt the agent. When idle, twice quickly opens the rewind picker (`/undo`). |
| `Ctrl+C` | Close a menu, clear the draft, interrupt the running work. Twice on an idle empty input exits. |
| `Ctrl+D` | Exit when the input is empty. |
| `Ctrl+O` | Toggle compact and expanded tool output and thinking. |
| `Ctrl+T` | Open the transcript view. |
| `Ctrl+S` | Send the input into the running turn (steer). |
| `Alt+Enter` | Queue the input as a follow-up. On Windows the key is `Ctrl+Q`. |
| `Ctrl+V` | Paste an image from the clipboard. On Windows the key is `Alt+V`. |
| `Ctrl+B` | Move the running shell command to the background; with nothing running, cursor left. |
| `Ctrl+L` | Redraw the screen. |
| `Ctrl+Z` | Suspend mare (`fg` resumes it). Not available on Windows. |
| `Alt+S` | Select mode: drag with the mouse to select text in the live area; releasing copies. |
| `?` | List all key bindings (on an empty input). |

Rebind any of these with `mare.keymap.set`; see [Keymaps](../configure/keymaps.md).

## Slash commands

Type `/` at the start of the input to open the command list, then pick with the arrow keys and `Tab` or `Enter`. `/help` lists every command, `/commands` opens a browsable list that runs the one you pick, and `/skills` does the same for [skills](context.md).

| Group | Commands |
| --- | --- |
| Session | `/new` (`/clear`), `/resume` (`/sessions`), `/name`, `/undo`, `/rewind`, `/fork`, `/clone`, `/tree`, `/compact`, `/export`, `/copy`, `/delete`, `/archive` |
| Model | `/model`, `/effort`, `/login`, `/logout` |
| Context | `/init`, `/memory`, `/context`, `/add-dir` |
| Safety | `/permission` (`/permissions`), `/yolo`, `/approval` |
| Inspect | `/status`, `/usage`, `/diff`, `/jobs`, `/mcp`, `/doctor`, `/config`, `/debug` |
| Interface | `/theme`, `/statusline`, `/hotkeys`, `/terminal-setup`, `/show-reasoning` |
| Other | `/btw`, `/reload`, `/plugins`, `/exit` (`/quit`) |

The command list depends on which plugins are enabled, so use `/help` for the authoritative list in your install. The full reference is in [Slash commands](../reference/slash-commands.md).

You can add your own short names with `mare.command.alias("m", "model")` in `init.lua`.

## File mentions

Type `@` and start a path. `Tab` completes it with a fuzzy match over the files under the working directory and any directory added with `/add-dir`.

```text
review @src/main.rs and compare it with @"docs/old design.md"
```

On send, mare attaches the contents of each mentioned file to your message. Paths with spaces use `@"..."`. A directory is attached as a listing. A reference that cannot be attached is not dropped silently: the message gets a one-line note saying why (missing, binary, too large, denied) and pointing at the `read` tool. Limits are 100 KiB per file and 256 KiB per message; both are options of the `file-ref` plugin (`max_bytes`, `max_total_bytes`).

```lua
mare.plugin("file-ref", { max_bytes = 200 * 1024 })
```

## Shell commands

Type `!` on an empty input to enter shell mode, then a command:

```text
!git status           runs the command; command and output go into the context
!!cat .env.example    runs the command; the output is shown but not added to the context
```

The command uses the same configured shell program and prelude as the agent's `bash` tool, but it is a separate process: it does not share the agent shell's state (working directory, exported variables) and does not get the `bash` tool's `env_include` and `env_exclude` policy. `Esc` or `Ctrl+C` aborts it. Backspace on an empty shell prompt leaves shell mode. If the agent is busy, the output is delivered after the current tool results, like a steering message. The model sees at most 2000 lines or 50 KiB of the output (the middle is cut).

Shell commands you type do not go through the approval prompt; they are your commands. See [Permissions](permissions.md) for what the policy covers.

## Working while the agent runs

You can keep typing while the agent works.

- `Enter` queues your message. It appears above the input until the agent takes it, then becomes a normal user message.
- `Ctrl+S` steers: the text is injected into the running turn and delivered after the current tool calls finish.
- `Alt+Enter` queues the message as a follow-up, the same as `Enter` while busy.
- With an empty input, `Up` selects a queued message; `Up` and `Down` move between them, `Enter` loads the selected one into the input to edit it, `Delete` removes it and `Esc` leaves the selection.
- `Enter` on an empty input while messages are queued opens a menu: steer now, interrupt and send, or keep queued.
- `Esc` interrupts the turn. `Esc` twice on an idle, empty input opens the `/undo` picker. Pressing `Esc` while a `!cmd` runs aborts only that command.

`/btw <question>` asks a side question that is answered in a widget above the editor and not added to the conversation.

## Transcript view

`Ctrl+T` opens the full transcript in an overlay with tool output and thinking expanded. Keys follow `less` and Vim:

| Key | Action |
| --- | --- |
| `j` / `Down`, `k` / `Up` | Scroll a line. |
| `Space`, `f`, `PageDown`; `b`, `PageUp` | Scroll a page. |
| `g` / `Home`, `G` / `End` | Top / bottom. |
| `/`, `n`, `N` | Search, next match, previous match. |
| `y` | Copy the message at the reading position. |
| `q`, `Ctrl+T` | Close. |
| `Esc`, `Ctrl+C` | Clear the search, then close. |

Output already written to the scrollback cannot be redrawn, so `Ctrl+O` on an old tool call opens it in the transcript view, scrolled to that call. `Ctrl+O` while a call is still live switches all later output between compact and expanded.

## Images

`Ctrl+V` (`Alt+V` on Windows) attaches an image from the clipboard to the next message. On terminals with an image protocol (Kitty, iTerm2, sixel) pasted images and images returned by tools are drawn inline; elsewhere the transcript shows `[image]`. Disable inline drawing or change the size with the `images` plugin options `user`, `read`, `tools`, `max_rows` and `max_cols`.

## Status line

The bar under the input shows the permission mode, model and effort, a context gauge, the working directory, the git branch and its pull request (through `gh`), changed files, cost and quota. `/statusline` toggles segments and saves the choice in `state.json`. To run your own command instead, give it a JSON object on stdin and show its first output line:

```lua
mare.plugin("statusline", { command = "~/bin/sl" })
```

See [UI](../configure/ui.md) for layouts and themes.

## Notifications

When a turn that took at least five seconds finishes, or mare is waiting for you (a question or an approval), it sends a terminal notification (OSC 9, 777 or 99, or a bell) while the terminal is unfocused. Tune it with the `notify` plugin:

```lua
mare.plugin("notify", { min_turn_ms = 10000, only_unfocused = true })
-- or run a program with a JSON event on stdin:
mare.plugin("notify", { cmd = { "notify-send-mare" } })
```

`cmd` also runs in headless and RPC modes. Set `enabled = false` to turn the plugin off.

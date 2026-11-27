# Sessions

Each session is an append-only JSONL file scoped to a working directory. Every entry stores an `id` and `parent`, so rewinding or retrying creates a branch while keeping prior branches in the same file.

## Where sessions live

```text
~/.local/state/mare/sessions/<directory-slug>/<timestamp>_<id-suffix>.jsonl
```

The state directory follows `$XDG_STATE_HOME` (or `$MARE_HOME/state` if `MARE_HOME` is set). `<directory-slug>` is the working directory path with every character other than Unicode alphanumeric characters, `.`, and `_` replaced by `-`, wrapped in `--`. A session file is created when the first message is written, not at startup. For a session file `abc.jsonl`, its attachments sit beside it in `abc.attachments/` (images moved out of the log) and its `/rewind` backups in `abc.jsonl.snapshots/`. When there is no session file, snapshots use `<state>/snapshots/run-<pid>-<uuid>/`, and compacted images fall back to `<state>/attachments/<session-id or unsaved>/`.

See [Files](../reference/files.md) for the full file layout.

### Durability and crash recovery

- Appends are whole lines. A trailing line without a newline is a torn write: readers ignore it and the next write truncates it.
- At semantic boundaries (for example when a message is complete) the file is flushed to disk with `fsync`. A batch that fails to write or sync is rolled back to its previous length.
- A line that ends in a newline but does not parse is treated as corruption. mare never opens such a file for writing or rewrites it; you can view it read-only or fork from a stable snapshot.
- One process writes a session at a time, enforced with a lock file next to the session. If another process has it open, mare offers a read-only view or a fork instead.
- mare records a `running` marker when it opens a session and a `closed` marker on a clean exit. A session whose last marker is `running` was interrupted. It shows as crashed in the `/resume` picker, and `/continue` resumes an interrupted turn.

## Resume and continue

```sh
mare -c                      # most recent session in this directory
mare -c --all                # most recent session in any directory
mare -r                      # open the picker
mare -r 3f9a1c2e             # by id, or an unambiguous id prefix (headless)
mare -r "fix login bug"      # by name, case-insensitive (headless)
mare -r -2                   # the second most recent session here
mare -r ./some/session.jsonl # by file path
```

| Option | Meaning |
| --- | --- |
| `-c`, `--continue` | Continue the most recent session in this directory. |
| `-r`, `--resume [VALUE]` | Resume by id, name, `-N` or path. Without a value, open the picker. |
| `--all` | With `-c` or `-r`, look at sessions of every directory. |
| `--fork-session` | With `-c` or `-r`, continue in a copy and leave the original untouched. |
| `--session-id ID` | Use this id for a new session: 8 to 64 letters, digits, `-` or `_`. Fails if the id exists. |
| `--name NAME` | Name the new or resumed session. |
| `--no-session` | Do not save the session. mare may still write runtime files and per-run snapshots, and tools can write files. The session cannot be resumed, and `-c` or `-r` together with it is an error. |

In the interactive UI, `-r <text>` that is not a full id, a `-N` or a file path opens the picker with the text as the search. In headless runs a name or prefix that matches several sessions is an error that lists the candidates; mare never picks "the newest" for you. A session that cannot be found is an error (exit code 1), never a silent new session.

If you resume a session that was recorded in another directory, the TUI asks which directory to use. The `session` plugin option `resume_cwd` takes `"ask"` (default), `"current"` or `"session"`.

```lua
mare.plugin("session", { resume_cwd = "current" })
```

### Scripts

`mare session new` creates an empty session in the current directory and prints its id, so a script can create the session first and run prompts into it:

```sh
id=$(mare session new --name nightly-review)
mare -p --resume "$id" "list the TODO comments in src/"
mare -p --resume "$id" "now rank them by risk"
```

`mare session` provides `new`, `list`, `fork`, `archive`, `unarchive`, and `delete` subcommands (see [CLI reference](../reference/cli.md#mare-session)). It runs a lightweight session host without loading user/project configs or plugins, using `--cwd` and (for `new` and `fork`) `--name`; pass run options such as `--model`, `--trust`, or `--permission` to the subsequent `mare -p --resume "$id"` calls. See [Headless](headless.md).

## In the session

| Command | Effect |
| --- | --- |
| `/new` (`/clear`) | Start a new session. The old one stays on disk. |
| `/resume` (`/sessions`) | Open the picker. `/resume all` lists every directory; `/resume search <text>` filters; `/resume <id or name>` loads one. Not available while a turn runs. |
| `/name <title>` (`/title`, `/rename`) | Name the session. Without a title it shows the current name. |
| `/fork` | Fork into a new session from your latest message. `/fork pick` lets you choose the message. |
| `/clone` | Duplicate the whole session into a new one. |
| `/tree` | Browse the session tree and branch from any earlier point. |
| `/label <text>` | Label the current point in the tree. |
| `/retry` | Regenerate the last answer on a new branch. The old answer stays at its branch. |
| `/continue` | Continue an interrupted turn after a crash or kill. |
| `/archive` | Mark the session archived and start a new one. The file stays. |
| `/delete` | Delete this session after a confirmation. |

Picker keys: `Ctrl+D` delete, `Ctrl+R` rename, `Ctrl+S` sort by created or updated, `Ctrl+N` named only, `Ctrl+A` show archived, `Ctrl+X` archive, `Ctrl+E` expand, `Ctrl+T` transcript, `Ctrl+L` change layout. The `session` plugin option `picker_style` selects `"comfortable"` (default), `"dense"` or `"cards"`.

### Undo and rewind

`/undo` and `/rewind` are different.

- `/undo` rewinds the conversation to before a prompt and puts the prompt text back in the input. Files on disk stay as they are. `/undo` opens a picker; `/undo 2` goes back two prompts. Only prompts since the last compaction can be undone. Pressing `Esc` twice on an empty idle input opens the same picker. The undone messages stay in the file on their own branch (see `/tree`).
- `/rewind` restores files. mare saves the original of a file before any tool writes, removes or renames it, and `/rewind [turn]` puts those files back to how they were before a turn. You choose between restoring files only, restoring files and the conversation, or summarizing the conversation from that turn on.

`/rewind` cannot undo changes made by shell commands, and the turn says so. If a file changed outside mare after the turn, it is listed and restored only after you confirm. Backups are capped (`changes` plugin option `cap_bytes`, 64 MiB by default); when the oldest are trimmed they are reported as missing. `/diff` shows the files changed in each turn.

### Compaction

Long sessions are compacted so they fit in the model's context window. Compaction writes summary or projection records into the session JSONL file while leaving all original messages intact on disk (reachable via `/tree` or inspecting the file).

```text
/compact                      compact now
/compact keep the migration plan   compact with a focus instruction
```

For the four compaction stages (`prune`, `offload`, `remote`, `local`), threshold precedence, backoff and `stalled` handling, and `compact` plugin options, see [Usage and compaction](usage-and-compaction.md). For what stays outside compaction on every request, see [Context](context.md).

### Export and copy

- `/export [path]` writes the conversation as redacted Markdown with private file permissions (`0600` on Unix). Without a path the file is `mare-session-<date>-<time>.md` in the working directory. `/export-md` is an alias.
- `/copy` copies the last response to the clipboard as Markdown source. If it has code blocks, a picker offers each block or the full response; `w` in the picker writes the choice to a file and `a` remembers "always copy the full response". `/copy 2` copies the second-to-last response. The clipboard path uses OSC 52 over SSH.

### Deleting sessions

`/delete` removes the current session together with its attachments and snapshots, then starts a new one. `/delete <path>` deletes the session file at that path after a confirmation. Deletion is refused while another process has the session open. The picker's `Ctrl+D` does the same for the highlighted row. There is no undelete.

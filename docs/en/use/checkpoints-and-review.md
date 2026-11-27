# Checkpoints and code review

Before mare's file tools (`write`, `edit`, `apply_patch`) modify, delete, or rename a file, mare records the previous file bytes in a per-session snapshot journal. File changes made by shell subprocesses (`bash`, `powershell`, `shell`) are not snapshotted and cannot be guaranteed to be restored by `/rewind`; mare flags those turns as having potential unrecorded edits. You can inspect per-turn edits with `/diff`, restore snapshotted files with `/rewind` (or rewind prompts with `/undo`), and run an isolated read-only review with `/review`.

## Snapshots, `/diff`, and `/rewind`

The `changes` plugin stores snapshots beside the session log at `<session-path>.snapshots/` (or under `<state>/snapshots/run-<pid>-<uuid>/` when running with `--no-session`). Storage is capped by `cap_bytes` (64 MiB by default) and an index limit of 20,000 records (oldest turns are trimmed first); individual files larger than 16 MiB can still be written, but their snapshot is recorded as missing.

### Inspecting changes with `/diff` and `Ctrl+R`

- `/diff` opens an overlay listing every turn that modified files through mare tools, newest first, with `+adds / -dels` counts and unified diffs.
- `Ctrl+R` (or `/review changes`) opens the multi-scope change viewer: mare's per-turn edits (`turns`), plus git `unstaged`, `staged`, and `committed` (last commit) diffs. Diffs longer than `collapse_lines` (default 400) start collapsed.

If a turn ran shell commands (`bash`, `powershell`, or `shell`), mare flags it because shell side effects cannot be snapshotted by the file guard.

### Restoring files with `/rewind`

Run `/rewind` (or `/rewind <turn-id>`) to pick a turn. When the session supports conversation rewind and has a recorded history point before that turn, mare offers three actions (otherwise it restores files only):

| Option | Effect |
|---|---|
| Restore files | Puts files modified in that turn and later back to their state before the turn. Conversation history stays unchanged. |
| Restore files and the conversation | Restores the files, branches the session tree back to before the turn, and puts that turn's prompt back into your input box. |
| Summarize from here | Leaves files on disk untouched and folds the conversation from that turn onward into a summary. |

Safety checks during restore:

- **External conflict check**: If a file changed outside mare since the turn recorded it, `/rewind` lists the conflicting paths and asks before overwriting them.
- **Link refusal**: Restores never write through symlinks or hard links. Any path that became a link is refused and reported.
- **Trimmed or oversized backups**: If a backup was trimmed under `cap_bytes` or the 20,000-record index limit, or skipped because the original file exceeded 16 MiB, `/rewind` reports it as missing rather than guessing.

Configure snapshot retention in `init.lua`:

```lua
mare.plugin("changes", {
  cap_bytes = 128 * 1024 * 1024,
})
```

### Conversation-only `/undo`

`/undo` (or pressing `Esc` twice on an empty idle input) rewinds only the conversation to before a chosen prompt and places that prompt back in the editor, without modifying files on disk. Undone turns remain on their original branch in the session JSONL file (`/tree`).

## Read-only code review (`/review`)

The `review` plugin runs a one-shot read-only subagent session using the `read-only` permission preset and read-only tools (`read`, `ls`, `bash` inside the read-only sandbox). Review findings render directly in the transcript and never enter your main conversation's context window.

```text
/review                                    # pick a target interactively
/review uncommitted                        # staged, unstaged, and untracked files
/review <sha>                              # review a single commit (also /review commit <sha>)
/review base main                          # diff against base branch
/review focus on race conditions and locks # custom review instructions
/review changes [path]                     # open the diff viewer instead of running an agent review
```

Configure the reviewer in `init.lua`:

```lua
mare.plugin("review", {
  collapse_lines = 400,
  scopes = { "turns", "unstaged", "staged", "committed" },
  prompt_diff_bytes = 120 * 1024,
  tools = { "read", "ls", "bash" },
})
```

To mask `Ctrl+R` in the editor, call `mare.keymap.set("editor", "<C-r>", false)`. To use it for history search instead, bind it explicitly to `"history-search"`: `mare.keymap.set("editor", "<C-r>", "history-search")`.

## Related

- [Sessions](sessions.md) for `/tree`, `/fork`, and session JSONL durability
- [Permissions and sandbox](permissions.md) for the `read-only` sandbox used by `/review`
- [Appearance](../configure/ui.md) for diff highlight groups (`MareDiffAdd`, `MareDiffDelete`)

# Context and instructions

## What the model sees

Each request sends the system prompt, tool definitions, and conversation history. The system prompt combines named sections: identity line, general guidance, tool list, active sandbox and approval policy, instruction files, skills index, and working directory.

mare builds the system prompt once per session and model to preserve provider prompt caching, rebuilding only when inputs change. Run `/context` for section token counts, or `/context all` for per-item sizes (instruction files, skills, tool definitions, largest messages).

## Instruction files

mare reads these files, in this order:

1. A user file in the config directory: `~/.config/mare/AGENTS.md` (or `AGENTS.override.md`, `CLAUDE.md` there).
2. For the project, every directory from the project root down to the working directory. The project root is the nearest ancestor that contains `.git`; without one, only the working directory counts.

In each directory the first existing name wins: `AGENTS.override.md` (a local override you do not commit), then `AGENTS.md`, then `CLAUDE.md`. A symlink to a file already loaded, or a copy of it, counts once. Blank files are ignored.

```text
~/.config/mare/AGENTS.md          user instructions, loaded everywhere
/repo/AGENTS.md                   project root
/repo/services/api/AGENTS.md      loaded when you start in services/api
```

Files are separated in the prompt as `<project_instructions path="...">` blocks, with the user file first and then root to working directory. The total is capped at 32 KiB (`project_doc_max_bytes`). When the cap is reached the files farthest from the working directory are dropped first, the file that crosses the cap is cut with a `[truncated]` note, and a note names any omitted files.

When the model reads a file below the working directory, the `AGENTS.md` of each directory between the working directory and that file is appended to the read result once, within the same size budget. A monorepo can therefore keep rules next to the code they cover.

An `AGENTS.md` from a project you have not trusted is still loaded, because it only instructs the model, but it is labelled as coming from an untrusted project. Project instruction files that resolve outside the project root, or follow symlinks inside an untrusted workspace, are skipped instead of loaded. See [Permissions](permissions.md).

Options of the `context` plugin:

```lua
mare.plugin("context", {
  files = { "AGENTS.override.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md" },  -- names, in priority order
  project_root_markers = { ".git" },       -- {} = only the working directory
  project_doc_max_bytes = 64 * 1024,
})
```

### Creating and editing them

- `/init` asks the model to analyze the repository and create or improve `AGENTS.md` at the root: build and test commands, architecture, and conventions actually used.
- `/memory` lists the instruction files in effect, with sizes; Enter shows one. `/memory add <text>` appends a bullet to the project's `AGENTS.md`, `/memory add --user <text>` to the user file, creating it when needed.
- You can edit the files in any editor. Before each prompt mare compares the modification time and size of the instruction files, `SYSTEM.md`, `APPEND_SYSTEM.md` and every `SKILL.md`, and rebuilds the system prompt if anything changed. `/memory add` tells you its note applies from the next session; `/new` starts one.

## System prompt files

| File | Effect |
| --- | --- |
| `SYSTEM.md` | Replaces the whole default system prompt. |
| `APPEND_SYSTEM.md` | Adds text at the end of the default prompt. |

mare looks in `.mare/` of the project resource root (the nearest directory up to the git root containing `.mare/` or `.mcp.json`; only if the project is trusted), then in `~/.config/mare/`. Replacing the prompt removes every default section, including the sandbox and tool descriptions, so the model no longer sees them. mare says so at startup and in `mare doctor`; use `APPEND_SYSTEM.md` unless you mean it.

The same can be set in `init.lua` or on the command line:

```lua
mare.plugin("context", {
  append = "Always answer in French.",        -- like APPEND_SYSTEM.md
  persona = "You are a careful Rust reviewer.", -- replaces only the identity line
  -- system = "...",                          -- like SYSTEM.md: replaces everything
})
```

```sh
mare --append-system-prompt "Prefer small commits." "tidy up the tests"
```

Add or remove individual sections with `mare.prompt.section { name = ..., order = ..., text = ... }` and `mare.prompt.remove(name)`.

## Skills

A skill is a directory with a `SKILL.md` file: YAML frontmatter with a `name` and a `description`, then instructions. mare follows the Agent Skills layout, so skills written for other agents work unchanged.

```text
~/.config/mare/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from the commits since the last tag. Use when asked to prepare a release.
---

1. Run `git describe --tags --abbrev=0` to find the last tag.
2. List the commits since then and group them by area.
3. Write the notes in `RELEASE_NOTES.md`.
```

Skills are discovered in these directories; when two skills share a name the first found wins:

| Directory | Scope |
| --- | --- |
| `~/.config/mare/skills` | User. |
| `~/.agents/skills` | User, shared with other agents. |
| `.mare/skills` | Project, in the project resource root (nearest `.mare/` or `.mcp.json`, up to the git root). Trusted projects only. |
| `.agents/skills` | Project, resolved against the same project resource root (`mare.project.root`). Trusted projects only. |
| `skills/` in installed [packages](../configure/packages.md) | Package. |

Change the list with the `skill_dirs` option. Relative entries count as project directories and need trust.

Only the name, description and location of each skill go into the system prompt. When a task matches a description, the model reads the full `SKILL.md` with the `read` tool. This keeps the prompt small however many skills you install. A skill whose frontmatter sets `disable-model-invocation: true` is left out of the list and used only when you ask for it.

To load a skill yourself, run `/skill:<name>` with optional text after it:

```text
/skill:release-notes for v2.4
```

The skill's instructions are sent as a message, followed by your text. `/skills` opens a browsable list of skills and shows any warnings from the scan. A skill without a `description` is skipped; other problems (a name that is not lowercase letters, digits and hyphens, a description over 1,024 characters) are warnings and the skill still loads. `mare doctor` reports them too. `allowed-tools` is read but not enforced.

## Prompt templates

A Markdown file in `~/.config/mare/prompts/` or `.mare/prompts/` (project ones need a trusted project) becomes a slash command named after the file. Packages can ship templates in their `prompts/` directory. A template never replaces an existing command.

```markdown
---
description: Review the staged changes
argument-hint: "[focus]"
---
Review the output of `git diff --staged`. Focus on: ${@:-correctness and tests}.
```

Saved as `~/.config/mare/prompts/review-staged.md`, this runs as `/review-staged security`. (A name that is already a command, such as the built-in `/review`, is skipped.) Arguments: `$1`, `$2` and so on, `$@` or `$ARGUMENTS` for all of them, `${1:-default}` and `${@:-default}` for defaults, and `${@:N}` or `${@:N:L}` for a slice. Quoted words stay together. Set `prompt_dirs` to change where templates are read from.

## File mentions

`@path` in your message attaches the file's contents to that message, `@"path with spaces"` quotes a path, and a directory attaches its listing. Limits are 100 KiB per file and 256 KiB per message. See [Interactive interface](tui.md). On the command line, a separate argument that starts with `@` attaches a text file to the first prompt: `mare "review" @src/lib.rs`. The command line reads the whole file with none of those limits, and an image or an unreadable path stays as written.

## How compaction treats context

When the conversation approaches the context window, mare compacts conversation messages while keeping the original history in the session file. The system prompt, tool definitions, skill headers, and instruction files are not part of the conversation: they are sent again with every request, are never summarized, and keep counting against the window. A very large `AGENTS.md` therefore costs tokens on every request.

Anything the model must keep across a compaction belongs in `AGENTS.md`, not only in the conversation. Use `/compact <focus>` to tell a manual summary what to preserve. For the four compaction stages, retention budgets, thresholds, and options, see [Usage and compaction](usage-and-compaction.md).

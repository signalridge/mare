# Skills and prompt templates

Skills package reusable `SKILL.md` instructions that load on demand. Prompt templates turn Markdown files into `/command` shortcuts with shell-style argument expansion.

## Skills

A skill is a directory containing a `SKILL.md` file with YAML frontmatter and Markdown instructions:

```text
~/.config/mare/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from commits since the last tag. Use when asked to prepare a release.
argument-hint: "[tag]"
---

1. Run `git describe --tags --abbrev=0` to find the previous tag.
2. Group commits since that tag by area.
3. Write the draft in `RELEASE_NOTES.md`.
```

Only each skill's `name`, `description`, and file location enter the system prompt. When a task matches the description, the model reads `SKILL.md` with the `read` tool. You can install dozens of skills without bloating every turn.

### Discovery order and trust

The `context` plugin scans these directories in order. When two skills share a `name`, the first one found wins:

| Order | Directory | Scope | Trust required |
|---|---|---|---|
| 1 | `~/.config/mare/skills/` | User | No |
| 2 | `~/.agents/skills/` | User, shared across agents | No |
| 3 | `.mare/skills/` | Project resource root (nearest `.mare/` or `.mcp.json` up to `.git`) | Yes |
| 4 | `.agents/skills/` | Resolved against the same project resource root (`mare.project.root`) | Yes |
| 5 | `<package>/skills/` | Installed [packages](../configure/packages.md) | No |
| 6 | `~/.local/share/mare/runtime/skills/` | Bundled skills shipped with mare (for example `plugin-authoring`) | No |

Change search paths with `skill_dirs` in `init.lua`. Relative entries resolve via `mare.project.root` and require [project trust](permissions.md#project-trust). User, project, and package skills always shadow a bundled skill with the same name.

```lua
mare.plugin("context", {
  skill_dirs = { "~/.config/mare/skills", "~/.agents/skills", ".mare/skills", ".agents/skills" },
})
```

> Startup trust detection walks ancestor directories up to `.git` for `.agents/skills`, so an ancestor `.agents/skills/` can trigger a trust prompt even though the skill loader only scans the resolved project root.

### Frontmatter fields and validation

| Field | Required | Effect |
|---|---|---|
| `name` | No | Defaults to the directory name. Lowercase `a-z`, `0-9`, and hyphens (`-`), up to 64 characters. |
| `description` | Yes | Shown to the model and `/skills`. Skills without a `description` are skipped. Maximum 1024 characters. |
| `argument-hint` | No | Shown after `/skill:<name>` in the `/` menu. |
| `disable-model-invocation` | No | When `true`, omits the skill from the system prompt so it runs only when you invoke `/skill:<name>`. |
| `compatibility` | No | Checked up to 500 characters; diagnostics warn if exceeded. |
| `allowed-tools` | No | Parsed from frontmatter, not enforced by mare. |

Naming and length issues produce warnings instead of blocking startup. Run `/skills` or `mare doctor` to inspect skipped skills and warnings.

### Running a skill manually

Invoke any loaded skill as a slash command, with optional arguments appended after the instructions:

```text
/skill:release-notes v0.2.0
```

Running `/skills` with no arguments opens a searchable picker of loaded skills and prints any scan warnings. Before each prompt, mare checks modification times and sizes on `SKILL.md` files and rebuilds the cached system prompt if anything changed.

## Prompt templates

A Markdown file under `~/.config/mare/prompts/` or `.mare/prompts/` registers a slash command named after the file (without `.md`). Installed packages can also ship templates in `prompts/`.

When names collide, project templates override user templates, and user templates override package templates. A prompt template never shadows an existing built-in or plugin command such as `/review`.

```markdown
---
description: Review staged changes before committing
argument-hint: "[focus]"
---
Inspect the output of `git diff --staged`.
Focus on: ${@:-correctness, edge cases, and tests}.
```

Save this as `~/.config/mare/prompts/review-staged.md` and run:

```text
/review-staged concurrency and error paths
```

### Argument substitution

Templates support shell-style quoting (for example `"error paths"`) and these placeholders:

| Placeholder | Expands to |
|---|---|
| `$1`–`$9` | Single-digit positional argument (empty string if omitted; bare `$10` expands as `$1` plus literal `0`) |
| `$@`, `$ARGUMENTS` | All arguments joined by spaces |
| `${N:-default}` | Positional argument `N` (supports multi-digit indices such as `${10:-fallback}`), or `default` when omitted/empty |
| `${@:-default}` | All arguments, or `default` when none are given |
| `${@:N}` | Arguments starting from index `N` (supports multi-digit `N`) |
| `${@:N:L}` | Up to `L` arguments starting from index `N` (supports multi-digit `N`, `L`) |

Override the template directories in `init.lua` with `prompt_dirs`:

```lua
mare.plugin("context", {
  prompt_dirs = { "~/.config/mare/prompts", ".mare/prompts" },
})
```

Relative paths in `prompt_dirs` load only after the project is trusted (`mare trust`).

## Related

- [Context and instructions](context.md) for `AGENTS.md`, `SYSTEM.md`, and `APPEND_SYSTEM.md`
- [Packages](../configure/packages.md) for sharing skills and prompts in git packages
- [Permissions and sandbox](permissions.md#project-trust) for how project trust is stored and checked

# 上下文与指令

## 模型看到什么

每次请求包含系统提示词、工具定义与对话历史。系统提示词由命名分段拼装：身份说明、通用指引、工具列表、当前沙箱与审批策略、指令文件、技能索引以及工作目录。

mare 对每个会话与模型只构建一次系统提示词以保持提示词缓存有效，仅在输入变化时重建。运行 `/context` 查看各分段 token 数，`/context all` 查看逐项明细（指令文件、技能、工具定义与最大消息）。

## 指令文件

mare 按以下顺序读取这些文件：

1. 配置目录中的用户文件：`~/.config/mare/AGENTS.md`（或那里的 `AGENTS.override.md`、`CLAUDE.md`）。
2. 对于项目，从项目根目录一直到工作目录的每个目录。项目根目录是包含 `.git` 的最近祖先目录；没有的话，只算工作目录。

在每个目录中，第一个存在的名称生效：`AGENTS.override.md`（你不提交的本地覆盖），然后是 `AGENTS.md`，再是 `CLAUDE.md`。指向已加载文件的符号链接或其副本只算一次。空白文件会被忽略。

```text
~/.config/mare/AGENTS.md          user instructions, loaded everywhere
/repo/AGENTS.md                   project root
/repo/services/api/AGENTS.md      loaded when you start in services/api
```

在提示词中，各文件以 `<project_instructions path="...">` 块分隔，用户文件在最前，然后从根目录到工作目录依次排列。总量上限为 32 KiB（`project_doc_max_bytes`）。达到上限时，离工作目录最远的文件先被丢弃，跨过上限的那个文件会被截断并带有 `[truncated]` 说明，并且有一条说明列出被省略的文件。

当模型读取工作目录下面的文件时，工作目录与该文件之间每个目录的 `AGENTS.md` 会在同一大小预算内，一次性追加到读取结果中。因此 monorepo 可以把规则放在它们所涵盖的代码旁边。

来自你尚未信任的项目的 `AGENTS.md` 仍会被加载，因为它只是向模型下指令，但会被标注为来自不受信任的项目。解析到项目根目录之外、或在不受信任工作区中通过符号链接指向的指令文件会被跳过，不会被加载。见[权限](permissions.md)。

`context` 插件的选项：

```lua
mare.plugin("context", {
  files = { "AGENTS.override.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md" },  -- names, in priority order
  project_root_markers = { ".git" },       -- {} = only the working directory
  project_doc_max_bytes = 64 * 1024,
})
```

### 创建和编辑

- `/init` 让模型分析仓库，并在根目录创建或改进 `AGENTS.md`：构建和测试命令、架构，以及实际使用的约定。
- `/memory` 列出生效的指令文件及其大小；按 Enter 查看某一个。`/memory add <text>` 向项目的 `AGENTS.md` 追加一条列表项，`/memory add --user <text>` 追加到用户文件，必要时会创建它。
- 你可以用任何编辑器编辑这些文件。每次提示之前，mare 都会比较指令文件、`SYSTEM.md`、`APPEND_SYSTEM.md` 和每个 `SKILL.md` 的修改时间与大小，如有变化就重新构建系统提示词。`/memory add` 会告诉你它的备注从下一个会话起生效；`/new` 会开始一个新会话。

## 系统提示词文件

| 文件 | 效果 |
| --- | --- |
| `SYSTEM.md` | 替换整个默认系统提示词。 |
| `APPEND_SYSTEM.md` | 在默认提示词末尾添加文本。 |

mare 先在项目资源根（从当前目录向上直到 git 根最近的含 `.mare/` 或 `.mcp.json` 的目录）的 `.mare/` 中查找（仅当项目受信任），然后在 `~/.config/mare/` 中查找。替换提示词会移除所有默认部分，包括沙箱和工具说明，因此模型不再看到它们。mare 会在启动时和 `mare doctor` 中提示这一点；除非你确实要这样做，否则请使用 `APPEND_SYSTEM.md`。

同样的设置也可以在 `init.lua` 或命令行中进行：

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

用 `mare.prompt.section { name = ..., order = ..., text = ... }` 和 `mare.prompt.remove(name)` 添加或移除单个部分。

## 技能

技能是一个包含 `SKILL.md` 文件的目录：YAML frontmatter 中有 `name` 和 `description`，后面是指令。mare 遵循 Agent Skills 布局，所以为其他智能体写的技能无需改动即可使用。

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

技能会在以下目录中被发现；当两个技能同名时，先找到的生效：

| 目录 | 范围 |
| --- | --- |
| `~/.config/mare/skills` | 用户。 |
| `~/.agents/skills` | 用户，与其他智能体共享。 |
| `.mare/skills` | 项目，来自项目资源根（最近含 `.mare/` 或 `.mcp.json` 的目录，止于 git 根）。仅限受信任的项目。 |
| `.agents/skills` | 项目，按同一项目资源根（`mare.project.root`）解析。仅限受信任的项目。 |
| 已安装[包](../configure/packages.md)中的 `skills/` | 包。 |

用 `skill_dirs` 选项修改这个列表。相对路径条目视为项目目录，需要信任。

只有每个技能的名称、描述和位置会进入系统提示词。当任务与某个描述匹配时，模型会用 `read` 工具读取完整的 `SKILL.md`。这样无论你安装多少技能，提示词都能保持很小。frontmatter 中设置了 `disable-model-invocation: true` 的技能不会出现在列表中，只有你要求时才会使用。

要自己加载某个技能，运行 `/skill:<name>`，后面可以跟可选文本：

```text
/skill:release-notes for v2.4
```

技能的指令会作为一条消息发送，后面跟着你的文本。`/skills` 打开一个可浏览的技能列表，并显示扫描产生的任何警告。没有 `description` 的技能会被跳过；其他问题（名称不是小写字母、数字和连字符，描述超过 1,024 个字符）只是警告，技能仍会加载。`mare doctor` 也会报告它们。`allowed-tools` 会被读取，但不会被强制执行。

## 提示词模板

`~/.config/mare/prompts/` 或 `.mare/prompts/` 中的 Markdown 文件（项目中的需要项目受信任）会成为以文件名命名的斜杠命令。包可以在它们的 `prompts/` 目录中提供模板。模板绝不会替换已有的命令。

```markdown
---
description: Review the staged changes
argument-hint: "[focus]"
---
Review the output of `git diff --staged`. Focus on: ${@:-correctness and tests}.
```

保存为 `~/.config/mare/prompts/review-staged.md` 后，可以这样运行：`/review-staged security`。（已是命令的名称，例如内置的 `/review`，会被跳过。）参数：`$1`、`$2` 等，`$@` 或 `$ARGUMENTS` 表示全部，`${1:-default}` 和 `${@:-default}` 表示默认值，`${@:N}` 或 `${@:N:L}` 表示切片。带引号的词保持在一起。设置 `prompt_dirs` 可以改变读取模板的位置。

## 文件提及

消息中的 `@path` 会把文件内容附加到该消息，`@"path with spaces"` 用引号包住路径，目录则附加其列表。限制是每个文件 100 KiB、每条消息 256 KiB。见[交互界面](tui.md)。在命令行上，单独一个以 `@` 开头的参数会把文本文件附加到第一个提示词：`mare "review" @src/lib.rs`。命令行会读取整个文件，不受上述限制；图片或无法读取的路径则保持原样。

## 压缩如何处理上下文

当对话接近上下文窗口时，mare 会压缩对话消息，同时把原始历史完整留在会话文件里。系统提示词、工具定义、技能摘要行和指令文件不属于对话历史：它们随每个请求重新发送，不会被摘要，也持续占用上下文窗口。因此特别大的 `AGENTS.md` 会在每个请求上都消耗 token。

任何必须在压缩后保留的约定，都应放在 `AGENTS.md` 里，而不只是写在对话中。用 `/compact <focus>` 可以指定手动摘要要保留的重点。四个压缩阶段、保留配额、阈值和配置项见[用量统计与上下文压缩](usage-and-compaction.md)。

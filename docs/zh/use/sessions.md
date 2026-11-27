# 会话

每个会话是按工作目录存放的只追加 JSONL 文件。每条记录包含 `id` 与父记录，回退或重试会新建分支，旧分支保留在同一文件中。

## 会话存放在哪里

```text
~/.local/state/mare/sessions/<directory-slug>/<timestamp>_<id-suffix>.jsonl
```

状态目录遵循 `$XDG_STATE_HOME`（如果设置了 `MARE_HOME`，则为 `$MARE_HOME/state`）。`<directory-slug>` 是工作目录路径，其中除 Unicode 字母与数字、`.` 和 `_` 之外的每个字符都被替换为 `-`，再用 `--` 包起来。会话文件在写入第一条消息时才创建，而不是在启动时。对于会话文件 `abc.jsonl`，其附件目录为同级的 `abc.attachments/`（从日志中移出的图片），`/rewind` 快照目录为 `abc.jsonl.snapshots/`。没有会话文件时，快照存放在 `<state>/snapshots/run-<pid>-<uuid>/`，压缩图片回退到 `<state>/attachments/<session-id 或 unsaved>/`。

完整的文件布局见[文件](../reference/files.md)。

### 持久性与崩溃恢复

- 追加以整行为单位。末尾没有换行符的一行是撕裂写入：读取方会忽略它，下一次写入会将其截断。
- 在语义边界处（例如一条消息完整时），文件会用 `fsync` 刷到磁盘。写入或同步失败的批次会回滚到之前的长度。
- 以换行符结尾但无法解析的行被视为损坏。mare 绝不会以写入方式打开这样的文件，也不会重写它；你可以只读查看，或从稳定的快照分叉。
- 同一时间只有一个进程写入某个会话，通过会话旁边的锁文件强制保证。如果另一个进程已将其打开，mare 会提供只读视图或分叉。
- mare 打开会话时记录一个 `running` 标记，正常退出时记录一个 `closed` 标记。最后一个标记是 `running` 的会话是被中断的。它在 `/resume` 选择器中显示为已崩溃，`/continue` 可以恢复被中断的一轮。

## 恢复与继续

```sh
mare -c                      # most recent session in this directory
mare -c --all                # most recent session in any directory
mare -r                      # open the picker
mare -r 3f9a1c2e             # by id, or an unambiguous id prefix (headless)
mare -r "fix login bug"      # by name, case-insensitive (headless)
mare -r -2                   # the second most recent session here
mare -r ./some/session.jsonl # by file path
```

| 选项 | 含义 |
| --- | --- |
| `-c`、`--continue` | 继续此目录中最近的会话。 |
| `-r`、`--resume [VALUE]` | 按 id、名称、`-N` 或路径恢复。不带值时打开选择器。 |
| `--all` | 与 `-c` 或 `-r` 一起使用，查看所有目录的会话。 |
| `--fork-session` | 与 `-c` 或 `-r` 一起使用，在副本中继续，原会话保持不变。 |
| `--session-id ID` | 为新会话使用这个 id：8 到 64 个字母、数字、`-` 或 `_`。id 已存在则失败。 |
| `--name NAME` | 为新会话或恢复的会话命名。 |
| `--no-session` | 不保存会话。mare 仍可能写入运行时文件和每次运行的快照，工具也可以写文件。该会话无法恢复，并且与 `-c` 或 `-r` 一起使用会报错。 |

在交互式 UI 中，`-r <text>` 如果不是完整的 id、`-N` 或文件路径，就会打开选择器并把该文本作为搜索词。在无界面运行中，匹配多个会话的名称或前缀会报错并列出候选项；mare 从不替你选择“最新的”。找不到的会话是错误（退出码 1），绝不会悄悄新建会话。

如果你恢复的会话记录于另一个目录，TUI 会询问使用哪个目录。`session` 插件的选项 `resume_cwd` 接受 `"ask"`（默认）、`"current"` 或 `"session"`。

```lua
mare.plugin("session", { resume_cwd = "current" })
```

### 脚本

`mare session new` 在当前目录创建一个空会话并打印它的 id，这样脚本可以先创建会话，再向其中运行提示词：

```sh
id=$(mare session new --name nightly-review)
mare -p --resume "$id" "list the TODO comments in src/"
mare -p --resume "$id" "now rank them by risk"
```

`mare session` 提供 `new`、`list`、`fork`、`archive`、`unarchive` 和 `delete` 子命令（见 [CLI 参考](../reference/cli.md#mare-session)）。它运行轻量级会话宿主，不加载用户或项目配置及插件，仅使用 `--cwd` 以及（用于 `new` 和 `fork` 的）`--name`；`--model`、`--trust`、`--permission` 等运行选项请传给后续的 `mare -p --resume "$id"` 调用。见[无界面模式](headless.md)。

## 会话内操作

| 命令 | 效果 |
| --- | --- |
| `/new`（`/clear`） | 开始新会话。旧会话保留在磁盘上。 |
| `/resume`（`/sessions`） | 打开选择器。`/resume all` 列出所有目录；`/resume search <text>` 进行过滤；`/resume <id or name>` 加载一个会话。一轮运行期间不可用。 |
| `/name <title>`（`/title`、`/rename`） | 给会话命名。不带标题时显示当前名称。 |
| `/fork` | 从你最近的一条消息分叉出新会话。`/fork pick` 让你选择消息。 |
| `/clone` | 把整个会话复制到一个新会话。 |
| `/tree` | 浏览会话树，并从任何较早的位置创建分支。 |
| `/label <text>` | 给树中的当前位置打标签。 |
| `/retry` | 在新分支上重新生成上一条回答。旧回答保留在它的分支上。 |
| `/continue` | 在崩溃或被终止后继续被中断的一轮。 |
| `/archive` | 将会话标记为已归档并开始新会话。文件保留。 |
| `/delete` | 确认后删除此会话。 |

选择器按键：`Ctrl+D` 删除、`Ctrl+R` 重命名、`Ctrl+S` 按创建时间或更新时间排序、`Ctrl+N` 只显示已命名的、`Ctrl+A` 显示已归档的、`Ctrl+X` 归档、`Ctrl+E` 展开、`Ctrl+T` 对话记录、`Ctrl+L` 更改布局。`session` 插件的选项 `picker_style` 可选 `"comfortable"`（默认）、`"dense"` 或 `"cards"`。

### 撤销与回退

`/undo` 和 `/rewind` 不同。

- `/undo` 把对话回退到某条提示词之前，并把该提示词文本放回输入框。磁盘上的文件保持原样。`/undo` 打开选择器；`/undo 2` 回退两条提示词。只有上次压缩之后的提示词可以撤销。在空闲的空输入上连按两次 `Esc` 会打开同一个选择器。被撤销的消息留在文件中它们自己的分支上（见 `/tree`）。
- `/rewind` 恢复文件。在任何工具写入、删除或重命名文件之前，mare 都会保存该文件的原始版本——包括子代理通过文件工具所做的修改（记录在编辑发生时父会话的当前轮次下，通常就是启动它的那一轮；后台子代理跨轮运行时记入较晚的轮次），`/rewind [turn]` 会把这些文件恢复到某一轮之前的样子。你可以在三者中选择：仅恢复文件、恢复文件和对话，或从那一轮起对对话做摘要。

`/rewind` 无法撤销 shell 命令（包括子代理执行的 shell 命令）所做的更改，该轮会对此作出说明。如果某个文件在那一轮之后被 mare 之外的程序修改过，它会被列出，确认后才会恢复。备份有上限（`changes` 插件的选项 `cap_bytes`，默认 64 MiB）；最旧的备份被清理后，会被报告为缺失。`/diff` 显示每一轮改动的文件（包含子代理的修改）。

### 压缩

长会话会被压缩，使其能放进模型的上下文窗口。压缩会在会话 JSONL 文件中追加摘要或裁剪投影记录，原始消息始终完整保留在磁盘上（可通过 `/tree` 或直接查看文件找回）。

```text
/compact                      compact now
/compact keep the migration plan   compact with a focus instruction
```

四个压缩阶段（`prune`、`offload`、`remote`、`local`）、阈值优先级、失败退避与 `stalled` 处理以及 `compact` 插件选项，见[用量统计与上下文压缩](usage-and-compaction.md)；每次请求中不参与压缩的部分见[上下文](context.md)。

### 导出与复制

- `/export [path]` 将对话脱敏后写成私有权限（Unix 上为 `0600`）的 Markdown 文件。不带路径时，文件是工作目录中的 `mare-session-<date>-<time>.md`。`/export-md` 是它的别名。
- `/copy` 把最后一条回复以 Markdown 源码复制到剪贴板。如果其中有代码块，选择器会提供每个代码块或完整回复；在选择器中按 `w` 把选择写入文件，按 `a` 记住“总是复制完整回复”。`/copy 2` 复制倒数第二条回复。通过 SSH 时，剪贴板路径使用 OSC 52。

### 删除会话

`/delete` 删除当前会话及其附件和快照，然后开始一个新会话。`/delete <path>` 在确认后删除该路径上的会话文件。如果另一个进程已打开该会话，则拒绝删除。选择器中的 `Ctrl+D` 对高亮行执行同样的操作。删除无法撤销。

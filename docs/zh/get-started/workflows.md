# 工作流

## 以只读方式探索代码库

```sh
mare --permission read-only "Give me a map of this repository: the main modules, the entry points and how they connect."
```

`read-only` 禁止写入并阻断网络。默认 Windows 层级无法阻断网络，会在每次 shell 或命令工具执行前询问（已批准或规则放行 `all` / `danger-full-access` 除外）。

接着问更具体的问题，并用 `@` 附加文件：

```text
Where is the retry policy decided? Compare @src/client.rs with @src/config.rs.
```

如果想从根本上限制模型能调用的工具，就指定工具名称。`read` 和 `bash` 是内置工具中的两个（其他还有 `write` 和 `edit`）：

```sh
mare --tools read,bash --permission read-only
```

在项目中运行一次 `/init`，让智能体写出 `AGENTS.md`，其中包含构建命令、架构说明和约定。之后的会话会把它作为上下文读取。

## 在审批下修复 bug

描述症状，并要求先写一个失败的测试：

```sh
mare "The CSV export drops the last row when the file has no trailing newline. Reproduce it with a failing test, then fix it."
```

在默认的 `auto` 预设下，工作区内的编辑会直接通过，当某个调用会离开沙箱时 mare 会询问你。对审批框可以回答 `y`（一次）、`a`（本会话）或 `n`（拒绝，并说明原因）。如果想亲自批准每一次写入，运行 `/permission read-only`。每种回答背后的规则见[权限](../use/permissions.md)。

提交之前先检查发生了什么：

- `/diff` 按从新到旧的顺序显示每一轮改动的文件。
- `Ctrl+R` 打开更改查看器：显示 mare 自己每一轮的编辑，或 git 的未暂存、已暂存和最近一次提交的差异。
- 输入 `!git diff --stat` 可以自己运行命令，并把结果放进上下文。

## 审阅更改

`/review` 会启动一个单独的只读会话来审阅代码，并在对话记录中显示发现的问题。这些内容不会进入你对话的上下文。

```text
/review                     pick a target from a list
/review uncommitted         staged, unstaged and untracked changes
/review <sha>               one commit
/review base main           the changes against a base branch
/review focus on error handling and concurrency
```

任何不是目标的文本都会被当作自定义审阅指令。`/review changes [path]` 会打开更改查看器，而不是运行审阅。

## 在脚本或 CI 中以无界面模式运行

`-p` 运行一个提示词，把最终回答打印到标准输出后退出：

```sh
mare -p "Summarize the changes in @CHANGELOG.md in three bullets"
```

当标准输入或标准输出不是终端时，mare 也会自动进入无界面模式。管道输入会被追加到提示词之后；如果你传入 `-` 或不给提示词，管道输入就成为提示词：

```sh
git diff main... | mare -p "Review this diff for bugs"
echo "list the TODO comments in src/" | mare -p -
```

无界面运行无法向你提问。请预先设置权限级别，并为运行设定上限：

```sh
mare -p --permission read-only --max-turns 20 --timeout 10m "Find unused exported functions"
```

| 退出码 | 含义 |
| --- | --- |
| `0` | 已完成 |
| `1` | 错误 |
| `2` | 参数或配置有误 |
| `3` | 一次审批被回答为“拒绝并停止”，这一轮被停止 |
| `4` | 达到 `--max-turns` 或 `--timeout` |
| `130` | 被中断 |

需要机器可读的输出时，使用 `--json`，它每行输出一个 JSON 对象。每行形如 `{"type": ..., "data": ...}`，最先是一条 `session` 记录，最后是一条 `settled` 记录，包含最终文本、用量、`exit_code` 和 `reason`：

```sh
mare --json "Run the tests and report failures" | tail -n 1 | jq '.data | {exit_code, reason}'
```

加上 `--stream-deltas` 可获得 token 级别的事件。无界面运行从不询问项目信任：已保存的决定会生效，否则除非传入 `--trust`，项目的 `.mare/` 文件不会被加载。项目声明的 MCP 服务器需要 `--approve-mcps`。在 CI 中，用 `--no-session`（不保存会话也无法恢复；运行时文件和工具写入仍会发生）和 `--strict-config`（配置有警告时拒绝启动）让运行可复现。详见[无界面模式](../use/headless.md)和 [JSON 流](../integrate/json-stream.md)。

如果要在脚本里于同一个会话中运行多个提示词，先创建会话：

```sh
id=$(mare session new --name nightly)
mare -p --resume "$id" "Step 1: list failing tests"
mare -p --resume "$id" "Step 2: fix the first one"
```

## 继续与分叉会话

```sh
mare -c                  # continue the most recent session in this directory
mare -r                  # open the session picker
mare -r -2               # the second most recent session in this directory
mare -r nightly          # by name, id or unambiguous id prefix
mare -c --fork-session   # continue in a copy and leave the original unchanged
mare -c --all            # most recent session in any directory
```

在会话内：

| 命令 | 作用 |
| --- | --- |
| `/new`（别名 `/clear`） | 开始新会话 |
| `/resume`（别名 `/sessions`） | 选择会话；`/resume all` 列出所有目录，`/resume search <text>` 进行搜索 |
| `/name <title>`（别名 `/rename`、`/title`） | 给会话命名，这样 `-r <name>` 就能找到它 |
| `/fork` | 从你最近的一条消息分叉出新会话；`/fork pick` 让你选择消息 |
| `/tree` | 浏览会话树，并从任何较早的位置创建分支 |
| `/retry` | 重新生成上一条回答；旧回答保留在它自己的分支上 |
| `/btw <question>` | 问一个顺带的问题；回答不会加入对话 |
| `/export [path]` | 把对话导出为 Markdown |

会话是 `~/.local/state/mare/sessions` 中的 JSONL 文件。详见[会话](../use/sessions.md)。

## 撤销与回退

撤销有两个层级，作用不同。

- `/undo` 把对话回退到某条提示词之前。单独的 `/undo` 会让你选择，`/undo N` 取倒数第 N 条。文件保持原样。智能体空闲时连按两次 Esc 会打开同一个选择器。
- `/rewind [turn]` 把文件恢复到某一轮之前的样子。它也可以把对话回退到那个位置（作为 `/tree` 的一个分支，所以旧的会被保留），或者把从那一轮开始的对话折叠成摘要。

在任何工具写入、删除或重命名文件之前，mare 都会保存该文件的原始版本，无论是哪个工具。如果文件在此后被 mare 之外的程序修改过，会被列出来，确认后才会恢复。通过 shell 命令所做的更改无法做快照，该轮会对此作出说明。

## 后台任务

对于耗时较长的命令，智能体会把它们放到后台启动，并继续工作。任务结束时你会收到通知，并附上一小段输出。

- 直接要求：“在后台启动开发服务器，并检查它是否在 8080 端口响应。”
- 命令运行时按 `Ctrl+B`，可以把它移到后台。
- `/jobs` 列出任务，并可打开某个任务的输出或停止它。输入框上方有一行显示正在运行的任务。

## 智能体工作时的引导与排队

你不必等一轮结束。

| 按键 | 智能体工作时的效果 |
| --- | --- |
| Enter | 把消息排入队列；这一轮结束后发送 |
| `Ctrl+S` | 引导：把输入送入正在运行的这一轮 |
| `Alt+Enter` | 明确地作为后续消息排队（Windows 上为 `Ctrl+Q`） |
| Esc | 中断这一轮 |
| `Ctrl+C` | 中断，或清空输入；连按两次退出 |

有消息排队时，在空输入上按 Enter 会弹出一个有三个选项的菜单：立即引导、中断并发送，或继续排队。排队的消息显示在输入框上方；按 Up 选中一条，再按 Enter 就地编辑。

## 使用图片

- 在输入框中按 `Ctrl+V` 可以粘贴剪贴板中的图片（Windows 上终端占用了 `Ctrl+V`，改用 `Alt+V`）。
- 让智能体查看图片文件：“Look at docs/mockup.png and list the UI elements.” 读取工具会把 PNG、JPEG、GIF 和 WebP 文件作为图片返回。

在支持 Kitty、iTerm2 或 sixel 图像协议的终端上，mare 会在对话记录中内嵌绘制图片。其他终端中，对话记录显示 `[image]`。`images` 插件控制绘制哪些图片；见 [UI](../configure/ui.md)。

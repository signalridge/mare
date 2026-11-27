# 快照回退与代码审阅

在 mare 的文件工具（`write`、`edit`、`apply_patch`）修改、删除或重命名文件之前，mare 会把原文件字节记进当前会话的快照日志。Shell 子进程（`bash`、`powershell`、`shell`）产生的文件副作用不会进入快照，无法保证通过 `/rewind` 恢复，mare 只会把这类轮次标为可能存在未记录修改。你可以用 `/diff` 按轮次查看改动，用 `/rewind` 恢复快照文件（或用 `/undo` 只回退对话），也能用 `/review` 启动只读的代码审阅会话。

## 快照、`/diff` 与 `/rewind`

`changes` 插件把快照存放在会话文件旁的 `<session-path>.snapshots/`（使用 `--no-session` 时存在 `<state>/snapshots/run-<pid>-<uuid>/`）。总容量由 `cap_bytes` 控制（默认 64 MiB），索引上限为 20,000 条记录（超出时按最旧轮次裁剪）；单个文件超过 16 MiB 时写入可继续，但快照记为缺失。

### 用 `/diff` 和 `Ctrl+R` 查看改动

- `/diff` 打开浮层，按最新到最旧的顺序列出每轮通过 mare 工具修改的文件、增删行数和统一 diff。
- `Ctrl+R`（或 `/review changes`）打开多视角改动查看器：可切换查看 mare 按轮次的改动（`turns`），以及 git 的工作区未暂存（`unstaged`）、已暂存（`staged`）和最近一次提交（`committed`）。超过 `collapse_lines`（默认 400 行）的 diff 初始处于折叠状态。

如果某一轮里执行过 shell 命令（`bash`、`powershell` 或 `shell`），mare 会标出提示，因为 shell 命令修改的文件无法由文件守卫记录快照。

### 用 `/rewind` 恢复文件

运行 `/rewind`（或 `/rewind <turn-id>`）选定某一轮。当当前会话支持回退对话且记录了该轮之前的历史位置时，会提供三种处理方式（否则只执行恢复文件）：

| 选项 | 效果 |
|---|---|
| 恢复文件（Restore files） | 把该轮及之后修改过的文件还原到该轮开始前的状态，对话历史保持不变。 |
| 恢复文件与对话（Restore files and the conversation） | 还原文件的同时把会话树切回该轮之前，并把当时那条提示放回输入框。 |
| 从此处摘要（Summarize from here） | 不动磁盘文件，把该轮及之后的对话折叠成一条摘要。 |

恢复时的安全保护：

- **外部修改冲突检查**：若文件在快照记录之后被 mare 外部修改过，`/rewind` 会列出冲突路径并在覆盖前请求确认。
- **拒绝链接路径**：恢复过程绝不穿透符号链接或硬链接写入；变成链接的路径会被拒绝并报告。
- **被裁剪或超限的备份**：如果备份因超出 `cap_bytes`、20,000 条索引上限而被清理，或因单文件超过 16 MiB 未能备份，`/rewind` 会报告缺失，绝不会猜测内容。

在 `init.lua` 中调整快照容量：

```lua
mare.plugin("changes", {
  cap_bytes = 128 * 1024 * 1024,
})
```

### 仅回退对话的 `/undo`

`/undo`（或在空闲且输入框为空时连按两次 `Esc`）只把对话退回到某条提示之前并放回输入框，不改动磁盘上的文件。被退回的轮次仍保留在会话 JSONL 的原分支上（可通过 `/tree` 找回）。

## 只读代码审阅（`/review`）

`review` 插件会以 `read-only` 权限预设和只读工具（`read`、`ls` 以及只读沙箱下的 `bash`）启动一次性的子代理会话。审阅结论直接显示在界面记录中，不会进入主对话的上下文窗口。

```text
/review                                    # 交互选择审阅目标
/review uncommitted                        # 审阅暂存、未暂存与未跟踪文件
/review <sha>                              # 审阅指定提交（也可用 /review commit <sha>）
/review base main                          # 对比基准分支的改动
/review focus on race conditions and locks # 按自定义要求审阅
/review changes [path]                     # 打开改动查看器，不运行代理审阅
```

在 `init.lua` 中配置 `review`：

```lua
mare.plugin("review", {
  collapse_lines = 400,
  scopes = { "turns", "unstaged", "staged", "committed" },
  prompt_diff_bytes = 120 * 1024,
  tools = { "read", "ls", "bash" },
})
```

要在编辑器中屏蔽 `Ctrl+R`，写 `mare.keymap.set("editor", "<C-r>", false)`；要把它用于历史搜索，显式绑定 `"history-search"`：`mare.keymap.set("editor", "<C-r>", "history-search")`。

## 相关页面

- [会话](sessions.md)：`/tree`、`/fork` 与 JSONL 会话持久化
- [权限与沙箱](permissions.md)：`/review` 使用的 `read-only` 沙箱
- [外观](../configure/ui.md)：diff 高亮组（`MareDiffAdd`、`MareDiffDelete`）

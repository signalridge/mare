# 退出码

`mare` 在无界面运行、RPC 和 ACP 服务器、交互界面以及子命令下的进程退出码，以及最终 `settled` 记录的 `reason` 取值。

> 本参考页由人工维护（不经 `scripts/gen_docs.py` 自动生成）。

## 无界面运行

`mare -p` 和 `mare --json` 以下列代码之一结束。使用 `--json` 时，相同的代码和与之对应的 `reason` 会出现在最后一条记录中（见 [JSON 事件流](../integrate/json-stream.md)）。

| 代码 | `settled.reason` | 含义 |
|---|---|---|
| 0 | `completed` | 这一轮没有出错地完成。 |
| 1 | `error` | provider、工具或运行时错误。也包括 `-c` 或 `--resume` 找不到的会话，以及 SIGTERM 或 SIGHUP。 |
| 2 | `usage` | 参数或配置有误：未知的标志、没有提示词、无法读取的标准输入、失败的 `init.lua`、`--strict-config` 下的配置警告，或信任问题。运行从未开始。 |
| 3 | `approval_unavailable` | 一次审批被回答为“拒绝并停止”（`abort`），这一轮被停止。 |
| 4 | `max_turns` | 达到 `--max-turns`。 |
| 4 | `timeout` | 达到 `--timeout`。 |
| 5 | `save_failed` | 会话持久化失败。最终答案仍输出到标准输出，未保存的记录写入 `<state>/quarantine/`，并在标准错误输出警告。 |
| 130 | `interrupted` | SIGINT（Ctrl+C）。 |
| 141 | 无 | 标准输出被读取方关闭，例如 `mare --json ... \| head`。这一轮被停止，且不会写入 `settled` 记录。 |

关于这些代码如何选取的几点说明：

- 代码 3 仅用于因一次被回答为 `abort` 的审批而停止的运行。无人回答的审批只是一次被拒绝的调用，而不是停止：模型得到拒绝，若它绕过该拒绝，这一轮仍以 0 完成。无论退出码是什么，每一次拒绝都会列在标准错误上。
- `--timeout` 约束整个运行，包括输出的送达。触发时，mare 取消这一轮，尝试写入 `turn.failed` 和 `settled`（reason 为 `timeout`），并以 4 退出。如果标准输出的读取方被阻塞，流可能在没有它们的情况下结束，因此请将缺失的 `settled` 视为失败。
- 第一次 SIGINT 取消这一轮并正常结束，退出码为 130。第二次 SIGINT 会立即以 130 退出，且不写入 `settled` 记录。
- `--json` 下的用法错误仍会打印 `session`、`turn.failed` 和 `settled` 行，这样读取流的脚本总能得到结果。
- 在无界面运行（`-p` 和 `--json`）之外使用 `--timeout` 会以 2 被拒绝。

```sh
mare -p --timeout 5m --max-turns 30 "Fix the failing tests"
case $? in
  0) echo done ;;
  3) echo "stopped by an approval abort: rerun with a permission preset that allows it" ;;
  4) echo "ran out of turns or time" ;;
  *) echo "failed" ;;
esac
```

## 服务器与界面

| 模式 | 代码 | 时机 |
|---|---|---|
| `mare --rpc` | 0 | 标准输入关闭、收到 `shutdown`，或收到 SIGTERM 或 SIGINT。 |
| `mare --rpc` | 2 | `--strict-config` 下的配置警告、损坏的 `init.lua`，或启动时的信任问题。mare 把消息写到标准错误。 |
| `mare acp` | 0 | 编辑器关闭了连接。 |
| 交互式 | 0 | 正常退出，或启动期间收到 SIGTERM。 |
| 交互式 | 130 | 界面接管之前收到 SIGINT。 |
| 交互式 | 2 | 你在配置文件出错的启动对话框中选择了退出。 |

## 子命令

| 命令 | 代码 |
|---|---|
| `mare doctor` | 所有检查通过时为 0，任何检查失败时为 1。`--json` 使用相同的代码。 |
| `mare session ...` | `new`、`list`、`fork`、`archive`、`unarchive`、`delete`：成功为 0（`new` 和 `fork` 打印 id）；`new` 未创建会话、`delete` 被取消或操作失败时为 1；`--limit` 无效、会话选择器无法解析或存在歧义，或非 TTY 标准输入下执行 `delete` 未传 `--yes` 时为 2。 |
| `mare test` | 全部测试通过为 0，有测试用例失败为 1，参数错误、用例发现失败、宿主启动失败或 spec 加载/语法错误时为 2。 |
| `mare token rotate` | 成功为 0，失败为 1。 |
| `mare web url` | 成功为 0（打印带 `?token=` 的登录链接），没有运行中的监听服务或令牌读取失败时为 1。 |
| `mare login`、`mare logout`、`mare model refresh` | 成功为 0，失败为 1；宿主无法启动或 `--with-api-key` 在标准输入上没有密钥时为 2。 |
| `mare mcp ...` | 0；当操作报告配置错误或失败时为 1。 |
| `mare peer send` | 接收会话接受消息后为 0（打印 `held`、`delivered` 或 `handled`）。目标缺失、消息被拒绝或 30 秒内没有回答时为 1。 |
| `mare sandbox status` | 0。带 `--require-full` 时，除非所有沙箱隔离能力都已完全强制执行，否则为 1（目前在 Windows 上即使增强自检通过也会因未强制每次调用的网络隔离而返回 1）。 |
| `mare sandbox setup` | 成功为 0，失败为 1。在 Windows 以外的平台为 2。 |
| `mare trust`、`mare init`、`mare runtime`、`mare plugin ...`、`mare completion`、`mare rpc schema` | 成功为 0。任何错误都报告在标准错误上，退出码为 1。 |

命令行解析器自身的参数错误（例如未知的子命令）以 2 退出。导致进程结束的崩溃会在 `<state>/crashes/` 下写入报告并以非零状态退出（见[文件](files.md)）。

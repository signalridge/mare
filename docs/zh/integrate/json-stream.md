# JSON 事件流

`mare --json` 以无界面方式运行一个提示词，并向标准输出每行写入一条 JSON 记录。流以 `session` 记录开始，在运行过程中输出公开的智能体事件，最后以 `settled` 记录结束，其中包含最终文本和退出码。本页是脚本可以依赖的约定。

## 运行

```sh
mare --json "Run the tests and fix any failures"
```

`--json` 是 `--mode json` 的简写。提示词是参数；如果传入 `-` 或没有参数且标准输入是管道，则提示词来自标准输入。每条记录都是单独一行 `{"type": ..., "data": ...}`。标准输出上只有记录。警告和被拒绝的审批输出到标准错误。

下面是一次从未真正开始的运行的样子。错误的参数仍然以一条 `settled` 记录结束，所以读取方总能得到一个结果：

```json
{"type":"session","data":{"protocol_version":"3.0","cwd":"/work/app"}}
{"type":"turn.failed","data":{"error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"}}}
{"type":"settled","data":{"text":"","error":{"code":"invalid_arguments","message":"unexpected argument '--bogus' found"},"exit_code":2,"reason":"usage"}}
```

## 记录

每一行有两个键：

| 键 | 含义 |
|---|---|
| `type` | `session`、事件名称、`lagged`、`dropped`、`turn.failed` 或 `settled`。 |
| `data` | 记录的载荷，始终是对象。 |

读取方应忽略不认识的键。新字段会不经通知地添加，且不会改变现有字段的含义。

### `session`

始终是第一行。

| 字段 | 含义 |
|---|---|
| `protocol_version` | 协议版本，`major.minor`。 |
| `session_id` | 会话 id。使用 `--no-session` 时它是一个无法恢复的临时 id。 |
| `path` | 会话文件。使用 `--no-session` 时不存在。 |
| `cwd` | 本次运行的工作目录。 |
| `model` | 模型引用（已知时）。 |
| `ephemeral` | 使用 `--no-session` 时为 `true`：不保存会话记录，无法恢复运行，也没有崩溃恢复。其他写入（运行时文件、快照、工具输出）仍可能发生。 |

之后可以用 `session_id` 通过 `mare -p --resume <session_id> "..."` 继续对话（见[会话](../use/sessions.md)）。

### 事件记录

在 `session` 和 `settled` 之间，`type` 是公开智能体事件的名称，`data` 是它的载荷。事件名称的形式为 `area/name`；全部内置公开事件（含投递类别、可忽略标志、载荷字段与示例）见[事件参考](../reference/events.md)。

内部的插件事件从不出现。要把自定义插件事件发布到公开流中，可在服务契约里将其声明为 `expose = "both"`。通过 RPC 时，主连接也可以用 `events.subscribe { session_id: "...", include: [...] }`（或 `{ scope: "all", include: [...] }`）订阅其他 JSON 内核事件。

内置事件在送达之前会按其 schema 检查。载荷必须是纯 JSON。使用保留名称（`session`、`turn.failed`、`settled`、`lagged`、`dropped`）的插件事件会使运行因流错误而失败。用 `mare rpc schema` 打印载荷的 schema；流本身的记录在其 `headless` 键之下。

工具调用结束时，`item/completed` 携带 `item.type == "tool_call"` 的条目，包含 `tool_call_id`、`name`、`message`、`result_status`（`success`、`error`、`denied`、`cancelled` 或 `persist_failed`）与 `execution_status`。这些状态并不说明副作用是否发生。

### 条目级别与增量

默认情况下，流会略过按块触发的 `ignorable` 增量/通知事件（如文本、推理和工具输出的 `item/delta`），只输出生命周期边界与条目事件（`turn/*`、`step/*`、`item/started`、`item/completed`）。

加上 `--stream-deltas` 可以包含 `item/delta` 等 `ignorable` 事件：

```sh
mare --json --stream-deltas "Explain src/main.rs" | jq -r 'select(.type=="item/delta" and .data.kind=="text") | .data.delta'
```

### `lagged` 与 `dropped`

`{"type":"lagged","data":{"from_seq":A,"to_seq":B}}` 表示读取方比 mare 慢导致游标跳过的连续序号范围；`{"type":"dropped","data":{"count":N}}` 表示在背压或拒绝策略下被丢弃的未保留 `lossy` 事件计数。`delivery = "required"` 的事件（`turn/*`、`step/*`、`item/started`、`item/delta`、`item/completed`、`subagent/*`）会等待空位而不会被静默丢弃。

### `turn.failed`

当运行因错误而结束时，会在 `settled` 之前写入 `{"type":"turn.failed","data":{"error":{"code":"...","message":"..."}}}`（可带可选 `data`）。

### `settled`

始终是最后一行。其后没有任何内容。

| 字段 | 含义 |
|---|---|
| `text` | 最终的助手文本。 |
| `usage` | token 计数，以及用量插件报告了费用时的 `cost`。 |
| `error` | 运行失败时的结构化错误对象 `{code, message, data?}`。 |
| `exit_code` | 进程退出码。 |
| `reason` | 运行结束的原因。 |

`reason` 与 `exit_code` 的对应关系如下（见[退出码](../reference/exit-codes.md)）：

| `reason` | `exit_code` | 含义 |
|---|---|---|
| `completed` | 0 | 这一轮已完成。 |
| `error` | 1 | provider、工具或运行时错误，或进程收到 SIGTERM 或 SIGHUP。 |
| `usage` | 2 | 参数或配置有误；运行从未开始。 |
| `approval_unavailable` | 3 | 一次审批被回答为“拒绝并停止”（`abort`），这一轮被停止。无人回答的审批只会被拒绝，运行仍可能以 `0` 完成。 |
| `max_turns` | 4 | 达到 `--max-turns`。 |
| `timeout` | 4 | 达到 `--timeout`。 |
| `save_failed` | 5 | 会话持久化失败；未保存的记录已隔离写入 `<state>/quarantine/`。 |
| `interrupted` | 130 | SIGINT。 |

## 保证

- 第一条记录是 `session`，最后一条是 `settled`，标准输出关闭或输出停止送达的情况除外（见下文）。
- 记录由单一写入方按顺序写入，每条记录都先刷新，下一条才会被接纳。
- 事件顺序就是智能体产生它们的顺序。`lagged` 标记在序列中保持其位置。
- `settled.exit_code` 等于进程退出码。
- 如果读取方消失了（`mare --json ... | head`），mare 会停止这一轮，而不是为无人读取的输出白白消耗 token，并以 141 退出。
- `--timeout` 既涵盖智能体的工作，也涵盖输出的送达。如果读取方永久阻塞，流可能在没有 `settled` 记录的情况下结束，同时在标准错误上给出消息，并以非零退出码退出。请将缺失的 `settled` 视为运行失败。
- 第一次 SIGINT 会取消这一轮，并仍然写入 reason 为 `interrupted` 的 `settled`。第二次会立即以退出码 130 退出，且没有 `settled`。

## jq

打印最终文本，然后以 mare 的退出码退出（Bash）：

```sh
mare --json "List the TODOs in src/" \
  | tee run.jsonl \
  | jq -r 'select(.type == "settled") | .data.text'
code=${PIPESTATUS[0]}
exit "$code"
```

在工具调用结束时跟踪它们：

```sh
mare --json "Update the changelog" \
  | jq -r 'select(.type == "item/completed" and .data.item.type == "tool_call") | "tool: \(.data.item.name)"'
```

只显示结果：

```sh
mare --json "Run the linter" | jq -c 'select(.type == "settled") | .data | {reason, exit_code, usage}'
```

## 脚本

这个 Python 脚本运行一个提示词，打印会话 id 和每个完成的工具调用，并以运行本身的退出码退出。当流在没有 `settled` 的情况下结束时，它会明确失败。

```python
import json
import subprocess
import sys

proc = subprocess.Popen(
    ["mare", "--json", *sys.argv[1:]],
    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True,
)
settled = None
for line in proc.stdout:
    record = json.loads(line)
    kind, data = record["type"], record["data"]
    if kind == "session":
        print("session", data.get("session_id"), data["cwd"])
    elif kind == "item/completed" and (data.get("item") or {}).get("type") == "tool_call":
        print("tool", data["item"].get("name"))
    elif kind == "settled":
        settled = data
code = proc.wait()
if settled is None:
    sys.exit(f"stream ended without a settled record (exit code {code})")
print(settled["reason"], settled["text"][:200])
sys.exit(settled["exit_code"])
```

以 `python3 run.py "Fix the failing test"` 运行。需要双向控制（审批、宿主工具、引导正在运行的一轮）时，请改用 [RPC](rpc.md) 接口。

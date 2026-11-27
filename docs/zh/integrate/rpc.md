# RPC

`mare --rpc` 是 mare 基于标准输入输出 JSON-RPC 2.0 的完整双向编程接口（与[无界面模式](../use/headless.md)、[ACP](acp.md) 和 [Web 监听器](../use/web.md)并列），让任何能启动进程并按行读写的程序都无需特定语言 SDK 即可驱动会话、注册工具和钩子并回答审批。本页涵盖传输、握手、方法分组、mare 回调的内容以及对等套接字。

## 启动服务器

```sh
mare --rpc --no-session
```

`--mode rpc` 是同一件事。每条消息是一行上的一个 JSON 对象。标准输出只承载协议消息；日志写入日志文件（见[文件](../reference/files.md)）。该进程通过标准输入和标准输出服务一个宿主：没有端口，也没有认证，因为只有父进程持有管道。

关闭标准输入（或读取错误）会结束会话、取消活动的一轮，并以退出码 0 退出。一次性的提示词请改用无界面模式（见[无界面模式](../use/headless.md)）。

用下面的命令打印协议的 JSON Schema：

```sh
mare rpc schema > mare-rpc.schema.json
```

输出是一个目录，而不是一整条消息的 schema。它的顶层键是 `envelope`、`methods`、`events`、`callbacks`、`notifications`、`client_notifications`、`errors`、`headless`、`ui_image_resource` 和 `ui_output_image_key`，另外还有 `protocol_version`（`events` 合并了原生协议事件与内置服务契约中声明的事件）。条目之间用 `$ref` 相互指向，所以请针对完整目录来解析。协议字段与目录键统一使用 `snake_case`（`protocol_version`、`callback_timeout_ms`、`turn_id`、`session_id`、`registration_id`、事件和契约服务方法）。

## 最小客户端

这个 Python 客户端启动 mare，执行握手，订阅事件，发送一个提示词并等待回答。它只使用标准库。带命令行参数的可直接运行示例见 [`examples/rpc`](../../../examples/rpc/README.md)（英文）。

```python
import json
import subprocess

proc = subprocess.Popen(
    ["mare", "--rpc", "--no-session"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)
next_id = 0


def send(msg):
    msg["jsonrpc"] = "2.0"
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def handle(msg):
    """Notifications and callbacks that arrive while we wait for a response."""
    method = msg["method"]
    if method == "event":
        print("event", msg["params"]["type"])
    elif method == "approval.request":
        send({"id": msg["id"], "result": {"decision": "deny"}})
    elif "id" in msg:  # a callback we did not declare support for
        send({"id": msg["id"], "error": {"code": -32601, "message": "not supported"}})


def call(method, params=None):
    global next_id
    next_id += 1
    rid = next_id
    send({"id": rid, "method": method, "params": params or {}})
    for line in proc.stdout:
        msg = json.loads(line)
        if "method" in msg:
            handle(msg)
        elif msg.get("id") == rid:
            if "error" in msg:
                raise RuntimeError(msg["error"])
            return msg["result"]
    raise RuntimeError("mare closed stdout")


call("initialize", {
    "protocol_version": "3.0",
    "client": {"name": "demo", "version": "0.1"},
    "capabilities": {"approvals": True},
})
session = call("session.create")
sid = session["session_id"]
call("events.subscribe", {"session_id": sid})
started = call("agent.prompt", {"session_id": sid, "input": "Summarize this repository"})
result = call("agent.wait", {"session_id": sid, "turn_id": started["turn_id"]})
print(result.get("text", ""))
call("shutdown")
proc.wait()
```

来自 mare 的请求（回调）带有 `id` 和 `method`；你的客户端必须回答每一个。通知有 `method` 而没有 `id`。上面的循环在等待自己的响应时同时处理这两种消息。

## 握手

`initialize` 必须是第一个请求，且每条连接最多成功一次。在连接上 `initialize` 成功之前，除 `shutdown` 之外的任何其他请求都会以 `-32600` 失败。后续能力变更请使用 `connection.set_capabilities`。

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version":"3.0",
  "client":{"name":"my-host","version":"1.0"},
  "capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":false},
  "callback_timeout_ms":30000,
  "approval_timeout_ms":120000}}
```

| 参数 | 含义 |
|---|---|
| `protocol_version` | 必填的 `major.minor`。缺失、null、非字符串、格式错误或不同的主版本会以 `invalid_arguments` / `unsupported_version` 失败（可修正后重试 `initialize`）。 |
| `client` | `{name, version?}`，仅供参考。 |
| `capabilities` | 你会回答的回调：`callbacks`（工具、钩子、服务、provider、web 与渲染器的统一 `callback.call`）、`ui`、`approvals`、`elicitation`。默认全部为 `false`。mare 绝不会发送你没有声明的回调。 |
| `callback_timeout_ms` | 普通回调的期限（毫秒）。默认 30000，钳制到 `1..=3600000`。 |
| `approval_timeout_ms` | 人工审批的期限。没有它时，审批会无期限地等待。 |

结果包含 `protocol_version`、`server`（`{name, version}`）、`capabilities`，以及进程启动时打开的 `session` 快照（若有，包含 `session_id` 与 `session_epoch`）：

| 字段 | 含义 |
|---|---|
| `methods` | 此连接可以调用的宿主到 mare 的方法。 |
| `callbacks` | 你声明的能力中处于活动状态的子集。在对等连接上，`ui`、`approvals` 和 `elicitation` 为 `false`。 |
| `session` | 可选的启动会话快照。调用会话作用域方法时请将 `session.session_id` 作为 `session_id` 传入。 |

次版本会添加可选的字段、方法和事件。删除或重命名内容会提升主版本。`3.0` 不保留 `2.x` 兼容层。

## 宿主到 mare 的方法

| 分组 | 方法 |
|---|---|
| 会话 | `session.create`、`session.load` `{session_id?, path?}`、`session.get` `{session_id}`、`session.fork` `{session_id, before_item_id?}`、`session.list`、`session.list_items` `{session_id}`、`session.add_record`、`session.list_records`、`session.archive`、`session.unarchive`、`session.delete`、`session.rewind`、`session.search`、`session.snapshot`、`session.close` |
| 提示 | `agent.prompt` `{session_id, input}`、`agent.wait` `{session_id, turn_id?}`、`agent.steer` `{session_id, input}`、`agent.cancel` `{session_id}`、`agent.continue` `{session_id}`、`agent.get` `{session_id}`、`agent.list_messages` `{session_id}`、`agent.list_queue` / `agent.update_queued` / `agent.delete_queued`、`compact.run` `{session_id, instructions?}` |
| 状态 | `state.snapshot`、`state.compare_exchange`、`status.get`、`llm.list_models`、`flags.list`、`flags.get`、`workspace.get` `{cwd}`、`workspace.set_trust` `{cwd, trusted, save?}`、`permissions.get` `{session_id}`、`permissions.list` `{session_id}`、`permissions.set` `{session_id, name, confirm?}`、`commands.run` `{line, input?, session_id?}` |
| 选项 | `options.get` `{session_id, keys?, describe?}`、`options.set` `{session_id, values}`、`config.describe` `{keys?}` |
| 扩展 | `events.list`、`events.subscribe` `{session_id?, scope?, session_epoch?, last_seq?, include?, exclude?}`、`baseline.read`、`tools.register` `{name, description, parameters, callbacks: ["execute"], side_effect?, scope?, session_id?, ...}`、`hooks.subscribe` `{event, session_id?, scope?, callbacks: ["handle"], ...}` |
| 服务 | `services.list`、`services.register`、`registrations.release` `{registration_id}`；契约中每个远程方法直接以 `service.method` 调用（`theme.*`、`llm.*`、`permissions.*`、`usage.get`、`web.*`、`ui.*`，见 `mare rpc schema`） |
| 输出恢复 | `spill.list_backends`、`spill.save`、`spill.read`、`spill.delete` |
| 生命周期 | `initialize`、`shutdown` |
| 对等 | `peer.info`、`peer.message`（见[对等套接字](#对等套接字)） |

### 提示

`agent.prompt` 接受 `{session_id, input, when_busy?, request_id?, output_schema?}`，返回 `{status, turn_id?, input_id?, duplicate?}`。`input` 是文本或内容块数组。忙时默认排队，设置 `when_busy: "reject"` 可拒绝。使用 `agent.wait {session_id, turn_id}` 获取结果，或通过 `event` 通知跟踪；`agent.steer` 向运行中的轮次提供输入，`agent.cancel` 取消轮次。`request_id` 支持会话内幂等重试。

#### 安全重发（`agent.prompt` 与 `agent.steer`）

收到错误不一定表示什么都没发生。按 `error.data.code` 分为两类：

- **提交前拒绝：** 没有入队、没有开始轮次、没有记录消息，`request_id` 也没有被记住；修正原因（或重连）后可以原样重发：`parse_error`、`invalid_request`、`method_not_found`、`overloaded`、`denied`、`invalid_arguments`、`unknown_session`、`unsupported`、`request_id_conflict`、`busy`、`agent.rejected`、`agent.no_active_turn`、`agent.turn_mismatch`。
- **不确定：** 输入可能已经提交：`internal`、`cancelled`、`stale_session`、`unavailable`、`invalid_result`、`persist_failed`、`timed_out`、`disconnected`、其他任何错误码，以及收到响应前连接已关闭。

`request_id_conflict` 不会改动原请求及其结果。`busy` 和 `agent.rejected` 只由 `agent.prompt` 返回；`agent.no_active_turn` 和 `agent.turn_mismatch` 只由 `agent.steer` 返回。

遇到不确定的失败后，用同一个 `request_id` 重发同一请求：已提交的 `agent.prompt` 返回原来的 `turn_id` 或 `input_id`，已提交的 `agent.steer` 返回原来的 `input_id`，并带 `duplicate: true`，不会执行两次。输入被前端转到别处（例如当前聚焦的侧边对话）时同样如此；第一次请求尚未完成时到达的重发会等待它的结果。没有带 `request_id` 时，重发前请先查看 `agent.list_queue` 和 `agent.list_messages`。

### 选项

`options.get` 会对敏感值（凭据、provider 密钥和请求头）做脱敏，与 `/config` 相同。带 `describe: true` 时，它为每个键返回 `{effective, default?, source?, description?, type?, deprecated?, sensitive, redacted}`。`options.set {session_id: "...", values: {model: "...", effort: "..."}}` 是设置 `mare.o.<name>` 的 RPC 形式。在主连接的默认会话上提交的 `options.set` 写入会更新运行期默认值，且仅在 `session.create` 时按运行期写入重放（不会提升到 `cli` 配置层）；`session.load` 和 `session.fork` 不会重放 `options.set` 默认值，而是还原目标会话自身记录的状态。见[选项](../configure/options.md)。

### 宿主工具

```json
{"jsonrpc":"2.0","id":2,"method":"tools.register","params":{
  "name":"lookup",
  "description":"Look up a ticket",
  "side_effect":"read",
  "parameters":{"type":"object","properties":{"ticket_id":{"type":"string"}},"required":["ticket_id"]},
  "callbacks":["execute"]}}
```

每次 `tools.register` 调用注册一个归属该连接的工具并返回 `{registration_id}`（可传 `scope: "all"|"session"` 与 `session_id` 限定单个会话）。`name`、`description`、`parameters` 与 `callbacks: ["execute"]` 为必填项，副作用用 `side_effect`（`"read"`、`"write"` 或 `"external"`）标注；远端工具始终在本地沙箱外非受限运行。你需要声明 `capabilities.callbacks: true`；mare 通过 `callback.call`（`callback: "execute"`，`arguments: {args, ctx: {tool_call_id?, idempotency_key?, cwd?}}`）调用工具，期望返回字符串或 `{content, is_error?, terminate?, details?}`。耗时工具可通过客户端到 mare 的 `callback.update {id, partial}` 通知（使用待处理 `callback.call` 的 JSON-RPC 请求 `id`）发送增量进度。通过 `registrations.release {registration_id}`（返回 `{}`）释放登记；连接断开时，该连接持有的所有登记也会自动移除。

### 钩子

`hooks.subscribe {event, prepend?, timeout_ms?, scope?, session_id?, callbacks: ["handle"]}` 需要 `capabilities.callbacks: true`，并返回 `{registration_id}`（通过 `registrations.release` 释放）。`prepend: true` 让监听器排在先前注册的监听器之前。不要向 `hooks.subscribe` 传 `mode` 参数：内核会根据 `event` 推断分发模式（`agent/queue` 和 `status/changed` 为 `emit`；`request/prepare` 为 `serial`；`request/build`、`tool/result` 和 `tool/rewrite` 为 `waterfall`；`request/ready`、`tool/before`、`tool/approval` 和 `net/approval` 等决策点为 `policy`），并在 `callback.call`（`callback: "handle"`，`arguments: {event, mode, args}`）中传给回调。

### 服务与输出恢复

声明 `capabilities.callbacks: true` 后，宿主可通过 `services.register {name, methods, scope?, session_id?}` 注册服务并返回 `{registration_id, name}`（通过 `registrations.release` 释放）。服务名使用小写字母、数字、`_` 和 `-`，不能与协议根名冲突。对已有契约且全部方法可跨线传递的服务，省略 `parameters`、`result`、`order`、`expose`、`access`、`scope`（或原样重复）。注册新服务时，每个方法声明 `parameters`、`result` 及可选的 `order`、`doc`、`optional`、`expose`（默认 `"both"`，或 `"lua"`）、`access`（默认 `"primary"`，或 `"peer"`、`"peer_default"`）与 `scope`（默认 `"host"`，或 `"workspace"`、`"session"`）。`services.list` 列出可见的远端契约方法，调用时直接写 `service.method`。

可恢复的工具输出按会话存放，通过 `spill.list_backends {}`、`spill.save {session_id, content|data_base64, backend?}`（返回 `{spill_id, size}`；base64 解码上限 8 MiB）、`spill.read {session_id, spill_id, encoding?, offset?, limit?}`（`encoding: "text"|"base64"`，返回 `{content|data_base64, next_offset?, size?}`）和 `spill.delete {session_id, spill_id}` 操作。`spill_id` 绑定所属会话，且绝不接受或暴露宿主机文件系统路径。

## 来自 mare 的回调

mare 以请求的形式发送这些回调。对每一个都要用结果或错误来回答。

| 回调 | 参数 | 结果 |
|---|---|---|
| `callback.call` | `{registration_id, callback, session_id?, turn_id?, tool_call_id?, caller_id?, arguments}` | 该登记声明的返回值：宿主工具为 `{content, is_error?, terminate?}`（工具批次全部设置 `terminate: true` 时，除非 `agent/turn-stopping` 继续，否则停止运行）；钩子为 `null`、`{"next":true}` 或 `{"value":...}`；服务/provider/web/渲染器为其声明的 JSON 结果 |
| `approval.request` | `{session_id, turn_id?, kind: "tool"\|"network", approval_id?, tool_call_id?, tool?, arguments, command?, diff?, host?, cwd, reason?, side_effect?, unconfined, proposed_amendment?, requested_permissions?, scope?}` | `{decision, permissions?, scope?, amendment?, reason?}` |
| `mcp.elicitation` | `{session_id, turn_id?, server, request_id?, mode?, message, requested_schema?, url?, elicitation_id?, _meta?}` | `{action, content?}` |
| `ui.select` | `{title, body?, items: [{label, detail?}], selected?}` | `{index}`（`null` = 已取消） |
| `ui.input` | `{title, placeholder?, secret?}` | `{text}`（`null` = 已取消） |
| `ui.confirm` | `{prompt, default?}` | `{confirmed}` |
| `ui.editor` | `{title, prefill}` | `{text}`（`null` = 已取消） |
| `ui.questions` | `{questions}` | `{answers}` |

对于耗时较长的 `callback.call`（例如宿主工具或已注册 provider 流），可发送客户端到 mare 的 `callback.update` 通知 `{id, partial}`，其中 `id` 为正在执行的 `callback.call` 请求 `id`，`partial` 为 JSON 值（工具增量使用如 `{"output": "..."}` 的对象）。

### 审批

`approval.request` 只发送给声明了 `capabilities.approvals` 的主连接。`decision` 的取值：

| 决定 | 效果 |
|---|---|
| `allow` | 运行一次。 |
| `allow_session` | 运行；permissions 插件会在本会话内记住这项授权。 |
| `allow_amend` | 运行，并把 `amendment` 交给 permissions 插件，持久化为一条规则。 |
| `deny` | 拒绝。 |
| `abort` | 拒绝并结束当前一轮。 |

没有回答、回答无效、超时或断开都计为 `deny`。mare 不认识的决定值会被拒绝，并按 `deny` 处理。网络访问需要明确的肯定回答；弃权的监听器不会授予它。

### 钩子的回答

`waterfall` 用 `{"next":true}` 原样继续，用 `{"next":true,"value":...}` 改写后传给下一个监听器，或用 `{"value":...}` 短路；`{}` 会产生 `invalid_result`。`tool/rewrite` 的 `value` 是包含 `arguments` 的调用对象，例如 `{"value":{"id":"call-1","name":"tool","arguments":{"key":"new"}}}`。`policy` 决策放在 `value` 中，例如 `{"value":{"decision":"allow"}}`（也可以是 `ask`、`deny`，并附带可选投票字段）。缺少 `value` 表示弃权；未知决定或字段、其他无效回答、超时、断开或撤回 `capabilities.callbacks` 都会拒绝，第一个 deny 会停止分发。

### 取消与超时

当回调不再活动时，mare 发送 `$/cancel_request {id}`（客户端也可发送 `$/cancel_request {id}` 取消同一连接上协作式进行中的请求）。请关闭它的界面，并忽略之后的任何回答。普通回调在 `callback_timeout_ms` 之后超时。超时或断开时：

| 回调类别 | 结果 |
|---|---|
| 宿主工具 | 工具错误。 |
| 策略钩子和 `approval.request` | `deny`。 |
| `serial` 和 `waterfall` 钩子 | 错误会传给分发方。 |
| UI 调用 | 已取消。 |

## 通知

| 通知 | 参数 | 说明 |
|---|---|---|
| `event` | `{type, data, session_id?, session_epoch?, seq, turn_id?}` | `turn_id` 与 `agent.prompt` 的结果匹配。`seq` 在每个 `(session_id, session_epoch)` 内严格连续递增。 |
| `snapshot` | `{session_id?, session_epoch, seq, baseline_id, retained, parts}` | 在 `events.subscribe` 返回 `mode = "baseline"` 后立即发送，或在慢游标被淘汰发出 `lagged` 后立即发送。 |
| `lagged` | `{from_seq, to_seq}` | 游标落后的序号范围，随后紧跟新的 `snapshot`。 |
| `dropped` | `{count}` | 流中被省略的未保留 `lossy` 或拒收事件计数。 |
| `event` (`status/changed`) | `{type: "status/changed", data: {state, message, session, seq}}` | `state` 为 `idle`、`working` 或 `blocked`。仅在通过 `events.subscribe` 订阅选中时作为 `event` 通知下发（不是独立通知）。忽略比你见过的最后一个更小的 `seq`。 |
| `fs/changed` | `{watch_id, changed_paths, truncated?}` | 活动 `fs.watch` 订阅下经策略过滤的文件系统变更（超过 8192 项时 `truncated: true`）。 |
| `ui.notify` | `{text, ...}` | 给用户的消息。 |
| `ui/select-updated`、`ui/select-closed`、`ui/component-updated` | 见 [RPC 参考](../reference/rpc.md) | 正在打开的 `ui.select` 弹窗或已附着 UI 组件的增量更新。 |
| `stream.failed` | `{error}` | 事件流不完整，连接将关闭。 |
| `$/cancel_request` | `{id}` | 回调或协作请求被取消。 |
| `peer.message_status` | 见下文 | 仅限对等套接字。 |

默认情况下，`event` 携带与 `mare --json` 相同的公开事件（见 [JSON 事件流](json-stream.md)），包括服务契约中声明为 `expose = "both"` 的事件。`events.subscribe {session_id: "...", include: [...]}`（或在主连接上使用 `{scope: "all", include: [...]}`）按名称或 `prefix*` 选择加入更多 JSON 内核事件。进程本地和 provider 私有的事件从不送达。

### 续接与基线同步

当 `events.subscribe` 返回 `mode = "baseline"`（或在收到 `lagged` 后立即收到 `seq = S` 的新 `snapshot`）时，先应用 `snapshot.retained`，再按如下顺序恢复状态：

1. **先缓存 `S` 之后的事件**（`seq >= S + 1`），在分页拉取基线期间不要立即改动尚未对齐的状态。
2. 通过 `baseline.read { baseline_id, part, cursor? }` **分页读取 `snapshot.parts` 中的每个部分**（如 `"entries"`、`"active_items"`），直到不再返回 `next_cursor`。若因 60 秒空闲超时、epoch 变更、重订阅或读完释放而返回 JSON-RPC 错误 `error.data.code = "baseline.stale"`，重新调用 `events.subscribe` 获取新基线（无效的 part 名称会返回 `baseline.unknown_part`，单项超限会返回 `baseline.item_too_large`）。
3. 基线读取完毕后 **按序应用已缓存的事件**，并校验每条增量的 `item/delta.offset`（或内容块 `offset`）是否等于本地长度；若不一致，不带 `last_seq` 重新调用 `events.subscribe` 重建状态。

## 错误

每个错误都有 `error.data.code`，取自与 Lua 错误值共用的同一套词表；`error.data.data` 是该码的附加数据。数字只做分组：JSON-RPC 标准的五个、取消用 `-32800`，其余一律 `-32000`。服务自己的码写作 `<服务>.<名字>`，由服务契约声明，`services.list` 会列出；未声明或无法编码的错误是 `internal`，消息有长度上限。完整列表见 [RPC 参考](../reference/rpc.md#错误码)。

| `data.code` | 数字 | 含义 |
|---|---|---|
| `parse_error` | `-32700` | 解析错误。 |
| `invalid_request` | `-32600` | 无效请求：也包括 `initialize` 之前的调用、重复的进行中 id、超长的行。 |
| `method_not_found` | `-32601` | 未知方法，或 peer socket 上不允许的方法。 |
| `invalid_arguments` | `-32602` | 参数无效。 |
| `internal` | `-32603` | 内部错误；消息有长度上限，不带原始错误值。 |
| `cancelled` | `-32800` | 已取消（`$/cancel_request`、归属释放、关闭）。 |
| `overloaded` | `-32000` | 进行中的请求超过 256 个。 |
| `busy` | `-32000` | 某项每连接上限已满，或 peer 入站队列已满。 |
| `unsupported_version` | `-32000` | 不支持的协议版本。 |
| `request_id_conflict` | `-32000` | `agent.prompt` 的 `request_id` 被不同内容重复使用。 |
| `unsupported` | `-32000` | 不支持的能力或媒体输入；`data.data.capability` 说明是哪一项。 |
| `timed_out`、`disconnected`、`denied` | `-32000` | 服务调用超时、回调断开、对本连接不允许。远端实现返回的错误没有结构化码时为 `internal`。 |

## 限制

- 请求行最大可为 64 MiB。更长的行会被跳过，并以 `-32600` 回答。
- 每个连接最多接纳 4096 个选中的事件和 64 MiB 的序列化事件记录，输出队列最多容纳 4096 行和 64 MiB。超出预算的记录或行会使该连接失败。
- 选中的事件在被接纳时确定。对 `events.subscribe` 的更改适用于之后的事件。
- 响应排在已被接纳的事件之后。
- 如果某个事件无法编码或送达，mare 会在输出仍可写时发送 `stream.failed`。如果不可写，则以 EOF 或被截断的行作为信号。
- 失败或关闭的标准输出会取消活动的一轮。失败的对等连接不会取消共享的会话。

## 对等套接字

出站对等客户端通过 Lua 中的 `mare.peers` 构建（由 `peers` 插件契约拥有，空闲等待改用 `status/changed`）。每条消息的 `from.session_id` 都是发送时的当前会话 ID。

交互式会话还会监听 `<state>/peers/<pid>.sock`，用 `--peer` 启动的 `--rpc` 或无界面运行也会如此。同一用户的其他程序可以与它通信。该套接字使用本协议的一个最小权限子集：

- 套接字在 0700 目录中，权限为 0600（目录为符号链接或属主非当前用户时不监听）。accept 时 mare 会校验连接进程的用户 id（Linux `SO_PEERCRED`、macOS `getpeereid`）并断开其他用户。在 Windows 上，传输是 DACL 仅允许当前用户 SID 与 SYSTEM（拒绝远程客户端）的命名管道，随机 128 位管道名写入套接字路径处的端点文件并由客户端校验。
- 对等方可以调用 `initialize`、`peer.info`、`status.get`、`peer.message`、`events.subscribe`（仅限显式指定 `status/changed`）和 `shutdown`（只结束该连接）。其他一切都以 `-32601`（或更宽泛的 `events.subscribe` 过滤条件返回 `denied`）失败。
- 要允许更多，在 `peer` 选项中列出精确方法名称（例如用 `"events.subscribe"` 开放完整事件流）；不接受通配符或前缀：

```lua
mare.setup {
  peer = { name = "main", allow = { "agent.prompt", "tools.register", "events.subscribe" } },
}
```

`peer = false` 会关闭套接字。仅限主连接的方法（`options.set`、`commands.run`、`hooks.subscribe` 及其他 `access = "primary"` 操作）永远不能授予对等方。对等方永远不会接管 UI 或审批。对等套接字上的请求行限制为 1 MiB，同时最多有 32 条入站消息在等待用户处理。

| 方法 | 参数 | 结果 |
|---|---|---|
| `peer.info` | `{}` | `{name, pid, cwd, version, allowed}` |
| `status.get` | `{}` | 统一状态，形状与 `status/changed` 相同。 |
| `peer.message` | `{from: {name?, pid?, cwd?, session_id?}, text, queue_only?, policy?}` | `{message_id, status}` |

发送方在 `from` 和 `policy` 中关于自己的说法会展示给接收方用户，且绝不授予任何权限。空的 `text` 会以 `-32602` 失败；在套接字能提供连接进程凭据的平台上，与凭据不一致的 `from.pid` 或已注册的 `from.name` 也会以 `-32602` 拒绝。消息会被保留，直到接收方用户接受或拒绝，答复以 `peer.message_status {message_id, status}` 通知到达，值为 `delivered` 或 `rejected`（无 UI 时直接拒绝）。送达的消息会作为后续消息进入智能体，并包裹在一个数据边界内，标明它来自另一个会话。

运行中的会话会在 `<state>/peers/<pid>.json` 登记 `{name, pid, cwd, socket, mode, started, status, plugins?}`，其中 `plugins` 保存插件经 `mare.peers.publish(namespace, fields)` 发布的可选不透明命名空间（最多每 200 ms 重写一次；退出时移除登记与套接字，列出时清理已结束进程的陈旧条目）。

### 从 shell 发送消息

```sh
mare peer send --to main "tests are green on the integration branch"
printf 'see the diff in /tmp/x.patch' | mare peer send --to main
```

`--to` 接受套接字路径或已注册的会话名称。不带它时，mare 使用 `$MARE_PEER_SOCKET`，由 mare 会话启动的工具和钩子会继承它。该命令打印结果状态（`held`、`delivered` 或 `handled`），接收方接受消息时以 0 退出。文本来自参数，没有参数时来自标准输入。

插件管理器会在进程内发出 Lua 事件 `plugins/completed { action, name?, status }`。载荷说明见[服务与事件](../extend/services-and-events.md)。

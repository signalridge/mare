# 服务与事件

插件之间如何通信：提供和替换服务、事件分发风格、策略事件和重写事件、内置服务以及主要事件。

## 服务

### 使用服务

在 `inject` 中列出你需要的服务。之后可以通过 `ctx.<name>` 或 `ctx:get(name)` 使用。调用方法时用冒号。

```lua
return {
  name = "show-model",
  inject = { "llm", "commands" },
  apply = function(ctx)
    ctx.commands:register("which-model", {
      description = "Show the current model",
      run = function()
        local m = ctx.llm:current()
        ui.notify(m and m.ref or "no model")
      end,
    })
  end,
}
```

`ctx.llm` 每次读取都会到达当前的提供者。该提供者重新加载或被更换时，注入它的插件随之重启。

插件没有它也能运行的服务，不写进 `inject`。`ctx:get(name)` 返回该服务，没有提供者时返回 `nil`。要向它注册内容，用 `ctx:inject({ name }, fn)`：服务有提供者期间运行 `fn(ctx)`，通过这个 `ctx` 注册的内容在服务消失时撤回，下一个提供者出现时 `fn` 再运行一次。内置的 `profile` 插件就是这样添加 `/profile` 的。

```lua
return {
  name = "show-model",
  inject = { "llm" },
  apply = function(ctx)
    -- /which-model exists while the commands service does
    ctx:inject({ "commands" }, function(sub)
      sub.commands:register("which-model", {
        description = "Show the current model",
        run = function()
          local m = ctx.llm:current()
          ui.notify(m and m.ref or "no model")
        end,
      })
    end)
  end,
}
```

### 提供服务

在 `provide` 中声明名称，在 `contracts` 中声明它的方法，然后在 `apply` 中用一个 `mare.Service` 类提供它：构造函数写明服务名，契约中的每个方法都是这个类的方法，`ctx:plugin(Class)` 提供它。在方法内部，`self.ctx` 是调用方插件的 `ctx`，所以方法作为 `self.ctx` 的 effect 注册的内容属于调用方，调用方消失时撤回。内置的 `commands` 服务就是这样注册命令的。

```lua
-- ~/.config/mare/plugins/greeter.lua
return {
  name = "greeter",
  provide = { "greeter" },
  contracts = { greeter = { required = { "greet", "remember" } } },
  apply = function(ctx)
    local known = {}
    local Greeter = require("cordis").class("Greeter", mare.Service)
    function Greeter:constructor(c)
      Greeter.super.constructor(self, c, "greeter")
    end
    function Greeter:greet(who)
      return known[who] and ("hello again, " .. who) or ("hello " .. who)
    end
    -- remembered until the calling plugin goes away
    function Greeter:remember(who)
      return self.ctx:effect(function()
        known[who] = true
        return function() known[who] = nil end
      end)
    end
    ctx:plugin(Greeter)
  end,
}
```

```lua
-- ~/.config/mare/plugins/greeter-user.lua
return {
  name = "greeter-user",
  inject = { "greeter" },
  apply = function(ctx)
    ctx.greeter:remember("world")
    ui.notify(ctx.greeter:greet("world"))   -- hello again, world
  end,
}
```

服务在发布前要按契约检查。没有契约，或缺少必需的方法时，它会被拒绝：服务不会被提供，原因记为该插件的问题。内置插件已经定义过的名称，其契约不能被重新声明。

契约也可以逐个描述方法，从而把方法发布给 Lua 之外的调用方。不写 `required` 列表，而是给出 `methods`：每个方法写上 `doc`、`params` 与 `result` 的 JSON Schema、位置参数顺序 `order` 和 `expose = "both"`。内核按这些 Schema 检查参数和结果，并按契约派生 JSON 适配，该方法即成为 RPC 方法 `service.method`。

### 替换内置服务

一个服务只有一个提供者。另一个插件在 `provide` 中声明同一名称时，会在运行前被拒绝，错误中写明两个文件，先前的插件保留该服务。不会回退：提供者消失后，该服务就不存在，直到有插件再次提供它。

要替换内置服务，在 `init.lua` 中用 `mare.plugin("status", { enabled = false })` 关掉它的插件（这会关掉这个名字下的所有来源，包括你自己的），再由你自己的插件提供该服务，实现其契约中所有必需的方法。

```lua
-- ~/.config/mare/plugins/my-status.lua
return {
  name = "my-status",
  provide = { "status" },
  apply = function(ctx)
    local Status = require("cordis").class("Status", mare.Service)
    function Status:constructor(c)
      Status.super.constructor(self, c, "status")
    end
    function Status:get()
      return { state = "idle", message = "", cwd = mare.cwd, pid = mare.pid, seq = 0, session = {} }
    end
    ctx:plugin(Status)
  end,
}
```

或者给你的插件取与内置插件相同的名称并设置 `override = true`；这样它也会继承被替换插件所负责的服务。如果关键服务的替代者启动失败，请求会被阻止，而不是在缺少它的情况下运行。

要改变行为而不替换服务，优先使用事件监听器，或通过各自的 API 修改某一部分：`ctx.tools:shadow(name, def)` 重新注册某个工具修改后的副本，UI 组件也可以被包装（见[外观](../configure/ui.md)）。

### 内置服务

| 服务 | 提供者 | 作用 |
|---|---|---|
| `agent` | `agent` | 运行智能体循环：`prompt`（支持 `follow_up` 投递类型）、`steer`、`cancel`、`activity`（`agent:activity().busy`）、`messages`。 |
| `llm` | `llm` | provider 注册表、模型目录、登录和参数：`current`、`set_model`、`resolve`、`recent`、`cycle_param`、`login`、`logout`、`auth_keys`。 |
| `session` | `session` | 磁盘上的会话树：`info`、`create`、`load`、`fork`、`close`、`list`、`entries`、`record`、`records`。 |
| `context` | `context` | 系统提示词组装：`section`、`suppress`、`variable`、`system_prompt`、`skills`、`templates`。 |
| `compact` | `compact` | 分阶段的上下文压缩：`run`、`register_stage`、`measure`、`threshold`。 |
| `tools` | `tools` | 工具注册表和唯一受保护的执行路径：`register`、`shadow`、`command`、`get`、`list`、`execute`、`disable`、`enable`。 |
| `permissions` | `permissions` | 沙箱和审批预设：`preset`、`request`、`grants`。 |
| `approvals` | `approvals` | 独立的审批策略和审阅者：`policy`、`set`、`request`、`register_reviewer`。 |
| `commands` | `commands` | 斜杠命令：`register`（通过 `{ alias_of = target }` 注册别名）、`run`、`list`、`get`。 |
| `ui` | `ui` | 限定于插件的界面部件：`widget`、`status`、`invalidate`、`key`、`action`、`renderer`、`style`。 |
| `theme` | `theme-default` | 主题：`list`、`get`、`register`、`select`、`preview`、`cancel_preview`、`register_override`。 |
| `jobs` | `jobs` | 后台进程：`start`、`track`、`get`、`list`、`background`、`cancel`（暴露给模型的工具名为 `job_kill`）、`read`。 |
| `usage` | `usage` | token 和费用总计：`totals`、`context_tokens`、`cost_text`、`rate_limits`。 |
| `status` | `status` | 统一的智能体状态：`get`。 |
| `mcp` | `mcp` | MCP 服务器：`status`。 |
| `changes` | `changes` | 每轮的文件更改和回退：`turns`、`diffs`、`rewind`。 |
| `review` | `review` | 代码审阅：`run`、`viewer`。 |
| `images`、`copy`、`plugins`、`update-check`、`spill` | 同名插件 | 图片显示、剪贴板复制、插件目录界面、更新检查、结果存储。 |

运行时目录（`mare runtime`）中的 Lua 类型文件给出完整的签名；[Lua API](lua-api.md) 页面由它们生成。

### 会话提供者契约

`session` 服务将生命周期编排与会话存储解耦，使自定义 `session` 提供者在 TUI、CLI 以及多会话 RPC/Web/ACP 宿主上表现一致：

- **宿主提供的生命周期方法**：`session.create`、`session.load`、`session.fork` 和 `session.close` 由宿主会话编排器提供（契约中标注 `provider = "host"`），统一负责项目越界检查、`request_id` 幂等回执、`expected_epoch` 校验与运行时生命周期；Lua、斜杠命令与 RPC 调用方走同一路径。自定义 `session` 提供者不得实现这四个方法。
- **多运行时宿主中的启动参数（`mare.args`）**：多运行时宿主（`mare --rpc`、Web、ACP）创建、加载或分叉会话时，编排器会派生独立运行时并在 `app/ready` 之前向 `mare.args` 写入启动参数：
  - `session_id`：预分配的会话 ID，新建或分叉时直接采用。
  - `resume`：启动时要加载的既有会话路径或选择器。
  - `fork_session`（布尔值，可选配 `fork_before_entry`）：从恢复的源会话分叉，而非原地打开。
  - `name`：初始会话名称。
  - `fork_images`（可选映射，当 `prepare_fork` 返回内存图片时由编排器注入）：分叉所需的已校验图片载荷；提供者在启动读取后应清空 `mare.args.fork_images`。
  - 若启动加载或分叉失败，需通过 `session:info().resume_error` 暴露错误，以便编排器关闭并回滚该运行时。
- **仅限 Lua 的提供者钩子（`expose = "lua"`）**：
  - `session.switch(opts)`：在**单运行时**宿主上由编排器调用，原地切换当前活动会话（`opts.kind = "new" | "load" | "fork"`，可选 `cwd`、`name`、`session_id`、`path`、`before_item_id`、`id`），返回切换后的实时会话快照。提供者通过 `ctx.kernel:begin_session_switch` / `commit` 原子协调宿主身份、会话级信任撤销与在途调用守卫（`handle:commit(session_id)` 会自动将当前会话导出调用标记为已提交）。若自定义提供者省略 `switch`，单运行时生命周期调用会返回 `unavailable`。
  - `session.prepare_fork()`：多运行时分叉前由编排器在存活源运行时上调用，用于排空旁路请求、刷盘日志并返回 `{ path, images? }`（无持久化文件时返回 `nil`）。提供者未实现该方法时，针对存活源的多运行时分叉报 `unavailable`。
- **两种运行时容量下的 `session.close` 行为**：
  - **多运行时宿主**：关闭并卸载目标会话的运行时、释放文件锁，返回 `{}`。
  - **单运行时宿主**：关闭当前已绑定会话（支持精确 ID、路径或至少 4 位唯一前缀）时，编排器通过 `session.switch({ kind = "new" })` 原地切换到新的空会话并释放旧会话锁，返回 `{}`，不会退出宿主或重启插件与 MCP 服务。关闭未加载的会话为空操作返回 `{}`；前缀歧义报 `invalid_arguments`，未知 ID 报 `unknown_session`，过期 `expected_epoch` 报 `stale_session`，回合进行中报 `busy`。

## 事件

### 监听

`ctx:on(event, fn, opts)` 在插件的生命周期内订阅事件。监听器按注册顺序运行；`opts.prepend = true` 把它放到最前。它返回一个取消订阅的函数。`ctx:once(event, fn)` 在第一次调用后移除。在 `init.lua` 中使用 `mare.on(event, fn)`，它是同一种监听器；要对 `tool/before` 这类决定点投票，使用 `mare.policy.voter(point, fn, opts)`。

```lua
ctx:on("model/changed", function(ev)
  ui.notify("model is now " .. ev.ref)
end)
```

waterfall 监听器接收 `(payload, ev)`。调用 `ev.next(payload)`（或 `ev(payload)`）会把该值传给后续监听器（`ev.next()` 沿用当前值）并返回它们的结果；监听器自身的返回值就是最终结果。不调用 `ev.next` 的监听器会在此结束整条链。

```lua
ctx:on("context/system", function(payload, ev)
  payload.text = payload.text .. "\nAnswer in English."
  return ev.next(payload)
end)
```

### 事件如何分发

分发风格由事件的生产者选择。它决定了你的返回值意味着什么。

| 风格 | 生产者调用 | 监听器 | 你的返回值 |
|---|---|---|---|
| 通知 | `ctx.bus:emit` | 全部，按顺序。 | 被忽略。错误会被报告，其余监听器仍会运行。 |
| 受检通知 | `ctx.bus:emit_checked` | 全部，按顺序。 | 错误会停止分发并传回生产者。 |
| bail | `ctx.bus:bail` | 按顺序，直到某个返回非 `nil`。 | 第一个非 `nil` 的值就是结果。 |
| waterfall | `ctx.bus:waterfall`、`ctx.bus:waterfall_checked` | 先注册的在最外层；每个调用 `next` 让其余的运行。 | 传给 `next` 的值继续传下去；最外层监听器的返回值就是结果。`waterfall_checked` 把抛错的监听器的错误交给生产者。 |
| 策略 | `ctx.policy:decide(point, args, opts)` | 投票者，按顺序。 | 一票。见下文。设了 `opts.require_explicit_approval` 时，没有投票者或全部投 `nil`，结果是 `unavailable` 的拒绝。 |
| 重写 | `ctx.policy:rewrite(point, call)` | 改写者，按顺序。 | `nil` 表示保持参数，或新的参数表；成功直接返回改写后的参数表，失败抛出 `policy.rewrite_refused`。 |

插件用同样的调用发出自己的事件（`inject = { "bus" }`）。所有派发的事件都必须在契约（`contracts.<root>.events`）中声明；使用 `name/verb` 形式的名称，并保证它们在你的插件内唯一。每个声明的事件通过 `expose` 指定三档可见性：

- `expose = "process"`：严格仅限进程内。绝不进入外部事件流，也不能经 RPC 观察或 hook，任何 `include` 设置都不会放行。
- `expose = "lua"`（默认）：对进程内 Lua 插件可见，不默认进入公开 RPC/JSON 事件流。主 RPC 连接的 `include` / hook 订阅可以选择接入，因此 `"lua"` 不构成保密边界。
- `expose = "both"`：同时对进程内 Lua 与默认公开 RPC/JSON 事件流可见。

没有单独的本地通道。

### 策略事件

决定点请插件允许或拒绝某件事。它采用失败即关闭的方式。

- 投票者返回 `nil`（无异议）、`"allow"`、`"deny"`、`"ask"`，或表 `{ decision = "deny", reason = "..." }`。未知的字段或决定会使投票无效，而无效的投票视为拒绝。
- 第一个 `deny` 结束分发且为最终结果。否则 `ask` 胜过 `allow`。
- 抛出错误，或在超时内没有回答的投票者，计为 `deny`。默认超时是 30 秒。在投票者的选项中设置 `timeout_ms` 可以更改；`0` 取消限制，需要等待人的投票者就需要这样。
- `slot = "name"` 给有状态的检查一个决定槽位。该槽位只有第一个注册的投票者会运行。
- 投票从不重写任何内容。参数在投票前已被冻结。

```lua
ctx.policy:voter("tool/before", function(call, info)
  local command = call.name == "bash" and call.arguments.command or ""
  if command:find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

插件像上面这样用 policy 服务的 `voter` 投票（`inject = { "policy" }`）；决定点上的监听者（`ctx:on`）不是投票者。在 `init.lua` 中对应的是 `mare.policy.voter(point, fn, opts)`。

重写发生得更早，在 `tool/rewrite` 链中。每个改写者收到带有当前参数的调用副本，并返回 `nil` 或新的参数表。错误、超时或非表的结果会使重写失败，调用被拒绝。

```lua
ctx.policy:rewriter("tool/rewrite", function(call)
  if call.name == "bash" then
    local args = {}
    for k, v in pairs(call.arguments) do args[k] = v end
    args.command = (args.command or ""):gsub("^%s+", "")
    return args
  end
end)
```

决定点有 `tool/before`、`tool/approval`、`net/approval`、`request/ready` 和 `permission/verdict`，另外还有编辑工具写入之前的 `fs/edit-intent` 和 `fs/write-intent`。

### 主要事件

![mare 单次 turn 数据流](../../assets/diagrams/turn-dataflow.svg)

全部内置公开事件由注册表生成于[事件参考](../reference/events.md)。下面列出代表性的进程内生命周期与 hook 事件；载荷是 Lua 表，其余部分见运行时目录 `types/mare/events.lua` 中的注释。

| 事件 | 风格 | 载荷与含义 |
|---|---|---|
| `app/ready` | 通知 | UI 已就绪。 |
| `app/shutdown` | 通知 | 正常退出；会等待监听器。 |
| `session/started` | 通知 | `{ session_id, path, resumed }` |
| `session/closed` | 通知 | `{ session_id, cwd, reason, path? }`（`session_id`、`cwd`、`reason` 为必填，`path` 为可选），仅在主宿主会话退出、切换或重载时发出（子宿主不触发），且早于依赖 `session` 的观察者销毁。 |
| `agent/step-ready` | 策略 | `{ id, step, cancel, turn_id }`，每次模型请求之前；deny 会停止本次运行。 |
| `agent/step-committed` | 通知 | `{ step_id, step, turn_id }`，某一步的参与者状态提交之后。 |
| `turn/completed` | 通知 | `{ turn_id, status, termination_reason, cancellation_cause?, text, structured, usage, error, ... }`，一轮结束时发出（当 `termination_reason` 为 `"interrupted"` 时，`cancellation_cause` 为 `"user"`、`"parent"`、`"hook"` 或 `"disposed"`）。 |
| `agent/settled` | 通知 | `{ turn_id, busy }`，在该次运行的 `turn/completed` 监听器全部返回之后发出；另一次运行或外层 `turn/completed` 派发仍在进行时 `busy` 为 true。 |
| `agent/turn-stopping` | bail | 返回字符串或 `{ message, source? }` 可使循环继续（携带 `source` 时续接消息会带上 `source` 与 `notification = true`）。 |
| `request/prepare` | 受检通知 | `{ model, cancel }`，在一个提示词的第一个请求之前。错误会停止请求。 |
| `request/ready` | 策略 | 拒绝会停止请求：显示为 `not sent: <reason>`。 |
| `input/submit` | 通知 | 用户提交了输入。 |
| `input/transform` | waterfall | 在发送之前修改输入。 |
| `context/transform` | waterfall | 在请求之前修改消息。 |
| `request/build` | 受检 waterfall | 修改出站请求；抛错会在发往模型提供方之前中止请求。 |
| `context/system` | waterfall | 载荷为 `{ text, model }`；修改 `payload.text` 后返回 `ev.next(payload)`。 |
| `tool/rewrite` | 重写 | 在投票之前修改调用的参数。 |
| `tool/before` | 策略 | `(call, { side_effect, idempotent, unconfined })` |
| `tool/approval` | 策略 | 用 `{ decision = "allow" }`、`{ decision = "allow", grant = "allow_session" }`（或带 `amendment` 的 `"allow_amend"`）、`{ decision = "deny" }` 或 `{ decision = "deny", abort = true }` 回答一次 `ask`。 |
| `tool/after` | waterfall | 在工具运行之后、`tool/result` 之前修改结果；`result.sandbox_denials` 列出被拒绝的操作。 |
| `tool/result` | 受检 waterfall | 在 `tool/after` 之后、默认截断之前修改结果；监听器抛错会生成错误工具结果。 |
| `provider/request` | waterfall | 发送之前的 `{ provider, url, headers, body, model }`。 |
| `llm/usage` | 通知 | 每次 provider 请求之后的用量（`usage.cache_write_1h?` 为 `cache_write` 子集、`usage.cost?`）。 |
| `compact/before` | bail | `false` 取消一次压缩。 |
| `option/changed` | 通知 | 在 `mare.o.<name> = value` 或重新加载之后的 `{ name, value }`。 |
| `option/apply` | checked | 运行时写入插件选项时、宣布之前的 `{ name = "plugins.<p>.<path>", value }`：所属插件在此应用，出错则写入失败并全部恢复。 |
| `theme/changed` | 通知 | 主题快照。 |
| `theme/palette` | waterfall | 在构建样式之前调整某个风格的调色板。 |
| `ui/caps` | waterfall | 添加或覆盖终端能力。 |
| `ui/commit-before` | waterfall | 即将进入回滚缓冲区的 `{ lines }`。 |
| `plugins/completed` | 通知 | 包操作之后的 `{ action, name?, status }`。 |
| `reload/completed` | 通知 | `/reload` 之后的 `{ added, removed, replaced, reloaded, notices }`。 |
| `kernel/reloaded` | 通知 | 插件已重新加载。 |
| `kernel/error` | 通知 | `{ message, plugin, event, what, error }`：监听器、任务、定时器或销毁钩子失败。 |

外部程序可以通过 RPC 订阅事件；见 [RPC](../integrate/rpc.md)。在服务契约中把事件声明为 `expose = "both"`，即可将自己的事件发布到公开流。

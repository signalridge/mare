# 插件

mare 插件是什么：插件从哪里加载、插件定义的字段、生命周期、开关内置插件，以及如何隔离故障。

## 插件是什么

插件是一个返回单个表的 Lua 文件。表描述插件；它的 `apply` 函数负责实际工作。

```lua
-- ~/.config/mare/plugins/hello.lua
return {
  name = "hello",
  inject = { "commands" },
  config_schema = { greeting = { type = "string", default = "hello", desc = "what to say" } },
  apply = function(ctx, cfg)
    ctx.commands:register("hello", {
      description = "Say hello",
      run = function() ui.notify(cfg.greeting) end,
    })
  end,
}
```

文件的顶层允许定义局部常量与辅助计算，并使用白名单内的纯标准库名称（`string`、`table`、`math`、`pairs` 等），最终返回插件定义表。读取非白名单全局变量（如 `mare` 或 `require`）或写入全局变量都会报错并指出出错的行。运行时逻辑放进 `apply`。[编写插件](writing-a-plugin.md)会一步步构建一个完整的插件。

## 插件从哪里加载

mare 按以下顺序收集插件来源。靠后的来源优先级更高。

| 顺序 | 来源 | 位置 |
|---|---|---|
| 1 | `builtin` | 发布构建中编译进 mare，调试构建直接从磁盘 `crates/mare-plugin/preset/plugins/` 读取。运行时目录（`mare runtime`）中有只读副本。 |
| 2 | `package` | 每个已安装[包](../configure/packages.md)的 `plugins/`，遵从其 `only` 列表。 |
| 3 | `user` | `~/.config/mare/plugins/` |
| 4 | `project` | `<project>/.mare/plugins/`，仅限受信任的项目。 |

在目录中，`name.lua` 和 `name/init.lua` 是插件。名称以 `_` 或 `.` 开头的条目会被忽略。

两个插件不能悄悄地同名。较高来源层的定义可以用 `override = true` 替换较低来源层的同名插件；未设置时，较晚的定义会被拒绝并提示添加 `override = true` 或改名。如果**同一来源层**存在两个同名定义，两者都会被拒绝并保留较低层的原定义，此时添加 `override = true` 也不能解决同层冲突。关掉靠前的那个不能绕开这条规则：`enabled = false` 关掉的是整个名字，所有来源都不加载。`/plugins` 把被替换的插件显示为 `overrides built-in <name>`。整个插件定义可放在 `<name>.lua` 或 `<name>/init.lua`（依赖私有模块时一并提供 `lua/`）并加上 `override = true`；单独放一个模块文件不会替换整个插件（如需临时替换单个模块，请使用 `package.preload`）。

`--clean` 只加载内置插件。

## 模块与 `require`

所有内置与第三方插件共用同一个开放、按所有者分区的 `require` 加载器：

1. **解析顺序**：`package.loaded[name]`（全局覆盖与核心模块）→ `package.preload[name]` → 调用者插件自己的 `lua/` 目录 → 核心模块（`cordis*`、`mare_kernel*`、`mare_lib*`、`mare.test`）→ 其他插件（按模块根名唯一解析）。
2. **代码开放，状态走服务**：跨插件 `require` 只用于共享无状态代码；跨插件的运行时状态与能力应通过 Cordis 服务与事件（`inject`/`provide`/`ctx:get`/`ctx.bus`）交互。请将跨插件 `require` 写在 `apply` 内（不要写在定义文件顶层），以确保所有插件的 `lua/` 根索引已完成并正确记录热重载依赖边。模块在**提供者**的隔离环境中运行，因此被跨插件 `require` 的模块写入的全局变量落在提供者环境，而非调用者环境。
3. **四条可观察特性**：
   - 同名模块按所有者隔离：不同插件各自的 `lua/util.lua` 落在各自缓存分区，互不串表。
   - 跨插件根名歧义报错：多个插件提供同一模块根时，跨插件 `require` 报错并点名全部提供者（插件自有 `lua/` 仍优先解析）。
   - `mare_lib*` 归核心所有：整个 VM 仅一份实例，不会随某个调用者插件的重载被清空。
   - 依赖方随提供者重载：重载提供者只清空提供者自己的模块分区，并在提供者重建完成后自动重载 `require` 过其模块的依赖方。
   - 覆盖仅支持整插件替换（`override = true`）或 `package.preload`；公开模块由 `types/` 下的 `---@meta <name>` 声明。

## 定义表

| 字段 | 类型 | 含义 |
|---|---|---|
| `name` | 字符串 | 插件名称。默认为文件名。 |
| `inject` | 字符串列表 | 此插件需要的服务。在每一个都有提供者之前，它等待而不启动。 |
| `provide` | 字符串列表 | 它提供的服务（一个 `mare.Service`，见[服务与事件](services-and-events.md)）。两个已启用的插件不能声明同一个：加载顺序靠后的被拒绝，错误中写明两个文件。 |
| `contracts` | 表 | 它定义的新服务的方法列表。见[服务与事件](services-and-events.md)。 |
| `config_schema` | 表 | 插件的选项，每项一条声明：`{ type, default, desc, choices?, min?, max?, fields?, items?, values?, variants?, on_invalid?, apply? }`。由它得出 `apply` 收到的 `cfg` 默认值，并对每个入口（init.lua、`--set`、`MARE_*`、运行时 `mare.plugin` 写入）做同样的检查；无效值会报告并保留默认值。`type = "union", variants = {…}` 要求值恰好符合其中一种形式。`on_invalid = "fail"` 让插件不启动，而不是退回默认值（用于审批策略这类限制性设置）。运行时写入先经 `option/apply` 交给所属插件应用（那里出错，如写盘失败，写入即失败、什么都不变），生效后再发 `option/changed`。用户用 `mare.plugin(name, {...})` 设置选项。 |
| `config_api` | 表 | 在 `init.lua` 中成为 `mare.<name>` 的函数。见下文。 |
| `apply` | 函数 | `apply(ctx, cfg)`。在启动时和每次重新加载后运行。可以 yield。 |
| `required` | 布尔值 | 如果启动失败，所有模型请求都会被拒绝，直到完整的 `/reload` 成功。 |
| `override` | 布尔值 | 可以替换来自更早来源的同名插件。 |
| `tags` | 字符串列表 | 插件的种类，无需运行即可读取。`"provider"` 标记模型 provider。 |
| `wires` | 字符串列表 | 它向 `llm` 服务注册的协议格式。 |
| `providers` | 字符串列表 | 它向 `llm` 服务注册的模型 provider 名称（即使插件被禁用也会被读取，用于状态展示与归因）。 |

`cfg` 是 `config_schema` 的默认值与用户在 `plugins.<name>` 中设置的内容合并后的副本，表会递归合并。`mare.plugin("hello", { greeting = "hi" })` 会改变 `cfg.greeting`；未知的键或错误的类型会产生带“你是不是想写”提示的警告，所以请在 `config_schema` 中声明每一个选项。

### 配置函数

`config_api` 让插件向配置语言添加函数。键是点分路径；函数的第一个参数是配置表。

```lua
config_api = {
  ["greeting.set"] = function(C, text) C.greeting = text end,
},
```

插件安装之后，`init.lua` 可以调用 `mare.greeting.set("hi")`，`apply` 读取 `mare.config.greeting`。内置的 `tools` 插件用它实现 `mare.tool.command`。

### 厂商包（Provider packs）

原则是“厂商是数据，协议是代码，名字只是标识”。第三方插件可以把兼容 Chat Completions 或 Responses 的厂商写成纯数据条目，无需手写 `stream` 或 `endpoint` 回调，并从同一顶层表派生 `providers` 声明，避免双处维护漂移：

```lua
local PRESETS = {
  acme = {
    label = "Acme AI",
    wire = "openai-chat",
    dialect = "deepseek",
    base_url = "https://api.acme.example/v1",
    api_key_env = "ACME_API_KEY",
    key_prefixes = { "acme-" },
    models_dev = false,
    default_model = "acme-v1",
    models = {
      { model_id = "acme-v1", context_window = 128000, max_output = 8192 },
    },
  },
}

local NAMES = {}
for name in pairs(PRESETS) do NAMES[#NAMES + 1] = name end
table.sort(NAMES)

return {
  name = "vendor-acme",
  inject = { "llm" },
  tags = { "provider" },
  providers = NAMES,
  apply = function(ctx)
    for _, name in ipairs(NAMES) do
      local entry = table.copy(PRESETS[name])
      entry.provider = name
      ctx.llm:register_provider(entry)
    end
  end,
}
```

用户可以通过 `mare.provider("acme", { ... })` 叠加覆盖任意字段（如 `base_url`、`default_model`、`models` 或 `wire_api`）；禁用或卸载 `vendor-acme` 时，其 provider 与参数规则会一并撤回。

## 生命周期

![mare 插件生命周期与 override / reload 流程](../../assets/diagrams/plugin-lifecycle.svg)

1. mare 读取每个定义。无法读取的文件会被报告并跳过。
2. 插件按依赖顺序启动：插件要等 `inject` 中的每个服务都有提供者，所以提供者先启动，其余的按来源顺序启动。缺少提供者时，依赖它的插件会等待；存在依赖环的插件会被拒绝，并报告 `not loaded: dependency cycle`。
3. 对每个插件，mare 构建 `cfg` 并调用 `apply(ctx, cfg)`。
4. 通过 `ctx` 注册的一切都属于该插件：监听器、服务、定时器、任务、工具、命令、键位、状态片段，以及每一个 `ctx:effect`。
5. 在重新加载或退出时，mare 会销毁插件，撤回上述全部内容，重新加载时再重新应用它。注入了它所提供服务的插件随之重启。

这套生命周期中 mare 承诺的内容见 [插件运行时承诺](runtime-guarantees.md)，每条都有测试锁定。

`ctx` 有以下方法和字段：

| 成员 | 作用 |
|---|---|
| `ctx.<service>` | `inject` 中列出的服务，例如 `ctx.tools`。 |
| `ctx:get(name)` | 按名称取服务；没有提供者时为 `nil`。用于插件没有它也能运行的服务。 |
| `ctx:inject({ name, ... }, fn)` | 在这些服务都有提供者期间运行 `fn(ctx)`。其中任何一个消失时撤回 `fn` 注册的内容；它们恢复后 `fn` 再运行一次。 |
| `ctx:on(event, fn, opts)`、`ctx:once(event, fn)` | 监听事件。`opts.prepend = true` 把监听器放到最前。返回一个移除它的函数。 |
| `ctx.bus:emit`、`:emit_checked`、`:bail`、`:waterfall`、`:waterfall_checked` | 分发事件（`inject = { "bus" }`）。见[服务与事件](services-and-events.md)。 |
| `ctx.policy:voter`、`:rewriter`、`:decide` | 对决策点投票、改写参数和发起决策（`inject = { "policy" }`）。 |
| `ctx:plugin(Class)` | 提供一个服务：`Class` 是 `mare.Service` 的子类。 |
| `ctx:effect(fn)` | 立即运行 `fn`；它返回的函数在插件消失时运行。 |
| `ctx:spawn(fn)` | 运行后台任务，销毁时取消（`inject = { "task" }`）。 |
| `ctx:after(ms, fn)`、`ctx:every(ms, fn)` | 定时器，销毁时取消（`inject = { "task" }`）。 |
| `ctx.logger:warn(...)`、`ctx.logger:error(...)` | 以插件名写入日志。error 还会记为该插件的问题，并发出 `kernel/error`。 |
| `ctx.kernel:reload(name)`、`:plugins()`、`:problems()` | 重新加载一个插件或全部；插件列表和已记录的失败（`inject = { "kernel" }`）。 |

已应用的插件也可以通过 `mare.plugins.status(name)` 和 `mare.plugins.list()` 查看。`mare.plugins.failures()` 返回启动时或上次完整重新加载中失败的内容。

## 开关内置插件

单个插件，无论来源：

```lua
mare.plugin("update-check", { enabled = false })
```

一个名字就是一个槽位，跨所有来源：`enabled = false` 会关掉这个名字下的内置插件以及包、用户、项目中的同名插件。各层的值逐叶子比较，所以受信任项目的 `false` 压过你的 `true`，`--set plugins.<名字>.enabled=…` 又压过两者。

精简插件集合是一个预设：

```lua
mare.setup { plugins = { preset = "minimal" } }
mare.plugin("jobs", { enabled = true }) -- 需要什么再加回来
```

`minimal` 关掉所有既没有 `minimal` 也没有 `provider` 标签的内置插件。带 `minimal` 标签的有 `agent`、`agent-approvals`、`approval-review-model`、`approvals`、`baseline`、`commands`、`context`、`conversations`、`fs`、`keymap-default`、`llm`、`permissions`、`session`、`statusline`、`theme-default`、`tools`、`ui`、`webserver`，以及所有带 `provider` 标签的内置 provider（`provider-anthropic`、`provider-google`、`provider-openai`、`provider-openai-compat`、`provider-opencode-go`，以及仅在 debug 或启用 `faux` 特性的构建中提供的 `provider-faux`）。用 `override = true` 替换了内置插件的插件，以及所有包、用户、项目插件，都不受影响。任何一层显式写的 `enabled` 都压过预设。未知的预设名是配置错误：会报告，并按没有预设处理（`--strict-config` 下拒绝启动）。`preset` 是保留字，插件不能叫这个名字。

按领域划分的内置插件：

| 领域 | 插件 |
|---|---|
| 核心 | `agent`、`baseline`、`cells`、`compact`、`context`、`conversations`、`fs`、`jobs`、`llm`、`mcp`、`message`、`proc`、`query`、`query-disk`、`session`、`spill`、`subagent`、`terminal`、`tools`、`tools-apply-patch`、`web`、`web-fetch-http`、`webserver` |
| Provider | `provider-anthropic`、`provider-google`、`provider-openai`、`provider-openai-compat`、`provider-opencode-go`、`provider-faux`（仅 debug 构建或显式启用 `faux` 特性的 release 构建提供） |
| 安全 | `permissions`、`approvals`、`approval-review-model`、`agent-approvals`、`repeat-guard` |
| 界面 | `ui`、`statusline`、`theme-default`、`keymap-default`、`commands`、`copy`、`images`、`file-ref`、`shell-mode`、`agents`、`review`、`changes`、`notify`、`usage`、`status` |
| 日常维护 | `plugins`、`profile`、`lua-console`、`update-check`、`onboarding`、`user-config` |

## 检查与重新加载

`/plugins` 列出每个插件：活动的插件前面有实心圆点，同时显示其来源、任何覆盖关系，并在下方列出每个已记录的问题及其回溯。`mare doctor` 会包含被定义了两次的插件名。

| 命令 | 作用 |
|---|---|
| `/reload` | 全量宿主重载：重新运行 `init.lua`、安装新包、启动新插件、卸载被移除的插件、重载其余插件，并发出 `reload/completed`。 |
| `/reload config` | 配置重载：重新运行 `init.lua`、比较选项变化、同步启停状态并重试待处理项。 |
| `/reload <name>` | 具名重载：原地替换目标插件定义（新定义加载失败时保留旧定义）。 |

模型请求从组装到流结束一直持有租约，所以重新加载无法在它之下替换插件。请求处于活动状态时重新加载会被拒绝；请稍后再运行。新定义加载失败的插件会继续用旧定义运行。

在 debug 构建中，每次启动都会从 `crates/mare-plugin/preset/plugins/` 读取内置插件的源码，所以编辑后运行 `/reload` 就够了。在 release 构建中，`MARE_DEV_PLUGINS=<dir>` 会让 mare 在 `<dir>/<name>.lua` 存在时，从那里读取对应的内置插件。

## 插件失败时

故障只停留在引起它的插件内部。

| 失败的内容 | 会发生什么 |
|---|---|
| 定义无法加载（语法错误、禁止的顶层访问） | 插件被跳过。mare 给出警告 `requests continue without it`，TUI 状态栏显示 `plugin <name> failed · /reload`。 |
| `apply` 抛出错误 | 插件不启动。错误被记录；其他插件正常启动。 |
| `inject` 中的服务没有提供者 | 插件等待，有提供者后启动。`/reload` 会报告它在等什么。 |
| 监听器、任务或定时器抛出错误 | 错误以通知和 `kernel/error` 事件的形式报告。插件继续运行。 |
| 投票者抛出错误或超时 | 视为 `deny`。 |
| Lua 长时间不 yield 地运行 | 看门狗中止该回调。 |

当插件提供某个关键服务（`agent`、`llm`、`tools`、`permissions`、`approvals`、`session`）、声明了 `required = true`，或来自必需包（`required = true`）时，它的失败会阻止所有模型请求，并显示原因。完整的 `/reload` 成功后，阻止状态解除。普通可选插件或可选包的失败只会给出警告，不影响后续请求。

看门狗的限制是 `watchdog_ms`，默认 10000。它在 `init.lua` 运行之前读取，所以要在命令行或环境变量中设置：`mare --set watchdog_ms=20000` 或 `MARE_WATCHDOG_MS=20000`。在失控的代码外面包一层 `pcall` 不会吞掉这个中止。

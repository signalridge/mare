# `loader` 宿主插件

插件树运行在 cordis-lua 的加载器（`cordis.loader`）之上。mare 将其包装成宿主插件 `mare_kernel.plugins.loader`，由启动引导片段（`boot.lua`）挂载在根上下文中，提供 `loader` 服务。类型定义位于 `types/mare/host/loader.lua`。

宿主插件只使用有类型声明和文档的原语。下面这些表由启动引导片段作为配置传给宿主插件；运行中的 `prims` 对象由宿主传入，既不是全局变量，也不是可 `require` 的模块（而内部的 `mare_kernel.*` 源模块本身遵循开放 `require` 模型，但未在 `types/` 中声明为公开接口）。

## 配置

```lua
root:use(require("mare_kernel.plugins.loader"), {
  component = component, -- fun(name): 按插件名返回对应的 cordis 组件
  prims = prims,         -- 加载器读取的插件记录原语；会读取 prims.refused 和 prims.profile_record
})
```

`component(name)` 是插件树获取组件的唯一入口。传入未知名称会触发 `module '<name>' not found:` 错误，加载器只针对该条记录上报错误而不影响其他插件。

## 插件记录原语

| 原语 | 返回内容 |
|---|---|
| `prims.rows()` | 按加载顺序返回当前定义和配置对应的行列表，末尾附带因配置禁用而关闭的行。 |
| `prims.refused()` | 返回被静态 `provide` 冲突检查拒绝的插件定义。 |
| `prims.profile_record(label, ms)` | 向启动耗时记录（`/profile`、`--startuptime`）写入一条计时。加载器会记录每个插件 `apply` 的 CPU 耗时（`plugin <name> (<origin>)`）。 |

每一行是一个 `{ id, name, config, disabled, origin, path, overrides }` 表。`id` 和 `name` 为插件名；当 `plugins.<name>.enabled = false` 或被精简预设关闭时，`disabled` 为 `true`。加载器主要读取 `id`、`name`、`config` 和 `disabled`，同时保留 `origin`、`path` 和 `overrides` 供 `dump()` 诊断使用。

声明了已被其他定义提供的服务的插件会被拒绝，它们不会进入行列表（行以插件名为键，避免被拒绝的行顶替正在运行的插件）。`dump()` 会在行列表之后单独列出被拒绝的定义及原因，这些定义始终不会运行。

## `loader` 服务

| 方法 | 调用方 | 作用 |
|---|---|---|
| `loader:apply_rows(rows)` | 启动、`/reload config` | 使插件树与 `rows` 保持一致：启动新增行，销毁已移除或禁用的行并等待其清理结束，为配置发生变化的行更新配置；其余 fiber 保持不动。上次失败过的 fiber 会在配置更新后重试一次。 |
| `loader:replace(name, component)` | `/reload <name>` | 替换运行中的定义，并按新定义重启对应 fiber。若新定义应用失败，会恢复旧定义并让 Future 抛出错误。 |
| `loader:dump()` | `mare plugin tree`、`/plugins` | 为每行输出一条记录：来源、路径、覆盖信息、禁用状态、fiber 状态、等待中的缺失服务（`missing`）、提供的服务和 effect 数量，随后列出被拒绝的定义。 |

`apply_rows` 和 `replace` 都会返回 cordis Future，调用时需使用 `:await()`。

`missing` 读取自每个 fiber 的 `_store`（cordis-lua `api-core.md`“Diagnostics”一节记录的只读诊断字段）。mare 其他任何代码都不会读取 `_store`。

## `mare plugin tree`

`mare plugin tree` 按每插件一行的格式打印 `loader:dump()`（包含 id、来源、fiber 状态 / `disabled` / `refused` 及详情）。加 `--json` 参数可以直接输出原始结构化结果。

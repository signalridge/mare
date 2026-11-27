# 启动引导片段与 `task` 宿主插件

`crates/mare-kernel/lua/mare_kernel/boot.lua` 基于 `mare.task` 与根上下文构建 cordis 宿主，挂载宿主插件（`task`、`loader`），并将每个插件定义转为 cordis 组件（`component.lua`，包含所有者记录、插件的 `lua/` 目录以及用于失败和重载报告的 `apply` 执行结果）。类型定义位于 `types/mare/host/kernel.lua`。

## 原语

启动引导片段从 Rust（`lua_env/cordis_host.rs`）接收 `prims`，宿主插件拿到其中带类型的部分作为自身配置。以下为导出的常用原语（引导片段内部自用的 `capture`、`spawn_root`、`run_root` 未列入）：

| 原语 | 作用 |
|---|---|
| `handler_failed(plugin, what, err)` | 上报插件回调错误：写入问题列表、记日志并触发 `kernel/error` |
| `unload_modules(name)` | 当插件 fiber 卸载时，从 `package.loaded` 中清除该插件拥有的模块 |
| `mark_active(name, owner \| nil, config?)` | 记录当前活跃的插件实例及其生效的 `config` 表；`owner` 为 `nil` 时从内核活跃集合中移除 |

所有者、事件、定义和重载表结构定义在 [prims 类型文件（英文源码）](https://github.com/signalridge/mare/blob/main/crates/mare-plugin/preset/types/mare/host/prims.lua)。插件记录结构见 [loader 宿主插件](host-loader.md)。

## 启动与重载

`booted.start()` 与 `booted.reload(name, targets)` 运行在宿主自身上下文中（`prims.run_root`）。由受限 peer 或工具内部触发的重载，不会把调用方自身的权限授予重建后的插件 fiber。

- 对运行中的单个插件执行 `reload(name)` 时会调用 `loader:replace`；若新定义应用失败，会恢复原有定义。
- 否则按逆序（最后一行先退）逐个将行移出插件树：先移除已消失的行，再移除 `targets`，随后用完整行列表按新定义和配置重建目标插件。

## `task`

`ctx:after(ms, fn)`、`ctx:every(ms, fn)` 与 `ctx:spawn(fn, ...)` 会启动归属于当前调用方 fiber 的任务，并在 fiber 卸载时自动中止。需要这些方法时，插件应声明 `inject = { "task" }`。

## `kernel`

`kernel` 是带契约的宿主服务（`mare_kernel.plugins.kernel`，一个 `mare.Service`），替代早期的 Ctx 方法 `plugins`、`problems`、`is_builtin`、`provides`、`reload` 和 `reload_barrier`；使用它需声明 `inject = { "kernel" }`。
调用 `kernel:reload(name?)` 会走完整重载流程（先由 Rust 准备新定义，再切换插件树），执行与发起调用者解耦，避免重载销毁调用方自身时中断操作。

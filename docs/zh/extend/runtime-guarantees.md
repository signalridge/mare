# 插件运行时承诺

mare 对插件作者在生命周期、服务、登记、重载和清理上的承诺。插件可以依赖这里的每一条；破坏其中任何一条都算不兼容变更。没有列在这里的行为仍然写在 [插件](plugins.md) 和 [服务与事件](services-and-events.md) 中，但不属于这份清单。

每条承诺有一个键。要找某条承诺的测试，搜索它的键即可：`rg 'guarantee: effects-dispose-in-reverse-start-order'`。

## 依赖与等待

| 键 | 承诺 |
|---|---|
| `inject-waits-instead-of-failing` | 插件的 `inject` 里有无人提供的服务时不算失败：它等待（PENDING），宿主报告它在等什么，一旦出现提供者就启动。 |
| `inject-cycle-refused-and-the-rest-starts` | 互相注入只有对方才提供的服务的插件在运行前被拒绝；问题信息会指出这个环，环外的插件照常启动。 |
| `dependents-stop-before-their-provider-and-restart-on-the-new-one` | 提供者被重载或替换时，注入其服务的插件先于它卸载，然后在新提供者上重新启动，绝不会留在旧提供者上。 |
| `dependent-waits-while-its-provider-is-gone` | 提供者离开时，依赖它的插件被卸载（清理会运行）并进入等待；这不算失败，提供者回来后它们再次启动。 |
| `provider-outlives-dependents-in-flight-cleanup` | 提供者的卸载会等待依赖方仍在进行的清理，因此依赖方的清理仍可调用它启动时所用的服务。 |

## 服务

| 键 | 承诺 |
|---|---|
| `one-provider-per-service-name` | 一个服务名只有一个提供者。在 `provide` 里声明同名服务的第二个定义会在运行前被拒绝，错误信息给出两个文件，第一个保留该服务。 |
| `no-fallback-when-a-provider-leaves` | 运行时向已占用的名字提供服务会以 `already_provided` 被拒绝，原持有者继续应答；持有者离开后不会回退到别的提供者，直到有插件重新提供之前该名字都没有提供者。 |
| `undeclared-raw-provide-refused` | 已声明的服务只能通过 `ctx:plugin` 加 `mare.Service` 类提供，因而每次调用都经过契约校验；在已声明的名字下直接 `ctx:provide` 会被拒绝。 |
| `host-methods-stay-with-the-host` | 服务契约中由宿主实现的方法不能被插件实现或替换。 |

## 登记的归属

| 键 | 承诺 |
|---|---|
| `ctx-registrations-withdrawn-on-dispose` | 通过插件 `ctx` 登记的一切（监听器、服务、定时器、任务、工具、命令、按键绑定、状态栏片段、effect）在插件卸载时撤回。 |
| `registration-withdrawn-on-caller-unload` | 通过其他插件的服务做的登记归调用方所有：调用方离开时撤回；它拿到的释放函数可重复调用，显式释放过的不会再释放一次。 |
| `release-after-provider-is-gone-does-nothing` | 提供者先离开时，原调用方手里的释放函数什么都不做，也绝不调用已卸载的提供者。 |
| `no-registration-escapes-an-unload` | 插件卸载期间在清理里做的登记不会比插件活得久：之后永远不会运行。 |
| `shared-cleanup-runs-once-per-registration` | 两个插件登记同一个清理函数算两份登记：卸载其中一个插件只运行一次，另一份保持不变。 |

## 清理

| 键 | 承诺 |
|---|---|
| `effects-dispose-in-reverse-start-order` | 插件的清理按后进先出的顺序启动，每个恰好一次。顺序要紧的清理请写进同一个 effect。 |
| `cleanup-error-does-not-stop-the-rest` | 某个清理抛错不会阻止其余清理；它的错误只报告一次。 |
| `yielding-cleanups-run-concurrently` | 会让出的清理（等定时器、进程、网络）并发运行：一个不等另一个结束，全部结束时卸载才结束。 |
| `turning-a-plugin-off-waits-for-its-cleanup` | 关闭一个插件会等它的清理（包括异步部分）结束，重载才报告完成。 |

## 失败与回滚

| 键 | 承诺 |
|---|---|
| `failed-apply-rolled-back-before-it-is-reported` | `apply` 抛错时，在报告失败之前它提供和登记的东西已经撤回，清理已经运行。 |
| `failed-reload-keeps-the-old-definition-serving` | 重载的新定义在 `apply` 中失败时，旧定义作为新实例重新运行（不恢复之前的内存状态），依赖方重新应用，并报告失败。完整 `/reload` 会让这样的插件继续运行。 |
| `broken-definition-reload-changes-nothing` | 新定义无法读取或在 `apply` 前校验失败的重载，不会动正在运行的实例。 |
| `dependents-put-back-with-their-provider` | 一次重载中提供者和依赖方的新定义都失败时，各自回到旧定义，依赖方排在提供者之后。 |
| `handler-error-reported-and-plugin-keeps-running` | 监听器、任务或定时器抛错时每次都会报告（`kernel/error`、通知），插件保持加载；最新的错误留在问题列表里，直到完整 `/reload`。 |

## 重载

| 键 | 承诺 |
|---|---|
| `config-change-restarts-only-its-plugin` | 修改某个插件的选项只重启这个插件。 |
| `rejected-config-keeps-the-previous-one` | 被插件 schema 拒绝的配置不会生效，旧配置继续运行；改正后的编辑会生效。 |
| `cancelled-reload-waits-for-disposal` | 清理进行中被取消的重载不会留下卸载了一半的插件：重试会先等这次卸载完成。 |
| `full-reload-keeps-startup-and-session` | 完整 `/reload` 重启每个插件，但不重放启动流程（不会再有 `app/ready`、`session/start`），并保持用户当前所在的会话。 |
| `reload-rereads-plugin-modules` | 重载插件会丢弃它 `require` 过的模块，修改后的模块文件会被重新读取。 |

## 关机与卡住的插件

| 键 | 承诺 |
|---|---|
| `shutdown-is-bounded-and-names-unfinished-cleanups` | 退出时等待插件清理，最多 `shutdown_grace_ms`。超过期限后不再等待，并列出仍在卸载的插件（包括卸载在退出前就已开始的）；其余插件已经卸载。 |
| `unload-stall-reported-with-effect-labels` | 超过 `stall_ms`（默认 10 秒，0 关闭）仍在卸载的插件会被报告，附带卸载开始时它持有的 effect 标签（`ctx:effect(fn, label)`），每次卡住报告一次，出现在 `/plugins tree`、`mare plugin tree --json`、日志和 `kernel/stalled` 通知里。标签是卸载开始时的快照：列出的是所有已启动的清理，而不只是仍在运行的那个。配置变更引起的重启不经过卸载流程，这种情况的报告不带标签。 |
| `load-stall-reported-without-abort` | `apply` 超过 `stall_ms` 仍未返回的插件以同样方式报告；不会中止或隔离任何东西，启动它的重载继续等待。 |
| `pending-stall-reported-as-waiting` | 超过 `stall_ms` 仍在等待服务的插件会被报告，附带它等待的服务。它仍是 PENDING：报告不新增问题条目，也绝不把等待变成失败。 |

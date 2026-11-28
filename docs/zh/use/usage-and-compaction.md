# 用量统计与上下文压缩

mare 按会话累计 token 与费用并记录服务商额度窗口（`usage`），在逼近上下文上限前分四个阶段压缩历史消息（`compact`），并提供启动与帧耗时分析（`profile`）。

## Token 用量与费用（`/usage`）

`usage` 插件统计当前会话的 `input`、`output`、`cache_read`、`cache_write` 和 `cache_write_1h`，并把摘要压缩、`/side` / `/btw` 等主对话之外的请求一并记入当前会话。

在会话里运行 `/usage`（别名 `/cost`）可以看到：

- 总请求数（标注其中有多少次属于旁路对话或摘要等对话外请求）。
- 输入、输出、缓存读取和缓存写入 token 合计。
- 按模型目录定价或 `mare.setup { models = ... }` 中自定义 `cost` 估算的费用（ChatGPT OAuth 等订阅请求显示为 `subscription, not billed`）。
- 响应头带回的服务商额度窗口（`5h`、`weekly`、点数余额和重置倒计时）。

用量按会话独立计算。新建会话从 0 开始；恢复或分叉会话时，mare 会根据会话 JSONL 中的记录重新汇总。

## 分阶段上下文压缩（`/compact`）

`compact` 插件负责在上下文接近上限时缩减消息体积，同时把原始记录完整留在会话 JSONL 文件里。

### 自动触发时机

每次请求模型前，mare 按类型优先级计算触发阈值：先看 `tokens`（`model_thresholds.<ref>.tokens` 优先于全局 `threshold_tokens`），再看 `percent`（`model_thresholds.<ref>.percent` 优先于全局 `threshold_percent`），最后看 `ratio`（`model_thresholds.<ref>.ratio` 优先于全局 `threshold_ratio`）。同一类型内按模型配置优先于全局配置，但更高优先级的全局类型（如 `threshold_tokens`）仍优先于按模型设置的 `percent` 或 `ratio`。最终结果始终受本次请求的压力预算上限约束：

`min(configured_threshold, context_window - max_output - headroom_tokens)`

默认 `threshold_ratio = 0.8`，`headroom_tokens = 32768`（未显式配置时，在较小上下文窗口上会自动按 `context_window - max_output` 的 20% 缩放，避免压力预算归零）。用量超过阈值的 90% 时状态栏会给出提醒。也可以随时手动运行：

```text
/compact
/compact 保留数据库 schema 设计与失败用例名称
```

### 四个阶段

压缩按 `stages` 列表顺序（默认 `{ "prune", "offload", "remote", "local" }`）依次执行，每完成一步重新测量。停止条件取决于触发原因：

- **自动压缩**：只在估算用量降到阈值及以下时提前停止；若某阶段（包括摘要阶段）完成后用量仍高于阈值，会继续执行后续阶段直到满足阈值或走完列表。
- **手动 `/compact`**：即使 `prune` 或 `offload` 已使估算用量低于阈值也不会停，而是继续执行直到某个摘要阶段（`remote` 或 `local`）成功。
- **溢出恢复**：请求因超长被服务端拒绝（或触发静默溢出检测）后触发，可越过 `prune_min_saving_tokens` 下限，并持续推进直到摘要阶段成功或走完列表。只要压缩缩减了上下文，智能体就会重试请求，最多连续重试 `max_overflow_compactions` 轮（`agent` 插件默认 `3` 轮），超出后报错停止。

各阶段行为：

1. **`prune`**：把超过 `prune_tool_result_chars`（8192 字符）的旧工具输出裁剪为头部（4096）和尾部（1024），最近 `prune_protect_tokens`（40,000）保留不动。自动压缩时只有预计至少省下 `prune_min_saving_tokens`（20,000）才会执行，溢出恢复可越过该下限。
2. **`offload`**：把较早的内联图片转存为附件文件并在消息里改为路径引用，仅保留最近 `offload_keep_images`（2 张）内联。
3. **`remote`**：如果当前 provider 支持服务端压缩（如 OpenAI `/responses/compact`），优先调用远端压缩。
4. **`local`**：调用模型（或指定的廉价 `summary_model`）生成结构化摘要，原样保留最近 `keep_recent_tokens`（20,000）以及最多 `recent_user_tokens`（20,000）的较早用户消息。未能缩减体积的摘要会在落盘前被拒收。

### 失败退避与停滞（`stalled`）

自动压缩只要失败一次就会设置指数退避（从 `failure_backoff_ms` 逐次翻倍至 `failure_backoff_max_ms`）。若自动压缩耗尽本地摘要候选后上下文仍高于阈值，压缩会进入 `stalled` 状态并停止当前运行，同时输出系统提示词、摘要与保留消息各自的 token 占比，提示改用 `/new`、切换到更大窗口的模型或精简指令文件。

### 配置示例

```lua
mare.plugin("compact", {
  auto = true,
  threshold_percent = 75,              -- 或使用 threshold_tokens = 150000
  summary_model = "opencode-go/glm-5.3-flash",
  keep_recent_tokens = 20000,
  stages = { "prune", "offload", "remote", "local" },
  model_thresholds = {
    ["openai/gpt-6.1-terra"] = { percent = 80, headroom_tokens = 32768 },
  },
})
```

输入 `/context`（或 `/context all`）可以按来源查看系统提示词、工具定义、技能、指令文件和对话各自占用的 token。

## 启动与运行性能分析（`/profile`）

`profile` 插件会记录启动阶段耗时、每个插件 `apply` 的耗时、超过 16 ms 的慢渲染帧、超过阈值的慢 Lua 回调（`slow_callback_ms`、`slow_callback_record_ms`），以及会话途中工具列表的变化（工具列表变动会让服务端提示词缓存失效）。

- 会话内运行 `/profile` 打开耗时分析面板。
- 命令行加 `mare --profile-startup` 在启动完成打印耗时后退出，或用 `mare --startuptime <file>` 写出启动时间日志。

## 相关页面

- [会话](sessions.md)：JSONL 压缩记录与 `/tree`
- [上下文与指令文件](context.md)：哪些内容不会参与压缩
- [模型与 provider](models.md#按模型覆盖)：配置 `context_window` 与 `cost`

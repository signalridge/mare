# Usage and compaction

mare tracks per-session token counts, cost, and provider quota windows (`usage`), compacts long histories in four stages before hitting context limits (`compact`), and records startup and frame timings (`profile`).

## Token usage and cost (`/usage`)

The `usage` plugin counts `input`, `output`, `cache_read`, `cache_write`, and `cache_write_1h` tokens across the active session, including off-thread requests such as compaction summaries and `/side` / `/btw` queries.

Run `/usage` (alias `/cost`) inside a session to see:

- Request count (including how many were side requests outside the main conversation).
- Input, output, cache read, and cache write token totals.
- Estimated dollar cost using provider metadata or per-model `cost` overrides in `mare.setup { models = ... }` (subscription requests such as ChatGPT OAuth show as `subscription, not billed`).
- Latest provider quota windows (`5h`, `weekly`, credits, and reset countdowns) reported via response headers.

Totals belong to the session. Starting a new session resets counters to zero, and resuming or forking a session recounts usage from the saved JSONL entries.

## Staged compaction (`/compact`)

The `compact` plugin keeps conversations inside the model's context window while leaving the original history intact in the session JSONL file.

### When automatic compaction runs

Before each model request, mare computes the trigger threshold by kind in precedence order: `tokens` (`model_thresholds.<ref>.tokens` else `threshold_tokens`), then `percent` (`model_thresholds.<ref>.percent` else `threshold_percent`), then `ratio` (`model_thresholds.<ref>.ratio` else `threshold_ratio`). Within each kind, a per-model entry wins over the global option, while a global higher-precedence kind (such as `threshold_tokens`) still takes precedence over a per-model `percent` or `ratio`. The result is always capped at the request's pressure budget:

`min(configured_threshold, context_window - max_output - headroom_tokens)`

Defaults are `threshold_ratio = 0.8` and `headroom_tokens = 32768`. The status line warns when usage exceeds 90% of the threshold. You can also compact manually at any time:

```text
/compact
/compact preserve the database schema decisions and failing test names
```

### The four stages

Compaction walks `stages` in order (`{ "prune", "offload", "remote", "local" }`) and re-measures after each stage. Stopping behavior depends on why compaction ran:

- **Automatic compaction** stops as soon as estimated usage drops to or below the threshold; if usage is still above the threshold after an earlier stage (including a summary stage), it continues walking `stages` until the threshold is met or the list ends.
- **Manual `/compact`** continues past `prune` and `offload` even if usage is below the threshold, and stops once a summary stage (`remote` or `local`) succeeds.
- **Overflow recovery** (after a provider rejects a request as too large, or on silent overflow) can bypass `prune_min_saving_tokens` and walks stages until a summary stage succeeds or the list ends. If compaction shrinks the context, the agent retries the request, up to `max_overflow_compactions` consecutive rounds (default `3` on the `agent` plugin) before stopping with an error.

Stage details:

1. **`prune`**: Trims old tool outputs longer than `prune_tool_result_chars` (8192 chars) to head (4096) and tail (1024), leaving the most recent `prune_protect_tokens` (40,000) untouched. In automatic runs, it applies only when it saves at least `prune_min_saving_tokens` (20,000); overflow recovery may bypass that floor.
2. **`offload`**: Moves older inline images to attachment files referenced by path, keeping the newest `offload_keep_images` (2) inline.
3. **`remote`**: Uses provider-side compaction (such as OpenAI `/responses/compact`) when supported by the provider.
4. **`local`**: Calls the model (or a cheaper `summary_model`) to write a structured summary, keeping the most recent `keep_recent_tokens` (20,000) and up to `recent_user_tokens` (20,000) of older user messages verbatim. Non-converging summaries that fail to shrink context are rejected before saving.

### Failures, backoff, and `stalled`

Any single automatic compaction failure sets an exponential backoff (`failure_backoff_ms` doubling up to `failure_backoff_max_ms`) before automatic compaction retries. If local summary candidates are exhausted during automatic compaction and the context is still above the threshold, compaction marks the session `stalled` and stops the current run with a breakdown of system/summary/kept tokens, advising `/new`, a larger-window model, or trimming instructions.

### Configuration options

```lua
mare.plugin("compact", {
  auto = true,
  threshold_percent = 75,              -- or threshold_tokens = 150000
  summary_model = "opencode-go/glm-5.3-flash",
  keep_recent_tokens = 20000,
  stages = { "prune", "offload", "remote", "local" },
  model_thresholds = {
    ["openai/gpt-6.1-terra"] = { percent = 80, headroom_tokens = 32768 },
  },
})
```

Run `/context` (or `/context all`) to inspect how many tokens the system prompt, tools, skills, instruction files, and messages consume right now.

## Startup and runtime profiling (`/profile`)

The `profile` plugin records startup stage timings, individual plugin `apply` times, slow UI frames (> 16 ms), slow Lua callbacks (`slow_callback_ms`, `slow_callback_record_ms`), and mid-session changes to the tool definition list (which invalidate provider prompt caches).

- Run `/profile` inside a session to open the timing overlay.
- From the shell, pass `mare --profile-startup` to print startup timings and exit, or `mare --startuptime <file>` to write a startup log.

## Related

- [Sessions](sessions.md) for JSONL compaction records and `/tree`
- [Context and instructions](context.md) for what stays outside compaction
- [Models and providers](models.md#per-model-overrides) for setting `context_window` and `cost`

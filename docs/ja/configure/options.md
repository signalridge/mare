# オプション

コアオプション。`init.lua` の `mare.setup { ... }`、環境変数 `MARE_<NAME>`、コマンドラインの `--set name=value` で設定します。

> このページは `scripts/gen_docs.py` がコードから生成します。手で編集しないでください。説明文はコード由来のため英語のままです。

プラグインのオプションは `mare.plugin("name", { ... })` で設定します。実行中のセッションでは `/config` で各値と、その値を決めたレイヤー・ファイル・行を確認できます。

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `agents` | table | `{ max_active = 16, max_depth = 4 }` | root-shared subagent limits: non-negative integers max_active (all live descendant threads) and max_depth (edges from root); zero disables delegation |
| `border` | string | `"rounded"` | panel/popup borders: rounded\|single\|double\|thick\|hidden |
| `cache_retention` | string | `"short"` | prompt cache retention tier: none\|short\|long (default short) |
| `char_widths` | table |  | display width overrides { { from = 0x2600, to = 0x26FF, width = 2 }, { char = "⚡", width = 1 }, … } |
| `copy` | table |  | /copy: { always_full = true } skips the code-block picker and copies the whole response |
| `editor` | string |  | editor for Ctrl+G (default $VISUAL / $EDITOR) |
| `effort` | string |  | **起動時のみ.** invocation-only model effort override |
| `fallback_models` | list |  | up to 3 models (provider/id or alias) tried in order when the current one is overloaded, missing or out of quota |
| `favorite_models` | table |  | models (provider/id or alias) the model-cycle key steps through; without it, the recently used models |
| `flavor` | string | `"auto"` | theme flavor: auto (OSC 11 background query)\|dark\|light\|installed name |
| `flavor_dark` | string | `"dark"` | flavor selected by auto on a dark or unknown terminal background |
| `flavor_light` | string | `"light"` | flavor selected by auto on a light terminal background |
| `icons` | string |  | icon set: nerd\|unicode\|ascii (default: auto-detect) |
| `images_protocol` | string |  | terminal image protocol: kitty\|iterm2\|sixel\|none (default: detected) |
| `key_conflicts` | table |  | extra known key conflicts { { key = "ctrl+tab", terminal = "ghostty"\|"any", by = "…", fix = "…" }, … } (appended to the built-in table) |
| `keys` | table |  | key -&gt; action shorthand |
| `logo` | boolean or string |  | startup logo on a new session: true (pixel logo) \| false \| "compact" (one line) |
| `lua_memory_mb` | number | `512` | **起動時のみ.** Lua VM memory limit in MiB (0 disables the limit). CLI/env only: --set lua_memory_mb=… or MARE_LUA_MEMORY_MB (the VM memory limit is applied before init.lua, so a value written in init.lua has no effect) |
| `markdown` | table |  | Markdown rendering: { glyphs = { bullet1 = "•", rule = "─", … }, code_line_numbers = false } |
| `model` | string |  | model as provider/id |
| `models` | table |  | per-model overrides |
| `net` | table | `{ fake_ip = "off" }` | network authorization: { fake_ip = "off"\|"auto" } ("off" default; "auto" recognizes fake-IP 198.18.0.0/15 resolution shape and relaxes non-public checks for it) |
| `news` | boolean | `true` | after mare or an enabled plugin is updated, show its NEWS.md once in the scrollback |
| `paste_burst` | boolean |  | treat a fast stream of keys as a paste (an Enter inside it inserts a newline); default on Windows only, where pastes aren't bracketed |
| `peer` | boolean or table |  | peer socket + registry: false disables; { name = "...", allow = { "agent.prompt", "events.subscribe", ... } } names the session and opens methods beyond the peer defaults; inbound messages always wait for the user |
| `perf` | table |  | cache sizes (entries) { highlight_cache = 256, markdown_cache = 128, wrap_cache = 1024, preview_cache = 256 } |
| `plugins` | table | `{ allow_insecure = false, concurrency = 8, sources = {  }, timeout = 120, url_format = "https://github.com/%s.git" }` | packages: { url_format = "https://github.com/%s.git" (short names), allow_insecure = false (http://, git://), timeout = 120 (seconds per git operation), concurrency = 8 (parallel fetches), sources = { "owner/repo", … } (more extras catalogs) } |
| `policy_timeout_ms` | number | `30000` | default timeout for policy/rewrite listeners without explicit timeout_ms (ms) |
| `prompt` | string | `"❯"` | input prompt symbol |
| `providers` | table |  | providers (mare.provider): { base_url, api_key, headers, query_params, retries, stream_idle_timeout_ms, connect_timeout_ms, wire, wire_api, login_hint, proxy, hosted_tools, cache_retention } plus wire-declared provider_fields |
| `reduce_motion` | boolean | `false` | no spinner animation or other motion: static status text instead |
| `shutdown_grace_ms` | number | `4000` | grace period (ms) for cooperative tasks during host shutdown before process-tree termination |
| `slow_callback_ms` | number | `1000` | Lua callback busy-time threshold in ms for slow-callback warnings (default 1000 ms; /profile aggregates callbacks over 2 ms) |
| `slow_callback_record_ms` | number | `1000` | Lua callback busy-time threshold in ms above which individual slow callbacks are recorded in /profile |
| `slow_callback_table_rows` | number | `100` | maximum number of (plugin, event) rows shown in the /profile callback timing table |
| `spinner` | table |  | spinner glyphs: a list { "a", "b", … } or { frames = {…}, still = "•" } (still: the marker under reduce_motion); default braille |
| `stall_ms` | number | `10000` | a plugin still loading, unloading or waiting for a service after this many ms is reported (/plugins tree, `mare plugin tree --json`, the log, a notice); never aborted; 0 turns it off |
| `terminal` | table | `{ ambiwidth = "single", color = "auto", probe = { background = true, enabled = true } }` | terminal capability, probing, and ambiwidth configuration (`terminal.{ambiwidth,color,probe.{enabled,background}}`) |
| `terminal_caps` | table |  | terminal capability overrides { { match = { TERM_PROGRAM = "…" }, caps = { sync = false, … } }, … }; ui/caps listeners may also adjust them |
| `theme` | table |  | named style overrides { MareX = spec } |
| `trust_default` | string | `"ask"` | project trust for a folder with no saved decision (after --trust/--no-trust and trust/resolve plugins): ask\|trust\|distrust |
| `ui` | table | `{ layout = { regions = {  } }, stopping = { spinner = true }, tool_call = { spinner = "braille" }, working = { elapsed = "auto", glyph = "swell", interrupt_hint = true, label = "glint", preset = "combined", sep = "dot", text_throttle_ms = 250, tips = true, tokens = true, tools = "still" } }` | UI configuration: { layout = { ... }, working = ..., tool_call = { ... }, stopping = { ... } } |
| `update_check` | boolean or string | `"notify"` | check GitHub releases for a newer mare: "notify" \| "auto" (stage standalone updates) \| false (at most daily, interactive sessions; nothing else is sent) |
| `watchdog_ms` | number | `10000` | **起動時のみ.** per-callback limit for Lua that never yields (ms). CLI/env only: --set watchdog_ms=… or MARE_WATCHDOG_MS (the watchdog starts before init.lua, so a value written in init.lua has no effect) |
| `webserver` | table | `{ accept_partial = {  }, allowed_origins = {  }, enabled = false, header_read_timeout_ms = 30000, host = "127.0.0.1", max_connections = 256, plain_http = false, port = 0, remote_admin = false, trusted_hosts = {  }, ws_max_message_bytes = 67108864, ws_ping_interval_ms = 15000, ws_pong_timeout_ms = 10000 }` | Host HTTP/WebSocket RPC listener: { enabled, host, port, token?, trusted_hosts, allowed_origins, remote_admin, tls_cert?, tls_key?, plain_http?, max_connections, header_read_timeout_ms, ws_max_message_bytes, ws_max_frame_bytes, ws_ping_interval_ms, ws_pong_timeout_ms, accept_partial } |

## プラグインオプション

組み込みプラグインが `config_schema` で宣言するオプション。`mare.plugin("<name>", { ... })` で設定します。各プラグインは `enabled = false` で無効化することもできます。

### `agent`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `max_image_wire_bytes` | integer | `20971520` | Maximum estimated inline image bytes before offloading older images. |
| `max_overflow_compactions` | integer | `3` | Consecutive context overflow, compact, retry rounds. |
| `max_parallel` | integer | `10` | Concurrency-safe tool calls of one turn run in parallel, at most this many at once. |
| `max_turns` | integer | `200` | Most model turns in one run. |
| `request_retries` | integer | `3` | Attempts for a request interrupted by a crash, shared with network retries. |
| `request_retry_base_ms` | integer | `2000` | First backoff of a request retry. |
| `request_retry_max_ms` | integer | `60000` | Largest backoff of a request retry. |

### `agents`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `preview_min_width` | integer | `84` | The preview pane shows from this terminal width (side by side). |
| `refresh_ms` | integer | `1000` | The open list refreshes in place this often (only when it changed). |
| `split.min` | integer | `80` | Narrowest the list may get, in cells. |
| `split.ratio` | number | `0.5` | Share of the width the list takes. |

### `approval-review-model`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `model` | string |  | Model ref used to review approvals. |
| `name` | string | `"model"` | Name the reviewer registers under. |

### `approvals`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `fallback` | boolean | `false` | Fall back to asking the user when the automatic reviewer fails. |
| `policy` | ask\|never\|untrusted or object | `"ask"` | **Env:** `MARE_APPROVAL_POLICY`. Approval policy: ask, never, untrusted, or a granular table. |
| `review_all` | boolean | `false` | Require review of every request. |
| `review_timeout_ms` | integer | `30000` | Time an automatic reviewer has to decide. |
| `reviewer` | string | `"user"` | Name of the approval reviewer; "user" asks the user. |

### `cells`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `execution_mode` | per-call\|persistent-in-memory\|vm-script | `"per-call"` | How script VMs are reused between calls. |
| `grace_ms` | integer | `3000` | Grace period of a script run in milliseconds. |
| `max_bytes` | integer | `1048576` | Byte cap of sandbox data; also the default of the message and output caps. |
| `max_instructions` | integer | `10000000` | Instruction budget of a script. |
| `max_message_bytes` | integer | `0` | Largest message; 0 inherits max_bytes. |
| `max_output_bytes` | integer | `0` | Largest output; 0 inherits max_bytes. |
| `max_parallel_sub_calls` | integer | `10` | Parallel tool sub-calls from a script. |
| `max_pending_calls` | integer | `128` | Pending host calls allowed at once. |
| `max_sub_calls_per_minute` | integer | `1024` | Rate cap of tool sub-calls from a script. |
| `memory_bytes` | integer | `33554432` | Memory limit of a script VM. |
| `script_persistence` | boolean | `false` | Persist scripts across sessions. |
| `stop_grace_ms` | integer | `3000` | Grace period when stopping a script. |
| `timeout_ms` | integer | `60000` | Script timeout in milliseconds. |

### `changes`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `cap_bytes` | integer | `67108864` | Cap for stored backups; the oldest turns' backups are trimmed first. |

### `compact`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `auto` | boolean | `true` | Compact automatically when the threshold is reached. |
| `failure_backoff_max_ms` | integer | `600000` | Largest backoff after repeated failures. |
| `failure_backoff_ms` | integer | `30000` | Initial backoff after a failed compaction. |
| `headroom_tokens` | integer | `32768` | Tokens kept free of the window besides the output reserve. |
| `keep_recent_tokens` | integer | `20000` | Recent context kept verbatim after a summary. |
| `model_thresholds` | map | `{}` | Per-model trigger overrides by model ref. |
| `offload_keep_images` | integer | `2` | The most recent images stay inline. |
| `prune_head_chars` | integer | `4096` | Characters kept from the head of a pruned result. |
| `prune_min_saving_tokens` | integer | `20000` | Prune stage: least saving that makes it worth running. |
| `prune_protect_tokens` | integer | `40000` | Prune stage: most recent tokens left untouched. |
| `prune_tail_chars` | integer | `1024` | Characters kept from the tail of a pruned result. |
| `prune_tool_result_chars` | integer | `8192` | Tool results longer than this are pruned. |
| `recent_user_tokens` | integer | `20000` | Older user messages kept verbatim. |
| `remote` | boolean | `true` | Run the provider-side compaction stage (can also be removed from `stages`). |
| `retain_ratio` | number | `0.16` | Share of the message budget retained after a summary. |
| `stages` | list | `{ "prune", "offload", "remote", "local" }` | Compaction stages, in order. |
| `summary_max_tokens` | number |  | Output cap of a summary request; default headroom. |
| `summary_model` | string |  | Model ref for local summaries, e.g. a cheaper one. |
| `threshold_percent` | number |  | Explicit trigger as a percent of the window. |
| `threshold_ratio` | number | `0.8` | Share of the context window that triggers compaction. |
| `threshold_tokens` | number |  | Explicit trigger as a token count. |

### `context`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `append` | string | `""` | Extra text appended to the system prompt (with APPEND_SYSTEM.md). |
| `files` | list | `{ "AGENTS.override.md", "AGENTS.md", "CLAUDE.md" }` | Project instruction file names; per directory the first existing one wins. |
| `persona` | string | `""` | Replaces only the identity line; `{{model}}` etc. are interpolated. |
| `project_doc_max_bytes` | integer | `32768` | Byte cap of the project documents. |
| `project_root_markers` | list | `{ ".git" }` | Markers of a project root; {} = only the cwd. |
| `prompt_dirs` | list |  | Prompt template directories (`<dir>/<name>.md` becomes `/name`); nil = `<config dir>/prompts` plus `.mare/prompts`. |
| `rules` | list | `{}` | Extra rule bullets. |
| `skill_dirs` | list | `{ "~/.config/mare/skills", "~/.agents/skills", ".mare/skills", ".agents/skills" }` | Directories skills are loaded from. |
| `system` | string | `""` | The whole system prompt, verbatim, like SYSTEM.md; no default section is sent. |

### `copy`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `always_full` | boolean | `false` | Always copy the full text. |

### `file-ref`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `limit` | integer | `30` | Completion items. |
| `max_bytes` | integer | `102400` | Byte cap per referenced file. |
| `max_total_bytes` | integer | `262144` | Byte cap of all files of one message together. |
| `refresh_ms` | integer | `1000` | The file index checks for changes at most this often. |

### `images`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `max_cols` | integer | `60` | Widest an image may be drawn, in cells. |
| `max_rows` | integer | `12` | Tallest an image may be drawn, in cells. |
| `read` | boolean | `true` | Show images returned by the read tool. |
| `tools` | boolean | `true` | Show images in other tools' results. |
| `user` | boolean | `true` | Show images pasted / attached to a prompt. |

### `jobs`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `batch_ms` | integer | `50` | Jobs finishing within this window share one notification. |
| `buffer_bytes` | integer | `1048576` | Ring buffer per job. |
| `kill_grace_ms` | integer | `3000` | SIGTERM, then SIGKILL after this long. |
| `max_jobs` | integer | `64` | Jobs kept in total; finished ones beyond this are pruned, least recently used first. |
| `max_running` | integer | `8` | Concurrent jobs per agent. |
| `max_wait_ms` | integer | `600000` | Cap of the job_output wait. |
| `max_wakeups` | number |  | Consecutive wake-ups of an idle agent without user input (nil = no limit). |
| `notify_inline_bytes` | integer | `2048` | Completion notices include output up to this size. |
| `read_max_bytes` | integer | `51200` | Most job_output returns per call. |
| `view_refresh_ms` | integer | `500` | /jobs output view refresh while open. |
| `view_tail_bytes` | integer | `65536` | /jobs output view: the last this many bytes. |
| `wait_timeout_ms` | integer | `30000` | Default wait of job_output. |
| `widget` | boolean | `true` | Running-jobs line above the input box. |

### `llm`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `catalog_open_refresh_ms` | integer | `600000` | Opening /model refreshes the configured providers' lists older than this. |
| `catalog_refresh_ms` | integer | `86400000` | Age after which a provider's cached model list is refreshed in the background. |
| `crash_limit` | integer | `3` | A request that crashed mare this many times is not sent again. |
| `models_dev` | boolean | `true` | Merge models.dev metadata (price, modalities, reasoning, limits) into listed models. |
| `models_dev_url` | string | `"https://models.dev/api.json"` | Where the models.dev metadata is fetched from. |
| `overloaded_retries` | integer | `3` | Retries of a 529 / overloaded response. |
| `retries` | integer | `5` | Retries of a failed request. |
| `retry_after_max_ms` | integer | `60000` | A server asking to wait longer than this fails at once. |
| `retry_base_ms` | integer | `500` | First retry backoff. |
| `retry_max_ms` | integer | `8000` | Largest retry backoff. |
| `sse_max_event_bytes` | integer | `16777216` | Longest SSE event (its lines together); longer is a malformed_response. |
| `sse_max_line_bytes` | integer | `16777216` | Longest SSE line; longer is a malformed_response. |
| `stream_idle_timeout_ms` | integer | `300000` | No bytes at all for this long (heartbeats count) is a transport error. |

### `lua-console`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `history_max` | integer | `500` | Most history entries kept. |

### `mcp`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `call_timeout_ms` | integer | `120000` | One tools/call (progress notifications reset it). |
| `files` | boolean | `true` | Read mcp.json / .mcp.json / .mare/mcp.json / $MARE_MCP_CONFIG. |
| `servers` | map | `{}` | MCP servers by name. |
| `startup_timeout_ms` | integer | `60000` | Initialize plus the first listings (at most 300000). |
| `text_limit` | integer | `20480` | Longer text results are cut in the middle. |
| `tier` | string | `"deferred"` | Default exposure tier for servers without their own. |

### `notify`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `bell` | boolean | `false` | Also ring the bell after the OSC notification. |
| `cmd` | string or table |  | Program (string) or argv list run with the event as JSON on stdin. |
| `cmd_timeout_ms` | integer | `10000` | Timeout of `cmd` in milliseconds. |
| `dedupe_ms` | integer | `10000` | Window in which an identical notification is dropped. |
| `min_turn_ms` | integer | `5000` | Shortest turn that notifies. |
| `on_done` | boolean | `true` | Notify when a turn is done. |
| `on_input` | boolean | `true` | Notify when input is needed. |
| `only_unfocused` | boolean | `true` | Notify only while the terminal is unfocused. |
| `protocol` | string | `"auto"` | Notification protocol; "bel" is just the bell, "native" a desktop notification. |

### `onboarding`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `enabled` | boolean | `true` | Offer the guided first-run login when no credentials exist. |
| `plugins` | boolean | `true` | Offer optional plugins on first open. |

### `permissions`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `accept_partial` | list |  | Sandbox dimensions accepted as unenforced when the platform backend cannot provide them. |
| `dangerous.disable` | list | `{}` | Built-in rules to turn off by name. |
| `dangerous.enabled` | boolean | `true` | Whether dangerous-command detection is on. |
| `dangerous.rules` | list | `{}` | Extra argv prefixes treated as dangerous. |
| `deny_read` | list | `{}` | Literal deny-read paths. |
| `network` | boolean or table | `true` | Network access of the `auto` preset: true (open), false, or a deny/allow table. |
| `parse.deny_commands` | list | `{}` | Command words that make a command not analyzable. |
| `parse.safe_sinks` | list | `{}` | Extra fixed redirect targets that keep a command analyzable. |
| `preset` | read-only\|auto\|full-access\|&lt;presets&gt; | `"auto"` | Sandbox and approval preset (a built-in one or a name from `presets`). |
| `presets` | map | `{}` | Extra presets by name. |
| `readable` | list | `{}` | Extra readable roots for the `auto` preset. |
| `rules` | list | `{}` | Permission rules. |
| `rules_file` | string |  | File of persisted rules ("always allow"); default &lt;state&gt;/permissions.json. |
| `sensitive.disable` | list | `{}` |  |
| `sensitive.enabled` | boolean | `true` |  |
| `sensitive.patterns` | list | `{}` |  |
| `suggest_after` | integer | `3` | Consecutive approvals of the same kind before suggesting a wider preset. |
| `unix_sockets` | list | `{}` | Unix sockets reachable while the network is restricted. |
| `untrusted_preset` | string | `"read-only"` | Preset untrusted projects start with, whatever `preset` says. |
| `writable` | list | `{}` | Extra writable roots for the `auto` preset. |

### `plugins`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `watch_ms` | integer | `2000` | Poll interval of the package watcher. |

### `provider-anthropic`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `answer_headroom` | integer | `8192` | Room kept for the answer next to the reasoning budget. |
| `base_url` | string | `"https://api.anthropic.com"` | API endpoint root. |
| `cache_retention` | none\|short\|long |  | Prompt cache retention tier: "none" \| "short" \| "long" (nil = global/short). |
| `default_max_output` | integer | `32000` | Output cap when the model declares none. |
| `headers` | map | `{}` | Extra request headers. |
| `reasoning_budgets` | map | `{ high = 24576, low = 4096, medium = 10240, minimal = 1024, xhigh = 32768 }` | Reasoning budget per effort for non-adaptive models. |
| `reasoning_display` | string | `"summarized"` | How reasoning is returned: "summarized" or "omitted". |
| `transport` | string | `"auto"` | "auto", "http" or a transport plugin's name (`provider/transport`). |

### `provider-faux`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `script` | string |  | Scripted replies. |
| `script_json` | string |  | Scripted replies as JSON. |

### `provider-google`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `base_url` | string | `"https://generativelanguage.googleapis.com/v1beta"` | API endpoint root. |
| `headers` | map | `{}` | Extra request headers. |
| `transport` | string | `"auto"` | "auto", "http" or a transport plugin's name (`provider/transport`). |

### `provider-openai`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `auth_base_url` | string | `"https://auth.openai.com"` | Auth endpoint root. |
| `base_url` | string |  | API-key endpoint override (default https://api.openai.com/v1). |
| `cache_retention` | none\|short\|long |  | Prompt cache retention tier: "none" \| "short" \| "long" (nil = global/short). |
| `callback_ports` | list |  | Local ports tried for the browser login callback (default 1455, 1457). |
| `codex_base_url` | string | `"https://chatgpt.com/backend-api"` | ChatGPT (Codex) endpoint root. |
| `compact_timeout_ms` | integer |  | Timeout for /responses/compact requests (ms). |
| `device_code_timeout_ms` | integer |  | How long the device-code login waits (default 15 minutes). |
| `headers` | map | `{}` | Extra request headers, merged over the computed ones (e.g. a proxy's auth header). |
| `originator` | string | `"mare"` | Originator reported to the API. |
| `reasoning_summary` | string |  | Reasoning summary mode sent with requests (default "auto"). |
| `session_header` | string |  | Request header carrying the session id on every request of a session; nil = none. |
| `stream_idle_timeout_ms` | integer | `300000` | No bytes for this long ends a stream as a transport error. |
| `transport` | string | `"auto"` | "auto", "websocket", "http" or a transport plugin's name (`provider/transport`). |
| `verbosity` | string |  | Overrides the model default (low\|medium\|high). |
| `web_search` | disabled\|cached\|indexed\|live |  | OpenAI hosted web_search tool; nil / "disabled" = off (explicit opt-in). |
| `web_search_config` | map |  | Policy / config passthrough of the web_search tool. |
| `websocket_connect_timeout_ms` | integer | `15000` | WebSocket connect timeout. |

### `provider-openai-compat`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `cache_retention` | none\|short\|long |  | Prompt cache retention tier: "none" \| "short" \| "long" (nil = global/short). |
| `headers` | map | `{}` | Extra request headers for every endpoint; per-endpoint `headers` win. |
| `transport` | string | `"auto"` | "auto", "http" or a transport plugin's name (`provider/transport`). |

### `provider-opencode-go`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `base_url` | string | `"https://opencode.ai/inference/go"` | The gateway root; each route's address is base_url .. path. |
| `cache_retention` | none\|short\|long |  | Prompt cache retention tier: "none" \| "short" \| "long" (nil = global/short). |
| `custom_tools` | boolean | `false` | Send `custom` (freeform) tools; the gateway rejects them with a 400. |
| `default_route` | string | `"responses"` | Route of every model unless something below picks another. |
| `model_routes` | map | `{}` | Glob over model ids (or provider/id) to route; the longest pattern wins. |
| `models_dev_routes` | boolean | `false` | Take a listed model's route from its models.dev protocol (only onto configured routes). |
| `routes` | map | `{ anthropic = { auth = "bearer", path = "/anthropic", replay_unsigned_reasoning = true, wire = "anthropic-messages" }, responses = { path = "/openai/v1", wire = "openai-responses" } }` | Gateway routes by name: path, llm wire, auth and per-route wire options. |
| `session_header` | string | `"x-opencode-session"` | Stable per-session header on every request, main and auxiliary (400 MissingSessionID without it). |
| `supports_long_cache_retention` | boolean |  | Whether the provider supports long (1h / 24h / 12h) prompt cache retention. |

### `repeat-guard`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `arguments_preview_chars` | integer | `500` | Bounds the quoted arguments, never the detection. |
| `exclude` | list | `{}` | Tool-name patterns (`*` wildcard) never guarded. |
| `include` | list | `{}` | Tool-name patterns (`*` wildcard) to guard; empty = every tool. |
| `thresholds` | list | `{ 3, 5, 8 }` | Repeat counts at which the guard steps in. |

### `review`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `collapse_lines` | integer | `400` | Diffs longer than this many lines start collapsed. |
| `prompt_diff_bytes` | integer | `122880` | /review: the diff in the reviewer's prompt is cut at this many bytes. |
| `scopes` | list | `{ "turns", "unstaged", "staged", "committed" }` | Diff scopes offered, in order. |
| `tools` | list | `{ "read", "ls", "bash" }` | The reviewer's tools (read-only). |
| `untracked_file_bytes` | integer | `262144` | Most bytes of one untracked file. |
| `untracked_files` | integer | `50` | Most untracked files included. |

### `session`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `branch_summary` | boolean | `true` | Summarize the branch left behind when switching branches. |
| `picker_looks` | map |  | Looks to add or replace, by name. |
| `picker_segments` | map |  | Custom picker segments by name. |
| `picker_split` | map |  | The list's share of a wide picker next to its preview. |
| `picker_style` | string | `"comfortable"` | Default /resume look: a built-in one or a name from `picker_looks`. |
| `resume_cwd` | ask\|current\|session | `"ask"` | Directory used when resuming a session recorded in another one. |
| `transcript_poll_ms` | integer | `500` | Live refresh of an open sub-session transcript. |

### `shell-mode`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `context_max_bytes` | integer | `51200` | Bytes of output the model sees (middle-truncated beyond this). |
| `context_max_lines` | integer | `2000` | Lines of output the model sees (middle-truncated beyond this). |
| `preview_lines` | integer | `5` | Lines of the collapsed transcript preview; Ctrl+O always shows everything. |
| `timeout_ms` | integer | `600000` | Timeout of a `!cmd` command. |

### `spill`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `backend` | string | `"local"` | Name of the spill store backend. |
| `cleanup_period_days` | integer | `30` | Days stored spills are kept. |
| `root` | string |  | Directory of the local spill store. |

### `status`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `exit_timeout_ms` | integer | `3000` | How long shutdown waits for `exit` hooks in total. |
| `message_max` | integer | `120` | Message length cap. |
| `throttle_ms` | integer | `200` | Default per-hook coalescing window. |
| `timeout_ms` | integer | `2000` | Default per-hook command timeout (the process is killed after it). |
| `title` | boolean | `true` | Set the terminal title (OSC 2) from the state. |
| `title_format` | string | `"%s — mare"` | Terminal title; %s = "working: …" / "waiting: …" / project name. |

### `statusline`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `changed_files` | boolean | `true` | Show `± N`: files `git status` reports changed (refreshed once per turn). |
| `command` | string or table |  | Command (string, run via the shell, or argv table) whose output joins the bar. |
| `command_replaces` | boolean | `false` | Replace the built-in segments instead of adding the command's output to them. |
| `command_timeout_ms` | integer | `1000` | Timeout of `command`. |
| `cwd_min_terminal_width` | integer | `100` | The directory segment shows from this terminal width on. |
| `cwd_width` | integer | `24` | Cells of the working directory (longer paths keep their last components). |
| `gauge_width` | integer | `6` | Cells of the context usage bar. |
| `git_timeout_ms` | integer | `10000` | Timeout of git queries. |
| `hidden` | list | `{}` | Segments not shown. |
| `layout` | string or table |  | The bar's layout: a built-in look, a name in `looks`, or a look table. |
| `layout_margin` | integer | `2` | Cells kept free at the right edge when fitting a layout. |
| `layout_opts` | map |  | Overrides single elements of the layout. |
| `looks` | map |  | Adds or replaces named looks. |
| `pr` | boolean | `true` | Show the current branch's pull request via `gh pr view` (when gh is installed). |
| `pr_timeout_ms` | integer | `5000` | Timeout of the pull request query. |

### `subagent`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `extra_agent_dirs` | list | `{}` | Additional agent definition directories; project-relative paths require trust. |
| `notify_inline_bytes` | integer | `4096` | Completion notices include a sub-agent's output up to this size. |

### `theme-default`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `blend_background` | boolean | `true` | Blend the palette with the terminal background. |
| `colors` | map |  | Palette overrides by base (dark / light). |
| `dirs` | list |  | Theme directories; default `<config dir>/themes`. |
| `flavors` | map |  | Flavors by name. |
| `ratios` | map |  | Background blend ratios. |

### `tools`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `bash_max_timeout_ms` | integer | `600000` | Largest bash timeout a call may ask for. |
| `bash_read_bytes` | integer | `4194304` | Most output a finished command returns from its job buffer. |
| `bash_timeout_ms` | integer | `120000` | Default bash timeout. |
| `cancel_grace_ms` | integer | `2000` | After cancellation, how long a tool may take to stop by itself. |
| `edit_max_bytes` | integer | `10485760` | Largest file edit / apply_patch accept. |
| `edit_mode` | auto\|apply_patch\|replace | `"auto"` | Edit tool: "auto" (apply_patch for GPT models, replace otherwise), "apply_patch" or "replace". |
| `env.exclude` | list | `{}` | More names to drop. |
| `env.include` | list | `{}` | Names to keep despite the scrubbing. |
| `env.set` | map | `{}` | Name to value, added after scrubbing. |
| `image_max_dim` | integer | `2000` | Images read by `read` are downscaled to fit. |
| `live_tail_bytes` | integer | `4096` | Running-command output shown in the UI. |
| `ls_limit` | integer | `500` | Default `ls` entry limit. |
| `max_bytes` | integer | `51200` | Tool output truncation: bytes. |
| `max_line_chars` | integer | `500` | Search output: per-line cap. |
| `max_lines` | integer | `2000` | Tool output truncation: lines. |
| `powershell` | string or boolean | `"auto"` | PowerShell tool: "auto" (on Windows), true, false or "replace". |
| `prelude` | string or table |  | String or list run before every shell command (bash tool, `!cmd`), e.g. 'eval "$(mise env -s bash)"'. |
| `read_before_edit` | boolean | `true` | Require a read of a file in this session before editing or overwriting it. |
| `read_max_line_bytes` | integer | `2000` | Read: lines longer than this are cut on a UTF-8 boundary. |
| `read_stream_min_bytes` | integer | `10485760` | Files at or above this size are read by streaming the requested window. |
| `search_results` | integer | `5` | Default result count of tool_search. |
| `shell` | string |  | Path of the shell; default $SHELL (bash/zsh) or bash; Windows: Git Bash. |
| `shell_buffer_bytes` | integer | `1048576` | Output of one command the persistent shell keeps in memory. |
| `shell_session` | string | `"persistent"` | "persistent" (one shell per agent) or "fresh" (a new shell per call). |
| `stall_ms` | integer | `30000` | A command silent this long is returned as "still running"; 0 = only the timeout. |
| `timeout_ms` | integer | `600000` | Generic per-tool timeout. |
| `update_interval_ms` | integer | `150` | UI update throttle for running commands. |

### `ui`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `diff_fold_lines` | integer or boolean | `8` | Rows a compact diff keeps before folding the rest (false = show full diff). |
| `double_esc_ms` | integer | `600` | Esc Esc window (the /undo picker when idle). |
| `group_lines` | integer | `4` | Merged lines an exploration group keeps; older ones become "N earlier". |
| `preview_lines` | integer | `2` | Compact view (Ctrl+O toggles): lines of command / output shown per tool call. |
| `tool_update_interval_ms` | integer | `100` | Refresh of live tool output. |
| `widget_area_ratio` | number | `0.5` | Share of the terminal height the widget area may use, minus the input box and status bar. |
| `widget_reserved_rows` | integer | `4` | Rows kept free of widgets. |

### `update-check`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `enabled` | boolean | `true` | Check for a newer release. |
| `interval_hours` | number | `24` | Hours between checks. |
| `plugins` | boolean | `true` | Also check declared git plugins for updates. |
| `timeout_ms` | integer | `5000` | Request timeout in milliseconds. |
| `url` | string | `"https://api.github.com/repos/signalridge/mare/releases/latest"` | Release API endpoint to query. |

### `web`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `allowed_domains` | list |  | Native search: only these domains (exclusive with blocked_domains). |
| `blocked_domains` | list |  | Native search: never these domains (exclusive with allowed_domains). |
| `context_size` | low\|medium\|high |  | Native search context size. |
| `fetch` | boolean | `true` | Offer the web fetch tool. |
| `fetch_max_output_chars` | integer | `200000` | Most characters a fetch returns. |
| `fetch_provider` | string |  | Fetch provider name (overridden by MARE_WEB_FETCH_PROVIDER). |
| `fetch_timeout_ms` | integer | `30000` | Fetch timeout. |
| `location` | map |  | Approximate user location passed to native search. |
| `native_max_uses` | integer | `5` | Most uses of a provider's native search per request. |
| `native_version` | string |  | Version of the provider-native search tool. |
| `search` | boolean | `true` | Offer the web search tool. |
| `search_max_queries` | integer | `10` | Most queries in one search call. |
| `search_max_results` | integer | `10` | Most results per query. |
| `search_provider` | string |  | Search provider name (overridden by MARE_WEB_SEARCH_PROVIDER). |
| `search_summary_max_chars` | integer | `800` | Characters kept of a result summary. |
| `search_timeout_ms` | integer | `30000` | Search timeout. |
| `web_search` | disabled\|cached\|indexed\|live | `"cached"` | Web search mode. |

### `web-fetch-http`

| オプション | 型 | 既定値 | 説明 |
|---|---|---|---|
| `max_html_depth` | integer | `128` | Maximum DOM nesting depth during conversion. |
| `max_html_nodes` | integer | `50000` | Maximum DOM node count traversed during conversion. |
| `max_html_tags` | integer | `100000` | Maximum HTML tag count parsed during conversion. |
| `max_output_bytes` | integer | `100000` | Maximum extracted text/markdown output size in bytes. |
| `max_redirects` | integer | `5` | Maximum number of HTTP redirects to follow. |
| `max_response_bytes` | integer | `5000000` | Maximum raw HTTP response body size in bytes. |
| `max_url_length` | integer | `8192` | Maximum allowed URL length in bytes. |
| `redirect` | string | `"same-origin"` | Redirect policy: same-origin, follow, or manual. |
| `timeout_ms` | integer | `30000` | Overall fetch request timeout in milliseconds. |
| `user_agent` | string | `"mare-web-fetch/1"` | User-Agent header sent on fetch requests. |

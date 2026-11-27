# Channel & Backpressure Registry

Every long-lived or cross-boundary channel in `mare` is registered here by class:
producer, consumer, capacity, full-queue behavior (`block`, `coalesce`, or `reject`),
ownership, and upper-bound justification.

## 1. Bounded Channels (External Input & Model-Driven Execution)

Channels driven by out-of-process input
(RPC, peer, WS, HTTP, MCP, ACP, netproxy) or model-driven execution (tool output,
subprocess/PTY output, tap event streams) MUST be bounded.

| Class | Location | Producer | Consumer | Capacity | Full-queue behavior | Ownership |
|---|---|---|---|---|---|---|
| RPC outbound lines | `crates/mare-cli/src/rpc/serve.rs` (`out_tx` / `out_slots`) | Host RPC handlers & tap forwarder | RPC stdout/socket writer | `OUT_QUEUE` (4096) | `coalesce` / disconnect (`output_lost` + cancel primary run) | RPC `Conn` |
| RPC tap events | `crates/mare-cli/src/rpc/serve.rs` (`tap_tx`) | `Kernel` event dispatch | RPC event forwarder | `TAP_CAPACITY` (4096) | `coalesce` (`Lagged` marker in-place) | RPC `Conn` |
| Kernel taps (headless, peer, ACP, TUI, agent) | `crates/mare-kernel/src/dispatch/tap.rs` | `Kernel` event dispatch | Frontend / agent event loop | `TAP_CAPACITY` (4096) | `coalesce` (`Lagged` marker in-place for best-effort; reliable delivery for required events) | `Kernel` / caller |
| HTTP request/response body stream | `crates/mare-binding/src/http_serve_req.rs` | `mare.http.serve` handler | Hyper response body | `STREAM_QUEUE_CAPACITY` (16) | `block` (async backpressure, cancellable) | HTTP `ServeBody` |
| HTTP WebSocket inbound/outbound | `crates/mare-binding/src/http_serve_ws.rs` | WS peer / Lua handler | Lua handler / WS sink | `WS_QUEUE_CAPACITY` (64) | Outbound: `block` (async send); Inbound: `reject` (send Close 1008 if writable, log & terminate otherwise) | `ServerWs` |
| MCP server events | `crates/mare-binding/src/mcp/conn.rs` (`Inner::tx`) | `rmcp` client handler / reconnect | Lua `client:next_event()` | `EVENT_CAPACITY` (256) | `coalesce` (`Reconnected`) + `drop_newest` (`try_send` for debounced `list_changed`, `cancel` on `close()`/elicitation) | `mcp::conn::Inner` |
| MCP progress tokens | `crates/mare-binding/src/mcp/conn.rs` (`progress`) | `rmcp` progress notifications | Progress listener | `PROGRESS_CAPACITY` (64) | `coalesce` (broadcast lag) | `mcp::conn::Inner` |
| ACP inbound requests | `crates/mare-cli/src/acp/mod.rs` (`serve` -> main loop) | `mare-acp` thread | Host `LocalSet` loop | 256 | `block` (async send backpressure on ACP connection) | `acp::run` |
| ACP session updates | `crates/mare-cli/src/acp/mod.rs` (`updates`) | Prompt `spawn_local` task | ACP notification forwarder | `TAP_CAPACITY` (4096) | `block` (async send backpressure) | ACP prompt task |
| Network approval asks | `crates/mare-host/src/approvals/net.rs` (`NetApprover`) | `netproxy` sandboxed connections | Host `install_net_approver` task | 256 | `block` (async send, fail-closed on close) | `Host` |
| PTY input & output | `crates/mare-binding/src/pty.rs`, `pty/output.rs` | Lua `pty:write` / PTY reader thread | PTY writer / Lua `pty:read` | `QUEUE_CHUNKS` | `block` (bounded chunk queue) | `Pty` |
| RPC provider stream | `crates/mare-cli/src/rpc/providers.rs` | External provider RPC response | `stream` consumer | 1024 | `block` (async send) | Provider call |
| RPC host tool partials | `crates/mare-cli/src/rpc/host_tools.rs` | Host tool execution | RPC tool partial forwarder | 1024 | `block` (async send) | Tool call |
| Headless output writer | `crates/mare-cli/src/headless_output.rs` | `headless::run` | Dedicated stdout writer thread | 1 | `block` (synchronous backpressure) | `Output` |
| Subagent steer queue | `crates/mare-host/src/agent/mod.rs`, `agent_registry.rs` | `subagent_steer` tool / `mare.subagent.steer` / RPC `subagent.steer` | Child agent `steer_task` | `STEER_QUEUE_CAPACITY` (64) | `reject` (`try_send` error when full, `false` when closed) | `AgentRegistry` / `ChildRuntime` |

## 2. Internal, UI, & Lifecycle Channels (Registered Unbounded)

These channels are not driven by unbounded external input or model output; each has an
explicit structural upper bound and/or must remain non-yielding for `__close` / UI invariants.

| Class | Location | Producer | Consumer | Capacity | Full-queue behavior | Ownership & Upper-bound justification |
|---|---|---|---|---|---|---|
| `mare.task.channel()` (default) | `crates/mare-binding/src/task.rs` | Lua coroutines & `__close` finalizers | Lua coroutines | Unbounded (optional `capacity` for bounded) | N/A (sync `send`) | Owned by Lua caller. Used for 1-slot mutexes (`agent/inputs.lua:50`), per-request oneshots (`session/store.lua:494`), and `__close` cleanup (`session/store.lua:504`) which must never yield or drop. Bounded `mare.task.channel(capacity)` is available for yielding producers. |
| Child agent approvals | `crates/mare-host/src/approvals/child.rs` (`atx`) | Child agent hosts | Parent `answer_child_approval` | Unbounded | N/A | Bounded by `agent_tree::Scope::max_active` (default 16): each child blocks on a single oneshot approval at a time. |
| TUI / Plain UI commands | `crates/mare-cli/src/app/event_loop.rs`, `plain.rs` | `UiShared` (`Host` / `mare.ui`) | TUI / Plain event loop | Unbounded | N/A | Bounded by active modal dialogs (1 at a time via `UiWaits`) and human-rate UI updates; drained on every event loop tick. |
| TUI `AppMsg` | `crates/mare-cli/src/app/event_loop.rs` | Background TUI workers (image decode, etc.) | TUI event loop | Unbounded | N/A | Bounded by active terminal frame / viewport work; drained on every event loop tick. |
| Config reload trigger | `crates/mare-host/src/reload.rs` | File watcher | Host reload task | Unbounded | N/A | Coalesced by watcher debounce; drained by reload loop. |
| Package install progress | `crates/mare-host/src/startup/lifecycle.rs`, `mare-plugin/src/pkg_binding.rs` | `mare_plugin_manager` installer | Startup UI / `/plugins` | Unbounded | N/A | Bounded by declared packages in `mare.o.plugins` (one line per git step). |
| Terminal size / resize | `crates/mare-binding/src/ui.rs` | Terminal resize listener | UI size watcher | Unbounded | N/A | Human/window-manager resize events; drained immediately. |
| ACP session close completion | `crates/mare-cli/src/acp/mod.rs` (`close_done_tx`) | `CloseSession` tasks | ACP main loop | Unbounded | N/A | Bounded by number of active ACP sessions closing concurrently (1 message per closed session). |

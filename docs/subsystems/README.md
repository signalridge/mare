# Subsystem design references

The table maps each subsystem to its responsibilities and implementation.

| Subsystem | Responsibility & Boundary | Source of Truth |
|---|---|---|
| Plugin kernel | Cordis-style plugin lifecycle, topological dependency resolution, `mare.Service` contracts, event dispatch (`emit`, `bail`, `waterfall`, `policy`), Lua module ownership, bytecode cache, and watchdog | [`crates/mare-kernel`](../../crates/mare-kernel/src/lib.rs) |
| Host runtime | Combines one Lua VM, kernel, primitives, and loaded plugins; owns workspace trust, config reloads, subagent registry (`AgentRegistry`), multi-host coordination (`HostManager`), approvals, and HTTP/WS web routes | [`crates/mare-host`](../../crates/mare-host/src/lib.rs) |
| Lua primitives & bindings | Rust binding modules (`fs`, `proc`, `job`, `http`, `http_serve`, `pty`, `ossandbox`, `netproxy`, `mcp`, `peers`, `spill`, `snapshot`, `clipboard`, `image`, `exec_ctx`) exposed as public Lua paths such as `mare.fs`, `mare.proc`, `mare.proc.start`, `mare.proc.sandbox`, `mare.http`, `mare.http.serve`, `mare.pty`, `mare.mcp`, `mare.peers`, `mare.spill`, `mare.snapshot`, `mare.clipboard`, `mare.image`, and `mare.exec` | [`crates/mare-binding`](../../crates/mare-binding/src/lib.rs) |
| Session store | Append-only JSONL session log with `fsync` batches, torn-write trimming, strict parse checks, single-writer `flock` locks, reverse scanner, search, and rebuildable SQLite metadata index | [`crates/mare-store`](../../crates/mare-store/src/lib.rs) |
| RPC protocol | Wire types for mare RPC protocol 3.0 (JSON-RPC 2.0 envelope: `initialize`, `session.*`, `agent.*`, `services.*`, `spill.*`, `peer.*`), `protocol_events!` registry, headless `--json` stream shapes, and JSON Schema catalog (`mare rpc schema`) | [`crates/mare-protocol`](../../crates/mare-protocol/src/lib.rs) |
| Config & trust | Evaluates `~/.config/mare/init.lua` and trusted `.mare/init.lua` in an isolated Lua VM, resolves six config layers (`default` through `--set`), tracks option sources (`config_api.lua`), and persists `state.json` and `trust.json` | [`crates/mare-config`](../../crates/mare-config/src/lib.rs) |
| Main-screen TUI | Renders finished turns to native scrollback and diff-renders the live bottom area (streaming text, input editor, key normalization, syntax highlighting, Markdown, Kitty/iTerm2/sixel images) | [`crates/mare-tui`](../../crates/mare-tui/src/lib.rs) |
| File editing engine | Fuzzy line-seek `apply_patch` parser/applier and exact/fuzzy `replace` matcher shared across editing tools | [`crates/mare-edit`](../../crates/mare-edit/src/lib.rs) |
| Package manager | Git package resolution (`mare.plugins.add`), semver tags, parallel fetch, content hashes, `mare-lock.json`, `plugins.json` v2 catalog state, and `mare-plugin.toml` manifests | [`crates/mare-plugin-manager`](../../crates/mare-plugin-manager/src/lib.rs) |
| Plugin runtime & built-in Lua preset | Rust runtime assembly (`install()` creating the global `mare` table, installing bindings, freezing stdlibs, and loading presets) plus all built-in Lua plugins (`agent`, `llm`, `tools`, `session`, `compact`, `context`, `permissions`, `approvals`, `mcp`, `subagent`, `web`, `webserver`, `ui`, `statusline`, providers), UI components, and `types/` LuaLS contracts | [`crates/mare-plugin/src/lib.rs`](../../crates/mare-plugin/src/lib.rs) · [`crates/mare-plugin/preset`](../../crates/mare-plugin/preset) |
| CLI & frontends | Binary entrypoint (`bin/mare.rs`) calling the `mare_cli` library (`src/main.rs`), clap parser, interactive TUI loop, plain `TERM=dumb` mode, `-p` / `--json` headless runner, `--rpc` stdio server, `mare acp` server, `doctor`, `login`, `peer`, `sandbox`, and `test` runner | [`crates/mare-cli/src/bin/mare.rs`](../../crates/mare-cli/src/bin/mare.rs) · [`crates/mare-cli/src/main.rs`](../../crates/mare-cli/src/main.rs) |
| Shared primitives | Cross-crate paths, artifact registry, error vocabulary (`CoreErrorCode`), redaction, wire projection, UI node trees, style specs, and execution-context boundaries | [`crates/mare-shared`](../../crates/mare-shared/src/lib.rs) |
| Preset embed macros | Compile-time embedding (`include_str!` / bytecode) for release builds and live disk reads for debug builds | [`crates/mare-macro`](../../crates/mare-macro/src/lib.rs) |

## Cross-cutting technical notes

- [RPC guide](../en/integrate/rpc.md) and [RPC reference](../en/reference/rpc.md): wire limits, peer socket least-privilege rules, service contracts, and spill output behavior.
- [Channel and backpressure registry](../channels.md): bounded channels, capacity limits, and full-queue behavior.
- [Output-backed image resources](../image-resources.md): artifact image locators, decoder budgets, and viewport materialization.
- [Unix sandbox acceptance matrix](../unix-sandbox-matrix.md): verification matrix for Seatbelt, bubblewrap, and Landlock.
- [Host kernel](../en/extend/host-kernel.md) and [Host loader](../en/extend/host-loader.md): Lua-side cordis boot fragment, `task`, `kernel`, and `loader` plugins.

## Maintain a reference

Update the owning reference in the same commit when a change alters a documented responsibility, interface, state transition, runtime path, compatibility rule, or failure behavior. Internal refactors that preserve those contracts do not require prose updates.

Keep each fact in one place and link to source files and named symbols instead of copying volatile implementation details. Document current behavior separately from proposed changes; proposals belong in issues.

When reviewing a subsystem change:

1. Identify the affected crate or built-in plugin and trace the actual runtime path.
2. Compare claimed before/after behavior against callers, state transitions, and contracts.
3. Verify that updated contracts and source links resolve and that cited tests or checks actually ran.

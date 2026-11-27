---@meta _
-- LuaLS definitions: processes, jobs, the OS sandbox, the tool execution context and pseudo-terminals (entry: types/mare.lua)

---@class mare.JobStatus
---@field pid integer|nil
---@field state "running"|"exited"
---@field code integer|nil
---@field signal integer|nil
---@field success boolean|nil
---@field elapsed_ms integer
---@field bytes integer total output bytes produced
---@field unread integer bytes after the read cursor
---@field timed_out boolean|nil (wait only)
---@field cancelled boolean|nil (wait only)
---@field stop_unconfirmed? boolean (wait with timeout/cancel only)
---@field background_left? boolean (wait only: shell exited while descendants hold output pipe)

--- A process started with mare.proc.start: output lives in a bounded ring buffer in Rust.
---@class mare.ShellConfig
---@field path string
---@field args string[] arguments before the command
---@field transport "argv"|"stdin" stdin: pass the command as `input` (legacy WSL launcher)
---@field kind "bash"|"zsh"|"sh"|"wsl"|"powershell"
---@field msys_blocked? boolean ToolsService:shell() only: PowerShell because Git Bash cannot start in this sandbox

--- Resolve the shell for commands. Unix: `shell_path`, $SHELL (bash/zsh), /bin/bash, bash, sh.
--- Windows (pi's order): `shell_path` → %ProgramFiles%\Git\bin\bash.exe →
--- %ProgramFiles(x86)%\Git\bin\bash.exe → bash.exe on PATH; the error lists every location checked.
--- `model = true`: the default for model commands under the current sandbox — PowerShell inside
--- the Windows default-tier sandbox (Git Bash cannot start there) unless `shell_path` is set.
---@param opts? {shell_path?: string, model?: boolean}
---@return mare.ShellConfig|nil, string? err
function mare.proc.shell(opts) end
--- PowerShell 7 (pwsh) or Windows PowerShell with `-NoProfile -NonInteractive -ExecutionPolicy
--- Bypass -Command`; Windows only.
---@return mare.ShellConfig|nil, string? err
function mare.proc.powershell() end
--- `C:\x\y` → `/mnt/c/x/y` (default WSL mount root only); nil for other paths.
---@param p string
---@return string|nil
function mare.proc.wsl_path(p) end
--- `/mnt/c/x/y` → `C:\x\y`; nil outside `/mnt/<drive>`.
---@param p string
---@return string|nil
function mare.proc.windows_path(p) end

---@class mare.Job
local Job = {}
---@return integer|nil
function Job:pid() end
---@return mare.SandboxReport|nil
function Job:sandbox() end
--- Structured denial for this job given its output (nil when the sandbox did not refuse).
---@param output? string
---@return mare.SandboxDenial|nil
function Job:sandbox_denied(output) end
---@return mare.JobStatus
function Job:status() end
--- New output since the last read (moves the cursor). `dropped`: bytes pushed out of the ring
--- before they were read; `done`: the process exited and everything was read.
---@param opts? {max_bytes?: integer, stream?: "stdout"|"stderr"}
---@return {text: string, dropped: integer, offset: integer, done: boolean}
function Job:read(opts) end
--- Output from an absolute offset; does not move the cursor.
---@param offset integer
---@param opts? {max_bytes?: integer, stream?: "stdout"|"stderr"}
---@return {text: string, next: integer, dropped: integer}
function Job:read_at(offset, opts) end
---@param n? integer bytes (default 4096)
---@return string
function Job:tail(n) end
---@async
---@param opts? {timeout_ms?: number, cancel?: mare.Token}
---@return mare.JobStatus
function Job:wait(opts) end
--- Signal the whole process group.
---@param sig? "TERM"|"KILL"|"INT"|"HUP"
function Job:kill(sig) end
--- Stop the whole process group: SIGTERM, `grace_ms` (default 3000), then SIGKILL.
--- `interrupt`: a user's interrupt, SIGINT and 1 s first.
---@async
---@param opts? {grace_ms?: number, interrupt?: boolean}
---@return {stopped: boolean, stop_unconfirmed: boolean}
function Job:stop(opts) end
---@async
---@param data string
---@param opts? {close_stdin?: boolean}
function Job:write(data, opts) end
---@async
function Job:close_stdin() end

---@class mare.Process
local Process = {}
---@return integer|nil
function Process:pid() end
---@return mare.SandboxReport|nil
function Process:sandbox() end
--- Structured denial for this process given its output (nil when the sandbox did not refuse).
---@param output? string
---@return mare.SandboxDenial|nil
function Process:sandbox_denied(output) end
---@async
---@param data string
function Process:write(data) end
---@async
function Process:close_stdin() end
---@async
---@return string|nil
function Process:read_line() end
---@async
---@return string|nil
function Process:read() end
---@async
---@return string|nil
function Process:read_stderr_line() end
---@async
---@return {code: integer|nil, signal: integer|nil, success: boolean}
function Process:wait() end
---@param sig? "TERM"|"KILL"|"INT"|"HUP"
function Process:kill(sig) end

--- OS sandbox for a spawned process. Levels on one ladder:
--- read-only → workspace-write → danger-full-access (`full-access` is an alias; it and `false`
--- mean unsandboxed). `"current"` = the session's current policy. read-only: no writes, network
--- blocked unless set; workspace-write: cwd + `writable` + temp dirs, network open unless set.
--- `writable` dirs must be existing absolute physical directories (no symlinks) — re-checked at
--- every spawn (`SANDBOX_INVALID_ROOT`). A table `network` filters by domain through a local proxy
--- (`*.example.com` = subdomains, `**.example.com` = apex too; bare `*` is rejected; local and
--- private addresses need `allow_local`). Backends: Seatbelt (macOS), bubblewrap then Landlock
--- (Linux); none usable → error `SANDBOX_UNAVAILABLE`, never an unconfined run.
---@alias mare.SandboxLevel "read-only"|"workspace-write"|"danger-full-access"|"full-access"
---@class mare.SandboxNetwork
---@field allow? string[]
---@field deny? string[]
---@field default? "deny"|"allow"
---@field allow_local? boolean
---@class mare.SandboxPolicy
---@field level? mare.SandboxLevel default "workspace-write"
---@field cwd? string workspace root (default: the call's cwd)
---@field exclude_tmpdir_env_var? boolean workspace-write: omit default environment/platform temp roots (default false)
---@field exclude_slash_tmp? boolean workspace-write: omit default Unix /tmp root (default false)
---@field writable? string[] extra writable dirs (absolute, `~` expanded)
---@field write? string[] replace the workspace root with these writable dirs
---@field roots? string[] (normalized output) every effective writable root
---@field readable? string[] extra readable dirs (matters with restrict_read)
---@field deny_read? string[] literal paths excluded from reads; unresolved entries require explicit partial acceptance and are not masked
---@field restrict_read? boolean only system roots + writable + readable are readable
---@field reads_pinned? boolean keep readable roots fixed instead of adding writable roots
---@field network? boolean|mare.SandboxNetwork
---@field profile? string raw seatbelt profile (macOS)
---@field unix_sockets? string[] Unix sockets reachable while the network is restricted (policy layer and kernel use this one list; the askpass socket is always included)
---@field accept_partial? mare.SandboxCap[] dimensions accepted as unenforced when the backend lacks them (else the call is refused with SANDBOX_INSUFFICIENT); "network" = net_block + net_proxy + unix_socket
---@field unavailable? mare.SandboxCap[] (normalized output) required capabilities unavailable on this host
---@field marker? string (normalized output) the denial marker for this level
---@alias mare.SandboxSpec boolean|mare.SandboxLevel|"current"|mare.SandboxPolicy
--- A sandbox capability dimension. A policy requires fs_write (always),
--- fs_protect (protected paths under writable workspace roots), fs_read (restrict_read), fs_deny_read (deny_read paths), net_block (network = false) or net_proxy (domain filtering) plus
--- unix_socket (any restricted network); a backend lacking one refuses the call
--- (`SANDBOX_INSUFFICIENT: …`) unless it is in `accept_partial`.
---@alias mare.SandboxCap "fs_read"|"fs_deny_read"|"fs_write"|"fs_protect"|"fs_host_protect"|"net_block"|"net_proxy"|"unix_socket"|"network"|"read"|"write"
--- What a sandbox actually enforces here: every required dimension is in `enforced` or
--- `unavailable`; `enforcement` is "partial" only when something required is unavailable, which
--- happens only for dimensions the policy listed in `accept_partial`.
---@class mare.SandboxReport
---@field level mare.SandboxLevel
---@field mechanism "seatbelt"|"bwrap"|"landlock"|"windows-acl"|"windows-user"|"none"
---@field reads? string Windows default tier: "not controlled"; enhanced tier: deny-read ACLs
---@field enhanced? string Windows default tier: why the enhanced tier (`mare sandbox setup`) is not in use — not set up, or which self-test check failed
---@field network_control? string Windows default tier with a non-open network: not enforced (no WFP filters)
---@field enforcement "full"|"partial"
---@field network "open"|"blocked"|"filtered"
---@field proxy? string
---@field enforced mare.SandboxCap[]
---@field unavailable mare.SandboxCap[]
---@field accepted_partial? mare.SandboxCap[] the unavailable dimensions the user accepted
---@field note? string backend caveat (e.g. an older Landlock ABI)
---@field temp? string Windows workspace-write with a writable root containing %TEMP%: "temp directory not separately isolated …"
--- Only set when the sandbox itself refused (never for ordinary failures).
---@class mare.SandboxDenial
---@field sandbox_denied "write"|"read"|"network"|"unavailable"|"insufficient"
---@field path? string
---@field host? string
---@field marker? string e.g. "[sandbox: file access denied under workspace-write mode]"

---@class mare.proc
mare.proc = {}
--- Whether a process with `pid` currently exists (and matches `start_time` if both are known).
---@param pid integer
---@param start_time? integer
---@return boolean
function mare.proc.alive(pid, start_time) end
--- Start time token of `pid` (defaults to the current process), or nil when unavailable.
---@param pid? integer
---@return integer?
function mare.proc.start_time(pid) end
--- Write or append to a spill file the caller keeps: `<state>/spill/<prefix>-*.log`, the caller
--- naming its own prefix (`[a-z0-9-]+`, e.g. "bash"). Any other path or prefix is refused.
---@param prefix string
---@param path string
---@param data string
---@param append? boolean
function mare.proc.spill_write(prefix, path, data, append) end
--- Remove a spill file the caller keeps (`<state>/spill/<prefix>-*.log`).
---@param prefix string
---@param path string
function mare.proc.spill_remove(prefix, path) end
--- Scrubbed parent environment inherited by spawned children.
---@param opts? {env_include?: string[], env_exclude?: string[]}
---@return table<string, string>
function mare.proc.env(opts) end
---@param opts {cmd: string, args?: string[], cwd?: string, env?: table<string,string>, clear_env?: boolean, env_include?: string[], env_exclude?: string[], stdin?: boolean, stderr?: "pipe"|"merge", sandbox?: mare.SandboxSpec}
---@return mare.Process
function mare.proc.spawn(opts) end
---@async
---@param opts {cmd: string, args?: string[], cwd?: string, env?: table<string,string>, clear_env?: boolean, env_include?: string[], env_exclude?: string[], input?: string, timeout_ms?: number, kill_grace_ms?: number, cancel?: mare.Token, on_output?: fun(chunk: string), max_output?: integer, stderr?: "pipe"|"merge", sandbox?: mare.SandboxSpec, spill?: string, spill_prefix?: string} `spill`: a file that receives the complete output; with `spill_prefix` it must be the caller's spill file `<state>/spill/<prefix>-*.log` (exempt from writable roots, never from deny_read), without it any path the write policy allows
--- `sandbox_failed`/`sandbox_marker` "[sandbox: failed to start (...)]" = the sandbox itself could not
--- start (not a denial); `sandbox_denied`/`denial`/`sandbox_marker` = the sandbox refused the command.
---@return {code: integer|nil, signal: integer|nil, success: boolean, stdout: string, stderr: string, timed_out: boolean, cancelled: boolean, stop_unconfirmed?: boolean, background_left?: boolean, background_job?: mare.Job, dropped: integer, sandbox?: mare.SandboxReport, sandbox_denied?: "write"|"read"|"network", denial?: mare.SandboxDenial, sandbox_failed?: string, sandbox_marker?: string}
function mare.proc.run(opts) end
---@class mare.ProcManager
local ProcManager = {}
---@param opts {process_id: string, command: string[], cwd?: string, env?: table<string, string>, tty?: boolean, size?: {rows: integer, cols: integer}, on_output?: fun(ev: {process_id: string, stream: "stdout"|"stderr"|"pty", data_base64: string}), on_exit?: fun(ev: {process_id: string, exit_code?: integer, output_drain_timed_out?: boolean})}
---@return {process_id: string}|nil, string?, string?
function ProcManager:spawn(opts) end
---@async
---@param opts {process_id: string, data_base64?: string, close_stdin?: boolean}
---@return boolean|nil, string?, string?
function ProcManager:write(opts) end
---@param opts {process_id: string, size: {rows: integer, cols: integer}}
---@return boolean|nil, string?, string?
function ProcManager:resize(opts) end
---@param opts {process_id: string}
---@return boolean|nil, string?, string?
function ProcManager:kill(opts) end
---@async
function ProcManager:close_all() end
---@return {active: integer, lingering: integer}
function ProcManager:counts() end
--- Create an owner-scoped process manager backing `proc.*`.
---@return mare.ProcManager
function mare.proc.manager() end
---@class mare.proc.sandbox
mare.proc.sandbox = {}
--- Same rules as a sandboxed spawn. Inside a tool call core already enforces the current policy
--- on `mare.fs`/`proc`/`http`/`net`; this is for asking ahead.
---@param op "write"|"read"|"network"|"exec"
---@param target string path, host (or URL) for "network"
---@param policy? mare.SandboxSpec nil = the current policy
---@param cwd? string
---@return boolean allowed, string|nil reason, mare.SandboxDenial|nil denial
function mare.proc.sandbox.check(op, target, policy, cwd) end
--- Normalized policy (resolved roots), nil when the spec means unsandboxed.
---@param spec mare.SandboxSpec
---@param cwd? string
---@return mare.SandboxPolicy|nil
function mare.proc.sandbox.policy(spec, cwd) end
--- The session's current policy (enforced inside tool calls), nil = unconfined.
---@return mare.SandboxPolicy|nil
function mare.proc.sandbox.current() end

---@class mare.SandboxSnapshot
--- Opaque native policy value. Keep it in memory; it is not a Lua policy table and cannot
--- be serialized. Snapshots retain validated policy identity across replaceable Lua setters.

--- Capture the current session policy after enforcing the inherited ceiling, revalidating
--- roots and omitting only roots that have disappeared. Refused inside a tool call.
---@return mare.SandboxPolicy|false policy false when unconfined
---@return mare.SandboxSnapshot|false snapshot false when unconfined
---@return string[] omitted missing roots
function mare.proc.sandbox.snapshot_current() end

--- Normalize and capture a policy against the host workspace and inherited ceiling, without
--- applying it. Missing roots are reported separately; other validation errors are refused.
--- Refused inside a tool call.
---@param spec mare.SandboxSpec|nil
---@return mare.SandboxPolicy|false policy
---@return mare.SandboxSnapshot|false snapshot
---@return string[] omitted missing roots
function mare.proc.sandbox.snapshot_policy(spec) end

--- Restore a captured native snapshot through the host setter. Revalidates the policy and
--- inherited ceiling, reporting only genuinely missing roots; bypasses replaceable Lua
--- setters. Also accepts a normal policy spec with the same normalization as set_current.
--- Refused inside a tool call. The returned snapshot captures the applied policy.
---@param snapshot mare.SandboxSnapshot|mare.SandboxSpec|nil
---@return mare.SandboxPolicy|nil policy
---@return string[] omitted missing roots
---@return mare.SandboxSnapshot|false applied snapshot
function mare.proc.sandbox.restore_snapshot(snapshot) end

--- True only for a genuinely absent root. Permission errors and invalid roots are false;
--- callers must not silently omit them. Refused inside a tool call.
---@param path string
---@return boolean
function mare.proc.sandbox.root_missing(path) end

--- Check whether per-workspace protected mounts can be prepared; may briefly create an owned empty mount point.
---@param spec? mare.SandboxSpec
---@param cwd? string
---@return boolean ok, string? reason
function mare.proc.sandbox.preflight_protection(spec, cwd) end
--- Changes whenever the current policy changes (approvals are bound to it).
---@return integer
function mare.proc.sandbox.generation() end
--- Validate a policy spec against the active ceiling and persistent PTY fence without mutating
--- the current session policy. Refused inside a tool call.
---@param spec mare.SandboxSpec|nil
---@param cwd? string
---@return boolean
function mare.proc.sandbox.check_set(spec, cwd) end
--- Replace the current policy (takes effect from the next call); errors on an invalid spec.
--- Prefer `ctx.session:set_sandbox_mode(spec)`, which also logs it in the session.
---@param spec mare.SandboxSpec|nil
---@param cwd? string
---@return mare.SandboxPolicy|nil
function mare.proc.sandbox.set_current(spec, cwd) end
--- Per-dimension intersection of two policies (nil = the current policy): each dimension keeps
--- only what both allow. An approval answer narrows the requested policy with this.
---@param a mare.SandboxSpec|nil
---@param b mare.SandboxSpec|nil
---@param cwd? string
---@return mare.SandboxPolicy|nil
function mare.proc.sandbox.intersect(a, b, cwd) end
--- Whether `path` (resolved; a dangling symlink counts as outside) lies under one of `roots`.
---@param path string
---@param roots string[]
---@param opts? {stable_roots?: boolean} reject stored roots redirected by a symlink/junction
---@return boolean
function mare.proc.sandbox.within(path, roots, opts) end
--- "[sandbox: file access denied under <level> mode]"
---@param level mare.SandboxLevel
---@return string
function mare.proc.sandbox.marker(level) end
--- Set when a sandboxed command is an MSYS2/Cygwin program (Git Bash) and died with
--- 0xC0000142 under a Windows mechanism: "SANDBOX_MSYS_UNSUPPORTED: …"; nil otherwise.
---@param o {mechanism?: string, code?: integer, program?: string, output?: string}
---@return string|nil
function mare.proc.sandbox.msys_failure(o) end
---@type mare.SandboxLevel[]
mare.proc.sandbox.levels = {}
---@param cwd? string workspace to check for the %TEMP% note
---@return {platform: string, mechanism: "seatbelt"|"bwrap"|"landlock"|"windows-acl"|"windows-user"|"none", enforcement: "full"|"partial"|"none", levels: mare.SandboxLevel[], caps: mare.SandboxCap[], missing: mare.SandboxCap[], summary: string, network_scope: string, network_filter: boolean, network_block: boolean, restrict_read: boolean, escalation_reads: string, temp?: string} capability dimensions are probed on this host (not inferred from the backend name); `escalation_reads` says whether an approved escalation can stay sandboxed with the session's denied reads kept here (else it is refused); `network_scope` says what domain rules guarantee (the connection target, not names inside HTTPS tunnels); with `cwd`, `temp` is set on Windows when that workspace contains %TEMP% ("temp directory not separately isolated")
function mare.proc.sandbox.capabilities(cwd) end
--- Start a long-running process whose output goes to a bounded ring buffer.
--- `spill` additionally receives the complete output.
---@param opts {cmd: string, args?: string[], cwd?: string, env?: table<string,string>, env_include?: string[], env_exclude?: string[], clear_env?: boolean, stdin?: boolean, input?: string, tty?: boolean, cols?: integer, rows?: integer, on_policy_change?: "fence"|"stop", sandbox?: mare.SandboxSpec, buffer_bytes?: integer, spill?: string, spill_prefix?: string} `input`: written to stdin, then stdin is closed (or written to the PTY when `tty = true`); `on_policy_change`: `"fence"` (default, blocks sandbox policy changes with `PTY_POLICY_BUSY` while alive) or `"stop"` (stops the job when sandbox policy changes)
---@return mare.Job
function mare.proc.start(opts) end
---@param url string
function mare.proc.open(url) end
--- The absolute path of an executable, or nil; starts no process (Neovim `exepath()`). A bare
--- name is looked up in PATH (on Windows with the PATHEXT extensions, so `rg` finds `rg.exe`); a
--- name containing a path separator is a path, relative to the working directory, and is
--- returned only when it is an executable file. Plugins use it to offer a tool only when it is
--- installed.
---@param name string a command name (`"rg"`) or a path (`"./bin/tool"`)
---@return string|nil path absolute path, or nil when not found or not executable
function mare.proc.which(name) end

--- coroutines it starts) is checked by core against the sandbox policy.
---@class mare.exec
mare.exec = {}
--- Run `fn(...)` tagged with `ctx` (an outer context is never replaced: a nested run belongs
--- to the outer call). While a session journal (`mare.store.open`) is open, a new call is
--- admitted only on its durable start — a journal write with `ack = ctx.tool_call_id`;
--- otherwise it raises `EXEC_NOT_DURABLE: <reason>` and `fn` never runs. Each ack admits once.
---@async
--- `approval_key` names the frozen call (tools service): in a sub-agent, a call the parent
--- approved is matched by it and escapes the inherited ceiling.
--- lifetime="ephemeral-read-only" skips journal admission only with an explicit confined
--- read-only sandbox blocking all network. Unknown or unsafe lifetimes fail before fn runs.
---@param ctx {lifetime?: "ephemeral-read-only", tool?: string, tool_call_id?: string, approval_key?: string, approval_snapshot?: string, projection_version?: integer, projection_authorization?: mare.ProjectionAuthorization, cwd?: string, sandbox?: any, unconfined?: boolean, cancel?: mare.Token}
---@param fn async fun(...): ...
function mare.exec.run(ctx, fn, ...) end
--- A detached, read-only view of the calling code's context (nil outside tool calls).
--- `denials` lists refusals so far; changing this view cannot change the active policy.
---@return {tool?: string, tool_call_id?: string, cwd?: string, sandbox?: any, unconfined?: boolean, cancel?: mare.Token, denials?: {sandbox_denied: string, path?: string, host?: string, op: string}[]}|nil
function mare.exec.current() end
--- Raise if `op` ("write"|"read"|"network") on `target` is refused for the calling code.
---@param op string
---@param target string
function mare.exec.check(op, target) end
--- Refuse a host-state mutation inside ephemeral-read-only execution. Called before changing
--- options or service state; filesystem/network/process operations also enforce their policies.
---@param operation string
function mare.exec.check_mutation(operation) end
--- Raise if `what` cannot be confined while a sandbox policy is active.
---@param what string
function mare.exec.refuse_unconfinable(what) end

---@class mare.Pty
local Pty = {}
--- Enqueue at most 16384 bytes. Input queue holds at most 32 chunks; a full/closed queue
--- raises PTY_INPUT_UNAVAILABLE without accepting this write. Success means queued, not delivered.
--- Every write checks the launch sandbox generation and effective caller policy.
---@param data string
function Pty:write(data) end
---@async
---@return string|nil chunk nil when the program exited
function Pty:read() end
--- Safe incremental UTF-8 text: removes terminal controls across chunk boundaries.
--- Use one read API consistently for a PTY; read and read_text share the same consuming cursor.
--- Raw output is backpressured in at most 32 chunks of 8192 bytes (plus an in-flight chunk).
---@async
---@return string|nil
function Pty:read_text() end
--- Verified kernel foreground group. Unknown readiness is nil, never inferred from silence.
--- Linux may positively identify read(0) on the terminal; other platforms may return nil.
---@return {pgid: integer, input_waiting?: boolean}|nil
function Pty:foreground() end
--- Requires spawn{emulate=true} and the current resource lease policy. Bounded VT viewport and OSC 133 command-boundary metadata from consumed output.
--- Text is display-safe; protocol replies are bounded and handled by the transport.
---@param opts? {max_bytes?: integer, screen?: boolean} screen=false omits viewport text for inexpensive state polling
---@return {screen?: string, view_truncated?: boolean, tail: string, alternate_screen: boolean, prompt_generation: integer, command_exit_code?: integer}
function Pty:view(opts) end
--- Read bounded VT history plus current viewport; offset counts back from newest lines.
---@param opts? {offset?: integer, count?: integer, max_bytes?: integer}
---@return {text: string, total_lines: integer, line_begin: integer, line_end: integer, truncated: boolean}
function Pty:page(opts) end
--- Signal a verified foreground group; rejects arbitrary signals and SIGKILL of the root/protected group.
--- Unsupported platforms raise an error rather than report delivery. No arbitrary PID accepted.
---@param signal "SIGINT"|"SIGTERM"|"SIGKILL"|"SIGTSTP"|"SIGHUP"
---@param opts? {protected_pgid?: integer} additional group protected against SIGKILL, checked at delivery
---@return integer pgid signalled
function Pty:signal(signal, opts) end
--- Stop the whole owned tree, wait for root and tree disappearance. Idempotent, bounded.
--- The policy-change fence remains held until the owned tree is confirmed gone.
---@async
---@param opts? {grace_ms?: integer}
---@return {stopped: boolean, stop_unconfirmed: boolean}
function Pty:stop(opts) end
---@param cols integer
---@param rows integer
function Pty:resize(cols, rows) end
function Pty:kill() end
--- Stop the terminal process tree cleanly (Ctrl+C -> SIGTERM -> grace_ms -> SIGKILL).
---@async
---@param opts? {grace_ms?: number}
---@return {stopped: boolean, stop_unconfirmed: boolean}
function Pty:stop(opts) end
---@return integer|nil exit code, nil while running
function Pty:try_wait() end
--- Actual process outcome; signal remains separate from an exit code.
---@return {code?: integer, signal?: integer}|nil
function Pty:exit_status() end
---@return integer|nil
function Pty:pid() end
--- What the sandbox actually enforces for this terminal (nil when unconfined).
---@return mare.SandboxReport|nil
function Pty:sandbox() end

---@class mare.pty
mare.pty = {}
--- Discover enforced transport budgets and optional platform foreground capabilities.
---@return {streaming_text: boolean, policy_fence: boolean, max_write_bytes: integer, queue_chunks: integer, output_chunk_bytes: integer, foreground: boolean, foreground_signals: boolean, terminal_view: boolean, max_view_cells: integer}
function mare.pty.capabilities() end
--- Run a program in a pseudo-terminal (interactive tools). Per-call policy elevations are refused
--- because their authority must not escape into later input calls. While a PTY tree is alive,
--- sandbox policy changes raise PTY_POLICY_BUSY; close/stop terminals before switching policy.
--- Inside a confined tool call (or with
--- `sandbox`) the program is confined like `mare.proc`: Seatbelt / bubblewrap wrap it inside the
--- PTY; Landlock (and bubblewrap's Unix-socket filter) apply in the terminal child before exec;
--- on Windows the sandbox runner owns the ConPTY and bridges it.
---@param opts {cmd: string, args?: string[], cwd?: string, env?: table<string,string>, env_include?: string[], env_exclude?: string[], cols?: integer, rows?: integer, buffer_bytes?: integer, scrollback_lines?: integer, emulate?: boolean, sandbox?: mare.SandboxSpec}
---@return mare.Pty
function mare.pty.spawn(opts) end

---@meta _
-- LuaLS definitions: HTTP, Unix sockets and peer sessions (entry: types/mare.lua)

---@class mare.Response
---@field status integer
---@field url string
---@field ok boolean
---@field headers table<string, string>
local Response = {}
---@async
---@return string|nil
function Response:read() end
---@async
---@return string
function Response:text() end
---@async
---@return any
function Response:json() end
---@async
---@return {event: string, data: string, id?: string}|nil
function Response:next_event() end
---@async
function Response:close() end

---@class mare.UnixConn
local UnixConn = {}
--- Write one line ("\n" appended if missing). Default timeout 5000ms.
---@async
---@param line string
---@param opts? {timeout_ms?: integer}
---@return true|nil ok, string? err
function UnixConn:write_line(line, opts) end
--- Read one bounded UTF-8 line (without newline and optional preceding CR).
--- Set timeout_ms=false to wait without a deadline (close or task cancellation still ends it).
--- A timeout or cancelled Lua wait preserves consumed partial bytes for the next read.
--- Errors include timeout, eof, incomplete frame (EOF before newline), invalid utf-8,
--- and line too long (terminates further reads). Reads and writes may run concurrently.
---@async
---@param opts? {timeout_ms?: integer|false}
---@return string|nil line, string? err
function UnixConn:read_line(opts) end
--- Wake pending I/O with closed, then release both halves; safe to call repeatedly.
---@async
function UnixConn:close() end

--- Generic line-oriented Unix socket client. Failures return `nil, err`, never raise.
---@class mare.net
mare.net = {}
---@class mare.net.unix
mare.net.unix = {}
---@async
---@param path string
---@param opts? {timeout_ms?: integer}
---@return mare.UnixConn|nil conn, string? err
function mare.net.unix.connect(path, opts) end
--- One connection, one line out, one line back; `timeout_ms` bounds the whole exchange.
---@async
---@param path string
---@param line string
---@param opts? {timeout_ms?: integer}
---@return string|nil response, string? err
function mare.net.unix.request(path, line, opts) end

---@class mare.UnixServer
local UnixServer = {}
---@return string
function UnixServer:path() end
--- Stop accepting and remove the socket file (also happens when the server is collected).
function UnixServer:close() end

--- Listen on a Unix socket: `handler(conn)` runs (async) once per accepted connection, so
--- several clients can be connected at once. A stale socket file is replaced; one that still
--- answers is refused ("already in use"). The file mode defaults to 0600.
---@param path string
---@param handler async fun(conn: mare.UnixConn)
---@param opts? {mode?: integer}
---@return mare.UnixServer|nil server, string? err
function mare.net.unix.listen(path, handler, opts) end

-- Peer sessions: sessions serving RPC on <state>/peers/<pid>.sock register here.
---@class mare.PeerEntry
---@field name string "<cwd basename>-<short id>" unless `peer = { name = ... }`
---@field pid integer
---@field cwd string
---@field socket string path of the session's peer socket (least-privilege RPC subset; see the RPC guide)
---@field mode string tui|print|json|rpc
---@field started_at_ms integer epoch ms
---@field status mare.Status|nil unified status, kept current
--- what that process's plugins published (`mare.peers.publish`), by namespace — e.g.
--- `plugins.session = {session_id, name?}`, `plugins.status = {held}`; absent in older entries and opaque
--- to core: validate before use
---@field plugins table<string, table>|nil
---@class mare.peers
mare.peers = {}
--- Put `fields` (JSON-serialisable, named) into this process's registry entry as
--- `plugins.<namespace>`, replacing what that namespace held; `nil` removes it. Other namespaces
--- and the core fields are never touched. `namespace`: lowercase letters, digits, `.`, `_`, `-` —
--- by convention the publishing plugin's name (one owner per namespace). Written with the next
--- registry rewrite (at most every 200 ms); kept, unwritten, while the session isn't registered.
---@param namespace string
---@param fields table|nil
function mare.peers.publish(namespace, fields) end
--- Registered sessions on this machine (this one included); dead ones are pruned.
---@return mare.PeerEntry[]
function mare.peers.list() end
--- This session's entry, or nil when it isn't registered (headless/--rpc without --peer,
--- or `peer = false`).
---@return mare.PeerEntry|nil
function mare.peers.self() end
--- The registry directory.
---@return string
function mare.peers.dir() end

---@class mare.http
mare.http = {}
---@async
--- Failures read `transport: …` (retryable), `malformed_response: …` (an unparseable stream,
--- including invalid UTF-8) or `aborted` (cancelled). `idle_timeout_ms`: no bytes at all for this
--- long fails the read (heartbeat comments count as activity); `header_timeout_ms`: wait for the
--- response headers; `first_event_timeout_ms`: wait for the first dispatched SSE data event
--- (heartbeat comments do not extend it); `fresh`: a new connection instead of a pooled one. Proxies come from
--- HTTP(S)_PROXY / ALL_PROXY / NO_PROXY (SOCKS and PAC are a clear error); custom CAs from
--- MARE_CA_CERTIFICATE, else SSL_CERT_FILE plus SSL_CERT_DIR (a directory of PEM / .crt files). `proxy`: this request's own HTTP(S) proxy (NO_PROXY
--- still applies); `connect_timeout_ms`: its connect timeout (default 30 s). `sse_max_line_bytes`
--- / `sse_max_event_bytes`: `next_event` fails with `malformed_response` once one SSE line, or one
--- event's lines together, exceed this many bytes (default 16 MiB each). `redirect`:
--- `"follow"` (default), `"manual"` (return 3xx without following), or `"same-origin"`.
--- `max_redirects`: maximum redirects allowed (default 10).
---@param opts {url: string, method?: string, headers?: table<string,string>, body?: string, json?: any, form?: table<string,string>, timeout_ms?: number, cancel?: mare.Token, idle_timeout_ms?: number, header_timeout_ms?: number, first_event_timeout_ms?: number, fresh?: boolean, proxy?: string, connect_timeout_ms?: number, sse_max_line_bytes?: number, sse_max_event_bytes?: number, redirect?: "follow"|"manual"|"same-origin", max_redirects?: integer}
---@return mare.Response
function mare.http.request(opts) end
--- `retry-after-ms`, or `Retry-After` in seconds or as an HTTP date, in milliseconds.
---@param headers table<string,string>
---@return integer|nil
function mare.http.retry_after(headers) end

---@async
--- `ports`: the first free one is used; `on_bound(port)` runs once listening (build the redirect URL).
---@param opts {port?: integer, ports?: integer[], on_bound?: fun(port: integer), path?: string, host?: string, timeout_ms?: number, cancel?: mare.Token}
---@return {path: string, query: table<string,string>, port: integer}|nil, string|nil err
function mare.http.listen_once(opts) end
---@param s string
---@return string
function mare.http.url_encode(s) end
---@param base string
---@param query? table<string,string>
---@return string
function mare.http.build_url(base, query) end

---@class mare.PeerClientError
---@field code string invalid_target, ambiguous_target, self, target_changed, stale, permission_denied, unavailable, protocol_error, capacity, cancelled, timeout, closed, invalid_message, remote_error, or outcome_unknown
---@field message string
---@class mare.PeerWaitOptions
---@field timeout_ms? integer|false Default 5000; false disables the response/wait deadline; maximum 3600000. Connect/write transport stays bounded to 5000 when false.
---@field cancel? mare.Token Cancels only this local wait; accepted messages cannot be retracted.
---@class mare.PeerMessageReceipt
---@field message_id string Peer message identity.
---@field status "held"|"handled"|"delivered"|"rejected"
---@field reason? string Receiving decision/admission failure, not permission evidence.
---@class mare.PeerClient
local PeerClient = {}
--- Cached self-reported identity and allowed RPC methods, not an authorization grant.
---@return table
function PeerClient:info() end
---@async
---@param opts? mare.PeerWaitOptions
---@return table|nil status, mare.PeerClientError? error
function PeerClient:status(opts) end
--- Requires receiver confirmation. held/handled are not agent completion. An outcome_unknown
--- result may already have been accepted: do not resend automatically.
---@async
---@param message {text: string, queue_only?: boolean}
---@param opts? mare.PeerWaitOptions
---@return {message_id: string, status: "held"|"handled"}|nil receipt, mare.PeerClientError? error
function PeerClient:send(message, opts) end
--- Non-consuming wait for this client's message receipt (delivered, rejected, or handled).
---@async
---@param message_id string
---@param opts? mare.PeerWaitOptions
---@return mare.PeerMessageReceipt|nil receipt, mare.PeerClientError? error
function PeerClient:wait_message(message_id, opts) end
--- Wait for the next transition; current_or_next also accepts a currently idle snapshot.
--- Requires named remote subscriptions. Cancellation/timeout confirms unsubscribe; a lost
--- cleanup acknowledgement closes this client. Late IDs cannot satisfy a subsequent wait.
--- A connection allows at most 128 lifetime subscription IDs (including completed waits).
--- exited is distinct from idle. This is not an agent mailbox wait_agent operation.
---@async
---@param opts? mare.PeerWaitOptions|{current_or_next?: boolean}
---@return table|nil state, mare.PeerClientError? error
function PeerClient:wait_idle(opts) end
--- Idempotently close and await the reader and pending-call cleanup.
---@async
function PeerClient:close() end
--- Open a registered peer. The caller owns the client; close it, e.g. from a ctx:effect cleanup.
--- Clients survive session switches; each message names the session current when it is sent
--- (`from.session_id`, self-reported; none for a message sent while the host is still starting).
--- Default peer privileges never allow arbitrary control. No arbitrary socket path is accepted.
---@async
---@param target string|{pid: integer, started_at_ms: integer} Exact name or stable process instance.
---@param opts? mare.PeerWaitOptions
---@return mare.PeerClient|nil client, mare.PeerClientError? error
function mare.peers.connect(target, opts) end

---@class mare.WebSocket
local WebSocket = {}
---@async
---@param text string
function WebSocket:send(text) end
---@async
---@param opts? {idle_ms?: number, cancel?: mare.Token}
---@return string|nil text, string|nil reason
function WebSocket:recv(opts) end
---@async
function WebSocket:close() end
---@async
---@return boolean
function WebSocket:is_closed() end

---@async
---@param opts {url: string, headers?: table<string,string>, timeout_ms?: number}
---@return mare.WebSocket
function mare.http.websocket(opts) end

---@class mare.ServerWs
local ServerWs = {}
---@async
---@param data string
---@param opts? {binary?: boolean}
function ServerWs:send(data, opts) end
---@async
---@param opts? {idle_ms?: integer, cancel?: mare.Token}
---@return string|nil msg, string|nil reason, boolean|nil is_binary
function ServerWs:recv(opts) end
---@async
---@param opts? {code?: integer, reason?: string}
function ServerWs:close(opts) end
---@return boolean
function ServerWs:is_closed() end

---@class mare.HttpRequest
---@field method string
---@field path string
---@field raw_query string|nil
---@field query table<string, string>
---@field headers table<string, string>
---@field cancel mare.Token
local HttpRequest = {}
---@async
---@return string|nil chunk, string|nil err
function HttpRequest:read() end
---@async
---@return string|nil body, string|nil err
function HttpRequest:body() end
---@async
---@return string|nil text, string|nil err
function HttpRequest:text() end
---@async
---@return any, string|nil err
function HttpRequest:json() end
---@async
---@return mare.ServerWs|nil ws, string|nil err
function HttpRequest:upgrade() end

---@class mare.SseEvent
---@field data? string
---@field event? string
---@field id? string
---@field retry? integer

---@class mare.HttpResponse
---@field status? integer
---@field headers? table<string, string>
---@field body? string | (async fun(): string|mare.SseEvent|nil)

---@class mare.HttpServer
local HttpServer = {}
---@return string
function HttpServer:addr() end
---@return integer
function HttpServer:port() end
--- Stop listening and cancel active connections and handlers.
---@async
function HttpServer:close() end

--- Start a loopback-only HTTP/1.1 + SSE + WebSocket server. Defaults to `127.0.0.1` and ephemeral port `0`.
--- Rejects non-loopback `host` binds, validates `Host` against loopback names/IPs (and optional `trusted_hosts`)
--- (DNS rebinding protection), and enforces `Sec-Fetch-Site` / `Origin` checks (rejects cross-site browser requests and WebSockets).
--- Token authentication is mandatory by default: when `token` is omitted, uses the persisted 32-byte random
--- runtime token (`<state>/host/token`, `0600`, rotated via `mare token rotate` for new requests/connections);
--- pass `token = false` to explicitly disable authentication, or pass a non-empty string for a custom static token.
--- `<state>/host/` is protected by the `fs_host_protect` sandbox capability (gated before creation/rotation unless
--- `accept_partial = { "fs_host_protect" }` is set). Not protected by `fs_host_protect`: `danger-full-access`,
--- disabled permissions plugin, approved unconfined escalations, custom static tokens, restricted processes launched
--- with a raw Seatbelt profile before token creation, and pre-existing or external-process sandboxes on partial backends.
--- Accepts `Authorization: Bearer <token>`, `x-mare-token`, or browser `GET ?token=<token>` (exchanged via
--- `303 See Other` for an in-memory `HttpOnly; SameSite=Strict` session cookie); WebSocket upgrades also accept `?token=`.
--- `header_read_timeout_ms` sets the HTTP header read timeout (default `30000`, at least 1) to mitigate slowloris.
--- `ws_max_message_bytes` (default `4194304`) and `ws_max_frame_bytes` (default `1048576`) cap WebSocket sizes.
--- `max_concurrency` and `max_connections` must be at least 1 and are clamped to Tokio's `Semaphore::MAX_PERMITS`.
--- `close_grace_ms` sets the WebSocket close grace period in milliseconds (default `5000`, non-negative) before aborting a non-draining handler.
--- `body_stall_ms` bounds how long a request body may stay unread while the client keeps sending, and how long a WebSocket may stay backpressured by a full, unconsumed inbound queue (default `60000`, non-negative); the connection is then cancelled and closed. Cancellation happens early when a disconnect is observable; otherwise the stall timeout is the backstop.
--- `cancel` (optional `mare.Token`) only cancels waiting for the startup `.token.lock` (within the 5000 ms lock acquisition timeout when using the default runtime token); once `mare.http.serve` has started, cancelling it does not stop the server (call `HttpServer:close()` to shut down).
---@async
---@param opts {host?: string, port?: integer, token?: string|false, allowed_origins?: string[], trusted_hosts?: string[], accept_partial?: mare.SandboxCap[], header_read_timeout_ms?: integer, ws_max_message_bytes?: integer, ws_max_frame_bytes?: integer, max_body_bytes?: integer, max_concurrency?: integer, max_connections?: integer, close_grace_ms?: integer, body_stall_ms?: integer, cancel?: mare.Token, handler: async fun(req: mare.HttpRequest): mare.HttpResponse|nil}
---@return mare.HttpServer|nil server, string|nil err
function mare.http.serve(opts) end

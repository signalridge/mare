---@meta _
-- LuaLS definitions: regexes, JSON, crypto and text helpers (entry: types/mare.lua)

---@class mare.Regex
local Regex = {}
---@param s string
---@return boolean
function Regex:is_match(s) end
---@param s string
---@param init? integer
---@return integer|nil, integer|nil
function Regex:find(s, init) end
---@param s string
---@return table|nil
function Regex:captures(s) end
---@param s string
---@param rep string
---@param all? boolean
---@return string
function Regex:replace(s, rep, all) end
---@param s string
---@return string[]
function Regex:split(s) end

---@class mare.json
mare.json = {}
---@param v any
---@param opts? {pretty?: boolean}
---@return string
function mare.json.encode(v, opts) end
---@param s string
---@return any|nil value, string|nil err
function mare.json.decode(s) end
--- Decode JSON and reject duplicate object members at every depth (after key unescaping).
---@param s string
---@return any|nil value, string|nil err
function mare.json.decode_unique(s) end
--- Preserve null and empty containers from the serde_json parsed value; reject further Lua conversion loss.
--- Raw number precision and duplicate keys may already be lost during parsing.
---@param s string
---@return any|nil, string|nil
function mare.json.decode_lossless(s) end
--- Encode a JSON value; reject functions, non-finite numbers, cycles, sparse arrays and unsupported metatables.
---@param v any
---@return string
function mare.json.encode_strict(v) end
--- Deep-clone a Lua value through strict JSON value representation, preserving `mare.json.null` and empty arrays.
---@generic T
---@param v T
---@return T
function mare.json.clone(v) end
--- Copy JSON schema property-ordering metadata recursively from `source` tables to matching `target` tables.
---@param source any
---@param target any
function mare.json.copy_metadata(source, target) end
---@param t? table
---@return table
function mare.json.array(t) end
--- Whether a table carries the JSON array marker (including an empty array).
---@param t table
---@return boolean
function mare.json.is_array(t) end
mare.json.null = nil

---@class mare.crypto
mare.crypto = {}
---@param s string
---@return string hex
function mare.crypto.sha256(s) end
---@param n integer
---@return string
function mare.crypto.random_bytes(n) end
---@param n integer bytes
---@return string
function mare.crypto.random_hex(n) end
---@return string
function mare.crypto.uuid() end
---@return {verifier: string, challenge: string}
function mare.crypto.pkce() end
function mare.crypto.base64_encode(s) end
function mare.crypto.base64_decode(s) end
function mare.crypto.base64url_encode(s) end
function mare.crypto.base64url_decode(s) end

---@async
---@async
---@async
---@async
---@class mare.text
mare.text = {}
--- Redact credential-looking strings and sensitive keys in tables recursively.
---@generic T
---@param value T
---@return T
function mare.text.redact(value) end
---@param pattern string
---@return mare.Regex|nil, string|nil
function mare.text.regex(pattern) end
---@param s string
---@param opts? {max_lines?: integer, max_bytes?: integer, keep?: "head"|"tail"|"middle", max_line_chars?: integer}
---@return string, {truncated: boolean, total_lines: integer, shown_lines: integer, omitted_lines: integer, head_lines: integer, tail_lines: integer}
function mare.text.truncate(s, opts) end
--- The first (`keep = "head"`, default) or last `n` units of `s`, never splitting a UTF-8
--- sequence. `unit = "bytes"` (default): at most `n` bytes, the cut moved inward to a character
--- boundary; `unit = "chars"`: `n` code points (raw bytes when `s` isn't valid UTF-8).
---@param s string
---@param n integer
---@param opts? {keep?: "head"|"tail", unit?: "bytes"|"chars"}
---@return string
function mare.text.cut(s, n, opts) end
---@class mare.TokenEstimateHint
---@field image_types? string[] Additional object `type` values whose `data` string holds an inline image payload.
---@field image_mime_keys? string[] Additional MIME key names (with `"image/..."` values) whose sibling `data` holds an inline image payload.
---@field capped_fields? table<string, integer> Field names whose string byte lengths are capped at a flat budget.

--- The one token estimate (compaction, /context, providers): a string is bytes / 4; a table is
--- its JSON size with each inline image counted as `image_tokens`, not its base64 payload, plus
--- any wire-declared estimation hints registered via `register_estimate_hint`.
---@param s string|table
---@return integer
function mare.text.tokens(s) end
--- Register namespaced (`<namespace>:<id>`) token-estimation hints for a wire format; returns an idempotent disposer function.
---@param name string
---@param hint mare.TokenEstimateHint
---@return fun()
function mare.text.register_estimate_hint(name, hint) end
--- Estimated tokens of one inline image (pi ESTIMATED_IMAGE_CHARS / 4).
---@type integer
mare.text.image_tokens = 1200
--- Parse the first document of a YAML 1.2 stream (yaml-rust2) into Lua values.
---@param s string
---@return any value, string? err
function mare.text.yaml(s) end
--- A leading `---` YAML block (SKILL.md, prompt templates). BOM and CRLF are normalised; a block
--- that isn't valid YAML is read as `key: value` lines and `warning` says why.
---@param text string
---@return table fields, string body, string? warning
function mare.text.frontmatter(text) end
---@param s string
---@return integer
function mare.text.width(s) end
---@param s string
---@param w integer
---@return string
function mare.text.trunc_width(s, w) end
---@param s string
---@return string
function mare.text.sanitize(s) end
---@param query string
---@param items string[]
---@param limit? integer
---@return string[]
function mare.text.fuzzy(query, items, limit) end
--- Nearest candidate to `word` within a small optimal-string-alignment edit distance, or nil.
--- `candidates` may be a string list or a table whose string keys are candidate names.
---@param word string
---@param candidates string[]|table<string, any>
---@return string|nil
function mare.text.closest(word, candidates) end
---@param content string
---@param old string
---@param new string
---@param all? boolean
---@return string|nil, string|nil
function mare.text.replace(content, old, new, all) end
--- Parse patch paths without opening any of the named files. With `opts.resolve = true`,
--- relative paths are resolved literally against cwd (matching `apply_patch`).
---@param patch string
---@param opts? {resolve?: boolean}
---@return {kind: 'add'|'delete'|'update', path: string, move_to?: string}[]|nil, string|nil
function mare.text.patch_paths(patch, opts) end
---@param patch string
---@param opts? {dry_run?: boolean}
---@return {changes: {path: string, kind: string, move_to?: string, old?: string, new?: string}[], summary: string}|nil, string|nil
function mare.text.apply_patch(patch, opts) end
---@param old string
---@param new string
--- Unified diff of two texts (`context` lines around each hunk, default 3; `a/<path>` and
--- `b/<path>` headers), with the added and removed line counts (as `diff_stats`) and the longer
--- side's line count (`ui.Diff{unified=, lines=}`) from the same diff.
---@param opts? {context?: integer, path?: string}
---@return string unified, integer added, integer removed, integer lines
function mare.text.diff(old, new, opts) end
---@return integer added, integer removed
function mare.text.diff_stats(old, new) end
--- Lines of `s` (`\n` or `\r\n` ends one; a final line break adds no empty line) and their
--- count. With `from` only lines from..to (1-based, inclusive, `to` default the last) are
--- returned, so a window of a big text builds only its own strings; `total` counts all.
---@param s string
---@param from? integer
---@param to? integer
---@return string[] lines, integer total
function mare.text.split_lines(s, from, to) end
--- Whether `s` is valid UTF-8 (what strict `utf8.len` accepts), checked in Rust.
---@param s string
---@return boolean
function mare.text.is_utf8(s) end

--- POSIX-shell word splitting (nil when quoting is malformed) and whether the command uses
--- compound syntax outside quotes (`;` `&` `|` `<` `>` newline, `$(…)`, backticks). Command rules
--- must never match a compound command by its first words.
---@param cmd string
---@return string[]|nil argv, boolean compound
function mare.text.shell_split(cmd) end
--- Parse a shell command into plain-word segments joined by `&&` `||` `;` `|`.
--- `analyzable = false` (with `reason`) for substitutions, expansions, subshells,
--- redirections other than fixed sinks (`/dev/null`, `2>&1`), `eval`, nested shells beyond one
--- `bash -c '…'` level, syntax errors, unknown syntax or an exhausted budget. `script` is the
--- inner script of a `bash -lc '…'` / `pwsh -Command '…'` wrapper. PowerShell uses a
--- conservative lexer (plain words, `|`, `;` only).
---@param cmd string|string[]
--- `safe_sinks`: extra fixed redirect targets that keep a command analyzable (e.g. "/dev/stderr").
---@param opts? {shell?: "bash"|"powershell", budget_ms?: integer, max_nodes?: integer, safe_sinks?: string[]}
--- Each segment's `unwrapped` is for conservative denial/prompt matching, never allow rules.
--- `literal_paths` contains only statically identifiable argv words; it is not a complete
--- inventory of the paths a script can access.
---@return {analyzable: boolean, reason?: string, segments: {argv: string[], unwrapped: string[], op?: "&&"|"||"|";"|"|"}[], script?: string, literal_paths: string[]}
function mare.text.shell_parse(cmd, opts) end
--- Built-in dangerous-command rules: the rule a
--- command hits ("forced_rm": rm -f/--force or find -delete anywhere, through sudo/env/trap/
--- bash -c/heredocs/command/exec/nohup/timeout/xargs/find -exec; a script too long to parse or
--- with a syntax error falls back to a plain word scan, so it fails closed;
--- "windows_force_delete": Remove-Item -Force, del /f, rd /s /q — on Windows or for PowerShell),
--- or nil; plus every literal command in the script (argv lists) so plugins can add their own.
---@param cmd string|string[]
---@param opts? {shell?: "bash"|"powershell"}
---@return string|nil rule, string[][] commands
function mare.text.dangerous(cmd, opts) end

--- UTF-8 safe head/tail preview; suffix is kept intact inside both budgets. Errors if it cannot fit.
---@param s string
---@param opts {max_bytes?: integer, max_lines?: integer, suffix?: string}
---@return string, boolean truncated
function mare.text.retain(s, opts) end

--- Strictly decode at most 5 MB using a declared WHATWG charset (default UTF-8).
--- Unknown charsets and malformed sequences raise an error; no replacement characters.
---@param bytes string
---@param charset? string
---@param opts? {final?: boolean} final=false accepts an incomplete trailing character in a bounded prefix; malformed sequences still raise
---@return string
function mare.text.decode(bytes, charset, opts) end

--- Convert HTML on a worker thread, skipping script/style/head/template/noscript and embedded content.
--- Parsed hidden/aria-hidden, hidden inputs and inline display/visibility remove their subtrees.
--- External stylesheets/computed CSS are not evaluated. Hidden/template nodes still count toward work limits.
--- max_tags bounds parse complexity (100000); DOM traversal checks max_nodes (50000)
--- and max_depth (128) before recursive conversion. Ordinary wide pages are supported.
--- Input and output ceilings are 5 MB and 1 MB; truncation preserves UTF-8.
---@async
---@param html string
---@param opts? {max_bytes?: integer, max_output_bytes?: integer, max_tags?: integer, max_nodes?: integer, max_depth?: integer}
---@return string markdown, boolean truncated
function mare.text.html_to_markdown(html, opts) end

--- Explicit VM-local sensitive text projection. No environment, disk, or credential discovery.
--- Only registered exact UTF-8 values are covered; binary/encoded data and opaque userdata are not.
--- Registrations are trusted local capabilities, not an authorization or exfiltration filter.
mare.projection = {}
---@class mare.ProjectionLease
local ProjectionLease = {}
---@type string Random reference, independent of the value; not an authorization token.
ProjectionLease.reference = ""
--- Revoke restoration (idempotent). Projection-only tombstones survive until the VM exits,
--- protecting late output after cancellation/disposal. No memory zeroization guarantee.
function ProjectionLease:close() end
--- Register nonempty UTF-8 text; reject active duplicates and overlap with reserved references.
--- Keep the lease alive; GC closes it. Close explicitly in a ctx:effect cleanup and on session switches.
---@param value string
---@return mare.ProjectionLease
function mare.projection.register(value) end
--- Copy nested string VALUES, preserving table metatables. Cycles/depth>64 and sensitive/reserved
--- string keys fail closed. Functions/userdata are preserved opaque. Does not rewrite originals.
---@generic T
---@param value T
---@return T
--- Always returns a fresh copy. Non-UTF-8 string values fail unless opts.lossy (replace with
--- U+FFFD, second result true when replaced) or opts.preserve_invalid (keep the bytes, projecting
--- registered values inside their valid UTF-8 runs); the two are exclusive. Keys must be UTF-8.
---@param value any
---@param opts? {lossy?: boolean, preserve_invalid?: boolean}
---@return any projected
---@return boolean? lossy
function mare.projection.project(value, opts) end
---@class mare.ProjectionAuthorization opaque, single-use capability derived only from native human receipt consumption

--- Restore only inside an execution context carrying a native projection_authorization,
--- the approved approval_snapshot and matching projection_version. The capability permits
--- only the exact projected arguments accepted by that human response, once.
--- This is not a plaintext getter outside approved execution. Unknown/revoked/malformed references
--- error. The tools service calls this only inside its approved execution context, after persistence
--- and cancellation checks, then validates the restored arguments again. Custom execution services
--- must implement equivalent approval/sandbox boundaries; this primitive alone grants no permission.
---@generic T
---@param value T
---@param revision integer
---@return T
function mare.projection.restore(value, revision) end
---@return integer Monotonic revision, incremented on register/revoke; binds tool approval snapshots.
function mare.projection.revision() end
---@return boolean Has registered or retired values (decoded streams need bounded projection; opaque fragments need withholding).
function mare.projection.active() end
---@param value any
---@return boolean Includes unknown reserved references; does not authorize restoring them.
function mare.projection.contains(value) end

---@class mare.ProjectionChanges
local ProjectionChanges = {}
--- Wait for a later registry revision (register, revoke, GC). Coalesces changes; carries only
--- a revision, never names/values. nil on close/cancellation. Concurrent receivers serialize.
---@async
---@param cancel? mare.Token
---@return integer|nil
function ProjectionChanges:recv(cancel) end
--- Idempotent; wakes pending receivers. Close it in a ctx:effect cleanup; GC closes unused subscriptions.
function ProjectionChanges:close() end
---@return mare.ProjectionChanges
function mare.projection.subscribe() end

---@class mare.ProjectionStream
local ProjectionStream = {}
--- Decoded UTF-8 text only; never encoded JSON/tool-argument fragments. Buffers at most
--- longest known value byte length minus one, plus UTF-8 boundary slack. Separate streams
--- must be used for separate logical ids/types. Revision changes close the stream with error.
---@param chunk string
---@return string safe_prefix May be empty; incomplete potential matches remain private.
function ProjectionStream:push(chunk) end
--- Clean EOF only. Idempotent; returns the projected remaining tail.
---@return string safe_tail
function ProjectionStream:finish() end
--- Cancel/error: discard the incomplete tail instead of publishing it.
function ProjectionStream:close() end
---@return mare.ProjectionStream
function mare.projection.stream() end

---@class mare.json
local JsonInternal = {}
--- Deep-clone a Lua value through strict JSON value representation without JSON text encode/decode.
---@generic T
---@param v T
---@return T
function JsonInternal._clone_lossless(v) end

---@class mare.text
local TextInternal = {}
--- Parse patch paths and resolve relative targets literally against cwd (matching `apply_patch`).
---@param patch string
---@return {kind: 'add'|'delete'|'update', path: string, move_to?: string}[]|nil, string|nil
function TextInternal._patch_paths_resolved(patch) end

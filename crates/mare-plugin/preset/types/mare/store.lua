---@meta _
-- LuaLS definitions: session store, runtime state, crash marks and reload-surviving globals (entry: types/mare.lua)

--- Requests that crash mare (core, survives restarts, bounded to 64 entries —
--- only ended entries are evicted, never a request still recovering).
---@class mare.crash
mare.crash = {}
--- Durably mark a request in flight (persisted before it returns); a mark a dead process left
--- counts as one crash. The fingerprint is the stable request id (`llm` uses `"req:" .. id`).
---@async
---@param fingerprint string
---@return integer crashes so far
---@return string? quarantined_backup path when a corrupt state file was quarantined and reset
function mare.crash.begin(fingerprint) end
--- The attempt ended normally: the entry ends (kept, evictable) and its count is cleared.
---@async
---@param fingerprint string
---@return string? quarantined_backup path when a corrupt state file was quarantined and reset
function mare.crash.finish(fingerprint) end
--- The request was refused, not sent (crash limit): the entry ends (evictable) but keeps its
--- count, so the same request stays refused and the refusal is not counted as another crash.
---@async
---@param fingerprint string
---@return string? quarantined_backup path when a corrupt state file was quarantined and reset
function mare.crash.release(fingerprint) end

---@async
---@class mare.store
mare.store = {}
---@type string
mare.store.root = ""
--- The session format version a header (`{type = "session", version = ...}`) carries; the host
--- refuses a header with any other version.
---@type integer
mare.store.format_version = 0
---@class mare.SessionFile
---@field append fun(self: mare.SessionFile, entry: table, opts?: {sync?: boolean, ack?: string}): boolean, boolean
---@field append_batch fun(self: mare.SessionFile, entries: table[], opts?: {sync?: boolean, ack?: string}): boolean, boolean
---@field append_batch_async async fun(self: mare.SessionFile, entries: table[], opts?: {sync?: boolean, ack?: string}): boolean, boolean
---@field activate fun(self: mare.SessionFile)
---@field flush fun(self: mare.SessionFile): boolean
---@field flush_async async fun(self: mare.SessionFile): boolean
---@field close fun(self: mare.SessionFile)
---@field discard fun(self: mare.SessionFile) release the journal without writing a checkpoint (prepared target rollback)
---@field path fun(self: mare.SessionFile): string
---@field matches_path fun(self: mare.SessionFile, path: string): boolean false proves a missing/different file or lock identity; other IO failures and poisoned writers raise
---@field checkpoint async fun(self: mare.SessionFile): boolean
---@field stats fun(self: mare.SessionFile): {scanned_from: integer, checkpoint_mismatch: boolean, torn_bytes: integer}
---@field install_staged_attachments fun(self: mare.SessionFile, stage: mare.AttachmentStage) install only into a newly created session's exclusive directory

---@class mare.SnapshotRevision
--- Opaque in-memory file identity, stamp and journal generation. Cannot be serialized or
--- substituted between session handles; a revision from another file is rejected.

---@class mare.AttachmentStage
---@field close fun(self: mare.AttachmentStage) release the staging owner; pending writes retain their own ownership
---@field write async fun(self: mare.AttachmentStage, name: string, bytes: string): boolean exclusively create one plain filename and sync; identical contents are idempotent, different contents at the same name are refused

--- Open a session for writing: a non-blocking flock on `<path>.lock` (re-checked
--- against the inode at that path) first, then strict validation of the current format — all
--- under the lock. Unsupported versions are refused unchanged. Fails with "session locked by
--- another process", "session is already open in this process" (a second open here is refused
--- until the first handle is closed) or "session file is corrupt" (file untouched). A torn final
--- entry is cut before the first append. With a resume checkpoint in the session index (length
--- + mtime), only the part after it is validated when it still holds — an optimization that
--- assumes only mare appends; `mare doctor` parses every session in full. Appends are plain
--- writes; `sync = true` (semantic boundaries) fsyncs, and a failed write or fsync truncates back
--- to before it. `append_batch` writes several entries at once; `checkpoint()` records a
--- checkpoint (also done on close); `close()` invalidates the handle immediately. An active
--- snapshot read retains the session lock until that read and any deferred cleanup finish.
--- `existing = true` (resume): a missing file fails with "session file not found" and nothing
--- is created — a stale path never becomes a new, headerless session.
--- The first ordinary handle becomes the active journal; after opening another ordinary handle,
--- `activate()` selects it when the session switches. Switching or closing the active journal
--- discards its pending acknowledgements; with ordinary handles open but none active, the gate
--- refuses calls. Open unrelated/child transcripts with `auxiliary = true`: they neither activate
--- nor gate execution and cannot acknowledge a call. On the active journal, `append(entry,
--- { ack = tool_call_id })` (also supported by `append_batch`) requires a matching entry with
--- `type = "tool_state"`, `state = "started"`, and `tool_call_id = tool_call_id`; it implies `sync`.
--- The host admits that call into `mare.exec.run` once after the write and fsync succeed.
---@param path string
--- `defer_activation = true` opens an ordinary journal without activating it, even when it is the first.
--- `create_new = true` refuses an existing path and creates atomically.
---@param opts? {existing?: boolean, create_new?: boolean, auxiliary?: boolean, defer_activation?: boolean}
---@return mare.SessionFile
function mare.store.open(path, opts) end
--- Read a path-bound snapshot (optionally lenient), retaining the identity of the observed
--- source rather than reopening it after selection. Refused inside a tool call.
---@async
---@param path string
---@param lenient boolean
---@return table[] entries
---@return integer skipped
function mare.store.read_bound(path, lenient) end

--- Read a strict fork snapshot independent of the source writer/lock, optionally requiring
--- the observed session header identity. Refused inside a tool call.
---@async
---@param path string
---@param expected_session_id? string
---@return table[] entries
function mare.store.read_fork_snapshot(path, expected_session_id) end

--- Read through an independently opened handle verified against the locked writer. An
--- optional revision must belong to this handle and still match its stamp/generation.
--- The native read retains the session lock through deferred cleanup. Refused in tool calls.
---@async
---@param file mare.SessionFile
---@param revision? mare.SnapshotRevision
---@return table[] entries
function mare.store.read_snapshot(file, revision) end

--- Capture a stable tail and revision from the same locked writer observation. A subsequent
--- full read can use its revision to reject source mutation. Refused inside a tool call.
---@async
---@param file mare.SessionFile
---@return {header: table, entries: table[], partial: boolean, stable: true, revision: mare.SnapshotRevision}
function mare.store.read_tail_snapshot(file) end

--- Check both the exact handle identity and its current journal generation/file stamp.
--- A revision from another file returns false. Refused inside a tool call.
---@param file mare.SessionFile
---@param revision mare.SnapshotRevision
---@return boolean
function mare.store.snapshot_matches(file, revision) end

--- Exclusively reserve the attachment directory for an unpublished session path. The stage
--- owns cleanup until its owner and every pending write release it. Refused in tool calls
--- and ephemeral read-only sessions. No existing directory is reused or overwritten.
---@param path string unpublished session path (the attachment directory is derived by the host)
---@return mare.AttachmentStage
function mare.store.reserve_staging_attachments(path) end

--- Read a session attachment constrained to `<session>.attachments/` without following
--- directory or file symlinks, bounded by `max_bytes` and verified against its SHA-256 name.
---@async
---@param journal string session file path
---@param name string attachment filename (`<sha256>.<ext>`)
---@param max_bytes? integer
---@return string|nil bytes, string|nil err
function mare.store.read_attachment(journal, name, max_bytes) end

--- Read all entries, lock-free, up to the end as of the call; an unterminated final entry
--- (still being written, or torn) never counts. Strict: any other unparseable line is an error
--- ("session file is corrupt"). `lenient = true` (read-only views of a damaged file)
--- skips bad lines and returns how many. Never migrates or rewrites the file. The file is
--- stat'ed (inode, length, mtime) before and after; if it changed it is read once more —
--- `stable = false` means it still changed, so the result may be incomplete.
---@async
---@param path string
---@param opts? {lenient?: boolean}
---@return table[] entries, integer skipped, integer intact, boolean stable
function mare.store.read(path, opts) end
---@async
---@param dir string
---@return {path: string, header?: table, mtime: number, size: integer}[]
---@return string[] unsupported_paths
function mare.store.list(dir) end
--- Every file of a session: the session file, its lock file, attachments,
--- snapshot journal and sub-agent sessions (`<dir>/agents/<run>/` naming it as parent).
---@param path string
---@return string[]
function mare.store.files(path) end
--- Delete a session and everything in `files(path)` under its lock: refused ("session locked
--- by another process" / "already open in this process") while it is open. The index is the
--- caller's: `index_append({session_id = session_id, path = path, deleted = true})` (never rebuild it).
---@async
---@param path string
---@return boolean ok, string? err
function mare.store.delete(path) end
---@param cwd string
---@return string
function mare.store.dir(cwd) end
--- Reverse read back to the latest compaction (and the message it keeps from). Strict and
--- read-only like `read`.
---@async
---@param path string
---@return {entries: table[], partial: boolean, stable: boolean, header?: table}
function mare.store.read_from_compaction(path) end
---@async
---@param path string
---@param n integer
---@return table[] last n entries
function mare.store.read_tail(path, n) end
---@async
---@param path string
---@param query string
---@return string|nil snippet
function mare.store.search(path, query) end
--- A row of the session index (`<sessions dir>/index.sqlite`): metadata derived
--- from the session file, which stays the only source of truth (the index can be rebuilt).
---@class mare.store.IndexRow
---@field session_id string
---@field path string
---@field kind "main"|"fork"|"agent"
---@field cwd string
---@field project_root? string the repository root Git discovers from `cwd` (the nearest ancestor with a valid `.git` directory or gitfile), else `cwd`
---@field parent_session_id? string the fork origin / the sub-agent's owner
---@field run_id? string sub-agent run
---@field title? string the last `name` entry (a sub-agent: its label)
---@field preview? string the first own prompt
---@field last_input? string the latest own prompt
---@field labels? string[] the session tree's labels
---@field archived boolean
---@field archived_at_ms? integer ms
---@field model? string
---@field provider? string
---@field git_branch? string
---@field created_at_ms integer ms
---@field updated_at_ms integer last message, ms
---@field seq integer activity order (higher = more recent)
---@field messages integer
---@field tokens_in integer
---@field tokens_out integer
local IndexRow = {}

--- After appending to a session file, bring its index row up to date with the file: every
--- field is derived from the file (a catch-up parses only what was appended), `cwd` records
--- where a live writer runs. The row moves to the top when it gained messages; a `name`,
--- `label` or `archive` entry changes its column only. `{session_id, path, deleted = true}`: the
--- session was deleted, drop its row (and its sub-agents'). Multi-process safe.
---@async
---@param entry {path?: string, cwd?: string, session_id?: string, deleted?: boolean}
---@return mare.store.IndexRow? row
function mare.store.index_append(entry) end
--- Sessions from the index, newest first (by `seq`); built from the session files on first
--- use. When the index can't be opened the session files are scanned instead (slower, logged).
--- `scope`: "all" (default), "cwd" (`cwd`, default `mare.cwd`), "project" (the same
--- `project_root`); `archived`: false (default), true, "any"; `kind`: "session" (default: main
--- sessions and forks), "main", "fork", "agent", "any"; `nonempty`: only sessions with messages;
--- `session_id`, `session_id_prefix`, `parent_session_id`: exact filters; `limit`, `offset`: one page. Returns the page and
--- how many rows match in all.
---@async
---@param opts? {scope?: "all"|"cwd"|"project", cwd?: string, archived?: boolean|"any", kind?: "session"|"main"|"fork"|"agent"|"any", nonempty?: boolean, session_id?: string, session_id_prefix?: string, parent_session_id?: string, limit?: integer, offset?: integer}
---@return mare.store.IndexRow[] rows
---@return integer total
---@return string[] unsupported_paths
function mare.store.index_read(opts) end
---@param paths string[]
---@return string[] paths not warned earlier in this process
function mare.store.index_warn_unsupported(paths) end
--- Make the index agree with the session files: unchanged files are only stat'ed, changed ones
--- caught up, unknown ones added, rows of deleted files dropped (the session plugin runs it in
--- the background at startup).
---@async
---@return {files: integer, updated: integer, removed: integer, failed: integer, pending: integer, unsupported: string[]}
function mare.store.index_reconcile() end
--- Re-derive every row from the session files in one transaction (order and resume
--- checkpoints kept). Returns the row count.
---@async
---@return integer count
function mare.store.index_rebuild() end
--- Why the index had to be rebuilt in this process (a damaged file or a schema version
--- change), once; nil when it wasn't or the reason was handed out.
---@return string? reason
function mare.store.index_notice() end
---@async
---@param path string
---@param query string
---@return boolean
function mare.store.file_contains(path, query) end

--- State that survives `/reload` and is gone when mare exits (cf. WezTerm
--- `wezterm.GLOBAL`, Neovim `vim.g`). Strings, numbers, booleans and tables only; reads return
--- copies; no lock — key by plugin name (`mare.g["my-plugin"]`).
---@type table<string, any>
mare.g = {}

---@class mare.store.SessionFilters
---@field exclude_session_id? string
---@field exclude_session_ids? string[]
---@field archived? boolean
---@field session_ids? string[]
---@field created_from_ms? integer epoch ms
---@field created_to_ms? integer epoch ms
---@field parent_session_ids? string[]
---@field include_root_sessions? boolean
---@field availability? ("live"|"persisted")[]

---@class mare.store.EventFilters
---@field seq_from? integer
---@field seq_to? integer
---@field from_ms? integer epoch ms
---@field to_ms? integer epoch ms
---@field event_types? string[]
---@field surfaces? ("current"|"shadowed"|"log_only")[]

---@class mare.store.LiveSessionInput
---@field path string
---@field entries table[]
---@field session_epoch string
---@field step_boundary? integer

---@class mare.store.QueryExecOpts
---@field cancel? mare.Token
---@field live_sessions? mare.store.LiveSessionInput[]

---@class mare.store.SearchHit
---@field session_id string
---@field title string
---@field event_id string
---@field position {seq: integer, text_offset: integer}
---@field snippet string
---@field surface "current"|"shadowed"|"log_only"
---@field type string
---@field matched_at_ms? integer
---@field session_created_at_ms integer
---@field updated_at_ms integer
---@field archived boolean
---@field availability "live"|"persisted"
---@field rank {match_count: integer, document_length: integer}

---@class mare.store.SearchPage
---@field data mare.store.SearchHit[]
---@field next_cursor? string
---@field corpus_revision string
---@field incomplete boolean

--- Project session journal entries to message and tool-call items; control records are omitted.
---@param entries table[]
---@return table[]
function mare.store.project_items(entries) end

---@class mare.store.EventNeighbor
---@field event_id string
---@field seq integer
---@field type string
---@field summary string

---@class mare.store.ReadEventResult
---@field session_id string
---@field corpus_revision string
---@field seq integer
---@field event? table present when reading inline (without offset/max_bytes)
---@field chunk? string present when reading a UTF-8 bounded chunk
---@field chunk_offset? integer
---@field next_offset? integer
---@field total_bytes? integer
---@field truncated? boolean
---@field neighbors mare.store.EventNeighbor[]

---@class mare.SessionLineageHeader
---@field session_id string
---@field created_at_ms? integer epoch ms
---@field title_available boolean false for the native header-only projection

---@class mare.SessionLineageNode
---@field session mare.SessionLineageHeader
---@field descendants mare.SessionLineageNode[]

---@class mare.store.TraceSessionResult
---@field target mare.SessionLineageHeader
---@field ancestors mare.SessionLineageHeader[]
---@field descendants mare.SessionLineageNode[]
---@field complete boolean
---@field root? mare.SessionLineageHeader
---@field parent_boundary boolean
---@field metadata_only boolean
---@field incomplete boolean
---@field scope string

---@class mare.store.TraceEventResult
---@field session_id string
---@field event_id string
---@field seq integer
---@field entry_id? string
---@field type string
---@field role? string
---@field created_at_ms? integer
---@field surface "current"|"shadowed"|"log_only"
---@field replaced_by? integer
---@field replacement_chain integer[]
---@field replaced_event_seqs integer[]
---@field source_event_seqs integer[]
---@field derived_event_seqs integer[]
---@field summary string
---@field availability "live"|"persisted"
---@field incomplete boolean

--- Cwd-scoped policy-bound history, ONLY in tool execution context. Cwd comes from the call.
--- Directory and each candidate must satisfy frozen read policy; denied metadata is omitted.
--- No host index. Native session/agents-run containers only, never attachments or links.
--- At most 10000 readable files/directories, nesting 32; headers <=64 KiB. Selected raw snapshots
--- <=32 MiB total / 8 MiB each. Exceeding a budget is QUERY_BUDGET_EXCEEDED, never silent omission.
--- Session/time filters run on authorized headers first, so a narrowed retry can fit the budget.
--- Message text literal FTS5 phrase (unicode61), 1..512 chars (or title_query); query syntax remains inert data.
--- Match-count descending, text-length ascending, time descending, stable id/seq tiebreaks;
--- each session returns its strongest event. Pages default to 20 results. Opaque cursor binds corpus revision, query and filters;
--- any observed corpus change rejects old cursors as stale. incomplete marks skipped sources.
---@async
---@param opts {query?: string, title_query?: string, sort_key?: "relevance"|"created_at_ms"|"updated_at_ms", sort_direction?: "asc"|"desc", filters?: mare.store.SessionFilters, event_filters?: mare.store.EventFilters, cursor?: string, limit?: integer, step_bound?: boolean}
---@param execution? mare.store.QueryExecOpts cancellation is checked between read chunks/records
---@return mare.store.SearchPage
function mare.store.search_sessions(opts, execution) end

--- Same authority/bounds/corpus revision as search_sessions; all matching events in one session.
--- seq is the zero-based position in the strict complete-entry snapshot (header is zero).
--- text_offset is a Unicode character offset in the original flattened message text.
---@async
---@param opts {session_id: string, query?: string, title_query?: string, sort_key?: "relevance"|"created_at_ms"|"updated_at_ms", sort_direction?: "asc"|"desc", filters?: mare.store.SessionFilters, event_filters?: mare.store.EventFilters, cursor?: string, limit?: integer, step_bound?: boolean}
---@param execution? mare.store.QueryExecOpts
---@return mare.store.SearchPage
function mare.store.search_events(opts, execution) end

--- Search semantic message and tool-call items in one session under the same history policy.
---@async
---@param opts {session_id: string, query?: string, title_query?: string, sort_key?: "relevance"|"created_at_ms"|"updated_at_ms", sort_direction?: "asc"|"desc", filters?: mare.store.SessionFilters, event_filters?: mare.store.EventFilters, cursor?: string, limit?: integer, step_bound?: boolean}
---@param execution? mare.store.QueryExecOpts
---@return {data: table[], next_cursor?: string, corpus_revision: string, incomplete: boolean}
function mare.store.search_items(opts, execution) end

--- Exact raw event at an opaque event_id from a hit, plus bounded neighbor summaries.
--- before/after each 0..10. Single event >64 KiB is refused inline (QUERY_EVENT_TOO_LARGE);
--- pass offset/max_bytes for UTF-8 bounded chunked reads.
--- Corpus revision changes reject old event ids; missing/other-cwd/unreadable targets share errors.
--- Does not load or switch sessions. No child-agent traversal. No persistent query cache.
---@async
---@param opts {event_id: string, before?: integer, after?: integer, offset?: integer, max_bytes?: integer}
---@param execution? mare.store.QueryExecOpts
---@return mare.store.ReadEventResult
function mare.store.read_event(opts, execution) end

--- Read a semantic item in one authorized session; event tokens are not item IDs.
---@async
---@param opts {session_id: string, item_id: string, before?: integer, after?: integer, offset?: integer, max_bytes?: integer}
---@param execution? mare.store.QueryExecOpts
---@return {session_id: string, item_id: string, corpus_revision: string, seq: integer, item?: table, chunk?: string, chunk_offset?: integer, next_offset?: integer, total_bytes?: integer, neighbors: table[]}
function mare.store.read_item(opts, execution) end

--- Trace the journal surface of a semantic item using its session-scoped ID.
---@async
---@param opts {session_id: string, item_id: string}
---@param execution? mare.store.QueryExecOpts
---@return table
function mare.store.trace_item(opts, execution) end

--- Genuine persisted/live lineage from authorized V3 header.parent_session_id, never directory inference.
--- Only visible headers enter the projection. An unavailable parent is an anonymous boundary,
--- no hidden parent id/title/path. complete describes the known parent chain, not a global corpus.
--- Max 1000 nodes / descendant depth 32; over-budget/cycles/source changes fail explicitly.
---@async
---@param opts {session_id: string}
---@param execution? mare.store.QueryExecOpts
---@return mare.store.TraceSessionResult
function mare.store.trace_session(opts, execution) end

--- Event surface projection and lineage trace (compaction replacement chains, branch summaries,
--- tool call/result links) for one event by event_id or (session_id, seq).
---@async
---@param opts {event_id?: string, session_id?: string, seq?: integer}
---@param execution? mare.store.QueryExecOpts
---@return mare.store.TraceEventResult
function mare.store.trace_event(opts, execution) end

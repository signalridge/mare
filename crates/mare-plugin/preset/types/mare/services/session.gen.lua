---@meta _
-- GENERATED from types/mare/services/contracts/session.lua (service `session`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.SessionService
local M = {}

--- Append a custom plugin record outside the model message tree.
---@param kind string
---@param data? any
---@param opts? {persist?: boolean, sync?: boolean, entry_id?: string, session_epoch?: string, session_id?: string}
---@return {durable: boolean, record_id: string}
function M:add_record(kind, data, opts) end

--- Atomically validate, persist, and publish additional workspace directories.
---@param dirs string[]
---@return { dirs: string[], sandbox?: mare.SandboxPolicy|nil }
function M:add_workspace_dirs(dirs) end

--- Record a compaction entry and return the updated message context or `nil, err`.
---@param summary string
---@param first_kept_msg? mare.Message
---@param tokens_before integer
---@param details? table<string, any>
---@param extra? {method?: string, range?: mare.Message[], tokens_after?: integer, preserved?: table[], remote?: table, projections?: table<mare.Message, any>, model?: mare.Model, validate?: (fun(): boolean), publish?: fun(new: mare.Message[])}
---@return mare.Message[]|nil
---@return string?
function M:append_compaction(summary, first_kept_msg, tokens_before, details, extra) end

--- Record a message-content prune entry and return the updated message context or `nil, err`.
---@param map table<mare.Message, any>
---@param meta? {method?: string, tokens_before?: integer, tokens_after?: integer, model?: mare.Model, validate?: (fun(): boolean), publish?: fun(new: mare.Message[])}
---@return mare.Message[]|nil
---@return string?
function M:append_prune(map, meta) end

--- Archive a saved inactive session (and its child agent sessions) by id, unique prefix, or path.
---@param session_id string
---@return table
function M:archive(session_id) end

--- Whether the session store is usable now (`false, reason` under `--no-session`, in a child agent, or during a persistence fault).
---@return boolean
---@return string|nil
function M:available() end

--- Non-yielding snapshot cut of in-memory session entries.
---@return table<string, any>
function M:baseline_cut() end

--- Open a sub-session transcript handle under `<session dir>/agents/<run_id>/` owned by the caller.
---@param opts {label?: string, meta?: table<string, any>, run_id: string, seq?: integer, [string]: any}
---@return {path: string, session_id: string, append: fun(entry: table, opts?: {sync?: boolean}), close: fun()}
function M:child(opts) end

--- List sub-session transcript files, newest run first.
---@param run_id? string
---@return {name: string, path: string, run_id: string, [string]: any}[]
function M:children(run_id) end

--- Claim or replay a generic idempotent operation; concurrent duplicates wait on the same writer.
---@param identity mare.OperationIdentity
---@param cancel mare.Token
---@return mare.OperationClaim|nil
---@return string?
function M:claim_operation(identity, cancel) end

--- Unload the active session runtime and release its lock without exiting the host.
---@param session_id string
---@param opts? {expected_epoch?: string}
---@return table
function M:close(session_id, opts) end

--- Atomically append accepted user messages and the remaining identified inbox.
---@param batch {version: 1, messages: table[], inbox: {steer: table[], follow_up: table[]}}
---@param opts? {session_epoch?: string, session_id?: string, turn_id?: string, cancel?: mare.Token?, [string]: any}
---@return {messages: table[], inbox: table}|nil
---@return string?
function M:commit(batch, opts) end

--- Atomically commit one model step's `agent/step-state` record and validated context disclosure messages.
---@param record {states: table<string, any>, step: integer, step_id: string, turn_id: string, version: integer, [string]: any}
---@param messages mare.Message[]
---@param opts {persist: boolean, session_epoch: string, session_id: string, cancel?: mare.Token, [string]: any}
---@return boolean|nil
---@return string?
function M:commit_step(record, messages, opts) end

--- Create a new idle session runtime (optionally named, in cwd, or idempotent by request_id).
---@param opts? {cwd?: string, name?: string, request_id?: string, subscribe?: boolean}
---@return mare.LiveSession
function M:create(opts) end

--- Permanently delete a saved inactive session (and its child agent sessions) by id, unique prefix, or path.
---@param session_id string
---@return table
function M:delete(session_id) end

--- Return session journal entries (`expand = true` expands compacted/pruned views).
---@param opts? {expand?: boolean, [string]: any}
---@return table<string, any>[]
function M:entries(opts) end

--- Return the session entry id of a message in the current context, or nil when unknown.
---@param msg mare.Message
---@return string|nil
function M:entry_id(msg) end

--- Return files edited by tools in this session (`old`/`new` content or `false` when absent).
---@return ({new: string|boolean, old: string|boolean, path: string, [string]: any})[]
function M:file_changes() end

--- Create a new session forked from an active or saved session (optionally before an item).
---@param opts {before_item_id?: string, expected_epoch?: string, name?: string, request_id?: string, session_id: string, subscribe?: boolean}
---@return mare.LiveSession
function M:fork(opts) end

--- Fetch one session snapshot by id, unique prefix, or path.
---@param session_id string
---@return mare.Session
function M:get(session_id) end

--- Read one semantic item by session_id and item_id, inline or UTF-8 chunked, with optional neighbors.
---@param opts {after?: integer, before?: integer, item_id: string, max_bytes?: integer, offset?: integer, session_id: string}
---@return {chunk?: string, chunk_offset?: integer, corpus_revision: string, item?: {archived?: boolean, arguments?: any, availability?: string, entry_id: string, item_id: string, matched_at_ms?: integer, message?: table<string, any>, name?: string, parent_entry_id?: string, position?: {seq: integer, text_offset: integer}, provider_call_id?: string, rank?: {document_length: integer, match_count: integer}, result_entry_id?: string, session_created_at_ms?: integer, session_id?: string, snippet?: string, surface?: string, title?: string, tool_call_id?: string, type: "user_message"|"assistant_message"|"tool_call"|"custom", updated_at_ms?: integer}, item_id: string, neighbors: {item_id: string, type: string}[], next_offset?: integer, seq: integer, session_id: string, total_bytes?: integer}
function M:get_item(opts) end

--- Return live session identity, path, leaf entry id, message count, and status flags.
---@return {children_dir?: string, count: integer, crashed?: boolean, ephemeral?: boolean, leaf_entry_id?: string, name?: string, path?: string, planned?: string, readonly?: boolean, resume_error?: string, resume_error_code?: string, session_epoch: string, session_id: string, unstable?: boolean, [string]: any}
function M:info() end

--- Set or clear a label on a session entry (defaults to the current leaf).
---@param label string
---@param opts? {item_id?: string}
---@return {item_id: string, label?: string}
function M:label_item(label, opts) end

--- Return the id of the current session leaf entry, or nil before the first message.
---@return string|nil
function M:leaf() end

--- List saved sessions in the index, newest first.
---@param opts? {all?: boolean, archived?: boolean, cursor?: string, cwd?: string, limit?: integer, loaded?: boolean, parent_session_id?: string}
---@return {data: mare.Session[], next_cursor?: string}
function M:list(opts) end

--- List semantic items of the open session, optionally filtered by branch, item type, or order.
---@param opts? {branch?: "current"|"all", cursor?: string, direction?: "asc"|"desc", limit?: integer, types?: string[]}
---@return {data: ({archived?: boolean, arguments?: any, availability?: string, entry_id: string, item_id: string, matched_at_ms?: integer, message?: table<string, any>, name?: string, parent_entry_id?: string, position?: {seq: integer, text_offset: integer}, provider_call_id?: string, rank?: {document_length: integer, match_count: integer}, result_entry_id?: string, session_created_at_ms?: integer, session_id?: string, snippet?: string, surface?: string, title?: string, tool_call_id?: string, type: "user_message"|"assistant_message"|"tool_call"|"custom", updated_at_ms?: integer})[], leaf_entry_id?: string, next_cursor?: string}
function M:list_items(opts) end

--- List custom plugin records of the given kind in the open session.
---@param kind string
---@param opts? {branch?: "current"|"all", cursor?: string, limit?: integer, loaded_only?: boolean, session_epoch?: string, session_id?: string}
---@return {data: any[], next_cursor?: string}
function M:list_records(kind, opts) end

--- Return a detached live snapshot of the active session for step-bounded history queries, or nil.
---@return {entries: table<string, any>[], path: string, step_boundary?: integer, [string]: any}|nil
function M:live_snapshot() end

--- Load an existing session by session_id, unique prefix, name, or (for primary) path.
---@param opts? {path?: string, session_id?: string, subscribe?: boolean}
---@return mare.LiveSession
function M:load(opts) end

--- Open a session transcript file in the live transcript viewer.
---@param path string
---@param title? string
---@param opts? {keys?: ({action: string|any[], label: string, [string]: any})[], [string]: any}
---@return table
function M:load_transcript(path, title, opts) end

--- Register a pre-switch session resource drain callback owned by the caller; returns a release disposer.
---@param fn fun()
---@return fun() dispose releases this registration
function M:on_drain(fn) end

--- Read-only lookup of a committed generic operation receipt without claiming or mutating state.
---@param identity mare.OperationIdentity
---@return mare.OperationReceiptLookup|nil
---@return string?
---@return string?
function M:operation_receipt(identity) end

--- Return the branch path of entries leading to `leaf_entry_id` (default: current leaf).
---@param leaf_entry_id? string
---@return table<string, any>[]
function M:path_to(leaf_entry_id) end

--- Persist one conversation message synchronously to the session journal.
---@param opts {durable?: boolean, entry_id?: string, message: table<string, any>, session_epoch?: string, session_id?: string, skip_initial_model_quarantine?: boolean, skip_quarantine?: boolean, [string]: any}
---@return string|{entry_id?: string, status: "read_only", [string]: any}
function M:persist_message(opts) end

--- Ensure the parent session header exists before recording sub-agent transcripts.
---@return boolean|nil
---@return string?
function M:prepare_children() end

--- Drain side requests, flush the active journal, and return `{ path, images? }` for a multi-runtime fork.
---@return { path: string, images?: table<string, string> }|nil
function M:prepare_fork() end

--- Record an agent/request marker synchronously.
---@param opts {attempt?: integer, durable?: boolean, entry_id?: string, request_id: string, session_epoch?: string, session_id?: string, skip_quarantine?: boolean, [string]: any}
---@return string|{entry_id?: string, status: "read_only", [string]: any}
function M:record_request(opts) end

--- Record an agent/request-aborted marker synchronously.
---@param opts {attempt?: integer, durable?: boolean, entry_id?: string, request_id: string, session_epoch?: string, session_id?: string, skip_quarantine?: boolean, [string]: any}
---@return string|{entry_id?: string, status: "read_only", [string]: any}
function M:record_request_aborted(opts) end

--- Record an agent/request-end marker synchronously.
---@param opts {attempt?: integer, durable?: boolean, entry_id?: string, request_id: string, session_epoch?: string, session_id?: string, skip_quarantine?: boolean, [string]: any}
---@return string|{entry_id?: string, status: "read_only", [string]: any}
function M:record_request_end(opts) end

--- Record an agent/run-end fork boundary marker durably.
---@param opts? {durable?: boolean, run_id?: string, session_epoch?: string, session_id?: string, skip_quarantine?: boolean, [string]: any}
---@return boolean|{status: "read_only", [string]: any}
function M:record_run_end(opts) end

--- Record one tool execution phase marker synchronously.
---@param opts {durable?: boolean, entry_id?: string, idempotent?: boolean, name: string, session_epoch?: string, session_id?: string, side_effect?: string, skip_quarantine?: boolean, state: "prepared"|"started"|"completed", tool_call_id: string, cancel?: mare.Token?, [string]: any}
---@return string|{entry_id?: string, status: "read_only", [string]: any}
function M:record_tool_state(opts) end

--- Release an uncommitted operation claim and wake duplicate waiters without changing durable state.
---@param claim mare.OperationClaim
---@param reason? string
---@return boolean
function M:release_operation(claim, reason) end

--- Move the active session leaf back before an item (optionally summarizing or restoring files).
---@param opts {before_item_id?: string, expected_epoch?: string, restore_files?: boolean, session_id: string, summarize?: boolean, text?: string}
---@return mare.LiveSession
function M:rewind(opts) end

--- Full-text and metadata search across persisted and live sessions in the active workspace.
---@param opts? {cursor?: string, event_filters?: {event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: ("current"|"shadowed"|"log_only")[], to_ms?: integer}, filters?: {archived?: boolean, availability?: ("live"|"persisted")[], created_from_ms?: integer, created_to_ms?: integer, exclude_session_id?: string, exclude_session_ids?: string[], include_root_sessions?: boolean, parent_session_ids?: string[], session_ids?: string[]}, limit?: integer, query?: string, sort_direction?: "asc"|"desc", sort_key?: "relevance"|"created_at_ms"|"updated_at_ms", step_bound?: boolean, title_query?: string}
---@return {corpus_revision: string, data: {archived?: boolean, availability?: string, item_id?: string, matched_at_ms?: integer, position: {seq: integer, text_offset: integer}, rank?: {document_length: integer, match_count: integer}, session_created_at_ms?: integer, session_id: string, snippet: string, surface?: string, title: string, type?: string, updated_at_ms?: integer}[], incomplete: boolean, next_cursor?: string}
function M:search(opts) end

--- Search matching semantic items inside one session under workspace policy.
---@param opts {cursor?: string, event_filters?: {event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: ("current"|"shadowed"|"log_only")[], to_ms?: integer}, filters?: {archived?: boolean, availability?: ("live"|"persisted")[], created_from_ms?: integer, created_to_ms?: integer, exclude_session_id?: string, exclude_session_ids?: string[], include_root_sessions?: boolean, parent_session_ids?: string[], session_ids?: string[]}, limit?: integer, query?: string, session_id: string, sort_direction?: "asc"|"desc", sort_key?: "relevance"|"created_at_ms"|"updated_at_ms", step_bound?: boolean, title_query?: string}
---@return {corpus_revision: string, data: ({archived?: boolean, arguments?: any, availability?: string, entry_id: string, item_id: string, matched_at_ms?: integer, message?: table<string, any>, name?: string, parent_entry_id?: string, position?: {seq: integer, text_offset: integer}, provider_call_id?: string, rank?: {document_length: integer, match_count: integer}, result_entry_id?: string, session_created_at_ms?: integer, session_id?: string, snippet?: string, surface?: string, title?: string, tool_call_id?: string, type: "user_message"|"assistant_message"|"tool_call"|"custom", updated_at_ms?: integer})[], incomplete: boolean, next_cursor?: string}
function M:search_items(opts) end

--- Retain the configured default sandbox policy for new or unrecorded resumed sessions without changing the current policy.
---@param spec mare.SandboxSpec|false
---@return table
function M:set_default_sandbox_mode(spec) end

--- Move the session leaf to `entry_id` (`nil` = before the first message) and return the new context or `nil, err`.
---@param entry_id? string|nil
---@return table[]|nil
---@return string?
function M:set_leaf(entry_id) end

--- Switch this session's sandbox policy, log it in the session, and emit `sandbox/changed`.
---@param spec mare.SandboxSpec|nil
---@return mare.SandboxPolicy|nil
function M:set_sandbox_mode(spec) end

--- Return the shortest unique prefix (8+ characters) of a session id in the index.
---@param session_id string
---@return string
function M:short_id(session_id) end

--- Create a persisted fork snapshot of an active or saved session without changing the active session target or cancelling running turns.
---@param opts {before_item_id?: string, effort?: string, expected_epoch?: string, initiating_tool_call_id?: string, last_completed_turns?: integer, model?: string, name?: string, request_id?: string, session_id: string}
---@return mare.Session
function M:snapshot(opts) end

--- Build a normalized SES-FORK context snapshot without switching or modifying the source session.
---@param opts? {allow_memory?: boolean, before_item_id?: string, effort?: string, model?: string, session_id?: string}
---@return {completed_turn_indices?: integer[], cwd?: string, effort?: string, leaf_entry_id?: string, messages: table[], model?: string, name?: string, path?: string, persisted: boolean, session_id: string, source_count: integer}
function M:snapshot_context(opts) end

--- Switch the active session in-place on a single-runtime host (`new`, `load`, or `fork`).
---@param opts {before_item_id?: string, cwd?: string, kind: "new"|"load"|"fork", name?: string, new_session_id?: string, path?: string, restore_permission_ceiling?: string, restore_sandbox_ceiling?: table<string, any>, session_id?: string}
---@return mare.LiveSession
function M:switch(opts) end

--- True while a session switch is preparing or finishing, or after an indeterminate failure.
---@return boolean
function M:switching() end

--- Trace ancestor and descendant session lineage under workspace policy.
---@param session_id string
---@return {ancestors: {created_at_ms?: integer, session_id: string, title_available: boolean}[], complete: boolean, descendants: table<string, any>[], incomplete: boolean, metadata_only: boolean, parent_boundary: boolean, root?: {created_at_ms?: integer, session_id: string, title_available: boolean}, scope: string, target: {created_at_ms?: integer, session_id: string, title_available: boolean}}
function M:trace(session_id) end

--- Trace the containing entry and compaction surface for one semantic item.
---@param opts {item_id: string, session_id: string}
---@return {availability: string, created_at_ms?: integer, derived_event_seqs: integer[], entry_id?: string, incomplete: boolean, item_id: string, replaced_by?: integer, replaced_event_seqs: integer[], replacement_chain: integer[], result_entry_id?: string, role?: string, seq: integer, session_id: string, source_event_seqs: integer[], summary: string, surface: string, type: string}
function M:trace_item(opts) end

--- Orchestrate a clean-context (`new`) or `workspace` session transition.
---@param spec mare.SessionTransitionSpec
---@param opts {defer?: boolean, session_epoch: string, session_id: string, cancel?: mare.Token?, [string]: any}
---@return {status: "applied"|"deferred"|"queued"|"indeterminate"|"stale"|"rejected", transition_id?: string, mode?: string, source?: mare.SessionTransitionSource, target?: mare.SessionTransitionSource, initial_status?: "settled", turn_id?: string, error?: string}
function M:transition(spec, opts) end

--- Restore an archived session (and its child agent sessions) by id, unique prefix, or path.
---@param session_id string
---@return table
function M:unarchive(session_id) end

--- Rename the open session (optionally guarded by expected_revision) and return its Session snapshot.
---@param name string
---@param opts? {expected_revision?: integer}
---@return mare.LiveSession
function M:update(name, opts) end

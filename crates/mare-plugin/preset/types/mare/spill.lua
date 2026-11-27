---@meta _
--- Output storage: session-owned recovery objects with local age maintenance.
--- All operations are async and throw on failure. Text and bounded raw bytes; files are private/exclusive.
--- Save/delete bypass writable roots only for this storage, never deny_read; read honors restrict_read.
--- Reject external paths, symlinks and hardlinks. Resume/unload retain files; delete invalidates the spill_id.
---@alias mare.SpillSource {kind: "tool", tool_name: string, tool_call_id: string, label?: string}|{kind: "session-reference", session_id: string, label: string}
---@alias mare.SpillSaveOpts {suggested_name?: string, source?: mare.SpillSource}
---@alias mare.SpillRef {spill_id: string, size: integer, path: string, retrieval_hint: string, source?: mare.SpillSource}

mare.spill = {}
---@async
---@param session_id string owning session; RPC always supplies the active session
---@param text string
---@param opts? mare.SpillSaveOpts
---@return mare.SpillRef path is for local read/bash interoperability; RPC omits it
function mare.spill.save(session_id, text, opts) end
--- Bounded UTF-8 pagination (max 2000 lines/50KiB). next_byte continues a giant line exactly.
--- Ownership is checked against the active execution context's session.
--- Inside a confined call, `opts.session_id` may only equal the active context session; passing a
--- different session fails with `denied`.
---@async
---@param spill_id string opaque spill_id returned by save, must belong to the active session
---@param opts? {offset?: integer, limit?: integer, cursor?: integer, session_id?: string} offset is 1-based from cursor (default byte 0)
---@return {content: string, more: boolean, next_byte: integer}
function mare.spill.read(spill_id, opts) end
--- Delete a session-owned spill object. Ownership is checked against the active session.
--- Inside a confined call, `opts.session_id` may only equal the active context session; passing a
--- different session fails with `denied`.
---@async
---@param spill_id string opaque spill_id returned by save
---@param opts? {session_id?: string}
function mare.spill.delete(spill_id, opts) end

--- Persist raw bytes (including NUL/non-UTF-8); at most 8 MiB per call. Same spill_ids,
--- owner checks, permission policy, cancellation cleanup and explicit delete as text save.
---@async
---@param session_id string
---@param bytes string
---@param opts? mare.SpillSaveOpts
---@return mare.SpillRef
function mare.spill.save_bytes(session_id, bytes, opts) end
--- Read a byte page without text decoding. Cursor is zero-based, beyond EOF is refused.
--- Default limit 1 MiB, maximum 8 MiB. EOF returns empty bytes, more=false. Text read still
--- rejects non-UTF-8; byte read works for both text and binary output. No image/MIME inference.
--- Inside a confined call, `opts.session_id` may only equal the active context session; passing a
--- different session fails with `denied`.
---@async
---@param spill_id string opaque spill_id returned by save_bytes, must belong to the active session
---@param opts? {cursor?: integer, limit?: integer, session_id?: string}
---@return {bytes: string, more: boolean, next_byte: integer, size: integer}
function mare.spill.read_bytes(spill_id, opts) end

---@class mare.LocalSpill
---@field identity string Stable opaque namespace derived from the fixed resolved root; contains no host path.
---@field save fun(session_id: string, text: string, opts?: mare.SpillSaveOpts): mare.SpillRef
---@field read fun(spill_id: string, opts?: {offset?: integer, limit?: integer, cursor?: integer, session_id?: string}): {content: string, more: boolean, next_byte: integer}
---@field delete fun(spill_id: string, opts?: {session_id?: string})
---@field save_bytes fun(session_id: string, bytes: string, opts?: mare.SpillSaveOpts): mare.SpillRef
---@field read_bytes fun(spill_id: string, opts?: {cursor?: integer, limit?: integer, session_id?: string}): {bytes: string, more: boolean, next_byte: integer, size: integer}
---@field dispose fun(): {removed: integer, warnings: string[]} async; refuses new calls, waits for in-flight I/O and startup cleanup; files survive

--- Construct an owned local backend, using the same text/binary I/O and permissions.
--- Relative root resolves against the host session cwd, fixed at construction.
--- cleanup_period_days defaults to 30; integer 0 disables cleanup. A one-shot background
--- sweep removes only strictly expired managed private regular files, never follows links,
--- preserves active directories and reports failures as warnings without blocking saves.
--- Maintenance freezes its originating policy and optional cancellation token; it never
--- reads a later wider global policy. Supply the operation token when constructing inside
--- cancelable work; an available execution-context cancel token is inherited by default.
--- Reads never start maintenance. Call dispose from the owning context's ctx:effect cleanup;
--- cached methods then reject calls.
--- Reconstructing the same root permits reading its surviving session-owned spill objects.
---@async
---@param opts? {root?: string, cleanup_period_days?: integer, cancel?: mare.Token}
---@return mare.LocalSpill
function mare.spill.new_local(opts) end

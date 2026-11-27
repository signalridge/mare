---@meta _
--- mare's runtime state (`<state>/state.json`): locked, atomic read-modify-write; a corrupt file
--- is backed up to `state.json.bak` and never overwritten (the write fails instead).
--- `version` is reserved for the file format and is not exposed as a state key.
---@class mare.state
mare.state = {}
--- The whole state (`{}` when missing); nil + error when the file is corrupt.
---@return table|nil state, string|nil err
function mare.state.read() end
--- `fn(state)` edits the table in place or returns a replacement; must not yield. Existing JSON
--- nulls are passed as `mare.json.null` so unchanged values survive an update.
---@param fn fun(state: table): table|nil
---@return table
function mare.state.update(fn) end
--- Set (nil removes) one key.
---@param key string
---@param value any
function mare.state.set(key, value) end

---@class mare.StateSnapshot
---@field namespace string
---@field revision integer safe monotonic CAS token; zero only when no envelope exists
---@field exists boolean false for absent/deleted data, including a retained revision tombstone
---@field value any strict JSON; absent/deleted value is mare.json.null, present JSON null stays distinct
---@alias mare.StateErrorCode "invalid_request"|"invalid_namespace"|"invalid_value"|"read_only"|"denied"|"cancelled"|"lock_timeout"|"corrupt"|"unsupported_version"|"revision_exhausted"|"io"|"unknown_state"
---@class mare.StateFailure
---@field code mare.StateErrorCode
---@field message string
---@alias mare.StateReadResult {status: "ok", snapshot: mare.StateSnapshot}|{status: "rejected", error: mare.StateFailure}
---@alias mare.StateExchangeResult {status: "ok"|"conflict", snapshot: mare.StateSnapshot}|{status: "rejected", error: mare.StateFailure}
--- Read one isolated private namespace under <state>/domains, without modifying legacy state.json.
--- Namespace names are 1..128 ASCII letters/digits/._-, first character alphanumeric, never paths
--- or authority. Null and empty object/array identities are preserved; malformed/unknown versions reject.
--- No callbacks, resources or handles survive this call. Caller read restrictions still apply.
---@async
---@param request {namespace: string}
---@return mare.StateReadResult
function mare.state.snapshot(request) end
--- Atomically replace a namespace at its exact revision, with temp fsync + rename + directory fsync.
--- Conflict performs no write. Deletion requires value=mare.json.null and retains a revision tombstone.
--- Values are strict JSON; functions, arbitrary userdata/metatables, cycles and unsafe numbers reject.
--- Ephemeral-read-only mutation and deny_read contexts reject; no namespace grants permissions.
--- Cancellation/drop during lock wait or before publication rejects; the final check after temp fsync
--- admits the irreversible publication phase (CAS visibility linearizes at rename). After it, cancellation
--- cannot promise rollback. unknown_state means publication
--- may have happened: reread/reopen and reconcile before continuing, never pretend the old value is intact.
--- Plugin unload aborts the owner future; the worker retains only detached JSON and a child cancel token.
---@async
---@param request {namespace: string, expected_revision: integer, value: any, delete?: boolean}
---@param options? {cancel?: mare.Token}
---@return mare.StateExchangeResult
function mare.state.compare_exchange(request, options) end

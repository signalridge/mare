---@meta _

--- VM-local stream helpers; no IO, background timer or event dispatch. These handles are
--- not serializable and grant no execution authority. RPC consumers receive stream events.
---@class mare.stream
mare.stream = {}

---@class mare.StreamBuffer
local Buffer = {}
--- Append exact bytes, including NUL and incomplete UTF8. Callers bound accumulated input;
--- no automatic truncation or output retention is applied to this local buffer.
---@param bytes string
function Buffer:append(bytes) end
--- Materialize an immutable byte-string snapshot; later appends and clear do not change it.
---@return string
function Buffer:snapshot() end
--- Materialize a bounded UTF-8-safe tail covering at most `max_lines` lines within the last
--- `max_bytes` bytes (O(min(len, max_bytes))).
---@param max_lines integer
---@param max_bytes integer
---@return string
function Buffer:tail(max_lines, max_bytes) end
function Buffer:clear() end
---@return integer byte_count
function Buffer:len() end
---@return mare.StreamBuffer independent empty buffer
function mare.stream.buffer() end

---@alias mare.CoalescedDeltaKind "text_delta"|"reasoning_delta"|"tool_call_delta"
---@class mare.CoalescedDelta
---@field type mare.CoalescedDeltaKind
---@field id? string
---@field delta string exact concatenated bytes

---@class mare.StreamCoalescer
local Coalescer = {}
--- Append only when kind/id match the pending key. False leaves pending data unchanged;
--- flush, dispatch and retry this same delta after a key change. Invalid kind raises.
---@param kind mare.CoalescedDeltaKind
---@param id string|nil
---@param bytes string
---@return boolean appended
function Coalescer:append(kind, id, bytes) end
--- After dispatching a flush, preserve any pending key installed by a reentrant listener
--- and append bytes to it; otherwise initialize the supplied key. Does not dispatch.
---@param kind mare.CoalescedDeltaKind
---@param id string|nil
---@param bytes string
function Coalescer:append_after_flush(kind, id, bytes) end
---@return boolean
function Coalescer:has_pending() end
--- True with pending data once 50ms elapsed since last emission. Caller supplies one
--- consistent monotonic clock in milliseconds; initial last emission is zero.
---@param now_ms number
---@return boolean
function Coalescer:due(now_ms) end
--- Transfer pending data and mark its emission time before dispatch. Empty returns nil
--- without changing the clock. Non-delta events must flush pending data first.
---@param now_ms number
---@return mare.CoalescedDelta|nil
function Coalescer:flush(now_ms) end
--- Transfer pending data without changing emission time or invoking callbacks.
---@return mare.CoalescedDelta|nil
function Coalescer:take_pending() end
---@param now_ms number
function Coalescer:mark_emitted(now_ms) end
---@return mare.StreamCoalescer independent empty coalescer
function mare.stream.coalescer() end

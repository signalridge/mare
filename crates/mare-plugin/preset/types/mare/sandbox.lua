---@meta _
--- Bounded Lua capability VM in a dedicated worker process, fresh or owner-retained.
--- No io/os/require/debug/mare or code loaders. Bounded scope-owned timers are available.
--- Host callbacks bind the current run execution context; retained old capabilities expire.
---@class mare.SandboxOptions
---@field persistent? boolean retain Lua globals/functions in the same owner VM (default false)
---@field worker_executable? string trusted mare executable path (default current executable)
---@field expose? table<string,any> explicit capabilities, no metatables/cycles; reserved globals rejected
---@field timeout_ms? integer|false 1..600000; default 60000; false disables elapsed deadline
---@field memory_bytes? integer 1..268435456; default 33554432, Lua allocator only
---@field max_bytes? integer 1..16777216; default 1048576, input/bridge/output byte bound
---@field max_message_bytes? integer 1..16777216, defaults to max_bytes; bridge/pending JSON bytes
---@field max_output_bytes? integer 4..16777216, defaults to max_bytes; per-run exact JSON items array + result or diagnostic bytes
---@field max_pending_calls? integer 1..1024, default 128
---@field stop_grace_ms? integer 1..600000, default 3000; polite process stop
---@field grace_ms? integer 1..600000, default 3000; diagnostic stream drain
---@field max_instructions? integer 1..100000000; default 10000000
---@field on_output? fun(text:string) receives bounded printed output in execution context
---@field cancel? mare.Token cooperative cancellation, sticky even when Lua catches errors

---@class mare.LuaSandbox
local Sandbox = {}
--- Fresh VMs run once; persistent VMs accept sequential runs. All bridge/return values must be lossless JSON; use json.null and json.array().
--- Cancellation/timeouts terminate and reap the worker process (including native C work).
--- Failed process cleanup reports stop_unconfirmed. Host callbacks still require cooperative
--- cancellation; confirmed worker exit never implies external effects were rolled back.
---@param code string text only
---@param opts? mare.SandboxOptions per-run limits/expose/cancel/output callback; retained capabilities expire each run
---@return {result:any, output:string,state_lost?:boolean}|nil
---@return string|nil error
---@return {output:string,cancelled:boolean,stop_unconfirmed:boolean,state_lost?:boolean}|nil failure status; output preserves streamed prints
function Sandbox:run(code, opts) end
--- Sticky cancellation; single-use VMs cannot restart.
function Sandbox:stop() end
--- Cancel active work, await worker cleanup and release retained state.
---@async
---@return boolean confirmed
function Sandbox:close() end

---@class mare.SandboxCompletion
--- Opaque trusted-host callback control. Returning it ends only the current run successfully.
--- It is not a JSON value; script code cannot construct it. Cancel/limits still take precedence.
---@class mare.sandbox
mare.sandbox = {}
--- Return the opaque host-callback sentinel that ends the current sandbox run successfully.
---@return mare.SandboxCompletion
function mare.sandbox.complete() end
--- Create a bounded Lua capability VM backed by a dedicated worker process.
---@param opts? mare.SandboxOptions
---@return mare.LuaSandbox
function mare.sandbox.new(opts) end

--- One run's exact JSON output ledger. Entries are copied immutable JSON, with
--- array brackets/commas/escaping/UTF8 charged. Finalization charges the return
--- value or error string. Overflow retains a fitting text prefix and bounded
--- diagnostic; entries already streamed to observers cannot be recalled.
---@class mare.OutputLedger
local OutputLedger = {}
--- Admit a lossless JSON entry if it fits within the remaining byte budget.
---@param value any lossless JSON entry
---@return boolean accepted false is sticky until this ledger is discarded
function OutputLedger:admit(value) end
--- Finalize the ledger with an optional return value or error diagnostic.
---@param value? any optional JSON return value (json.null is present)
---@param error? string failure diagnostic; takes precedence over value
---@return {items:table[],result?:any,error?:string,limited:boolean,bytes:integer}
function OutputLedger:finish(value, error) end
--- Create a bounded JSON output ledger with an exact `max_bytes` budget (4..16777216).
---@param max_bytes integer 4..16777216
---@return mare.OutputLedger
function mare.sandbox.output_ledger(max_bytes) end

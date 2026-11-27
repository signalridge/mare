---@meta _
-- LuaLS types for security decisions as a service (mare_kernel/plugins/policy.lua) and its
-- host primitive. A plugin reaches it as `ctx.policy` (`inject = { "policy" }`).

--- A voter's options.
---@class mare.PolicyRegisterOptions
---@field timeout_ms? number a vote not given in time is a deny
---@field slot? string one decision slot: only the earliest registered voter of a slot runs
---@field prepend? boolean run before the ones registered earlier

--- A rewriter's options.
---@class mare.PolicyRewriteOptions
---@field timeout_ms? number a rewrite not given in time is a failure
---@field prepend? boolean run before the ones registered earlier

---@class mare.PolicyService
local PolicyService = {}

--- Register a voter for decision point `point`; an effect of the caller's fiber (it goes with it).
--- The voter gets the decision's arguments (frozen) and returns nil (no objection), "allow", "deny",
--- "ask" or a vote table.
---@param point string
---@param fn fun(...): mare.PolicyVote|"allow"|"ask"|"deny"|nil
---@param opts? mare.PolicyRegisterOptions
---@return fun() dispose
function PolicyService:voter(point, fn, opts) end

--- Register a rewriter for rewrite point `point` (`tool/rewrite`): it gets a copy of the call with the
--- arguments so far and returns nil (keep) or a table of new arguments.
---@param point string
---@param fn fun(call: table): table?
---@param opts? mare.PolicyRewriteOptions
---@return fun() dispose
function PolicyService:rewriter(point, fn, opts) end

--- Decide `point` over `args`: the voters in order, the first deny stops, an error, timeout or invalid
--- vote is a deny. Returns the folded decision `{decision, reason?, reasons, grant?, ...}`.
---@param point string
---@param args any[]
---@param options? {require_explicit_approval?: boolean}
---@return table
function PolicyService:decide(point, args, options) end

--- Rewrite `call` through `point`'s rewriters: returns the rewritten `arguments` table directly,
--- or raises a structured `mare.Error` (`policy.rewrite_refused`, with `message` set to the
--- failure reason) when any rewriter refuses, times out, or returns a non-table value.
---@param point string
---@param call table
---@return table arguments
function PolicyService:rewrite(point, call) end

--- The policy epoch: it moves when what a decision would answer may change (an approval's fingerprint).
---@return integer
function PolicyService:epoch() end

--- Whether any voter (rewriter) takes part in `point` now.
---@param point string
---@return boolean
function PolicyService:has_voters(point) end

--- `config.prims.policy` of host plugins (never a global or a module).
---@class mare.host.Policy
local Policy = {}

--- The fail-closed fold of `point` (Kernel::policy_with: the service's voters through `attach`).
---@param point string
---@param options? {require_explicit_approval?: boolean}
---@param ... any the decision's arguments
---@return table
function Policy.decide(point, options, ...) end

--- The strict rewrite chain of `point` (Kernel::rewrite).
---@param point string
---@param call table
---@return boolean ok
---@return any arguments_or_reason
function Policy.rewrite(point, call) end

--- Hand Rust the service's live lists: `lists(kind, point)` -> `{ {fn, owner, timeout_ms?}, ... }`
--- (`kind` = "voters" | "rewriters"); nil detaches.
---@param lists? fun(kind: string, point: string): table?
function Policy.attach(lists) end

--- A voter or rewriter came or went: the policy epoch (an approval's fingerprint) moves.
function Policy.changed() end

--- The service's decision points and rewrite points (one list with Rust's policy epoch).
---@type string[]
Policy.points = {}
---@type string[]
Policy.rewrite_points = {}

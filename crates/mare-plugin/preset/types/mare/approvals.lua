---@meta _
-- LuaLS definitions: native `mare.approvals` primitives and approval data types (entry: types/mare.lua).

---@alias mare.ApprovalPolicy "ask"|"never"|"untrusted"|{mode: "granular",categories: table<string,boolean>}
---@class mare.ApprovalBinding
---@field owner string service incarnation
---@field turn string current agent turn or manual invocation
---@field request string unique approval request nonce
---@field snapshot string exact proposed action
---@class mare.HumanApprovalReceipt
---@field token string native-issued, single use evidence; never place in events or persistence
---@field binding mare.ApprovalBinding
---@field decision string native signed affirmative decision
---@class mare.approvals
mare.approvals = {}
--- Check native human evidence without consuming it. Lua cannot issue evidence.
---@param token? string
---@param binding mare.ApprovalBinding
---@param arguments? table exact projected tool arguments; nil for a non-tool request
---@param decision string exact requested approval decision
---@return boolean
function mare.approvals.check_human(token, binding, arguments, decision) end
--- Consume native evidence once, after matching all owner/turn/request/snapshot fields.
---@param token? string
---@param binding mare.ApprovalBinding
---@param arguments table exact projected arguments
---@param revision? integer request native restoration authority for these exact hidden inputs
---@param decision string actual native affirmative decision
---@return boolean
---@return userdata? projection_authorization opaque, one-shot; keep private to the execution context
function mare.approvals.consume_human(token, binding, arguments, revision, decision) end
---@class mare.ApprovalSnapshotLease
local ApprovalSnapshotLease = {}
function ApprovalSnapshotLease:release() end
---@param binding mare.ApprovalBinding
---@param action table
---@return mare.ApprovalSnapshotLease lease opaque native lease; call :release() or bind <close>
function mare.approvals.register_snapshot(binding, action) end
---@class mare.ApprovalAuditCall
---@field tool_call_id string
---@field name string
---@class mare.ApprovalAudit
---@field call mare.ApprovalAuditCall identity only, never execution arguments or schema
---@field decision string
---@field source string fixed audit provenance
---@field outcome? "timeout"|"cancelled"
---@field scope? "turn"|"session"
---@field child_approval_id? integer
--- Project a full local result into the closed public audit DTO. No free-form reason,
--- arguments, schema, permissions, amendment, snapshot, or receipt is exposed.
---@param result table
---@return mare.ApprovalAudit
function mare.approvals.public_result(result) end
---@class mare.ApprovalResult
---@field decision "allow"|"allow_session"|"allow_amend"|"deny"|"abort"|"unavailable"
---@field reason? string
---@field source? string display provenance; never an authorization credential
---@field human_approval? string transient native UI/RPC answer evidence; consumed at the final execution boundary
---@field outcome? "timeout"|"cancelled"
---@field permissions? mare.PermissionProfile user/RPC answer subset, never a wider grant
---@field permission_constraints? mare.PermissionProfile[] intersected approval votes
---@field scope? "turn"|"session" user/RPC grant lifetime; request remains an upper bound

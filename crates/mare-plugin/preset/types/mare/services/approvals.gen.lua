---@meta _
-- GENERATED from types/mare/services/contracts/approvals.lua (service `approvals`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ApprovalsService
local M = {}

--- Validate the selected reviewer within the configured review timeout.
---@param cancel? mare.Token
---@return boolean?
---@return string?
function M:admit(cancel) end

--- Services that delegation requires even when a child changes provider configuration.
---@return string[]
function M:delegation_services() end

--- Evaluate the independent policy gate for a category; returns a denial or nil.
---@param category string
---@return mare.ApprovalResult?
function M:gate(category) end

--- All ancestor policy ceilings plus this session's live policy, copied for delegation.
---@return mare.ApprovalPolicy[]
function M:inheritance() end

--- Current effective approval policy.
---@return mare.ApprovalPolicy
function M:policy() end

--- Register a named approval reviewer owned by the calling ctx; automatically withdrawn on dispose.
---@param name string
---@param review async fun(action: table, cancel?: mare.Token): {decision: "allow"|"deny", reason?: string}
---@param opts? {admit?: (async fun(cancel?: mare.Token): boolean, string?)?}
---@return fun() dispose releases this registration
function M:register_reviewer(name, review, opts) end

--- Apply the independent gate, selected reviewer, cancellation and audit lifecycle for an approval request.
---@param category string
---@param action table
---@param policy async fun(binding: mare.ApprovalBinding): table
---@param answer async fun(binding: mare.ApprovalBinding): mare.ApprovalResult
---@param cancel? mare.Token
---@param opts? {approval_id?: string, child_approval_id?: integer, emit_resolved?: boolean}
---@return mare.ApprovalResult
---@return mare.HumanApprovalReceipt?
function M:request(category, action, policy, answer, cancel, opts) end

--- Whether every request requires review under current configuration or inherited flags.
---@return boolean
function M:review_required() end

--- Monotonic revision counter incremented whenever policy or reviewers change.
---@return integer
function M:revision() end

--- Persist a session override before changing the effective approval policy.
---@param value ("ask"|"never"|"untrusted")|{categories: table<string, boolean>, mode: "granular"}
---@return mare.ApprovalPolicy
function M:set(value) end

--- Reload-safe service incarnation plus policy/reviewer revision.
---@return string
function M:version() end

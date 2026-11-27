---@meta _
-- GENERATED from types/mare/services/contracts/permissions.lua (service `permissions`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.PermissionsService
local M = {}

--- Add a rule source owned by the calling plugin (removed when the caller is disposed).
---@param rules table<string, any>[]
---@return table
function M:add_rules(rules) end

--- Evaluate the vote the permissions plugin casts for a tool call (for UIs and tests).
---@param call table<string, any>
---@param info? {side_effect?: string, unconfined?: boolean, [string]: any}
---@return {amendment?: table<string, any>, decision: "allow"|"ask"|"deny", reason?: string, sandbox?: any, [string]: any}
function M:check(call, info) end

--- Recheck and commit a previously resolved structured permission request immediately before execution.
---@param call table<string, any>
---@return boolean|nil
function M:commit_request(call) end

--- Return the display metadata of a built-in or configured preset, or nil if unknown.
---@param name string
---@return ({desc?: string, label?: string, level: string, like: "read-only"|"auto"|"full-access", [string]: any})|nil
function M:describe(name) end

--- Describe a frozen call's structured permission request without granting it.
---@param call table<string, any>
---@param expected? string
---@return mare.PermissionProfile|nil
---@return "turn"|"session"|nil
function M:describe_request(call, expected) end

--- Current active permission preset and policy revision for this session (`rules` and `grants` are included for primary/in-process callers and omitted for peer callers).
---@return {desc?: string, grants?: {network?: boolean|string[], read?: string[], write?: string[], [string]: any}, label: string, level?: string, like?: "read-only"|"auto"|"full-access", partial: boolean, policy_revision: integer, preset: string, rules?: table<string, any>[]}
function M:get() end

--- Detached snapshot of this turn's and session's granted permission profiles.
---@return mare.PermissionProfile
function M:grants() end

--- Whether the active sandbox backend cannot enforce network restrictions requested by the preset.
---@return boolean
function M:is_partial() end

--- Available permission presets (built-in and configured) with display metadata and active indicator.
---@return {data: ({current: boolean, desc: string, label: string, level: string, like: "read-only"|"auto"|"full-access", name: string})[]}
function M:list() end

--- Monotonic revision counter incremented whenever permission policy or rules change.
---@return integer
function M:policy_revision() end

--- Current sandbox preset ("off" when the host policy is unavailable).
---@return string
function M:preset() end

--- List available permission preset names.
---@return string[]
function M:presets() end

--- Request structured permissions through the ordinary tool policy, approval, and execution chain.
---@param profile mare.PermissionProfile
---@param opts {justification: string, request_id?: string, scope?: "turn"|"session", approve?: function?, cancel?: mare.Token?, on_state?: function?}
---@return table
---@return table?
function M:request(profile, opts) end

--- Intersect approval constraints with a frozen permission request and current policy.
---@param call table<string, any>
---@param constraints? mare.PermissionProfile[]
---@param scope? "turn"|"session"
---@param expected? string
---@return mare.PermissionProfile|nil
---@return table|nil
---@return "turn"|"session"|nil
function M:resolve_request(call, constraints, scope, expected) end

--- What configured and remembered rules alone decide about a call (excluding session-only rules), or nil.
---@param call table<string, any>
---@param info? {side_effect?: string, unconfined?: boolean, [string]: any}
---@return {decision: "allow", sandbox: any}|{decision: "deny", reason?: string}|nil
function M:rule_verdict(call, info) end

--- Effective permission rules: `init.lua` rules, persisted rules, and session rules.
---@return table<string, any>[]
function M:rules() end

--- Switch the active permission preset (`confirm` asks before the first switch to full access); returns the updated permission state or raises a coded error.
---@param name string
---@param opts? {confirm?: boolean}
---@return {desc?: string, grants?: {network?: boolean|string[], read?: string[], write?: string[], [string]: any}, label: string, level?: string, like?: "read-only"|"auto"|"full-access", partial: boolean, policy_revision: integer, preset: string, rules?: table<string, any>[]}
function M:set(name, opts) end

---@meta _
-- LuaLS definitions: sub-agents (entry: types/mare.lua)

---@class mare.subagent
mare.subagent = {}
--- Low-level host primitive: start an independent child agent VM + thread directly.
--- Distinct from the `subagent` contract service's `ctx.subagent:start`, which layers agent
--- type lookup, tool ceilings, and session fork snapshots on top of this primitive.
--- Root config `agents.max_active` (default 16) bounds all live descendant threads, including
--- startup/shutdown; `agents.max_depth` (default 4) bounds edges from the root (zero disables
--- children). Descendants cannot override these limits. Admission errors carry structured codes
--- (`subagent.limit_reached`, `subagent.tree_closed`, `subagent.duplicate_name`, `invalid_arguments`).
--- cancel() cancels this child and its descendants. Host shutdown closes admission and waits
--- for all descendant cleanup; cancellation is cooperative, not forced OS-thread termination.
--- The sub-agent's prompt (its VM's agent layer): `persona` replaces the identity
--- (interpolated), `sections` adds or shadows sections ({name, order, text, tag?, raw?,
--- interpolate?, complete?}, text a string) for this sub-agent only, `system` is the whole prompt
--- (verbatim). `params` are the sub-agent's model-parameter selections (its agent layer, above
--- its config and below its own session): on the parent's current model it starts with the
--- parent session's values, `effort` = `params.effort`, `params` wins. The parent's parameter
--- rules (`mare.params{}`, `mare.model{ params = }`) reach it with their Lua functions.
--- In a saved, writable parent session, the host creates and writes a child transcript and assigns
--- its identity. This is a message transcript, not a recoverable tool-state journal: child
--- mode retains its no durable-start/ack gate and no crash-recovery semantics.
--- `run_id` and `seq` are optional grouping metadata, not binding credentials.
--- An ephemeral or read-only parent does not record a child transcript. A direct tool call
--- against an as-yet-unsaved writable parent fails; an ordinary agent turn saves the parent
--- before it starts its tools.
---@param opts {prompt: string, name?: string, run_id?: string, seq?: integer, label?: string, phase?: string, task?: string, model?: string, effort?: string, params?: table<string, any>, system?: string, persona?: string, sections?: table[], cwd?: string, tools?: string[], messages?: table[], config?: table, output_schema?: table}
---@return mare.SubagentHandle
function mare.subagent.start(opts) end
--- Close admission for the current session and wait for its children and lifecycle events.
---@async
function mare.subagent.drain_session() end
--- Whether the host's inherited tool ceiling permits this tool name.
---@param name string
---@return boolean
function mare.subagent.tool_allowed(name) end
---@class mare.SubagentHandle
---@field subagent_id fun(self: mare.SubagentHandle): string
---@field info fun(self: mare.SubagentHandle): mare.SubagentInfo
---@field next_event async fun(self: mare.SubagentHandle): {name: string, payload: any}|nil
---@field wait async fun(self: mare.SubagentHandle): {text: string, structured?: any, messages: table[], usage: table, error?: string, error_kind?: string}
---@field cancel fun(self: mare.SubagentHandle)
---@field steer fun(self: mare.SubagentHandle, text: string): boolean
---@class mare.SubagentInfo
---@field subagent_id string
---@field name? string
---@field label? string
---@field run_id? string
---@field seq? integer
---@field task? string
---@field parent_session_id? string
---@field status "running"|"completed"|"failed"|"cancelled"
---@field started_at_ms integer
---@field ended_at_ms? integer
---@field error? string

---@class mare.SubagentWaitResult
---@field subagent_id string
---@field status "running"|"completed"|"failed"|"cancelled"
---@field timed_out boolean
---@field cancelled? boolean
---@field text? string
---@field messages? table[]
---@field usage? table
---@field error? string

--- List subagents spawned by the current parent session.
---@return mare.SubagentInfo[]
function mare.subagent.list() end
--- Get a subagent by subagent_id or unique name in the current parent session.
---@param id string
---@return mare.SubagentInfo|nil
function mare.subagent.get(id) end
--- Wait for a subagent to finish (non-consuming; timeout/cancel does not cancel the child).
---@async
---@param id string
---@param opts? {timeout_ms?: integer, cancel?: mare.Token}
---@return mare.SubagentWaitResult
function mare.subagent.wait(id, opts) end
--- Steer a running subagent.
---@param id string
---@param text string
---@return boolean
function mare.subagent.steer(id, text) end
--- Cancel a subagent by subagent_id or name.
---@param id string
---@return boolean
function mare.subagent.cancel(id) end
--- A host-owned binding in the current parent session. For display association only.
---@param subagent_id string
---@return {subagent_id: string, parent_session_id: string, parent_session_epoch: string}|nil
function mare.subagent.identity(subagent_id) end
--- What of a config value would not reach a sub-agent's VM: "<function site>: <reason>" for each
--- function inside it that can't be carried (it captures a userdata, a thread or a built-in the
--- globals don't reach). `mare doctor` uses it for the parameter rules.
---@param value any
---@return string[]
function mare.subagent.uncarried(value) end
--- The sub-agent rule the parent's approval dialog showed and the user chose to remember, once,
--- by the `rule_token` of its `pipeline/approval-decided` event (the permissions plugin persists it).
---@param token integer
---@return table|nil rule
function mare.subagent.take_rule(token) end
--- In a sub-agent sharing its parent's permission configuration (the tools service calls it):
--- lift the inherited ceiling for this allowed call when the parent's rules allow it too.
---@async
---@param call table the frozen call (id, approval_key, name, arguments, cwd)
---@param info table
---@return boolean lifted
function mare.subagent.rule_lift(call, info) end
--- Apply host-owned scoped grants to a frozen call's ceiling. No caller-supplied grant is trusted.
---@param call table
---@return boolean lifted
function mare.subagent.permission_lift(call) end
--- Commit the intersection of a pending host approval and the effective permissions.
--- No matching pending approval means no grant; turn/session lifetime remains host-owned.
---@param call table
---@param permissions mare.PermissionProfile
---@param scope? "turn"|"session"
function mare.subagent.permission_commit(call, permissions, scope) end

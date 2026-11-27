---@meta _
-- GENERATED from types/mare/services/contracts/jobs.lua (service `jobs`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.JobsService
local M = {}

--- Mark a job as backgrounded so completion emits a notification.
---@param job_id string
---@return table
function M:background(job_id) end

--- Request a running job to stop (SIGTERM, then SIGKILL after kill_grace_ms).
---@param job_id string
---@param reason? string
---@param opts? {silent?: boolean}
---@return table
function M:cancel(job_id, reason, opts) end

--- Look up a tracked job by id (optionally scoped to `owner`), or nil.
---@param job_id string
---@param owner? string
---@return mare.JobInfo|nil
function M:get(job_id, owner) end

--- Background jobs in start order, optionally filtered by owner (with optional cursor pagination).
---@param opts? {cursor?: string, limit?: integer, owner?: string}
---@return {data: ({background?: boolean, description?: string, ended_at_ms?: integer, exit_code?: integer, exit_signal?: integer, job_id: string, kill_reason?: string, kill_requested?: boolean, kind: string, label?: string, over_limit?: integer, owner: string, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", stop_unconfirmed?: boolean, [string]: any})[], next_cursor?: string}
function M:list(opts) end

--- Non-consumingly read job output starting at byte `offset`, with a trailing [status: ...] line.
---@param job_id string
---@param opts? {max_bytes?: integer, offset?: integer}
---@return {done?: boolean, dropped?: integer, next_offset?: integer, text: string}
function M:read(job_id, opts) end

--- Start and track a background process job.
---@param opts {args?: string[], background?: boolean, cmd: string, cwd?: string, description?: string, env?: table<string, any>, kind?: string, owner?: string, sandbox?: table<string, any>, spill?: string, [string]: any}
---@return mare.JobInfo|nil
---@return string?
function M:start(opts) end

--- Track an external handle implementing the `mare.Job` interface (sub-agents, terminals) as a job.
---@param opts {background?: boolean, description?: string, kind: string, owner?: string, handle?: table, [string]: any}
---@return mare.JobInfo
function M:track(opts) end

--- Write stdin data to a running job (optionally closing stdin with `eof = true`).
---@param job_id string
---@param data? string
---@param opts? {eof?: boolean}
---@return boolean
---@return string?
function M:write(job_id, data, opts) end

---@meta _
-- GENERATED from types/mare/services/contracts/proc.lua (service `proc`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ProcService
local M = {}

--- Terminate an owned running process and its process group.
---@param opts {process_id: string}
---@return table
function M:kill(opts) end

--- Resize the PTY of an owned process started with tty = true.
---@param opts {process_id: string, size: {cols: integer, rows: integer}}
---@return table
function M:resize(opts) end

--- Start a sandboxed piped or PTY child process bound to the caller lifecycle.
---@param opts {command: string[], cwd?: string, env?: table<string, string>, process_id: string, size?: {cols: integer, rows: integer}, tty?: boolean, on_exit?: (fun(ev: {process_id: string, exit_code?: integer, output_drain_timed_out?: boolean})), on_output?: (fun(ev: {process_id: string, stream: "stdout"|"stderr"|"pty", data_base64: string}))}
---@return {process_id: string}
function M:spawn(opts) end

--- Write base64 bytes to stdin or PTY of an owned process, optionally closing piped stdin.
---@param opts {close_stdin?: boolean, data_base64?: string, process_id: string}
---@return table
function M:write(opts) end

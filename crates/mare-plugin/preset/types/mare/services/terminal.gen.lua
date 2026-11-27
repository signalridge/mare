---@meta _
-- GENERATED from types/mare/services/contracts/terminal.lua (service `terminal`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.TerminalService
local M = {}

--- Stop and close an owned PTY session cleanly.
---@param opts {grace_ms?: integer, terminal_id: string}
---@return mare.TerminalSessionSnapshot
function M:close(opts) end

--- Create an owner-bound PTY session; automatically closed when the caller context or RPC connection ends.
---@param opts? {args?: string[], buffer_bytes?: integer, cmd?: string, cols?: integer, cwd?: string, emulate?: boolean, env?: table<string, string>, name?: string, rows?: integer, scrollback_lines?: integer}
---@return mare.TerminalSessionSnapshot
function M:create(opts) end

--- List PTY sessions owned by the caller connection or plugin.
---@return {data: mare.TerminalSessionSnapshot[]}
function M:list() end

--- Read incremental `output_offset` or VT-paged output from an owned PTY, capped at `max_bytes` bytes.
---@param opts {count?: integer, max_bytes?: integer, offset?: integer, output_offset?: integer, screen?: boolean, terminal_id: string, wait_ms?: integer}
---@return {alternate_screen?: boolean, cols: integer, command_exit_code?: integer, exit_code?: integer, exit_signal?: integer, line_begin: integer, line_end: integer, name?: string, next_output_offset: integer, output_offset: integer, pid?: integer, prompt_seq?: integer, rows: integer, screen?: string, status: "running"|"completed"|"failed"|"cancelled", terminal_id: string, text: string, total_lines: integer, truncated: boolean, view_truncated?: boolean}
function M:read(opts) end

--- Resize an owned PTY window.
---@param opts {cols: integer, rows: integer, terminal_id: string}
---@return table
function M:resize(opts) end

--- Send input bytes/text to an owned PTY and optionally wait for output/exit, capped at `max_bytes` bytes.
---@param opts {max_bytes?: integer, screen?: boolean, submit?: boolean, terminal_id: string, text?: string, wait_ms?: integer}
---@return {alternate_screen?: boolean, cols: integer, command_exit_code?: integer, exit_code?: integer, exit_signal?: integer, name?: string, next_output_offset: integer, output: string, output_offset: integer, pid?: integer, prompt_seq?: integer, rows: integer, screen?: string, status: "running"|"completed"|"failed"|"cancelled", terminal_id: string, timed_out: boolean, truncated: boolean, view_truncated?: boolean}
function M:send(opts) end

--- Deliver a signal to the foreground process group of an owned PTY.
---@param opts {signal: "SIGINT"|"SIGTERM"|"SIGKILL"|"SIGTSTP"|"SIGHUP", terminal_id: string}
---@return table
function M:signal(opts) end

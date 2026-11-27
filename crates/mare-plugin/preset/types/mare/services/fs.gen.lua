---@meta _
-- GENERATED from types/mare/services/contracts/fs.lua (service `fs`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.FsService
local M = {}

--- List directory entries filtered by the active sandbox read policy.
---@param path string
---@return {data: {file_name: string, is_directory: boolean, is_file: boolean}[]}
function M:list(path) end

--- Read a file as base64, bounded by active sandbox policy and optional max_bytes.
---@param path string
---@param opts? {max_bytes?: integer}
---@return {data_base64: string}
function M:read(path, opts) end

--- Inspect file metadata within the active sandbox read policy.
---@param path string
---@return {created_at_ms: integer, is_directory: boolean, is_file: boolean, mode?: integer, modified_at_ms: integer, nlink?: integer, size: integer}
function M:stat(path) end

--- Watch a workspace path for policy-filtered filesystem changes (emits `fs/changed`; release via `registrations.release`).
---@param path string
---@return fun() dispose releases this registration
function M:watch(path) end

--- Write base64 bytes to a file within writable sandbox roots and protected-path exclusions.
---@param path string
---@param data_base64 string
---@param opts? {atomic?: boolean, create_new?: boolean}
---@return table
function M:write(path, data_base64, opts) end

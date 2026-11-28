---@meta _
-- GENERATED from types/mare/services/contracts/commands.lua (service `commands`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.CommandsService
local M = {}

--- Current definition data of command `name` (or alias), or nil if absent.
---@param name string
---@return {alias_of?: string, args?: string, auto_execute?: boolean, availability?: ("always"|"idle"|"session"|"main"|"running"|"tui")|(("always"|"idle"|"session"|"main"|"running"|"tui")[]), description?: string, local_only?: boolean, name: string}|nil
function M:get(name) end

--- Registered non-alias slash commands, sorted by name (data only, no callbacks). Pass `available = true` to filter to commands executable right now for the active frontend and turn state.
---@param opts? {allow_local?: boolean, available?: boolean}
---@return {data: ({alias_of?: string, args?: string, auto_execute?: boolean, availability?: ("always"|"idle"|"session"|"main"|"running"|"tui")|(("always"|"idle"|"session"|"main"|"running"|"tui")[]), description?: string, local_only?: boolean, name: string})[]}
function M:list(opts) end

--- Register a slash command or alias (`alias_of`) owned by the caller; remote callers must pass `replace = true` to overwrite an existing name, and releasing a registration restores any previous definition.
---@param name string
---@param opts? {alias_of?: string, args?: string, auto_execute?: boolean, availability?: ("always"|"idle"|"session"|"main"|"running"|"tui")|(("always"|"idle"|"session"|"main"|"running"|"tui")[]), description?: string, local_only?: boolean, replace?: boolean, complete?: (fun(prefix: string): table[]), run?: (fun(args: string, input?: mare.CommandInput): any)}
---@return fun() dispose releases this registration
function M:register(name, opts) end

--- Run a slash command line (optionally targeting a loaded `session_id`); returns {result?} on success and throws a coded error on failure.
---@param line string
---@param input? {content?: {data?: string, mime?: string, name?: string, path?: string, text?: string, type: string, url?: string, [string]: any}[], [string]: any}
---@param opts? {session_id?: string}
---@return {result?: any}
function M:run(line, input, opts) end

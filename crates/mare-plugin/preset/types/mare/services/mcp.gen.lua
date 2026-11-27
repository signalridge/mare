---@meta _
-- GENERATED from types/mare/services/contracts/mcp.lua (service `mcp`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.McpService
local M = {}

--- Prepare/discover one exposure tier using lazy connection and tool registration (default `codemode`).
---@param tier? "direct"|"deferred"|"codemode"
---@return {description?: string, name: string, parameters: table<string, any>, server: string, [string]: any}[]
function M:catalog(tier) end

--- Connect one configured MCP server and return its status.
---@param server string
---@return mare.McpServerStatus
function M:connect(server) end

--- Expand a prompt on a configured MCP server.
---@param server string
---@param name string
---@param arguments? table<string, string>
---@return {description?: string, messages: table<string, any>[]}
function M:get_prompt(server, name, arguments) end

--- Prompts exposed by connected MCP servers (or one named server).
---@param opts? {connect?: boolean, server?: string}
---@return {data: table<string, any>[]}
function M:list_prompts(opts) end

--- Resources and resource templates exposed by connected MCP servers (or one named server).
---@param opts? {connect?: boolean, server?: string}
---@return {data: table<string, any>[], resource_templates: table<string, any>[]}
function M:list_resources(opts) end

--- Configured MCP servers and their current connection status (never connects).
---@return {data: mare.McpServerStatus[], errors: string[]}
function M:list_servers() end

--- Tools discovered from configured MCP servers (or one named server).
---@param opts? {connect?: boolean, server?: string}
---@return {data: {description?: string, input_schema?: any, name?: string, registered_name?: string, schema_error?: string, server: string, tier?: string}[]}
function M:list_tools(opts) end

--- Read one MCP resource URI from a server.
---@param server string
---@param uri string
---@return {contents: table<string, any>[]}
function M:read_resource(server, uri) end

--- Reconnect one configured MCP server and return its status.
---@param server string
---@return mare.McpServerStatus
function M:reconnect(server) end

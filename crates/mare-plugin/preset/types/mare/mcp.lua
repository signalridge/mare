---@meta _
-- LuaLS definitions: MCP (Model Context Protocol) client, OAuth & service (entry: types/mare.lua)

---@class mare.McpInfo
---@field protocolVersion? string
---@field capabilities? table
---@field serverInfo? {name?: string, version?: string}

---@class mare.McpTool
---@field name? string
---@field description? string
---@field inputSchema? table
---@field schema_error? string set when the tool's schema could not be losslessly represented in Lua

---@class mare.McpResource
---@field uri string
---@field name? string
---@field description? string
---@field mimeType? string

---@class mare.McpResourceTemplate
---@field uriTemplate string
---@field name? string
---@field description? string
---@field mimeType? string

---@class mare.McpPromptArg
---@field name string
---@field description? string
---@field required? boolean

---@class mare.McpPrompt
---@field name string
---@field description? string
---@field arguments? mare.McpPromptArg[]

---@class mare.McpCallOpts
---@field timeout_ms? integer
---@field cancel? mare.Token
---@field progress? fun(p: table)

---@class mare.McpCallResult
---@field content table[]
---@field isError? boolean
---@field structuredContent? any

---@class mare.McpEvent
---@field type "tools_changed"|"prompts_changed"|"resources_changed"|"reconnected"|"elicit"
---@field id? integer server JSON-RPC request id (when `type == "elicit"`)
---@field params? table elicitation payload (when `type == "elicit"`)
---@field cancel? mare.Token cancelled if the server cancels the elicitation or the client closes

---@class mare.McpOAuthOpts
---@field client_id? string
---@field scopes? string[]
---@field authorization_url? string
---@field token_url? string

---@class mare.McpSpec
---@field command? string
---@field args? string[]
---@field env? table<string, string>
---@field cwd? string
---@field sandbox? table|string
---@field env_include? string[]
---@field env_exclude? string[]
---@field url? string
---@field headers? table<string, string>
---@field oauth? boolean|mare.McpOAuthOpts
---@field name? string
---@field timeout_ms? integer
---@field call_timeout_ms? integer
---@field idle_timeout_ms? integer
---@field reconnects? integer

---@class mare.McpClient
local McpClient = {}
---@async
---@return mare.McpInfo
function McpClient:info() end
---@async
---@return "connected"|"disconnected"|"closed"
function McpClient:status() end
---@return integer
function McpClient:tools_generation() end
---@return string[]
function McpClient:stderr() end
---@async
---@return mare.McpTool[] tools, integer generation
function McpClient:tools() end
---@async
function McpClient:refresh() end
---@async
---@return mare.McpResource[]
function McpClient:resources() end
---@async
---@return mare.McpResourceTemplate[]
function McpClient:resource_templates() end
---@async
---@return mare.McpPrompt[]
function McpClient:prompts() end
---@async
---@param name string
---@param arguments? table
---@param opts? mare.McpCallOpts
---@return mare.McpCallResult
function McpClient:call(name, arguments, opts) end
---@async
---@param uri string
---@param opts? mare.McpCallOpts
---@return {contents: {uri?: string, mimeType?: string, text?: string, blob?: string}[]}
function McpClient:read_resource(uri, opts) end
---@async
---@param name string
---@param arguments? table<string, string>
---@param opts? mare.McpCallOpts
---@return {description?: string, messages: {role: string, content: table}[]}
function McpClient:get_prompt(name, arguments, opts) end
---@async
---@return mare.McpEvent|nil
function McpClient:next_event() end
---@param id integer|string
---@param result table
function McpClient:respond(id, result) end
---@async
function McpClient:reconnect() end
---@async
function McpClient:close() end

---@class mare.McpOAuthLogin
---@field url string
---@field redirect string local callback redirect URI
local OAuthLogin = {}
---@async
---@param timeout_ms? integer
function OAuthLogin:finish(timeout_ms) end
---@param url string full redirected URL pasted by the user (e.g. over SSH)
function OAuthLogin:paste(url) end

---@class mare.mcp
---@field log_path string path to the MCP log file
mare.mcp = {}
---@async
---@param spec mare.McpSpec
---@param cancel? mare.Token
---@return mare.McpClient
function mare.mcp.connect(spec, cancel) end
---@async
---@param url string
---@param key string server name / credential key
---@param opts? mare.McpOAuthOpts
---@return mare.McpOAuthLogin
function mare.mcp.oauth_begin(url, key, opts) end
---@param key string
---@param url? string
---@return boolean removed
function mare.mcp.logout(key, url) end
---@param key string
---@param url string
---@return boolean
function mare.mcp.has_credentials(key, url) end
---@param data string
---@param ext? string
---@return string path
function mare.mcp.spill(data, ext) end
---@param server string
---@param level string
---@param msg string
function mare.mcp.log(server, level, msg) end
---@param s string
---@return string
function mare.mcp.redact(s) end

---@class mare.McpServerStatus
---@field name string
---@field state string
---@field source? string
---@field tier? string
---@field transport? string
---@field target? string
---@field tools integer
---@field error? string
---@field missing? string[]

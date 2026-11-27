---@meta _
-- The `mcp` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, map, list_of = S.object, S.integer, S.string, S.boolean, S.map, S.list_of
local mcp_server_status = object({
	name = string,
	state = string,
	source = string,
	tier = string,
	transport = string,
	target = string,
	tools = integer,
	error = string,
	missing = { type = "array", items = string },
}, { "name", "state", "tools" })
mcp_server_status.title = "mare.McpServerStatus"
local mcp_tool = object({
	server = string,
	name = string,
	registered_name = string,
	description = string,
	input_schema = {},
	tier = string,
	schema_error = string,
}, { "server" })
local filter_params = object({ server = string, connect = boolean })
local server_params = object({ server = string }, { "server" })
local open_object = { type = "object", additionalProperties = true }
return {
	["mcp"] = {
		doc = "Model Context Protocol servers, tools, resources and prompts.",
		errors = {
			unknown_server = { doc = "no MCP server with that name" },
			connect_failed = { doc = "the MCP server is blocked or failed to connect" },
			request_failed = { doc = "the MCP server rejected or failed the request" },
		},
		methods = {
			list_servers = {
				doc = "Configured MCP servers and their current connection status (never connects).",
				expose = "both",
				access = "peer",
				params = object({}),
				order = {},
				result = object({
					data = { type = "array", items = mcp_server_status },
					errors = { type = "array", items = string },
				}, { "data", "errors" }),
			},
			list_tools = {
				doc = "Tools discovered from configured MCP servers (or one named server).",
				expose = "both",
				access = "peer",
				params = filter_params,
				order = { "..." },
				result = list_of(mcp_tool),
				errors = { "unknown_server" },
			},
			connect = {
				doc = "Connect one configured MCP server and return its status.",
				expose = "both",
				access = "primary",
				admin = true,
				params = server_params,
				order = { "server" },
				result = mcp_server_status,
				errors = { "unknown_server", "connect_failed" },
			},
			reconnect = {
				doc = "Reconnect one configured MCP server and return its status.",
				expose = "both",
				access = "primary",
				admin = true,
				params = server_params,
				order = { "server" },
				result = mcp_server_status,
				errors = { "unknown_server", "connect_failed" },
			},
			list_resources = {
				doc = "Resources and resource templates exposed by connected MCP servers (or one named server).",
				expose = "both",
				access = "peer",
				params = filter_params,
				order = { "..." },
				result = object({
					data = { type = "array", items = open_object },
					resource_templates = { type = "array", items = open_object },
				}, { "data", "resource_templates" }),
				errors = { "unknown_server" },
			},
			read_resource = {
				doc = "Read one MCP resource URI from a server.",
				expose = "both",
				access = "peer",
				params = object({ server = string, uri = string }, { "server", "uri" }),
				order = { "server", "uri" },
				result = object({ contents = { type = "array", items = open_object } }, { "contents" }),
				errors = { "unknown_server", "connect_failed", "request_failed" },
			},
			list_prompts = {
				doc = "Prompts exposed by connected MCP servers (or one named server).",
				expose = "both",
				access = "peer",
				params = filter_params,
				order = { "..." },
				result = list_of(open_object),
				errors = { "unknown_server" },
			},
			get_prompt = {
				doc = "Expand a prompt on a configured MCP server.",
				expose = "both",
				access = "peer",
				params = object({ server = string, name = string, arguments = map(string) }, { "server", "name" }),
				order = { "server", "name", "arguments" },
				result = object({
					description = string,
					messages = { type = "array", items = open_object },
				}, { "messages" }),
				errors = { "unknown_server", "connect_failed", "request_failed" },
			},
			catalog = {
				doc = "Prepare/discover one exposure tier using lazy connection and tool registration (default `codemode`).",
				params = object({
					tier = { type = "string", enum = { "direct", "deferred", "codemode" } },
				}),
				order = { "tier" },
				result = {
					type = "array",
					items = object({
						name = string,
						server = string,
						description = string,
						parameters = open_object,
					}, { "name", "server", "parameters" }, true),
				},
			},
		},
		events = {
			["changed"] = {
				doc = "An MCP server's resources, prompts, or connection state changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = object({ server = string }, { "server" }),
			},
			["cli"] = {
				doc = "Internal CLI /mcp subcommand dispatch.",
				mode = "emit",
				expose = "process",
				data = { type = "object", additionalProperties = true },
			},
			["codemode"] = {
				doc = "MCP server exposed code-mode tools for registration.",
				mode = "emit",
				expose = "lua",
				data = object({
					server = string,
					tools = { type = "array", items = open_object },
				}, { "server", "tools" }),
			},
			["elicit"] = {
				doc = "MCP server requested interactive user elicitation.",
				mode = "bail",
				expose = "process",
				data = object({ server = string }, { "server" }, true),
			},
			["error"] = {
				doc = "An MCP server failed or reported a connection/refresh error.",
				mode = "emit",
				expose = "lua",
				data = object({ server = string, error = string }, { "server", "error" }),
			},
			["ready"] = {
				doc = "An MCP server connected and exposed its direct tools.",
				mode = "emit",
				expose = "lua",
				data = object({
					server = string,
					tools = { type = "array", items = string },
				}, { "server", "tools" }),
			},
		},
	},
}

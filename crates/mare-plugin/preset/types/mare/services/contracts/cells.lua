---@meta _
-- The `cells` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, page_of = S.object, S.integer, S.string, S.boolean, S.page_of

local cell_status = { type = "string", enum = { "running", "completed", "failed", "cancelled" } }
local cell_result = object({
	cell_id = string,
	status = cell_status,
	content = { type = "array", items = {} },
	result = {},
	error = string,
	stop_unconfirmed = boolean,
}, { "cell_id", "status", "content" })
cell_result.title = "mare.CellResult"

local cell_summary = object({
	cell_id = string,
	status = cell_status,
}, { "cell_id", "status" })

local catalog_entry = object({
	name = string,
	description = string,
	parameters = { type = "object" },
	output_schema = { type = "object" },
	declaration = string,
}, { "name", "description", "parameters", "output_schema", "declaration" })

local status_result = object({
	running = integer,
	stored_keys = integer,
	retains_state = boolean,
	store_durable = boolean,
}, { "running", "stored_keys", "retains_state", "store_durable" })

local timeout_ms = { oneOf = { { type = "integer" }, { type = "boolean", const = false } } }

return {
	["cells"] = {
		doc = "Controlled Lua code cells and capability catalog. Cells and retained VM state belong to the caller.",
		errors = {
			unknown_cell = { doc = "no cell with that cell_id belongs to this caller" },
			duplicate_cell = { doc = "a cell with that cell_id is already active for this caller" },
			cell_limit_reached = { doc = "active cell limit reached; wait for or terminate an existing cell" },
			stop_unconfirmed = { doc = "active cell cancellation or retained worker cleanup did not confirm in time" },
		},
		methods = {
			start = {
				doc = "Start an asynchronous script cell in this caller's scope and wait up to yield_time_ms for initial output or completion. Lua callers may pass `cancel` / `wait_cancel` returning their cancel token.",
				expose = "both",
				access = "primary",
				admin = true,
				scope = "session",
				params = object({
					code = string,
					cell_id = string,
					capabilities = { type = "array", items = string },
					timeout_ms = timeout_ms,
					yield_time_ms = integer,
					max_output_tokens = integer,
					state_owner = string,
				}, { "code" }),
				order = { "code", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil", wait_cancel = "fun(): mare.Token|nil" },
				result = cell_result,
				errors = { "duplicate_cell", "cell_limit_reached" },
			},
			wait = {
				doc = "Wait for an active or completed cell owned by this caller, consuming newly emitted output. terminate cancels the cell and waits up to 5 s. Lua callers may pass `cancel` returning their cancel token.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({
					cell_id = string,
					yield_time_ms = integer,
					max_output_tokens = integer,
					terminate = boolean,
				}, { "cell_id" }),
				order = { "cell_id", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil" },
				result = cell_result,
				errors = { "unknown_cell" },
			},
			cancel = {
				doc = "Cancel an active cell owned by this caller.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({ cell_id = string }, { "cell_id" }),
				order = { "cell_id" },
				result = object({}),
				errors = { "unknown_cell" },
			},
			list = {
				doc = "List cells owned by this caller.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = S.page_params,
				order = { "..." },
				result = page_of(cell_summary),
			},
			get = {
				doc = "Status of the caller's cell environment.",
				expose = "both",
				access = "peer",
				scope = "session",
				result = status_result,
			},
			reset = {
				doc = "Cancel active cells owned by this caller and release caller-owned retained VM state.",
				expose = "both",
				access = "primary",
				scope = "session",
				result = object({}),
				errors = { "stop_unconfirmed" },
			},
			list_tools = {
				doc = "Discover tools currently callable from code cells, including the MCP codemode tier.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = S.page_params,
				order = { "..." },
				result = page_of(catalog_entry),
			},
		},
	},
}

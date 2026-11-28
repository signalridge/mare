---@meta _
-- Service contracts are independent of individual implementations. The kernel loads this
-- manifest before plugins run and validates each provided implementation.
--
-- Every contract is `{ doc, methods = { name = spec } }` (or an event-only root `{ owner, events }`).
-- Each method spec declares `doc`, `params` (JSON Schema, object root), `order` (positional Lua
-- arguments; a trailing "..." collects the remaining parameters into the final opts table), `result`
-- (JSON Schema, unless `registration = true`), `expose` = lua (default) | both,
-- `provider` = plugin (default) | host, `access` = primary (default) | peer | peer_default (remote only),
-- `admin` = true (requires admin privilege on remote RPC),
-- `scope` = host (default) | workspace | session, `registration` (Lua returns a disposer,
-- JSON callers get {registration_id}), `optional`. The kernel derives the Lua proxy guard, LuaLS types,
-- and the RPC adapter from it.
-- Files: schema.lua (shared helpers and types) and one file per service contract.
-- The host loads them as one manifest (mare_plugin::service_contracts_lua).
local function object(properties, required, open)
	return {
		type = "object",
		properties = properties,
		required = required,
		additionalProperties = open or false,
	}
end
local function merged(base, extra)
	local out = {}
	for k, v in pairs(base) do
		out[k] = v
	end
	for k, v in pairs(extra or {}) do
		out[k] = v
	end
	return out
end
local integer, number, string, boolean =
	{ type = "integer" }, { type = "number" }, { type = "string" }, { type = "boolean" }
local page_params_props = { cursor = string, limit = { type = "integer", minimum = 1 } }
local page_params = object(page_params_props)
local function list_of(items, extra)
	return object(merged({ data = { type = "array", items = items } }, extra), { "data" })
end
local function page_of(items, extra)
	return object(merged({ data = { type = "array", items = items }, next_cursor = string }, extra), { "data" })
end
local usage_totals = object({
	input = integer,
	output = integer,
	reasoning = integer,
	cache_read = integer,
	cache_write = integer,
	cache_write_1h = integer,
	cost = number,
	cost_unknown = boolean,
	requests = integer,
	subscription_requests = integer,
	side_requests = integer,
	last_context = integer,
	last_cache_read = integer,
}, {
	"input",
	"output",
	"cache_read",
	"cache_write",
	"cost",
	"requests",
	"subscription_requests",
	"side_requests",
	"last_context",
	"last_cache_read",
})
local function map(values)
	return { type = "object", additionalProperties = values }
end
-- Theme styles: definitions accept the known attributes only; results carry what the UI reports.
local theme_style = object({
	fg = string,
	bg = string,
	bold = boolean,
	dim = boolean,
	italic = boolean,
	underline = boolean,
	strike = boolean,
	reverse = boolean,
})
local theme_styles = map(theme_style)
local appearance = { type = "string", enum = { "dark", "light" } }
local theme_variant = object({ base = appearance, palette = map(string), styles = theme_styles, syntax = string })
local theme_definition = object({
	name = string,
	label = string,
	base = appearance,
	palette = map(string),
	styles = theme_styles,
	syntax = string,
	icons = map(string),
	variants = object({ dark = theme_variant, light = theme_variant }, { "dark", "light" }),
}, { "name" })
local theme_snapshot = object({
	preference = string,
	active = string,
	appearance = appearance,
	dark = string,
	light = string,
	preview = string,
	revision = integer,
	styles = map({ type = "object" }),
	syntax = string,
	component = {},
	icons = map(string),
}, { "preference", "active", "appearance", "dark", "light", "revision", "styles", "component", "icons" })
-- Lua declares it as mare.ThemeSnapshot (styles as mare.Style, component as mare.Node)
theme_snapshot.title = "mare.ThemeSnapshot"
local theme_descriptor = object({
	name = string,
	label = string,
	variants = { type = "array", items = appearance },
	origin = string,
}, { "name", "label", "variants", "origin" })
-- A model descriptor (llm:capabilities): identity, abilities, route and parameters.
local cost = object({ input = number, output = number, cache_read = number, cache_write = number })
local model_parameter = object({
	parameter_id = string,
	name = string,
	type = string,
	default = {},
	tooltip = string,
	cycleable = boolean,
	statusline = {},
	visible = {},
	values = { type = "array" },
	options = {
		type = "array",
		items = object(
			{ value = {}, name = string, tooltip = string, cost = { type = { "number", "object" } } },
			{ "value" }
		),
	},
}, { "parameter_id", "values", "options" })
local model = object({
	ref = string,
	model_id = string,
	provider = string,
	name = string,
	context_window = integer,
	max_output = integer,
	reasoning = boolean,
	tool_update = { type = "string", enum = { "addition_only" } },
	images = boolean,
	edit_tool = { type = "string", enum = { "apply_patch", "replace" } },
	cost = cost,
	route = string,
	wire_route = string,
	route_error = string,
	available = boolean,
	unavailable_reason = string,
	ready = boolean,
	ready_reason = string,
	efforts = { type = "array", items = string },
	default_effort = string,
	parameters = { type = "array", items = model_parameter },
}, {
	"ref",
	"model_id",
	"provider",
	"reasoning",
	"images",
	"available",
	"ready",
	"efforts",
	"parameters",
})
-- A provider's masked login & directory state (llm:auth_status / llm.list_providers rows); never a secret value.
local provider_status = object({
	provider = string,
	label = string,
	logged_in = boolean,
	no_key = boolean,
	method = string,
	account = string,
	expires_at_ms = integer,
	source = string,
	overridden = { type = "array", items = string },
	hint = string,
	state = { type = "string", enum = { "registered", "disabled", "failed", "unresolved" } },
	reason = string,
	plugin = string,
	origin = { type = "string", enum = { "plugin", "config", "rpc" } },
	wire = string,
}, { "provider", "logged_in", "overridden", "state" })
-- Provider snapshot from the llm/rate-limits event (primary?, secondary?, credits?, reached?, …)
-- plus the time it arrived; the provider decides the other fields.
-- one quota window of a subscription (provider-openai wire.lua parse_rate_limits)
local quota_window = object(
	{ used_percent = number, window_minutes = number, reset_at_ms = integer },
	{ "used_percent" },
	true
)
local rate_limit = object({
	received_at_ms = integer,
	primary = quota_window,
	secondary = quota_window,
	credits = string,
	reached = string,
	limit_name = string,
}, { "received_at_ms" }, true)
return {
	object = object,
	merged = merged,
	integer = integer,
	number = number,
	string = string,
	page_params_props = page_params_props,
	page_params = page_params,
	list_of = list_of,
	page_of = page_of,
	usage_totals = usage_totals,
	boolean = boolean,
	map = map,
	theme_style = theme_style,
	theme_styles = theme_styles,
	appearance = appearance,
	theme_variant = theme_variant,
	theme_definition = theme_definition,
	theme_snapshot = theme_snapshot,
	theme_descriptor = theme_descriptor,
	cost = cost,
	model_parameter = model_parameter,
	model = model,
	provider_status = provider_status,
	quota_window = quota_window,
	rate_limit = rate_limit,
}

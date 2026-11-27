---@meta _
-- The `pkg` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean = S.object, S.string, S.boolean

local content_kind = {
	type = "string",
	enum = { "skills", "prompts", "themes", "types" },
	description = "Optional content kind (`skills | prompts | themes | types`) for `dirs`; omitted returns package roots.",
}

local string_list = { type = "array", items = string }
local plugin_dependencies = object({ required = string_list, optional = string_list }, { "required", "optional" })
local plugin_provides = object({
	tools = string_list,
	commands = string_list,
	hooks = string_list,
	skills = string_list,
	prompts = string_list,
	themes = string_list,
	services = string_list,
	flags = string_list,
	mcp = string_list,
}, { "tools", "commands", "hooks", "skills", "prompts", "themes", "services", "flags", "mcp" })

local package_manifest = object({
	name = string,
	description = string,
	version = string,
	requires = string,
	provides = plugin_provides,
	platforms = { type = "array", items = { type = "string", enum = { "linux", "macos", "windows" } } },
	keywords = string_list,
	license = string,
	homepage = string,
	news = string,
	dependencies = string_list,
	skills = string_list,
	prompts = string_list,
	themes = string_list,
	category = string,
	deps = plugin_dependencies,
	conflicts = string_list,
	plugins = string_list,
	web = object({ dir = string, prefix = string, index = string, fallback = string }, { "dir" }),
}, { "dependencies", "skills", "prompts", "themes", "deps", "conflicts", "plugins", "platforms", "keywords", "news" })

local package_status = object({
	name = string,
	scope = { type = "string", enum = { "user", "project", "init.lua" } },
	display = string,
	url = string,
	dir = string,
	["local"] = boolean,
	installed = boolean,
	disabled = boolean,
	commit = string,
	locked = string,
	dirty = boolean,
	problem = string,
	constraint = string,
	branch = string,
	manifest = package_manifest,
}, { "name", "scope", "display", "url", "dir", "local", "installed", "disabled", "dirty" })

local package_dir = object({ name = string, dir = string }, { "name", "dir" })
local manifest_diagnostic = object(
	{ plugin = string, path = string, constraint = string, message = string },
	{ "plugin", "path", "message" }
)
local update_check = object({
	name = string,
	status = { type = "string", enum = { "up_to_date", "outdated", "pinned", "local", "unreachable" } },
	behind_count = { type = "integer", minimum = 0 },
	commits = string_list,
	breaking = boolean,
	error = string,
}, { "name", "status", "behind_count", "commits", "breaking" })

local reload_result = object({
	added = string_list,
	removed = string_list,
	replaced = string_list,
	reloaded = string_list,
	notices = string_list,
}, { "added", "removed", "replaced", "reloaded", "notices" })

local action_params = object({
	name = {
		type = "string",
		description = "Optional plugin short name or declared source name; omitted operates on all eligible sources.",
	},
	reload = { type = "boolean", description = "When true and the package operation succeeds, run a full host reload." },
})
local update_params = object({
	name = action_params.properties.name,
	reload = action_params.properties.reload,
	stash = { type = "boolean", description = "Save local changes and untracked files in git stash before updating." },
})

local action_result = object({ log = string_list, reload = reload_result }, { "log" })

return {
	["plugins"] = {
		doc = "Declared git packages, resource directories, update news, and optional plugins.",
		events = {
			["completed"] = {
				doc = "Package manager action finished.",
				mode = "emit",
				expose = "lua",
				data = object({
					action = string,
					name = string,
					status = { type = "string", enum = { "completed", "failed" } },
				}, { "action", "status" }),
			},
			["plugin"] = {
				doc = "An optional package plugin was enabled or disabled.",
				mode = "emit",
				expose = "lua",
				data = object({ name = string, enabled = boolean }, { "name", "enabled" }),
			},
		},
		errors = {
			failed = {
				doc = "The package operation did not succeed; `data.log` carries any output lines recorded before failure.",
				data = object({ log = string_list }, { "log" }),
			},
		},
		methods = {
			check = {
				doc = "Fetch and compare plugin revisions without moving checkouts or modifying locks.",
				expose = "both",
				access = "primary",
				confined = true,
				params = object({ names = string_list }),
				order = { "names" },
				result = object({ data = { type = "array", items = update_check } }, { "data" }),
			},
			list = {
				doc = "Declared/installed packages, parsed `mare-plugin.toml` manifests, and resource directories.",
				expose = "both",
				access = "peer",
				params = object({ kind = content_kind }),
				order = { "kind" },
				result = object({
					data = { type = "array", items = package_status },
					dirs = { type = "array", items = package_dir },
					diagnostics = { type = "array", items = manifest_diagnostic },
				}, { "data", "dirs", "diagnostics" }),
			},
			install = {
				doc = "Install a catalog plugin by short name or sync a declared source; raises `plugins.failed` on error.",
				expose = "both",
				access = "primary",
				admin = true,
				params = action_params,
				order = { "name", "..." },
				result = action_result,
				errors = { "failed" },
			},
			update = {
				doc = "Update declared git packages to their newest allowed revision (`update`); raises `plugins.failed` on error.",
				expose = "both",
				access = "primary",
				admin = true,
				params = update_params,
				order = { "name", "..." },
				result = action_result,
				errors = { "failed" },
			},
			uninstall = {
				doc = "Uninstall a catalog plugin by short name or clean an unused source; raises `plugins.failed` on error.",
				expose = "both",
				access = "primary",
				admin = true,
				params = action_params,
				order = { "name", "..." },
				result = action_result,
				errors = { "failed" },
			},
			news = {
				doc = "Show NEWS.md sections for mare / enabled plugins whose version changed since last seen.",
				params = object({}),
				order = {},
				result = string_list,
			},
			choose_plugins = {
				doc = "Offer the optional plugins, install selected packages, and return the outcome; nil when the offer is not due.",
				optional = true,
				params = object({}),
				order = {},
				result = {
					oneOf = {
						object({
							result = { type = "string", enum = { "chosen", "skipped", "unavailable" } },
							enabled = string_list,
							refused = {
								type = "array",
								items = object({ name = string, reason = string }, { "name", "reason" }),
							},
							failed = string_list,
							synced = boolean,
						}, { "result", "enabled", "refused", "failed", "synced" }),
						{ type = "null" },
					},
				},
			},
		},
	},
}

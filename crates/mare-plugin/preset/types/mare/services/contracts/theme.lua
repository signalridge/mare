---@meta _
-- The `theme` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, list_of, boolean, theme_styles, theme_definition, theme_snapshot, theme_descriptor =
	S.object, S.string, S.list_of, S.boolean, S.theme_styles, S.theme_definition, S.theme_snapshot, S.theme_descriptor
return {
	["theme"] = {
		doc = "Replaceable theme service. All returned data is copied and JSON-safe.",
		errors = { unknown_theme = { doc = "no installed or registered theme has that name" } },
		methods = {
			list = {
				doc = "Installed and registered themes, sorted by name.",
				expose = "both",
				access = "peer",
				result = list_of(theme_descriptor),
			},
			get = { doc = "The active theme snapshot.", expose = "both", access = "peer", result = theme_snapshot },
			inspect = {
				doc = "Builtin, dynamic, inactive and override-only style tokens.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = {
					type = "array",
					items = object(
						{ name = string, style = { type = "object", additionalProperties = true } },
						{ "name", "style" },
						true
					),
				},
				lua_result = "{name: string, style: mare.Style}[]",
			},
			register = {
				doc = "Register a theme owned by the caller. Duplicate or invalid definitions fail without "
					.. "changing state; releasing the registration removes only it, and releasing the "
					.. "selected theme restores auto.",
				expose = "both",
				access = "peer",
				params = object({ definition = theme_definition }, { "definition" }),
				order = { "definition" },
				registration = true,
			},
			select = {
				doc = "Validate, optionally save atomically, then publish the selection. save defaults to "
					.. "false (process only); init.lua still wins on the next start.",
				expose = "both",
				access = "peer",
				params = object({ theme = string, save = boolean }, { "theme" }),
				order = { "theme", "..." },
				result = theme_snapshot,
				errors = { "unknown_theme" },
			},
			preview = {
				doc = "Preview a theme. Affects rendering only, never the preference; ends with its caller.",
				expose = "both",
				access = "peer",
				params = object({ theme = string }, { "theme" }),
				order = { "theme" },
				result = theme_snapshot,
				errors = { "unknown_theme" },
			},
			cancel_preview = {
				doc = "End the current preview.",
				expose = "both",
				access = "peer",
				result = theme_snapshot,
			},
			register_override = {
				doc = "An owned style layer; later layers win, config.theme and explicit ui.style win over "
					.. "layers. Replacing a layer id keeps one layer; an older registration cannot remove "
					.. "its replacement.",
				expose = "both",
				access = "peer",
				params = object({ layer_id = string, styles = theme_styles }, { "layer_id", "styles" }),
				order = { "layer_id", "styles" },
				registration = true,
			},
		},
		events = {
			["changed"] = {
				doc = "Active theme or style layers changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = theme_snapshot,
			},
			["palette"] = {
				doc = "Transform the resolved palette before deriving theme styles.",
				mode = "waterfall",
				expose = "process",
				data = object({
					palette = { type = "object", additionalProperties = true },
					flavor = string,
					background = string,
				}, { "palette" }, true),
				result = object({
					palette = { type = "object", additionalProperties = true },
				}, nil, true),
			},
		},
	},
}

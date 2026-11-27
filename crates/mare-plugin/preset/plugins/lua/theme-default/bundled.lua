-- Built-in bundled syntax theme manifest for theme-default.
-- Individual theme data modules live in their own files (<=750 lines) with upstream license headers
-- and are loaded lazily on first use so startup only parses the active theme.

local function ansi_pair(item)
	return {
		name = item.name,
		label = item.label,
		variants = {
			dark = { base = "dark", palette = {}, syntax = item.doc },
			light = { base = "light", palette = {}, syntax = item.doc },
		},
	}
end

local function find_in(items, theme_name)
	for _, item in ipairs(items) do
		if item.name == theme_name then
			return item.doc and ansi_pair(item) or item
		end
	end
	error("missing bundled theme: " .. theme_name, 0)
end

local function normalize(item)
	return item.doc and ansi_pair(item) or item
end

return {
	{
		name = "ansi",
		label = "ANSI",
		variants = { "dark", "light" },
		load = function()
			return find_in(require("theme-default.bundled_ansi"), "ansi")
		end,
	},
	{
		name = "base16",
		label = "Base16",
		variants = { "dark", "light" },
		load = function()
			return find_in(require("theme-default.bundled_ansi"), "base16")
		end,
	},
	{
		name = "base16-256",
		label = "Base16 256",
		variants = { "dark", "light" },
		load = function()
			return find_in(require("theme-default.bundled_base16_256"), "base16-256")
		end,
	},
	{
		name = "base16-eighties-dark",
		label = "Base16 Eighties Dark",
		variants = { "dark" },
		load = function()
			return normalize(require("theme-default.bundled_base16_eighties"))
		end,
	},
	{
		name = "base16-mocha-dark",
		label = "Base16 Mocha Dark",
		variants = { "dark" },
		load = function()
			return normalize(require("theme-default.bundled_base16_mocha"))
		end,
	},
	{
		name = "base16-ocean-dark",
		label = "Base16 Ocean Dark",
		variants = { "dark" },
		load = function()
			return normalize(require("theme-default.bundled_base16_ocean_dark"))
		end,
	},
	{
		name = "base16-ocean-light",
		label = "Base16 Ocean Light",
		variants = { "light" },
		load = function()
			return normalize(require("theme-default.bundled_base16_ocean_light"))
		end,
	},
	{
		name = "inspired-github",
		label = "Inspired GitHub",
		variants = { "light" },
		load = function()
			return normalize(require("theme-default.bundled_inspired_github_b"))
		end,
	},
	{
		name = "ada",
		label = "Ada",
		variants = { "dark" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "ada")
		end,
	},
	{
		name = "babbage",
		label = "Babbage",
		variants = { "light" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "babbage")
		end,
	},
	{
		name = "curie",
		label = "Curie",
		variants = { "dark" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "curie")
		end,
	},
	{
		name = "cushman",
		label = "Cushman",
		variants = { "light" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "cushman")
		end,
	},
	{
		name = "dali",
		label = "Dali",
		variants = { "dark" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "dali")
		end,
	},
	{
		name = "davinci",
		label = "Davinci",
		variants = { "light" },
		load = function()
			return find_in(require("theme-default.bundled_codex_models"), "davinci")
		end,
	},
}

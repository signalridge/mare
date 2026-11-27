-- Default theme provider; all registration/selection uses the public theme contract.
return {
	name = "theme-default",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "theme" },
	-- mare events go through ctx.bus
	inject = { "bus" },
	config_schema = {
		dirs = {
			type = "list",
			items = { type = "string" },
			desc = "Theme directories; default `<config dir>/themes`.",
		},
		flavors = { type = "map", desc = "Flavors by name." },
		colors = { type = "map", desc = "Palette overrides by base (dark / light)." },
		ratios = { type = "map", desc = "Background blend ratios." },
		blend_background = {
			type = "boolean",
			default = true,
			desc = "Blend the palette with the terminal background.",
		},
	},
	apply = function(ctx, cfg)
		require("theme-default.service")(ctx, cfg, require("theme-default.defaults"))
	end,
}

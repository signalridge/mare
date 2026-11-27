-- images: where pictures are shown in the transcript. The core primitive
-- `mare.ui.image` picks the terminal's protocol (Kitty / iTerm2 / sixel) or a text placeholder
-- (`[image: <label> W×H]`); this plugin decides *where*: images the user pasted, images the read
-- tool returned and images in other tools' results.
--
-- Two layers: this file holds the config and the hooks; the drawing
-- (lua/images/render.lua) loads on the first image.

return {
	name = "images",
	provide = { "images" },
	-- show pasted images and images from read / tool results inline
	config_schema = {
		user = { type = "boolean", default = true, desc = "Show images pasted / attached to a prompt." },
		read = { type = "boolean", default = true, desc = "Show images returned by the read tool." },
		tools = { type = "boolean", default = true, desc = "Show images in other tools' results." },
		max_rows = { type = "integer", default = 12, desc = "Tallest an image may be drawn, in cells." },
		max_cols = { type = "integer", default = 60, desc = "Widest an image may be drawn, in cells." },
	},
	apply = function(ctx, cfg)
		-- every connected TUI routes through the core `mare.ui.image` primitive (graphic when a
		-- protocol is available, `[image: <label> W×H]` placeholder otherwise); headless output
		-- never loads the renderer
		local function enabled()
			return mare.mode == "tui" and mare.ui.connected()
		end

		-- drawing loads lua/images/render.lua, on the first content that carries an image
		local render
		local function show(content, indent, default_path)
			if not enabled() then
				return
			end
			for _, b in ipairs(content or {}) do
				if type(b) == "table" and b.type == "image" and (b.data or b.path) then
					render = render or require("images.render")(cfg)
					return render(content, indent, default_path)
				end
			end
		end

		ctx:on("item/completed", function(ev)
			local item = ev and ev.item
			if not item then
				return
			end
			if item.type == "tool_call" then
				local m = item.message
				if m and ((item.name == "read" and cfg.read) or (item.name ~= "read" and cfg.tools)) then
					local path = item.name == "read" and type(item.arguments) == "table" and item.arguments.path or nil
					show(m.content, "     ", path)
				end
				return
			end
			local m = item.message
			if cfg.user and m and m.role == "user" and not m.notification and not m.shell then
				show(m.content)
			end
		end)

		-- a mare.Service
		require("mare_lib").service(ctx, "images", {
			--- show(content): render the image blocks of a message/result content list
			show = function(_, _, content)
				show(content)
			end,
		})
	end,
}

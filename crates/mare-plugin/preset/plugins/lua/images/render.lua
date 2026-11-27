-- images: drawing the image blocks of a message / tool result into the transcript via the
-- core `mare.ui.image` primitive (graphic when a protocol is available, `[image: <label> W×H]`
-- placeholder otherwise); loaded on the first image to show.
--
-- render(cfg) -> show(content, indent?, default_path?): `cfg` is the plugin's config (the size limits).
return function(cfg)
	local function nodes_for(content, default_path)
		local out = {}
		for _, b in ipairs(content or {}) do
			if type(b) == "table" and b.type == "image" then
				local path = type(b.path) == "string" and b.path ~= "" and b.path or default_path
				if b.data then
					local ok, bytes = pcall(mare.crypto.base64_decode, b.data)
					if ok and bytes then
						out[#out + 1] = mare.ui.image({
							bytes = bytes,
							path = path,
							max_rows = cfg.max_rows,
							max_cols = cfg.max_cols,
						})
					end
				elseif path then
					out[#out + 1] = mare.ui.image({
						path = path,
						max_rows = cfg.max_rows,
						max_cols = cfg.max_cols,
					})
				end
			end
		end
		return out
	end

	local function show(content, indent, default_path)
		local nodes = nodes_for(content, default_path)
		if #nodes == 0 then
			return
		end
		local node = mare.ui.Lines(nodes)
		if indent then
			node = mare.ui.Indent(indent, node, indent)
		end
		mare.ui.finish(mare.ui.push("image", { node = node }))
	end

	return show
end

-- mcp.results: MCP results as content parts, long-text cuts, image support;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
local lib = require("mare_lib")

return function(D)
	local ctx, cfg = D.ctx, D.cfg

	-- ---- results --------------------------------------------------------------------

	local function images_supported()
		local ok, model = pcall(function()
			local llm = D.svc.llm
			return llm and llm:current()
		end)
		return not (ok and type(model) == "table" and lib.model_images(model) == false)
	end

	--- long text: middle cut, full text in a private file named in the result
	local function limit_text(text)
		local limit = cfg.text_limit or 20480
		if #text <= limit then
			return text
		end
		local ok, path = pcall(mare.mcp.spill, text, "txt")
		if not ok or type(path) ~= "string" or path == "" then
			error("MCP result could not be saved; refusing to return a truncated result: " .. tostring(path))
		end
		local half = math.floor(limit / 2)
		-- both cuts land on UTF-8 boundaries
		local head = mare.text.cut(text, half)
		local tail = mare.text.cut(text, half, { keep = "tail" })
		local cut = #text - #head - #tail
		return ("%s\n…%d chars truncated%s…\n%s"):format(head, cut, "; the full result is in " .. path, tail)
	end

	local function content_of(server, result)
		local parts = mare.json.array()
		local images = images_supported()
		for _, c in ipairs(type(result.content) == "table" and result.content or {}) do
			if c.type == "text" then
				parts[#parts + 1] = { type = "text", text = c.text or "" }
			elseif c.type == "image" and c.data then
				if images then
					parts[#parts + 1] = { type = "image", data = c.data, mime = c.mimeType }
				else
					parts[#parts + 1] = {
						type = "text",
						text = ("[image (%s) omitted: the current model doesn't accept images]"):format(
							tostring(c.mimeType or "image")
						),
					}
				end
			elseif c.type == "resource" and type(c.resource) == "table" then
				local r = c.resource
				parts[#parts + 1] = {
					type = "text",
					text = r.text or ("[binary resource %s (%s), %d bytes base64]"):format(
						tostring(r.uri),
						tostring(r.mimeType or "?"),
						#(r.blob or "")
					),
				}
			elseif c.type == "resource_link" then
				parts[#parts + 1] = {
					type = "text",
					text = ("[resource] %s — %s (read it with read_mcp_resource server=%q uri=%q)"):format(
						tostring(c.name or c.title or c.uri),
						tostring(c.uri),
						server,
						tostring(c.uri)
					),
				}
			elseif c.type == "audio" then
				parts[#parts + 1] = {
					type = "text",
					text = ("[audio (%s) returned by the server: mare can't pass audio to the model]"):format(
						tostring(c.mimeType or "audio")
					),
				}
			else
				parts[#parts + 1] =
					{ type = "text", text = ("[unsupported MCP content type %q]"):format(tostring(c.type)) }
			end
		end
		if result.structuredContent ~= nil then
			local s = mare.json.encode(result.structuredContent)
			parts[#parts + 1] = { type = "text", text = (#parts > 0 and "structuredContent: " or "") .. s }
		end
		for _, p in ipairs(parts) do
			if p.type == "text" then
				p.text = limit_text(p.text)
			end
		end
		return parts
	end

	return { images_supported = images_supported, limit_text = limit_text, content_of = content_of }
end

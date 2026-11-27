-- tools.prompt: the `tools` system prompt section — one line per tool that has a snippet (e.g.
-- "- read: Read file contents"); part of the `tools` plugin (plugins/tools.lua), loaded at the
-- end of its apply. The tools' guidelines go into context's `rules` section.
return function(D)
	local ctx, tools = D.ctx, D.registry.tools
	-- tools applies before context (an optional dependency of context): registered through
	-- mare_lib.prompt_section, so `/reload context` doesn't reload this plugin
	require("mare_lib").prompt_section(ctx, {
		name = "tools",
		order = 100,
		interpolate = false, -- snippets of plugin and user tools: verbatim
		text = function(model)
			local lines = {}
			for _, t in ipairs(tools.snippets(tools, ctx, model)) do
				lines[#lines + 1] = ("- %s: %s"):format(t.name, t.snippet)
			end
			local deferred = D.registry.deferred_names(model)
			if #lines == 0 and #deferred == 0 then
				return nil
			end
			local parts = {}
			if #lines > 0 then
				parts[#parts + 1] = table.concat(lines, "\n")
			end
			if #deferred > 0 then
				if D.registry.native_tool_update(model) then
					parts[#parts + 1] = "Deferred tools (call tool_search with keywords or select:<name1,name2> "
						.. "to load, then invoke directly): "
						.. table.concat(deferred, ", ")
				else
					parts[#parts + 1] = "Deferred tools (call tool_search with keywords or select:<name1,name2> "
						.. "for schemas, then invoke via call_tool): "
						.. table.concat(deferred, ", ")
				end
			else
				parts[#parts + 1] = "Additional tools may be available."
			end
			return table.concat(parts, "\n\n")
		end,
	})
end

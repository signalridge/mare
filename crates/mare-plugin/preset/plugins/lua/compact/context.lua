-- measure({ by_source = true }): the context split by source (what the context plugin's /context
-- shows), for the `compact` plugin (the entry layer, plugins/compact.lua); loaded on first use.
-- `D` carries the measuring helpers it shares with the entry (the same estimator as the
-- compaction trigger).
return function(D)
	local ctx = D.ctx
	local S = D.S
	local tokens = D.tokens
	local msg_tokens = D.msg_tokens
	local overhead = D.overhead

	--- the context split by source, with the same estimator and the same
	--- pieces as measure() (system prompt and tools from `overhead`, an image =
	--- mare.text.image_tokens), so /context's total and the compaction trigger agree:
	--- {system, instructions, skills, tools, messages, tool_results, images, total}
	local function breakdown()
		local out = { system = 0, instructions = 0, skills = 0, tools = 0, messages = 0, tool_results = 0, images = 0 }
		local model = ctx.llm:current()
		local _, oh = overhead(model)
		out.tools = oh.tools
		if S.svc.context then
			local sys = oh.system
			local files, ferr = S.svc.context:files()
			if files == nil and ferr ~= nil then
				error(ferr)
			end
			for _, f in ipairs(files or {}) do
				out.instructions = out.instructions + tokens(f.content or "")
			end
			local skills, serr = S.svc.context:skills()
			if skills == nil and serr ~= nil then
				error(serr)
			end
			for _, sk in ipairs(skills or {}) do
				out.skills = out.skills + tokens((sk.name or "") .. ": " .. (sk.description or ""))
			end
			out.system = math.max(0, sys - out.instructions - out.skills)
		end
		for _, m in ipairs(ctx.agent:messages()) do
			local imgs = 0
			for _, b in ipairs(m.content or {}) do
				if b.type == "image" then
					imgs = imgs + 1
				end
			end
			-- exactly msg_tokens(m), split into its images and the rest
			local all = msg_tokens(m)
			local img = math.min(all, imgs * mare.text.image_tokens)
			out.images = out.images + img
			local n = all - img
			if m.role == "tool_result" then
				out.tool_results = out.tool_results + n
			else
				out.messages = out.messages + n
			end
		end
		out.total = out.system
			+ out.instructions
			+ out.skills
			+ out.tools
			+ out.messages
			+ out.tool_results
			+ out.images
		return out
	end

	return { breakdown = breakdown }
end

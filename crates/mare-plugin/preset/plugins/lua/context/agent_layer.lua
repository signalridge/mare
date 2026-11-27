-- context.agent_layer: a sub-agent's own prompt sections: `mare.subagent.start` options
-- `persona`, `sections` and `system` are registered in this VM's agent layer, so they shadow the
-- same-named sections of the global and user layers for this sub-agent only (each sub-agent has a
-- VM of its own: nothing is left behind when it ends). Part of the `context` plugin
-- (plugins/context.lua), loaded at the start of its apply.
--   persona = "..." the identity (raw, like the built-in one)
--   sections = { {name, order, text,...}, … } any section; text is a string here
--   system = "..." the whole prompt, verbatim (a complete section)
return function(D)
	local ctx, context = D.ctx, D.context
	local args = mare.mode == "child" and mare.args or nil
	if type(args) ~= "table" then
		return
	end
	local function add(def)
		def.scope = "agent"
		context.section(context, ctx, def)
	end
	if type(args.persona) == "string" then
		add({ name = "persona", tag = "preamble", order = 0, raw = true, text = args.persona })
	end
	for _, s in ipairs(type(args.sections) == "table" and args.sections or {}) do
		if type(s) == "table" then
			add({
				name = s.name,
				tag = s.tag,
				order = s.order,
				text = s.text,
				raw = s.raw,
				complete = s.complete,
				interpolate = s.interpolate,
			})
		end
	end
	if type(args.system) == "string" then
		add({ name = "system", order = 0, raw = true, complete = true, interpolate = false, text = args.system })
	end
end

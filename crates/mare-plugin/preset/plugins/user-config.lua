-- user-config: init.lua customizations that act on other plugins through their public services
-- tool descriptions, prompt sections, command aliases.
--   mare.tool.describe("bash", "...") -- replace a tool's description
--   mare.prompt.section { name =, order =, text = } / mare.prompt.remove("name")
--   mare.command.alias("m", "model")
return {
	name = "user-config",
	-- tools, context and commands are optional, followed or
	-- read when used
	config_api = {
		["tool.describe"] = function(C, name, text)
			C.tool_descriptions = C.tool_descriptions or {}
			C.tool_descriptions[name] = text
		end,
		["prompt.section"] = function(C, def)
			assert(type(def) == "table" and def.name, "mare.prompt.section needs a name")
			C.prompt_sections = C.prompt_sections or {}
			C.prompt_sections[def.name] = { name = def.name, order = def.order, text = def.text }
		end,
		["prompt.remove"] = function(C, name)
			C.prompt_removed = C.prompt_removed or {}
			C.prompt_removed[name] = true
		end,
		["command.alias"] = function(C, short, target)
			C.command_aliases = C.command_aliases or {}
			C.command_aliases[short] = target
		end,
	},
	apply = function(ctx)
		local C = mare.config
		-- Tool shadows follow definitions replaced during startup or plugin reloads.
		ctx:inject({ "tools" }, function(sub)
			local tools = sub.tools
			local descriptions = {}
			local function describe_tools()
				if not C.tool_descriptions then
					return
				end
				for name, text in pairs(C.tool_descriptions) do
					if descriptions[name] and descriptions[name].alive() then
						goto continue
					end
					descriptions[name] = nil
					local def = tools:get(name)
					if def then
						def.description = text
						-- a freeform (custom) tool is declared with its own description (OpenAI
						-- Responses, provider-openai/wire.lua:348): the user's text replaces it too
						if type(def.freeform) == "table" then
							def.freeform.description = text
						end
						local remove, alive = tools:shadow(name, def)
						descriptions[name] = { remove = remove, alive = alive }
					else
						ctx.logger:warn("mare.tool.describe: no tool named " .. name)
					end
					::continue::
				end
			end
			describe_tools()
			sub:on("app/ready", describe_tools)
			sub:on("kernel/reloaded", describe_tools)
		end)
		-- registered while the optional `context` service is up (a child fiber on the cordis tree)
		ctx:inject({ "context" }, function(ctx)
			for _, sec in pairs(C.prompt_sections or {}) do
				-- the user layer shadows a plugin's section of the same name; user text is verbatim
				ctx.context:section({
					name = sec.name,
					order = sec.order or 500,
					text = sec.text,
					scope = "user",
					interpolate = false,
				})
			end
			-- hides the section in every scope, including agent-specific ones and sections
			-- registered later (e.g. after a plugin reload)
			for name in pairs(C.prompt_removed or {}) do
				ctx.context:suppress(name)
			end
		end)
		-- registered while the optional `commands` service is up (a child fiber)
		ctx:inject({ "commands" }, function(sub)
			local commands = sub.commands
			for short, target in pairs(C.command_aliases or {}) do
				commands:register(short, { alias_of = target })
			end
		end)
	end,
}

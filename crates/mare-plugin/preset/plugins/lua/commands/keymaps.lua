-- commands.keymaps: editor key bindings for model selection, cycling, effort, and model params;
-- part of the `commands` plugin (plugins/commands.lua).
local M = {}

local function cycle_model(ctx)
	local fav = (mare.o and mare.o.favorite_models) or mare.config.favorite_models
	local list = {}
	for _, ref in ipairs(type(fav) == "table" and #fav > 0 and fav or ctx:get("llm"):recent()) do
		local m = ctx:get("llm"):resolve(ref)
		if m and not table.find(list, m.ref) then
			list[#list + 1] = m.ref
		end
	end
	if #list < 2 then
		ui.notify("No other model to switch to: /model picks one; `favorite_models` lists models to cycle", "info")
		return
	end
	local cur = ctx:get("llm"):current()
	local at = cur and table.find(list, cur.ref) or 0
	local nxt = list[at % #list + 1]
	ctx:get("llm"):set_model(nxt)
	ui.notify(("Model: %s (%d/%d)"):format(nxt, table.find(list, nxt), #list))
end

local function register_editor_action(sub, name, key, fn, desc)
	local h = mare.keymap.action(name, fn, {
		desc = desc,
		contexts = { "editor" },
		keys = { editor = key },
	})
	sub:effect(function()
		return function()
			h:remove()
		end
	end)
end

function M.register(ctx, commands)
	-- the model keys exist while the llm service is up: registered from a follower of it (they go
	-- with it and come back with a new provider); spawned work stays on the plugin's own fiber
	ctx:inject({ "llm" }, function(sub)
		-- the picker is a popup over the input box, so the draft stays where it was
		register_editor_action(sub, "model.picker", "<M-p>", function()
			ctx:spawn(function()
				local ok, err = pcall(commands.run, commands, "/model")
				if
					not ok
					and err ~= nil
					and not (type(err) == "table" and err.code == "commands.command_unavailable")
				then
					ui.notify(tostring(err), "warning")
				end
			end)
		end, "select model (keeps the input)")

		-- cycle favorite (else recently used) models without the picker
		register_editor_action(sub, "model.cycle", "<M-m>", function()
			ctx:spawn(function()
				cycle_model(ctx)
			end)
		end, "next favorite / recent model")

		-- Alt+, / Alt+.: one supported effort level down / up
		for _, k in ipairs({
			{ "effort.down", "<M-,>", -1, "lower" },
			{ "effort.up", "<M-.>", 1, "raise" },
		}) do
			register_editor_action(sub, k[1], k[2], function()
				ctx:spawn(function()
					require("commands.effort").step(ctx, k[3])
				end)
			end, k[4] .. " reasoning effort (the model's levels)")
		end

		-- per-model effort: a switch restores the level remembered for the model, or says
		-- when the current one is adjusted (never a silent clamp)
		sub:on("llm/model-changed", function(ev)
			if type(ev) == "table" and ev.model and not ev.params_only then
				require("commands.effort").on_model_changed(ctx, ev.model)
			end
		end)
	end)
end

return M

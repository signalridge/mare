-- statusline.visibility: segment visibility state and `/statusline` picker;
-- part of the `statusline` plugin (plugins/statusline.lua).
local M = {}

local SEGMENTS = {
	{ "mode", "permission mode capsule (READ / AUTO / FULL)" },
	{ "model", "model name" },
	{ "effort", "reasoning effort next to the model" },
	{ "context", "context window gauge" },
	{ "cwd", "working directory (wide terminals)" },
	{ "branch", "git branch" },
	{ "pr", "the branch's pull request (gh)" },
	{ "changes", "number of changed files" },
	{ "cost", "session cost" },
	{ "quota", "subscription quota left" },
	{ "tips", "rotating usage tips" },
}

function M.invalidate_all(ctx)
	for _, id in ipairs({ "left", "right" }) do
		ctx.ui:invalidate(id)
	end
end

function M.open_picker(ctx, hidden, shown)
	ctx:spawn(function()
		local items = {}
		for i, sg in ipairs(SEGMENTS) do
			items[i] = { label = sg[1], detail = sg[2], checked = shown(sg[1]) }
		end
		local indices = ui.select({
			title = "Status bar segments",
			items = items,
			multi = true,
			accept_label = "apply",
		})
		if not indices then
			return
		end
		assert(type(indices) == "table", "multi-select must return an array")
		local selected = {}
		for _, i in ipairs(indices) do
			selected[i] = true
		end
		local list = {}
		for i, sg in ipairs(SEGMENTS) do
			hidden[sg[1]] = not selected[i] or nil
			if hidden[sg[1]] then
				list[#list + 1] = sg[1]
			end
		end
		require("mare_lib").save_state(ctx, "statusline_hidden", list, true)
		M.invalidate_all(ctx)
	end)
end

return M

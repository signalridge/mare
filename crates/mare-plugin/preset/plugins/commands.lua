-- commands: slash-command registry + basic commands (/help /exit /model /status /init /copy
-- /export /config …); input starting with `/` is routed here. Aliases resolve to their target
-- and stay out of /help and completion.

return {
	name = "commands",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "commands" },
	-- task: ctx:spawn; kernel: /plugins, /reload, /doctor, /status; llm, theme and session are
	-- read when they are up (ctx:get), and registrations into them come from followers
	inject = { "bus", "task", "kernel" },
	apply = function(ctx, _cfg)
		local reg = require("commands.registry").create(ctx)
		require("commands.completion").register(ctx, reg)
		require("commands.builtins").register(ctx, reg.commands, reg.stacks, reg.registry, reg.run_local)
		require("commands.keymaps").register(ctx, reg.commands)
	end,
}

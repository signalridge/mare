-- permissions: sandbox + approval presets. A builtin Lua plugin. Rust validates and
-- enforces policy; permission preferences live here. This uses public primitives: the kernel `policy` dispatch
-- (tool/before votes; the agent asks on `ask` and reports `approval/requested` / `approval/resolved`), the sandbox
-- primitives (mare.proc.sandbox, session:set_sandbox_mode), ui.select / ui.confirm / ui.notify
-- and session records. Disable or replace it in init.lua; without it mare runs with full access.
--
-- Presets (names approval-presets, behaviour permission-presets):
--   read-only READ sandbox read-only; file writes ask first, commands that write are
--                       refused by the sandbox and the model may request more
--   auto AUTO sandbox workspace + temp dirs (+ network unless turned off); asks only
--                       when the model requests to leave the sandbox, or for unconfined tools
--   full-access FULL no sandbox, never asks
--   custom CUSTOM shown when the session's sandbox matches no preset (not selectable)
--
-- Escalation: shell tools take
-- `sandbox_permissions` = "network" | "network:<host>" | "write:<path>" | "all" (or the
-- compatibility levels "workspace-write" / "danger-full-access") plus a `justification`; only
-- wider requests ask; an approval applies to that one call. Never auto-retried.
--
-- Rules: { argv = {"git","push"}, decision = "allow"|"prompt"|"forbidden",
--   scope = "network"|"write:<path>"|"all", cwd = "~/ghq/**", expires = "session"|"never",
--   deny_args = {"-c"}, justification = "..." } — matched per segment on the parsed command
--   plain words joined by && ||; | are split and
--   every segment must match an allow rule; $, backticks, expansions, subshells, redirections
--   (except fixed sinks like /dev/null), eval and nested shells make a command not analyzable —
--   no allow rule applies, any matching-cwd forbidden argv rule fails closed (deny), and any
--   matching-cwd prompt argv rule asks instead.
--   Unconfined tools can be allow-listed with { tool = "<name>", decision = "allow" }.

return {
	name = "permissions",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "permissions" },
	inject = { "bus", "policy", "task" },
	config_schema = {
		preset = {
			type = "string",
			default = "auto",
			choices = { "read-only", "auto", "full-access" },
			choices_from = "presets",
			desc = "Sandbox and approval preset (a built-in one or a name from `presets`).",
		},
		-- extra writable roots for `auto` (existing absolute directories; missing ones are skipped)
		writable = {
			type = "list",
			items = { type = "string" },
			default = {},
			desc = "Extra writable roots for the `auto` preset.",
		},
		readable = {
			type = "list",
			items = { type = "string" },
			default = {},
			desc = "Extra readable roots for the `auto` preset.",
		},
		-- Literal deny-read paths, empty by default. Unresolved paths require explicit partial
		-- acceptance and are not masked, even if created later during that policy's lifetime.
		deny_read = { type = "list", items = { type = "string" }, default = {}, desc = "Literal deny-read paths." },
		-- `auto` network: true (open) | false | { default = "deny", allow = {...}, allow_local = false }
		network = {
			type = { "boolean", "table" },
			default = true,
			desc = "Network access of the `auto` preset: true (open), false, or a deny/allow table.",
		},
		-- sandbox dimensions you accept as unenforced when this platform's backend can't provide
		-- them, e.g.
		-- { "network" }; names: fs_read, fs_deny_read, fs_write, fs_protect, net_block, net_proxy, unix_socket, network.
		-- nil (default): implicitly accepts a missing fs_protect and reports PARTIAL; on Windows'
		-- default tier (no `mare sandbox setup`) also accepts its uncontrolled network as PARTIAL.
		-- An explicit {} always means strict refusal.
		accept_partial = {
			type = "list",
			items = { type = "string" },
			desc = "Sandbox dimensions accepted as unenforced when the platform backend cannot provide them.",
		},
		-- Unix sockets reachable while the network is restricted (local daemons stay blocked
		-- otherwise; the sudo askpass socket is always allowed)
		unix_sockets = {
			type = "list",
			items = { type = "string" },
			default = {},
			desc = "Unix sockets reachable while the network is restricted.",
		},
		-- dangerous commands: a hit
		-- asks even in full-access, and headless (nobody to answer) denies it. Built-in rules:
		-- "forced_rm" (rm -f / --force or find -delete anywhere in the script, through wrappers;
		-- an unparsable script is word-scanned, failing closed),
		-- "windows_force_delete" (Remove-Item -Force, del /f, rd /s /q). `disable` turns
		-- built-ins off by name; `rules` adds argv prefixes, e.g. { "git", "push", "--force" }.
		-- Only an `allow` rule matching the exact command passes one without asking — never the
		-- session's approval cache.
		dangerous = {
			type = "object",
			desc = "Dangerous-command detection: a hit asks even in full-access and is denied headless.",
			fields = {
				enabled = { type = "boolean", default = true, desc = "Whether dangerous-command detection is on." },
				disable = {
					type = "list",
					items = { type = "string" },
					default = {},
					desc = "Built-in rules to turn off by name.",
				},
				rules = { type = "list", default = {}, desc = "Extra argv prefixes treated as dangerous." },
			},
		},
		rules = {
			type = "list",
			default = {},
			on_invalid = "fail",
			desc = "Permission rules.",
			items = {
				type = "object",
				on_invalid = "fail",
				fields = {
					decision = {
						type = "enum",
						choices = { "allow", "prompt", "forbidden" },
						on_invalid = "fail",
						desc = "Rule decision.",
					},
					argv = {
						type = "list",
						items = { type = "string", on_invalid = "fail" },
						on_invalid = "fail",
						desc = "Argv prefix to match.",
					},
					tool = { type = "string", on_invalid = "fail", desc = "Tool name to match." },
					tool_version = { type = "string", on_invalid = "fail", desc = "Tool version to match." },
					path = { type = "string", on_invalid = "fail", desc = "Path glob to match." },
					op = {
						type = "enum",
						choices = { "read", "write", "create", "delete" },
						on_invalid = "fail",
						desc = "File operation to match.",
					},
					cwd = { type = "string", on_invalid = "fail", desc = "Working directory glob to match." },
					deny_args = {
						type = "list",
						items = { type = "string", on_invalid = "fail" },
						on_invalid = "fail",
						desc = "Arguments disallowed for an allow rule.",
					},
					scope = { type = "string", on_invalid = "fail", desc = "Scope for an allow rule." },
					justification = { type = "string", on_invalid = "fail", desc = "Reason shown when forbidden." },
					description = { type = "string", on_invalid = "fail", desc = "Human-readable description." },
					name = { type = "string", on_invalid = "fail", desc = "Rule label." },
					expires = { type = "string", on_invalid = "fail", desc = "Rule lifetime." },
				},
			},
		},
		untrusted_preset = {
			type = "string",
			default = "read-only",
			desc = "Preset untrusted projects start with, whatever `preset` says.",
		},
		suggest_after = {
			type = "integer",
			default = 3,
			desc = "Consecutive approvals of the same kind before suggesting a wider preset.",
		},
		rules_file = {
			type = "string",
			desc = 'File of persisted rules ("always allow"); default <state>/permissions.json.',
		},
		sensitive = {
			type = "object",
			on_invalid = "fail",
			desc = "Credential path reminders.",
			fields = {
				enabled = { type = "boolean", default = true, on_invalid = "fail" },
				disable = {
					type = "list",
					default = {},
					on_invalid = "fail",
					items = {
						type = "enum",
						on_invalid = "fail",
						choices = {
							"env",
							"ssh_private_keys",
							"netrc",
							"git_credentials",
							"npmrc",
							"pypirc",
							"ssh_dir",
							"aws_dir",
							"azure_dir",
							"gnupg_dir",
							"kube_dir",
							"docker_dir",
							"gcloud_dir",
							"gh_dir",
							"gcp_dir",
							"mare_auth",
							"mare_mcp_oauth",
							"mare_host_token",
						},
					},
				},
				patterns = {
					type = "list",
					default = {},
					on_invalid = "fail",
					items = { type = "string", on_invalid = "fail" },
				},
			},
		},
		-- extra presets: name → { like = built-in or another preset name
		--   (approval behaviour), level?, label?, desc?, writable?, readable?, deny_read?, network? }
		-- an entry named like a built-in overrides its label / description / sandbox fields
		presets = { type = "map", default = {}, desc = "Extra presets by name." },
		-- command parsing: extra fixed redirect targets that keep a command
		-- analyzable, and command words that make it not analyzable
		parse = {
			type = "object",
			desc = "Command parsing tweaks.",
			fields = {
				safe_sinks = {
					type = "list",
					items = { type = "string" },
					default = {},
					desc = "Extra fixed redirect targets that keep a command analyzable.",
				},
				deny_commands = {
					type = "list",
					items = { type = "string" },
					default = {},
					desc = "Command words that make a command not analyzable.",
				},
			},
		},
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "session", "tools", "context" })
		local rules_file = cfg.rules_file or (mare.paths.state .. "/permissions.json")
		local child = mare.mode == "child"
		local lib = require("mare_lib") -- helpers shared by the built-ins

		local P = {
			preset = "auto", -- current preset or "custom"
			applying = false, -- our own sandbox change in progress (not a custom change)
			sandbox_ready = false, -- request gate opens only after host policy matches the preset
			sandbox_error = nil,
			session_cache = {}, -- key -> true (allow_session)
			-- approved escalation increments by kind; the turn store resets at
			-- agent/end
			approved = { session = {}, turn = {} },
			session_rules = {}, -- rules with expires = "session"
			streak = { key = nil, n = 0 },
		}
		local D
		local policy_revision = 0
		local function bump_policy_revision()
			policy_revision = policy_revision + 1
		end
		local function reset_session_grants(force_bump)
			local had = next(P.session_cache) ~= nil
				or next(P.session_rules) ~= nil
				or next(P.approved.session) ~= nil
				or next(P.approved.turn) ~= nil
			P.session_cache = {}
			P.session_rules = {}
			P.approved = { session = {}, turn = {} }
			P.streak = { key = nil, n = 0 }
			if force_bump or had then
				bump_policy_revision()
				if had and svc.context then
					svc.context:invalidate()
				end
			end
		end

		---------------------------------------------------------------------------
		-- small helpers
		---------------------------------------------------------------------------
		-- state.json through mare.state (locked, atomic; a corrupt file is never overwritten);
		-- writes go through lib.save_state (never from a child, failures logged)
		-- rules file (the shared "always allow" table): locked read-merge-write and change
		-- detection live in permissions.rules_file
		local shared = require("permissions.rules_file")(rules_file, bump_policy_revision)
		local function persist_rule(rule)
			local ok, err = shared.add(rule)
			if ok then
				bump_policy_revision()
			else
				mare.log("warn", "permissions: could not save the rule: " .. tostring(err))
			end
		end
		-- rule sources contributed by other plugins (service `add_rules`, removed on dispose)
		local plugin_rules = {}
		local function all_rules()
			local out = {}
			local lists = { cfg.rules or {}, shared.list(), P.session_rules }
			for _, src in ipairs(plugin_rules) do
				lists[#lists + 1] = src
			end
			for _, list in ipairs(lists) do
				for _, r in ipairs(list) do
					out[#out + 1] = r
				end
			end
			return out
		end
		-- coroutines judging a sub-agent's call for the host (rule_verdict): nothing is logged
		local quiet = setmetatable({}, { __mode = "k" })
		local function record(kind, data)
			if svc.session and not quiet[coroutine.running()] then
				pcall(svc.session.add_record, svc.session, "permission/" .. kind, data)
			end
		end
		-- missing roots are skipped with a one-time notice (permissions.writable_roots)
		local roots = require("permissions.writable_roots")
		local function existing_dirs(list)
			return roots.existing(ctx, list)
		end
		local copy = lib.copy_deep

		-- `mare doctor` / `/doctor`: configured writable directories that don't exist are skipped
		-- as sandbox roots; say so instead of staying silent
		ctx:on("kernel/doctor", function(checks)
			local all = {}
			local own = cfg.writable or {}
			table.move(own, 1, #own, 1, all)
			for _, preset in pairs(cfg.presets or {}) do
				local l = preset.writable or {}
				table.move(l, 1, #l, #all + 1, all)
			end
			local missing = roots.missing(all)
			if #missing > 0 then
				checks[#checks + 1] = {
					name = "sandbox writable",
					ok = false,
					warn = true,
					detail = "configured writable directories don't exist and are skipped (not writable in the sandbox): "
						.. table.concat(missing, ", "),
				}
			end
		end)

		-- structural submodules (plugins/lua/permissions/), loaded here in dependency order
		D = {
			svc = svc,
			ctx = ctx,
			cfg = cfg,
			lib = lib,
			child = child,
			P = P,
			copy = copy,
			shared = shared,
			persist_rule = persist_rule,
			bump_policy_revision = bump_policy_revision,
			reset_session_grants = reset_session_grants,
			policy_revision = function()
				return policy_revision
			end,
			plugin_rules = plugin_rules,
			all_rules = all_rules,
			quiet = quiet,
			record = record,
			roots = roots,
			existing_dirs = existing_dirs,
		}
		-- the vote path (argv parsing, rule matching, credential paths, the vote itself) registers
		-- nothing when loaded: each module loads the first time its field on D is read, normally
		-- the first tool call, instead of during startup
		local function cold(loader)
			local _, proxy = lib.lazy_group({}, {}, loader)
			return proxy
		end
		D.argv = cold(function()
			return require("permissions.argv")(D)
		end)
		D.rules = cold(function()
			return require("permissions.rules")(D)
		end)
		D.sensitive = cold(function()
			return require("permissions.sensitive")(D)
		end)
		D.vote = cold(function()
			return require("permissions.vote")(D)
		end)
		D.presets = require("permissions.presets")(D)
		D.escalation = require("permissions.escalation")(D)
		D.request = require("permissions.request")(D)
		-- configured rules and credential patterns still fail startup when invalid; the defaults
		-- (both empty) need no check, so they leave the modules cold
		if type(cfg.rules) ~= "table" or next(cfg.rules) ~= nil then
			local rules_ok, rules_err = D.rules.validate_source(cfg.rules or {})
			assert(rules_ok, rules_err)
		end
		if
			type(cfg.sensitive) == "table"
			and type(cfg.sensitive.patterns) == "table"
			and next(cfg.sensitive.patterns)
		then
			-- loading it checks every pattern
			local _ = D.sensitive.collect
		end
		require("permissions.approvals")(D)
		require("permissions.lifecycle")(D)
		require("permissions.command")(D)
		require("permissions.prompt")(D)
	end,
}

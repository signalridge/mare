local mare, C, core, ui_layout_factory, validate_factory = ...
local cfg = (core and core.config) or {}
local state = cfg.state
if not state then
	state = {}
	cfg.state = state
end
state.sources = state.sources or {}
state.warnings = state.warnings or {}
-- plugin -> the first invalid `on_invalid = "fail"` option: that plugin does not start (an
-- essential service failing to start fails startup); reset with the warnings per evaluation
state.plugin_failed = state.plugin_failed or {}
state.plugin_error = state.plugin_error or {}
state.layer = state.layer or "user"
state.plugin_defaults = state.plugin_defaults or {}
-- plugin name → its `config_schema`: what check_plugin checks values against
state.plugin_schemas = state.plugin_schemas or {}
state.plugin_slots = state.plugin_slots or {}
state.pending_on = state.pending_on or {}
-- Plugins may mark configuration paths as sensitive for user-facing diagnostics. Keep old
-- marks across reloads: failing to unmark a field is safer than showing a previously hidden key.
state.sensitive_config = state.sensitive_config or {}
function cfg.in_config()
	local s = cfg.state
	return s ~= nil and s.in_config == true and s.config_coroutine == coroutine.running()
end
function mare.config_sensitive(path)
	assert(type(path) == "string" and path ~= "", "config_sensitive expects a non-empty dotted path")
	state.sensitive_config[path] = true
end
function mare.config_sensitive_paths()
	local out = {}
	for path in pairs(state.sensitive_config) do
		out[path] = true
	end
	return out
end

local closest, luatype, validate_working

local WORKING_GLYPHS = {
	braille = true,
	ember = true,
	moon = true,
	phase = true,
	swell = true,
	wave = true,
	sail = true,
	bullet = true,
	pulse = true,
	none = true,
}

local WORKING_PRESETS = {
	"combined",
	"mare",
	"spinner",
	"codex",
	"cc",
	"kimi",
	"shimmer",
	"pulse",
	"dots",
	"wave",
	"foam",
	"sail",
	"sea",
}

validate_working = function(v, path, say)
	if type(v) ~= "table" then
		return true
	end
	local ok = true
	local custom_glyphs = type(v.glyphs) == "table" and v.glyphs or nil
	if not custom_glyphs and type(C.ui) == "table" and type(C.ui.working) == "table" then
		custom_glyphs = type(C.ui.working.glyphs) == "table" and C.ui.working.glyphs or nil
	end
	local function check_glyph(g, where_path)
		if type(g) ~= "string" then
			return
		end
		local cg = custom_glyphs and type(custom_glyphs[g]) == "table" and custom_glyphs[g] or nil
		if not WORKING_GLYPHS[g] and not cg then
			local near = closest and closest(g, WORKING_GLYPHS) or nil
			say(
				("option `%s`: unknown glyph `%s`%s"):format(
					where_path or (path .. ".glyph"),
					g,
					near and (" — did you mean `" .. near .. "`?") or ""
				)
			)
			ok = false
		end
	end
	local check_glyph_table
	check_glyph_table = function(gt, gt_path, is_ascii)
		if type(gt) ~= "table" then
			say(("option `%s`: glyph entry must be a table, got %s"):format(gt_path, luatype(gt)))
			ok = false
			return
		end
		local has_render = type(gt.render) == "function"
		local has_frames = type(gt.frames) == "table" and #gt.frames > 0
		if not has_render and not has_frames then
			say(("option `%s`: glyph entry requires `frames` (non-empty list) or `render` function"):format(gt_path))
			ok = false
		elseif has_frames then
			for i, f in ipairs(gt.frames) do
				if type(f) ~= "string" then
					say(("option `%s.frames[%d]`: expected string, got %s"):format(gt_path, i, luatype(f)))
					ok = false
					break
				end
			end
		end
		if gt.still ~= nil and type(gt.still) ~= "string" then
			say(("option `%s.still`: expected string, got %s"):format(gt_path, luatype(gt.still)))
			ok = false
		end
		if gt.ms ~= nil and (type(gt.ms) ~= "number" or gt.ms ~= gt.ms or gt.ms <= 0) then
			say(("option `%s.ms`: expected positive number, got %s"):format(gt_path, tostring(gt.ms)))
			ok = false
		end
		if not is_ascii and gt.ascii ~= nil then
			check_glyph_table(gt.ascii, gt_path .. ".ascii", true)
		end
	end
	if v.glyphs ~= nil then
		if type(v.glyphs) ~= "table" then
			say(("option `%s.glyphs`: expected a table, got %s"):format(path, luatype(v.glyphs)))
			ok = false
		else
			for gname, gval in pairs(v.glyphs) do
				if type(gname) ~= "string" or gname == "" then
					say(("option `%s.glyphs`: glyph name must be a non-empty string"):format(path))
					ok = false
				else
					check_glyph_table(gval, path .. ".glyphs." .. gname)
				end
			end
		end
	end
	if v.glyph ~= nil then
		check_glyph(v.glyph, path .. ".glyph")
	end
	if v.frames ~= nil then
		check_glyph_table(v.frames, path .. ".frames", false)
	end
	if v.verbs ~= nil then
		if type(v.verbs) ~= "table" then
			say(("option `%s.verbs`: expected a table, got %s"):format(path, luatype(v.verbs)))
			ok = false
		else
			for k, val in pairs(v.verbs) do
				if type(k) ~= "string" or k == "" or type(val) ~= "string" then
					say(("option `%s.verbs`: entries must map string phase names to strings"):format(path))
					ok = false
					break
				end
			end
		end
	end
	return ok
end

--- Core options: name -> { type, default, desc }. All option writes via RPC `options.set` are
--- primary-only (admin owner).
--- Plugins' options are declared in their `config_schema`.
mare.options = mare.options
	or {
		model = { type = "string", desc = "model as provider/id" },
		agents = {
			type = "table",
			luals_type = "mare.AgentLimits",
			default = { max_active = 16, max_depth = 4 },
			desc = "root-shared subagent limits: non-negative integers max_active (all live descendant threads) and max_depth (edges from root); zero disables delegation",
		},
		effort = { invocation_only = true, type = "string", desc = "invocation-only model effort override" },
		theme = {
			type = "table",
			luals_type = "table<mare.StyleName, string|mare.Style>",
			desc = "named style overrides { MareX = spec }",
		},
		editor = { type = "string", desc = "editor for Ctrl+G (default $VISUAL / $EDITOR)" },
		keys = { type = "table", luals_type = "table<string, string|false>", desc = "key -> action shorthand" },
		perf = {
			type = "table",
			desc = "cache sizes (entries) { highlight_cache = 256, markdown_cache = 128, wrap_cache = 1024, preview_cache = 256 }",
		},
		models = { type = "table", luals_type = "table<string, table>", desc = "per-model overrides" },
		providers = {
			type = "table",
			luals_type = "table<string, mare.ProviderOpts>",
			desc = "providers (mare.provider): { base_url, api_key, headers, query_params, retries, stream_idle_timeout_ms, connect_timeout_ms, wire, wire_api, login_hint, proxy, hosted_tools, cache_retention } plus wire-declared provider_fields",
		},
		cache_retention = {
			type = "string",
			luals_type = '"none"|"short"|"long"',
			default = "short",
			desc = "prompt cache retention tier: none|short|long (default short)",
		},
		fallback_models = {
			type = "list",
			items = { type = "string", on_invalid = "error" },
			max_items = 3,
			on_invalid = "error",
			luals_type = "string[]",
			desc = "up to 3 models (provider/id or alias) tried in order when the current one is overloaded, missing or out of quota",
		},
		peer = {
			type = { "boolean", "table" },
			desc = 'peer socket + registry: false disables; { name = "...", allow = { "agent.prompt", "events.subscribe", ... } } names the session and opens methods beyond the peer defaults; inbound messages always wait for the user',
		},
		webserver = {
			type = "table",
			luals_type = "mare.WebserverOpts",
			default = {
				enabled = false,
				host = "127.0.0.1",
				port = 0,
				trusted_hosts = {},
				allowed_origins = {},
				remote_admin = false,
				plain_http = false,
				max_connections = 256,
				header_read_timeout_ms = 30000,
				ws_max_message_bytes = 64 * 1024 * 1024,
				ws_ping_interval_ms = 15000,
				ws_pong_timeout_ms = 10000,
				accept_partial = {},
			},
			desc = "Host HTTP/WebSocket RPC listener: { enabled, host, port, token?, trusted_hosts, allowed_origins, remote_admin, tls_cert?, tls_key?, plain_http?, max_connections, header_read_timeout_ms, ws_max_message_bytes, ws_max_frame_bytes, ws_ping_interval_ms, ws_pong_timeout_ms, accept_partial }",
		},
		border = {
			type = "string",
			default = "rounded",
			desc = "panel/popup borders: rounded|single|double|thick|hidden",
		},
		icons = { type = "string", desc = "icon set: nerd|unicode|ascii (default: auto-detect)" },
		spinner = {
			type = "table",
			desc = 'spinner glyphs: a list { "a", "b", … } or { frames = {…}, still = "•" } (still: the marker under reduce_motion); default braille',
		},
		markdown = {
			type = "table",
			desc = 'Markdown rendering: { glyphs = { bullet1 = "•", rule = "─", … }, code_line_numbers = false }',
		},
		images_protocol = {
			type = "string",
			desc = "terminal image protocol: kitty|iterm2|sixel|none (default: detected)",
		},
		copy = {
			type = "table",
			desc = "/copy: { always_full = true } skips the code-block picker and copies the whole response",
		},
		flavor = {
			type = "string",
			default = "auto",
			desc = "theme flavor: auto (OSC 11 background query)|dark|light|installed name",
		},
		flavor_dark = {
			type = "string",
			default = "dark",
			desc = "flavor selected by auto on a dark or unknown terminal background",
		},
		flavor_light = {
			type = "string",
			default = "light",
			desc = "flavor selected by auto on a light terminal background",
		},
		logo = {
			type = { "boolean", "string" },
			desc = 'startup logo on a new session: true (pixel logo) | false | "compact" (one line)',
		},
		terminal_caps = {
			type = "table",
			desc = 'terminal capability overrides { { match = { TERM_PROGRAM = "…" }, caps = { sync = false, … } }, … }; ui/caps listeners may also adjust them',
		},
		paste_burst = {
			type = "boolean",
			desc = "treat a fast stream of keys as a paste (an Enter inside it inserts a newline); default on Windows only, where pastes aren't bracketed",
		},
		key_conflicts = {
			type = "table",
			desc = 'extra known key conflicts { { key = "ctrl+tab", terminal = "ghostty"|"any", by = "…", fix = "…" }, … } (appended to the built-in table)',
		},
		char_widths = {
			type = "table",
			desc = 'display width overrides { { from = 0x2600, to = 0x26FF, width = 2 }, { char = "⚡", width = 1 }, … }',
		},
		prompt = { type = "string", default = "❯", desc = "input prompt symbol" },
		update_check = {
			type = "boolean",
			default = true,
			desc = "check GitHub releases for a newer mare (at most daily, interactive sessions; nothing else is sent)",
		},
		news = {
			type = "boolean",
			default = true,
			desc = "after mare or an enabled plugin is updated, show its NEWS.md once in the scrollback",
		},
		plugins = {
			type = "table",
			default = {
				url_format = "https://github.com/%s.git",
				allow_insecure = false,
				timeout = 120,
				concurrency = 8,
				sources = {},
			},
			desc = 'packages: { url_format = "https://github.com/%s.git" (short names), allow_insecure = false (http://, git://), timeout = 120 (seconds per git operation), concurrency = 8 (parallel fetches), sources = { "owner/repo", … } (more extras catalogs) }',
		},
		net = {
			type = "table",
			default = {
				fake_ip = "off",
			},
			desc = 'network authorization: { fake_ip = "off"|"auto" } ("off" default; "auto" recognizes fake-IP 198.18.0.0/15 resolution shape and relaxes non-public checks for it)',
		},
		reduce_motion = {
			type = "boolean",
			default = false,
			desc = "no spinner animation or other motion: static status text instead",
		},
		terminal = {
			type = "table",
			luals_type = "mare.TerminalOpts",
			default = {
				ambiwidth = "single",
				color = "auto",
				probe = {
					enabled = true,
					background = true,
				},
			},
			fields = {
				ambiwidth = {
					type = "enum",
					choices = { "single", "double" },
					default = "single",
					apply = "live",
					desc = "width of East Asian ambiguous characters (bullets, ✓, ⚠ …): single|double, like Vim (default single)",
				},
				color = {
					type = "string",
					choices = { "auto", "truecolor", "256", "16", "none" },
					default = "auto",
					desc = 'terminal color depth: "auto" | "truecolor" | "256" | "16" | "none"',
				},
				probe = {
					type = "table",
					default = { enabled = true, background = true },
					fields = {
						enabled = {
							type = "boolean",
							default = true,
							desc = "enable post-first-frame terminal capability probe",
						},
						background = {
							type = "boolean",
							default = true,
							desc = "include OSC 11 background query in the terminal probe",
						},
					},
					desc = "startup terminal probe controls",
				},
			},
			desc = "terminal capability, probing, and ambiwidth configuration (`terminal.{ambiwidth,color,probe.{enabled,background}}`)",
		},
		ui = {
			type = "table",
			luals_type = "mare.UiOpts",
			default = {
				layout = {
					regions = {},
				},
				working = {
					preset = "combined",
					glyph = "swell",
					label = "glint",
					elapsed = "auto",
					tokens = true,
					interrupt_hint = true,
					tips = true,
					sep = "dot",
					tools = "still",
					text_throttle_ms = 250,
				},
				tool_call = {
					spinner = "braille",
				},
				stopping = {
					spinner = true,
				},
			},
			fields = {
				layout = {
					type = "table",
					desc = "stacked region layout configuration: { order = { ... }, regions = { ... } }",
				},
				working = {
					type = "table",
					default = {
						preset = "combined",
						glyph = "swell",
						label = "glint",
						elapsed = "auto",
						tokens = true,
						interrupt_hint = true,
						tips = true,
						sep = "dot",
						tools = "still",
						text_throttle_ms = 250,
					},
					fields = {
						preset = {
							type = "string",
							default = "combined",
							choices = WORKING_PRESETS,
							choices_from = "presets",
							desc = "working line preset (`combined|mare|spinner|codex|cc|kimi|shimmer|pulse|dots|wave|foam|sail|sea` or custom key in `presets`)",
						},
						presets = {
							type = "map",
							default = {},
							desc = "named custom working line presets referenced by `preset`",
						},
						glyph = {
							type = "string",
							default = "swell",
							desc = "working glyph name (`swell|ember|braille|moon|phase|wave|sail|bullet|pulse|none` or custom name in `glyphs`)",
						},
						label = {
							type = "enum",
							choices = { "accent", "shimmer", "glint", "foam", "dots" },
							default = "glint",
							desc = "working label treatment: `accent|shimmer|glint|foam|dots`",
						},
						elapsed = {
							type = "enum",
							choices = { "auto", "seconds", "off" },
							default = "auto",
							desc = "elapsed time display: `auto` (`59s`, `1m 05s`), `seconds` (`65s`), or `off`",
						},
						tokens = {
							type = "boolean",
							default = true,
							desc = "show detail / token estimate (`~1.2k`)",
						},
						interrupt_hint = {
							type = "boolean",
							default = true,
							desc = "show keymap-derived interrupt hint (`Esc to interrupt`)",
						},
						tips = {
							type = { "boolean", "table" },
							default = true,
							desc = "show occasional usage tips next to the working line (`true`, `false`, or custom list/table)",
						},
						sep = {
							type = "enum",
							choices = { "dot", "bullet" },
							default = "dot",
							desc = "tail separator: `dot` (`·`) or `bullet` (`•`)",
						},
						tools = {
							type = "enum",
							choices = { "still", "spin" },
							default = "still",
							desc = "running tool-row indicator when `ui.tool_call.spinner` is unset: `still` or `spin`",
						},
						interval_ms = {
							type = "number",
							min = 1,
							desc = "optional custom glyph animation interval in milliseconds",
						},
						text_throttle_ms = {
							type = "number",
							min = 0,
							default = 250,
							desc = "streaming working-line update throttle in milliseconds",
						},
						frames = {
							type = "table",
							desc = "inline custom glyph frame spec `{ frames = {...}, ms?, still?, ascii?, tint?, hold?, render? }`",
						},
						verbs = {
							type = "table",
							desc = "per-phase label overrides `{ reasoning?, responding?, tool?, shell?, exploring? }`",
						},
						glyphs = {
							type = "table",
							desc = "named custom glyph frame specs referenced by `glyph`",
						},
					},
					desc = "working indicator fine-grained options (`glyph`, `label`, `elapsed`, `tokens`, `interrupt_hint`, `sep`, `tools`, `interval_ms`, `text_throttle_ms`, `frames`, `verbs`, `glyphs`)",
				},
				tool_call = {
					type = "table",
					default = { spinner = "braille" },
					fields = {
						spinner = {
							type = { "boolean", "string", "table" },
							default = "braille",
							desc = "tool-call running spinner glyph name, frame spec, or false for static dot",
						},
					},
					desc = "tool-call presentation options: { spinner = ... }",
				},
				stopping = {
					type = "table",
					default = { spinner = true },
					fields = {
						spinner = {
							type = { "boolean", "string", "table" },
							default = true,
							desc = "stopping indicator spinner glyph name, frame spec, true (inherit working glyph), or false",
						},
					},
					desc = "stopping indicator presentation options: { spinner = ... }",
				},
			},
			desc = "UI configuration: { layout = { ... }, working = ..., tool_call = { ... }, stopping = { ... } }",
		},
		favorite_models = {
			type = "table",
			desc = "models (provider/id or alias) the model-cycle key steps through; without it, the recently used models",
		},
		trust_default = {
			type = "string",
			default = "ask",
			desc = "project trust for a folder with no saved decision (after --trust/--no-trust and trust/resolve plugins): ask|trust|distrust",
		},
		watchdog_ms = {
			invocation_only = true,
			invocation_note = "the watchdog starts before init.lua",
			type = "number",
			luals_type = "integer",
			default = 10000,
			desc = "per-callback limit for Lua that never yields (ms). CLI/env only: --set watchdog_ms=… or MARE_WATCHDOG_MS (the watchdog starts before init.lua, so a value written in init.lua has no effect)",
		},
		lua_memory_mb = {
			invocation_only = true,
			invocation_note = "the VM memory limit is applied before init.lua",
			type = "number",
			luals_type = "integer",
			default = 512,
			desc = "Lua VM memory limit in MiB (0 disables the limit). CLI/env only: --set lua_memory_mb=… or MARE_LUA_MEMORY_MB (the VM memory limit is applied before init.lua, so a value written in init.lua has no effect)",
		},
		policy_timeout_ms = {
			type = "number",
			luals_type = "integer",
			default = 30000,
			desc = "default timeout for policy/rewrite listeners without explicit timeout_ms (ms)",
		},
		shutdown_grace_ms = {
			type = "number",
			luals_type = "integer",
			default = 4000,
			desc = "grace period (ms) for cooperative tasks during host shutdown before process-tree termination",
		},
		stall_ms = {
			type = "number",
			luals_type = "integer",
			default = 10000,
			desc = "a plugin still loading, unloading or waiting for a service after this many ms is reported (/plugins tree, `mare plugin tree --json`, the log, a notice); never aborted; 0 turns it off",
		},
		slow_callback_ms = {
			type = "number",
			default = 1000,
			desc = "Lua callback busy-time threshold in ms for slow-callback warnings (default 1000 ms; /profile aggregates callbacks over 2 ms)",
		},
		slow_callback_record_ms = {
			type = "number",
			default = 1000,
			desc = "Lua callback busy-time threshold in ms above which individual slow callbacks are recorded in /profile",
		},
		slow_callback_table_rows = {
			type = "number",
			luals_type = "integer",
			default = 100,
			desc = "maximum number of (plugin, event) rows shown in the /profile callback timing table",
		},
	}
-- structural keys written by config functions (not user-facing options)
local STRUCT = {
	plugins = true,
	packages = true,
	tool_commands = true,
	tools_disabled = true,
	tools_excluded = true,
	tools_enabled = true,
	tool_descriptions = true,
	prompt_sections = true,
	prompt_removed = true,
	command_aliases = true,
	model_aliases = true,
}

--- optimal string alignment distance (Levenshtein + adjacent transpositions, so `modle` → `model` is 1)
local function lev(a, b)
	if a == b then
		return 0
	end
	local la, lb = #a, #b
	if la == 0 then
		return lb
	end
	if lb == 0 then
		return la
	end
	local d = {}
	for i = 0, la do
		d[i] = { [0] = i }
	end
	for j = 0, lb do
		d[0][j] = j
	end
	for i = 1, la do
		local ca = a:byte(i)
		for j = 1, lb do
			local cb = b:byte(j)
			local v = math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + ((ca == cb) and 0 or 1))
			if i > 1 and j > 1 and ca == b:byte(j - 1) and a:byte(i - 1) == cb then
				v = math.min(v, d[i - 2][j - 2] + 1)
			end
			d[i][j] = v
		end
	end
	return d[la][lb]
end

--- closest name in `names` (a set or list) or nil
closest = function(name, names)
	local best, bd = nil, math.huge
	for k, v in pairs(names) do
		local cand = type(k) == "string" and k or v
		if type(cand) == "string" then
			local d = lev(name, cand)
			if d < bd or (d == bd and cand < best) then
				best, bd = cand, d
			end -- deterministic ties
		end
	end
	if best and bd <= math.max(2, #name // 3) then
		return best
	end
	return nil
end

--- Single preset-as-data resolver shared by core options (`ui.working`), built-in plugins
--- (`permissions`), and user plugins.
--- `spec` = { defaults = {...}, presets = { [name] = {...} }, extra? = {...}, default_preset? = string, inherit_key? = "like" }
--- `value` = preset name string, config table `{ preset?, presets?, ...leaf overrides }`, or nil.
--- Returns `(resolved, err, meta)` where `meta = { preset, base, presets, resolve_one }`.
function mare.config_resolve_preset(spec, value)
	assert(type(spec) == "table", "config_resolve_preset: spec must be a table")
	local inherit_key = spec.inherit_key or "like"
	local base_presets = type(spec.presets) == "table" and spec.presets or {}
	local stop_at = type(spec.stop_at) == "table" and spec.stop_at or nil
	local extra = (type(value) == "table" and type(value.presets) == "table" and value.presets)
		or (type(spec.extra) == "table" and spec.extra)
		or {}
	local merged_presets = {}
	for name, def in pairs(base_presets) do
		if type(def) == "table" then
			local copy = {}
			for k, v in pairs(def) do
				copy[k] = v
			end
			merged_presets[name] = copy
		end
	end
	for name, def in pairs(extra) do
		if type(def) == "table" then
			local base = merged_presets[name]
			if base then
				for k, v in pairs(def) do
					base[k] = v
				end
			else
				local copy = {}
				for k, v in pairs(def) do
					copy[k] = v
				end
				merged_presets[name] = copy
			end
		end
	end

	local cache, cache_base = {}, {}
	local function resolve_one(name, visiting)
		if cache[name] then
			return cache[name], nil, cache_base[name]
		end
		local raw = merged_presets[name]
		if type(raw) ~= "table" then
			local near = type(name) == "string" and closest(name, merged_presets) or nil
			return nil,
				("unknown preset `%s`%s"):format(tostring(name), near and (" — did you mean `" .. near .. "`?") or "")
		end
		visiting = visiting or {}
		if visiting[name] then
			return nil, ("preset inheritance cycle involving `%s`"):format(tostring(name))
		end
		visiting[name] = true
		local out = {}
		local root_base = name
		local parent_name = raw[inherit_key]
		if parent_name ~= nil and parent_name ~= name then
			if type(parent_name) ~= "string" then
				visiting[name] = nil
				return nil, ("invalid preset `%s` target for `%s`"):format(tostring(inherit_key), tostring(name))
			end
			if stop_at and stop_at[parent_name] then
				local base_def = merged_presets[parent_name] or base_presets[parent_name]
				if type(base_def) == "table" then
					for k, v in pairs(base_def) do
						if k ~= inherit_key then
							out[k] = v
						end
					end
				end
				root_base = parent_name
			else
				local parent, err, pbase = resolve_one(parent_name, visiting)
				if err then
					visiting[name] = nil
					return nil, err
				end
				for k, v in pairs(parent) do
					out[k] = v
				end
				root_base = pbase or parent_name
			end
		elseif type(parent_name) == "string" then
			root_base = parent_name
		end
		for k, v in pairs(raw) do
			if k ~= inherit_key then
				out[k] = v
			end
		end
		visiting[name] = nil
		cache[name] = out
		cache_base[name] = root_base
		return out, nil, root_base
	end

	local chosen
	if type(value) == "string" then
		chosen = value
	elseif type(value) == "table" and value.preset ~= nil then
		chosen = value.preset
	else
		chosen = spec.default_preset
	end

	local out = {}
	if type(spec.defaults) == "table" then
		for k, v in pairs(spec.defaults) do
			out[k] = v
		end
	end

	local root_base
	if chosen ~= nil then
		local resolved, err, base = resolve_one(chosen)
		if err then
			return nil, err, { preset = chosen, presets = merged_presets, resolve_one = resolve_one }
		end
		for k, v in pairs(resolved) do
			out[k] = v
		end
		root_base = base
	end

	if type(value) == "table" then
		for k, v in pairs(value) do
			if k ~= "preset" and k ~= "presets" and k ~= inherit_key then
				out[k] = v
			end
		end
	end

	return out, nil, { preset = chosen, base = root_base, presets = merged_presets, resolve_one = resolve_one }
end

local function where()
	return mare.caller_site and mare.caller_site() or nil
end

function luatype(v)
	if mare.json and mare.json.null and rawequal(v, mare.json.null) then
		return "null"
	end
	local t = type(v)
	if t == "number" then
		return "number"
	end
	return t
end

local function current_layer()
	return state.layer
end

local function record(key, layer, at)
	state.sources[key] = { layer = layer or current_layer(), where = at }
end

-- an empty table has no leaves to record either way
local function is_list_value(t)
	return type(t) == "table" and #t > 0
end

--- record `key` and, below a table, every leaf (sources are kept per leaf;
--- a list is one value, so one leaf)
local function record_leaves(key, value, layer, at)
	record(key, layer, at)
	if type(value) == "table" and not is_list_value(value) then
		for k, v in pairs(value) do
			if type(k) == "string" then
				record_leaves(key .. "." .. k, v, layer, at)
			end
		end
	end
end

local function deepcopy(v, depth)
	if type(v) ~= "table" then
		return v
	end
	depth = depth or 0
	if depth > 64 then
		return v
	end
	local out = {}
	for k, x in pairs(v) do
		out[k] = deepcopy(x, depth + 1)
	end
	return out
end
state.deepcopy = deepcopy

-- Older option proxies can be retained by callers across a reload. They must not mutate the
-- committed table while the replacement is being evaluated; the config coroutine may write.
local function in_config_scope()
	return cfg.in_config()
end

--- a write made at runtime (after startup installed the runtime surface, outside config
--- execution): it must validate before it commits; a config file's write
--- (startup, /reload, a package or doctor VM) warns instead
local function runtime_write()
	return state.layer_runtime ~= nil and not in_config_scope()
end

local function guard_option_write()
	if cfg.state and cfg.state.stage and not cfg.in_config() then
		error("option update unavailable during config reload; retry afterwards", 3)
	end
	-- Package declarations use the active layer to select their lock file.
	return current_layer()
end
state.guard_option_write = guard_option_write

--- lazy.nvim opts merge: tables merge recursively, lists (and non-tables) replace
local function is_list(t)
	return type(t) == "table" and (#t > 0 or next(t) == nil)
end
local function merge(dst, src)
	for k, v in pairs(src) do
		if type(v) == "table" and not is_list(v) and type(dst[k]) == "table" and not is_list(dst[k]) then
			merge(dst[k], v)
		else
			dst[k] = v
		end
	end
end
state.merge = merge

-- the sources at `key` and below it
local function clear_sources(key)
	for k in pairs(state.sources) do
		if k == key or k:sub(1, #key + 1) == key .. "." then
			state.sources[k] = nil
		end
	end
end

-- Tracks per-evaluation whether an invalid `plugins.preset` invalidated prior preset layers,
-- clearing `C.plugins.preset` (the candidate table, never live `mare.config`) without leaving
-- stale state when a higher layer sets a valid preset.
local function set_preset_state(active)
	state.preset_cleared = not active
	if not active then
		if type(C.plugins) == "table" then
			C.plugins.preset = nil
		end
		clear_sources("plugins.preset")
	end
end

local validate_ui_config = ui_layout_factory(mare, luatype, closest)
local validators = validate_factory(mare, {
	cfg = cfg,
	state = state,
	C = C,
	STRUCT = STRUCT,
	luatype = luatype,
	where = where,
	in_config_scope = in_config_scope,
	merge = merge,
	deepcopy = deepcopy,
	validate_ui_config = validate_ui_config,
	validate_working = validate_working,
	clear_sources = clear_sources,
	closest = closest,
	set_preset_state = set_preset_state,
})
local check_options = validators.check_options
local check_plugin = validators.check_plugin

--- mare.setup { model = ..., keys = {...}, theme = {...}, models = {...} }
function mare.setup(t)
	guard_option_write()
	local at = where()
	check_options(t)
	for k, v in pairs(t) do
		record_leaves(k, v, nil, at)
	end
	merge(C, t)
end

-- the sources at `path` and below it
local function sources_at(path)
	local out = {}
	for sk, src in pairs(state.sources) do
		if sk == path or sk:sub(1, #path + 1) == path .. "." then
			out[sk] = src
		end
	end
	return out
end

--- mare.config_clear(path): clear one layer's override (`nil` cannot say
--- "clear"). In a config file: the value at `path` (any depth) and its sources go, so the default or
--- a later layer applies. At runtime: the runtime write of an option (`name` or
--- `plugins.<plugin>.<option>`) goes and the startup configuration shows through again, with its
--- source, applied and announced like a write. A path nothing set is a no-op.
function mare.config_clear(path)
	guard_option_write()
	assert(type(path) == "string" and path ~= "", "config_clear: path must be a non-empty string")
	if not runtime_write() then
		local parts = {}
		for part in path:gmatch("[^.]+") do
			parts[#parts + 1] = part
		end
		local t = C
		for i = 1, #parts - 1 do
			t = type(t) == "table" and t[parts[i]] or nil
		end
		if type(t) == "table" then
			t[parts[#parts]] = nil
		end
		clear_sources(path)
		return
	end
	if path:find("^plugins%.") then
		if not state.config_clear_runtime(path) then
			error("config_clear at runtime takes a whole option: `name` or `plugins.<plugin>.<option>`", 2)
		end
		return
	end
	if path:find(".", 1, true) then
		error("config_clear at runtime takes a whole option: `name` or `plugins.<plugin>.<option>`", 2)
	end
	local base = state.runtime_base and state.runtime_base[path]
	if base == nil then
		return -- nothing was written at runtime
	end
	state.runtime_base[path] = nil
	if base.existed then
		C[path] = deepcopy(base.value)
	else
		C[path] = nil
	end
	clear_sources(path)
	for sk, src in pairs(base.sources or {}) do
		state.sources[sk] = src
	end
	if state.option_changed then
		state.option_changed(path, mare.o[path])
	end
end

--- mare.plugin(name, opts): deep-merge over the plugin's default opts
function mare.plugin(name, t)
	guard_option_write()
	local at = where()
	if runtime_write() then
		if name == "preset" then
			error("`plugins.preset` cannot be modified at runtime", 0)
		end
		-- a runtime write is one transaction (config_txn.lua): validated, committed, applied by
		-- the owner and announced, or refused with nothing changed
		return state.config_write({ [name] = t or {} }, nil, at)
	end
	if name == "preset" then
		if check_plugin(name, t) then
			set_preset_state(true)
			C.plugins = C.plugins or {}
			C.plugins.preset = t
			record_leaves("plugins.preset", t, nil, at)
		else
			set_preset_state(false)
		end
		return
	end
	check_plugin(name, t)
	C.plugins = C.plugins or {}
	C.plugins[name] = C.plugins[name] or {}
	local own = C.plugins[name]
	merge(own, t or {})
	local schema = state.plugin_schemas[name] or {}
	for k, v in pairs(t or {}) do
		local entry = schema[k]
		if entry and (entry.type == "union" or entry.type == "list") then
			-- a union or a list is one value: replaced, never merged into the old one
			own[k] = deepcopy(v)
			clear_sources("plugins." .. name .. "." .. k)
		end
		record_leaves("plugins." .. name .. "." .. k, v, nil, at)
	end
end

local function key_get(t, path)
	for _, k in ipairs(path) do
		if type(t) ~= "table" then
			return nil
		end
		t = t[k]
	end
	return t
end
state.key_get = key_get
local function key_set(t, path, v)
	for i = 1, #path - 1 do
		if type(t[path[i]]) ~= "table" then
			t[path[i]] = {}
		end
		t = t[path[i]]
	end
	t[path[#path]] = v
end
state.key_set = key_set

--- mare.o.<name>: read (value, falling back to the default) / write (takes effect immediately).
--- A table option the user hasn't set reads through a proxy (defaults are never handed out, so
--- they can't be changed by accident); a dotted write (`mare.o.plugins.url_format = …`) creates the
--- user's table, records the source as `plugins.url_format` and reports the option as changed.
local function validate_runtime_table(tbl)
	local problems = state.validate_options(tbl)
	if #problems > 0 then
		error(table.concat(problems, "; "), 3)
	end
end

local function remember_base(name)
	if state.remember_base then
		state.remember_base(name, C[name] ~= nil, C[name], sources_at(name))
	end
end

local option_proxy
option_proxy = function(name, path)
	local function default_at(p)
		local o = mare.options[name]
		return key_get(o and o.default, p)
	end
	local function merged()
		local base = deepcopy(default_at(path))
		local own = key_get(C[name], path)
		if type(base) ~= "table" then
			base = {}
		end
		if type(own) == "table" then
			merge(base, deepcopy(own))
		end
		return base
	end
	return setmetatable({}, {
		__index = function(_, k)
			local p = { table.unpack(path) }
			p[#p + 1] = k
			local own, d = key_get(C[name], p), default_at(p)
			if type(own) == "table" or (type(d) == "table" and own == nil) then
				-- own fields over the default's, at any depth
				return option_proxy(name, p)
			end
			if own ~= nil then
				return own
			end
			return d
		end,
		__newindex = function(_, k, v)
			guard_option_write()
			if name == "plugins" and #path == 0 and k == "preset" and runtime_write() then
				error("`plugins.preset` cannot be modified at runtime", 0)
			end
			if name == "ui" then
				local cand = type(C[name]) == "table" and deepcopy(C[name]) or {}
				local p = { table.unpack(path) }
				p[#p + 1] = k
				key_set(cand, p, v)
				if runtime_write() then
					validate_runtime_table({ ui = cand })
				else
					local checked = { ui = deepcopy(cand) }
					check_options(checked)
					if
						cand.working_interval_ms ~= nil
						and (type(checked.ui) ~= "table" or checked.ui.working_interval_ms == nil)
					then
						return
					end
					if cand.working ~= nil and (type(checked.ui) ~= "table" or checked.ui.working == nil) then
						return
					end
					local ok_ui = pcall(validate_ui_config, cand)
					if not ok_ui then
						validate_ui_config(cand)
					end
				end
			elseif
				mare.options[name]
				and (mare.options[name].type == "object" or mare.options[name].type == "table")
				and runtime_write()
			then
				local cand = type(C[name]) == "table" and deepcopy(C[name]) or {}
				local p = { table.unpack(path) }
				p[#p + 1] = k
				key_set(cand, p, v)
				validate_runtime_table({ [name] = cand })
			end
			if runtime_write() then
				remember_base(name)
			end
			if type(C[name]) ~= "table" then
				C[name] = {}
			end
			local t = C[name]
			for _, seg in ipairs(path) do
				if type(t[seg]) ~= "table" then
					t[seg] = {}
				end
				t = t[seg]
			end
			t[k] = v
			local key = name
			for _, seg in ipairs(path) do
				key = key .. "." .. tostring(seg)
			end
			record(key .. "." .. tostring(k), state.layer_runtime or nil, where())
			if state.option_changed then
				state.option_changed(name, C[name])
			end
		end,
		__pairs = function()
			return next, merged(), nil
		end,
		__len = function()
			return #merged()
		end,
	})
end

mare.o = setmetatable({}, {
	__index = function(_, name)
		local o, own = mare.options[name], C[name]
		if o and (o.type == "table" or o.type == "object") then
			-- a table option reads own fields over the default's and takes dotted writes; with
			-- nothing to merge (no default) the user's own table is handed out as it is
			if type(o.default) == "table" and (own == nil or type(own) == "table") then
				return option_proxy(name, {})
			elseif own == nil then
				return option_proxy(name, {})
			end
		end
		if own ~= nil then
			return own
		end
		if o then
			return o.default -- may be false: never `o and o.default or nil`
		end
		return nil
	end,
	__newindex = function(_, name, value)
		guard_option_write()
		if not runtime_write() then
			local t = { [name] = value }
			check_options(t)
			if value ~= nil and t[name] == nil then
				return -- invalid: reported, the default stays
			end
		else
			-- a runtime write commits only a valid candidate; the old value stays otherwise
			validate_runtime_table({ [name] = value })
			remember_base(name)
		end
		C[name] = value
		record(name, state.layer_runtime or nil, where())
		if state.option_changed then
			state.option_changed(name, value)
		end
	end,
})

local function on_now_owned(...)
	local off = state.on_now(...)
	table.insert(state.on_unsub, off)
	return off
end

--- mare.on(event, fn): subscribe from init.lua (e.g. "model/changed" for per-model options); a
--- native listener on the host's fiber once the kernel exists, as a plugin's `ctx:on`:
--- a waterfall listener is `fn(value, ..., next)` and passes on with `next(...)`; a decision point
--- is voted on with `mare.policy.voter`, not listened to
function mare.on(event, fn)
	if type(event) ~= "string" or event == "" then
		error("mare.on: the event must be a non-empty string", 2)
	end
	if type(fn) ~= "function" then
		error("mare.on: the listener must be a function", 2)
	end
	if not cfg.in_config() and state.on_now then
		return on_now_owned(event, fn)
	end
	table.insert(state.pending_on, { event, fn })
end

mare.policy = {}
--- mare.policy.voter(point, fn, opts?): vote on a decision point (tool/before, tool/approval, …)
--- from init.lua, as a plugin's `ctx.policy:voter`; `opts` = { timeout_ms? }. The voters run in
--- their declaration order, after the host's readiness vote
function mare.policy.voter(point, fn, opts)
	-- checked here, as it is called: an error is the config evaluation's (rolled back, the old
	-- voters stay), never found only after the commit withdrew them
	-- the decision points are known to a full host only (it sets `policy_points` before
	-- init.lua runs); a short-lived VM (mare plugin / sandbox / doctor, flag discovery) evaluates the
	-- config without them and only collects the declaration
	if type(point) ~= "string" or point == "" then
		error("mare.policy.voter: the point must be a non-empty string", 2)
	end
	local points = state.policy_points
	if points and not points[point] then
		error(("mare.policy.voter: `%s` is not a decision point"):format(point), 2)
	end
	if type(fn) ~= "function" then
		error("mare.policy.voter: the voter must be a function", 2)
	end
	if opts ~= nil and type(opts) ~= "table" then
		error("mare.policy.voter: opts must be a table", 2)
	end
	opts = opts or {}
	for key, value in pairs(opts) do
		-- no `prepend`: init.lua's voters run in their declaration order, after the host's own
		if key == "timeout_ms" then
			if type(value) ~= "number" or value < 0 or value ~= value then
				error("mare.policy.voter: opts.timeout_ms must be a non-negative number", 2)
			end
		else
			error(("mare.policy.voter: unknown option `%s`"):format(tostring(key)), 2)
		end
	end
	if not cfg.in_config() and state.on_now then
		return on_now_owned(point, fn, opts)
	end
	table.insert(state.pending_on, { point, fn, opts })
end

-- Feature-specific config functions (mare.tool.command, mare.model, …) are not defined
-- here: plugins declare them in their `config_api` field and the core installs them
-- (see install_plugin_config_api), so the kernel stays free of feature knowledge.

-- what the runtime write transaction (config_txn.lua) uses of this evaluation's config layer
state.internal = {
	C = C,
	record_leaves = record_leaves,
	clear_sources = clear_sources,
	sources_at = sources_at,
}

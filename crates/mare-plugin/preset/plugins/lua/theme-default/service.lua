-- Declarative theme registry with owned layers and preview/commit separation.
-- No UI host or private state access.
return function(ctx, cfg, defaults)
	local registry, layers, applied, applied_icons = {}, {}, {}, {}
	local aliases = {
		["one-half-dark"] = "one-halfdark",
		["one-half-light"] = "one-halflight",
		["one-halfdark"] = "one-half-dark",
		["one-halflight"] = "one-half-light",
	}
	local function raw_lookup(name)
		if type(name) ~= "string" then
			return nil
		end
		return registry[name] or (aliases[name] and registry[aliases[name]])
	end
	local definition
	local function materialize(entry)
		if not entry or not entry.load then
			return entry
		end
		local def = definition(entry.load(), entry.owner, entry.origin)
		registry[entry.name] = def
		return def
	end
	local function lookup(name)
		return materialize(raw_lookup(name))
	end
	local function matches_name(value, canonical)
		return value == canonical or (value ~= nil and aliases[value] == canonical)
	end
	local original_styles = {}
	for _, name in ipairs(mare.ui.styles()) do
		original_styles[name] = mare.ui.style(name)
	end
	local revision, seq = 0, 0
	local valid_options = {}
	local pending_preference
	local pending_pairs = {}
	-- owners are calling plugins by identity (mare_lib.caller_id: an old ctx's id or a cordis
	-- fiber's uid), since a mare.Service method's caller is a traced ctx, not one stable table
	local preview_owner, preview_owners = nil, {}
	local caller_id = require("mare_lib").caller_id
	local appearance, background, preview = "dark", nil, nil
	local service = {}
	local function copy(v)
		if type(v) ~= "table" then
			return v
		end
		local t = {}
		for k, x in pairs(v) do
			t[k] = copy(x)
		end
		return t
	end
	local function merge(a, b)
		local t = copy(a or {})
		for k, v in pairs(b or {}) do
			t[k] = copy(v)
		end
		return t
	end
	local function valid_name(n)
		assert(
			type(n) == "string" and #n > 0 and #n <= 128 and not n:find("[%z\1-\31\127]"),
			"theme name must be nonempty printable text (max 128 bytes)"
		)
	end

	local function color(v)
		return type(v) == "string" and v:match("^#%x%x%x%x%x%x$")
	end
	local style_keys = {
		fg = true,
		bg = true,
		bold = true,
		dim = true,
		italic = true,
		underline = true,
		strike = true,
		reverse = true,
	}
	local known_styles = {}
	for k in pairs(defaults.palette(defaults.colors.dark)) do
		known_styles[k] = true
	end
	local seen_diagnostics = {}
	local function warn_once(msg)
		if not seen_diagnostics[msg] then
			seen_diagnostics[msg] = true
			ctx.logger:warn(msg)
		end
	end
	local function diag(strict, msg)
		if strict then
			error(msg, 0)
		end
		warn_once(msg)
	end
	local function is_reserved_style(name)
		return type(name) == "string" and name:sub(1, 4) == "Mare" and not known_styles[name]
	end
	local function merge_style_into(combined, name, spec)
		local cur = type(combined[name]) == "table" and copy(combined[name]) or {}
		for k, v in pairs(spec) do
			cur[k] = copy(v)
		end
		combined[name] = cur
	end
	local function sanitize_styles(t, source, allow_reserved, strict)
		if type(t) ~= "table" then
			diag(strict, source .. ": theme styles must be a table")
			return {}
		end
		local out = {}
		for name, s in pairs(t) do
			if type(name) ~= "string" or name == "" then
				diag(strict, source .. ": style name must be a non-empty string")
			elseif not allow_reserved and is_reserved_style(name) then
				diag(strict, ("%s: unknown core style `%s` (`Mare*` prefix is reserved for core)"):format(source, name))
			elseif not strict and type(s) == "string" then
				if color(s) then
					out[name] = { fg = s }
				else
					warn_once(("%s: invalid color `%s` for style `%s`"):format(source, tostring(s), name))
				end
			elseif type(s) ~= "table" then
				diag(strict, ("%s: style `%s` must be a table"):format(source, name))
			else
				local cleaned, any = {}, false
				for k, v in pairs(s) do
					if not style_keys[k] then
						diag(strict, ("%s: unknown field `%s` on style `%s`"):format(source, tostring(k), name))
					elseif k == "fg" or k == "bg" then
						if color(v) then
							cleaned[k] = v
							any = true
						else
							diag(strict, ("%s: invalid color `%s` on `%s.%s`"):format(source, tostring(v), name, k))
						end
					elseif type(v) == "boolean" then
						cleaned[k] = v
						any = true
					else
						diag(strict, ("%s: field `%s.%s` must be a boolean"):format(source, name, tostring(k)))
					end
				end
				if any then
					out[name] = cleaned
				end
			end
		end
		return out
	end
	local function sanitize_palette(t, base, source, strict)
		if type(t) ~= "table" then
			diag(strict, source .. ": theme palette must be a table")
			return {}
		end
		local out = {}
		for k, v in pairs(t) do
			if defaults.colors[base][k] == nil then
				diag(strict, ("%s: unknown palette color `%s`"):format(source, tostring(k)))
			elseif not color(v) then
				diag(strict, ("%s: invalid color `%s` for palette `%s`"):format(source, tostring(v), tostring(k)))
			else
				out[k] = v
			end
		end
		return out
	end
	local function variant(t, origin)
		assert(type(t) == "table", "theme variant must be a table")
		local base = t.base or "dark"
		assert(base == "dark" or base == "light", "theme base must be dark or light")
		local result = { base = base, syntax = t.syntax }
		assert(t.syntax == nil or type(t.syntax) == "string", "syntax must be a tmTheme document")
		if t.syntax then
			mare.ui.syntax_theme(t.syntax, true)
		end
		local strict = origin ~= "user" and origin ~= "config"
		if t.palette then
			assert(type(t.palette) == "table", "theme palette must be a table")
			result.palette = merge(defaults.colors[base], sanitize_palette(t.palette, base, "theme palette", strict))
		else
			result.styles = sanitize_styles(t.styles or {}, "theme styles", origin == "builtin", strict)
		end
		return result
	end
	function definition(def, owner, origin)
		assert(type(def) == "table", "theme definition must be a table")
		valid_name(def.name)
		assert(def.label == nil or type(def.label) == "string", "theme label must be a string")
		assert(def.name ~= "auto", "auto is a preference, not a theme name")
		local icons = copy(def.icons or {})
		assert(type(icons) == "table", "theme icons must be a table")
		for name, glyph in pairs(icons) do
			assert(type(name) == "string" and name:match("^[%w_.-]+$"), "invalid icon name")
			assert(
				type(glyph) == "string" and #glyph > 0 and #glyph <= 128 and not glyph:find("[%z\1-\31\127]"),
				"icon must be a printable glyph"
			)
		end
		local variants = {}
		if def.variants then
			assert(type(def.variants) == "table", "theme variants must be a table")
			for mode, v in pairs(def.variants) do
				assert(mode == "dark" or mode == "light", "variant must be dark or light")
				assert(v.base == nil or v.base == mode, "variant base must match " .. mode)
				v = copy(v)
				v.base = mode
				variants[mode] = variant(v, origin)
			end
			assert(variants.dark and variants.light, "paired theme requires dark and light variants")
		else
			local v = variant(def, origin)
			variants[v.base] = v
		end
		return {
			name = def.name,
			label = def.label or def.name,
			icons = icons,
			variants = variants,
			owner = owner,
			origin = origin or "plugin",
		}
	end
	for _, mode in ipairs({ "dark", "light" }) do
		registry[mode] =
			definition({ name = mode, base = mode, palette = cfg.colors and cfg.colors[mode] or {} }, ctx, "builtin")
	end
	for _, item in ipairs(require("theme-default.bundled")) do
		local modes = copy(item.variants)
		table.sort(modes)
		registry[item.name] = {
			name = item.name,
			label = item.label or item.name,
			variant_modes = modes,
			owner = ctx,
			origin = "builtin",
			load = item.load,
		}
	end
	local directories = mare.plugins.dirs("themes")
	if not mare.no_user_plugins then
		for _, dir in ipairs(cfg.dirs or { mare.paths.config .. "/themes" }) do
			if mare.fs.exists(dir) then
				directories[#directories + 1] = { dir = dir, name = "user" }
			end
		end
	end
	for _, d in ipairs(directories) do
		local files = {}
		for _, e in ipairs(mare.fs.list(d.dir) or {}) do
			if e.kind == "file" and (e.name:match("%.lua$") or e.name:match("%.tmTheme$")) then
				files[#files + 1] = e.name
			end
		end
		table.sort(files)
		for _, file in ipairs(files) do
			local name = file:gsub("%.lua$", ""):gsub("%.tmTheme$", "")
			if name ~= "auto" and name ~= "dark" and name ~= "light" then
				local ok, result = pcall(function()
					local t
					if file:match("%.tmTheme$") then
						t = {
							syntax = assert(mare.fs.read(d.dir .. "/" .. file, { max_bytes = 1024 * 1024 })),
							palette = {},
						}
					else
						t = assert(loadfile(d.dir .. "/" .. file))()
						if type(t) == "table" and type(t.syntax_file) == "string" and t.syntax == nil then
							assert(
								not t.syntax_file:match("^/") and not t.syntax_file:match("%.%."),
								"invalid syntax_file path"
							)
							t.syntax = assert(mare.fs.read(d.dir .. "/" .. t.syntax_file, { max_bytes = 1024 * 1024 }))
						end
					end
					assert(type(t) == "table", "theme file must return a table")
					-- Legacy direct named-style tables remain data, not a second implementation.
					if not t.palette and not t.variants and not t.styles and not t.syntax then
						t = { styles = t }
					end
					t.name = name
					return definition(t, ctx, d.name)
				end)
				if ok then
					local prev = registry[name]
					if prev and prev.origin ~= "builtin" then
						ctx.logger:warn(
							"theme " .. name .. " (" .. d.name .. ") overrides earlier definition from " .. prev.origin
						)
					end
					registry[name] = result
				else
					ctx.logger:warn("theme " .. name .. " not loaded: " .. tostring(result))
				end
			end
		end
	end
	for name, value in pairs(cfg.flavors or {}) do
		assert(type(value) == "table", "config flavor must be a table")
		if
			value.variants ~= nil
			or value.palette ~= nil
			or value.styles ~= nil
			or value.syntax ~= nil
			or value.icons ~= nil
			or value.label ~= nil
			or value.base ~= nil
		then
			local entry = copy(value)
			entry.name = name
			registry[name] = definition(entry, ctx, "config")
		else
			registry[name] = definition({ name = name, styles = value }, ctx, "config")
		end
	end
	local function preference()
		return mare.o.flavor or "auto"
	end
	local function resolved(name)
		if name == "auto" then
			name = mare.o["flavor_" .. appearance] or appearance
		end
		local def = lookup(name)
		if not def then
			require("mare_lib").fail("theme.unknown_theme", "unknown theme: " .. tostring(name))
		end
		return def, def.variants[appearance] or def.variants.dark or def.variants.light
	end
	local function sample()
		return {
			type = "lines",
			items = {
				{ type = "code", lang = "rust", code = 'fn preview() { let value = "theme"; }' },
				{ type = "code", lang = "diff", code = "+ added line\n- removed line" },
				{
					type = "line",
					spans = {
						{ type = "span", text = "Heading", style = "MareHeading" },
						{ type = "span", text = "  muted", style = "MareMuted" },
					},
				},
				{
					type = "line",
					spans = {
						{ type = "span", text = "Success", style = "MareSuccess" },
						{ type = "span", text = "  warning", style = "MareWarning" },
						{ type = "span", text = "  error", style = "MareError" },
					},
				},
			},
		}
	end
	local function snapshot()
		local def, v = resolved(preview or preference())
		local current = {}
		for name in pairs(applied) do
			current[name] = mare.ui.style(name)
		end
		return {
			preference = preference(),
			active = def.name,
			appearance = appearance,
			dark = mare.o.flavor_dark or "dark",
			light = mare.o.flavor_light or "light",
			preview = preview,
			revision = revision,
			styles = current,
			syntax = v.syntax or (v.palette and defaults.syntax(v.palette)),
			component = sample(),
			icons = copy(def.icons),
		}
	end
	local function withdraw_icons()
		for set, entries in pairs(applied_icons) do
			local target = mare.ui.icons and mare.ui.icons[set]
			if type(target) == "table" then
				for name, entry in pairs(entries) do
					if target[name] == entry.written then
						target[name] = entry.prev
					end
				end
			end
		end
		applied_icons = {}
	end
	local function apply()
		local def, v = resolved(preview or preference())
		withdraw_icons()
		for set, icons in pairs(mare.ui.icons or {}) do
			if type(icons) == "table" then
				applied_icons[set] = {}
				for name, glyph in pairs(def.icons) do
					applied_icons[set][name] = { prev = icons[name], written = glyph }
					icons[name] = glyph
				end
			end
		end
		mare.ui.syntax_theme(v.syntax or (v.palette and defaults.syntax(v.palette)))
		local combined = defaults.palette(defaults.colors[v.base])
		local live_theme = mare.o and mare.o.theme
		local user_theme = type(live_theme) == "table" and live_theme
			or (type(mare.config.theme) == "table" and mare.config.theme or {})
		local function user_palette_colors(base)
			local tbl = user_theme.colors
			if type(tbl) ~= "table" then
				return {}
			end
			local flat, sub = {}, {}
			for k, val in pairs(tbl) do
				if k == "dark" or k == "light" then
					if k == base and type(val) == "table" then
						sub = sanitize_palette(val, base, "config.theme.colors." .. base)
					end
				else
					flat[k] = val
				end
			end
			return merge(sanitize_palette(flat, base, "config.theme.colors"), sub)
		end
		if v.palette then
			local p = copy(v.palette)
			if cfg.colors and cfg.colors[v.base] then
				p = merge(p, sanitize_palette(cfg.colors[v.base], v.base, "theme-default.colors." .. v.base))
			end
			p = merge(p, user_palette_colors(v.base))
			if background and cfg.blend_background then
				p = defaults.blend(p, background, v.base, merge(defaults.ratios, cfg.ratios))
			end
			local wf = ctx.bus:waterfall("theme/palette", {
				palette = p,
				flavor = def.name,
				background = background,
			})
			p = (type(wf) == "table" and type(wf.palette) == "table" and wf.palette) or p
			for name, spec in pairs(defaults.palette(p)) do
				merge_style_into(combined, name, spec)
			end
		else
			for name, spec in pairs(v.styles) do
				merge_style_into(combined, name, spec)
			end
		end
		local ordered = {}
		for _, layer in pairs(layers) do
			ordered[#ordered + 1] = layer
		end
		table.sort(ordered, function(a, b)
			return a.seq < b.seq
		end)
		for _, layer in ipairs(ordered) do
			for name, spec in pairs(layer.styles) do
				merge_style_into(combined, name, spec)
			end
		end
		local direct_user = {}
		for name, vv in pairs(user_theme) do
			if name ~= "colors" and name ~= "styles" then
				direct_user[name] = vv
			end
		end
		for name, spec in pairs(sanitize_styles(direct_user, "config.theme", false)) do
			merge_style_into(combined, name, spec)
		end
		if user_theme.styles ~= nil then
			for name, spec in pairs(sanitize_styles(user_theme.styles, "config.theme.styles", false)) do
				merge_style_into(combined, name, spec)
			end
		end
		for name, spec in pairs(mare.ui.style_overrides()) do
			combined[name] = copy(spec)
		end
		for name in pairs(applied) do
			if not combined[name] then
				mare.ui.theme_style(name, {})
			end
		end
		applied = {}
		for name, spec in pairs(combined) do
			mare.ui.theme_style(name, spec)
			applied[name] = true
		end
		for _, key in ipairs({ "flavor", "flavor_dark", "flavor_light" }) do
			valid_options[key] = mare.o[key]
		end
		revision = revision + 1
		ctx.bus:emit("theme/changed", snapshot())
	end
	function service.list()
		local out = {}
		for name, d in pairs(registry) do
			local modes
			if d.load then
				modes = copy(d.variant_modes)
			else
				modes = {}
				for mode in pairs(d.variants) do
					modes[#modes + 1] = mode
				end
				table.sort(modes)
			end
			out[#out + 1] = { name = name, label = d.label, variants = modes, origin = d.origin }
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return { data = mare.json.array(out) }
	end
	function service.inspect()
		local names = {}
		for name in pairs(defaults.palette(defaults.colors.dark)) do
			names[name] = true
		end
		local entries = {}
		for _, d in pairs(registry) do
			entries[#entries + 1] = d
		end
		for _, d in ipairs(entries) do
			local resolved_def = materialize(d)
			for _, v in pairs(resolved_def.variants) do
				for name in pairs(v.styles or defaults.palette(v.palette)) do
					names[name] = true
				end
			end
		end
		for _, layer in pairs(layers) do
			for name in pairs(layer.styles) do
				names[name] = true
			end
		end
		for name in pairs(applied) do
			names[name] = true
		end
		local out = {}
		for name in pairs(names) do
			out[#out + 1] = { name = name, style = mare.ui.style(name) }
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return out
	end
	function service.get()
		return copy(snapshot())
	end
	function service.select(_, _, name, opts)
		-- resolved() raises theme.unknown_theme for a name it does not know (as preview does)
		resolved(name)
		if opts and opts.save then
			mare.state.set("flavor", name)
		end
		pending_preference = nil
		preview_owner = nil
		preview = nil
		mare.o.flavor = name
		apply()
		return snapshot()
	end
	function service.preview(_, caller, name)
		resolved(name)
		local owner = caller_id(caller) or caller
		preview_owner = owner
		if not preview_owners[owner] then
			preview_owners[owner] = true
			caller:effect(function()
				return function()
					preview_owners[owner] = nil
					if preview_owner == owner then
						preview_owner = nil
						preview = nil
						apply()
					end
				end
			end, "theme preview")
		end
		preview = name
		apply()
		return snapshot()
	end
	function service.cancel_preview()
		preview_owner = nil
		preview = nil
		apply()
		return snapshot()
	end
	function service.register(_, caller, def)
		local d = definition(def, caller_id(caller))
		assert(not registry[d.name], "theme already registered: " .. d.name)
		registry[d.name] = d
		for mode, name in pairs(pending_pairs) do
			if matches_name(name, d.name) then
				mare.o["flavor_" .. mode] = name
				pending_pairs[mode] = nil
			end
		end
		if matches_name(pending_preference, d.name) then
			mare.o.flavor = pending_preference
			pending_preference = nil
		end
		apply()
		local function remove()
			if registry[d.name] ~= d then
				return
			end
			registry[d.name] = nil
			if matches_name(preview, d.name) then
				preview_owner = nil
				preview = nil
			end
			if matches_name(preference(), d.name) then
				mare.o.flavor = "auto"
			end
			for _, mode in ipairs({ "dark", "light" }) do
				if matches_name(mare.o["flavor_" .. mode], d.name) then
					mare.o["flavor_" .. mode] = mode
				end
			end
			apply()
		end
		caller:effect(function()
			return remove
		end, "theme " .. d.name)
		return remove
	end
	function service.register_override(_, caller, layer_id, value)
		valid_name(layer_id)
		local owner = caller_id(caller) or caller
		local old = layers[layer_id]
		assert(not old or old.owner == owner, "override owned by another caller")
		seq = seq + 1
		local layer = { owner = owner, seq = seq, styles = sanitize_styles(value, "theme override", false, true) }
		layers[layer_id] = layer
		apply()
		local function remove()
			if layers[layer_id] == layer then
				layers[layer_id] = nil
				apply()
			end
		end
		caller:effect(function()
			return remove
		end, "theme override " .. layer_id)
		return remove
	end
	ctx:effect(function()
		return function()
			withdraw_icons()
			for name in pairs(applied) do
				mare.ui.theme_style(name, original_styles[name] or {})
			end
			for name, style in pairs(mare.ui.style_overrides()) do
				mare.ui.theme_style(name, style)
			end
			mare.ui.syntax_theme(nil)
		end
	end, "theme styles")
	-- a mare.Service: `caller` in a method is the calling plugin's ctx
	require("mare_lib").service(ctx, "theme", service)
	-- Invalid startup config is reported, not silently represented as a successful choice.
	local ok = pcall(resolved, preference())
	if not ok then
		pending_preference = preference()
		mare.o.flavor = "auto"
	end
	for _, mode in ipairs({ "dark", "light" }) do
		local n = mare.o["flavor_" .. mode]
		if n and not raw_lookup(n) then
			pending_pairs[mode] = n
			mare.o["flavor_" .. mode] = mode
		end
	end
	apply()
	local function report_pending()
		for mode, name in pairs(pending_pairs) do
			ctx.logger:warn("unknown " .. mode .. " theme: " .. name)
		end
		pending_pairs = {}
		if pending_preference then
			ctx.logger:warn("unknown theme: " .. pending_preference .. "; using auto")
			pending_preference = nil
		end
	end
	ctx:on("app/ready", report_pending)
	ctx:on("reload/completed", report_pending)
	ctx:on("ui/background", function(ev)
		appearance = ev and ev.dark == false and "light" or "dark"
		if ev and ev.r and ev.g and ev.b then
			background = { ev.r, ev.g, ev.b }
		end
		apply()
	end)
	ctx:on("option/changed", function(ev)
		-- a runtime config write announces `option/changed` from its own task, after `select`
		-- already applied this value: applying it again would announce the same theme
		-- twice and make every renderer redo its work
		if valid_options[ev.name] ~= nil and valid_options[ev.name] == ev.value then
			return
		end
		if ev.name == "flavor" or ev.name == "flavor_dark" or ev.name == "flavor_light" then
			if not (ev.name == "flavor" and ev.value == "auto") and not raw_lookup(ev.value) then
				ctx.logger:warn("unknown theme: " .. tostring(ev.value))
				mare.o[ev.name] = valid_options[ev.name] or (ev.name == "flavor" and "auto" or ev.name:sub(8))
				return
			end
			apply()
		elseif ev.name == "theme" then
			apply()
		end
	end)
end

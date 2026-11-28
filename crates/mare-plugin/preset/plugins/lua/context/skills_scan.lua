-- context.skills_scan: the SKILL.md scan, its cache and problems, the skills section text and
-- the freshness stamp of the prompt's sources; part of the `context` plugin
-- (plugins/context.lua), which registers the section, listeners and cache teardown during apply
-- and loads this module the first time one of them runs.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local frontmatter, name_problems = D.frontmatter, D.name_problems
	local MAX_DESCRIPTION, MAX_COMPATIBILITY = D.MAX_DESCRIPTION, D.MAX_COMPATIBILITY
	local xml = D.xml
	local lib = require("mare_lib")

	-- installed packages' `skills/`, read by the plugin's apply
	local pkg_skill_dirs = D.pkg_skill_dirs
	local runtime_ready_dir = nil
	local function ensure_runtime_ready()
		local runtime_dir = mare.paths and mare.paths.runtime
		if type(runtime_dir) ~= "string" or runtime_dir == "" then
			return true
		end
		if runtime_ready_dir == runtime_dir then
			return true
		end
		if mare.plugins and mare.plugins.ensure_runtime then
			local ok, err = pcall(mare.plugins.ensure_runtime)
			if ok then
				runtime_ready_dir = runtime_dir
				return true
			end
			mare.log("warn", "bundled runtime assets unavailable: " .. tostring(err))
			return false
		end
		return true
	end
	--- every skill directory as {root, project}: the configured ones (project-level =
	--- relative, loaded only in trusted projects), then installed packages', then bundled runtime
	--- skills (first name wins, so user/project/package skills always shadow bundled ones)
	local function skill_roots()
		local out = {}
		for _, dir in ipairs(cfg.skill_dirs or {}) do
			local project = lib.is_project_relative(dir)
			local root = lib.resolve_project_dir(dir)
			if root then
				out[#out + 1] = { root = root, project = project }
			end
		end
		for _, dir in ipairs(pkg_skill_dirs) do
			out[#out + 1] = { root = dir, project = false }
		end
		local runtime_dir = mare.paths and mare.paths.runtime
		if type(runtime_dir) == "string" and runtime_dir ~= "" then
			out[#out + 1] = { root = runtime_dir .. "/skills", project = false }
		end
		return out
	end

	-- The scope names what this context is allowed to see, including root precedence.
	-- A source stamp names one observation within that scope. Keeping both in the
	-- filename means an older, cancelled write cannot replace a newer observation.
	local CACHE_VERSION = 1
	local MAX_CACHED_SNAPSHOTS = 4 -- like the bounded per-provider model catalog
	local MAX_CACHED_FILES = 64 -- soft bound across workspaces and cache versions
	local cache_dir = mare.paths.state .. "/skills-cache"
	local active, generation, wanted, snapshot = true, 0, nil, nil
	local function cache_scope()
		local roots = skill_roots()
		local ordered = mare.json.array({})
		for _, root in ipairs(roots) do
			ordered[#ordered + 1] = { path = mare.fs.realpath(root.root), project = root.project }
		end
		local trusted = mare.trusted == true
		local key = mare.crypto.sha256(mare.json.encode({
			version = CACHE_VERSION,
			cwd = mare.fs.realpath(mare.cwd),
			trusted = trusted,
			roots = ordered,
		}))
		return roots, key, trusted
	end

	local function observe_skills()
		local runtime_ready = ensure_runtime_ready()
		local roots, scope, trusted = cache_scope()
		local files, stamps, candidates = {}, mare.json.array({}), {}
		local cacheable = runtime_ready
		for _, root in ipairs(roots) do
			if not root.project or trusted then
				local entries, _, list_code = mare.fs.list(root.root, { strict = true })
				if not entries then
					-- A missing root is normal; an unreadable existing root must be scanned again.
					cacheable = cacheable and list_code == "not_found"
					entries = list_code == "not_found" and {} or (mare.fs.list(root.root) or {})
				end
				if entries.truncated then
					cacheable = false
				end
				for _, entry in ipairs(entries) do
					if entry.kind == "dir" then
						local path = root.root .. "/" .. entry.name .. "/SKILL.md"
						local st, _, stat_code = mare.fs.stat(path, { strict = true })
						if not st and stat_code ~= "not_found" then
							cacheable = false
						end
						files[#files + 1] = { path = path, name = entry.name }
						if st then
							candidates[path] = true
						end
						stamps[#stamps + 1] = mare.json.array({
							path,
							st and st.kind or "missing",
							st and tostring(st.mtime) or "",
							st and tostring(st.size) or "",
							st and tostring(st.ctime) or "",
							st and tostring(st.mode or "") or "",
							st and (mare.fs.identity(path) or "") or "",
							st and mare.fs.realpath(path) or "",
						})
					end
				end
			end
		end
		local _, current_scope = cache_scope()
		if current_scope ~= scope then
			return nil
		end
		local stamp = mare.crypto.sha256(mare.json.encode(stamps))
		return {
			scope = scope,
			stamp = stamp,
			key = scope .. "-" .. stamp,
			files = files,
			stamps = stamps,
			candidates = candidates,
			cacheable = cacheable,
		}
	end

	--- Freshness stamp of everything the system prompt is built from:
	--- instruction-file candidates and every SKILL.md, by mtime + size. Checked before the
	--- first request of each prompt — cheap stats, no reads when nothing changed.
	local unstable_observation = 0
	local function freshness_stamp()
		local parts = { tostring(mare.trusted == true), mare.fs.realpath(mare.cwd) }
		local function stat(p)
			local st = mare.fs.stat(p)
			parts[#parts + 1] = p
				.. "@"
				.. (
					st
						and (st.kind .. ":" .. st.mtime .. ":" .. st.size .. ":" .. st.ctime .. ":" .. tostring(
							st.mode or ""
						) .. ":" .. (mare.fs.identity(p) or ""))
					or "missing"
				)
		end
		local dirs = { mare.paths.config }
		for _, d in ipairs(D.ancestor_dirs()) do
			dirs[#dirs + 1] = d
		end
		for _, dir in ipairs(dirs) do
			for _, name in ipairs(cfg.files) do
				stat(dir .. "/" .. name)
			end
		end
		for _, name in ipairs({ "SYSTEM.md", "APPEND_SYSTEM.md" }) do
			stat(mare.paths.config .. "/" .. name)
			local proj_file = lib.project_file(".mare/" .. name)
			if proj_file then
				stat(proj_file)
			end
		end
		local observation = observe_skills()
		if observation then
			parts[#parts + 1] = observation.key
		else
			-- Scope changed during observation; force the readiness loop to retry.
			unstable_observation = unstable_observation + 1
			parts[#parts + 1] = "unstable:" .. unstable_observation
		end
		-- An incomplete scan must rebuild the prompt on the next request, but should not
		-- make the current request fail merely because the same source is still unreadable.
		local cacheable = observation and observation.cacheable and not (wanted == observation.key and snapshot == nil)
		return table.concat(parts, "|"), cacheable == true
	end

	local warned = {} -- path .. message -> true: skill problems already logged
	local function problem(list, path, message, skipped)
		list[#list + 1] = { path = path, message = message, skipped = skipped }
	end
	local function report_problems(problems)
		for _, p in ipairs(problems) do
			local key = p.path .. "\0" .. p.message
			if not warned[key] then
				warned[key] = true
				mare.log("warn", ("skill %s: %s"):format(p.path, p.message))
				if p.message:match("^unreadable:") and mare.ui.connected() then
					mare.ui.notify("Skipped skill " .. p.path .. ": " .. p.message, "warning")
				end
			end
		end
	end

	--- Agent Skills spec (agentskills.io),
	--- validated leniently: no `name` → the directory name; a name
	--- off the spec is a warning; the name isn't compared with the directory; only a missing or
	--- empty `description` skips the skill; the first of two same-named skills wins and the
	--- collision is reported. Problems go to the log, /skills and `mare doctor` — never a
	--- startup interruption. Every frontmatter key is kept (`frontmatter`), the spec's optional
	--- ones also by name; `allowed-tools` is read but not enforced (experimental).
	local function scan_skills(observation)
		-- scans can overlap (reads yield): each collects its own list, published at the end
		local skills, seen, real_seen, probs = mare.json.array({}), {}, {}, mare.json.array({})
		local cacheable = observation.cacheable
		for _, entry in ipairs(observation.files) do
			local p = entry.path
			local s, err = mare.fs.read(p)
			-- Do not cache an unreadable file: it might become readable without a new stamp.
			if not s and mare.fs.exists(p) then
				problem(probs, p, "unreadable: " .. tostring(err), true)
				cacheable = false
			end
			local real = s and mare.fs.realpath(p)
			if s and not real_seen[real] then
				real_seen[real] = true -- the same file through a symlink: once, silently
				local fm, _, warn = frontmatter(s)
				if warn then
					problem(probs, p, warn)
				end
				local name = (type(fm.name) == "string" and fm.name ~= "") and fm.name or entry.name
				local desc = type(fm.description) == "string" and fm.description:match("^%s*(.-)%s*$") or ""
				for _, msg in ipairs(name_problems(name)) do
					problem(probs, p, msg)
				end
				if #desc > MAX_DESCRIPTION then
					problem(probs, p, ("description exceeds %d characters (%d)"):format(MAX_DESCRIPTION, #desc))
				end
				if type(fm.compatibility) == "string" and #fm.compatibility > MAX_COMPATIBILITY then
					problem(probs, p, ("compatibility exceeds %d characters"):format(MAX_COMPATIBILITY))
				end
				if desc == "" then
					problem(probs, p, "description is required; skill skipped", true)
				elseif seen[name] then
					problem(probs, p, ('name "%s" collides with %s; that one is used'):format(name, seen[name]), true)
				else
					seen[name] = p
					local dmi = fm["disable-model-invocation"]
					skills[#skills + 1] = {
						name = name,
						description = desc,
						path = p,
						model_invocation = dmi ~= true and dmi ~= "true",
						license = fm.license,
						compatibility = fm.compatibility,
						metadata = fm.metadata,
						allowed_tools = fm["allowed-tools"],
						argument_hint = type(fm["argument-hint"]) == "string" and fm["argument-hint"] or nil,
						frontmatter = fm,
					}
				end
			end
		end
		return skills, probs, cacheable
	end

	-- the plugin's "skills cache" effect calls this on dispose: writes still in flight are dropped
	local function deactivate()
		active = false
		generation = generation + 1
	end

	local function cached_skills(observation)
		local ok, doc = pcall(function()
			return mare.json.decode(
				lib.strip_utf8_bom(mare.fs.read(cache_dir .. "/" .. observation.key .. ".json") or "")
			)
		end)
		if
			not ok
			or type(doc) ~= "table"
			or doc.version ~= CACHE_VERSION
			or doc.scope ~= observation.scope
			or doc.stamp ~= observation.stamp
			or type(doc.sources) ~= "table"
			or not mare.json.is_array(doc.sources)
			or mare.crypto.sha256(mare.json.encode(doc.sources)) ~= observation.stamp
			or type(doc.skills) ~= "table"
			or not mare.json.is_array(doc.skills)
			or type(doc.problems) ~= "table"
			or not mare.json.is_array(doc.problems)
			or type(doc.content_hash) ~= "string"
			or mare.crypto.sha256(mare.json.encode({ skills = doc.skills, problems = doc.problems }))
				~= doc.content_hash
		then
			return nil
		end
		local skills, problems = mare.json.array({}), mare.json.array({})
		for _, sk in ipairs(doc.skills) do
			if
				type(sk) ~= "table"
				or type(sk.name) ~= "string"
				or sk.name == ""
				or type(sk.description) ~= "string"
				or sk.description == ""
				or type(sk.path) ~= "string"
				or not observation.candidates[sk.path]
				or type(sk.model_invocation) ~= "boolean"
				or type(sk.frontmatter) ~= "table"
				or (sk.argument_hint ~= nil and type(sk.argument_hint) ~= "string")
			then
				return nil
			end
			-- Do not expose additional fields from a malformed cache entry.
			skills[#skills + 1] = {
				name = sk.name,
				description = sk.description,
				path = sk.path,
				model_invocation = sk.model_invocation,
				license = sk.license,
				compatibility = sk.compatibility,
				metadata = sk.metadata,
				allowed_tools = sk.allowed_tools,
				argument_hint = sk.argument_hint,
				frontmatter = sk.frontmatter,
			}
		end
		for _, p in ipairs(doc.problems) do
			if
				type(p) ~= "table"
				or type(p.path) ~= "string"
				or not observation.candidates[p.path]
				or type(p.message) ~= "string"
				or (p.skipped ~= nil and type(p.skipped) ~= "boolean")
			then
				return nil
			end
			problems[#problems + 1] = { path = p.path, message = p.message, skipped = p.skipped }
		end
		return skills, problems
	end

	local function prune_cache(observation, ticket)
		local prefix = observation.scope .. "-"
		local current_name = observation.key .. ".json"
		local older = {}
		for _, entry in ipairs(mare.fs.list(cache_dir) or {}) do
			if entry.kind == "file" and entry.name ~= current_name and entry.name:match("%.json$") then
				local path = cache_dir .. "/" .. entry.name
				local st = mare.fs.stat(path)
				local same_scope = #entry.name == #prefix + 64 + 5
					and entry.name:sub(1, #prefix) == prefix
					and entry.name:sub(#prefix + 1):match("^[0-9a-f]+%.json$")
				older[#older + 1] =
					{ path = path, name = entry.name, mtime = st and st.mtime or 0, same_scope = same_scope }
			end
		end
		table.sort(older, function(a, b)
			return a.mtime == b.mtime and a.name > b.name or a.mtime > b.mtime
		end)
		-- Always keep the just-written file, then retain recent files within both limits.
		local kept, scope_kept = 1, 1
		for _, entry in ipairs(older) do
			if kept < MAX_CACHED_FILES and (not entry.same_scope or scope_kept < MAX_CACHED_SNAPSHOTS) then
				kept = kept + 1
				if entry.same_scope then
					scope_kept = scope_kept + 1
				end
			else
				if not active or ticket ~= generation or wanted ~= observation.key then
					return
				end
				local current = observe_skills()
				if not active or not current or current.key ~= observation.key or ticket ~= generation then
					return
				end
				mare.fs.remove(entry.path)
			end
		end
	end

	---@return table skills, table problems
	local function load_skills(force_scan)
		-- A changed scope or source invalidates in-flight scans. Re-observe after every
		-- yielding read so a first request never publishes a partially stale scan.
		for _ = 1, 4 do
			local observation = observe_skills()
			if not active then
				error("context was disposed while loading skills", 0)
			end
			if observation then
				if wanted ~= observation.key then
					wanted = observation.key
					generation = generation + 1
				end
				local ticket = generation
				local skills, problems, cacheable, scanned
				if not force_scan and observation.cacheable and snapshot and snapshot.key == observation.key then
					skills, problems, cacheable = snapshot.skills, snapshot.problems, true
				elseif not force_scan and observation.cacheable then
					local ok, cached, cached_problems = pcall(cached_skills, observation)
					if ok then
						skills, problems = cached, cached_problems
					end
					cacheable = skills ~= nil
				end
				if not skills then
					skills, problems, cacheable = scan_skills(observation)
					scanned = true
				end
				local after = observe_skills()
				if active and ticket == generation and after and after.key == observation.key then
					if snapshot and snapshot.key ~= observation.key then
						D.invalidate()
					end
					snapshot = cacheable and { key = observation.key, skills = skills, problems = problems } or nil
					report_problems(problems)
					if scanned and cacheable then
						-- Cache failure is not a startup or prompt failure. The cache contains
						-- parsed frontmatter only, never the executable skill body.
						pcall(function()
							local content = { skills = skills, problems = problems }
							local encoded = mare.json.encode({
								version = CACHE_VERSION,
								scope = observation.scope,
								stamp = observation.stamp,
								sources = observation.stamps,
								skills = skills,
								problems = problems,
								content_hash = mare.crypto.sha256(mare.json.encode(content)),
							})
							local written = mare.fs.write(
								cache_dir .. "/" .. observation.key .. ".json",
								encoded,
								{ atomic = true, mode = 384 }
							)
							if written then
								prune_cache(observation, ticket)
							end
						end)
					end
					if active and ticket == generation then
						local final = observe_skills()
						if active and ticket == generation and final and final.key == observation.key then
							return lib.copy_deep(skills), lib.copy_deep(problems)
						end
					end
				end
			end
		end
		error("skills changed while they were being read; retry the request", 0)
	end

	--- what the last skill scan warned about or skipped, for /skills and `mare doctor`
	local function skill_problems()
		local _, problems = load_skills(true)
		return problems
	end

	-- `mare doctor`: skills loaded, and every warning; fails only when a skill was dropped
	local function doctor_check(checks)
		local skills, problems = load_skills(true)
		local skipped, lines = 0, {}
		for _, pr in ipairs(problems) do
			skipped = skipped + (pr.skipped and 1 or 0)
			lines[#lines + 1] = pr.path .. ": " .. pr.message
		end
		checks[#checks + 1] = {
			name = "skills",
			ok = skipped == 0,
			detail = ("%d loaded, %d skipped, %d warning(s)"):format(#skills, skipped, #problems - skipped)
				.. (#lines > 0 and ("; " .. table.concat(lines, "; ")) or ""),
		}
	end

	-- the skills section: name, description and location of each model-invocable skill
	local function section_text()
		local visible = {}
		for _, sk in ipairs(load_skills()) do
			if sk.model_invocation then
				visible[#visible + 1] = sk
			end
		end
		if #visible == 0 then
			return nil
		end
		local s = {
			"The following skills provide specialized instructions for specific tasks.",
			"Use the read tool to load a skill's file when the task matches its description.",
			"Resolve relative paths against the skill's directory (parent of SKILL.md).",
			"",
			"<available_skills>",
		}
		for _, sk in ipairs(visible) do
			s[#s + 1] = ("  <skill>\n    <name>%s</name>\n    <description>%s</description>\n    <location>%s</location>\n  </skill>"):format(
				xml(sk.name),
				xml(sk.description),
				xml(sk.path)
			)
		end
		s[#s + 1] = "</available_skills>"
		return table.concat(s, "\n")
	end

	return {
		freshness_stamp = freshness_stamp,
		load_skills = load_skills,
		deactivate = deactivate,
		skill_problems = skill_problems,
		doctor_check = doctor_check,
		section_text = section_text,
	}
end

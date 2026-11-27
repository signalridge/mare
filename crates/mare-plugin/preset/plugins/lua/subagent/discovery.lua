-- Load agent files into the same session-local registry as explicit registrations.
return function(ctx, cfg, types, lib)
	local collected
	local function warn(path, reason)
		mare.log("warn", "subagent type " .. tostring(path) .. ": " .. tostring(reason))
	end
	local function canonical(path)
		local ok, result = pcall(mare.fs.realpath, path)
		return ok and result or nil
	end
	local function inside(path, root)
		if not path or not root then
			return false
		end
		path, root = path:gsub("\\", "/"), root:gsub("\\", "/"):gsub("/+$", "")
		if mare.os == "windows" then
			path, root = path:lower(), root:lower()
		end
		return path == root or path:sub(1, #root + 1) == root .. "/"
	end
	local function scan(directory, project)
		if project and not mare.trusted then
			return
		end
		local resolved = project and lib.resolve_project_dir(directory) or mare.fs.resolve(directory)
		if not resolved or not mare.fs.exists(resolved) then
			return
		end
		local root = canonical(resolved)
		local project_root = project and mare.project and canonical(mare.project.root) or nil
		if not root then
			if project then
				warn(resolved, "directory escapes the trusted project root")
			end
			return
		end
		if project and not inside(root, project_root) then
			warn(resolved, "directory escapes the trusted project root")
			return
		end
		local entries = mare.fs.list(root, { no_follow = true }) or {}
		table.sort(entries, function(a, b)
			return a.name < b.name
		end)
		local seen = {}
		for _, entry in ipairs(entries) do
			local basename = entry.name:match("^(.+)%.md$")
			if basename then
				local path = root .. "/" .. entry.name
				if entry.kind ~= "file" then
					warn(path, "not a regular file")
				else
					local real = canonical(path)
					if not real or not inside(real, root) or (project and not inside(real, project_root)) then
						warn(path, "file escapes its trusted directory")
					else
						local text, err = mare.fs.read(real, { no_follow = true })
						if not text then
							warn(path, err or "cannot read file")
						else
							local normalized = text:gsub("^\239\187\191", ""):gsub("\r\n", "\n")
							local closed = normalized:find("\n---\n", 4, true) or normalized:match("\n%-%-%-$")
							if not normalized:match("^%-%-%-\n") or not closed then
								warn(path, "missing YAML frontmatter")
							else
								local ok, fm, body, warning = pcall(mare.text.frontmatter, text)
								if not ok or warning or type(fm) ~= "table" then
									warn(path, warning or fm or "invalid YAML frontmatter")
								else
									local valid, bad = pcall(types.validate_frontmatter, fm)
									fm.name, fm.system = fm.name or basename, body
									if not valid then
										warn(path, type(bad) == "table" and bad.message or bad)
									elseif seen[fm.name] then
										warn(path, "duplicate name already loaded from " .. seen[fm.name])
									else
										local added, why = pcall(types.validate, fm)
										if added then
											seen[fm.name] = path
											collected[fm.name] = why
										else
											warn(path, type(why) == "table" and why.message or why)
										end
									end
								end
							end
						end
					end
				end
			end
		end
	end

	local function reload(notify)
		collected = {}
		scan(mare.paths.config .. "/agents", false)
		for _, directory in ipairs(cfg.extra_agent_dirs or {}) do
			scan(directory, lib.is_project_relative(directory))
		end
		scan(".mare/agents", true)
		types.replace_discovered(collected, notify)
		collected = nil
	end
	reload(false)
	ctx:on("trust/changed", function()
		reload(true)
	end)
end

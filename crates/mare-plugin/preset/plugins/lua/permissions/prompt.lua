-- permissions.prompt: the `sandbox` system prompt section: the current level
-- and writable roots, what the preset already allows, plus the nudge so models try a needed
-- change instead of skipping it. Part of the `permissions` plugin (plugins/permissions.lua),
-- loaded at the end of its apply.
return function(D)
	local ctx, P = D.ctx, D.P
	-- permissions doesn't depend on context: registered through mare_lib.prompt_section (now if
	-- context is up, and again for each new provider of it); context rebuilds the prompt on
	-- permission/changed and sandbox/changed
	require("mare_lib").prompt_section(ctx, {
		name = "sandbox",
		order = 10010,
		interpolate = false, -- writable roots and hosts are paths and names: verbatim
		text = function()
			local p = mare.proc.sandbox.current()
			if not p then
				return nil
			end
			local base, extra = p, D.request.stores()
			if not D.escalation.inc_empty(extra) then
				local spec = D.escalation.apply_inc(base, extra)
				if base.restrict_read then
					spec.reads_pinned = true
					spec.readable = D.copy(base.readable or {})
					if not base.reads_pinned then
						for _, path in ipairs(base.roots or {}) do
							table.insert(spec.readable, path)
						end
					end
					for _, path in ipairs(extra.read or {}) do
						table.insert(spec.readable, path)
					end
				end
				p = mare.proc.sandbox.policy(spec)
			end
			-- what the current preset already allows, item by item:
			-- the model must not request what it already has (e.g. network under auto)
			local lines = {}
			lines[#lines + 1] = "Sandbox: " .. p.level .. " (permission preset: " .. tostring(P.preset) .. ")"
			lines[#lines + 1] = "Already allowed without asking:"
			local restricted_reads = p.restrict_read or #(p.deny_read or {}) > 0
			if p.restrict_read then
				local roots = D.copy(p.readable or {})
				if not p.reads_pinned then
					for _, path in ipairs(p.roots or {}) do
						table.insert(roots, path)
					end
				end
				lines[#lines + 1] = #roots > 0
						and ("- reading system paths and configured roots: " .. table.concat(roots, ", "))
					or "- reading system paths only"
			elseif #(p.deny_read or {}) > 0 then
				lines[#lines + 1] = "- reading files, except configured deny_read paths"
			else
				lines[#lines + 1] = "- reading files"
			end
			if p.level == "danger-full-access" then
				lines[#lines + 1] = "- writing anywhere"
			elseif p.level == "workspace-write" then
				lines[#lines + 1] = "- writing inside: " .. table.concat(p.roots or {}, ", ")
			end
			local net = p.network
			if net == true then
				lines[#lines + 1] = "- network access (open)"
			elseif type(net) == "table" and net.default == "allow" then
				lines[#lines + 1] = "- network access, except: " .. table.concat(net.deny or {}, ", ")
			elseif type(net) == "table" and #(net.allow or {}) > 0 then
				lines[#lines + 1] = "- network access to: " .. table.concat(net.allow, ", ")
			end
			lines[#lines + 1] = "Not allowed by this sandbox:"
			if p.level == "read-only" then
				lines[#lines + 1] = "- writing files"
			elseif p.level == "workspace-write" then
				lines[#lines + 1] = "- writing outside the directories above"
			end
			if p.restrict_read then
				lines[#lines + 1] = "- reading outside the allowed read roots"
			end
			if #(p.deny_read or {}) > 0 then
				lines[#lines + 1] = "- reading deny_read paths: " .. table.concat(p.deny_read, ", ")
			end
			if net == false or net == nil then
				lines[#lines + 1] = "- network access (blocked)"
			elseif type(net) == "table" and net.default ~= "allow" then
				lines[#lines + 1] = "- network access to other hosts"
			end
			if #lines > 0 and lines[#lines] == "Not allowed by this sandbox:" then
				lines[#lines] = nil
			end
			-- deny first, then request
			lines[#lines + 1] =
				"Approved additional rights are included above and expire at their returned turn/session scope."
			lines[#lines + 1] = "Commands run inside this sandbox. Never request what is already allowed above "
				.. "(for example network access when it is open). "
				.. (restricted_reads and "Read restrictions above remain in force. Writing inside the writable directories never needs extra permission. " or "Reading files, and writing inside the writable directories, never need extra permission: just run the command. ")
				.. "Don't request "
				.. "permissions in advance. Only after the sandbox denies a command the task needs, retry that "
				.. "exact command with sandbox_permissions set to the narrowest scope the denial names "
				.. '(network:<host> or write:<path>; "all" only if nothing narrower works) and a '
				.. "justification. After an access denial, request_permissions can request the required read/write/network rights "
				.. "for this turn or session. It may grant a subset: use only the returned additional rights. It cannot override deny_read "
				.. "or a denied tool call. Don't give up on a necessary change because of the sandbox."
			return table.concat(lines, "\n")
		end,
	})
end

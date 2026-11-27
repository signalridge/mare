-- One ordered control lane. Only classified-safe execution bodies overlap.
return function(classify, limit, interrupted)
	local phases, next_prepare, next_commit, submitted = {}, 1, 1, 0
	local lane, active, exclusive = false, 0, false
	local function sweep()
		while phases[next_prepare] and phases[next_prepare].aborted do
			next_prepare = next_prepare + 1
		end
		while phases[next_commit] and phases[next_commit].aborted do
			phases[next_commit] = nil
			next_commit = next_commit + 1
		end
	end
	local function reserve(call)
		submitted = submitted + 1
		local phase =
			{ index = submitted, call = call, released = false, settled = false, committing = false, finished = false }
		phases[phase.index] = phase
		function phase:prepare()
			while true do
				sweep()
				local safe = classify(self.call)
				local ready = not lane
					and self.index == next_prepare
					and not exclusive
					and active < limit
					and not (phases[next_commit] and phases[next_commit].settled)
				if ready and (safe or (active == 0 and next_commit == self.index)) then
					lane = true
					self.owns_lane = true
					return
				end
				mare.task.yield()
			end
		end
		function phase:body(frozen)
			if self.released then
				return
			end
			local safe = classify(frozen or self.call)
			-- Rewrites/definition replacement can change classification. Release only the
			-- control lane while earlier bodies commit; later prepares cannot pass this index.
			if not safe and (active > 0 or next_commit < self.index) then
				lane = false
				self.owns_lane = false
				while active > 0 or lane or next_commit < self.index do
					mare.task.yield()
				end
				lane = true
				self.owns_lane = true
			end
			active = active + 1
			self.running = true
			exclusive = not safe
			self.exclusive = not safe
			self.released = true
			next_prepare = next_prepare + 1
			lane = false
			self.owns_lane = false
		end
		function phase:commit()
			self.settled = true
			if self.running then
				active = active - 1
				self.running = false
			end
			while lane or self.index ~= next_commit do
				sweep()
				mare.task.yield()
			end
			lane = true
			self.owns_lane = true
			self.committing = true
		end
		function phase:finish()
			self.finished = true
			if self.running then
				active = active - 1
			end
			if self.exclusive then
				exclusive = false
			end
			if self.owns_lane then
				lane = false
				self.owns_lane = false
			end
			phases[self.index] = nil
			next_commit = next_commit + 1
			sweep()
		end
		function phase:abort()
			if self.finished then
				return
			end
			interrupted()
			if self.running then
				active = active - 1
			end
			if self.exclusive then
				exclusive = false
			end
			if self.owns_lane then
				lane = false
				self.owns_lane = false
			end
			self.aborted = true
			sweep()
		end
		return phase
	end
	return { reserve = reserve }
end

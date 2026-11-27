---@meta _
-- mare.log and mare.profile: logging
-- attributed to the plugin whose work is running, and the /profile data.

--- Write a log line attributed to the running plugin.
---@param level "error"|"warn"|"info"|"debug"
---@param msg string
function mare.log(level, msg) end

mare.profile = {}

--- Add a point to the /profile timeline (e.g. a cache invalidation).
---@param label string
function mare.profile.mark(label) end

--- The startup timeline and plugin load times.
---@return {at: number, took: number, label: string}[]
function mare.profile.report() end

--- Callbacks over `slow_callback_record_ms`, newest last (128 kept).
---@return {at_ms: number, ms: number, plugin: string, event: string}[]
function mare.profile.slow_callbacks() end

--- Callback time per plugin and event, largest first.
---@param limit? integer
---@return {plugin: string, event: string, count: integer, total_ms: number, max_ms: number}[]
function mare.profile.callback_timings(limit) end

---@meta _
--- Pure time arithmetic; no timers, task storage, agent execution or filesystem access.
--- The same implementation in the root runtime, child and headless hosts, exposed by an RPC contract.
mare.time = {}
---@alias mare.TimePattern {kind: "instant", at_ms: integer}|{kind: "interval", anchor_ms: integer, every_ms: integer}|{kind: "calendar", time: string, timezone: string, weekdays?: integer[]}|{kind: "cron", expression: string, timezone: string}|{kind: "rrule", rrule: string}
---@alias mare.TimeQuery {action: "sample", instant_ms?: integer}|{action: "instant", value?: string, date?: string, time?: string, timezone?: string}|{action: "format", instant_ms: integer, timezone?: string, format?: string}|{action: "normalize"|"next", pattern: mare.TimePattern, after_ms: integer}|{action: "resolve", pattern: mare.TimePattern, scheduled_ms: integer, accepted_ms: integer}
---@alias mare.TimeResult {action: "sample", instant_ms: integer, local_date: string, timestamp: string, timezone?: string, offset: string}|{action: "instant", instant_ms: integer, iso: string}|{action: "format", text: string, timezone: string}|{action: "normalize", pattern: mare.TimePattern, next_ms: integer|userdata}|{action: "next", next_ms: integer|userdata}|{action: "resolve", occurrence_ms: integer, next_ms: integer|userdata}
--- Synchronous, deterministic, bounded queries. Instants are integer epoch milliseconds in
--- years 0001..9999; explicit local instants reject gaps/folds. Calendar/Cron/RRULE recurrence
--- skips gaps and chooses earlier folds. Exhausted next_ms is mare.json.null (JSON null), never false or zero.
--- RRULE is the bounded two-line DTSTART/RRULE profile: six frequencies, COUNT <=1000,
--- one BYSECOND, <=10080 candidates per period, <=31 effective BYSETPOS selectors;
--- unsupported/duplicate fields, invalid combinations and unsafe integers reject.
--- normalize applies the stricter new-rule authoring profile; next/resolve retain supported
--- historical ineffective BYSETPOS rules. Cron normalization preserves the Vixie star flag.
--- Errors return nil, diagnostic; no work is scheduled. No resource handles to dispose.
--- Synchronous bounded calculation does not accept a cancellation token or yield mid-query.
---@param query mare.TimeQuery
---@return mare.TimeResult|nil result
---@return string|nil error
function mare.time.query(query) end

--- sample captures the host clock once (or formats explicit instant_ms) in the current host
--- local zone. timezone is nil when the OS supplies only a fixed offset; no IANA name is invented.
--- Host-zone discovery errors are explicit. sample is observational, unlike pure arithmetic actions.

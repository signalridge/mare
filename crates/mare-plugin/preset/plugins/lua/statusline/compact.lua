-- Optional compact status capability. Diagnose a malformed extension once per service.
local warned = setmetatable({}, { __mode = "k" })

return function(service, model)
	if not service or service.threshold == nil then
		return nil
	end
	if not require("mare_lib").callable(service.threshold) then
		if not warned[service] then
			warned[service] = true
			local message = "compact.threshold must be a function when provided"
			if mare.ui.connected() then
				mare.ui.notify(message, "error")
			else
				io.stderr:write("mare: " .. message .. "\n")
			end
		end
		return nil
	end
	warned[service] = nil
	return service:threshold(model)
end

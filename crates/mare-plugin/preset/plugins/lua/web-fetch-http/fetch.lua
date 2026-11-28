local function parse_charset(content_type)
	for param in content_type:gmatch(";%s*([^;]+)") do
		local key, val = param:match("^%s*([%w_-]+)%s*=%s*(.-)%s*$")
		if key and key:lower() == "charset" then
			if
				#val >= 2
				and ((val:sub(1, 1) == '"' and val:sub(-1) == '"') or (val:sub(1, 1) == "'" and val:sub(-1) == "'"))
			then
				val = val:sub(2, -2):match("^%s*(.-)%s*$")
			end
			if val ~= "" then
				return val
			end
		end
	end
	return nil
end

local function fail(cond, msg)
	if not cond then
		error(msg, 0)
	end
	return cond
end

local function normalize_url(url, max_url_length)
	local length = type(url) == "string" and utf8.len(url)
	fail(
		length and length > 0 and length <= max_url_length,
		("WEB_INVALID_URL: url must contain 1–%d UTF-8 characters"):format(max_url_length)
	)
	fail(not url:find("[%c%s]"), "WEB_INVALID_URL: url must not contain whitespace or control characters")
	local _, authority = url:match("^([Hh][Tt][Tt][Pp][Ss]?)://([^/?#]+)(.*)$")
	fail(
		authority and not authority:match("^:"),
		"WEB_INVALID_URL: url must be HTTP(S), with a host and no credentials"
	)
	fail(not authority:find("@", 1, true), "WEB_BLOCKED_URL: url must not contain embedded credentials")
	local ok, normalized = pcall(mare.http.build_url, url)
	if not ok or type(normalized) ~= "string" then
		error("WEB_INVALID_URL: " .. tostring(normalized), 0)
	end
	return normalized
end

local MAX_DETAIL_CHARS = 500

local FAILURE_CLASSES = {
	{
		code = "WEB_FETCH_CONNECTION_TIMEOUT",
		summary = "the connection timed out",
		patterns = { "etimedout", "und_err_connect_timeout", "connect timeout", "connection timed out" },
	},
	{
		code = "WEB_FETCH_DNS_FAILED",
		summary = "could not resolve the host name",
		patterns = {
			"enotfound",
			"eai_again",
			"eai_fail",
			"eai_noname",
			"eai_nodata",
			"dns error",
			"failed to lookup address information",
			"could not resolve",
			"name or service not known",
			"nodename nor servname provided",
			"no such host",
		},
	},
	{
		code = "WEB_FETCH_CONNECTION_REFUSED",
		summary = "the server refused the connection",
		patterns = { "econnrefused", "connection refused" },
	},
	{
		code = "WEB_FETCH_CERTIFICATE_INVALID",
		summary = "the server certificate could not be verified",
		patterns = {
			"cert_has_expired",
			"cert_not_yet_valid",
			"cert_revoked",
			"cert_untrusted",
			"cert_rejected",
			"cert_signature_failure",
			"depth_zero_self_signed_cert",
			"self_signed_cert_in_chain",
			"unable_to_get_issuer_cert",
			"unable_to_get_issuer_cert_locally",
			"unable_to_verify_leaf_signature",
			"unable_to_decrypt_cert_signature",
			"cert_chain_too_long",
			"invalid_ca",
			"hostname_mismatch",
			"err_tls_cert_altname_invalid",
			"certificate verify failed",
			"invalid certificate",
			"self-signed certificate",
			"unknown issuer",
			"certificate has expired",
			"tls handshake",
			"ssl handshake",
			"x509",
		},
	},
	{
		code = "WEB_FETCH_CONNECTION_INTERRUPTED",
		summary = "the connection was interrupted",
		patterns = {
			"econnreset",
			"econnaborted",
			"epipe",
			"und_err_socket",
			"connection reset",
			"connection aborted",
			"broken pipe",
			"unexpected eof",
			"early eof",
			"incomplete message",
			"other side closed",
			"socket hang up",
			"connection closed",
		},
	},
	{
		code = "WEB_FETCH_NETWORK_UNREACHABLE",
		summary = "the network or host is unreachable",
		patterns = {
			"enetunreach",
			"ehostunreach",
			"enetdown",
			"ehostdown",
			"network is unreachable",
			"host is unreachable",
			"no route to host",
			"network is down",
			"host is down",
		},
	},
}

local function bounded_detail(msg)
	local clean = mare.text.redact(msg):gsub("\r?\nstack traceback:.*$", ""):match("^%s*(.-)%s*$")
	local len = utf8.len(clean)
	if len then
		if len <= MAX_DETAIL_CHARS then
			return clean
		end
		local cutoff = utf8.offset(clean, MAX_DETAIL_CHARS + 1)
		return clean:sub(1, cutoff - 1) .. "…"
	end
	if #clean <= MAX_DETAIL_CHARS then
		return clean
	end
	return clean:sub(1, MAX_DETAIL_CHARS) .. "…"
end

local function matches_any(text, patterns)
	for _, pat in ipairs(patterns) do
		if text:find(pat, 1, true) then
			return true
		end
	end
	return false
end

local function classify_transport_error(err, cancel)
	local raw = tostring(err)
	if raw:match("^WEB_[%u_]+:") then
		return mare.text.redact(raw)
	end
	local detail = bounded_detail(raw)
	local lower = detail:lower()
	if
		(cancel and cancel.is_cancelled and cancel:is_cancelled())
		or lower:find("aborted", 1, true)
		or lower:find("cancel", 1, true)
	then
		return "WEB_ABORTED: " .. detail
	end
	if
		lower:find("sandbox:", 1, true)
		or lower:find("local/private", 1, true)
		or lower:find("offline test guard", 1, true)
		or lower:find("fake-ip address", 1, true)
		or lower:find("non-local proxy", 1, true)
	then
		return "WEB_BLOCKED_URL: " .. detail
	end
	if
		lower:find("too many redirects", 1, true)
		or lower:find("cross-origin redirect", 1, true)
		or lower:find("bad redirect location", 1, true)
	then
		return "WEB_REDIRECT_BLOCKED: " .. detail
	end
	for _, entry in ipairs(FAILURE_CLASSES) do
		if matches_any(lower, entry.patterns) then
			return entry.code .. ": " .. entry.summary .. ": " .. detail
		end
	end
	if lower:find("timed out", 1, true) or lower:find("timeout", 1, true) then
		return "WEB_FETCH_TIMEOUT: " .. detail
	end
	return "WEB_FETCH_NETWORK_ERROR: the network request failed: " .. detail
end

local SUPPORTED_APP_MIMES = {
	"application/json",
	"application/xml",
	"application/javascript",
	"application/x-javascript",
	"application/ecmascript",
}

local SUPPORTED_APP_MIME_SET = {}
for _, mime in ipairs(SUPPORTED_APP_MIMES) do
	SUPPORTED_APP_MIME_SET[mime] = true
end

local ACCEPT_MIMES = {
	"text/html",
	"application/xhtml+xml",
	"text/plain",
	"text/markdown",
	"text/javascript",
}
for _, mime in ipairs(SUPPORTED_APP_MIMES) do
	ACCEPT_MIMES[#ACCEPT_MIMES + 1] = mime
end
local ACCEPT_HEADER = table.concat(ACCEPT_MIMES, ", ")

local SUPPORTED_FORMAT_LABELS = { "text/*", "*+json", "*+xml" }
for _, mime in ipairs(SUPPORTED_APP_MIMES) do
	SUPPORTED_FORMAT_LABELS[#SUPPORTED_FORMAT_LABELS + 1] = mime
end
local SUPPORTED_FORMATS = table.concat(SUPPORTED_FORMAT_LABELS, ", ")

local function is_supported_mime(mime)
	return mime ~= nil
		and (
			mime:match("^text/") ~= nil
			or SUPPORTED_APP_MIME_SET[mime] == true
			or mime:match("%+json$") ~= nil
			or mime:match("%+xml$") ~= nil
		)
end

-- Bounded HTTP retrieval with text/plain and text/markdown passthrough.
return function(request, call, config)
	local cfg = config or {}
	local MAX_RESPONSE = cfg.max_response_bytes or 5000000
	local MAX_OUTPUT = cfg.max_output_bytes or 100000
	local MAX_URL_LENGTH = cfg.max_url_length or 8192
	local response
	local ok, result = pcall(function()
		local url = normalize_url(request.url, MAX_URL_LENGTH)
		local req_ok, req_res = pcall(mare.http.request, {
			url = url,
			method = "GET",
			headers = {
				accept = ACCEPT_HEADER,
				["user-agent"] = cfg.user_agent or "mare-web-fetch/1",
			},
			timeout_ms = cfg.timeout_ms or 30000,
			redirect = cfg.redirect or "same-origin",
			max_redirects = cfg.max_redirects or 5,
			block_private = true,
			cancel = call.cancel,
		})
		if not req_ok then
			error(classify_transport_error(req_res, call.cancel), 0)
		end
		response = req_res
		fail(response.ok, "WEB_HTTP_STATUS: HTTP " .. response.status)
		local content_type = response.headers["content-type"] or ""
		local mime = content_type:match("^%s*([^;]+)")
		mime = mime and mime:lower():match("^%s*(.-)%s*$")
		fail(
			is_supported_mime(mime),
			"WEB_UNSUPPORTED_CONTENT_TYPE: unsupported content type: "
				.. content_type
				.. " (supported: "
				.. SUPPORTED_FORMATS
				.. ")"
		)
		local charset = parse_charset(content_type)
		if charset then
			local ok_cs, cs_err = pcall(mare.text.decode, "", charset, { final = false })
			if not ok_cs then
				error("WEB_UNSUPPORTED_CONTENT_TYPE: " .. tostring(cs_err), 0)
			end
		end
		local declared = tonumber(response.headers["content-length"])
		fail(
			not declared or declared <= MAX_RESPONSE,
			"WEB_FETCH_TOO_LARGE: response exceeds " .. MAX_RESPONSE .. " bytes"
		)
		local chunks, total, stream_truncated = {}, 0, false
		while true do
			local read_ok, chunk = pcall(response.read, response)
			if not read_ok then
				error(classify_transport_error(chunk, call.cancel), 0)
			end
			if not chunk then
				break
			end
			if total + #chunk > MAX_RESPONSE then
				chunks[#chunks + 1] = chunk:sub(1, MAX_RESPONSE - total)
				stream_truncated = true
				break
			end
			total = total + #chunk
			chunks[#chunks + 1] = chunk
		end
		local bytes = table.concat(chunks)
		-- A bounded response prefix may end within any declared character encoding.
		local dec_ok, body = pcall(mare.text.decode, bytes, charset, { final = not stream_truncated })
		if not dec_ok then
			error("WEB_DECODE_ERROR: " .. tostring(body), 0)
		end
		local converted_truncated = false
		if mime == "text/html" or mime == "application/xhtml+xml" then
			local html_ok, md, html_trunc = pcall(mare.text.html_to_markdown, body, {
				max_output_bytes = math.min(MAX_OUTPUT, 1000000),
				max_tags = cfg.max_html_tags,
				max_nodes = cfg.max_html_nodes,
				max_depth = cfg.max_html_depth,
			})
			if not html_ok then
				error("WEB_HTML_ERROR: " .. tostring(md), 0)
			end
			body, converted_truncated = md, html_trunc
		end
		local truncated = stream_truncated or converted_truncated or #body > MAX_OUTPUT
		if truncated then
			body = mare.text.cut(body, MAX_OUTPUT)
		end
		return {
			requested_url = url,
			url = response.url,
			status = response.status,
			content_type = content_type,
			content = body,
			truncated = truncated,
		}
	end)
	if response then
		pcall(function()
			response:close()
		end)
	end
	if not ok then
		error(result, 0)
	end
	return result
end

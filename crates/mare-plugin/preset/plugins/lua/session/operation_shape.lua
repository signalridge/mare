-- Pure validation of the generic durable operation-result record, owned by session.
return function(receipt)
	assert(type(receipt) == "table" and receipt.version == 1, "unsupported session operation receipt")
	assert(type(receipt.session_id) == "string" and receipt.session_id ~= "", "invalid operation receipt owner")
	assert(
		type(receipt.request_id) == "string" and receipt.request_id ~= "" and #receipt.request_id <= 512,
		"invalid operation receipt request_id"
	)
	assert(
		type(receipt.kind) == "string" and receipt.kind ~= "" and #receipt.kind <= 128,
		"invalid operation receipt kind"
	)
	assert(
		type(receipt.fingerprint) == "string"
			and #receipt.fingerprint == 64
			and receipt.fingerprint:match("^[a-f0-9]+$"),
		"invalid operation receipt fingerprint"
	)
	assert(receipt.result ~= nil, "operation receipt has no complete result")
	return true
end

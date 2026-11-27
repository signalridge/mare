//! Policy configuration -> actual guarded execution, with real reviewer callbacks.
use super::*;

const SETUP: &str = r#"
    Q = C.cordis:get("approvals")
    ran, answered, reviewed = 0, 0, 0
    T:register("effect", {description = "test effect", parameters = {type="object"},
        side_effect = "external", execute = function() ran = ran + 1; return {content="ran"} end})
    C.cordis:get("policy"):voter("tool/before", function(call)
        if call.name == "effect" then return {decision="ask", reason="test approval"} end
    end)
    function EXECUTE(call, opts)
        opts = opts or {}
        opts.on_state = function(c, state, info)
            local ok, err = pcall(function()
                return C.cordis:get("session"):record_tool_state({
                    tool_call_id=c.id, name=c.name, state=state,
                    side_effect=info.side_effect, idempotent=info.idempotent,
                    durable=state == "started",
                })
            end)
            if not ok then return nil, type(err) == "table" and err.message or tostring(err) end
            return true
        end
        return T:execute(call,opts)
    end
    function RUN()
        return EXECUTE({id="policy-call", name="effect", arguments={}}, {
            approve=function() answered = answered + 1; return {decision="allow"} end})
    end
"#;

mod audit_and_queue_commit;
mod core;
mod inheritance_and_recheck;
mod projections_and_custom;
mod reviewer_guards;
mod shared_review;

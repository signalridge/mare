//! ACP outbound serialization: project once before sending a response or client request.

use agent_client_protocol::Client;
use agent_client_protocol::v1::schema::{RequestPermissionRequest, RequestPermissionResponse, SessionNotification};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{JsonRpcResponse, Result, UntypedMessage};
use mare_shared::wire_projection::WireProjector;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::AcpErr;

fn project<T: Serialize + DeserializeOwned>(value: T, projectors: &[WireProjector]) -> Result<T> {
    if !projectors.iter().any(WireProjector::is_active) {
        return Ok(value);
    }
    let json = serde_json::to_value(value).map_err(|_| agent_client_protocol::Error::internal_error())?;
    serde_json::from_value(project_value(json, projectors)).map_err(|_| agent_client_protocol::Error::internal_error())
}

fn project_value(mut json: Value, projectors: &[WireProjector]) -> Value {
    for projector in projectors {
        projector.project_json(&mut json);
    }
    json
}

pub(super) fn project_error(
    mut error: agent_client_protocol::Error,
    projectors: &[WireProjector],
) -> agent_client_protocol::Error {
    for projector in projectors {
        error.message = projector.project_str(&error.message);
        if let Some(data) = &mut error.data {
            projector.project_json(data);
        }
    }
    error
}

pub(super) fn send_reply<T>(
    responder: Responder<T>,
    value: std::result::Result<T, AcpErr>,
    projectors: &[WireProjector],
) -> Result<()>
where
    T: Serialize + DeserializeOwned + JsonRpcResponse,
{
    match value {
        Ok(value) => match project(value, projectors) {
            Ok(value) => responder.respond(value),
            Err(error) => responder.respond_with_error(error),
        },
        Err(error) => responder.respond_with_error(error.into_rpc()),
    }
}

pub(super) fn send_update(
    conn: &ConnectionTo<Client>,
    session: &str,
    update: Value,
    projectors: &[WireProjector],
) -> std::result::Result<(), String> {
    let notification = json!({ "sessionId": session, "update": update });
    let sent = serde_json::from_value::<SessionNotification>(notification)
        .map_err(|_| agent_client_protocol::Error::internal_error())
        .and_then(|notification| project(notification, projectors))
        .and_then(|notification| UntypedMessage::new("session/update", notification))
        .and_then(|notification| conn.send_notification(notification));
    if sent.is_err() {
        tracing::warn!("failed to send projected ACP session update");
        return Err("failed to send projected ACP session update".into());
    }
    Ok(())
}

pub(super) async fn request_permission(
    conn: &ConnectionTo<Client>,
    request: RequestPermissionRequest,
    projectors: &[WireProjector],
) -> Result<RequestPermissionResponse> {
    let request = serde_json::to_value(request).map_err(|_| agent_client_protocol::Error::internal_error())?;
    let request = UntypedMessage::new("session/request_permission", project_value(request, projectors))?;
    let response = conn.send_request(request).block_task().await?;
    serde_json::from_value(response).map_err(|_| agent_client_protocol::Error::internal_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse, StopReason, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use agent_client_protocol::{Agent, Channel, ErrorCode, RawJsonRpcMessage, TransportFrame};
    use futures_util::StreamExt;

    fn registered(secret: &str) -> (mlua::Lua, mlua::Value, WireProjector) {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        mare_binding::projection::install(&lua, &mare).unwrap();
        lua.globals().set("SECRET", secret).unwrap();
        let lease = lua.load("return mare.projection.register(SECRET)").eval().unwrap();
        let projector = mare_shared::wire_projection::hook(&lua).unwrap();
        (lua, lease, projector)
    }

    #[test]
    fn projecting_invalid_response_returns_internal_error() {
        let (_lua, _lease, projector) = registered("end_turn");
        let err = project(PromptResponse::new(StopReason::EndTurn), std::slice::from_ref(&projector)).unwrap_err();
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(!serde_json::to_string(&err).unwrap().contains("end_turn"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reply_with_projected_invalid_enum_returns_internal_error() {
        let (_lua, _lease, projector) = registered("end_turn");
        let agent = Agent.v1().on_receive_request(
            async move |_: PromptRequest, responder, _conn| {
                send_reply(responder, Ok(PromptResponse::new(StopReason::EndTurn)), std::slice::from_ref(&projector))
            },
            agent_client_protocol::on_receive_request!(),
        );
        Client
            .v1()
            .connect_with(agent, async |conn| {
                let err = conn.send_request(PromptRequest::new("s", vec![])).block_task().await.unwrap_err();
                assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
                assert!(!serde_json::to_string(&err).unwrap().contains("end_turn"));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn updates_validate_the_projected_notification_before_sending() {
        let (_lua, _lease, projector) = registered("notification-secret");
        let (_tag_lua, _tag_lease, tag_projector) = registered("agent_message_chunk");
        let (transport, mut peer) = Channel::duplex();
        let connection = Agent.v1().connect_with(transport, async move |conn| {
            let update =
                |text| json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } });
            assert!(send_update(&conn, "s", update("notification-secret"), &[projector]).is_ok());
            let err = send_update(&conn, "s", update("must-not-be-sent"), &[tag_projector])
                .expect_err("tag-corrupted update must report an error");
            assert!(!err.contains("must-not-be-sent"), "error must not leak unprojected payload: {err}");
            assert!(!err.contains("agent_message_chunk"), "error must not leak secret value: {err}");
            assert!(send_update(&conn, "s", update("barrier"), &[]).is_ok());
            Ok(())
        });
        let receive = async {
            let mut messages = Vec::new();
            for _ in 0..2 {
                let frame = peer.rx.next().await.unwrap();
                let TransportFrame::Single(RawJsonRpcMessage::Notification(message)) = frame else {
                    panic!("expected notification");
                };
                messages.push(serde_json::to_value(message).unwrap());
            }
            assert!(!messages[0].to_string().contains("notification-secret"));
            assert!(messages[0].to_string().contains("[[mare-value:"));
            assert!(messages[1].to_string().contains("barrier"), "unexpected projected notification: {}", messages[1]);
        };
        let (result, ()) = tokio::join!(connection, receive);
        result.unwrap();
    }

    #[test]
    fn scoped_error_projects_values_from_multiple_registries() {
        let (_lua1, _lease1, first) = registered("first-secret");
        let (_lua2, _lease2, second) = registered("second-secret");
        let mut error = agent_client_protocol::Error::internal_error().data("first-secret and second-secret");
        error.message = "first-secret and second-secret".into();
        let projected = project_error(error, &[first.clone(), second.clone()]);
        assert!(!projected.message.contains("first-secret"));
        assert!(!projected.message.contains("second-secret"));
        assert_eq!(projected.message.matches("[[mare-value:").count(), 2);
        let error =
            AcpErr::InvalidParams("first-secret and second-secret".into()).with_hooks(vec![first, second]).into_rpc();
        let output = serde_json::to_string(&error).unwrap();
        assert!(!output.contains("first-secret"));
        assert!(!output.contains("second-secret"));
        assert_eq!(output.matches("[[mare-value:").count(), 2);
    }

    #[test]
    fn outgoing_responses_errors_requests_and_notifications_hide_registered_values() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        mare_binding::projection::install(&lua, &mare).unwrap();
        let secret = "ACP_WIRE_SECRET_42\nquoted\"value";
        lua.globals().set("SECRET", secret).unwrap();
        let _lease: mlua::Value = lua.load("return mare.projection.register(SECRET)").eval().unwrap();
        let projector = mare_shared::wire_projection::hook(&lua).unwrap();

        let response = project(json!({ "result": { "nested": [secret] } }), std::slice::from_ref(&projector)).unwrap();
        assert!(!response.to_string().contains("ACP_WIRE_SECRET_42"), "{response}");
        assert!(response["result"]["nested"][0].as_str().unwrap().contains("[[mare-value:"));

        let mut error = agent_client_protocol::Error::invalid_params().data(json!({ "reason": secret }));
        error.message = format!("failure: {secret}");
        let error = project_error(error, std::slice::from_ref(&projector));
        assert!(!serde_json::to_string(&error).unwrap().contains("ACP_WIRE_SECRET_42"));
        assert!(error.message.contains("[[mare-value:"));
        assert!(error.data.unwrap()["reason"].as_str().unwrap().contains("[[mare-value:"));

        let notification: SessionNotification = serde_json::from_value(json!({
            "sessionId": "s", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": secret } }
        }))
        .unwrap();
        let notification = project(notification, std::slice::from_ref(&projector)).unwrap();
        assert!(!serde_json::to_string(&notification).unwrap().contains("ACP_WIRE_SECRET_42"));

        let call =
            ToolCallUpdate::new("call", ToolCallUpdateFields::new().title(secret).raw_input(json!({ "text": secret })));
        let request = RequestPermissionRequest::new(
            "s",
            call,
            vec![PermissionOption::new("allow-once", "Allow", PermissionOptionKind::AllowOnce)],
        );
        let request = project(request, std::slice::from_ref(&projector)).unwrap();
        assert!(!serde_json::to_string(&request).unwrap().contains("ACP_WIRE_SECRET_42"));
    }
}

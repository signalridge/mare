//! ACP helpers: workspace/MCP validation, prompt content extraction, event-to-update mapping,
//! model/reasoning config options, and session listing/resolution.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use agent_client_protocol::v1::schema::{
    ContentBlock, EmbeddedResourceResource, ListSessionsResponse, McpServer, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectGroup, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionInfo,
};
use serde_json::{Map, Value as J, json};

use mare_host::Host;

use super::AcpErr;
use crate::rpc::call_service;

const DEFAULT_PAGE_SIZE: usize = 100;

/// Resolve and validate a workspace `cwd` (falling back to `default_cwd` when empty).
pub(super) fn resolve_cwd(cwd: &Path, default_cwd: &Path) -> Result<PathBuf, String> {
    let target = if cwd.as_os_str().is_empty() { default_cwd } else { cwd };
    if !target.is_absolute() {
        return Err(format!("cwd must be an absolute path: {}", target.display()));
    }
    Ok(target.to_path_buf())
}

/// Compare two directory paths by physical identity when resolvable, else lexically.
pub(super) fn same_directory(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => left == right,
    }
}

fn normalize_server_name(name: &str) -> Result<String, String> {
    if name.trim().is_empty() || name.chars().any(|c| c.is_control()) {
        return Err("mcpServers contains an invalid server name".into());
    }
    let is_valid_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.len() <= 32 && name.chars().all(is_valid_char) {
        return Ok(name.to_string());
    }
    use sha2::Digest;
    let raw_slug: String = name.chars().map(|c| if is_valid_char(c) { c } else { '_' }).collect();
    let trimmed = raw_slug.trim_matches('_');
    let slug = if trimmed.is_empty() { "server" } else { &trimmed[..trimmed.len().min(20)] };
    let digest = sha2::Sha256::digest(name.as_bytes());
    let hex = format!("{:02x}{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2], digest[3]);
    let combined = format!("{slug}_{hex}");
    Ok(combined[..combined.len().min(32)].to_string())
}

/// Validate ACP `mcpServers` and convert them to `mare` `plugins.mcp.servers` config entries.
pub(super) fn translate_mcp_servers(servers: &[McpServer], cwd: &Path) -> Result<Map<String, J>, String> {
    let mut out = Map::new();
    let mut seen = HashSet::new();
    for (idx, srv) in servers.iter().enumerate() {
        match srv {
            McpServer::Stdio(s) => {
                let name = normalize_server_name(&s.name)?;
                if !seen.insert(name.replace('-', "_")) {
                    return Err(format!("mcpServers contains duplicate normalized name: {name}"));
                }
                if !s.command.is_absolute() {
                    return Err(format!("mcpServers[{idx}].command must be an absolute path"));
                }
                let mut env = Map::new();
                for e in &s.env {
                    if e.name.is_empty() || e.name.contains('=') || e.name.contains('\0') || e.value.contains('\0') {
                        return Err(format!("mcpServers[{idx}].env contains an invalid environment entry"));
                    }
                    if env.insert(e.name.clone(), J::String(e.value.clone())).is_some() {
                        return Err(format!("mcpServers[{idx}].env contains duplicate name: {}", e.name));
                    }
                }
                out.insert(
                    name,
                    json!({
                        "command": s.command.to_string_lossy(),
                        "args": s.args,
                        "env": env,
                        "cwd": cwd.to_string_lossy(),
                        "tier": "direct",
                    }),
                );
            }
            McpServer::Http(h) => {
                let name = normalize_server_name(&h.name)?;
                if !seen.insert(name.replace('-', "_")) {
                    return Err(format!("mcpServers contains duplicate normalized name: {name}"));
                }
                let parsed_url = url::Url::parse(&h.url)
                    .map_err(|_| format!("mcpServers[{idx}].url must be a valid HTTP(S) URL"))?;
                if !matches!(parsed_url.scheme(), "http" | "https") || !parsed_url.has_host() {
                    return Err(format!("mcpServers[{idx}].url must be a valid HTTP(S) URL"));
                }
                let mut headers = Map::new();
                let mut header_names = HashSet::new();
                for hdr in &h.headers {
                    let hdr_name = hdr
                        .name
                        .parse::<http::header::HeaderName>()
                        .map_err(|_| format!("mcpServers[{idx}].headers contains an invalid header entry"))?;
                    let _ = http::header::HeaderValue::from_str(&hdr.value)
                        .map_err(|_| format!("mcpServers[{idx}].headers contains an invalid header entry"))?;
                    if !header_names.insert(hdr_name.as_str().to_string()) {
                        return Err(format!("mcpServers[{idx}].headers contains duplicate name: {}", hdr.name));
                    }
                    headers.insert(hdr.name.clone(), J::String(hdr.value.clone()));
                }
                out.insert(name, json!({ "url": h.url, "headers": headers, "tier": "direct" }));
            }
            McpServer::Sse(_) => return Err(format!("mcpServers[{idx}] transport sse is not supported")),
            _ => return Err(format!("mcpServers[{idx}] transport is not supported")),
        }
    }
    Ok(out)
}

/// Extracted ACP prompt payload preserving ordered content blocks.
pub(super) struct ExtractedPrompt {
    pub text: String,
    pub images: Vec<J>,
    pub content: Vec<J>,
}

fn push_text_block(texts: &mut Vec<String>, content: &mut Vec<J>, text: String) {
    if let Some(last) = content.last_mut()
        && last.get("type").and_then(J::as_str) == Some("text")
        && let Some(prev) = last.get("text").and_then(J::as_str)
    {
        let merged = if prev.is_empty() {
            text.clone()
        } else if text.is_empty() {
            prev.to_string()
        } else {
            format!("{prev}\n\n{text}")
        };
        last["text"] = json!(merged);
    } else {
        content.push(json!({ "type": "text", "text": text }));
    }
    texts.push(text);
}

fn push_image_block(images: &mut Vec<J>, content: &mut Vec<J>, data: String, mime: String) {
    let image = json!({ "type": "image", "data": data, "mime": mime });
    images.push(image.clone());
    content.push(image);
}

fn supported_blob_image_mime(mime_type: Option<&str>) -> Option<String> {
    let raw = mime_type?.trim();
    let media_type = raw.split(';').next()?.trim().to_ascii_lowercase();
    matches!(media_type.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp").then_some(media_type)
}

/// Extract ordered text and image blocks from an ACP `session/prompt` request.
pub(super) fn extract_prompt(blocks: &[ContentBlock]) -> Result<ExtractedPrompt, String> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => push_text_block(&mut texts, &mut content, t.text.clone()),
            ContentBlock::Resource(r) => match &r.resource {
                EmbeddedResourceResource::TextResourceContents(tr) => {
                    let text =
                        if tr.uri.trim().is_empty() { tr.text.clone() } else { format!("[{}]\n{}", tr.uri, tr.text) };
                    push_text_block(&mut texts, &mut content, text);
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    match blob.mime_type.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(declared) => {
                            if let Some(mime) = supported_blob_image_mime(Some(declared)) {
                                push_image_block(&mut images, &mut content, blob.blob.clone(), mime);
                            } else {
                                return Err("binary embedded resource prompt content is not supported".into());
                            }
                        }
                        None => {
                            let validated = mare_binding::image::validate_base64(&blob.blob)
                                .map_err(|_| "binary embedded resource prompt content is not supported".to_string())?;
                            push_image_block(&mut images, &mut content, validated.data, validated.mime.to_string());
                        }
                    }
                }
                _ => return Err("unsupported embedded resource prompt content".into()),
            },
            ContentBlock::ResourceLink(rl) => {
                let name = serde_json::to_string(&rl.name).unwrap_or_default();
                let uri = serde_json::to_string(&rl.uri).unwrap_or_default();
                push_text_block(&mut texts, &mut content, format!("[resource_link name={name} uri={uri}]"));
            }
            ContentBlock::Image(img) => {
                push_image_block(&mut images, &mut content, img.data.clone(), img.mime_type.clone());
            }
            ContentBlock::Audio(_) => return Err("audio prompt content is not supported".into()),
            _ => return Err("unsupported ACP prompt content".into()),
        }
    }
    Ok(ExtractedPrompt { text: texts.join("\n\n"), images, content })
}

/// Derive ACP tool kind from kernel tool policy metadata (`command_tool`, `side_effect`).
pub(super) fn acp_tool_kind_str(info: &J) -> &'static str {
    if info.get("command_tool").and_then(|v| v.as_bool()) == Some(true) {
        return "execute";
    }
    match info.get("side_effect").and_then(|v| v.as_str()) {
        Some("read") => "read",
        Some("write") => "edit",
        _ => "other",
    }
}

fn tool_call_update_start(id: &J, name: &str, arguments: &J, info: Option<&J>) -> J {
    let arg = arguments
        .as_object()
        .and_then(|o| o.get("path").or_else(|| o.get("command")))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let kind = info.map(acp_tool_kind_str).unwrap_or("other");
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": if arg.is_empty() { name.to_string() } else { format!("{name} {arg}") },
        "kind": kind,
        "status": "in_progress",
        "rawInput": arguments,
    })
}

fn block_image_to_acp(block: &J) -> Option<J> {
    if block.get("type").and_then(J::as_str) != Some("image") {
        return None;
    }
    let data = block.get("data").and_then(J::as_str).unwrap_or("");
    let mime = block.get("mime").or_else(|| block.get("mimeType")).and_then(J::as_str).unwrap_or("");
    (!data.is_empty() && !mime.is_empty()).then(|| json!({ "type": "image", "data": data, "mimeType": mime }))
}

fn tool_result_content(content: &J) -> Vec<J> {
    match content {
        J::String(s) => vec![json!({ "type": "content", "content": { "type": "text", "text": s } })],
        J::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                if let Some(text) = block.get("text").and_then(J::as_str) {
                    Some(json!({ "type": "content", "content": { "type": "text", "text": text } }))
                } else {
                    block_image_to_acp(block).map(|img| json!({ "type": "content", "content": img }))
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_call_update_end(id: &J, failed: bool, content: &J) -> J {
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": if failed { "failed" } else { "completed" },
        "content": tool_result_content(content),
    })
}

/// Events translated into ACP `SessionUpdate` notifications by [`to_update`].
pub(super) fn handles_event(name: &str) -> bool {
    matches!(name, "item/delta" | "item/started" | "item/completed" | "usage/changed")
}

pub(super) fn has_truncated_image(content: &J) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block.get("type").and_then(J::as_str) == Some("image")
                && block.get("truncated").and_then(J::as_bool) == Some(true)
                && block.get("data").and_then(J::as_str).is_none_or(str::is_empty)
        })
    })
}

fn parse_completed_tool_call(p: &J) -> Option<(&J, bool, &J)> {
    let item = p.get("item")?;
    if item.get("type").and_then(J::as_str) != Some("tool_call") {
        return None;
    }
    let call_id = item.get("tool_call_id").or_else(|| p.get("item_id"))?;
    let is_error = item
        .pointer("/message/is_error")
        .and_then(J::as_bool)
        .or_else(|| item.pointer("/result/is_error").and_then(J::as_bool))
        .unwrap_or(false);
    let status_failed = item["status"].as_str().is_some_and(|s| s != "completed");
    let failed = is_error || status_failed;
    let content = item.pointer("/message/content").unwrap_or(&J::Null);
    Some((call_id, failed, content))
}

async fn fetch_full_tool_result_content(host: &Host, call_id: &str) -> Option<J> {
    let msgs = call_service(host, "agent", "messages", vec![]).await.ok()?;
    msgs.as_array()?.iter().rev().find_map(|msg| {
        (msg.get("role").and_then(J::as_str) == Some("tool_result")
            && msg.get("tool_call_id").and_then(J::as_str) == Some(call_id))
        .then(|| msg.get("content").cloned())
        .flatten()
    })
}

/// Resolve an `item/completed` event into an ACP `tool_call_update`, reading the full tool result
/// from `agent.messages` when inline image blocks were truncated by the kernel wire bound.
pub(super) async fn hydrate_tool_result_update(host: &Host, p: &J) -> Option<J> {
    let (call_id, failed, content) = parse_completed_tool_call(p)?;
    if has_truncated_image(content)
        && let Some(id_str) = call_id.as_str()
        && let Some(full) = fetch_full_tool_result_content(host, id_str).await
    {
        return Some(tool_call_update_end(call_id, failed, &full));
    }
    Some(tool_call_update_end(call_id, failed, content))
}

/// Map a mare kernel event to an ACP `SessionUpdate` (as JSON).
pub(super) fn to_update(name: &str, p: &J) -> Option<J> {
    match name {
        "item/delta" => match p.get("kind")?.as_str()? {
            "text" => Some(
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": p["delta"] } }),
            ),
            "reasoning" => Some(
                json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": p["delta"] } }),
            ),
            _ => None,
        },
        "item/started" => {
            let item = p.get("item")?;
            if item.get("type").and_then(J::as_str) != Some("tool_call") {
                return None;
            }
            let call_id = item.get("tool_call_id").or_else(|| p.get("item_id"))?;
            let tool_name = item["name"].as_str().unwrap_or("tool");
            let info = item.get("info").or_else(|| p.get("info"));
            Some(tool_call_update_start(call_id, tool_name, &item["arguments"], info))
        }
        "item/completed" => {
            let (call_id, failed, content) = parse_completed_tool_call(p)?;
            Some(tool_call_update_end(call_id, failed, content))
        }
        "usage/changed" => {
            let used = p.get("last_context").and_then(|v| v.as_u64())?;
            let size = p.get("context_window").and_then(|v| v.as_u64()).filter(|&s| s > 0)?;
            Some(json!({ "sessionUpdate": "usage_update", "used": used, "size": size }))
        }
        _ => None,
    }
}

fn push_replay_chunk(out: &mut Vec<J>, kind: &str, text: &str) {
    if !text.is_empty() {
        out.push(json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
    }
}

/// Collect registered tool policy metadata (`command_tool`, `side_effect`) keyed by tool name so
/// history replay derives ACP `kind` from declared metadata instead of hardcoded tool names.
pub(super) async fn tool_metadata_map(host: &Host) -> std::collections::HashMap<String, J> {
    let mut map = std::collections::HashMap::new();
    if let Ok(J::Array(defs)) = call_service(host, "tools", "list", vec![]).await {
        for def in defs {
            if let Some(name) = def.get("name").and_then(J::as_str) {
                map.insert(name.to_string(), def);
            }
        }
    }
    map
}

/// Project restored `agent:messages` entries into ordered ACP `SessionUpdate` replay items for `session/load`.
#[cfg(test)]
pub(super) fn messages_to_updates(messages: &[J]) -> Vec<J> {
    messages_to_updates_with_info(messages, &std::collections::HashMap::new())
}

/// Project restored `agent:messages` entries into ordered ACP `SessionUpdate` replay items for
/// `session/load`, resolving tool kinds from optional tool metadata keyed by tool name.
pub(super) fn messages_to_updates_with_info(
    messages: &[J],
    tool_info: &std::collections::HashMap<String, J>,
) -> Vec<J> {
    let mut out = Vec::new();
    let mut seen_tool_calls = HashSet::new();
    let mut tool_call_order = Vec::new();
    let mut closed_tool_calls = HashSet::new();
    for msg in messages {
        let Some(role) = msg.get("role").and_then(J::as_str) else { continue };
        match role {
            "user" => {
                if msg.get("notification").and_then(J::as_bool) == Some(true)
                    || msg.get("turn_aborted").and_then(J::as_bool) == Some(true)
                    || msg.get("compaction").and_then(J::as_bool) == Some(true)
                    || msg.get("branch_summary").and_then(J::as_bool) == Some(true)
                {
                    continue;
                }
                match msg.get("content") {
                    Some(J::String(text)) => push_replay_chunk(&mut out, "user_message_chunk", text),
                    Some(J::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(J::as_str) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(J::as_str) {
                                        push_replay_chunk(&mut out, "user_message_chunk", text);
                                    }
                                }
                                Some("image") => {
                                    if let Some(img) = block_image_to_acp(block) {
                                        out.push(json!({
                                            "sessionUpdate": "user_message_chunk",
                                            "content": img,
                                        }));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => match msg.get("content") {
                Some(J::String(text)) => push_replay_chunk(&mut out, "agent_message_chunk", text),
                Some(J::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(J::as_str) {
                            Some("reasoning") => {
                                if block.get("redacted").and_then(J::as_bool) != Some(true)
                                    && let Some(text) = block.get("text").and_then(J::as_str)
                                {
                                    push_replay_chunk(&mut out, "agent_thought_chunk", text);
                                }
                            }
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(J::as_str) {
                                    push_replay_chunk(&mut out, "agent_message_chunk", text);
                                }
                            }
                            Some("tool_call") => {
                                if let Some(id_val) = block.get("id")
                                    && let Some(id) = id_val.as_str().filter(|s| !s.is_empty())
                                {
                                    let name = block.get("name").and_then(J::as_str).unwrap_or("tool");
                                    let args = block.get("arguments").unwrap_or(&J::Null);
                                    let info = block.get("info").or_else(|| tool_info.get(name));
                                    if block.get("hosted").and_then(J::as_bool) == Some(true) {
                                        out.push(tool_call_update_start(id_val, name, args, info));
                                        let result = block.get("result").unwrap_or(&J::Null);
                                        let failed = result.get("status").and_then(J::as_str) == Some("failed");
                                        out.push(tool_call_update_end(id_val, failed, result));
                                    } else {
                                        if seen_tool_calls.insert(id.to_string()) {
                                            tool_call_order.push(id.to_string());
                                        }
                                        out.push(tool_call_update_start(id_val, name, args, info));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            "tool_result" => {
                if let Some(id_val) = msg.get("tool_call_id")
                    && let Some(id) = id_val.as_str().filter(|s| !s.is_empty())
                    && seen_tool_calls.contains(id)
                {
                    closed_tool_calls.insert(id.to_string());
                    let failed = msg.get("is_error").and_then(J::as_bool).unwrap_or(false);
                    out.push(tool_call_update_end(id_val, failed, &msg["content"]));
                }
            }
            _ => {}
        }
    }
    for id in tool_call_order {
        if !closed_tool_calls.contains(&id) {
            out.push(tool_call_update_end(&J::String(id), true, &J::Null));
        }
    }
    out
}

/// Build ACP `configOptions` (`model` + `reasoning_effort`) from the host's `llm` service.
pub(super) async fn config_options(host: &Host) -> Vec<SessionConfigOption> {
    let models = call_service(host, "llm", "models", vec![]).await.unwrap_or(J::Null);
    let current = call_service(host, "llm", "current", vec![]).await.unwrap_or(J::Null);
    let current_ref = current.get("ref").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let mut by_provider: BTreeMap<String, Vec<SessionConfigSelectOption>> = BTreeMap::new();
    for m in models.as_array().into_iter().flatten() {
        let Some(r) = m.get("ref").and_then(|v| v.as_str()) else { continue };
        let provider = m.get("provider").and_then(|v| v.as_str()).unwrap_or("default").to_string();
        let name = m.get("name").and_then(|v| v.as_str()).unwrap_or(r).to_string();
        let mut opt = SessionConfigSelectOption::new(r.to_string(), name);
        if let Some(desc) = m.get("description").and_then(|v| v.as_str()) {
            opt = opt.description(desc.to_string());
        }
        by_provider.entry(provider).or_default().push(opt);
    }
    if !current_ref.is_empty()
        && !by_provider.values().any(|opts| opts.iter().any(|o| o.value.0.as_ref() == current_ref))
    {
        let provider = current.get("provider").and_then(|v| v.as_str()).unwrap_or("default").to_string();
        let name = current.get("id").and_then(|v| v.as_str()).unwrap_or(&current_ref).to_string();
        by_provider.entry(provider).or_default().insert(0, SessionConfigSelectOption::new(current_ref.clone(), name));
    }

    let mut out = Vec::new();
    if !by_provider.is_empty() {
        let groups: Vec<SessionConfigSelectGroup> = by_provider
            .into_iter()
            .filter(|(_, opts)| !opts.is_empty())
            .map(|(provider, opts)| SessionConfigSelectGroup::new(provider.clone(), provider, opts))
            .collect();
        if !groups.is_empty() {
            out.push(
                SessionConfigOption::select("model", "Model", current_ref, SessionConfigSelectOptions::Grouped(groups))
                    .category(SessionConfigOptionCategory::Model),
            );
        }
    }

    if let Ok(J::Array(efforts)) = call_service(host, "llm", "effort_options", vec![]).await
        && !efforts.is_empty()
    {
        let current_effort = call_service(host, "llm", "effort", vec![])
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let opts: Vec<SessionConfigSelectOption> = efforts
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| SessionConfigSelectOption::new(s.to_string(), s.to_string())))
            .collect();
        if !opts.is_empty() {
            out.push(
                SessionConfigOption::select(
                    "reasoning_effort",
                    "Reasoning effort",
                    current_effort,
                    SessionConfigSelectOptions::Ungrouped(opts),
                )
                .category(SessionConfigOptionCategory::ThoughtLevel),
            );
        }
    }
    out
}

/// The refusal for a prompt carrying images when the session's current model takes none (the
/// `llm` descriptor's `images`; missing counts as no).
pub(super) async fn image_refusal(host: &Host) -> Result<(), AcpErr> {
    let current = call_service(host, "llm", "get_model", vec![])
        .await
        .map_err(|e| AcpErr::Internal(format!("llm.get_model failed: {e}")))?;
    if current.get("images").and_then(J::as_bool) == Some(true) {
        return Ok(());
    }
    let model = current.get("ref").and_then(J::as_str).unwrap_or("");
    Err(AcpErr::InvalidParams(format!(
        "the current model `{model}` does not accept images; switch the model (config option `model`) or send text only"
    )))
}

/// Apply a `session/set_config_option` mutation and return the updated `configOptions`.
pub(super) async fn set_config_option(
    host: &Host,
    config_id: &str,
    value: &SessionConfigOptionValue,
) -> Result<Vec<SessionConfigOption>, String> {
    let val = match value {
        SessionConfigOptionValue::ValueId { value } => value.0.as_ref().to_string(),
        SessionConfigOptionValue::Boolean { .. } => {
            return Err(format!("{config_id} requires a select value"));
        }
        _ => return Err(format!("{config_id} requires a select value")),
    };
    match config_id {
        "model" => {
            let models = call_service(host, "llm", "models", vec![]).await.map_err(|e| e.to_string())?;
            let known = models
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m.get("ref").and_then(|v| v.as_str()) == Some(val.as_str()));
            if !known {
                return Err(format!("unknown model option: {val}"));
            }
            call_service(host, "llm", "set_model", vec![J::String(val), json!({ "target": "session" })])
                .await
                .map_err(|e| e.to_string())?;
        }
        "reasoning_effort" => {
            let efforts = call_service(host, "llm", "effort_options", vec![]).await.map_err(|e| e.to_string())?;
            let known = efforts.as_array().into_iter().flatten().any(|e| e.as_str() == Some(val.as_str()));
            if !known {
                return Err(format!("unknown reasoning effort: {val}"));
            }
            call_service(host, "llm", "set_effort", vec![J::String(val), json!({ "target": "session" })])
                .await
                .map_err(|e| e.to_string())?;
        }
        _ => return Err(format!("unknown session config option: {config_id}")),
    }
    Ok(config_options(host).await)
}

#[derive(Clone)]
pub(super) struct PersistedSession {
    pub(super) cwd: PathBuf,
    pub(super) path: PathBuf,
}

#[cfg_attr(test, derive(Clone))]
struct SessionMeta {
    id: String,
    cwd: PathBuf,
    title: Option<String>,
    path: PathBuf,
    updated_at_ms: i64,
}

fn format_updated_at(ms: i64) -> Option<String> {
    (ms > 0).then(|| jiff::Timestamp::from_millisecond(ms).ok().map(|ts| ts.to_string())).flatten()
}

fn collect_session_metas() -> Vec<SessionMeta> {
    let root = mare_shared::paths::sessions_dir();
    if !root.is_dir() {
        return Vec::new();
    }
    // Reuse mare-store's shared SQLite index (falling back to scan_list if unavailable) so user
    // `fork` sessions (`kind = "fork"`) are included, sub-agent sessions (`kind = "agent"`) are
    // excluded, and `{type = "archive", archived = true}` records + legacy `archive/` dirs are
    // consistently respected without re-reading every file on disk.
    let query = mare_store::index::Query {
        kinds: mare_store::index::Kinds::Sessions,
        archived: Some(false),
        ..Default::default()
    };
    let rows = match mare_store::index::ready(&root) {
        Ok(shared) => {
            let _ = mare_store::index::reconcile(&shared);
            match shared.lock() {
                Ok(ix) => ix
                    .list(&query)
                    .map(|(rows, _)| rows)
                    .or_else(|_| mare_store::index::scan_list(&root, &query).map(|(rows, _)| rows)),
                Err(_) => mare_store::index::scan_list(&root, &query).map(|(rows, _)| rows),
            }
        }
        Err(_) => mare_store::index::scan_list(&root, &query).map(|(rows, _)| rows),
    };
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows {
        if row.session_id.is_empty() {
            continue;
        }
        let path = PathBuf::from(&row.path);
        // Guard against stray subagent files not stored under `agents/<run_id>/`.
        if let Some(header) = mare_store::read_header(&path)
            && header.get("origin").and_then(|v| v.as_str()) == Some("subagent")
        {
            continue;
        }
        let cwd = PathBuf::from(&row.cwd);
        if !cwd.is_absolute() {
            continue;
        }
        let updated_at_ms = if row.updated_at_ms > 0 {
            row.updated_at_ms
        } else if row.created_at_ms > 0 {
            row.created_at_ms
        } else if row.indexed_mtime > 0 {
            (row.indexed_mtime / 1_000_000).max(0)
        } else {
            0
        };
        let title = row.title.filter(|t| !t.is_empty());
        out.push(SessionMeta { id: row.session_id, cwd, title, path, updated_at_ms });
    }
    out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Find a persisted non-subagent session by id.
pub(super) fn find_session(session_id: &str) -> Option<PersistedSession> {
    let meta = collect_session_metas().into_iter().find(|s| s.id == session_id)?;
    Some(PersistedSession { cwd: meta.cwd, path: meta.path })
}

fn parse_list_cursor(cursor: Option<&str>) -> Result<Option<(i64, &str)>, String> {
    let Some(c) = cursor else {
        return Ok(None);
    };
    let (ms_str, id) = c.split_once(':').ok_or_else(|| "session/list cursor is invalid".to_string())?;
    if id.is_empty() {
        return Err("session/list cursor is invalid".to_string());
    }
    let ms = ms_str.parse::<i64>().map_err(|_| "session/list cursor is invalid".to_string())?;
    if ms < 0 {
        return Err("session/list cursor is invalid".to_string());
    }
    Ok(Some((ms, id)))
}

fn paginate_sessions(
    metas: Vec<SessionMeta>,
    cwd_filter: Option<&Path>,
    after: Option<(i64, &str)>,
    active_ids: &HashSet<String>,
    page_size: usize,
) -> ListSessionsResponse {
    let mut entries: Vec<SessionMeta> = metas
        .into_iter()
        .filter(|s| !active_ids.contains(&s.id))
        .filter(|s| cwd_filter.is_none_or(|c| same_directory(&s.cwd, c)))
        .filter(|s| match after {
            None => true,
            Some((ms, id)) => s.updated_at_ms < ms || (s.updated_at_ms == ms && s.id.as_str() > id),
        })
        .collect();
    let has_more = entries.len() > page_size;
    entries.truncate(page_size);
    let next_cursor = has_more.then(|| entries.last().map(|s| format!("{}:{}", s.updated_at_ms, s.id))).flatten();
    let infos = entries
        .into_iter()
        .map(|s| {
            let mut si = SessionInfo::new(s.id, s.cwd);
            if let Some(t) = s.title {
                si = si.title(t);
            }
            if let Some(updated_at) = format_updated_at(s.updated_at_ms) {
                si = si.updated_at(updated_at);
            }
            si
        })
        .collect();
    let mut resp = ListSessionsResponse::new(infos);
    if let Some(next) = next_cursor {
        resp = resp.next_cursor(next);
    }
    resp
}

/// List persisted non-subagent sessions (excluding `active_ids`), with optional `cwd` filter and cursor.
pub(super) fn list_sessions(
    cwd_filter: Option<&Path>,
    cursor: Option<&str>,
    active_ids: &HashSet<String>,
) -> Result<ListSessionsResponse, String> {
    if let Some(c) = cwd_filter
        && !c.is_absolute()
    {
        return Err(format!("cwd must be an absolute path: {}", c.display()));
    }
    let after = parse_list_cursor(cursor)?;
    Ok(paginate_sessions(collect_session_metas(), cwd_filter, after, active_ids, DEFAULT_PAGE_SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::SessionNotification;

    #[test]
    fn messages_to_updates_maps_all_valid_items_and_skips_synthetic_or_orphaned_records() {
        let history = vec![
            json!({ "role": "user", "notification": true, "content": [{ "type": "text", "text": "bg note" }] }),
            json!({ "role": "user", "turn_aborted": true, "content": [{ "type": "text", "text": "<turn_aborted/>" }] }),
            json!({ "role": "user", "compaction": true, "content": [{ "type": "text", "text": "<summary/>" }] }),
            json!({ "role": "user", "branch_summary": true, "content": [{ "type": "text", "text": "<branch/>" }] }),
            json!({ "role": "user", "content": [
                { "type": "text", "text": "hi" },
                { "type": "image", "data": "AQID", "mime": "image/png" }
            ] }),
            json!({ "role": "assistant", "content": [
                { "type": "reasoning", "text": "secret", "redacted": true },
                { "type": "reasoning", "text": "plan" },
                { "type": "tool_call", "id": "tc1", "name": "bash", "arguments": { "command": "false" } },
                { "type": "text", "text": "done" }
            ] }),
            json!({ "role": "tool_result", "tool_call_id": "orphan", "content": "ignored", "is_error": false }),
            json!({ "role": "tool_result", "tool_call_id": "tc1", "content": [{ "type": "text", "text": "boom" }], "is_error": true }),
        ];

        let updates = messages_to_updates(&history);
        assert_eq!(updates.len(), 6, "{updates:?}");
        assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["content"]["mimeType"], "image/png");
        assert_eq!(updates[2]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(updates[2]["content"]["text"], "plan");
        assert_eq!(updates[3]["sessionUpdate"], "tool_call");
        assert_eq!(updates[3]["kind"], "other");
        assert_eq!(updates[4]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(updates[5]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[5]["status"], "failed");
        assert_eq!(updates[5]["content"][0]["content"]["text"], "boom");

        for update in &updates {
            let parsed: Result<SessionNotification, _> =
                serde_json::from_value(json!({ "sessionId": "s1", "update": update }));
            assert!(parsed.is_ok(), "failed to parse {update}: {parsed:?}");
        }
    }

    #[test]
    fn messages_to_updates_marks_unpaired_tool_call_failed() {
        let history = vec![
            json!({ "role": "assistant", "content": [
                { "type": "tool_call", "id": "tc_ok", "name": "read", "arguments": { "path": "a" } },
                { "type": "tool_call", "id": "tc_unpaired", "name": "bash", "arguments": { "command": "sleep 9" } }
            ] }),
            json!({ "role": "tool_result", "tool_call_id": "tc_ok", "content": "ok", "is_error": false }),
        ];

        let updates = messages_to_updates(&history);
        assert_eq!(updates.len(), 4, "{updates:?}");
        assert_eq!(updates[0]["sessionUpdate"], "tool_call");
        assert_eq!(updates[1]["sessionUpdate"], "tool_call");
        assert_eq!(updates[2]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[2]["toolCallId"], "tc_ok");
        assert_eq!(updates[2]["status"], "completed");
        assert_eq!(updates[3]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[3]["toolCallId"], "tc_unpaired");
        assert_eq!(updates[3]["status"], "failed");

        for update in &updates {
            let parsed: Result<SessionNotification, _> =
                serde_json::from_value(json!({ "sessionId": "s1", "update": update }));
            assert!(parsed.is_ok(), "failed to parse {update}: {parsed:?}");
        }
    }

    #[test]
    fn translate_mcp_servers_validates_http_urls_and_headers() {
        use agent_client_protocol::v1::schema::{HttpHeader, McpServerHttp};
        let cwd = std::path::Path::new("/tmp");

        let bad_url = vec![McpServer::Http(McpServerHttp::new("srv", "http://"))];
        assert!(translate_mcp_servers(&bad_url, cwd).is_err());

        let non_http = vec![McpServer::Http(McpServerHttp::new("srv", "ftp://example.com/mcp"))];
        assert!(translate_mcp_servers(&non_http, cwd).is_err());

        let bad_hdr_name = vec![McpServer::Http(
            McpServerHttp::new("srv", "https://example.com/mcp").headers(vec![HttpHeader::new("Bad Name", "v")]),
        )];
        assert!(translate_mcp_servers(&bad_hdr_name, cwd).is_err());

        let bad_hdr_val = vec![McpServer::Http(
            McpServerHttp::new("srv", "https://example.com/mcp")
                .headers(vec![HttpHeader::new("Authorization", "a\nb")]),
        )];
        assert!(translate_mcp_servers(&bad_hdr_val, cwd).is_err());

        let valid = vec![McpServer::Http(
            McpServerHttp::new("srv", "https://example.com/mcp")
                .headers(vec![HttpHeader::new("Authorization", "Bearer tok")]),
        )];
        assert!(translate_mcp_servers(&valid, cwd).is_ok());
    }

    #[test]
    fn to_update_reports_persist_failed_as_failed_even_when_message_is_error_false() {
        let payload = json!({
            "turn_id": "t-1",
            "item_id": "c-1",
            "status": "persist_failed",
            "item": {
                "type": "tool_call",
                "tool_call_id": "c-1",
                "name": "write",
                "status": "persist_failed",
                "message": {
                    "role": "tool_result",
                    "tool_call_id": "c-1",
                    "content": [{ "type": "text", "text": "wrote file" }],
                    "is_error": false
                },
                "result_status": "persist_failed",
                "execution_status": "unknown"
            }
        });
        let update = to_update("item/completed", &payload).expect("tool_call_update");
        assert_eq!(update["sessionUpdate"], "tool_call_update");
        assert_eq!(update["toolCallId"], "c-1");
        assert_eq!(
            update["status"], "failed",
            "persist_failed must be reported as failed even when message.is_error is false: {update}"
        );
    }

    #[test]
    fn tool_call_kind_uses_metadata_and_defaults_to_other_without_name_hardcoding() {
        let replay = messages_to_updates(&[json!({
            "role": "assistant",
            "content": [
                { "type": "tool_call", "id": "t1", "name": "read", "arguments": { "path": "a" } },
                { "type": "tool_call", "id": "t2", "name": "write", "arguments": { "path": "b" } },
                { "type": "tool_call", "id": "t3", "name": "bash", "arguments": { "command": "ls" } },
                { "type": "tool_call", "id": "t4", "name": "custom_cmd", "arguments": {}, "info": { "command_tool": true } },
                { "type": "tool_call", "id": "t5", "name": "custom_read", "arguments": {}, "info": { "side_effect": "read" } },
                { "type": "tool_call", "id": "t6", "name": "custom_edit", "arguments": {}, "info": { "side_effect": "write" } }
            ]
        })]);
        assert_eq!(replay[0]["kind"], "other");
        assert_eq!(replay[1]["kind"], "other");
        assert_eq!(replay[2]["kind"], "other");
        assert_eq!(replay[3]["kind"], "execute");
        assert_eq!(replay[4]["kind"], "read");
        assert_eq!(replay[5]["kind"], "edit");
    }

    #[test]
    fn list_sessions_keyset_cursor_survives_removals_and_equal_mtime_ties() {
        let cwd = PathBuf::from("/tmp/ws");
        let make = |id: &str, updated_at_ms: i64| SessionMeta {
            id: id.to_string(),
            cwd: cwd.clone(),
            title: None,
            path: PathBuf::from(format!("/tmp/ws/{id}.jsonl")),
            updated_at_ms,
        };
        let mut metas = vec![make("s-a", 200), make("s-b", 100), make("s-c", 100), make("s-d", 100), make("s-e", 50)];
        metas.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms).then_with(|| a.id.cmp(&b.id)));

        let mut active = HashSet::new();
        let page1 = paginate_sessions(metas.clone(), Some(&cwd), None, &active, 2);
        let ids1: Vec<_> = page1.sessions.iter().map(|s| s.session_id.0.as_ref()).collect();
        assert_eq!(ids1, vec!["s-a", "s-b"]);
        let cursor = page1.next_cursor.as_deref().expect("page 1 cursor");
        assert_eq!(cursor, "100:s-b");

        // Opening an item from page 1 must not skip `s-c` on page 2.
        active.insert("s-a".to_string());
        let after1 = parse_list_cursor(Some(cursor)).unwrap();
        let page2 = paginate_sessions(metas.clone(), Some(&cwd), after1, &active, 2);
        let ids2: Vec<_> = page2.sessions.iter().map(|s| s.session_id.0.as_ref()).collect();
        assert_eq!(ids2, vec!["s-c", "s-d"]);
        let cursor2 = page2.next_cursor.as_deref().expect("page 2 cursor");

        let after2 = parse_list_cursor(Some(cursor2)).unwrap();
        let page3 = paginate_sessions(metas, Some(&cwd), after2, &active, 2);
        let ids3: Vec<_> = page3.sessions.iter().map(|s| s.session_id.0.as_ref()).collect();
        assert_eq!(ids3, vec!["s-e"]);
        assert!(page3.next_cursor.is_none());

        assert!(parse_list_cursor(Some("100")).is_err());
        assert!(parse_list_cursor(Some("100:")).is_err());
        assert!(parse_list_cursor(Some("abc:id")).is_err());
    }

    #[test]
    fn extract_prompt_preserves_text_resource_uri_and_accepts_image_blobs() {
        use agent_client_protocol::v1::schema::{
            BlobResourceContents, EmbeddedResource, EmbeddedResourceResource, ResourceLink, TextResourceContents,
        };
        let blocks = vec![
            ContentBlock::Resource(EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
                TextResourceContents::new("body", "file:///notes/a.md"),
            ))),
            ContentBlock::Resource(EmbeddedResource::new(EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new("AQID", "file:///pic.png").mime_type("image/png; charset=binary"),
            ))),
            ContentBlock::ResourceLink(ResourceLink::new("ref", "file:///b.md")),
        ];
        let parsed = extract_prompt(&blocks).expect("prompt");
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0], json!({ "type": "image", "data": "AQID", "mime": "image/png" }));
        assert!(parsed.text.contains("[file:///notes/a.md]\nbody"), "{}", parsed.text);
        assert_eq!(parsed.content.len(), 3);

        let bad_blob =
            vec![ContentBlock::Resource(EmbeddedResource::new(EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new("AQID", "file:///doc.pdf").mime_type("application/pdf"),
            )))];
        assert!(extract_prompt(&bad_blob).is_err());
    }

    #[test]
    fn tool_result_images_forward_live_and_on_replay_without_empty_text() {
        let img_only = vec![json!({ "type": "image", "data": "AQID", "mime": "image/png" })];
        let live = to_update(
            "item/completed",
            &json!({
                "turn_id": "t1",
                "item_id": "c1",
                "status": "completed",
                "item": {
                    "type": "tool_call",
                    "tool_call_id": "c1",
                    "name": "img_only",
                    "status": "completed",
                    "message": { "role": "tool_result", "tool_call_id": "c1", "content": img_only, "is_error": false }
                }
            }),
        )
        .expect("live tool_call_update");
        assert_eq!(
            live["content"],
            json!([{ "type": "content", "content": { "type": "image", "data": "AQID", "mimeType": "image/png" } }])
        );
        assert!(
            serde_json::from_value::<SessionNotification>(json!({ "sessionId": "s1", "update": live })).is_ok(),
            "live image tool result must parse as SessionNotification"
        );

        let replay = messages_to_updates(&[
            json!({ "role": "assistant", "content": [{ "type": "tool_call", "id": "c1", "name": "img_only", "arguments": {} }] }),
            json!({ "role": "tool_result", "tool_call_id": "c1", "content": [
                { "type": "image", "data": "AQID", "mime": "image/png" }
            ], "is_error": false }),
        ]);
        assert_eq!(replay.len(), 2);
        assert_eq!(
            replay[1]["content"],
            json!([{ "type": "content", "content": { "type": "image", "data": "AQID", "mimeType": "image/png" } }])
        );
        assert!(
            serde_json::from_value::<SessionNotification>(json!({ "sessionId": "s1", "update": replay[1] })).is_ok(),
            "replayed image tool result must parse as SessionNotification"
        );

        assert!(has_truncated_image(&json!([{ "type": "image", "mime": "image/png", "truncated": true }])));
        assert!(!has_truncated_image(&json!([{ "type": "image", "data": "AQID", "mime": "image/png" }])));
    }

    #[test]
    fn paginate_sessions_populates_rfc3339_updated_at_and_paginates_by_activity_time() {
        let cwd = PathBuf::from("/tmp/ws");
        let metas = vec![
            SessionMeta {
                id: "s-new".into(),
                cwd: cwd.clone(),
                title: Some("Newer".into()),
                path: PathBuf::from("/tmp/new.jsonl"),
                updated_at_ms: 1_700_000_002_500,
            },
            SessionMeta {
                id: "s-old".into(),
                cwd: cwd.clone(),
                title: Some("Older".into()),
                path: PathBuf::from("/tmp/old.jsonl"),
                updated_at_ms: 1_700_000_001_000,
            },
            SessionMeta {
                id: "s-zero".into(),
                cwd: cwd.clone(),
                title: None,
                path: PathBuf::from("/tmp/zero.jsonl"),
                updated_at_ms: 0,
            },
        ];
        let active = HashSet::new();
        let page1 = paginate_sessions(metas.clone(), Some(&cwd), None, &active, 1);
        assert_eq!(page1.sessions.len(), 1);
        assert_eq!(page1.sessions[0].session_id.0.as_ref(), "s-new");
        assert_eq!(page1.sessions[0].updated_at.as_deref(), Some("2023-11-14T22:13:22.5Z"));
        let cursor = page1.next_cursor.as_deref().expect("next_cursor");
        let after = parse_list_cursor(Some(cursor)).expect("valid cursor");

        let page2 = paginate_sessions(metas, Some(&cwd), after, &active, 10);
        assert_eq!(page2.sessions.len(), 2);
        assert_eq!(page2.sessions[0].session_id.0.as_ref(), "s-old");
        assert_eq!(page2.sessions[0].updated_at.as_deref(), Some("2023-11-14T22:13:21Z"));
        assert_eq!(page2.sessions[1].session_id.0.as_ref(), "s-zero");
        assert_eq!(page2.sessions[1].updated_at, None);
        assert!(parse_list_cursor(Some("-1:abc")).is_err());
    }

    #[test]
    fn repro_317_extract_prompt_accepts_embedded_png_without_mime_type() {
        use agent_client_protocol::v1::schema::{BlobResourceContents, EmbeddedResource, EmbeddedResourceResource};
        const PNG_1X1_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(PNG_1X1_B64, "file:///pic.png")),
        ))];
        let parsed = extract_prompt(&blocks)
            .expect("embedded PNG blob without explicit mimeType should be accepted via byte sniffing");
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0]["mime"], "image/png");
    }

    #[test]
    fn extract_prompt_rejects_non_image_or_corrupt_blob_without_mime_type() {
        use agent_client_protocol::v1::schema::{BlobResourceContents, EmbeddedResource, EmbeddedResourceResource};
        use base64::Engine as _;
        const PNG_1X1_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let mut corrupt_pixels = base64::engine::general_purpose::STANDARD.decode(PNG_1X1_B64).unwrap();
        // Preserve PNG signature and valid 1x1 IHDR (first 33 bytes), corrupt the IDAT pixel payload.
        assert!(corrupt_pixels.len() > 45);
        for b in &mut corrupt_pixels[37..45] {
            *b ^= 0xff;
        }
        let corrupt_pixels_b64 = base64::engine::general_purpose::STANDARD.encode(&corrupt_pixels);

        for payload in ["SGVsbG8gd29ybGQ=", "iVBORw0KGgo=", "not-valid-base64!!!", corrupt_pixels_b64.as_str()] {
            let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(payload, "file:///blob.bin")),
            ))];
            assert!(extract_prompt(&blocks).is_err(), "payload {payload:?} without mimeType must be rejected");
        }

        // Explicit non-image MIME must still be rejected even when bytes are a valid PNG.
        let mut non_image_blob = BlobResourceContents::new(PNG_1X1_B64, "file:///blob.bin");
        non_image_blob.mime_type = Some("application/octet-stream".into());
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(non_image_blob),
        ))];
        assert!(extract_prompt(&blocks).is_err(), "explicit non-image mimeType must be rejected");
    }
}

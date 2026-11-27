//! The trust prompt overlay.

use crate::app::*;

const MAX_GROUP_ITEMS: usize = 6;
const MAX_MCP_JSON_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResourceGroup {
    label: &'static str,
    items: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct TrustResources {
    groups: Vec<ResourceGroup>,
    has_init_lua: bool,
    has_mcp: bool,
}

fn clean_text(s: &str) -> String {
    mare_shared::style::sanitize(s).replace('\n', " ")
}

fn sanitize_executable_basename(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) || trimmed.contains("://") {
        return None;
    }
    const FORBIDDEN_CHARS: &[char] =
        &['?', '#', '=', '&', ';', '|', '$', '`', '(', ')', '{', '}', '<', '>', '*', '\'', '"', '@'];
    if trimmed.chars().any(|c| FORBIDDEN_CHARS.contains(&c)) {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    let after_drive = normalized
        .as_bytes()
        .get(0..3)
        .filter(|b| b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/')
        .map(|_| &normalized[3..])
        .unwrap_or(&normalized);
    if after_drive.contains(':') {
        return None;
    }
    let leaf = normalized.rsplit('/').next().filter(|part| !part.is_empty())?;
    if leaf.starts_with('-') || !leaf.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return None;
    }
    let cleaned = clean_text(leaf);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn sanitize_url_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    let host = parsed.host_str()?;
    let cleaned = clean_text(host);
    (!cleaned.is_empty() && !cleaned.chars().any(char::is_whitespace)).then_some(cleaned)
}

fn read_bounded_mcp_file(rel: &std::path::Path, project_root: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let file = mare_binding::fs::open_no_follow_beneath(project_root, rel).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_MCP_JSON_BYTES as u64 {
        return None;
    }
    let mut buf = Vec::with_capacity((meta.len() as usize).min(MAX_MCP_JSON_BYTES));
    let n = file.take((MAX_MCP_JSON_BYTES + 1) as u64).read_to_end(&mut buf).ok()?;
    if n > MAX_MCP_JSON_BYTES {
        return None;
    }
    Some(buf)
}

fn sanitize_mcp_target(server: &serde_json::Value) -> Option<String> {
    let obj = server.as_object()?;
    if let Some(cmd) = obj.get("command") {
        let exe = match cmd {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Array(arr) => arr.first().and_then(serde_json::Value::as_str),
            _ => None,
        };
        if let Some(target) = exe.and_then(sanitize_executable_basename) {
            return Some(target);
        }
    }
    for key in ["url", "httpUrl", "sseUrl"] {
        if let Some(raw_url) = obj.get(key).and_then(serde_json::Value::as_str)
            && let Some(host) = sanitize_url_host(raw_url)
        {
            return Some(host);
        }
    }
    None
}

fn collect_resources(rr: &mare_shared::project::ResourceRoot, cwd: &std::path::Path) -> TrustResources {
    let root = mare_shared::project::trust_subject(rr);
    let mut init_items = Vec::new();
    let mut plugin_items = Vec::new();
    let mut skill_items = Vec::new();
    let mut prompt_items = Vec::new();
    let mut system_items = Vec::new();
    let mut mcp_items = Vec::new();

    for path in mare_config::trust::protected_resources(cwd) {
        let leaf = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut entries: Vec<_> = if path.is_dir() {
            std::fs::read_dir(&path).ok().into_iter().flatten().flatten().map(|entry| entry.path()).collect()
        } else {
            Vec::new()
        };
        if entries.is_empty() {
            entries.push(path.clone());
        }
        entries.sort();
        let target = match leaf {
            "init.lua" => &mut init_items,
            "plugins" => &mut plugin_items,
            "skills" => &mut skill_items,
            "prompts" => &mut prompt_items,
            _ => &mut system_items,
        };
        for entry in entries {
            let shown = entry.strip_prefix(root).unwrap_or(&entry).to_string_lossy().replace('\\', "/");
            target.push(clean_text(&shown));
        }
    }

    if let Some(root) = &rr.root {
        let mut mcp_candidates = vec![std::path::Path::new(".mcp.json")];
        if rr.mare_dir().is_some() {
            mcp_candidates.push(std::path::Path::new(".mare/mcp.json"));
        }
        for rel in mcp_candidates {
            let full = root.join(rel);
            if rr.rejected.contains(&full) {
                continue;
            }
            let Some(contents) = read_bounded_mcp_file(rel, root) else { continue };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&contents) else { continue };
            if let Some(servers) = value
                .get("mcpServers")
                .or_else(|| value.get("mcp_servers"))
                .or_else(|| value.get("servers"))
                .and_then(serde_json::Value::as_object)
            {
                for (name, cfg) in servers {
                    let clean_name = clean_text(name);
                    let item = match sanitize_mcp_target(cfg) {
                        Some(target) => format!("{clean_name} ({target})"),
                        None => clean_name,
                    };
                    mcp_items.push(item);
                }
            }
        }
    }

    let mut groups = Vec::new();
    for (label, mut items) in [
        ("Project config", init_items),
        ("Plugins", plugin_items),
        ("Skills", skill_items),
        ("Prompt templates", prompt_items),
        ("System prompt files", system_items),
        ("MCP servers", mcp_items),
    ] {
        items.sort();
        items.dedup();
        if !items.is_empty() {
            groups.push(ResourceGroup { label, items });
        }
    }

    let has_init_lua = groups.iter().any(|g| g.label == "Project config");
    let has_mcp = groups.iter().any(|g| g.label == "MCP servers");
    TrustResources { groups, has_init_lua, has_mcp }
}

fn build_trust_body(
    rr: &mare_shared::project::ResourceRoot,
    cwd: &std::path::Path,
    home: &str,
    expanded: bool,
) -> (Vec<String>, bool) {
    let subject = mare_shared::project::trust_subject(rr);
    // the same `~` + middle-elision rule as the welcome line and the status bar
    let shown = mare_shared::style::display_path(&subject.display().to_string(), home, 56);
    let mut body = vec![shown];
    if subject != rr.start.as_path() {
        let start_shown = mare_shared::style::display_path(&rr.start.display().to_string(), home, 56);
        body.push(format!("started in {start_shown}"));
    }
    body.push(String::new());
    let resources = collect_resources(rr, cwd);
    let has_more = resources.groups.iter().any(|g| g.items.len() > MAX_GROUP_ITEMS);
    if resources.groups.is_empty() {
        // a git repository without project resources: the question is about
        // the permission preset sessions here start with
        body.push(
            "Untrusted folders start each session read-only: edits and commands ask first. \
             Trusting applies from the next session."
                .to_string(),
        );
    } else {
        if resources.has_init_lua {
            body.push(
                "Warning: .mare/init.lua runs with the full Mare API and can register plugins, \
                 packages, MCP servers, and hooks at runtime."
                    .to_string(),
            );
            body.push(String::new());
        }
        body.push("This folder contains project resources:".to_string());
        for group in &resources.groups {
            body.push(format!("  {}:", group.label));
            let limit = if expanded { group.items.len() } else { MAX_GROUP_ITEMS };
            for item in group.items.iter().take(limit) {
                body.push(format!("    • {}", mare_shared::style::truncate_str(item, 64, "…")));
            }
            if !expanded && group.items.len() > MAX_GROUP_ITEMS {
                body.push(format!("    • +{} more", group.items.len() - MAX_GROUP_ITEMS));
            }
        }
        if has_more {
            body.push(if expanded {
                "Press e to collapse resource list.".to_string()
            } else {
                "Press e to expand full resource list.".to_string()
            });
        }
        if resources.has_mcp {
            body.push("Note: each MCP server still prompts individually after project trust.".to_string());
        }
        body.push(String::new());
        body.push("Untrusted folders start in read-only permission mode; these resources don't load.".to_string());
    }
    (body, has_more)
}

/// After the first frame: `trust/resolve` plugins and the configured default settle the trust
/// decision ([`Host::settle_trust`]); when nothing does and the default is to ask, the overlay.
pub(in crate::app) async fn settle_and_ask_trust(host: &std::sync::Arc<Host>) -> Result<()> {
    if host.settle_trust().await? {
        ask_trust(host).await?;
    }
    Ok(())
}

/// The trust overlay:
/// a normal select (so the same keys, hint bar and test harness apply) shown over the live first
/// frame; the answer goes to [`Host::answer_trust`], which loads the project's resources on
/// "Trust and load" or "Trust this session only". Esc means "Not now".
async fn ask_trust(host: &std::sync::Arc<Host>) -> Result<()> {
    use mare_host::TrustChoice;
    let rr = mare_shared::project::resource_root(&host.cwd);
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    let item = |label: &str, detail: &str| SelectItem {
        label: label.into(),
        detail: Some(detail.into()),
        ..Default::default()
    };
    let mut expanded = false;
    let mut selected = 3;
    let choice = loop {
        let (body, has_more) = build_trust_body(&rr, &host.cwd, &home, expanded);
        let items = vec![
            item("Trust and load", "remember for this folder"),
            item("Trust this session only", "load for this session only; don't save"),
            item("Don't trust", "remember; never load them"),
            item("Not now", "don't load them this time; ask again next time"),
        ];
        let keys =
            if has_more { vec![('e', if expanded { "collapse" } else { "expand list" }.into())] } else { vec![] };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = host.ui.send(mare_shared::ui::UiCmd::Select {
            title: "Trust this folder?".into(),
            title_right: vec![],
            split: Default::default(),
            items,
            selected,
            multi: false,
            accept_label: None,
            allow_search: false,
            filter: String::new(),
            tabs: vec![],
            tab: 0,
            kind: "list".into(),
            keys,
            body,
            vim: false,
            highlight: None,
            reply: tx,
            token: 0,
            modal_id: 0,
            approval: false,
        });
        let reply = if sent {
            rx.await.unwrap_or(Err(mare_shared::ui::Dismissed::Cancelled))
        } else {
            Err(mare_shared::ui::Dismissed::Cancelled)
        };
        match reply {
            Ok(r) if r.key == Some('e') && has_more => {
                expanded = !expanded;
                selected = r.index;
            }
            Ok(r) if r.index == 0 => break TrustChoice::Trust,
            Ok(r) if r.index == 1 => break TrustChoice::TrustSession,
            Ok(r) if r.index == 2 => break TrustChoice::DontTrust,
            _ => break TrustChoice::NotNow,
        }
    };
    host.answer_trust(choice).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_body_shows_init_disclaimer_sanitized_mcp_and_six_item_group_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".mare/plugins")).unwrap();
        std::fs::create_dir_all(root.join(".mare/skills")).unwrap();
        std::fs::write(root.join(".mare/init.lua"), "-- init\n").unwrap();
        for i in 1..=8 {
            std::fs::write(root.join(format!(".mare/plugins/p{i}.lua")), "return {}\n").unwrap();
        }
        std::fs::write(
            root.join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "local-stdio": {
                        "command": "/opt/tools/bin/mcp-runner",
                        "args": ["--secret-arg=super-secret"],
                        "env": { "API_TOKEN": "tok-secret-env" }
                    },
                    "remote-sse": {
                        "url": "https://user:secret-pass@mcp.example.com:8443/sse?token=secret-query#frag"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let rr = mare_shared::project::resource_root(root);
        let (collapsed, has_more) = build_trust_body(&rr, root, "", false);
        assert!(has_more);
        let text = collapsed.join("\n");
        assert!(text.contains(".mare/init.lua runs with the full Mare API"), "{text}");
        assert!(text.contains("register plugins, packages, MCP servers, and hooks"), "{text}");
        assert!(text.contains("local-stdio (mcp-runner)"), "{text}");
        assert!(text.contains("remote-sse (mcp.example.com)"), "{text}");
        assert!(!text.contains("super-secret"), "args must never appear: {text}");
        assert!(!text.contains("tok-secret-env"), "env must never appear: {text}");
        assert!(!text.contains("secret-pass"), "userinfo must never appear: {text}");
        assert!(!text.contains("secret-query"), "query must never appear: {text}");
        assert!(!text.contains("8443"), "url port/path must not leak: {text}");
        assert!(text.contains("each MCP server still prompts individually after project trust"), "{text}");
        assert!(text.contains("+2 more"), "{text}");
        assert!(text.contains("Press e to expand full resource list"), "{text}");
        assert!(!text.contains("p7.lua") && !text.contains("p8.lua"), "{text}");

        let (expanded, has_more_exp) = build_trust_body(&rr, root, "", true);
        assert!(has_more_exp);
        let exp_text = expanded.join("\n");
        assert!(exp_text.contains("p7.lua") && exp_text.contains("p8.lua"), "{exp_text}");
        assert!(!exp_text.contains("+2 more"), "{exp_text}");
        assert!(exp_text.contains("Press e to collapse resource list"), "{exp_text}");
    }

    #[test]
    fn mcp_sanitizer_rejects_inline_command_secrets_and_unparseable_urls_and_bounded_reader_skips_symlinks_and_fifos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside-secret-mcp.json");
        std::fs::write(&outside, r#"{"mcpServers":{"leaked":{"command":"sh"}}}"#).unwrap();

        std::fs::create_dir_all(root.join(".mare")).unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "inline-str": {
                        "command": "/usr/bin/mcp-cli --token=inline-secret-1"
                    },
                    "inline-arr": {
                        "command": ["/opt/bin/mcp --password=inline-secret-2", "--flag"]
                    },
                    "bad-url": {
                        "url": "https://user:pass@bad host/sse?k=inline-secret-3"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join(".mare/mcp.json")).expect("symlink setup");
        }

        let rr = mare_shared::project::resource_root(root);
        let resources = collect_resources(&rr, root);
        let mcp = resources.groups.iter().find(|g| g.label == "MCP servers").expect("mcp group");
        assert_eq!(mcp.items, vec!["bad-url", "inline-arr", "inline-str"]);
        let joined = mcp.items.join("\n");
        assert!(!joined.contains("inline-secret"), "{joined}");
        assert!(!joined.contains("leaked"), "symlinked outside file must never be read: {joined}");

        #[cfg(unix)]
        {
            let fifo_dir = tempfile::tempdir().unwrap();
            let fifo_path = fifo_dir.path().join(".mcp.json");
            let c_path = std::ffi::CString::new(fifo_path.to_string_lossy().as_bytes()).unwrap();
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(rc, 0, "mkfifo failed");
            assert!(
                read_bounded_mcp_file(std::path::Path::new(".mcp.json"), fifo_dir.path()).is_none(),
                "FIFO must be skipped without blocking"
            );

            // Root swapped with a symlink must also be refused.
            let link_root = outside_dir.path().join("swapped-root");
            std::os::unix::fs::symlink(root, &link_root).unwrap();
            assert!(
                read_bounded_mcp_file(std::path::Path::new(".mcp.json"), &link_root).is_none(),
                "symlinked project root must be refused"
            );
        }
    }
}

//! `mare plugin tree [--json]`: the composed plugin tree
//! every row with its source layer, whether it is disabled or refused, its
//! fiber state, and the services a PENDING plugin still waits for. Read from the `loader` host
//! plugin's `dump()`; a row stuck loading, unloading or waiting past `stall_ms` carries `stalled`
//! (`Kernel::stalled`: state, ms, effect labels, missing services, summary).

use anyhow::{Result, anyhow};
use mare_host::{Host, HostOptions};
use serde_json::Value;

/// Start a host with `opts` (built from the command line like a headless run's) and return the
/// loader's dump as JSON.
async fn load(opts: HostOptions) -> Result<Value> {
    let host = Host::new(opts).await?;
    // the loader runs the plugin tree; a host whose boot failed has none to ask
    let booted = match host.kernel.cordis_boot() {
        Ok(b) => b,
        Err(e) => {
            host.shutdown().await;
            return Err(anyhow!("the plugin tree did not boot: {e}"));
        }
    };
    let dump: Option<String> = host
        .lua()
        .load(
            r#"local booted = ...
            local loader = booted.root:get("loader")
            if not loader then return nil end
            return mare.json.encode(loader:dump())"#,
        )
        .set_name("=plugins tree")
        .call_async(booted)
        .await?;
    let stalled = host.kernel.stalled();
    host.shutdown().await;
    let dump = dump.ok_or_else(|| anyhow!("the plugin tree is not run by the loader host plugin in this build"))?;
    let mut dump: Value = serde_json::from_str(&dump)?;
    attach_stalled(&mut dump, &stalled);
    Ok(dump)
}

/// Put each stall on its row as `stalled` (a list: an old generation still unloading and the new
/// one loading are two entries under one name).
fn attach_stalled(dump: &mut Value, stalled: &[mare_kernel::StalledPlugin]) {
    let Some(rows) = dump.as_array_mut() else { return };
    for row in rows.iter_mut().filter(|r| r.get("refused").is_none_or(Value::is_null)) {
        let id = text(row, "id").to_string();
        let list: Vec<Value> = stalled
            .iter()
            .filter(|s| s.plugin == id)
            .map(|s| {
                serde_json::json!({
                    "state": s.state,
                    "ms": s.elapsed_ms,
                    "effects": s.effects,
                    "missing": s.missing,
                    "summary": s.message(),
                })
            })
            .collect();
        if !list.is_empty()
            && let Some(obj) = row.as_object_mut()
        {
            obj.insert("stalled".into(), Value::Array(list));
        }
    }
}

pub(crate) async fn run(opts: HostOptions, json: bool) -> Result<i32> {
    let dump = load(opts).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&dump)?);
    } else {
        print!("{}", render(&dump));
    }
    Ok(0)
}

fn text<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}

fn names(item: &Value, key: &str) -> Vec<String> {
    item.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// One line per row: `id  origin  status  details`. Status is the fiber state, `disabled`, or
/// `refused`; details name what a PENDING plugin waits for, what a refused one conflicts with, and
/// the layer an override replaced.
pub(crate) fn render(dump: &Value) -> String {
    let rows = dump.as_array().cloned().unwrap_or_default();
    let width = rows.iter().map(|r| text(r, "id").len()).max().unwrap_or(0).max(6);
    let mut out = String::new();
    for r in &rows {
        let refused = text(r, "refused");
        let disabled = r.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        let status = if !refused.is_empty() {
            "refused".to_string()
        } else if disabled {
            "disabled".to_string()
        } else {
            text(r, "state").to_lowercase()
        };
        let mut details = vec![];
        let missing = names(r, "missing");
        if !missing.is_empty() {
            details.push(format!("waits for {}", missing.join(", ")));
        }
        if !refused.is_empty() {
            details.push(refused.to_string());
        }
        let overrides = text(r, "overrides");
        if !overrides.is_empty() {
            details.push(format!("overrides {overrides}"));
        }
        let path = text(r, "path");
        if !path.is_empty() {
            details.push(path.to_string());
        }
        for s in r.get("stalled").and_then(Value::as_array).into_iter().flatten() {
            details.push(text(s, "summary").to_string());
        }
        let line =
            format!("{:<width$}  {:<8}  {:<8}  {}", text(r, "id"), text(r, "origin"), status, details.join("; "));
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{attach_stalled, render};
    use serde_json::json;

    #[test]
    fn a_stuck_row_carries_its_stall_in_json_and_text() {
        let mut dump = json!([
            { "id": "watch", "origin": "user", "state": "LOADING", "disabled": false },
            { "id": "dup", "origin": "user", "disabled": true, "refused": "provides x" }
        ]);
        let stalled = [
            mare_kernel::StalledPlugin {
                plugin: "watch".into(),
                state: "UNLOADING".into(),
                elapsed_ms: 11_000,
                effects: vec!["watcher".into()],
                missing: vec![],
            },
            mare_kernel::StalledPlugin { plugin: "dup".into(), state: "PENDING".into(), ..Default::default() },
        ];
        attach_stalled(&mut dump, &stalled);
        let s = &dump[0]["stalled"][0];
        assert_eq!((s["state"].as_str(), s["ms"].as_u64()), (Some("UNLOADING"), Some(11_000)));
        assert_eq!(s["effects"], json!(["watcher"]));
        assert!(dump[1].get("stalled").is_none(), "a refused definition never runs");
        assert!(
            render(&dump).contains("plugin watch still unloading after 11s: waiting for the cleanup of 'watcher'"),
            "{}",
            render(&dump)
        );
    }

    #[test]
    fn renders_state_disabled_refused_and_missing_services() {
        let dump = json!([
            { "id": "llm", "origin": "builtin", "state": "ACTIVE", "disabled": false, "provides": ["llm"] },
            { "id": "usage", "origin": "builtin", "state": "PENDING", "disabled": false, "missing": ["llm"] },
            { "id": "theme", "origin": "user", "state": "ACTIVE", "disabled": false, "overrides": "builtin",
              "path": "/u/plugins/theme/init.lua" },
            { "id": "web", "disabled": true },
            { "id": "dup", "origin": "user", "disabled": true, "refused": "provides llm already provided by llm (<builtin>)" }
        ]);
        assert_eq!(
            render(&dump),
            concat!(
                "llm     builtin   active\n",
                "usage   builtin   pending   waits for llm\n",
                "theme   user      active    overrides builtin; /u/plugins/theme/init.lua\n",
                "web               disabled\n",
                "dup     user      refused   provides llm already provided by llm (<builtin>)\n",
            )
        );
    }
}

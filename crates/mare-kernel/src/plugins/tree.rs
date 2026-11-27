//! The plugin tree's state as the host reads it: the `loader` host plugin's
//! `dump()` (one item per row plus the refused definitions), the same data
//! `mare plugin tree --json` prints. It replaces the kernel's own active set for the host's
//! introspection (`mare.plugins`, failures, doctor).

use mlua::{Function, Table, Value};

use crate::Kernel;

/// One row of the plugin tree (`loader:dump()`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginTreeRow {
    pub id: String,
    pub origin: Option<String>,
    pub path: Option<String>,
    pub overrides: Option<String>,
    pub refused: Option<String>,
    pub disabled: bool,
    /// the fiber's state (`PENDING`, `LOADING`, `ACTIVE`, `FAILED`, `DISPOSED`, `UNLOADING`), or
    /// `NONE` for a row whose fiber has not started; `None` for a disabled or refused row
    pub state: Option<String>,
    /// for a PENDING row: the injected services with no admitted provider
    pub missing: Vec<String>,
    pub provides: Vec<String>,
}

impl PluginTreeRow {
    pub fn active(&self) -> bool {
        self.state.as_deref() == Some("ACTIVE")
    }
}

fn strings(t: &Table, key: &str) -> mlua::Result<Vec<String>> {
    Ok(t.get::<Option<Vec<String>>>(key)?.unwrap_or_default())
}

fn row_from_table(t: &Table) -> mlua::Result<PluginTreeRow> {
    Ok(PluginTreeRow {
        id: t.get("id")?,
        origin: t.get("origin")?,
        path: t.get("path")?,
        overrides: t.get("overrides")?,
        refused: t.get("refused")?,
        disabled: t.get::<Option<bool>>("disabled")?.unwrap_or(false),
        state: t.get("state")?,
        missing: strings(t, "missing")?,
        provides: strings(t, "provides")?,
    })
}

pub(crate) fn rows_from_tables(items: &[Table]) -> mlua::Result<Vec<PluginTreeRow>> {
    items.iter().map(row_from_table).collect()
}

impl Kernel {
    fn loader_table(&self) -> mlua::Result<Option<Table>> {
        let booted = self.cordis_boot()?;
        let root: Table = booted.get("root")?;
        let get: Function = root.get("get")?;
        match get.call::<Value>((root.clone(), "loader", false))? {
            Value::Table(loader) => Ok(Some(loader)),
            _ => Ok(None),
        }
    }

    /// The plugin tree, row by row (empty before boot.lua started the loader).
    pub fn plugin_tree(&self) -> mlua::Result<Vec<PluginTreeRow>> {
        let Some(loader) = self.loader_table()? else {
            return Ok(vec![]);
        };
        let dump: Function = loader.get("dump")?;
        let opts = self.lua().create_table()?;
        opts.raw_set("effects", false)?;
        let items: Vec<Table> = dump.call((loader, opts))?;
        rows_from_tables(&items)
    }

    /// One row of the plugin tree by id, without dumping every other plugin row.
    pub fn plugin_tree_row(&self, name: &str) -> mlua::Result<Option<PluginTreeRow>> {
        let Some(loader) = self.loader_table()? else {
            return Ok(None);
        };
        let row_state: Function = loader.get("row_state")?;
        match row_state.call::<Value>((loader, name))? {
            Value::Table(t) => Ok(Some(row_from_table(&t)?)),
            _ => Ok(None),
        }
    }

    /// The services plugin `name` waits for when its fiber is PENDING (#8: not started, not
    /// failed); empty otherwise.
    pub fn plugin_missing(&self, name: &str) -> Vec<String> {
        match self.plugin_tree_row(name).ok().flatten() {
            Some(row) if row.refused.is_none() && row.state.as_deref() == Some("PENDING") => row.missing,
            _ => vec![],
        }
    }

    /// [`Self::plugin_missing`] over a tree snapshot the caller already took.
    pub fn missing_in(tree: &[PluginTreeRow], name: &str) -> Vec<String> {
        tree.iter()
            .find(|r| r.id == name && r.refused.is_none() && r.state.as_deref() == Some("PENDING"))
            .map(|r| r.missing.clone())
            .unwrap_or_default()
    }

    /// What waits now: every enabled row left PENDING, "plugin <id> is waiting for <services>"
    /// (boot.lua `booted.pending()`, the same lines a reload reports).
    pub async fn waiting(&self) -> Vec<String> {
        let Ok(booted) = self.cordis_boot() else { return vec![] };
        let Ok(pending) = booted.get::<mlua::Function>("pending") else { return vec![] };
        mare_shared::lua::call_async::<Vec<String>>(self.lua(), &pending, ()).await.unwrap_or_default()
    }

    /// Whether plugin `name`'s fiber is ACTIVE in the tree.
    pub fn plugin_active(&self, name: &str) -> bool {
        self.plugin_tree_row(name).ok().flatten().is_some_and(|row| row.refused.is_none() && row.active())
    }

    /// [`Self::plugin_active`] over a tree snapshot the caller already took.
    pub fn active_in(tree: &[PluginTreeRow], name: &str) -> bool {
        tree.iter().any(|r| r.id == name && r.refused.is_none() && r.active())
    }
}

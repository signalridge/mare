//! Package operations: the parallel runner, sync / install, update, restore, clean, and the
//! reports (`status`, `list`, log, diff, doctor).

use crate::*;

pub(super) mod clean;
pub(super) mod report;
pub(super) mod restore;
mod runner;
pub(super) mod sync;
pub(super) mod update;

fn find_spec(cfg: &serde_json::Value, name: &str) -> Result<PackageSpec> {
    specs(cfg)
        .into_iter()
        .chain(disabled_specs(cfg))
        .find(|s| s.name == name && s.dir.is_none())
        .with_context(|| format!("no declared package named {name}"))
}

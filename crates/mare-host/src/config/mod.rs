//! The configuration: evaluating its six layers, re-running it for a reload, and the runtime
//! surface installed after startup.

pub(crate) mod eval;
pub mod options;
mod reevaluate;
mod runtime;
mod txn;

/// Commit effective host config to both the kernel snapshot and workspace snapshot together.
pub(crate) fn commit_kernel_config(
    kernel: &mare_kernel::Kernel,
    workspace: &crate::WorkspaceState,
    cfg: serde_json::Value,
) {
    kernel.set_config(cfg.clone());
    workspace.update_config(cfg);
}

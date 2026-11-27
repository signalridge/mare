//! Look up the plugin associated with an owner record.

use crate::Kernel;

impl Kernel {
    pub fn ctx_plugin(&self, ctx: u64) -> Option<String> {
        self.st().ctxs.get(&ctx).map(|c| c.plugin.clone())
    }
}

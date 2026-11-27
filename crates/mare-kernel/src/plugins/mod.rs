//! Plugin definitions and their life: evaluation, the same-name override rule, dependency
//! order, apply / dispose and hot reload.

mod definitions;
mod lifecycle;
pub(crate) mod overrides;
pub(crate) mod prims;
pub(crate) mod provide_check;
pub(crate) mod reload;
pub(crate) mod rows;
pub(crate) mod stalls;
pub(crate) mod tree;

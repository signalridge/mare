//! The effective package set: enabled specs in load order (`specs`), what keeps them from
//! loading (`health`) and what they ship besides plugins (`contents`).

pub(super) mod contents;
pub(crate) mod health;
pub(crate) mod specs;

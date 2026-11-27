//! Drawing and what is drawn: blocks, modals, notices, the transcript, scrollback, UI commands.

pub(in crate::app) mod block;
pub(in crate::app) mod lua_components;
pub(in crate::app) mod modal;
pub(in crate::app) mod notices;
mod render;
pub(in crate::app) mod scrollback;
pub(in crate::app) mod select_filter;
mod select_update;
pub(in crate::app) mod selection;
pub(in crate::app) mod transcript;
mod ui_cmd;

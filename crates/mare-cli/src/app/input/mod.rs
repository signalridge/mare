//! Input: key routing, key help, completion, prompt history, submit, paste, the external editor.

pub(in crate::app) mod completion;
pub(in crate::app) mod external_editor;
mod key_help;
pub(in crate::app) use key_help::action_description;
mod keymap;
mod keys;
mod modal_keys;
pub(in crate::app) use modal_keys::select_chain;
pub(in crate::app) mod paste;
pub(in crate::app) mod prompt_history;
mod submit;

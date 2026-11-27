//! Process-level frontends against the real binary: `--rpc` (stdlib Python client), ACP over
//! stdio, the headless `--json` event contract, and terminal restore under a PTY.

use std::io::BufReader;
use std::process::Stdio;

use crate::common::*;
use serde_json::{Value, json};

mod acp;
mod acp_load;
mod doctor;
mod errors_and_themes;
mod headless;
mod headless_audit;
mod rpc;
mod rpc_lifecycle;
mod rpc_options_and_media;
mod rpc_select;
mod rpc_ui_and_receipts;
mod services_and_contracts;
mod signals;
mod terminal;

#[cfg(target_os = "linux")]
mod headless_pressure;
#[cfg(target_os = "linux")]
mod signals_process_trees;

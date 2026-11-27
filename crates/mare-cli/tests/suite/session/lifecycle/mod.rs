//! Session lifecycle: delete, archive, rename, snapshots and commands that move the session.

use super::*;

mod attachments_and_races;
mod fork_snapshots;
mod management;
mod switching;

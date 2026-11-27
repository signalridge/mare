//! Agent loop with the scripted faux model: tool execution, batching, turn-stopping, retries,
//! abort, steer, follow-up and tool errors (pi agent-session-*, Codex abort_tasks/pending_input).

mod events;
mod input_admission;
mod lifecycle;
mod models;
mod queue;
mod settled;
mod steps;
mod turns;
mod wire_turns;

//! Codex app-server emulator backed by an ACP agent.
//!
//! The bridge speaks the Codex app-server v2 protocol to its clients (the
//! Codex TUI over a unix socket, or anything else over stdio) and ACP v1 to
//! agent subprocesses. All state lives in one actor task ([`server`]); slow
//! ACP calls run in spawned tasks that report back to the actor, so client
//! requests and agent events are always processed in order without locks.

mod agents;
mod approvals;
mod content;
mod ext;
mod handoff;
mod history;
mod models;
mod records;
mod requests;
mod select;
mod server;
mod threads;
mod tool_calls;
mod turn;

pub use agents::AgentProfile;
pub use agents::ConnectedAgent;
pub use ext::AGENT_CONFIG_KEY;
pub use server::BridgeOptions;
pub use server::BridgeTransport;
pub use server::serve;
pub use server::start;

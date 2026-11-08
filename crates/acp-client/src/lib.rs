//! ACP v1 client built on the official Rust SDK.
//!
//! This crate knows nothing about Codex. It spawns an agent subprocess,
//! performs the `initialize` handshake, and exposes the stable v1 client
//! surface as typed calls plus one ordered [`AgentEvent`] stream.
//!
//! Ordering contract: session updates, agent-initiated requests, and the
//! completion of `session/prompt` / `session/load` are delivered on the same
//! channel in the order the agent sent them. A `PromptFinished` event is never
//! observed before the updates that preceded the prompt response on the wire.

mod auth;
mod capabilities;
mod connection;
mod error;
mod process;

pub use agent_client_protocol::schema::MaybeUndefined;
pub use agent_client_protocol::v1::schema;
pub use auth::TerminalAuthCommand;
pub use auth::is_sensitive_env_var;
pub use capabilities::AgentCaps;
pub use capabilities::ClientOptions;
pub use connection::AgentConnection;
pub use connection::AgentEvent;
pub use connection::LoadId;
pub use connection::PendingRequestId;
pub use connection::PromptId;
pub use error::AcpError;
pub use error::ConnectError;
pub use process::AgentCommand;

use serde_json::Value;

/// A JSON-RPC error returned by the agent or produced locally by the SDK.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct AcpError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl AcpError {
    pub const AUTH_REQUIRED: i32 = -32000;
    pub const RESOURCE_NOT_FOUND: i32 = -32002;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    pub const REQUEST_CANCELLED: i32 = -32800;

    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn is_auth_required(&self) -> bool {
        self.code == Self::AUTH_REQUIRED
    }

    pub fn is_resource_not_found(&self) -> bool {
        self.code == Self::RESOURCE_NOT_FOUND
    }

    pub fn is_method_not_found(&self) -> bool {
        self.code == Self::METHOD_NOT_FOUND
    }

    /// Requests the client refused locally because the agent did not
    /// advertise the capability that gates them.
    pub fn unsupported(method: &str) -> Self {
        Self::new(
            Self::METHOD_NOT_FOUND,
            format!("agent does not advertise support for {method}"),
        )
    }
}

impl From<agent_client_protocol::Error> for AcpError {
    fn from(err: agent_client_protocol::Error) -> Self {
        Self {
            code: i32::from(err.code),
            message: err.message,
            data: err.data,
        }
    }
}

/// Failure to start or initialize an agent.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("failed to start agent `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("agent rejected initialize: {0}")]
    Initialize(AcpError),
    #[error("agent speaks ACP protocol version {0}, but acpc only supports version 1")]
    UnsupportedProtocolVersion(String),
    #[error("agent exited during initialize{}", stderr_suffix(.stderr))]
    Exited { stderr: String },
    #[error("agent did not answer initialize within {seconds}s{}", stderr_suffix(.stderr))]
    Timeout { seconds: u64, stderr: String },
}

fn stderr_suffix(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(":\n{trimmed}")
    }
}

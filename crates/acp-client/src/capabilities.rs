use agent_client_protocol::v1::schema as acp;

/// What this client advertises in `initialize`.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
    /// Advertise `auth.terminal`. Only set this when the caller can hand an
    /// interactive terminal to a relaunched agent process.
    pub terminal_auth: bool,
}

impl ClientOptions {
    pub(crate) fn client_capabilities(&self) -> acp::ClientCapabilities {
        // fs/* and terminal/* stay unadvertised: agents then use their own
        // file system and shell, which is fully spec-compliant.
        acp::ClientCapabilities::new()
            .auth(acp::AuthCapabilities::new().terminal(self.terminal_auth))
            .elicitation(
                acp::ElicitationCapabilities::new()
                    .form(acp::ElicitationFormCapabilities::new())
                    .url(acp::ElicitationUrlCapabilities::new()),
            )
            .session(
                acp::ClientSessionCapabilities::new()
                    .compaction(acp::CompactionCapabilities::new())
                    .notices(acp::NoticeCapabilities::new())
                    .config_options(
                        acp::SessionConfigOptionsCapabilities::new()
                            .boolean(acp::BooleanConfigOptionCapabilities::new()),
                    ),
            )
    }

    pub(crate) fn client_info(&self) -> acp::Implementation {
        acp::Implementation::new(&self.name, &self.version).title(self.title.clone())
    }
}

/// The agent's `initialize` response with capability-gating helpers.
#[derive(Debug, Clone)]
pub struct AgentCaps {
    pub init: acp::InitializeResponse,
}

impl AgentCaps {
    pub fn agent_name(&self) -> Option<&str> {
        self.init
            .agent_info
            .as_ref()
            .map(|info| info.title.as_deref().unwrap_or(&info.name))
    }

    pub fn auth_methods(&self) -> &[acp::AuthMethod] {
        &self.init.auth_methods
    }

    pub fn load_session(&self) -> bool {
        self.init.agent_capabilities.load_session
    }

    pub fn resume_session(&self) -> bool {
        self.init
            .agent_capabilities
            .session_capabilities
            .resume
            .is_some()
    }

    pub fn list_sessions(&self) -> bool {
        self.init
            .agent_capabilities
            .session_capabilities
            .list
            .is_some()
    }

    pub fn close_session(&self) -> bool {
        self.init
            .agent_capabilities
            .session_capabilities
            .close
            .is_some()
    }

    pub fn delete_session(&self) -> bool {
        self.init
            .agent_capabilities
            .session_capabilities
            .delete
            .is_some()
    }

    pub fn additional_directories(&self) -> bool {
        self.init
            .agent_capabilities
            .session_capabilities
            .additional_directories
            .is_some()
    }

    pub fn logout(&self) -> bool {
        self.init.agent_capabilities.auth.logout.is_some()
    }

    pub fn prompt_image(&self) -> bool {
        self.init.agent_capabilities.prompt_capabilities.image
    }

    pub fn prompt_audio(&self) -> bool {
        self.init.agent_capabilities.prompt_capabilities.audio
    }

    pub fn prompt_embedded_context(&self) -> bool {
        self.init
            .agent_capabilities
            .prompt_capabilities
            .embedded_context
    }

    pub fn mcp_http(&self) -> bool {
        self.init.agent_capabilities.mcp_capabilities.http
    }

    pub fn mcp_sse(&self) -> bool {
        self.init.agent_capabilities.mcp_capabilities.sse
    }

    /// Drop MCP servers whose transport the agent did not advertise.
    pub fn filter_mcp_servers(&self, servers: Vec<acp::McpServer>) -> Vec<acp::McpServer> {
        servers
            .into_iter()
            .filter(|server| match server {
                acp::McpServer::Stdio(_) => true,
                acp::McpServer::Http(_) => self.mcp_http(),
                acp::McpServer::Sse(_) => self.mcp_sse(),
                _ => false,
            })
            .collect()
    }
}

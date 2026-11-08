//! `~/.config/acpc/config.toml`.
//!
//! ```toml
//! agent = "claude"                     # default agent slug
//!
//! [[agents]]
//! slug = "claude"
//! name = "Claude"
//! command = "npx"
//! args = ["-y", "@agentclientprotocol/claude-agent-acp@latest"]
//! env = { SOME_VAR = "value" }
//! default_model = "opus"
//! model_override = { env = "ANTHROPIC_MODEL" }   # or { arg = "--model" }
//!
//! [[mcp_servers]]
//! name = "files"
//! command = "mcp-files"                # stdio transport
//! # url = "https://…"; transport = "http" | "sse"
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use acp_bridge::AgentProfile;
use acp_client::AgentCommand;
use acp_client::schema as acp;
use anyhow::Context;
use anyhow::bail;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Slug of the agent used when `--agent` is not given.
    pub agent: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub slug: String,
    pub name: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Model forced at spawn through `model_override`.
    pub default_model: Option<String>,
    pub model_override: Option<ModelOverride>,
}

/// The out-of-band channel an agent reads a forced model id from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    pub env: Option<String>,
    pub arg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub command: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    /// `http` (default for `url`) or `sse`.
    pub transport: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for agent in &self.agents {
            if agent.slug.trim().is_empty() {
                bail!("an [[agents]] entry has an empty slug");
            }
            if !seen.insert(agent.slug.as_str()) {
                bail!("duplicate agent slug `{}`", agent.slug);
            }
            if let Some(model_override) = &agent.model_override
                && model_override.env.is_none()
                && model_override.arg.is_none()
            {
                bail!("agent `{}`: model_override needs `env` or `arg`", agent.slug);
            }
        }
        for server in &self.mcp_servers {
            if server.command.is_some() == server.url.is_some() {
                bail!(
                    "mcp server `{}`: set exactly one of `command` or `url`",
                    server.name
                );
            }
        }
        Ok(())
    }

    /// The agent to start with: `--agent`, then `agent =`, then the only one.
    pub fn select_agent(&self, requested: Option<&str>) -> anyhow::Result<String> {
        if self.agents.is_empty() {
            bail!(
                "no agents configured. Add an [[agents]] entry to {}",
                crate::home::config_path().display()
            );
        }
        let slug = match requested.or(self.agent.as_deref()) {
            Some(slug) => slug.to_string(),
            None if self.agents.len() == 1 => self.agents[0].slug.clone(),
            None => bail!("several agents are configured; set `agent = \"…\"` or pass --agent"),
        };
        if !self.agents.iter().any(|agent| agent.slug == slug) {
            let known: Vec<&str> = self.agents.iter().map(|agent| agent.slug.as_str()).collect();
            bail!("unknown agent `{slug}` (configured: {})", known.join(", "));
        }
        Ok(slug)
    }

    pub fn mcp_servers(&self) -> Vec<acp::McpServer> {
        self.mcp_servers.iter().map(McpServerConfig::to_acp).collect()
    }
}

impl AgentConfig {
    /// The launch command, with the default model injected and acpc's own
    /// process environment undone so the agent sees the user's environment.
    pub fn to_profile(&self, env_restore: &EnvRestore) -> AgentProfile {
        let mut command = AgentCommand {
            command: self.command.clone(),
            args: self.args.clone(),
            env: Vec::new(),
            env_remove: env_restore.remove.clone(),
            cwd: None,
        };
        command.env.extend(env_restore.set.iter().cloned());
        command
            .env
            .extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        if let (Some(model), Some(model_override)) = (&self.default_model, &self.model_override) {
            if let Some(env) = &model_override.env {
                command.env.push((env.clone(), model.clone()));
            } else if let Some(arg) = &model_override.arg {
                command.args.push(arg.clone());
                command.args.push(model.clone());
            }
        }
        AgentProfile {
            slug: self.slug.clone(),
            name: self.name.clone().unwrap_or_else(|| self.slug.clone()),
            command,
        }
    }
}

impl McpServerConfig {
    fn to_acp(&self) -> acp::McpServer {
        if let Some(url) = &self.url {
            let headers = self
                .headers
                .iter()
                .map(|(name, value)| acp::HttpHeader::new(name, value))
                .collect();
            if self.transport.as_deref() == Some("sse") {
                return acp::McpServer::Sse(acp::McpServerSse::new(&self.name, url).headers(headers));
            }
            return acp::McpServer::Http(acp::McpServerHttp::new(&self.name, url).headers(headers));
        }
        let env = self
            .env
            .iter()
            .map(|(name, value)| acp::EnvVariable::new(name, value))
            .collect();
        acp::McpServer::Stdio(
            acp::McpServerStdio::new(&self.name, self.command.clone().unwrap_or_default())
                .args(self.args.clone())
                .env(env),
        )
    }
}

/// Environment changes acpc makes for the embedded TUI that agents must not
/// inherit (e.g. `CODEX_HOME`).
#[derive(Debug, Clone, Default)]
pub struct EnvRestore {
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_agents_and_injects_default_model() {
        let config: Config = toml::from_str(
            r#"
agent = "a"
[[agents]]
slug = "a"
command = "agent-a"
args = ["acp"]
default_model = "big"
model_override = { arg = "--model" }

[[agents]]
slug = "b"
command = "agent-b"
default_model = "small"
model_override = { env = "B_MODEL" }
"#,
        )
        .expect("config");
        config.validate().expect("valid");
        let restore = EnvRestore {
            set: vec![],
            remove: vec!["CODEX_HOME".to_string()],
        };
        let a = config.agents[0].to_profile(&restore);
        assert_eq!(a.command.args, vec!["acp", "--model", "big"]);
        assert_eq!(a.command.env_remove, vec!["CODEX_HOME".to_string()]);
        let b = config.agents[1].to_profile(&restore);
        assert_eq!(b.command.env, vec![("B_MODEL".to_string(), "small".to_string())]);
        assert_eq!(config.select_agent(None).expect("agent"), "a");
        assert!(config.select_agent(Some("c")).is_err());
    }

    #[test]
    fn rejects_duplicate_slugs() {
        let config: Config = toml::from_str(
            "[[agents]]\nslug = \"a\"\ncommand = \"x\"\n[[agents]]\nslug = \"a\"\ncommand = \"y\"\n",
        )
        .expect("config");
        assert!(config.validate().is_err());
    }
}

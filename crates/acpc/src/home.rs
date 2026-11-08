//! acpc's directories and the private `CODEX_HOME` given to the Codex TUI.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

/// `$ACPC_CONFIG`, else `$XDG_CONFIG_HOME/acpc/config.toml`.
pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ACPC_CONFIG") {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("acpc")
        .join("config.toml")
}

/// `$ACPC_HOME`, else `$XDG_DATA_HOME/acpc`: logs, sockets, the TUI's home.
pub fn data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("ACPC_HOME") {
        return PathBuf::from(path);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("acpc")
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

/// Settings acpc forces on the embedded Codex TUI. The TUI never talks to
/// OpenAI through acpc, so every Codex network feature is switched off.
const MANAGED_CODEX_CONFIG: &str = r#"# Managed by acpc; rewritten on every launch. Edit ~/.config/acpc/config.toml instead.
check_for_update_on_startup = false

[analytics]
enabled = false

[feedback]
enabled = false

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

# Codex's welcome art, tips and server-version notices describe Codex, not
# the ACP agent.
[tui]
show_tooltips = false
show_server_version_notice = false

[tui.effects]
welcome = false
"#;

pub struct Home {
    pub codex_home: PathBuf,
    pub log_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl Home {
    pub fn prepare() -> anyhow::Result<Self> {
        let data_dir = data_dir();
        let codex_home = data_dir.join("codex");
        let log_dir = data_dir.join("log");
        let run_dir = data_dir.join("run");
        for dir in [&data_dir, &codex_home, &log_dir, &run_dir] {
            create_private_dir(dir)?;
        }
        let config = codex_home.join("config.toml");
        std::fs::write(&config, MANAGED_CODEX_CONFIG)
            .with_context(|| format!("writing {}", config.display()))?;
        Ok(Self {
            codex_home,
            log_dir,
            run_dir,
        })
    }

    pub fn socket_path(&self) -> PathBuf {
        self.run_dir.join(format!("bridge-{}.sock", std::process::id()))
    }
}

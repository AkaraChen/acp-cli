// Laying out codex-tui's startup future exceeds rustc's default query depth
// (same as upstream's tui/src/main.rs).
#![recursion_limit = "256"]

mod add;
mod auth;
mod config;
mod home;
mod prompt;

use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;

use acp_bridge::BridgeOptions;
use acp_bridge::BridgeTransport;
use acp_bridge::ConnectedAgent;
use acp_client::ClientOptions;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use codex_arg0::Arg0DispatchPaths;
use codex_tui::ExitReason;
use codex_tui::RemoteAppServerEndpoint;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::EnvRestore;
use crate::home::Home;

/// acpc's own release version (the crate version tracks upstream Codex so
/// the TUI sees a matching server version). Release builds take it from the
/// tag through `ACPC_RELEASE_VERSION`.
const ACPC_VERSION: &str = match option_env!("ACPC_RELEASE_VERSION") {
    Some(version) => version,
    None => "0.1.0",
};

/// Stack size for threads that poll the TUI's futures (as upstream Codex).
const THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "acpc", version = ACPC_VERSION, about = "A terminal client for ACP agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Agent to use (a `slug` from config.toml).
    #[arg(long, short = 'a', global = true)]
    agent: Option<String>,

    /// Working directory for new sessions.
    #[arg(long = "cd", short = 'C', global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// YOLO mode: approve every agent permission request without asking.
    /// Same as `yolo = true` in config.toml.
    #[arg(long, global = true)]
    yolo: bool,

    /// Optional first prompt.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Add an agent to config.toml, e.g. `acpc add "opencode acp"`.
    Add {
        /// Command that starts the agent in ACP mode: one quoted string, or
        /// its words (options for `add` go before them).
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
        /// Slug for `--agent` (default: derived from the command).
        #[arg(long)]
        slug: Option<String>,
        /// Display name (default: the slug).
        #[arg(long)]
        name: Option<String>,
        /// Make it the default agent.
        #[arg(long)]
        default: bool,
    },
    /// Serve the Codex app-server protocol on stdin/stdout, backed by an ACP agent.
    AppServer,
    /// Sign in to the agent (runs its advertised ACP auth method), then exit.
    Login,
    /// Pick a previous session to continue (or pass its id).
    Resume {
        /// Session id printed when acpc exited.
        session_id: Option<String>,
        /// Continue the most recent session in this directory.
        #[arg(long)]
        last: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Add {
        command,
        slug,
        name,
        default,
    }) = cli.command
    {
        return add_agent(command, slug, name, default);
    }
    let config = Config::load(&home::config_path())?;
    let home = Home::prepare()?;
    init_logging(&home)?;

    // The embedded Codex TUI reads its settings from CODEX_HOME; point it at
    // acpc's private copy. Agents get the user's original value back.
    let env_restore = match std::env::var("CODEX_HOME") {
        Ok(value) => EnvRestore {
            set: vec![("CODEX_HOME".to_string(), value)],
            remove: Vec::new(),
        },
        Err(_) => EnvRestore {
            set: Vec::new(),
            remove: vec!["CODEX_HOME".to_string()],
        },
    };
    // SAFETY: still single-threaded; no runtime or helper threads exist yet.
    unsafe {
        std::env::set_var("CODEX_HOME", &home.codex_home);
    }

    let thread = std::thread::Builder::new()
        .name("acpc-main".to_string())
        .stack_size(THREAD_STACK_SIZE)
        .spawn(move || -> anyhow::Result<i32> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(THREAD_STACK_SIZE)
                .build()?;
            runtime.block_on(run(cli, config, home, env_restore))
        })?;
    let code = match thread.join() {
        Ok(result) => result?,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    std::process::exit(code);
}

fn add_agent(command: Vec<String>, slug: Option<String>, name: Option<String>, default: bool) -> anyhow::Result<()> {
    let path = home::config_path();
    let request = add::AddAgent {
        command,
        slug,
        name,
        make_default: default,
    };
    let added = add::add_agent(&path, request)?;
    println!("Added agent `{}` to {}.", added.slug, path.display());
    if !added.command_found {
        println!("Note: its command was not found; install it or fix `command` there before use.");
    }
    if added.default.as_deref() == Some(added.slug.as_str()) {
        println!("Start it with: acpc");
    } else {
        println!("Start it with: acpc -a {}", added.slug);
    }
    Ok(())
}

fn init_logging(home: &Home) -> anyhow::Result<()> {
    let path = home.log_dir.join("acpc.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("ACPC_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,acp_bridge=info,acp_client=info,acpc=info"));
    // The TUI tries to install its own subscriber afterwards; ours wins and
    // its attempt is ignored (it uses `try_init`).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
    Ok(())
}

fn client_options(terminal_auth: bool) -> ClientOptions {
    ClientOptions {
        name: "acpc".to_string(),
        title: Some("acpc".to_string()),
        version: ACPC_VERSION.to_string(),
        terminal_auth,
    }
}

async fn run(cli: Cli, config: Config, home: Home, env_restore: EnvRestore) -> anyhow::Result<i32> {
    let agent = config.select_agent(cli.agent.as_deref())?;
    let cwd = match &cli.cwd {
        Some(dir) => std::fs::canonicalize(dir).with_context(|| format!("--cd {}", dir.display()))?,
        None => std::env::current_dir()?,
    };
    let default_cwd = AbsolutePathBuf::from_absolute_path(&cwd)?;
    let mut agents: Vec<_> = config
        .agents
        .iter()
        .map(|agent| agent.to_profile(&env_restore))
        .collect();
    for profile in &mut agents {
        profile.command.cwd = Some(cwd.clone());
    }
    let interactive = !matches!(cli.command, Some(Command::AppServer));
    let resumable = std::sync::Arc::default();
    let options = BridgeOptions {
        agents,
        default_agent: agent.clone(),
        codex_home: AbsolutePathBuf::from_absolute_path(&home.codex_home)?,
        default_cwd,
        client: client_options(interactive),
        mcp_servers: config.mcp_servers(),
        user_agent: format!("acpc/{} (acpc {ACPC_VERSION})", env!("CARGO_PKG_VERSION")),
        resumable: std::sync::Arc::clone(&resumable),
        yolo: cli.yolo || config.yolo,
    };
    let shutdown = CancellationToken::new();

    if !interactive {
        acp_bridge::serve(options, BridgeTransport::Stdio, None, shutdown).await?;
        return Ok(0);
    }

    let login_only = matches!(cli.command, Some(Command::Login));
    // Resuming and signing in must not create a throwaway session on the agent.
    let prepare_session = !matches!(cli.command, Some(Command::Resume { .. } | Command::Login));
    let initial = connect_initial_agent(&options, &agent, login_only, prepare_session).await?;
    if login_only {
        initial.connection.shutdown().await;
        return Ok(0);
    }
    let socket = AbsolutePathBuf::from_absolute_path(home.socket_path())?;
    let bridge = acp_bridge::start(
        options,
        BridgeTransport::UnixSocket(socket.clone()),
        Some(initial),
        shutdown.clone(),
    )
    .await
    .context("starting the app-server bridge")?;

    let mut tui_cli = codex_tui::Cli::try_parse_from(["acpc"])?;
    tui_cli.prompt = cli.prompt.clone();
    // Sent with thread/start and used as the resume picker's cwd filter.
    tui_cli.cwd = Some(cwd.clone());
    if let Some(Command::Resume { session_id, last }) = &cli.command {
        match session_id {
            Some(id) => tui_cli.resume_session_id = Some(id.clone()),
            None if *last => tui_cli.resume_last = true,
            None => tui_cli.resume_picker = true,
        }
    }
    // The TUI builds a local exec environment from this path; acpc never
    // lets it run Codex subcommands (execution belongs to the agent).
    let arg0 = Arg0DispatchPaths {
        codex_self_exe: std::env::current_exe().ok(),
        codex_linux_sandbox_exe: None,
        main_execve_wrapper_exe: None,
    };
    codex_tui::acpc::enable();
    let exit = codex_tui::run_main(
        tui_cli,
        arg0,
        codex_config::LoaderOverrides::default(),
        Some(RemoteAppServerEndpoint::UnixSocket { socket_path: socket }),
    )
    .await;

    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), bridge).await;

    let exit = exit?;
    let code = match &exit.exit_reason {
        ExitReason::Fatal(message) => {
            eprintln!("ERROR: {message}");
            1
        }
        ExitReason::UserRequested | ExitReason::TurnInterrupted => 0,
    };
    // Only a session its agent can list and reopen can be resumed.
    // Name its agent too, or the session would reopen under the default one.
    let resumable = exit.thread_id.and_then(|thread_id| {
        let thread_id = thread_id.to_string();
        let agent = resumable.lock().ok()?.get(&thread_id)?.clone();
        Some((agent, thread_id))
    });
    if let Some((agent, thread_id)) = resumable {
        let color = std::io::stdout().is_terminal();
        let command = format!("acpc resume -a {agent} {thread_id}");
        let command = if color {
            format!("\u{1b}[36m{command}\u{1b}[39m")
        } else {
            command
        };
        println!("To continue this session, run: {command}");
    }
    std::io::stdout().flush()?;
    Ok(code)
}

/// Start the selected agent and its first session before the TUI takes
/// over the terminal, so startup failures and sign-in happen on a plain
/// terminal.
async fn connect_initial_agent(
    options: &BridgeOptions,
    slug: &str,
    force_login: bool,
    prepare_session: bool,
) -> anyhow::Result<ConnectedAgent> {
    let profile = options
        .agents
        .iter()
        .find(|profile| profile.slug == slug)
        .context("selected agent is not configured")?;
    eprintln!("Starting {}…", profile.name);
    let request = acp_client::schema::NewSessionRequest::new(options.default_cwd.as_path().to_path_buf())
        .mcp_servers(options.mcp_servers.clone());
    let connected = auth::connect_with_auth(
        profile,
        &options.client,
        options.default_cwd.as_path(),
        prepare_session.then_some(request),
        force_login,
    )
    .await?;
    Ok(ConnectedAgent {
        slug: slug.to_string(),
        connection: connected.connection,
        events: connected.events,
        prepared_session: connected
            .session
            .map(|session| (options.default_cwd.clone(), session)),
    })
}

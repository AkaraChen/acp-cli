//! `acpc-mock-agent`: a scriptable ACP v1 agent over stdio for testing acpc.
//!
//! Usage: `acpc-mock-agent [--scenario <path>]` (or `$ACPC_MOCK_SCENARIO`).
//! stdout carries ACP only; logs go to stderr. Set `ACPC_MOCK_TRACE=1` to
//! log every JSON-RPC line.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Logs to stderr; stdout is reserved for the protocol.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[acpc-mock-agent] {}", format_args!($($arg)*))
    };
}

mod agent;
mod demo;
mod login;
mod prompt;
mod scenario;
mod server;
mod sessions;
mod step;
mod store;
mod turn;

use crate::scenario::Scenario;
use crate::store::StateDir;

const SCENARIO_ENV: &str = "ACPC_MOCK_SCENARIO";
const TRACE_ENV: &str = "ACPC_MOCK_TRACE";
const USAGE: &str = "\
Usage: acpc-mock-agent [--scenario <path>]

A scriptable ACP v1 agent speaking JSON-RPC on stdin/stdout.

Options:
  --scenario <path>  Scenario JSON (default: $ACPC_MOCK_SCENARIO, else the built-in demo)
  -h, --help         Show this help

Environment:
  ACPC_MOCK_SCENARIO  Scenario JSON path
  ACPC_MOCK_STATE     State directory (auth marker, sessions/); default: in memory,
                      which terminal login refuses
  ACPC_MOCK_TRACE     Log every JSON-RPC line to stderr when set

Arguments listed by a terminal auth method in the scenario run the
interactive login instead of the agent.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            log!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let mut scenario_path = std::env::var_os(SCENARIO_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut rest = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        } else if arg == "--scenario" {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--scenario needs a path"))?;
            scenario_path = Some(PathBuf::from(path));
        } else if let Some(path) = arg.strip_prefix("--scenario=") {
            scenario_path = Some(PathBuf::from(path));
        } else {
            rest.push(arg);
        }
    }

    let scenario = match &scenario_path {
        Some(path) => Scenario::load(path)?,
        None => Scenario::default(),
    };
    let state = StateDir::resolve(&scenario);
    if let Some(method) = login::requested_method(&scenario, &rest) {
        return Ok(login::run(&state, method));
    }
    if let Some(arg) = rest.first() {
        anyhow::bail!("unexpected argument `{arg}` (see --help)");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(scenario, state))?;
    Ok(ExitCode::SUCCESS)
}

async fn serve(scenario: Scenario, state: StateDir) -> anyhow::Result<()> {
    match &state.root() {
        Some(root) => log!("state directory: {}", root.display()),
        None => log!("no state directory; sessions and auth live in memory"),
    }
    let agent = Arc::new(agent::MockAgent::new(scenario, state));
    let mut transport = agent_client_protocol::Stdio::new();
    if std::env::var_os(TRACE_ENV).is_some() {
        transport = transport.with_debug(|line, direction| log!("{direction:?} {line}"));
    }
    server::serve(agent, transport).await?;
    Ok(())
}

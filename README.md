# acpc

A terminal client for [Agent Client Protocol](https://agentclientprotocol.com) agents.

acpc speaks ACP (stable protocol v1) to any agent you configure, and shows
the conversation in the OpenAI Codex terminal UI, reused unmodified apart
from a small patch series.

```
 codex-tui (upstream, pinned + patches/)
     │  Codex app-server protocol (WebSocket over a private unix socket)
 acp-bridge  — emulates a Codex app-server; translates threads/turns/items ⇄ ACP sessions/updates
     │  ACP v1 (JSON-RPC over stdio)
 acp-client  — the official Rust SDK plus ACP client semantics
     │
 your agent process (from config.toml)
```

## Configure

`~/.config/acpc/config.toml` (or `$ACPC_CONFIG`):

```toml
agent = "claude"                 # default agent

[[agents]]
slug = "claude"
name = "Claude"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@latest"]
# env = { SOME_VAR = "value" }
# default_model = "opus"                       # forced at spawn through:
# model_override = { env = "ANTHROPIC_MODEL" } # or { arg = "--model" }

[[agents]]
slug = "gemini"
name = "Gemini"
command = "npx"
args = ["-y", "@google/gemini-cli", "--experimental-acp"]

# MCP servers passed to every session (stdio, or url + transport = "http" | "sse")
# [[mcp_servers]]
# name = "files"
# command = "mcp-files"
```

acpc has no built-in agents: anything that speaks ACP over stdio works.

## Use

```sh
acpc                     # new session with the default agent
acpc -a gemini "prompt"  # pick an agent, optional first prompt
acpc -C ~/src/project    # working directory for the session
acpc resume [ID|--last]  # reopen a session (agents with session/list + load/resume)
acpc login               # run the agent's ACP sign-in method
acpc app-server          # Codex app-server protocol on stdio, backed by the agent
```

Inside the TUI:

- `/agent` switches agents (starts a new session).
- `/model` picks the agent's model and reasoning level (ACP config options).
- `/options` shows the agent's other session options and modes.
- Commands the agent advertises appear in the `/` popup.

### Authentication

If the agent answers `auth_required`, acpc runs one of the agent's advertised
ACP auth methods before the TUI starts: `agent` methods call `authenticate`;
`terminal` methods run the agent's own command with the method's arguments in
your terminal. Forms the agent asks for while signing in are answered on the
terminal, too. Switching with `/agent` to an agent that needs sign-in shows
the same choice inside the TUI. When a terminal method sets environment
variables that change how programs run (`PATH`, `LD_*`, `NODE_OPTIONS`,
proxies, …), acpc shows them and asks first. acpc never handles credentials
itself; `acpc login` signs in without opening a session.

## Scope

acpc implements the client side of ACP v1 and nothing beyond it: no local
transcripts, no sandbox, no shell execution, no telemetry. It does not
advertise the optional `fs/*` and `terminal/*` client capabilities, so agents
use their own file system and shell.

### Limitations

- `acpc app-server` stops when stdin closes; requests still in flight are
  dropped.
- A session opened before its agent restarted (crash or terminal sign-in)
  must be reopened with `/resume`.

## Build

Requires Rust (the toolchain is pinned in `rust-toolchain.toml`), `git`, and
the system libraries Codex needs (OpenSSL, a C compiler).

```sh
scripts/sync-upstream            # fetch Codex at UPSTREAM, apply patches/
just build                       # target/debug/acpc
just test                        # unit, bridge integration and contract tests
just smoke                       # drive the real TUI in tmux against the mock agent
```

The first build compiles the Codex workspace and takes a while.

Tests:

- `acp-bridge` integration tests run the bridge in-process against
  `acpc-mock-agent` (a scriptable ACP agent). Every message the bridge emits
  is also decoded with the Codex protocol types the TUI uses, so protocol
  drift fails a test instead of being silently dropped by the TUI.
- `scripts/smoke-tui` starts `acpc` in tmux with two mock agents and checks a
  conversation, a tool call, `/options`, `/agent` and exit.

## Upstream and patches

- `UPSTREAM` — the openai/codex commit acpc builds against.
- `patches/*.patch` — the acpc patch series (`git format-patch` files):
  1. add acpc's crates to the Codex workspace;
  2. `acpc/*` protocol extension (agent commands, session options and modes,
     notices, agent list, agent selection, sign-in, terminal handoff) and an
     acpc mode switch in the TUI, off unless acpc enables it;
  3. agent slash commands in the composer; Codex-only built-ins hidden;
  4. `/agent`, `/options`, Shift+Tab mode cycling, sign-in picker and
     terminal handoff;
  5. approval prompts that show the agent's own permission options;
  6. number and multi-select fields in elicitation forms;
  7. regenerated protocol schema fixtures (drop and regenerate on bumps).
- `third_party/codex/` — generated checkout; never edit without exporting.

To change upstream code: edit in `third_party/codex`, commit there, then
`scripts/export-patches`. To move to a newer Codex: update `UPSTREAM`, run
`scripts/sync-upstream`, resolve conflicts (`git am --continue` in
`third_party/codex`), export, rebuild, delete `Cargo.lock` if needed and run
`just lock` to refresh the committed lockfile.

## Layout

| Path | What |
|---|---|
| `crates/acp-client` | ACP client: process spawn, initialize, sessions, permissions, elicitation, cancellation |
| `crates/acp-bridge` | Codex app-server emulator: thread/turn/item state, history rebuild, approvals |
| `crates/acpc` | The binary: config, auth, TUI launch, `app-server` subcommand |
| `crates/mock-agent` | Scriptable ACP agent for tests |

## License

MIT for acpc's own code. The Codex checkout and patches are Apache-2.0
(see `third_party/codex/LICENSE` and `NOTICE`).

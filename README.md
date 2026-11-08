# acpc

**One terminal UI for every coding agent.**

acpc puts any [Agent Client Protocol](https://agentclientprotocol.com) agent
(Claude, Gemini, OpenCode, Goose, or your own) inside the OpenAI Codex
terminal UI. You get streaming answers, tool calls with diffs, approvals,
plans, session history and a slash-command popup, all on top of an agent you
choose. Switch agents mid-conversation and take the conversation with you.

```sh
acpc add "opencode acp"
acpc add --name Claude npx -y @agentclientprotocol/claude-agent-acp@latest
acpc
```

## Why acpc

- **Bring your own agent.** acpc ships no agents and favours none. Anything
  that speaks ACP over stdio works, from a one-line `acpc add`.
- **A real terminal UI.** acpc reuses the Codex TUI at a pinned upstream
  commit, plus a small patch series. Rendering, diffs, approvals and history
  all work like in Codex.
- **Switch without starting over.** `/agent` can fork your conversation onto
  another agent. The conversation stays on screen, the new agent gets it as
  context, and the original session stays one `/resume` away.
- **The agent's own controls.** Its models and reasoning levels appear in
  `/model`, its modes on Shift+Tab, its other settings in `/options`, and its
  slash commands in the `/` popup. Approval prompts offer exactly the options
  the agent sent.
- **Faithful ACP v1.** acpc implements the full client side of ACP v1:
  - sessions: new, load, resume, list, close, delete
  - permissions and elicitation, as forms or links
  - config options and modes
  - available commands
  - sign-in through agent or terminal methods
  - cancellation, including of pending requests
- **Nothing extra.** No telemetry, no local transcripts, no sandbox, no shell
  of its own. Agents use their own tools, and acpc never touches your
  credentials.

## Quick start

You need Rust (the toolchain installs itself from `rust-toolchain.toml`),
`git`, a C compiler, OpenSSL headers and [just](https://github.com/casey/just).

```sh
git clone https://github.com/AkaraChen/acp-cli && cd acp-cli
scripts/sync-upstream          # fetch Codex at the pinned commit, apply patches/
just build                     # → target/debug/acpc
```

Put `target/debug/acpc` on your `PATH`, add an agent, and start:

```sh
acpc add "opencode acp"
acpc
```

The first build compiles the Codex workspace and takes a while. Install
[mold](https://github.com/rui314/mold) and `just` links with it
automatically, so later rebuilds relink in seconds.

## Adding agents

`acpc add` takes the command that starts an agent in ACP mode, as one quoted
string or as plain words:

```sh
acpc add "opencode acp"
acpc add npx -y @agentclientprotocol/claude-agent-acp@latest   # slug: claude-agent-acp
acpc add --slug gemini --name Gemini --default npx -y @google/gemini-cli --experimental-acp
```

- **Slug:** taken from the program name, or for launchers (`npx`, `uvx`,
  `pnpm dlx`, …) from the package name. Pass `--slug` to choose your own.
- **Options:** `--slug`, `--name` and `--default` go before the command.
- **Default agent:** the first agent you add becomes the default. Later ones
  don't replace it unless you pass `--default`.
- **The file:** `acpc add` appends to `~/.config/acpc/config.toml` (or
  `$ACPC_CONFIG`) and keeps your comments.

Everything `add` writes can also be edited by hand:

```toml
agent = "claude"                         # default agent

[[agents]]
slug = "claude"
name = "Claude"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp@latest"]
# env = { SOME_VAR = "value" }
# default_model = "opus"                       # forced at spawn through:
# model_override = { env = "ANTHROPIC_MODEL" } # or { arg = "--model" }

# MCP servers passed to every session
# [[mcp_servers]]
# name = "files"
# command = "mcp-files"                  # stdio; a bare name is looked up on PATH
# url = "https://…"; transport = "http"  # or "sse"
```

## Using it

```sh
acpc                     # new session with the default agent
acpc -a gemini "prompt"  # pick an agent, optionally with a first prompt
acpc -C ~/src/project    # working directory for the session
acpc resume [ID|--last]  # reopen a session (agents with session/list + load/resume)
acpc login               # sign in with the agent's ACP method, then exit
acpc app-server          # headless: the Codex app-server protocol on stdio
```

Inside the TUI:

| | |
|---|---|
| `/agent` | Switch agents. If the session has messages, choose **Bring this conversation** to fork it onto the new agent, or **Start fresh**. |
| `/model` | The agent's models and reasoning levels. |
| `/options` | The agent's other session settings and modes. |
| Shift+Tab | Cycle the agent's modes. |
| `/resume` | Reopen an earlier session, including forks; they show where they came from. |
| `/` | The agent's own slash commands, alongside acpc's. |

### Signing in

When an agent answers `auth_required`, acpc offers the sign-in methods the
agent advertises. That can happen at startup, when you switch to it, or on
your first prompt.

- **`agent` methods** sign in through ACP `authenticate`.
- **`terminal` methods** hand your terminal to the agent's own login program,
  then restart the agent.
- **Forms** the agent asks for while signing in are answered right there.
- **Risky environment overrides** (`PATH`, `LD_*`, `NODE_OPTIONS`, proxies, …)
  are shown and confirmed before they run.

`acpc login` signs in without opening a session.

### Headless: `acpc app-server`

`acpc app-server` speaks the Codex app-server protocol on stdin/stdout, backed
by your ACP agent. Tools built to drive Codex can drive any ACP agent through
it.

## How it works

```
 Codex TUI (upstream, pinned + patches/)
     │  Codex app-server protocol (WebSocket over a private unix socket)
 acp-bridge   emulates a Codex app-server: threads/turns/items ⇄ ACP sessions/updates
     │  ACP v1 (JSON-RPC over stdio)
 acp-client   the official Rust SDK plus ACP client semantics
     │
 your agent process (from config.toml)
```

The TUI already talks to its backend through the app-server protocol.
acpc gives it a backend that translates that protocol into ACP:
- **Threads and turns:** a thread is an agent session, and a turn is a prompt.
- **Session updates** become items: messages, reasoning, plans, commands,
  diffs, tool calls.
- **Approvals and forms:** agent permission requests become approval prompts,
  and elicitations become forms.
- **History:** reopening a session rebuilds it from the agent's `session/load`
  replay.

The TUI changes stay small and switched off unless acpc enables them:
`patches/` is about 130 KB in total.

## Scope

acpc is a client for ACP v1 and nothing more:

- It doesn't advertise the optional `fs/*` and `terminal/*` client
  capabilities, so agents use their own file system and shell.
- After an agent restarts (a crash, or a terminal sign-in), reopen its earlier
  sessions with `/resume`.
- Codex's own features that only make sense with Codex are hidden: cloud,
  sandboxing, review, worktrees and the like.

## Development

```sh
just check    # type-check acpc's crates
just test     # unit, bridge integration and protocol-contract tests
just smoke    # drive the real TUI in tmux against two mock agents
just clippy
```

- **Integration tests:** they run the bridge in-process against
  `acpc-mock-agent`, a scriptable ACP agent. Every message the bridge sends is
  also decoded with the Codex protocol types the TUI uses. Protocol drift
  fails a test instead of being silently dropped by the TUI.
- **Smoke test:** `scripts/smoke-tui` checks a conversation, a tool call,
  `/options`, `/agent` and a clean exit.

### Upstream and patches

`UPSTREAM` pins the openai/codex commit acpc builds against.
`third_party/codex/` is a generated checkout of it with the patch series
applied. `patches/*.patch` are the patches, in order:

1. Add acpc's crates to the Codex workspace, and drop its V8-based members so
   the build never fetches V8.
2. The `acpc/*` protocol extension (agent commands, session options and modes,
   notices, agents, sign-in, terminal handoff), and acpc mode in the TUI.
3. Agent slash commands in the composer; Codex-only built-ins hidden.
4. `/agent` (including forking a conversation onto another agent), `/options`,
   mode cycling, the sign-in picker and the terminal handoff.
5. Approval prompts with the agent's own permission options.
6. Number and multi-select fields in elicitation forms.
7. Upstream schema fixture tests skipped, since the fixtures don't include
   `acpc/*`.

To change upstream code:
1. Edit and commit in `third_party/codex`.
2. Run `scripts/export-patches`.

To move to a newer Codex:
1. Update `UPSTREAM`.
2. Run `scripts/sync-upstream`.
3. Resolve any conflicts (`git am --continue` in `third_party/codex`).
4. Export the patches, rebuild, and run `just lock`.

| Path | What |
|---|---|
| `crates/acp-client` | ACP client: process spawn, initialize, sessions, permissions, elicitation, cancellation |
| `crates/acp-bridge` | The app-server emulator: thread/turn/item state, history rebuild, approvals, forks |
| `crates/acpc` | The binary: config, `add`, sign-in, TUI launch, `app-server` |
| `crates/mock-agent` | Scriptable ACP agent for tests |

## License

MIT for acpc's own code. The Codex checkout and the patches to it are
Apache-2.0 (see `third_party/codex/LICENSE` and `NOTICE`). acpc is not
affiliated with OpenAI.

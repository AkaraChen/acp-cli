# acpc developer recipes. Cargo runs inside the patched upstream workspace.

root := justfile_directory()
codex_rs := root / "third_party/codex/codex-rs"
export CARGO_TARGET_DIR := root / "target"

# Materialize third_party/codex at UPSTREAM and apply patches/.
sync:
    {{root}}/scripts/sync-upstream

# Regenerate patches/ from commits made in third_party/codex.
export-patches:
    {{root}}/scripts/export-patches

build *args:
    cd {{codex_rs}} && cargo build -p acpc -p acpc-mock-agent {{args}}

check *args:
    cd {{codex_rs}} && cargo check -p acp-client -p acp-bridge -p acpc -p acpc-mock-agent {{args}}

# Bridge integration tests drive the mock agent binary, so build it first.
test *args:
    cd {{codex_rs}} && cargo build -p acpc-mock-agent && cargo test -p acp-client -p acp-bridge -p acpc -p acpc-mock-agent {{args}}

# Drive the real TUI in tmux against the mock agent.
smoke: build
    {{root}}/scripts/smoke-tui

clippy *args:
    cd {{codex_rs}} && cargo clippy -p acp-client -p acp-bridge -p acpc -p acpc-mock-agent --all-targets {{args}}

fmt:
    cd {{codex_rs}} && cargo fmt -p acp-client -p acp-bridge -p acpc -p acpc-mock-agent

# Copy the workspace lockfile back after dependency changes.
lock:
    cp {{codex_rs}}/Cargo.lock {{root}}/Cargo.lock

run *args:
    {{root}}/target/debug/acpc {{args}}

#!/usr/bin/env bash
# agentpane.tmux — TPM entry point
#
# Install:
#   1. Add to .tmux.conf:  set -g @plugin 'neevek/agentpane'
#   2. Press prefix + I to install
#   3. Requires: cargo (Rust toolchain)
#
# Options (set before TPM init):
#   @agentpane-key   "a"  — prefix + key to toggle sidebar

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$CURRENT_DIR/target/release/agentpane"

# Build if binary doesn't exist or is older than source
if [ ! -x "$BINARY" ] || [ "$CURRENT_DIR/src/main.rs" -nt "$BINARY" ]; then
  (cd "$CURRENT_DIR" && cargo build --release 2>/tmp/agentpane-build.log) &
fi

# Read user options
TOGGLE_KEY=$(tmux show-option -gqv "@agentpane-key" 2>/dev/null)
TOGGLE_KEY="${TOGGLE_KEY:-a}"

# Bind toggle key
tmux bind-key "$TOGGLE_KEY" run-shell "'$BINARY' toggle"

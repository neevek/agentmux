#!/usr/bin/env bash
# agentpane.tmux — TPM entry point
#
# Install:
#   1. Add to .tmux.conf:  set -g @plugin 'neevek/agentpane'
#   2. Press prefix + I to install
#
# Options (set before TPM init):
#   @agentpane-key   "a"  — prefix + key to toggle sidebar

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$CURRENT_DIR/bin/agentpane"
REPO="neevek/agentpane"

get_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64) echo "aarch64-apple-darwin" ;;
        *)     echo "x86_64-apple-darwin" ;;
      esac ;;
    Linux)  echo "x86_64-unknown-linux-musl" ;;
    MINGW*|MSYS*|CYGWIN*) echo "x86_64-pc-windows-msvc" ;;
    *)      echo "" ;;
  esac
}

download_binary() {
  local target="$1"
  local tag ext url tmp

  tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  [ -z "$tag" ] && return 1

  case "$target" in
    *windows*) ext="zip" ;;
    *)         ext="tar.gz" ;;
  esac

  url="https://github.com/$REPO/releases/download/$tag/agentpane-${target}.${ext}"
  tmp="$(mktemp -d)"

  if curl -fsSL "$url" -o "$tmp/archive.$ext" 2>/dev/null; then
    mkdir -p "$CURRENT_DIR/bin"
    case "$ext" in
      tar.gz) tar xzf "$tmp/archive.$ext" -C "$CURRENT_DIR/bin" ;;
      zip)    unzip -o "$tmp/archive.$ext" -d "$CURRENT_DIR/bin" >/dev/null ;;
    esac
    rm -rf "$tmp"
    chmod +x "$BINARY" 2>/dev/null
    return 0
  fi
  rm -rf "$tmp"
  return 1
}

# Get binary: prefer existing, then download prebuilt, then build from source
if [ ! -x "$BINARY" ]; then
  target=$(get_target)
  if [ -n "$target" ]; then
    download_binary "$target" 2>/tmp/agentpane-download.log
  fi

  # Fallback: build from source if download failed and cargo is available
  if [ ! -x "$BINARY" ] && command -v cargo >/dev/null 2>&1; then
    (cd "$CURRENT_DIR" && cargo build --release 2>/tmp/agentpane-build.log \
      && mkdir -p bin && cp target/release/agentpane bin/) &
  fi
fi

# Read user options
TOGGLE_KEY=$(tmux show-option -gqv "@agentpane-key" 2>/dev/null)
TOGGLE_KEY="${TOGGLE_KEY:-a}"

# Bind toggle key
tmux bind-key "$TOGGLE_KEY" run-shell "'$BINARY' toggle"

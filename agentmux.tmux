#!/usr/bin/env bash
# agentmux.tmux — TPM entry point
#
# Install:
#   1. Add to .tmux.conf:  set -g @plugin 'neevek/agentmux'
#   2. Press prefix + I to install
#
# Options (set before TPM init):
#   @agentmux-key   "a"  — prefix + key to toggle sidebar

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="neevek/agentmux"

# Determine install directory: prefer ~/.local/bin, fallback to ~/bin
get_bin_dir() {
  if [ -d "$HOME/.local/bin" ]; then
    echo "$HOME/.local/bin"
  else
    echo "$HOME/bin"
  fi
}

BIN_DIR="$(get_bin_dir)"
BINARY="$BIN_DIR/agentmux"

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
    Linux)
      case "$arch" in
        aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
        *)             echo "x86_64-unknown-linux-musl" ;;
      esac ;;
    *)      echo "" ;;
  esac
}

# Ensure bin dir is in PATH via shell profile
ensure_path() {
  local bin_dir="$1"
  # Already in PATH — nothing to do
  case ":$PATH:" in
    *":$bin_dir:"*) return 0 ;;
  esac

  local export_line="export PATH=\"$bin_dir:\$PATH\""
  local profile=""

  # Pick the right shell profile
  if [ -n "$ZSH_VERSION" ] || [ "$(basename "$SHELL")" = "zsh" ]; then
    profile="$HOME/.zshrc"
  elif [ -f "$HOME/.bashrc" ]; then
    profile="$HOME/.bashrc"
  elif [ -f "$HOME/.bash_profile" ]; then
    profile="$HOME/.bash_profile"
  else
    profile="$HOME/.profile"
  fi

  # Don't add if already present in profile
  if [ -f "$profile" ] && grep -qF "$bin_dir" "$profile" 2>/dev/null; then
    return 0
  fi

  echo "" >> "$profile"
  echo "# Added by agentmux" >> "$profile"
  echo "$export_line" >> "$profile"

  # Also export for current session
  export PATH="$bin_dir:$PATH"
}

get_latest_tag() {
  curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
}

download_binary() {
  local target="$1" tag="$2"
  local url tmp

  url="https://github.com/$REPO/releases/download/$tag/agentmux-${target}.tar.gz"
  tmp="$(mktemp -d)"

  if curl -fsSL "$url" -o "$tmp/archive.tar.gz" 2>/dev/null; then
    mkdir -p "$BIN_DIR"
    tar xzf "$tmp/archive.tar.gz" -C "$BIN_DIR"
    rm -rf "$tmp"
    chmod +x "$BINARY" 2>/dev/null
    return 0
  fi
  rm -rf "$tmp"
  return 1
}

# Fetch latest tag once; used for both version check and download URL
LATEST_TAG="$(get_latest_tag)"

# Install if binary is missing or the installed version differs from latest
if [ ! -x "$BINARY" ] || {
     [ -n "$LATEST_TAG" ] \
     && [ "${LATEST_TAG#v}" != "$("$BINARY" --version 2>/dev/null | awk '{print $2}')" ]
   }; then
  target=$(get_target)
  if [ -n "$target" ] && [ -n "$LATEST_TAG" ]; then
    download_binary "$target" "$LATEST_TAG" 2>/tmp/agentmux-download.log
  fi

  # Fallback: build from source if download failed and cargo is available
  if [ ! -x "$BINARY" ] && command -v cargo >/dev/null 2>&1; then
    (cd "$CURRENT_DIR" && cargo build --release 2>/tmp/agentmux-build.log \
      && mkdir -p "$BIN_DIR" && cp target/release/agentmux "$BIN_DIR/") &
  fi
fi

# Ensure the bin directory is in PATH
ensure_path "$BIN_DIR"

# Read user options
TOGGLE_KEY=$(tmux show-option -gqv "@agentmux-key" 2>/dev/null)
TOGGLE_KEY="${TOGGLE_KEY:-a}"

# Bind toggle key
tmux bind-key "$TOGGLE_KEY" run-shell "'$BINARY' toggle"

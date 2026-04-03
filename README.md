# agentmux

A tmux sidebar that monitors all your coding agent sessions — Claude Code, Codex, and more — from a single, always-visible pane.

## Features

- **Agent Detection** — Automatically discovers coding agents (Claude Code, Codex) running in any tmux pane via process tree scanning
- **Live Status** — Shows WORKING/IDLE state for each agent, updated every 3 seconds
- **Token Usage** — Displays cumulative input/output tokens per session (↑ input ↓ output)
- **Context Window** — Shows remaining context percentage, turns yellow when running low
- **Last Activity** — Previews the most recent tool call (Edit, Bash, Grep, etc.)
- **Elapsed Time** — How long each agent has been running
- **Window Indicator** — Shows which tmux window each agent lives in
- **Notification Badge** — Yellow `!` marks agents that finished while you were in another window
- **Keyboard Navigation** — j/k or arrow keys to select, Enter to jump to that agent's pane
- **Mouse Support** — Click to select and switch to an agent
- **Persistent Per-Window** — Each window gets its own sidebar, lazily created on first visit
- **No Squash** — Sidebar only takes space from the adjacent pane, not all panes
- **Selection Sync** — Selected item stays consistent across all windows

## Install

### With TPM (recommended)

Add to your `~/.tmux.conf`:

```tmux
set -g @plugin 'neevek/agentmux'
```

Press `prefix + I` to install. The plugin automatically downloads a prebuilt binary for your platform. If no binary is available, it falls back to building from source (requires Rust toolchain).

### Manual

```bash
git clone https://github.com/neevek/agentmux ~/.tmux/plugins/agentmux
cd ~/.tmux/plugins/agentmux
cargo build --release
mkdir -p bin && cp target/release/agentmux bin/
```

Add to `~/.tmux.conf`:

```tmux
run-shell ~/.tmux/plugins/agentmux/agentmux.tmux
```

## Usage

| Action | Key |
|--------|-----|
| Toggle sidebar | `prefix + a` |
| Move selection up | `k` or `↑` |
| Move selection down | `j` or `↓` |
| Jump to agent's pane | `Enter` or click |
| Quit sidebar | `q` |

Or use the CLI directly:

```bash
agentmux toggle   # Toggle sidebar
agentmux open     # Open if not already open
agentmux close    # Close all sidebars
```

## Configuration

Set these in `~/.tmux.conf` before the plugin line:

```tmux
# Change toggle keybinding (default: a)
set -g @agentmux-key 'a'

# Set sidebar width (default: 60)
set -g @agentmux-width 50
```

## Supported Agents

| Agent |
|-------|
| **Claude Code** |
| **Codex** |

Adding support for new agents is straightforward — implement the process pattern and JSONL/state parser in `src/detect/`.

## Why Rust?

Let's be honest. In the age of AI-assisted development, the lifecycle of any interesting open-source project written in Python or TypeScript now has a predictable epilogue: someone sees it on Hacker News, fires up their favorite coding agent, and by dinner time there's a `{project-name}-rs` repo with a freshly minted Cargo.toml and a README that opens with *"A blazingly fast rewrite of..."*

I decided to skip that step.

agentmux is written in Rust from day one — not because I wanted to be trendy, but because I wanted to be *last*. No one's going to rewrite-it-in-Rust if it's already in Rust. The PR writes itself: "closes #1: rewrite in Rust" — filed and merged before the repo is in public.

And it turns out Rust is genuinely the right tool here:

- **Zero runtime dependencies** — A single static binary. No Node.js, no Python, no Bun. `prefix + I` and you're done.
- **Low resource footprint** — Inactive sidebars cost near-zero CPU. One tmux query per second, no file I/O. Your laptop fan stays quiet.
- **Fast startup** — Millisecond launch. No interpreter warmup, no `npm install`, no waiting for your JIT to warm up while you watch a spinner.
- **Cross-platform builds** — One CI matrix, six targets (macOS ARM/Intel, Linux x86_64/ARM64, Windows x86_64/ARM64). No platform-specific shims or runtime bundles.

So yes — I chose Rust because it's fast, correct, and dependency-free. But mostly because I didn't want to wake up to a PR titled *"agentmux-rs: A blazingly fast rewrite"*. You're welcome.

## Architecture

```
src/
  main.rs              CLI entry point (toggle/open/close/ensure)
  tmux/mod.rs          Tmux command helpers
  detect/
    mod.rs             Agent scan coordinator
    process.rs         Process tree walking (ps → agent detection)
    state.rs           JSONL parsing, token counting, state detection
  sidebar/
    mod.rs             Main event loop (active/inactive polling)
    render.rs          ANSI terminal rendering
    input.rs           Keyboard and mouse input handling
```

**Design principles:**
- One sidebar process per window, each with its own stdin/stdout
- Active window sidebar does full scanning every 3s; inactive sidebars only sync selection state (1 tmux query/sec)
- JSONL token counting is cached by file size — no re-reads unless the file grows
- State detection uses a fast 32KB tail read, not the full file
- Sidebar creation auto-detects window layout to avoid squashing other panes

## License

MIT

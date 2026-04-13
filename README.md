# agentmux

A tmux sidebar that monitors all your coding agent sessions — Claude Code, Codex, and more — from a single, always-visible pane.

## Features

- **Agent Detection** — Discovers Claude Code and Codex processes via process tree scanning
- **Live Status** — WORKING/IDLE state per agent, updated every 3 seconds
- **Token & Cost** — Cumulative tokens with cache breakdown (↑↓), cost estimation per session
- **Usage History** — Header shows aggregated stats for today, 7 days, and all time
- **Context Window** — Remaining context %, turns yellow when low
- **Last Activity** — Most recent tool call preview (Edit, Bash, Grep, etc.)
- **Notifications** — Yellow `!` badge for agents that finished in another window
- **Navigation** — j/k or arrows to select, Enter or click to jump to agent's pane
- **Per-Window Sidebars** — Lazily created per tmux window, no pane squashing
- **Selection Sync** — Selection stays consistent across windows

## Install

### With TPM (recommended)

```tmux
set -g @plugin 'neevek/agentmux'
```

Press `prefix + I`. Downloads a prebuilt binary; falls back to `cargo build` if unavailable.

### Manual

```bash
git clone https://github.com/neevek/agentmux ~/.tmux/plugins/agentmux
cd ~/.tmux/plugins/agentmux
cargo build --release && mkdir -p bin && cp target/release/agentmux bin/
```

Add to `~/.tmux.conf`:

```tmux
run-shell ~/.tmux/plugins/agentmux/agentmux.tmux
```

## Usage

| Action | Key |
|--------|-----|
| Toggle sidebar | `prefix + a` |
| Navigate | `j`/`k` or `↑`/`↓` |
| Jump to agent | `Enter` or click |
| Quit | `q` |

```bash
agentmux toggle / open / close
```

## Configuration

```tmux
set -g @agentmux-key 'a'      # toggle keybinding (default: a)
set -g @agentmux-width 50     # sidebar width in columns (default: 60, min: 50)
```

Runtime settings (e.g. resized width) are persisted to `~/.config/agentmux/config.toml`.

## Supported Agents

| Agent | Tokens | Cost |
|-------|--------|------|
| Claude Code | ✓ with cache breakdown | ✓ |
| Codex | ✓ | ✓ |

Models: Claude Opus/Sonnet/Haiku, OpenAI o3/o4-mini, GPT-5.4/4.1/4o, and more.

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
  main.rs          CLI entry point
  config.rs        TOML config
  tmux/            Tmux command helpers
  detect/          Agent scanning, JSONL parsing, cost estimation, SQLite history
  sidebar/         Event loop, rendering, input, leader/follower coordination
```

- Active sidebar scans every 3s; inactive sidebars sync selection only (1 query/sec)
- JSONL token counts cached by file size/mtime; state detection uses 32KB tail reads
- Codex sessions parsed incrementally; usage history persisted to SQLite

## License

MIT

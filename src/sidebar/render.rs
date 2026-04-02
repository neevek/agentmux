use crate::detect::AgentInfo;
use crate::detect::process::format_elapsed;
use crate::detect::state::{AgentState, format_tokens};
use std::collections::HashSet;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

// Colors
const GREEN: &str = "\x1b[38;2;166;227;161m";
const GRAY: &str = "\x1b[38;2;127;132;156m";
const WHITE: &str = "\x1b[38;2;205;214;244m";
const YELLOW: &str = "\x1b[38;2;249;226;175m";
const BLUE: &str = "\x1b[38;2;137;180;250m"; // blue #89b4fa (input tokens)
const MAUVE: &str = "\x1b[38;2;203;166;247m"; // mauve #cba6f7 (output tokens)

// Backgrounds
const SEL_BG: &str = "\x1b[48;2;49;50;68m";
const HEADER_BG: &str = "\x1b[48;2;30;30;46m";

/// Number of rows per item
pub const ITEM_ROWS: u32 = 5;
/// Number of header rows
pub const HEADER_ROWS: u32 = 3;

pub fn render_sidebar(
    agents: &[AgentInfo],
    width: u32,
    height: u32,
    selected: usize,
    unseen_done: &HashSet<String>,
) -> String {
    let w = width as usize;
    let mut buf = String::new();
    let mut row: u32 = 1;

    buf.push_str("\x1b[?25l");

    // === Header ===
    let title = "Coding Agents";
    let padding = w.saturating_sub(title.len()) / 2;
    emit_line_bg(&mut buf, row, HEADER_BG, "");
    row += 1;
    emit_line_bg(
        &mut buf,
        row,
        HEADER_BG,
        &format!("{}{BOLD}{WHITE}{title}{RESET}", " ".repeat(padding)),
    );
    row += 1;
    emit_line_bg(&mut buf, row, HEADER_BG, "");
    row += 1;

    if agents.is_empty() {
        emit_line_clear(&mut buf, row);
        row += 1;
        emit_line_no_bg(
            &mut buf,
            row,
            "",
            &format!("  {DIM}No agents detected{RESET}"),
        );
        row += 1;
    } else {
        for (i, agent) in agents.iter().enumerate() {
            let is_selected = i == selected;
            let color = agent.kind.color_code();
            let name = agent.kind.display_name();
            let has_badge = unseen_done.contains(&agent.pane_id);

            let (state_color, state_label) = match agent.state {
                AgentState::Working => (GREEN, "WORKING"),
                AgentState::Idle => (GRAY, "IDLE"),
            };

            let elapsed = format_elapsed(agent.elapsed_secs);
            let short_cwd = truncate_path(&agent.cwd, w.saturating_sub(6));

            // Window name
            let win_name = &agent.window_name;

            // Notification badge
            let badge = if has_badge {
                format!(" {YELLOW}{BOLD}!{RESET}")
            } else {
                String::new()
            };

            let in_tok = format_tokens(agent.input_tokens);
            let out_tok = format_tokens(agent.output_tokens);

            let bg = if is_selected { SEL_BG } else { "" };
            let emit = if is_selected {
                emit_line_bg
            } else {
                emit_line_no_bg
            };

            // Build info trail: ↑19.1M ↓93.4k | 51% left
            let sep = format!("{bg} {DIM}|{RESET}{bg} ");
            let mut info_parts: Vec<String> = Vec::new();
            if !in_tok.is_empty() || !out_tok.is_empty() {
                info_parts.push(format!(
                    "{BLUE}↑ {in_tok}{RESET}{bg} {MAUVE}↓ {out_tok}{RESET}"
                ));
            }
            if let Some(pct) = agent.context_pct {
                let left = 100u8.saturating_sub(pct);
                let ctx_color = if left <= 20 { YELLOW } else { GREEN };
                info_parts.push(format!("{ctx_color}{left}% left{RESET}"));
            }
            let info_str = if info_parts.is_empty() {
                String::new()
            } else {
                format!("{bg} {DIM}|{RESET}{bg} {}", info_parts.join(&sep))
            };

            // Top margin
            emit(&mut buf, row, bg, "");
            row += 1;
            // Line 1: name ● STATE elapsed | ↑in ↓out | N% left
            emit(
                &mut buf,
                row,
                bg,
                &format!(
                    "  {color}{BOLD}{name}{RESET}{bg}{badge}{bg} {state_color}● {state_label}{RESET}{bg} {DIM}{elapsed}{RESET}{bg}{info_str}"
                ),
            );
            row += 1;
            // Line 2: [window] cwd
            emit(
                &mut buf,
                row,
                bg,
                &format!("  {DIM}[{win_name}]{RESET}{bg} {DIM}{short_cwd}{RESET}"),
            );
            row += 1;
            // Line 3: last activity
            if let Some(ref activity) = agent.last_activity {
                let short: String = activity.chars().take(w.saturating_sub(5)).collect();
                emit(
                    &mut buf,
                    row,
                    bg,
                    &format!("  {DIM}> {short}{RESET}"),
                );
            } else {
                emit(&mut buf, row, bg, "");
            }
            row += 1;
            // Bottom margin
            emit(&mut buf, row, bg, "");
            row += 1;
        }
    }

    while row <= height {
        emit_line_clear(&mut buf, row);
        row += 1;
    }

    buf
}

fn emit_line_bg(buf: &mut String, row: u32, bg: &str, content: &str) {
    buf.push_str(&format!("\x1b[{row};1H{bg}\x1b[K{content}{RESET}"));
}

fn emit_line_no_bg(buf: &mut String, row: u32, _bg: &str, content: &str) {
    buf.push_str(&format!("\x1b[{row};1H\x1b[K{content}"));
}

fn emit_line_clear(buf: &mut String, row: u32) {
    buf.push_str(&format!("\x1b[{row};1H\x1b[K"));
}

fn truncate_path(path: &str, max_len: usize) -> String {
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };

    if display.len() <= max_len {
        return display;
    }

    if let Some(last_sep) = display.rfind('/') {
        let tail = &display[last_sep..];
        let available = max_len.saturating_sub(tail.len()).saturating_sub(5);
        if available > 0 {
            return format!("~/...{tail}");
        }
    }

    let truncated: String = display.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

use crate::detect::AgentInfo;
use crate::detect::history::{AgentTotals, AggregatedStats};
use crate::detect::process::{AgentKind, format_elapsed};
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
const TEAL: &str = "\x1b[38;2;148;226;213m"; // teal #94e2d5 (context left)
const SUBTEXT: &str = "\x1b[38;2;186;194;222m"; // subtext0 #bac2de (cwd)
const PEACH: &str = "\x1b[38;2;250;179;135m"; // peach #fab387 (Claude)
const ROSEWATER: &str = "\x1b[38;2;245;224;220m"; // rosewater #f5e0dc (cost)
const SAPPHIRE: &str = "\x1b[38;2;116;199;236m"; // sapphire #74c7ec (model)
const FLAMINGO: &str = "\x1b[38;2;242;205;205m"; // flamingo #f2cdcd (msg count)

// Backgrounds
const SEL_BG: &str = "\x1b[48;2;49;50;68m";
const HEADER_BG: &str = "\x1b[48;2;30;30;46m";

/// Number of header rows (title 3 + table 5 = 8)
pub const HEADER_ROWS: u32 = 8;

/// Calculate the row count for a single agent item.
pub fn item_row_count(agent: &AgentInfo) -> u32 {
    // Always: top margin (1) + info (1) + path (1) + bottom margin (1) = 4
    let mut rows = 4u32;
    if has_detail_line(agent) {
        rows += 1;
    }
    rows
}

/// Calculate how many items fit in the visible area (adaptive heights).
pub fn visible_item_count(height: u32, agents: &[AgentInfo], scroll_offset: usize) -> usize {
    let mut available = height.saturating_sub(HEADER_ROWS);
    let mut count = 0;
    for agent in agents.iter().skip(scroll_offset) {
        let h = item_row_count(agent);
        if h > available {
            break;
        }
        available -= h;
        count += 1;
    }
    count
}

fn has_detail_line(agent: &AgentInfo) -> bool {
    agent.model.is_some() || agent.last_activity.is_some()
}

pub fn render_sidebar(
    agents: &[AgentInfo],
    width: u32,
    height: u32,
    selected: usize,
    scroll_offset: usize,
    unseen_done: &HashSet<String>,
    stats: &AggregatedStats,
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
    // Row 3: padding below title
    emit_line_bg(&mut buf, row, HEADER_BG, "");
    row += 1;

    // === Stats table (box-drawing chars: ─ │ ┬ ┼ ┴) ===
    let cell_w = w.saturating_sub(1) / 2; // │ takes 1 column
    let right_cell_w = w.saturating_sub(1).saturating_sub(cell_w);
    let h_left: String = std::iter::repeat_n('─', cell_w).collect();
    let h_right: String = std::iter::repeat_n('─', right_cell_w).collect();

    // Row 4: top border  ─────┬─────
    emit_line_no_bg(&mut buf, row, "", &format!("{DIM}{h_left}┬{h_right}{RESET}"));
    row += 1;

    // Row 5: header — centered "Claude" │ "Codex"
    let (lp, lr) = center_pad("Claude", cell_w);
    let (rp, rr) = center_pad("Codex", right_cell_w);
    emit_line_no_bg(
        &mut buf,
        row,
        "",
        &format!(
            "{}{PEACH}{BOLD}Claude{RESET}{}{DIM}│{RESET}{}{BLUE}{BOLD}Codex{RESET}{}",
            " ".repeat(lp),
            " ".repeat(lr),
            " ".repeat(rp),
            " ".repeat(rr),
        ),
    );
    row += 1;

    // Row 6: middle border  ─────┼─────
    emit_line_no_bg(&mut buf, row, "", &format!("{DIM}{h_left}┼{h_right}{RESET}"));
    row += 1;

    // Row 7: body — centered stats
    let left_body = format_agent_stats_centered(&stats.claude, cell_w);
    let right_body = format_agent_stats_centered(&stats.codex, right_cell_w);
    emit_line_no_bg(
        &mut buf,
        row,
        "",
        &format!("{left_body}{DIM}│{RESET}{right_body}"),
    );
    row += 1;

    // Row 8: bottom border  ─────┴─────
    emit_line_no_bg(&mut buf, row, "", &format!("{DIM}{h_left}┴{h_right}{RESET}"));
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
        let visible = visible_item_count(height, agents, scroll_offset);
        let end = (scroll_offset + visible).min(agents.len());

        for (vi, agent) in agents[scroll_offset..end].iter().enumerate() {
            let i = scroll_offset + vi;
            let is_selected = i == selected;
            let color = match agent.kind {
                AgentKind::ClaudeCode => PEACH,
                AgentKind::Codex => BLUE,
            };
            let name = agent.kind.display_name();
            let has_badge = unseen_done.contains(&agent.pane_id);

            let state_color = match agent.state {
                AgentState::Working => GREEN,
                AgentState::Idle => GRAY,
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

            // Build info trail: ↑19.1M ↓93.4k | $34.4 | 51% left | 22 msgs
            let sep = format!("{bg} {DIM}|{RESET}{bg} ");
            let mut info_parts: Vec<String> = Vec::new();
            // Always show tokens
            info_parts.push(format!(
                "{BLUE}↑ {in_tok}{RESET}{bg} {MAUVE}↓ {out_tok}{RESET}"
            ));
            if agent.cost_usd >= 0.01 {
                info_parts.push(format!("{ROSEWATER}{}{RESET}", format_cost(agent.cost_usd)));
            }
            // Show context left: use value if available, 100% for new sessions
            let left = match agent.context_pct {
                Some(pct) => 100u8.saturating_sub(pct),
                None => 100,
            };
            let ctx_color = if left <= 20 { YELLOW } else { TEAL };
            info_parts.push(format!("{ctx_color}{left}% left{RESET}"));
            if agent.turn_count > 0 {
                let msg_label = if agent.turn_count == 1 { "msg" } else { "msgs" };
                info_parts.push(format!("{FLAMINGO}{} {msg_label}{RESET}", agent.turn_count));
            }

            let info_str = format!("{bg} {DIM}|{RESET}{bg} {}", info_parts.join(&sep));

            // Top margin
            emit(&mut buf, row, bg, "");
            row += 1;
            // Line 1: ● name elapsed | ↑in ↓out | N% left
            emit(
                &mut buf,
                row,
                bg,
                &format!(
                    "  {state_color}● {RESET}{bg} {color}{BOLD}{name}{RESET}{bg}{badge}{bg} {DIM}{elapsed}{RESET}{bg}{info_str}"
                ),
            );
            row += 1;
            // Line 2: [window] cwd
            emit(
                &mut buf,
                row,
                bg,
                &format!("  {GRAY}[{win_name}]{RESET}{bg} {SUBTEXT}{short_cwd}{RESET}"),
            );
            row += 1;
            // Line 3 (optional): model (effort) | > last activity
            if has_detail_line(agent) {
                let model_short = agent
                    .model
                    .as_deref()
                    .map(short_model_name)
                    .unwrap_or_default();
                let mut detail_parts: Vec<String> = Vec::new();
                if !model_short.is_empty() {
                    let model_display = match &agent.effort {
                        Some(effort) => format!("{model_short} ({effort})"),
                        None => model_short,
                    };
                    detail_parts.push(format!("{SAPPHIRE}{model_display}{RESET}"));
                }
                if let Some(ref activity) = agent.last_activity {
                    let prefix = format!("{GREEN}{BOLD}>{RESET}{bg} ");
                    detail_parts.push(format!("{prefix}{DIM}{activity}{RESET}"));
                }
                emit(
                    &mut buf,
                    row,
                    bg,
                    &format!(
                        "  {}",
                        detail_parts.join(&format!("{bg} {DIM}|{RESET}{bg} "))
                    ),
                );
                row += 1;
            }
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

fn short_model_name(model: &str) -> String {
    // Claude: "claude-opus-4-6-20260401" → "opus 4.6"
    for family in &["opus", "sonnet", "haiku"] {
        if let Some(pos) = model.find(family) {
            let after = &model[pos + family.len()..];
            let version_parts: Vec<&str> = after
                .split('-')
                .filter(|s| !s.is_empty() && s.len() < 8 && s.chars().all(|c| c.is_ascii_digit()))
                .collect();
            return if version_parts.is_empty() {
                family.to_string()
            } else {
                format!("{}-{}", family, version_parts.join("."))
            };
        }
    }
    // OpenAI models — check specific variants before broad patterns
    for prefix in &[
        "o4-mini",
        "o3-mini",
        "o3",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-4.1-mini",
        "gpt-4.1-nano",
        "gpt-4.1",
        "gpt-4o-mini",
        "gpt-4o",
    ] {
        if model.contains(prefix) {
            return prefix.to_string();
        }
    }
    model.to_string()
}

/// Compute (left_pad, right_pad) to center `text` in `width` columns.
fn center_pad(text: &str, width: usize) -> (usize, usize) {
    let len = text.chars().count();
    let left = width.saturating_sub(len) / 2;
    let right = width.saturating_sub(len).saturating_sub(left);
    (left, right)
}

fn format_agent_stats_centered(totals: &AgentTotals, cell_w: usize) -> String {
    let input = format_tokens(totals.input_tokens);
    let output = format_tokens(totals.output_tokens);
    let cost = format_cost(totals.cost_usd);
    let msgs = format_compact_count(totals.turns);

    // Plain text for measuring visible width
    let plain = format!("↑ {input} ↓ {output} | {cost} | {msgs} msgs");
    let visible_len = plain.chars().count();

    let pad_left = cell_w.saturating_sub(visible_len) / 2;
    let pad_right = cell_w.saturating_sub(visible_len).saturating_sub(pad_left);

    format!(
        "{}{BLUE}↑ {input}{RESET} {MAUVE}↓ {output}{RESET} {DIM}|{RESET} {ROSEWATER}{cost}{RESET} {DIM}|{RESET} {FLAMINGO}{msgs} msgs{RESET}{}",
        " ".repeat(pad_left),
        " ".repeat(pad_right),
    )
}

fn format_compact_count(n: u32) -> String {
    if n < 1000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn format_cost(cost: f64) -> String {
    if cost >= 100.0 {
        format!("${:.0}", cost)
    } else if cost >= 10.0 {
        format!("${:.1}", cost)
    } else {
        format!("${:.2}", cost)
    }
}

fn truncate_path(path: &str, max_len: usize) -> String {
    use std::sync::OnceLock;
    static HOME: OnceLock<String> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_default()
    });
    let display = if !home.is_empty() && path.starts_with(home.as_str()) {
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

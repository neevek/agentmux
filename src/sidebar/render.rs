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

/// Number of header rows, depends on expanded/collapsed mode.
/// Title: 3 rows. Table header: 2 rows. Collapsed data: 5 rows. Expanded data: 13 rows.
pub fn header_rows(expanded: bool) -> u32 {
    if expanded { 18 } else { 10 }
}

/// Calculate the row count for a single agent item.
pub fn item_row_count(agent: &AgentInfo) -> u32 {
    // Always: top margin (1) + info (1) + path (1) + bottom margin (1) = 4
    let mut rows = 4u32;
    if agent.model.is_some() {
        rows += 1;
    }
    if agent.last_activity.is_some() {
        rows += 1;
    }
    rows
}

/// Calculate how many items fit in the visible area (adaptive heights).
pub fn visible_item_count(height: u32, agents: &[AgentInfo], scroll_offset: usize, expanded: bool) -> usize {
    let mut available = height.saturating_sub(header_rows(expanded));
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

pub fn render_sidebar(
    agents: &[AgentInfo],
    width: u32,
    height: u32,
    selected: usize,
    scroll_offset: usize,
    unseen_done: &HashSet<String>,
    stats: &AggregatedStats,
    expanded: bool,
    header_selected: bool,
) -> String {
    let w = width as usize;
    let mut buf = String::new();
    let mut row: u32 = 1;

    buf.push_str("\x1b[?25l");

    // === Header ===
    let title = "AgentMux";
    let padding = w.saturating_sub(title.len()) / 2;
    emit_line_bg(&mut buf, row, HEADER_BG, "");
    row += 1;
    emit_line_bg(
        &mut buf,
        row,
        HEADER_BG,
        &format!("{}{BOLD}{GREEN}{title}{RESET}", " ".repeat(padding)),
    );
    row += 1;
    emit_line_bg(&mut buf, row, HEADER_BG, "");
    row += 1;

    // === Stats table (5 columns: name │ period │ tokens │ cost │ turns) ===
    let col0 = "Claude".len().max("Codex".len()) + 2; // 8
    let col1 = "Weekly".len() + 2;                     // 8
    // Compute tokens/cost column widths from data (min fits header labels)
    let all_totals = [
        &stats.claude.today, &stats.claude.seven_days, &stats.claude.total,
        &stats.codex.today, &stats.codex.seven_days, &stats.codex.total,
    ];
    let col2 = all_totals.iter()
        .map(|t| {
            format!("↑ {} ↓ {}", format_tokens(t.input_tokens), format_tokens(t.output_tokens))
                .chars().count()
        })
        .max().unwrap_or(13)
        .max("In/Out tokens".len()) + 2;
    let col3 = all_totals.iter()
        .map(|t| format_cost(t.cost_usd).len())
        .max().unwrap_or(4)
        .max("Cost".len()) + 2;
    // col4 (Turns) gets all remaining width
    let col4 = w.saturating_sub(col0 + col1 + col2 + col3 + 4).max("Messages".len() + 2);
    let cw = [col0, col1, col2, col3, col4];

    let table_bg = if header_selected { SEL_BG } else { "" };
    let emit_table = if header_selected { emit_line_bg } else { emit_line_no_bg };

    // Blank prefix width for header rows: cols 0 + │ + col1 = col0 + 1 + col1
    let hdr_blank = cw[0] + 1 + cw[1];

    // Table header: partial top border (cols 2-4 only) + label row
    let hdr_top = format!(
        "{}┌{}┬{}┬{}",
        " ".repeat(hdr_blank),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
    );
    emit_table(&mut buf, row, table_bg, &format!("{DIM}{hdr_top}{RESET}"));
    row += 1;

    // Header labels (centered in each column)
    let bg = table_bg;
    let hdr_tok = centered_in("In/Out tokens", cw[2]);
    let hdr_cost = centered_in("Cost", cw[3]);
    let hdr_turns = centered_in("Messages", cw[4]);
    emit_table(&mut buf, row, table_bg, &format!(
        "{}{DIM}│{RESET}{bg}{WHITE}{hdr_tok}{RESET}{bg}{DIM}│{RESET}{bg}{WHITE}{hdr_cost}{RESET}{bg}{DIM}│{RESET}{bg}{WHITE}{hdr_turns}{RESET}{bg}",
        " ".repeat(hdr_blank),
    ));
    row += 1;

    // Border between header and data: cols 0-1 use ┬ (start), cols 2-4 use ┼ (continue)
    let data_top = format!(
        "{}┬{}┼{}┼{}┼{}",
        "─".repeat(cw[0]),
        "─".repeat(cw[1]),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
    );
    emit_table(&mut buf, row, table_bg, &format!("{DIM}{data_top}{RESET}"));
    row += 1;

    if expanded {
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            &format!("{PEACH}{BOLD}Claude{RESET}"), 6, "Today", &stats.claude.today);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border(&cw, '┼')));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            "", 0, "Weekly", &stats.claude.seven_days);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border_partial(&cw)));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            "", 0, "Total", &stats.claude.total);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border(&cw, '┼')));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            &format!("{BLUE}{BOLD}Codex{RESET}"), 5, "Today", &stats.codex.today);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border(&cw, '┼')));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            "", 0, "Weekly", &stats.codex.seven_days);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border_partial(&cw)));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            "", 0, "Total", &stats.codex.total);
    } else {
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            &format!("{PEACH}{BOLD}Claude{RESET}"), 6, "Today", &stats.claude.today);
        emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border(&cw, '┼')));
        row += 1;
        row = emit_stats_row(&mut buf, row, table_bg, emit_table, &cw,
            &format!("{BLUE}{BOLD}Codex{RESET}"), 5, "Today", &stats.codex.today);
    }

    // Bottom border with all junctions
    emit_table(&mut buf, row, table_bg, &format!("{DIM}{}{RESET}", table_border(&cw, '┴')));
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
        let visible = visible_item_count(height, agents, scroll_offset, expanded);
        let end = (scroll_offset + visible).min(agents.len());

        for (vi, agent) in agents[scroll_offset..end].iter().enumerate() {
            let i = scroll_offset + vi;
            let is_selected = !header_selected && i == selected;
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
            let win_name = &agent.window_name;

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

            // Build info trail: ↑2.8M ↓25.6k | $2.54
            let sep = format!("{bg} {DIM}|{RESET}{bg} ");
            let mut info_parts: Vec<String> = Vec::new();
            info_parts.push(format!(
                "{BLUE}↑ {in_tok}{RESET}{bg} {MAUVE}↓ {out_tok}{RESET}"
            ));
            if agent.cost_usd >= 0.01 {
                info_parts.push(format!("{ROSEWATER}{}{RESET}", format_cost(agent.cost_usd)));
            }
            let info_str = format!("{bg} {DIM}|{RESET}{bg} {}", info_parts.join(&sep));

            // Top margin
            emit(&mut buf, row, bg, "");
            row += 1;

            // Line 1: ● name elapsed | ↑in ↓out | $cost | N% left | N msgs
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

            // Line 3 (optional): model (effort) | N% left | N msgs
            if let Some(ref model) = agent.model {
                let model_short = short_model_name(model);
                let model_display = match &agent.effort {
                    Some(effort) => format!("{model_short} ({effort})"),
                    None => model_short,
                };
                let left = match agent.context_pct {
                    Some(pct) => 100u8.saturating_sub(pct),
                    None => 100,
                };
                let ctx_color = if left <= 20 { YELLOW } else { TEAL };
                let msg_label = if agent.turn_count == 1 { "msg" } else { "msgs" };
                emit(
                    &mut buf,
                    row,
                    bg,
                    &format!(
                        "  {SAPPHIRE}{model_display}{RESET}{bg}{sep}{ctx_color}{left}% left{RESET}{bg}{sep}{FLAMINGO}{} {msg_label}{RESET}",
                        agent.turn_count,
                    ),
                );
                row += 1;
            }

            // Line 4 (optional): > last activity
            if let Some(ref activity) = agent.last_activity {
                emit(
                    &mut buf,
                    row,
                    bg,
                    &format!("  {GREEN}{BOLD}>{RESET}{bg} {DIM}{activity}{RESET}"),
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

fn emit_stats_row(
    buf: &mut String,
    row: u32,
    bg: &str,
    emit: fn(&mut String, u32, &str, &str),
    cw: &[usize; 5],
    name_colored: &str,
    name_plain_len: usize,
    period: &str,
    totals: &AgentTotals,
) -> u32 {
    let cost_str = format_cost(totals.cost_usd);
    let turns_str = format_compact_count(totals.turns);
    let tok_str = format!(
        "↑ {} ↓ {}",
        format_tokens(totals.input_tokens),
        format_tokens(totals.output_tokens)
    );

    let s = format!("{DIM}│{RESET}{bg}"); // reusable separator

    // Col 0: name (left-aligned, 1 char padding)
    let name_cell = if name_plain_len > 0 {
        let pad = cw[0].saturating_sub(name_plain_len + 1);
        format!(" {name_colored}{RESET}{bg}{}", " ".repeat(pad))
    } else {
        format!("{bg}{}", " ".repeat(cw[0]))
    };

    // Col 1: period (left-aligned, 1 char padding)
    let period_pad = cw[1].saturating_sub(period.len() + 1);
    let period_cell = format!(" {DIM}{period}{RESET}{bg}{}", " ".repeat(period_pad));

    // Col 2: tokens (left-aligned, 1 char padding)
    let tok_plain_len = tok_str.chars().count();
    let tok_pad = cw[2].saturating_sub(tok_plain_len + 1);
    let tok_cell = format!(
        " {BLUE}↑ {}{RESET}{bg} {MAUVE}↓ {}{RESET}{bg}{}",
        format_tokens(totals.input_tokens),
        format_tokens(totals.output_tokens),
        " ".repeat(tok_pad),
    );

    // Col 3: cost (left-aligned, 1 char padding)
    let cost_pad = cw[3].saturating_sub(cost_str.len() + 1);
    let cost_cell = format!(" {ROSEWATER}{cost_str}{RESET}{bg}{}", " ".repeat(cost_pad));

    // Col 4: turns (left-aligned, 1 char padding)
    let turns_pad = cw[4].saturating_sub(turns_str.len() + 1);
    let turns_cell = format!(" {FLAMINGO}{turns_str}{RESET}{bg}{}", " ".repeat(turns_pad));

    emit(
        buf, row, bg,
        &format!("{name_cell}{s}{period_cell}{s}{tok_cell}{s}{cost_cell}{s}{turns_cell}"),
    );
    row + 1
}

fn table_border_partial(cw: &[usize; 5]) -> String {
    let blank: String = " ".repeat(cw[0]);
    format!(
        "{blank}├{}┼{}┼{}┼{}",
        "─".repeat(cw[1]),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
    )
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

fn table_border(col_widths: &[usize], junction: char) -> String {
    col_widths
        .iter()
        .map(|&w| {
            let s: String = std::iter::repeat_n('─', w).collect();
            s
        })
        .collect::<Vec<_>>()
        .join(&junction.to_string())
}

fn centered_in(text: &str, cell_width: usize) -> String {
    let pad = cell_width.saturating_sub(text.len());
    let left = pad / 2;
    let right = pad - left;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
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

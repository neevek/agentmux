use crate::detect::AgentInfo;
use crate::detect::history::{AgentTotals, AggregatedStats};
use crate::detect::process::{AgentKind, format_elapsed};
use crate::detect::state::{AgentState, format_tokens};
use std::collections::HashSet;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

// Colors
const GRAY: &str = "\x1b[38;2;127;132;156m";
const WHITE: &str = "\x1b[38;2;205;214;244m";
const YELLOW: &str = "\x1b[38;2;249;226;175m";
const BLUE: &str = "\x1b[38;2;137;180;250m"; // blue #89b4fa (input tokens)
const MAUVE: &str = "\x1b[38;2;203;166;247m"; // mauve #cba6f7 (output tokens)
const SUBTEXT: &str = "\x1b[38;2;186;194;222m"; // subtext0 #bac2de (cwd)
const PEACH: &str = "\x1b[38;2;250;179;135m"; // peach #fab387 (Claude)
const ROSEWATER: &str = "\x1b[38;2;245;224;220m"; // rosewater #f5e0dc (cost)
const FLAMINGO: &str = "\x1b[38;2;242;205;205m"; // flamingo #f2cdcd (msg count)

// Backgrounds
const SEL_BG: &str = "\x1b[48;2;49;50;68m";
const HEADER_BG: &str = "\x1b[48;2;30;30;46m";

/// Number of rendered rows occupied by the header before the first item begins.
pub fn header_rows(expanded: bool) -> u32 {
    if expanded { 16 } else { 8 }
}

pub fn item_row_count_opts(agent: &AgentInfo, compact: bool, separator: bool) -> u32 {
    // Normal: top margin (1) + summary (1) + path (1) + bottom margin (1)
    // Compact: no top/bottom margin; 1 row top + 1 row bottom replaced by separator
    let mut rows = if compact { 0u32 } else { 2u32 }; // margins
    rows += 1; // summary line
    rows += 1; // path line
    if has_metadata_line(agent) {
        rows += 1;
    }
    if agent.details_ready {
        rows += 1;
    }
    if compact && separator {
        rows += 1; // separator row (between items, counted as part of each item except the last)
    }
    rows
}

pub fn visible_item_count_opts(
    height: u32,
    agents: &[AgentInfo],
    scroll_offset: usize,
    expanded: bool,
    compact: bool,
    separator: bool,
) -> usize {
    let available = height.saturating_sub(header_rows(expanded));
    // Each item occupies its own rows; separators appear *between* visible items,
    // never after the last visible one. Greedily count items without any separator
    // overhead, then add (count-1) separator rows at the end to check fit.
    // This matches exactly what render_sidebar draws.
    let base_rows: Vec<u32> = agents
        .iter()
        .skip(scroll_offset)
        .map(|a| item_row_count_opts(a, compact, false))
        .collect();

    let sep_rows = if compact && separator { 1u32 } else { 0 };

    let mut used = 0u32;
    let mut count = 0usize;
    for &item_h in &base_rows {
        // Adding this item costs item_h rows plus one separator if it's not first
        let extra = if count > 0 { sep_rows } else { 0 };
        if used + extra + item_h > available {
            break;
        }
        used += extra + item_h;
        count += 1;
    }
    count
}

pub struct RenderOptions<'a> {
    pub width: u32,
    pub height: u32,
    pub selected: Option<usize>,
    pub scroll_offset: usize,
    pub unseen_done: &'a HashSet<String>,
    pub expanded: bool,
    pub header_selected: bool,
    pub compact_mode: bool,
    pub item_separator: bool,
    /// Elapsed milliseconds since sidebar start, used to drive the Working-state pulse animation.
    pub elapsed_ms: u64,
    /// When true, only emit Working-state indicator rows (skips header and all other lines).
    /// Used for pulse-animation frames to avoid flickering static content.
    pub pulse_only: bool,
    /// When true, skip header rows even in a full render (stats unchanged, only agents changed).
    pub skip_header: bool,
}

pub fn render_sidebar(
    agents: &[AgentInfo],
    stats: &AggregatedStats,
    opts: RenderOptions<'_>,
) -> String {
    let width = opts.width;
    let height = opts.height;
    let selected = opts.selected;
    let scroll_offset = opts.scroll_offset;
    let unseen_done = opts.unseen_done;
    let expanded = opts.expanded;
    let header_selected = opts.header_selected;
    let compact_mode = opts.compact_mode;
    let item_separator = opts.item_separator;
    let pulse_only = opts.pulse_only;
    let skip_header = opts.skip_header || pulse_only;
    let w = width as usize;
    let mut buf = String::new();
    let mut row: u32 = 1;

    buf.push_str("\x1b[?25l");

    if skip_header {
        // Skip header rows entirely; advance `row` past the header.
        row = header_rows(expanded) + 1;
    }

    // === Header ===
    if !skip_header {
    let title = "agentmux";
    let padding = w.saturating_sub(title.len()) / 2;
    emit_line_bg(
        &mut buf,
        row,
        HEADER_BG,
        &format!("{}{BOLD}{BLUE}{title}{RESET}", " ".repeat(padding)),
    );
    row += 1;

    // === Stats table (6 columns: name │ period │ in │ out │ cost │ turns) ===
    let col0 = "Claude".len().max("Codex".len()) + 2; // 8
    let col1 = "Weekly".len() + 2; // 8
    // Compute token/cost column widths from data (min fits header labels)
    let all_totals = [
        &stats.claude.today,
        &stats.claude.seven_days,
        &stats.claude.total,
        &stats.codex.today,
        &stats.codex.seven_days,
        &stats.codex.total,
    ];
    let col2 = all_totals
        .iter()
        .map(|t| format_tokens(t.input_tokens).len())
        .max()
        .unwrap_or(1)
        .max("↑ In".chars().count())
        + 2;
    let col3 = all_totals
        .iter()
        .map(|t| format_tokens(t.output_tokens).len())
        .max()
        .unwrap_or(1)
        .max("↓ Out".chars().count())
        + 2;
    let col4 = all_totals
        .iter()
        .map(|t| format_cost(t.cost_usd).len())
        .max()
        .unwrap_or(4)
        .max("Cost".len())
        + 2;
    // col5 (Turns) gets all remaining width
    let col5 = w
        .saturating_sub(col0 + col1 + col2 + col3 + col4 + 5)
        .max("Msgs".len() + 2);
    let cw = [col0, col1, col2, col3, col4, col5];

    let table_bg = if header_selected { SEL_BG } else { "" };
    let emit_table = if header_selected {
        emit_line_bg
    } else {
        emit_line_no_bg
    };

    // Blank prefix width for header rows: cols 0 + │ + col1 = col0 + 1 + col1
    let hdr_blank = cw[0] + 1 + cw[1];

    // Table header: partial top border (cols 2-5 only) + label row
    let hdr_top = format!(
        "{}┌{}┬{}┬{}┬{}",
        " ".repeat(hdr_blank),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
        "─".repeat(cw[5]),
    );
    emit_table(&mut buf, row, table_bg, &format!("{DIM}{hdr_top}{RESET}"));
    row += 1;

    // Header labels (centered in each column)
    let bg = table_bg;
    let hdr_in = centered_in("↑ In", cw[2]);
    let hdr_out = centered_in("↓ Out", cw[3]);
    let hdr_cost = centered_in("Cost", cw[4]);
    let hdr_turns = format!(
        " {}{}",
        "Msgs",
        " ".repeat(cw[5].saturating_sub("Msgs".len() + 1))
    );
    emit_table(
        &mut buf,
        row,
        table_bg,
        &format!(
            "{}{DIM}│{RESET}{bg}{BLUE}{hdr_in}{RESET}{bg}{DIM}│{RESET}{bg}{MAUVE}{hdr_out}{RESET}{bg}{DIM}│{RESET}{bg}{ROSEWATER}{hdr_cost}{RESET}{bg}{DIM}│{RESET}{bg}{FLAMINGO}{hdr_turns}{RESET}{bg}",
            " ".repeat(hdr_blank),
        ),
    );
    row += 1;

    // Border between header and data: cols 0-1 use ┬ (start), cols 2-5 use ┼ (continue)
    let data_top = format!(
        "{}┬{}┼{}┼{}┼{}┼{}",
        "─".repeat(cw[0]),
        "─".repeat(cw[1]),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
        "─".repeat(cw[5]),
    );
    emit_table(&mut buf, row, table_bg, &format!("{DIM}{data_top}{RESET}"));
    row += 1;

    if expanded {
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: &format!("{PEACH}{BOLD}Claude{RESET}"),
                name_plain_len: 6,
                period: "Daily",
            },
            &stats.claude.today,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border(&cw, '┼')),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: "",
                name_plain_len: 0,
                period: "Weekly",
            },
            &stats.claude.seven_days,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border_partial(&cw)),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: "",
                name_plain_len: 0,
                period: "Total",
            },
            &stats.claude.total,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border(&cw, '┼')),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: &format!("{BLUE}{BOLD}Codex{RESET}"),
                name_plain_len: 5,
                period: "Daily",
            },
            &stats.codex.today,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border(&cw, '┼')),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: "",
                name_plain_len: 0,
                period: "Weekly",
            },
            &stats.codex.seven_days,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border_partial(&cw)),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: "",
                name_plain_len: 0,
                period: "Total",
            },
            &stats.codex.total,
        );
    } else {
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: &format!("{PEACH}{BOLD}Claude{RESET}"),
                name_plain_len: 6,
                period: "Daily",
            },
            &stats.claude.today,
        );
        emit_table(
            &mut buf,
            row,
            table_bg,
            &format!("{DIM}{}{RESET}", table_border(&cw, '┼')),
        );
        row += 1;
        row = emit_stats_row(
            &mut buf,
            row,
            table_bg,
            emit_table,
            &cw,
            StatsRowLabel {
                name_colored: &format!("{BLUE}{BOLD}Codex{RESET}"),
                name_plain_len: 5,
                period: "Daily",
            },
            &stats.codex.today,
        );
    }

    // Bottom border with all junctions
    emit_table(
        &mut buf,
        row,
        table_bg,
        &format!("{DIM}{}{RESET}", table_border(&cw, '┴')),
    );
    row += 1;
    } // end !skip_header block

    if agents.is_empty() && !pulse_only {
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
        let visible = visible_item_count_opts(
            height,
            agents,
            scroll_offset,
            expanded,
            compact_mode,
            item_separator,
        );
        let end = (scroll_offset + visible).min(agents.len());

        for (vi, agent) in agents[scroll_offset..end].iter().enumerate() {
            let i = scroll_offset + vi;
            let is_last_visible = vi + 1 == end - scroll_offset;
            let is_selected = !header_selected && selected == Some(i);
            let color = match agent.kind {
                AgentKind::ClaudeCode => PEACH,
                AgentKind::Codex => BLUE,
            };
            let name = agent.kind.display_name();
            let has_badge = unseen_done.contains(&agent.pane_id);

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

            // Selection indicator: a colored '█' bar at column 1, no background fill.
            let sel_bar = if is_selected {
                format!("{color}█{RESET}")
            } else {
                " ".to_string()
            };

            // Build info trail: ↑2.8M ↓25.6k | $2.54
            let sep = format!(" {DIM}|{RESET} ");
            let mut info_parts: Vec<String> = Vec::new();
            info_parts.push(format!(
                "{SUBTEXT}↑ {in_tok}{RESET} {SUBTEXT}↓ {out_tok}{RESET}"
            ));
            if agent.cost_usd >= 0.01 {
                info_parts.push(format!("{SUBTEXT}{}{RESET}", format_cost(agent.cost_usd)));
            }
            let info_str = format!(" {DIM}|{RESET} {}", info_parts.join(&sep));

            // Top margin (skipped in compact mode)
            if !compact_mode {
                if !pulse_only {
                    emit_line_no_bg(&mut buf, row, "", &sel_bar);
                }
                row += 1;
            }

            // Line 1: name elapsed | ↑in ↓out | $cost
            if !pulse_only {
                emit_line_no_bg(
                    &mut buf,
                    row,
                    "",
                    &format!(
                        "{sel_bar} {color}{BOLD}{name}{RESET}{badge} {SUBTEXT}{elapsed}{RESET}{info_str}"
                    ),
                );
            }
            row += 1;

            // Line 2 (optional): model (effort) | N% left | N msgs
            let mut metadata_parts: Vec<String> = Vec::new();
            if let Some(model_display) = metadata_model_display(agent) {
                metadata_parts.push(format!("{SUBTEXT}{model_display}{RESET}"));
            }
            if let Some(pct) = agent.context_pct {
                let left = 100u8.saturating_sub(pct);
                metadata_parts.push(format!("{SUBTEXT}{left}% left{RESET}"));
            }
            if agent.turn_count > 0 {
                let msg_label = if agent.turn_count == 1 { "msg" } else { "msgs" };
                metadata_parts.push(format!("{SUBTEXT}{} {msg_label}{RESET}", agent.turn_count));
            }
            if !metadata_parts.is_empty() {
                if !pulse_only {
                    emit_line_no_bg(
                        &mut buf,
                        row,
                        "",
                        &format!("{sel_bar} {}", metadata_parts.join(&sep)),
                    );
                }
                row += 1;
            }

            // Line 3: [window] cwd
            if !pulse_only {
                emit_line_no_bg(
                    &mut buf,
                    row,
                    "",
                    &format!("{sel_bar} {GRAY}[{win_name}]{RESET} {SUBTEXT}{short_cwd}{RESET}"),
                );
            }
            row += 1;

            if agent.details_ready {
                // Line 4: state dot + last activity / fallback text
                let activity_text = agent.last_activity.as_deref().unwrap_or("");
                let state_line = match agent.state {
                    AgentState::Working => {
                        let text = if activity_text.is_empty() {
                            state_label(agent.state)
                        } else {
                            activity_text
                        };
                        // Pulse: GREEN → black → GREEN over 1 s using a cosine wave.
                        let phase = (opts.elapsed_ms % 1000) as f64 / 1000.0; // 0.0..1.0
                        let intensity = ((phase * std::f64::consts::TAU).cos() + 1.0) / 2.0; // 1→0→1
                        let g = (intensity * 255.0).round() as u8;
                        let pulse_green = format!("\x1b[38;2;0;{g};0m");
                        format!("{sel_bar} {pulse_green}{BOLD}●{RESET}  {GRAY}{text}{RESET}")
                    }
                    AgentState::Idle => {
                        let text = if activity_text.is_empty() {
                            state_label(agent.state)
                        } else {
                            activity_text
                        };
                        format!("{sel_bar} {DIM}●{RESET}  {DIM}{text}{RESET}")
                    }
                };
                // In pulse_only mode, only emit Working-state rows (Idle is static).
                if !pulse_only || matches!(agent.state, AgentState::Working) {
                    emit_line_no_bg(&mut buf, row, "", &state_line);
                }
                row += 1;
            }

            // Bottom margin (skipped in compact mode) or separator between items
            if compact_mode {
                if item_separator && !is_last_visible {
                    if !pulse_only {
                        let sep_line = format!("{DIM}{}{RESET}", "─".repeat(w));
                        emit_line_no_bg(&mut buf, row, "", &sep_line);
                    }
                    row += 1;
                }
            } else {
                if !pulse_only {
                    emit_line_no_bg(&mut buf, row, "", &sel_bar);
                }
                row += 1;
            }
        }
    }

    while !pulse_only && row <= height {
        emit_line_clear(&mut buf, row);
        row += 1;
    }

    // Park cursor at bottom-left (a cleared row) then hide it.
    // This way, if tmux briefly exposes the cursor between render cycles, it appears
    // against a blank background rather than in the middle of content.
    buf.push_str(&format!("\x1b[{height};1H\x1b[?25l"));

    buf
}

struct StatsRowLabel<'a> {
    name_colored: &'a str,
    name_plain_len: usize,
    period: &'a str,
}

fn emit_stats_row(
    buf: &mut String,
    row: u32,
    bg: &str,
    emit: fn(&mut String, u32, &str, &str),
    cw: &[usize; 6],
    label: StatsRowLabel<'_>,
    totals: &AgentTotals,
) -> u32 {
    let cost_str = format_cost(totals.cost_usd);
    let turns_str = format_tokens(totals.turns as u64);
    let in_str = format_tokens(totals.input_tokens);
    let out_str = format_tokens(totals.output_tokens);

    let s = format!("{DIM}│{RESET}{bg}"); // reusable separator
    let data_color = WHITE;

    // Col 0: name (left-aligned, 1 char padding)
    let name_cell = if label.name_plain_len > 0 {
        let pad = cw[0].saturating_sub(label.name_plain_len + 1);
        format!(" {}{RESET}{bg}{}", label.name_colored, " ".repeat(pad))
    } else {
        format!("{bg}{}", " ".repeat(cw[0]))
    };

    // Col 1: period (left-aligned, 1 char padding)
    let period_pad = cw[1].saturating_sub(label.period.len() + 1);
    let period_cell = format!(
        " {DIM}{}{RESET}{bg}{}",
        label.period,
        " ".repeat(period_pad)
    );

    // Col 2: input tokens (left-aligned, 1 char padding)
    let in_pad = cw[2].saturating_sub(in_str.len() + 1);
    let in_cell = format!(" {data_color}{in_str}{RESET}{bg}{}", " ".repeat(in_pad));

    // Col 3: output tokens (left-aligned, 1 char padding)
    let out_pad = cw[3].saturating_sub(out_str.len() + 1);
    let out_cell = format!(" {data_color}{out_str}{RESET}{bg}{}", " ".repeat(out_pad));

    // Col 4: cost (left-aligned, 1 char padding)
    let cost_pad = cw[4].saturating_sub(cost_str.len() + 1);
    let cost_cell = format!(" {data_color}{cost_str}{RESET}{bg}{}", " ".repeat(cost_pad));

    // Col 5: turns (left-aligned, 1 char padding)
    let turns_pad = cw[5].saturating_sub(turns_str.len() + 1);
    let turns_cell = format!(
        " {data_color}{turns_str}{RESET}{bg}{}",
        " ".repeat(turns_pad)
    );

    emit(
        buf,
        row,
        bg,
        &format!(
            "{name_cell}{s}{period_cell}{s}{in_cell}{s}{out_cell}{s}{cost_cell}{s}{turns_cell}"
        ),
    );
    row + 1
}

fn table_border_partial(cw: &[usize; 6]) -> String {
    let blank: String = " ".repeat(cw[0]);
    format!(
        "{blank}├{}┼{}┼{}┼{}┼{}",
        "─".repeat(cw[1]),
        "─".repeat(cw[2]),
        "─".repeat(cw[3]),
        "─".repeat(cw[4]),
        "─".repeat(cw[5]),
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

use crate::detect::short_model_name;

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
    let pad = cell_width.saturating_sub(text.chars().count());
    let left = pad / 2;
    let right = pad - left;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
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

fn has_metadata_line(agent: &AgentInfo) -> bool {
    metadata_model_display(agent).is_some() || agent.context_pct.is_some() || agent.turn_count > 0
}

fn metadata_model_display(agent: &AgentInfo) -> Option<String> {
    match (&agent.model, &agent.effort) {
        (Some(model), Some(effort)) => Some(format!("{} ({effort})", short_model_name(model))),
        (Some(model), None) => Some(short_model_name(model)),
        (None, Some(_)) | (None, None) => None,
    }
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "Working...",
        AgentState::Idle => "Idle",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::history::AggregatedStats;
    use crate::detect::process::AgentKind;

    fn sample_agent() -> AgentInfo {
        AgentInfo {
            kind: AgentKind::Codex,
            agent_pid: Some(42),
            pane_id: "%1".to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: AgentState::Working,
            elapsed_secs: 61,
            process_elapsed_secs: 61,
            input_tokens: 1_000,
            output_tokens: 200,
            last_activity: Some("exec_command cargo test".to_string()),
            context_pct: Some(25),
            model: Some("gpt-5.4".to_string()),
            effort: Some("medium".to_string()),
            cost_usd: 1.25,
            turn_count: 3,
            session_id: Some("session-1".to_string()),
            jsonl_path: None,
            resumed: false,
            details_ready: true,
        }
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                if matches!(chars.peek(), Some('[')) {
                    let _ = chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn rendered_rows(rendered: &str) -> Vec<(u32, String)> {
        let mut rows = Vec::new();
        let mut cursor = 0usize;

        while let Some((row, content_start, marker_start)) = next_row_marker(rendered, cursor) {
            let next_cursor = next_row_marker(rendered, content_start)
                .map(|(_, _, start)| start)
                .unwrap_or(rendered.len());
            rows.push((
                row,
                strip_ansi(&rendered[content_start..next_cursor]).replace('\u{0}', ""),
            ));
            cursor = next_cursor.max(marker_start + 1);
        }

        rows
    }

    fn next_row_marker(text: &str, from: usize) -> Option<(u32, usize, usize)> {
        let bytes = text.as_bytes();
        let mut i = from;
        while i + 3 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                let digit_start = i + 2;
                let mut j = digit_start;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digit_start && bytes.get(j..j + 3) == Some(b";1H") {
                    let row = text[digit_start..j].parse::<u32>().ok()?;
                    return Some((row, j + 3, i));
                }
            }
            i += 1;
        }
        None
    }

    #[test]
    fn item_row_count_reserves_state_line_and_optional_metadata_line() {
        let with_metadata = sample_agent();
        let mut without_metadata = sample_agent();
        without_metadata.model = None;
        without_metadata.effort = None;
        without_metadata.context_pct = None;
        without_metadata.turn_count = 0;

        assert_eq!(item_row_count_opts(&with_metadata, false, false), 6);
        assert_eq!(item_row_count_opts(&without_metadata, false, false), 5);
    }

    #[test]
    fn item_row_count_collapses_state_row_for_provisional_items() {
        let mut provisional = sample_agent();
        provisional.details_ready = false;
        provisional.model = None;
        provisional.effort = None;
        provisional.context_pct = None;
        provisional.turn_count = 0;

        assert_eq!(item_row_count_opts(&provisional, false, false), 4);
    }

    #[test]
    fn render_orders_metadata_before_dir_and_prefixes_state_line() {
        let rendered = render_sidebar(
            &[sample_agent()],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: Some(0),
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );
        let rows = rendered_rows(&rendered);
        let model_row = rows
            .iter()
            .find(|(_, line)| line.contains("gpt-5.4 (medium)"))
            .unwrap()
            .0;
        let dir_row = rows
            .iter()
            .find(|(_, line)| line.contains("[main] /tmp/project"))
            .unwrap()
            .0;
        let state_row = rows
            .iter()
            .find(|(_, line)| line.contains("●  exec_command cargo test"))
            .unwrap()
            .0;

        assert!(model_row < dir_row);
        assert!(dir_row < state_row);
        assert!(!rows.iter().any(
            |(_, line)| line.contains("working") || line.contains("> exec_command cargo test")
        ));
    }

    #[test]
    fn render_collapses_metadata_row_and_uses_working_fallback_when_empty() {
        let mut agent = sample_agent();
        agent.model = None;
        agent.effort = None;
        agent.context_pct = None;
        agent.turn_count = 0;
        agent.last_activity = None;

        let rendered = render_sidebar(
            &[agent],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: Some(0),
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );
        let rows = rendered_rows(&rendered);
        let dir_row = rows
            .iter()
            .find(|(_, line)| line.contains("[main] /tmp/project"))
            .unwrap()
            .0;
        let state_row = rows
            .iter()
            .find(|(_, line)| line.contains("●  Working..."))
            .unwrap()
            .0;

        assert_eq!(dir_row + 1, state_row);
        assert!(
            !rows
                .iter()
                .any(|(_, line)| line.contains("gpt-5.4") || line.contains("% left"))
        );
    }

    #[test]
    fn render_uses_idle_fallback_when_last_message_is_empty() {
        let mut agent = sample_agent();
        agent.state = AgentState::Idle;
        agent.last_activity = None;

        let rendered = render_sidebar(
            &[agent],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: Some(0),
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );

        assert!(strip_ansi(&rendered).contains("●  Idle"));
    }

    #[test]
    fn render_hides_state_row_for_provisional_items() {
        let mut agent = sample_agent();
        agent.details_ready = false;
        agent.last_activity = None;

        let rendered = render_sidebar(
            &[agent],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: Some(0),
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );
        let rows = rendered_rows(&rendered);
        let dir_row = rows
            .iter()
            .find(|(_, line)| line.contains("[main] /tmp/project"))
            .unwrap()
            .0;

        assert!(!strip_ansi(&rendered).contains("●  "));
        // Row after dir is the bottom margin; it must not contain a state dot
        assert!(
            rows.iter()
                .all(|(row, line)| *row != dir_row + 1 || !strip_ansi(line).contains('●'))
        );
    }

    #[test]
    fn render_does_not_show_effort_only_metadata_row() {
        let mut agent = sample_agent();
        agent.model = None;
        agent.effort = Some("medium".to_string());
        agent.context_pct = None;
        agent.turn_count = 0;

        let rendered = render_sidebar(
            &[agent],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: Some(0),
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );

        assert!(!strip_ansi(&rendered).contains("effort: medium"));
    }

    #[test]
    fn render_does_not_highlight_any_item_when_selection_is_none() {
        let rendered = render_sidebar(
            &[sample_agent()],
            &AggregatedStats::default(),
            RenderOptions {
                width: 100,
                height: 30,
                selected: None,
                scroll_offset: 0,
                unseen_done: &HashSet::new(),
                expanded: false,
                header_selected: false,
                compact_mode: false,
                item_separator: false,
                elapsed_ms: 0,
                pulse_only: false,
                skip_header: false,
            },
        );

        assert!(!rendered.contains(SEL_BG));
    }

    #[test]
    fn header_rows_match_rendered_layout() {
        assert_eq!(header_rows(false), 8);
        assert_eq!(header_rows(true), 16);
    }
}

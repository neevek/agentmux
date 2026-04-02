use std::collections::HashMap;
use std::process::Command;

use crate::tmux::PaneInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    fn process_patterns(&self) -> &[&str] {
        match self {
            AgentKind::ClaudeCode => &["claude"],
            AgentKind::Codex => &["codex"],
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            AgentKind::ClaudeCode => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// 24-bit ANSI foreground color code
    pub fn color_code(&self) -> &str {
        match self {
            AgentKind::ClaudeCode => "\x1b[38;2;250;179;135m", // peach #fab387
            AgentKind::Codex => "\x1b[38;2;137;180;250m",     // blue #89b4fa
        }
    }
}

const ALL_AGENTS: &[AgentKind] = &[AgentKind::ClaudeCode, AgentKind::Codex];

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    pub pane_id: String,
    pub pane_pid: u32,
    pub cwd: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    /// Elapsed seconds of the matched agent process
    pub elapsed_secs: u64,
}

struct ProcessTree {
    children_of: HashMap<u32, Vec<u32>>,
    comm_of: HashMap<u32, String>,
    etime_of: HashMap<u32, u64>,
}

fn build_process_tree() -> ProcessTree {
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut comm_of: HashMap<u32, String> = HashMap::new();
    let mut etime_of: HashMap<u32, u64> = HashMap::new();

    let Ok(output) = Command::new("ps")
        .args(["-eo", "pid=,ppid=,etime=,comm="])
        .output()
    else {
        return ProcessTree {
            children_of,
            comm_of,
            etime_of,
        };
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "  PID  PPID      ELAPSED COMMAND..."
        // split_whitespace() correctly handles variable-width column spacing
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 4 {
            continue;
        }
        let Some(pid) = tokens[0].parse::<u32>().ok() else {
            continue;
        };
        let Some(ppid) = tokens[1].parse::<u32>().ok() else {
            continue;
        };
        let etime = parse_etime(tokens[2]);
        // comm may contain spaces (e.g., "Google Chrome Helper"), rejoin remaining tokens
        let comm = tokens[3..].join(" ").to_lowercase();
        children_of.entry(ppid).or_default().push(pid);
        comm_of.insert(pid, comm);
        etime_of.insert(pid, etime);
    }

    ProcessTree {
        children_of,
        comm_of,
        etime_of,
    }
}

/// Parse ps etime format: "MM:SS", "HH:MM:SS", "D-HH:MM:SS" → seconds
fn parse_etime(s: &str) -> u64 {
    let (days, rest) = if let Some(pos) = s.find('-') {
        let d: u64 = s[..pos].parse().unwrap_or(0);
        (d, &s[pos + 1..])
    } else {
        (0, s)
    };

    let parts: Vec<u64> = rest.split(':').filter_map(|p| p.parse().ok()).collect();
    let (hours, minutes, seconds) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        2 => (0, parts[0], parts[1]),
        _ => return 0,
    };

    days * 86400 + hours * 3600 + minutes * 60 + seconds
}

/// Format elapsed seconds: always includes minutes; adds hours if >= 1h; adds days if >= 1d.
/// Examples: "0m", "23m", "1h05m", "2d3h"
pub fn format_elapsed(secs: u64) -> String {
    let total_minutes = secs / 60;
    let total_hours = total_minutes / 60;
    let days = total_hours / 24;

    if days > 0 {
        let remaining_hours = total_hours % 24;
        format!("{}d{}h", days, remaining_hours)
    } else if total_hours > 0 {
        let remaining_mins = total_minutes % 60;
        format!("{}h{:02}m", total_hours, remaining_mins)
    } else {
        format!("{}m", total_minutes)
    }
}

/// Match logic from opensessions: pattern must appear at start of comm or after `/`.
fn comm_matches(comm: &str, pattern: &str) -> bool {
    let Some(idx) = comm.find(pattern) else {
        return false;
    };
    if idx > 0 && comm.as_bytes()[idx - 1] != b'/' {
        return false;
    }
    true
}

/// Walk child processes up to 3 levels deep looking for agent patterns.
/// Returns the matched child pid if found.
fn match_process_tree(
    pid: u32,
    patterns: &[&str],
    tree: &ProcessTree,
    depth: u32,
) -> Option<u32> {
    if depth > 4 {
        return None;
    }
    let children = tree.children_of.get(&pid)?;
    for &child_pid in children {
        if let Some(comm) = tree.comm_of.get(&child_pid) {
            if patterns.iter().any(|pat| comm_matches(comm, pat)) {
                return Some(child_pid);
            }
        }
        if let Some(found) = match_process_tree(child_pid, patterns, tree, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Scan tmux panes for running coding agents.
pub fn scan_panes_for_agents(panes: &[PaneInfo], sidebar_title: &str) -> Vec<DetectedAgent> {
    let tree = build_process_tree();
    let mut results = Vec::new();

    for pane in panes {
        if pane.title == sidebar_title {
            continue;
        }

        for agent in ALL_AGENTS {
            let patterns = agent.process_patterns();
            if let Some(matched_pid) = match_process_tree(pane.pid, patterns, &tree, 0) {
                let elapsed_secs = tree.etime_of.get(&matched_pid).copied().unwrap_or(0);
                results.push(DetectedAgent {
                    kind: *agent,
                    pane_id: pane.id.clone(),
                    pane_pid: pane.pid,
                    cwd: pane.cwd.clone(),
                    session_name: pane.session_name.clone(),
                    window_id: pane.window_id.clone(),
                    window_index: pane.window_index,
                    elapsed_secs,
                });
                break;
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comm_matches() {
        assert!(comm_matches("claude", "claude"));
        assert!(comm_matches("/usr/bin/claude", "claude"));
        assert!(comm_matches("claude-code", "claude"));
        assert!(!comm_matches("proclaimer", "claude"));
        assert!(!comm_matches("tail-claude", "claude"));

        assert!(comm_matches("codex", "codex"));
        assert!(comm_matches("/usr/local/bin/codex", "codex"));
        assert!(!comm_matches("mycodex", "codex"));
    }

    #[test]
    fn test_parse_etime() {
        assert_eq!(parse_etime("05:30"), 330);
        assert_eq!(parse_etime("01:05:30"), 3930);
        assert_eq!(parse_etime("2-01:05:30"), 176730);
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(30), "0m");
        assert_eq!(format_elapsed(90), "1m");
        assert_eq!(format_elapsed(3660), "1h01m");
        assert_eq!(format_elapsed(3600), "1h00m");
        assert_eq!(format_elapsed(90000), "1d1h");
    }
}

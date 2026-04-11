use std::collections::HashMap;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::tmux::PaneInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// Stable key used in the database and for internal lookups.
    pub fn db_key(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::Codex => "codex",
        }
    }
}

const ALL_AGENTS: &[AgentKind] = &[AgentKind::ClaudeCode, AgentKind::Codex];

#[derive(Debug, Clone)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    pub pane_id: String,
    pub cwd: String,
    pub window_id: String,
    pub window_index: u32,
    pub agent_pid: u32,
    pub resumed: bool,
    /// Elapsed seconds of the matched agent process
    pub elapsed_secs: u64,
}

struct ProcessTree {
    children_of: HashMap<u32, Vec<u32>>,
    comm_of: HashMap<u32, String>,
    args_of: HashMap<u32, String>,
    etime_of: HashMap<u32, u64>,
}

fn build_process_tree() -> ProcessTree {
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut comm_of: HashMap<u32, String> = HashMap::new();
    let mut args_of: HashMap<u32, String> = HashMap::new();
    let mut etime_of: HashMap<u32, u64> = HashMap::new();

    let Ok(output) = Command::new("ps")
        .args(["-eo", "pid=,ppid=,etime=,comm=,args="])
        .output()
    else {
        return ProcessTree {
            children_of,
            comm_of,
            args_of,
            etime_of,
        };
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "  PID  PPID      ELAPSED COMMAND..."
        // split_whitespace() correctly handles variable-width column spacing
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 5 {
            continue;
        }
        let Some(pid) = tokens[0].parse::<u32>().ok() else {
            continue;
        };
        let Some(ppid) = tokens[1].parse::<u32>().ok() else {
            continue;
        };
        let etime = parse_etime(tokens[2]);
        let comm = tokens[3].to_lowercase();
        let args = tokens[4..].join(" ").to_lowercase();
        children_of.entry(ppid).or_default().push(pid);
        comm_of.insert(pid, comm);
        args_of.insert(pid, args);
        etime_of.insert(pid, etime);
    }

    ProcessTree {
        children_of,
        comm_of,
        args_of,
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

pub fn command_looks_like_agent(comm: &str) -> bool {
    let comm = comm.to_lowercase();
    ALL_AGENTS.iter().any(|agent| {
        agent
            .process_patterns()
            .iter()
            .any(|pattern| comm_matches(&comm, pattern))
    })
}

pub fn query_process_elapsed(pids: &[u32]) -> HashMap<u32, u64> {
    let mut etime_of: HashMap<u32, u64> = HashMap::new();
    if pids.is_empty() {
        return etime_of;
    }

    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let Ok(output) = Command::new("ps")
        .args(["-o", "pid=,etime=", "-p", &pid_list])
        .output()
    else {
        return etime_of;
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let Some(pid) = tokens[0].parse::<u32>().ok() else {
            continue;
        };
        etime_of.insert(pid, parse_etime(tokens[1]));
    }

    etime_of
}

/// Walk child processes up to 3 levels deep looking for agent patterns.
/// Returns the matched child pid if found.
fn match_process_tree(pid: u32, patterns: &[&str], tree: &ProcessTree, depth: u32) -> Option<u32> {
    if depth > 4 {
        return None;
    }
    let matches_pid = tree
        .comm_of
        .get(&pid)
        .is_some_and(|comm| patterns.iter().any(|pat| comm_matches(comm, pat)))
        || tree
            .args_of
            .get(&pid)
            .is_some_and(|args| patterns.iter().any(|pat| comm_matches(args, pat)));
    if matches_pid {
        return Some(pid);
    }
    let children = tree.children_of.get(&pid)?;
    for &child_pid in children {
        if tree
            .comm_of
            .get(&child_pid)
            .is_some_and(|comm| patterns.iter().any(|pat| comm_matches(comm, pat)))
            || tree
                .args_of
                .get(&child_pid)
                .is_some_and(|args| patterns.iter().any(|pat| comm_matches(args, pat)))
        {
            return Some(child_pid);
        }
        if let Some(found) = match_process_tree(child_pid, patterns, tree, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn process_args_indicate_resume(kind: AgentKind, args: Option<&str>) -> bool {
    let Some(args) = args else {
        return false;
    };
    match kind {
        AgentKind::ClaudeCode => args.split_whitespace().any(|part| part == "-r" || part == "--resume"),
        AgentKind::Codex => args.split_whitespace().any(|part| part == "resume"),
    }
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
                    cwd: pane.cwd.clone(),
                    window_id: pane.window_id.clone(),
                    window_index: pane.window_index,
                    agent_pid: matched_pid,
                    resumed: process_args_indicate_resume(
                        *agent,
                        tree.args_of.get(&matched_pid).map(String::as_str),
                    ),
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
    fn test_command_looks_like_agent() {
        assert!(command_looks_like_agent("codex"));
        assert!(command_looks_like_agent("/usr/bin/claude"));
        assert!(!command_looks_like_agent("zsh"));
    }

    #[test]
    fn process_tree_matches_root_pid_and_args() {
        let tree = ProcessTree {
            children_of: HashMap::from([(10, vec![11])]),
            comm_of: HashMap::from([(10, "claude".to_string()), (11, "node".to_string())]),
            args_of: HashMap::from([
                (10, "claude --dangerously-skip-permissions".to_string()),
                (11, "node /opt/bin/codex".to_string()),
            ]),
            etime_of: HashMap::new(),
        };

        assert_eq!(match_process_tree(10, &["claude"], &tree, 0), Some(10));
        assert_eq!(match_process_tree(11, &["codex"], &tree, 0), Some(11));
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

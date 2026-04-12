pub mod history;
pub mod process;
pub mod state;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tmux;
use process::AgentKind;
pub use state::SessionCache;

fn default_details_ready() -> bool {
    true
}

fn default_resumed() -> bool {
    false
}

fn default_process_elapsed_secs() -> u64 {
    0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub kind: AgentKind,
    #[serde(default)]
    pub agent_pid: Option<u32>,
    pub pane_id: String,
    pub cwd: String,
    pub window_id: String,
    pub window_name: String,
    pub state: state::AgentState,
    pub elapsed_secs: u64,
    #[serde(default = "default_process_elapsed_secs")]
    pub process_elapsed_secs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_activity: Option<String>,
    pub context_pct: Option<u8>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cost_usd: f64,
    pub turn_count: u32,
    pub session_id: Option<String>,
    pub jsonl_path: Option<PathBuf>,
    #[serde(default = "default_resumed")]
    pub resumed: bool,
    #[serde(default = "default_details_ready")]
    pub details_ready: bool,
}

/// Full scan: find all agent panes in the session and determine their state.
pub fn scan_agents(session: &str, cache: &mut SessionCache) -> Vec<AgentInfo> {
    let panes = tmux::list_session_panes(session);
    let mut detected = process::scan_panes_for_agents(&panes, crate::tmux::SIDEBAR_TITLE);
    cache.retain_live_agents(&detected);
    detected.sort_by_key(|agent| scan_order_key(agent, cache));
    agents_from_detected(session, detected, cache)
}

fn scan_order_key(agent: &process::DetectedAgent, cache: &mut SessionCache) -> (u64, u64) {
    (state::binding_priority(agent, cache), agent.elapsed_secs)
}

fn has_bound_session(agent: &AgentInfo) -> bool {
    agent.jsonl_path.is_some() && agent.session_id.is_some()
}

fn details_have_bound_session(details: &state::SessionDetails) -> bool {
    details.jsonl_path.is_some() && details.session_id.is_some()
}

fn should_force_full_rescan(agent: &AgentInfo, details: &state::SessionDetails) -> bool {
    if !agent.details_ready {
        return true;
    }

    has_bound_session(agent) && !details_have_bound_session(details)
}

fn display_elapsed_secs(
    _kind: AgentKind,
    process_elapsed_secs: u64,
    details: &state::SessionDetails,
) -> u64 {
    details
        .display_elapsed_secs
        .unwrap_or(process_elapsed_secs)
}

/// Fast scan for startup: discovers active agent panes without JSONL/state lookup.
/// This is intentionally lightweight so sidebar content appears immediately.
pub fn scan_agents_fast(session: &str) -> Vec<AgentInfo> {
    let panes = tmux::list_session_panes(session);
    let detected = process::scan_panes_for_agents(&panes, crate::tmux::SIDEBAR_TITLE);
    let window_names = tmux::list_window_names(session);

    let mut agents = Vec::new();
    for d in detected {
        let window_name = window_names
            .get(&d.window_id)
            .cloned()
            .unwrap_or_else(|| d.window_index.to_string());
        agents.push(AgentInfo {
            kind: d.kind,
            agent_pid: Some(d.agent_pid),
            pane_id: d.pane_id,
            cwd: d.cwd,
            window_id: d.window_id,
            window_name,
            state: state::AgentState::Working,
            elapsed_secs: d.elapsed_secs,
            process_elapsed_secs: d.elapsed_secs,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: None,
            jsonl_path: None,
            resumed: d.resumed,
            details_ready: false,
        });
    }

    agents.sort_by_key(|a| a.elapsed_secs);
    agents
}

pub fn discover_agents_in_panes(
    session: &str,
    panes: &[tmux::PaneInfo],
    cache: &mut SessionCache,
) -> Vec<AgentInfo> {
    let mut detected = process::scan_panes_for_agents(panes, crate::tmux::SIDEBAR_TITLE);
    detected.sort_by_key(|agent| scan_order_key(agent, cache));
    agents_from_detected(session, detected, cache)
}

fn agents_from_detected(
    session: &str,
    detected: Vec<process::DetectedAgent>,
    cache: &mut SessionCache,
) -> Vec<AgentInfo> {
    let window_names = tmux::list_window_names(session);
    let mut agents = Vec::new();

    for d in detected {
        let details = match d.kind {
            AgentKind::ClaudeCode => state::claude_code_details(&d, cache),
            AgentKind::Codex => state::codex_details(&d, cache),
        };

        let window_name = window_names
            .get(&d.window_id)
            .cloned()
            .unwrap_or_else(|| d.window_index.to_string());
        let cost_usd = details
            .model
            .as_deref()
            .map(|m| {
                estimate_cost(
                    m,
                    details.input_tokens,
                    details.output_tokens,
                    details.cache_read_tokens,
                    details.cache_creation_tokens,
                )
            })
            .unwrap_or(0.0);
        let elapsed_secs = display_elapsed_secs(d.kind, d.elapsed_secs, &details);
        agents.push(AgentInfo {
            kind: d.kind,
            agent_pid: Some(d.agent_pid),
            pane_id: d.pane_id,
            cwd: d.cwd,
            window_id: d.window_id,
            window_name,
            state: details.state,
            elapsed_secs,
            process_elapsed_secs: d.elapsed_secs,
            input_tokens: details.input_tokens,
            output_tokens: details.output_tokens,
            last_activity: details.last_activity,
            context_pct: details.context_pct,
            model: details.model,
            effort: details.effort,
            cost_usd,
            turn_count: details.turn_count,
            session_id: details.session_id,
            jsonl_path: details.jsonl_path,
            resumed: d.resumed,
            details_ready: true,
        });
    }

    agents.sort_by_key(|a| a.elapsed_secs);
    agents
}

/// Unified model info: (pattern, short_name, input_price_per_M, output_price_per_M).
/// Order matters: specific variants must come before broad patterns.
const MODEL_TABLE: &[(&str, &str, f64, f64)] = &[
    // Claude
    ("opus", "opus", 5.0, 25.0),
    ("sonnet", "sonnet", 3.0, 15.0),
    ("haiku", "haiku", 1.0, 5.0),
    // OpenAI — specific before broad
    ("o4-mini", "o4-mini", 0.55, 2.20),
    ("o3-mini", "o3-mini", 1.10, 4.40),
    ("o3", "o3", 2.0, 8.0),
    ("gpt-5.4-codex", "gpt-5.4-codex", 2.0, 8.0),
    ("gpt-5.4-mini", "gpt-5.4-mini", 0.40, 1.60),
    ("gpt-5.4-nano", "gpt-5.4-nano", 0.10, 0.40),
    ("gpt-5.4", "gpt-5.4", 2.0, 8.0),
    ("gpt-5.3-codex", "gpt-5.3-codex", 2.0, 8.0),
    ("gpt-4.1-nano", "gpt-4.1-nano", 0.10, 0.40),
    ("gpt-4.1-mini", "gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", "gpt-4.1", 2.0, 8.0),
    ("gpt-4o-mini", "gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", "gpt-4o", 2.50, 10.0),
];

fn lookup_model(model: &str) -> Option<&'static (&'static str, &'static str, f64, f64)> {
    MODEL_TABLE
        .iter()
        .find(|(pattern, _, _, _)| model.contains(pattern))
}

/// Estimate cost using per-type pricing.
/// Claude: cache_read = 10% of base input, cache_creation = 125% of base input.
/// Codex/OpenAI: no cache breakdown (cache_read/cache_creation = 0).
pub(crate) fn estimate_cost(
    model: &str,
    total_input: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
) -> f64 {
    let (base_input_price, output_price) = lookup_model(model)
        .map(|m| (m.2, m.3))
        .unwrap_or((3.0, 15.0));
    let noncached = total_input.saturating_sub(cache_read + cache_creation);
    (noncached as f64 * base_input_price
        + cache_read as f64 * base_input_price * 0.10
        + cache_creation as f64 * base_input_price * 1.25
        + output_tokens as f64 * output_price)
        / 1_000_000.0
}

/// Short display name for a model string. For Claude models, appends the version.
pub(crate) fn short_model_name(model: &str) -> String {
    let Some(&(_, base_name, _, _)) = lookup_model(model) else {
        return model.to_string();
    };
    // Claude models: extract version from "claude-opus-4-6-20260401" → "opus-4.6"
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
    base_name.to_string()
}

pub fn refresh_agents_incremental_from_panes(
    panes: &[tmux::PaneInfo],
    known_agents: &[AgentInfo],
    cache: &mut SessionCache,
) -> Option<Vec<AgentInfo>> {
    let known_pids: Vec<u32> = known_agents
        .iter()
        .filter_map(|agent| agent.agent_pid)
        .collect();
    let elapsed_by_pid = process::query_process_elapsed(&known_pids);
    refresh_agents_incremental_with_elapsed(panes, known_agents, cache, &elapsed_by_pid)
}

fn refresh_agents_incremental_with_elapsed(
    panes: &[tmux::PaneInfo],
    known_agents: &[AgentInfo],
    cache: &mut SessionCache,
    elapsed_by_pid: &HashMap<u32, u64>,
) -> Option<Vec<AgentInfo>> {
    let pane_map: HashMap<&str, &tmux::PaneInfo> = panes
        .iter()
        .filter(|pane| pane.title != crate::tmux::SIDEBAR_TITLE)
        .map(|pane| (pane.id.as_str(), pane))
        .collect();

    let mut refreshed = Vec::new();

    for agent in known_agents {
        let Some(pane) = pane_map.get(agent.pane_id.as_str()) else {
            continue;
        };
        let agent_pid = agent.agent_pid?;
        let Some(process_elapsed_secs) = elapsed_by_pid.get(&agent_pid).copied() else {
            // The tracked PID disappeared while the pane still exists.
            // Fall back to a full rescan to rediscover the live agent process.
            return None;
        };

        let details = state::refresh_tracked_details(agent, process_elapsed_secs, cache);
        if should_force_full_rescan(agent, &details) {
            return None;
        }

        let window_name = if agent.window_id == pane.window_id {
            agent.window_name.clone()
        } else {
            pane.window_index.to_string()
        };
        let cost_usd = details
            .model
            .as_deref()
            .map(|m| {
                estimate_cost(
                    m,
                    details.input_tokens,
                    details.output_tokens,
                    details.cache_read_tokens,
                    details.cache_creation_tokens,
                )
            })
            .unwrap_or(0.0);
        let elapsed_secs = display_elapsed_secs(agent.kind, process_elapsed_secs, &details);
        refreshed.push(AgentInfo {
            kind: agent.kind,
            agent_pid: Some(agent_pid),
            pane_id: pane.id.clone(),
            cwd: pane.cwd.clone(),
            window_id: pane.window_id.clone(),
            window_name,
            state: details.state,
            elapsed_secs,
            process_elapsed_secs,
            input_tokens: details.input_tokens,
            output_tokens: details.output_tokens,
            last_activity: details.last_activity,
            context_pct: details.context_pct,
            model: details.model,
            effort: details.effort,
            cost_usd,
            turn_count: details.turn_count,
            session_id: details.session_id,
            jsonl_path: details.jsonl_path,
            resumed: agent.resumed,
            details_ready: true,
        });
    }

    refreshed.sort_by_key(|a| a.elapsed_secs);
    Some(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_prefers_session_elapsed_over_process_elapsed() {
        let details = state::SessionDetails {
            display_elapsed_secs: Some(12),
            ..Default::default()
        };

        assert_eq!(display_elapsed_secs(AgentKind::Codex, 3600, &details), 12);
    }

    #[test]
    fn claude_uses_session_elapsed_when_available() {
        let details = state::SessionDetails {
            display_elapsed_secs: Some(12),
            ..Default::default()
        };

        assert_eq!(
            display_elapsed_secs(AgentKind::ClaudeCode, 3600, &details),
            12
        );
    }

    #[test]
    fn newer_agent_wins_scan_order_when_priority_ties() {
        let older = process::DetectedAgent {
            kind: AgentKind::Codex,
            pane_id: "%1".to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_index: 1,
            agent_pid: 101,
            resumed: false,
            elapsed_secs: 900,
        };
        let newer = process::DetectedAgent {
            pane_id: "%2".to_string(),
            agent_pid: 202,
            elapsed_secs: 90,
            ..older.clone()
        };

        let mut cache = SessionCache::new();
        assert!(scan_order_key(&newer, &mut cache) < scan_order_key(&older, &mut cache));
    }

    #[test]
    fn provisional_agents_still_force_full_rescan() {
        let agent = AgentInfo {
            kind: AgentKind::Codex,
            agent_pid: Some(101),
            pane_id: "%1".to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: state::AgentState::Working,
            elapsed_secs: 10,
            process_elapsed_secs: 10,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: None,
            jsonl_path: None,
            resumed: false,
            details_ready: false,
        };

        assert!(should_force_full_rescan(
            &agent,
            &state::SessionDetails::default()
        ));
    }

    #[test]
    fn metadata_less_tracked_agents_do_not_force_full_rescan() {
        let agent = AgentInfo {
            kind: AgentKind::Codex,
            agent_pid: Some(101),
            pane_id: "%1".to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: state::AgentState::Idle,
            elapsed_secs: 10,
            process_elapsed_secs: 10,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: None,
            jsonl_path: None,
            resumed: false,
            details_ready: true,
        };

        assert!(!should_force_full_rescan(
            &agent,
            &state::SessionDetails::default()
        ));
    }

    #[test]
    fn losing_a_bound_session_forces_full_rescan() {
        let agent = AgentInfo {
            kind: AgentKind::ClaudeCode,
            agent_pid: Some(101),
            pane_id: "%1".to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: state::AgentState::Working,
            elapsed_secs: 10,
            process_elapsed_secs: 10,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: Some("session-1".to_string()),
            jsonl_path: Some(std::path::PathBuf::from("/tmp/session-1.jsonl")),
            resumed: false,
            details_ready: true,
        };

        assert!(should_force_full_rescan(
            &agent,
            &state::SessionDetails::default()
        ));
    }

    fn tracked_agent(kind: AgentKind, pane_id: &str, pid: u32) -> AgentInfo {
        AgentInfo {
            kind,
            agent_pid: Some(pid),
            pane_id: pane_id.to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: state::AgentState::Working,
            elapsed_secs: 10,
            process_elapsed_secs: 10,
            input_tokens: 0,
            output_tokens: 0,
            last_activity: None,
            context_pct: None,
            model: None,
            effort: None,
            cost_usd: 0.0,
            turn_count: 0,
            session_id: None,
            jsonl_path: None,
            resumed: false,
            details_ready: true,
        }
    }

    fn pane(pane_id: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            id: pane_id.to_string(),
            window_id: "@1".to_string(),
            window_index: 1,
            pid: 1,
            cwd: "/tmp/project".to_string(),
            title: "shell".to_string(),
            current_command: "zsh".to_string(),
        }
    }

    #[test]
    fn incremental_refresh_requests_full_rescan_when_tracked_pid_exits() {
        let mut cache = SessionCache::new();
        let refreshed = refresh_agents_incremental_with_elapsed(
            &[pane("%1")],
            &[tracked_agent(AgentKind::ClaudeCode, "%1", 101)],
            &mut cache,
            &HashMap::new(),
        );

        assert!(refreshed.is_none());
    }

    #[test]
    fn incremental_refresh_uses_live_process_elapsed() {
        let mut cache = SessionCache::new();
        let refreshed = refresh_agents_incremental_with_elapsed(
            &[pane("%1")],
            &[tracked_agent(AgentKind::ClaudeCode, "%1", 101)],
            &mut cache,
            &HashMap::from([(101, 42)]),
        )
        .unwrap();

        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].process_elapsed_secs, 42);
        assert_eq!(refreshed[0].elapsed_secs, 42);
    }
}

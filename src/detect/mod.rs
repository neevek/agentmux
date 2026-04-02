pub mod process;
pub mod state;

use crate::tmux;
use process::AgentKind;

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub kind: AgentKind,
    pub pane_id: String,
    pub cwd: String,
    pub window_id: String,
    pub window_name: String,
    pub state: state::AgentState,
    pub elapsed_secs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_activity: Option<String>,
    pub context_pct: Option<u8>,
}

/// Full scan: find all agent panes in the session and determine their state.
pub fn scan_agents(session: &str) -> Vec<AgentInfo> {
    let panes = tmux::list_session_panes(session);
    let detected = process::scan_panes_for_agents(&panes, "tmux-agents-sidebar");

    // Build a window_id → window_name map
    let window_names = tmux::list_window_names(session);

    detected
        .into_iter()
        .map(|d| {
            let details = match d.kind {
                AgentKind::ClaudeCode => state::claude_code_details(&d.cwd),
                AgentKind::Codex => state::codex_details(&d.cwd),
            };
            let window_name = window_names
                .get(&d.window_id)
                .cloned()
                .unwrap_or_else(|| d.window_index.to_string());
            AgentInfo {
                kind: d.kind,
                pane_id: d.pane_id,
                cwd: d.cwd,
                window_id: d.window_id,
                window_name,
                state: details.state,
                elapsed_secs: d.elapsed_secs,
                input_tokens: details.input_tokens,
                output_tokens: details.output_tokens,
                last_activity: details.last_activity,
                context_pct: details.context_pct,
            }
        })
        .collect()
}

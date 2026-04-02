pub mod process;
pub mod state;

use crate::tmux;
use process::AgentKind;
pub use state::SessionCache;

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
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cost_usd: f64,
    pub turn_count: u32,
}

/// Full scan: find all agent panes in the session and determine their state.
pub fn scan_agents(session: &str, cache: &mut SessionCache) -> Vec<AgentInfo> {
    let panes = tmux::list_session_panes(session);
    let detected = process::scan_panes_for_agents(&panes, crate::tmux::SIDEBAR_TITLE);

    let window_names = tmux::list_window_names(session);

    let mut agents: Vec<AgentInfo> = detected
        .into_iter()
        .map(|d| {
            let details = match d.kind {
                AgentKind::ClaudeCode => {
                    state::claude_code_details(&d.cwd, d.elapsed_secs, cache)
                }
                AgentKind::Codex => state::codex_details(&d.cwd, d.elapsed_secs, cache),
            };
            let window_name = window_names
                .get(&d.window_id)
                .cloned()
                .unwrap_or_else(|| d.window_index.to_string());
            let cost_usd = details
                .model
                .as_deref()
                .map(|m| estimate_cost(
                    m,
                    details.input_tokens,
                    details.output_tokens,
                    details.cache_read_tokens,
                    details.cache_creation_tokens,
                ))
                .unwrap_or(0.0);
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
                model: details.model,
                effort: details.effort,
                cost_usd,
                turn_count: details.turn_count,
            }
        })
        .collect();

    // Newest agents first (lowest elapsed_secs = most recently started)
    agents.sort_by_key(|a| a.elapsed_secs);

    agents
}

/// Estimate cost using per-type pricing.
/// Claude: cache_read = 10% of base input, cache_creation = 125% of base input.
/// Codex/OpenAI: no cache breakdown (cache_read/cache_creation = 0).
fn estimate_cost(
    model: &str,
    total_input: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
) -> f64 {
    let (base_input_price, output_price) = model_pricing(model);
    let noncached = total_input.saturating_sub(cache_read + cache_creation);
    (noncached as f64 * base_input_price
        + cache_read as f64 * base_input_price * 0.10
        + cache_creation as f64 * base_input_price * 1.25
        + output_tokens as f64 * output_price)
        / 1_000_000.0
}

fn model_pricing(model: &str) -> (f64, f64) {
    // (base_input_per_million, output_per_million)
    // Claude models (4.5/4.6 pricing)
    if model.contains("opus") {
        (5.0, 25.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (1.0, 5.0)
    // OpenAI models — check specific variants before broad patterns
    } else if model.contains("o4-mini") {
        (0.55, 2.20)
    } else if model.contains("o3-mini") {
        (1.10, 4.40)
    } else if model.contains("o3") {
        (2.0, 8.0)
    } else if model.contains("gpt-4.1-nano") {
        (0.10, 0.40)
    } else if model.contains("gpt-4.1-mini") {
        (0.40, 1.60)
    } else if model.contains("gpt-4.1") {
        (2.0, 8.0)
    } else if model.contains("gpt-4o-mini") {
        (0.15, 0.60)
    } else if model.contains("gpt-4o") {
        (2.50, 10.0)
    } else {
        (3.0, 15.0)
    }
}

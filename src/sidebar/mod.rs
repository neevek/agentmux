pub mod input;
pub mod render;
mod runtime;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::detect;
use crate::detect::AgentInfo;
use crate::detect::history::{AggregatedStats, HistoryStore};
use crate::detect::state::AgentState;
use crate::tmux;

use self::runtime::{LiveSnapshot, RuntimeStore};

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

extern "C" fn exit_handler(_sig: libc::c_int) {
    SHOULD_EXIT.store(true, Ordering::Relaxed);
}

extern "C" fn winch_handler(_sig: libc::c_int) {}

const INPUT_POLL_MS: u64 = 1000;
const FOCUS_POLL_MS: u64 = 500;
const WIDTH_SAVE_DEBOUNCE_MS: u64 = 300;
const DISCOVERY_SWEEP_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderMode {
    Expanded,
    Collapsed,
}

struct HeaderConfig {
    auto_collapse: bool,
    auto_collapse_timeout_ms: u64,
    start_mode: HeaderMode,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            auto_collapse: true,
            auto_collapse_timeout_ms: 5000,
            start_mode: HeaderMode::Expanded,
        }
    }
}

#[derive(Default)]
enum SidebarRole {
    #[default]
    Inactive,
    Follower {
        last_poll: Instant,
    },
    Leader {
        epoch: u64,
        last_refresh: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneFingerprint {
    pid: u32,
    current_command: String,
    cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Selection {
    #[default]
    None,
    Header,
    Agent(String),
}

fn pane_fingerprint(pane: &tmux::PaneInfo) -> PaneFingerprint {
    PaneFingerprint {
        pid: pane.pid,
        current_command: pane.current_command.clone(),
        cwd: pane.cwd.clone(),
    }
}

fn suspect_pane_ids(
    panes: &[tmux::PaneInfo],
    previous: &HashMap<String, PaneFingerprint>,
    tracked_panes: &HashSet<&str>,
    force_sweep: bool,
) -> HashSet<String> {
    let mut suspect = HashSet::new();
    for pane in panes {
        if pane.title == tmux::SIDEBAR_TITLE {
            continue;
        }

        let current = pane_fingerprint(pane);
        if previous.get(&pane.id) != Some(&current) {
            suspect.insert(pane.id.clone());
            continue;
        }

        if force_sweep && !tracked_panes.contains(pane.id.as_str()) {
            suspect.insert(pane.id.clone());
        }
    }
    suspect
}

fn update_pane_fingerprints(
    fingerprints: &mut HashMap<String, PaneFingerprint>,
    panes: &[tmux::PaneInfo],
) {
    fingerprints.clear();
    for pane in panes {
        if pane.title != tmux::SIDEBAR_TITLE {
            fingerprints.insert(pane.id.clone(), pane_fingerprint(pane));
        }
    }
}

fn selection_index(selection: &Selection, agents: &[AgentInfo]) -> Option<usize> {
    match selection {
        Selection::Agent(pane_id) => agents.iter().position(|agent| agent.pane_id == *pane_id),
        Selection::None | Selection::Header => None,
    }
}

fn next_input_poll_timeout(focus_poll_elapsed: Duration, focus_poll_ms: u64) -> Duration {
    let input_timeout = Duration::from_millis(INPUT_POLL_MS);
    let focus_timeout = Duration::from_millis(focus_poll_ms);

    if focus_poll_elapsed >= focus_timeout {
        Duration::ZERO
    } else {
        (focus_timeout - focus_poll_elapsed).min(input_timeout)
    }
}

fn sync_selection_from_focus(
    selection: &mut Selection,
    last_focused_agent_pane_id: &mut Option<String>,
    is_active_window: bool,
    active_pane_id: Option<&str>,
    sidebar_pane_id: &str,
    sidebar_window_id: &str,
    agents: &[AgentInfo],
) -> bool {
    let last_focused_agent = last_focused_agent_pane_id
        .as_deref()
        .filter(|pane_id| agents.iter().any(|agent| agent.pane_id == **pane_id))
        .map(str::to_string);
    if last_focused_agent.is_none() {
        *last_focused_agent_pane_id = None;
    }
    let sole_sidebar_window_agent = if last_focused_agent.is_none() {
        let mut agents_in_window = agents
            .iter()
            .filter(|agent| agent.window_id == sidebar_window_id);
        let first = agents_in_window.next();
        if agents_in_window.next().is_none() {
            first
        } else {
            None
        }
    } else {
        None
    };
    let next = if !is_active_window {
        Selection::None
    } else if let Some(pane_id) = active_pane_id {
        if agents.iter().any(|agent| agent.pane_id == pane_id) {
            *last_focused_agent_pane_id = Some(pane_id.to_string());
            Selection::Agent(pane_id.to_string())
        } else if pane_id == sidebar_pane_id {
            match (
                &*selection,
                last_focused_agent.as_deref(),
                sole_sidebar_window_agent,
            ) {
                (Selection::Header, _, _) => Selection::Header,
                (_, Some(pane_id), _) => Selection::Agent(pane_id.to_string()),
                (_, None, Some(agent)) => Selection::Agent(agent.pane_id.clone()),
                _ => Selection::None,
            }
        } else {
            Selection::None
        }
    } else {
        Selection::None
    };

    if *selection != next {
        *selection = next;
        true
    } else {
        false
    }
}

fn select_agent(agents: &[AgentInfo], idx: usize) -> Selection {
    agents
        .get(idx)
        .map(|agent| Selection::Agent(agent.pane_id.clone()))
        .unwrap_or(Selection::None)
}

fn move_selection_up(selection: &Selection, agents: &[AgentInfo]) -> Selection {
    match selection {
        Selection::Header => Selection::Header,
        Selection::Agent(pane_id) => {
            match agents.iter().position(|agent| agent.pane_id == *pane_id) {
                Some(0) => Selection::Header,
                Some(idx) => select_agent(agents, idx.saturating_sub(1)),
                None => {
                    if agents.is_empty() {
                        Selection::Header
                    } else {
                        select_agent(agents, agents.len() - 1)
                    }
                }
            }
        }
        Selection::None => Selection::Header,
    }
}

fn move_selection_down(selection: &Selection, agents: &[AgentInfo]) -> Selection {
    match selection {
        Selection::Header | Selection::None => select_agent(agents, 0),
        Selection::Agent(pane_id) => {
            match agents.iter().position(|agent| agent.pane_id == *pane_id) {
                Some(idx) => {
                    let max = agents.len().saturating_sub(1);
                    let next_idx = if idx < max { idx + 1 } else { max };
                    select_agent(agents, next_idx)
                }
                None => select_agent(agents, 0),
            }
        }
    }
}

fn activate_agent(
    agent: &AgentInfo,
    unseen_done: &mut HashSet<String>,
    recently_acked: &mut HashSet<String>,
) {
    unseen_done.remove(&agent.pane_id);
    recently_acked.insert(agent.pane_id.clone());
    let in_current_window = tmux::current_window_id().is_some_and(|cw| cw == agent.window_id);
    if !in_current_window {
        tmux::select_window(&agent.window_id);
    }
    tmux::select_pane(&agent.pane_id);
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::detect::process::AgentKind;

    fn pane(id: &str, pid: u32, title: &str, current_command: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            id: id.to_string(),
            window_id: "@1".to_string(),
            window_index: 1,
            pid,
            cwd: "/tmp/project".to_string(),
            title: title.to_string(),
            current_command: current_command.to_string(),
            activity_secs: 0,
        }
    }

    fn agent(pane_id: &str) -> AgentInfo {
        AgentInfo {
            kind: AgentKind::Codex,
            agent_pid: Some(1),
            pane_id: pane_id.to_string(),
            cwd: "/tmp/project".to_string(),
            window_id: "@1".to_string(),
            window_name: "main".to_string(),
            state: AgentState::Working,
            elapsed_secs: 1,
            process_elapsed_secs: 1,
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

    #[test]
    fn suspect_panes_include_tracked_pane_fingerprint_changes() {
        let tracked = pane("%1", 100, "shell", "zsh");
        let previous = HashMap::from([("%1".to_string(), pane_fingerprint(&tracked))]);
        let changed = pane("%1", 101, "shell", "zsh");
        let tracked_panes = HashSet::from(["%1"]);

        let suspect = suspect_pane_ids(&[changed], &previous, &tracked_panes, false);
        assert_eq!(suspect, HashSet::from(["%1".to_string()]));
    }

    #[test]
    fn suspect_panes_ignore_title_only_changes() {
        let tracked = pane("%1", 100, "shell", "zsh");
        let previous = HashMap::from([("%1".to_string(), pane_fingerprint(&tracked))]);
        let retitled = pane("%1", 100, "spinner update", "zsh");
        let tracked_panes = HashSet::from(["%1"]);

        let suspect = suspect_pane_ids(&[retitled], &previous, &tracked_panes, false);
        assert!(suspect.is_empty());
    }

    #[test]
    fn sync_selection_tracks_focused_agent_pane() {
        let mut selection = Selection::None;
        let mut last_focused_agent_pane_id = None;
        let agents = vec![agent("%2"), agent("%3")];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%3"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%3".to_string()));
        assert_eq!(last_focused_agent_pane_id.as_deref(), Some("%3"));
    }

    #[test]
    fn next_input_poll_timeout_wakes_immediately_when_focus_refresh_is_due() {
        assert_eq!(
            next_input_poll_timeout(Duration::from_millis(FOCUS_POLL_MS), FOCUS_POLL_MS),
            Duration::ZERO
        );
        assert_eq!(
            next_input_poll_timeout(Duration::from_millis(FOCUS_POLL_MS + 10), FOCUS_POLL_MS),
            Duration::ZERO
        );
    }

    #[test]
    fn next_input_poll_timeout_caps_sleep_to_remaining_focus_deadline() {
        assert_eq!(
            next_input_poll_timeout(Duration::from_millis(0), FOCUS_POLL_MS),
            Duration::from_millis(FOCUS_POLL_MS)
        );
        assert_eq!(
            next_input_poll_timeout(Duration::from_millis(100), FOCUS_POLL_MS),
            Duration::from_millis(FOCUS_POLL_MS - 100)
        );
    }

    #[test]
    fn sync_selection_clears_when_focus_leaves_sidebar_and_agents() {
        let mut selection = Selection::Agent("%2".to_string());
        let mut last_focused_agent_pane_id = Some("%2".to_string());
        let agents = vec![agent("%2"), agent("%3")];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%9"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::None);
    }

    #[test]
    fn sync_selection_clears_header_when_sidebar_window_loses_focus() {
        let mut selection = Selection::Header;
        let mut last_focused_agent_pane_id = None;

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            false,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &[],
        );

        assert!(changed);
        assert_eq!(selection, Selection::None);
    }

    #[test]
    fn sync_selection_prefers_last_focused_when_sidebar_is_active() {
        let mut selection = Selection::Agent("%3".to_string());
        let mut last_focused_agent_pane_id = Some("%2".to_string());
        let mut first = agent("%3");
        first.state = AgentState::Working;
        let mut second = agent("%2");
        second.state = AgentState::Idle;
        let agents = vec![first, second];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));
    }

    #[test]
    fn sync_selection_clears_stale_last_focused_agent_when_sidebar_is_active() {
        let mut selection = Selection::Agent("%3".to_string());
        let mut last_focused_agent_pane_id = Some("%9".to_string());
        let mut idle = agent("%2");
        idle.state = AgentState::Idle;
        let mut second = agent("%3");
        second.window_id = "@2".to_string();
        let agents = vec![idle, second];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));
        assert_eq!(last_focused_agent_pane_id, None);
    }

    #[test]
    fn sync_selection_uses_sole_sidebar_window_agent_without_focus_history() {
        let mut selection = Selection::None;
        let mut last_focused_agent_pane_id = None;
        let agents = vec![agent("%2")];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));
    }

    #[test]
    fn sync_selection_falls_back_to_last_focused_when_no_working_agent_exists() {
        let mut selection = Selection::Agent("%3".to_string());
        let mut last_focused_agent_pane_id = Some("%2".to_string());
        let mut first = agent("%2");
        first.state = AgentState::Idle;
        let mut second = agent("%3");
        second.state = AgentState::Idle;
        let agents = vec![first, second];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));
    }

    #[test]
    fn sync_selection_preserves_header_when_sidebar_is_active() {
        let mut selection = Selection::Header;
        let mut last_focused_agent_pane_id = Some("%2".to_string());
        let agents = vec![agent("%2"), agent("%3")];

        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );

        assert!(!changed);
        assert_eq!(selection, Selection::Header);
    }

    #[test]
    fn keyboard_nav_down_reaches_last_item_even_with_last_focused_agent() {
        // Regression: sync_selection_from_focus used to snap selection back to
        // last_focused_agent on every loop iteration, preventing keyboard
        // navigation from reaching any item other than the last activated one.
        // Fix: keyboard handlers update last_focused_agent_pane_id so the sync
        // sees the selection as already matching and doesn't override.
        let agents = vec![agent("%1"), agent("%2"), agent("%3")];

        // Simulate: user previously activated %1, then returns to the sidebar
        // and presses j twice to reach %3.
        let mut selection = Selection::Agent("%1".to_string());
        let mut last_focused = Some("%1".to_string());

        // First j press: moves to %2, update last_focused to match
        let next = move_selection_down(&selection, &agents);
        assert_eq!(next, Selection::Agent("%2".to_string()));
        if let Selection::Agent(ref pane_id) = next {
            last_focused = Some(pane_id.clone());
        }
        selection = next;

        // sync_selection_from_focus should NOT override: last_focused now == selection
        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );
        assert!(!changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));

        // Second j press: moves to %3 (last item)
        let next = move_selection_down(&selection, &agents);
        assert_eq!(next, Selection::Agent("%3".to_string()));
        if let Selection::Agent(ref pane_id) = next {
            last_focused = Some(pane_id.clone());
        }
        selection = next;

        // sync_selection_from_focus should NOT override
        let changed = sync_selection_from_focus(
            &mut selection,
            &mut last_focused,
            true,
            Some("%sidebar"),
            "%sidebar",
            "@1",
            &agents,
        );
        assert!(!changed);
        assert_eq!(selection, Selection::Agent("%3".to_string()));
    }

    #[test]
    fn apply_agents_update_marks_unseen_only_for_stops_outside_active_window() {
        let mut prev_states = HashMap::from([("%1".to_string(), AgentState::Working)]);
        let mut unseen_done = HashSet::new();
        let mut stopped = agent("%1");
        stopped.state = AgentState::Idle;
        stopped.window_id = "@2".to_string();

        apply_agents_update(&mut prev_states, &mut unseen_done, &mut HashSet::new(), &[stopped], Some("@1"));

        assert!(unseen_done.contains("%1"));
    }

    #[test]
    fn apply_agents_update_skips_unseen_for_stops_in_active_window() {
        let mut prev_states = HashMap::from([("%1".to_string(), AgentState::Working)]);
        let mut unseen_done = HashSet::new();
        let mut stopped = agent("%1");
        stopped.state = AgentState::Idle;
        stopped.window_id = "@1".to_string();

        apply_agents_update(&mut prev_states, &mut unseen_done, &mut HashSet::new(), &[stopped], Some("@1"));

        assert!(!unseen_done.contains("%1"));
    }

    #[test]
    fn apply_agents_update_clears_unseen_when_window_becomes_active() {
        let mut prev_states = HashMap::from([("%1".to_string(), AgentState::Idle)]);
        let mut unseen_done = HashSet::from(["%1".to_string()]);
        let mut idle = agent("%1");
        idle.state = AgentState::Idle;
        idle.window_id = "@2".to_string();

        apply_agents_update(&mut prev_states, &mut unseen_done, &mut HashSet::new(), &[idle], Some("@2"));

        assert!(!unseen_done.contains("%1"));
    }
}

struct SidebarConfig {
    compact_mode: bool,
    item_separator: bool,
}

fn load_sidebar_config() -> SidebarConfig {
    use crate::config::read_value;
    SidebarConfig {
        compact_mode: read_value("sidebar", "compact_mode")
            .map(|v| v == "true")
            .unwrap_or(false),
        item_separator: read_value("sidebar", "item_separator")
            .map(|v| v == "true")
            .unwrap_or(false),
    }
}

fn load_header_config() -> HeaderConfig {
    use crate::config::read_value;
    let mut cfg = HeaderConfig::default();

    if let Some(v) = read_value("header", "auto_collapse") {
        cfg.auto_collapse = v == "true";
    }
    if let Some(v) = read_value("header", "auto_collapse_timeout_ms") {
        cfg.auto_collapse_timeout_ms = v.parse().unwrap_or(5000);
    }
    if let Some(v) = read_value("header", "start_mode") {
        cfg.start_mode = if v == "collapsed" {
            HeaderMode::Collapsed
        } else {
            HeaderMode::Expanded
        };
    }
    cfg
}

pub fn run() {
    crate::config::ensure_config();
    let session = tmux::current_session().expect("not running inside tmux");

    unsafe {
        let exit_h = exit_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, exit_h);
        libc::signal(libc::SIGINT, exit_h);
        libc::signal(libc::SIGHUP, exit_h);
        libc::signal(
            libc::SIGWINCH,
            winch_handler as *const () as libc::sighandler_t,
        );
    }

    let _guard = terminal_setup();

    let mut prev_states: HashMap<String, AgentState> = HashMap::new();
    let mut unseen_done: HashSet<String> = HashSet::new();
    let mut recently_acked: HashSet<String> = HashSet::new();
    let mut cached_agents = detect::scan_agents_fast(&session);
    let mut history = HistoryStore::start();
    let mut current_stats = history.aggregated_stats(&cached_agents);
    let mut role = SidebarRole::Inactive;
    let mut runtime_store = RuntimeStore::new(&session);
    let mut detect_cache = detect::SessionCache::new();
    let sidebar_pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let sidebar_window_id = tmux::pane_window_id(&sidebar_pane_id)
        .or_else(tmux::current_window_id)
        .unwrap_or_default();
    let mut last_active = false;
    let mut last_focus_poll = Instant::now() - Duration::from_millis(FOCUS_POLL_MS);
    let mut cached_focus_is_active = true;
    let mut cached_active_pane_id: Option<String> = None;
    let mut last_focused_agent_pane_id = None;
    let mut selection = Selection::None;
    let mut scroll_offset = 0usize;
    let mut last_width = 0u32;
    let mut last_width_change = Instant::now() - Duration::from_secs(10);
    let mut pending_width_save: Option<u32> = None;
    let mut needs_render = true;
    let mut last_rendered_stats: Option<crate::detect::history::AggregatedStats> = None;
    let mut last_rendered_header_selected: Option<bool> = None;
    let mut just_activated = false;
    let mut suppress_on_exit = false;
    let mut pane_fingerprints: HashMap<String, PaneFingerprint> = HashMap::new();
    let mut last_discovery_sweep = Instant::now() - Duration::from_millis(DISCOVERY_SWEEP_MS);

    let header_config = load_header_config();
    let sidebar_config = load_sidebar_config();
    let mut header_expanded = header_config.start_mode == HeaderMode::Expanded;
    let mut header_user_toggled = false;
    let start_time = Instant::now();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        let effective_focus_poll_ms = if matches!(selection, Selection::Header) {
            50
        } else {
            FOCUS_POLL_MS
        };
        if sidebar_window_id.is_empty() {
            cached_focus_is_active = true;
            cached_active_pane_id = None;
        } else if last_focus_poll.elapsed() >= Duration::from_millis(effective_focus_poll_ms) {
            if tmux::window_pane_count(&sidebar_window_id) <= 1 {
                tmux::kill_window(&sidebar_window_id);
                break;
            }
            let (is_active, active_pane_id) = tmux::window_focus(&sidebar_window_id);
            cached_focus_is_active = is_active;
            cached_active_pane_id = active_pane_id;
            last_focus_poll = Instant::now();
        }
        let is_active = cached_focus_is_active;
        let active_pane_id = cached_active_pane_id.as_deref();
        let active_window_id = tmux::current_window_id();
        if is_active != last_active {
            if !is_active {
                if !matches!(role, SidebarRole::Leader { .. }) {
                    role = SidebarRole::Inactive;
                }
            } else {
                activate_sidebar(
                    &mut role,
                    &mut runtime_store,
                    &mut cached_agents,
                    &mut current_stats,
                    &mut prev_states,
                    &mut unseen_done,
                    &mut recently_acked,
                    active_window_id.as_deref(),
                );
                just_activated = true;
            }
            last_active = is_active;
            needs_render = true;
        }

        if sync_selection_from_focus(
            &mut selection,
            &mut last_focused_agent_pane_id,
            is_active,
            active_pane_id,
            &sidebar_pane_id,
            &sidebar_window_id,
            &cached_agents,
        ) {
            needs_render = true;
        }

        if !just_activated {
            let mut promote_to_leader = None;
            match &mut role {
                SidebarRole::Leader {
                    epoch,
                    last_refresh,
                } if last_refresh.elapsed() >= Duration::from_millis(runtime::POLL_INTERVAL_MS) => {
                    if refresh_leader_state(
                        &session,
                        *epoch,
                        &mut runtime_store,
                        &mut history,
                        &mut detect_cache,
                        &mut cached_agents,
                        &mut current_stats,
                        &mut prev_states,
                        &mut unseen_done,
                        &mut recently_acked,
                        &mut pane_fingerprints,
                        &mut last_discovery_sweep,
                        active_window_id.as_deref(),
                    ) {
                        *last_refresh = Instant::now();
                        needs_render = true;
                    } else {
                        role = if is_active {
                            SidebarRole::Follower {
                                last_poll: Instant::now()
                                    - Duration::from_millis(runtime::POLL_INTERVAL_MS),
                            }
                        } else {
                            SidebarRole::Inactive
                        };
                    }
                }
                SidebarRole::Follower { last_poll }
                    if is_active
                        && last_poll.elapsed()
                            >= Duration::from_millis(runtime::POLL_INTERVAL_MS) =>
                {
                    let now = Instant::now();
                    let lease = runtime_store.read_lease();
                    if runtime_store.lease_is_stale(&lease) {
                        if let Some(epoch) = runtime_store.try_claim_leader() {
                            promote_to_leader = Some((epoch, now));
                        }
                    } else if let Some(snapshot) = runtime_store.load_snapshot_if_changed(&lease) {
                        apply_snapshot(
                            snapshot,
                            &mut cached_agents,
                            &mut current_stats,
                            &mut prev_states,
                            &mut unseen_done,
                            &mut recently_acked,
                            active_window_id.as_deref(),
                        );
                        needs_render = true;
                    }
                    *last_poll = now;
                }
                SidebarRole::Follower { last_poll }
                    if !is_active
                        && last_poll.elapsed()
                            >= Duration::from_millis(runtime::POLL_INTERVAL_MS) =>
                {
                    let lease = runtime_store.read_lease();
                    if let Some(snapshot) = runtime_store.load_snapshot_if_changed(&lease) {
                        apply_snapshot(
                            snapshot,
                            &mut cached_agents,
                            &mut current_stats,
                            &mut prev_states,
                            &mut unseen_done,
                            &mut recently_acked,
                            active_window_id.as_deref(),
                        );
                        needs_render = true;
                    }
                    *last_poll = Instant::now();
                }
                _ => {}
            }
            if let Some((epoch, now)) = promote_to_leader {
                let refreshed = refresh_leader_state(
                    &session,
                    epoch,
                    &mut runtime_store,
                    &mut history,
                    &mut detect_cache,
                    &mut cached_agents,
                    &mut current_stats,
                    &mut prev_states,
                    &mut unseen_done,
                    &mut recently_acked,
                    &mut pane_fingerprints,
                    &mut last_discovery_sweep,
                    active_window_id.as_deref(),
                );
                role = if refreshed {
                    SidebarRole::Leader {
                        epoch,
                        last_refresh: now,
                    }
                } else {
                    SidebarRole::Follower { last_poll: now }
                };
                needs_render = true;
            }
        }
        just_activated = false;

        if !header_user_toggled
            && header_expanded
            && header_config.start_mode == HeaderMode::Expanded
            && header_config.auto_collapse
            && start_time.elapsed() >= Duration::from_millis(header_config.auto_collapse_timeout_ms)
        {
            header_expanded = false;
            header_user_toggled = true;
            last_rendered_stats = None;
            needs_render = true;
        }

        let header_selected = matches!(selection, Selection::Header);
        let selected_idx = selection_index(&selection, &cached_agents);

        // Sample terminal dimensions once per iteration so all render paths
        // (full, pulse) use the same consistent values.
        let (raw_width, height) = terminal_size();
        let cur_width = raw_width.max(tmux::MIN_WIDTH);

        // Commit width change BEFORE the debounce check so the debounce timer
        // always reflects the most recently observed width.  If we checked the
        // debounce first we could fire resize-pane with a stale (wider) value
        // while the user is still dragging narrower.
        if cur_width != last_width {
            last_width = cur_width;
            last_width_change = Instant::now();
            pending_width_save = Some(cur_width);
            last_rendered_stats = None; // force header redraw at new width
            needs_render = true;
        }

        if let Some(selected_idx) = selected_idx {
            let visible = render::visible_item_count_opts(
                height,
                &cached_agents,
                scroll_offset,
                header_expanded,
                sidebar_config.compact_mode,
                sidebar_config.item_separator,
            );
            if visible > 0 {
                if selected_idx < scroll_offset {
                    scroll_offset = selected_idx;
                } else if selected_idx >= scroll_offset + visible {
                    scroll_offset = selected_idx + 1 - visible;
                }
            }
        }

        // Flush a pending width save once the width has been stable long enough
        // (debounce: avoids tmux resize-pane calls while the user is still dragging).
        if let Some(w) = pending_width_save {
            if last_width_change.elapsed() >= Duration::from_millis(WIDTH_SAVE_DEBOUNCE_MS) {
                tmux::save_sidebar_width(&session, w);
                pending_width_save = None;
            }
        }

        let has_working = cached_agents
            .iter()
            .any(|a| a.state == crate::detect::state::AgentState::Working);

        if needs_render {
            let width = cur_width;
            print!(
                "{}",
                render::render_sidebar(
                    &cached_agents,
                    &current_stats,
                    render::RenderOptions {
                        width,
                        height,
                        selected: selected_idx,
                        scroll_offset,
                        unseen_done: &unseen_done,
                        expanded: header_expanded,
                        header_selected,
                        compact_mode: sidebar_config.compact_mode,
                        item_separator: sidebar_config.item_separator,
                        elapsed_ms: start_time.elapsed().as_millis() as u64,
                        pulse_only: false,
                        skip_header: last_rendered_stats.as_ref() == Some(&current_stats)
                            && last_rendered_header_selected == Some(header_selected),
                    },
                )
            );
            flush();
            last_rendered_stats = Some(current_stats.clone());
            last_rendered_header_selected = Some(header_selected);
            needs_render = false;
        } else if has_working {
            // Pulse-only frame: update only the Working-state indicator rows.
            // This avoids touching static content (header) and prevents cursor flicker.
            let width = last_width;
            print!(
                "{}",
                render::render_sidebar(
                    &cached_agents,
                    &current_stats,
                    render::RenderOptions {
                        width,
                        height,
                        selected: selected_idx,
                        scroll_offset,
                        unseen_done: &unseen_done,
                        expanded: header_expanded,
                        header_selected,
                        compact_mode: sidebar_config.compact_mode,
                        item_separator: sidebar_config.item_separator,
                        elapsed_ms: start_time.elapsed().as_millis() as u64,
                        pulse_only: true,
                        skip_header: true,
                    },
                )
            );
            flush();
        }

        let input_timeout = {
            let base = if sidebar_window_id.is_empty() {
                Duration::from_millis(INPUT_POLL_MS)
            } else {
                next_input_poll_timeout(last_focus_poll.elapsed(), effective_focus_poll_ms)
            };
            // Wake up in time to flush a pending width save
            let base = if pending_width_save.is_some() {
                let debounce = Duration::from_millis(WIDTH_SAVE_DEBOUNCE_MS);
                let elapsed = last_width_change.elapsed();
                let remaining = debounce.saturating_sub(elapsed);
                base.min(remaining + Duration::from_millis(10))
            } else {
                base
            };
            // Drive the Working-state pulse animation at ~50 ms intervals.
            let has_working = cached_agents
                .iter()
                .any(|a| a.state == crate::detect::state::AgentState::Working);
            if has_working {
                base.min(Duration::from_millis(100))
            } else {
                base
            }
        };

        match input::poll_input(input_timeout) {
            input::InputEvent::KeyUp if is_active => {
                let next_selection = move_selection_up(&selection, &cached_agents);
                if next_selection != selection {
                    if let Selection::Agent(pane_id) = &next_selection {
                        last_focused_agent_pane_id = Some(pane_id.clone());
                    }
                    selection = next_selection;
                    needs_render = true;
                }
            }
            input::InputEvent::KeyDown if is_active => {
                let next_selection = move_selection_down(&selection, &cached_agents);
                if next_selection != selection {
                    if let Selection::Agent(pane_id) = &next_selection {
                        last_focused_agent_pane_id = Some(pane_id.clone());
                    }
                    selection = next_selection;
                    needs_render = true;
                }
            }
            input::InputEvent::MouseScrollUp if is_active => {
                scroll_offset = scroll_offset.saturating_sub(1);
                needs_render = true;
            }
            input::InputEvent::MouseScrollDown if is_active => {
                let max_offset = cached_agents.len().saturating_sub(
                    render::visible_item_count_opts(
                        height,
                        &cached_agents,
                        scroll_offset,
                        header_expanded,
                        sidebar_config.compact_mode,
                        sidebar_config.item_separator,
                    )
                    .max(1),
                );
                scroll_offset = (scroll_offset + 1).min(max_offset);
                needs_render = true;
            }
            input::InputEvent::KeyEnter if is_active => match &selection {
                Selection::Header => {
                    header_expanded = !header_expanded;
                    header_user_toggled = true;
                    last_rendered_stats = None;
                    needs_render = true;
                }
                Selection::Agent(pane_id) => {
                    if let Some(agent) =
                        cached_agents.iter().find(|agent| agent.pane_id == *pane_id)
                    {
                        activate_agent(agent, &mut unseen_done, &mut recently_acked);
                    }
                }
                Selection::None => {}
            },
            input::InputEvent::MouseClick { y } => {
                let hrows = render::header_rows(header_expanded);
                if y > 0 && y <= hrows {
                    header_expanded = !header_expanded;
                    header_user_toggled = true;
                    last_rendered_stats = None;
                    selection = Selection::Header;
                    cached_focus_is_active = true;
                    if !sidebar_pane_id.is_empty() {
                        cached_active_pane_id = Some(sidebar_pane_id.clone());
                        tmux::select_pane(&sidebar_pane_id);
                    }
                    needs_render = true;
                } else if let Some(agent) = input::click_to_agent_index(
                    y,
                    &cached_agents,
                    scroll_offset,
                    hrows,
                    sidebar_config.compact_mode,
                    sidebar_config.item_separator,
                )
                .and_then(|idx| cached_agents.get(idx))
                {
                    selection = Selection::Agent(agent.pane_id.clone());
                    // Eagerly update cached focus so sync_selection_from_focus
                    // sees the correct active pane on the very next iteration,
                    // rather than waiting up to FOCUS_POLL_MS for the real poll.
                    if agent.window_id == sidebar_window_id {
                        cached_active_pane_id = Some(agent.pane_id.clone());
                    } else {
                        cached_focus_is_active = false;
                        cached_active_pane_id = None;
                    }
                    activate_agent(agent, &mut unseen_done, &mut recently_acked);
                    needs_render = true;
                }
            }
            input::InputEvent::KeyQuit if is_active => {
                suppress_on_exit = true;
                break;
            }
            input::InputEvent::Resize => {
                needs_render = true;
            }
            input::InputEvent::KeyUp
            | input::InputEvent::KeyDown
            | input::InputEvent::KeyEnter
            | input::InputEvent::KeyQuit
            | input::InputEvent::MouseScrollUp
            | input::InputEvent::MouseScrollDown
            | input::InputEvent::None => {}
        }
    }

    if suppress_on_exit && let Some(window_id) = tmux::current_window_id() {
        if tmux::window_pane_count(&window_id) <= 1 {
            tmux::kill_window(&window_id);
        } else {
            tmux::suppress_window(&window_id);
        }
    }

    if let SidebarRole::Leader { epoch, .. } = role {
        runtime_store.release_leader(epoch);
    }
}

#[allow(clippy::too_many_arguments)]
fn activate_sidebar(
    role: &mut SidebarRole,
    runtime_store: &mut RuntimeStore,
    cached_agents: &mut Vec<AgentInfo>,
    current_stats: &mut AggregatedStats,
    prev_states: &mut HashMap<String, AgentState>,
    unseen_done: &mut HashSet<String>,
    recently_acked: &mut HashSet<String>,
    active_window_id: Option<&str>,
) {
    let mut snapshot_loaded = false;
    let lease = runtime_store.read_lease();
    if let Some(snapshot) = runtime_store.load_snapshot_for_activation() {
        apply_snapshot(
            snapshot,
            cached_agents,
            current_stats,
            prev_states,
            unseen_done,
            recently_acked,
            active_window_id,
        );
        snapshot_loaded = true;
    }

    if runtime_store.lease_is_stale(&lease)
        && let Some(epoch) = runtime_store.try_claim_leader()
    {
        *role = SidebarRole::Leader {
            epoch,
            last_refresh: Instant::now() - Duration::from_millis(runtime::POLL_INTERVAL_MS),
        };
        return;
    }

    *role = SidebarRole::Follower {
        last_poll: if snapshot_loaded {
            Instant::now()
        } else {
            Instant::now() - Duration::from_millis(runtime::POLL_INTERVAL_MS)
        },
    };
}

#[allow(clippy::too_many_arguments)]
fn refresh_leader_state(
    session: &str,
    epoch: u64,
    runtime_store: &mut RuntimeStore,
    history: &mut HistoryStore,
    detect_cache: &mut detect::SessionCache,
    cached_agents: &mut Vec<AgentInfo>,
    current_stats: &mut AggregatedStats,
    prev_states: &mut HashMap<String, AgentState>,
    unseen_done: &mut HashSet<String>,
    recently_acked: &mut HashSet<String>,
    pane_fingerprints: &mut HashMap<String, PaneFingerprint>,
    last_discovery_sweep: &mut Instant,
    active_window_id: Option<&str>,
) -> bool {
    if !runtime_store.heartbeat_leader(epoch) {
        return false;
    }

    history.refresh_persistent_baseline();
    let panes = tmux::list_session_panes(session);
    let tracked_panes: HashSet<&str> = cached_agents
        .iter()
        .map(|agent| agent.pane_id.as_str())
        .collect();
    let force_sweep = last_discovery_sweep.elapsed() >= Duration::from_millis(DISCOVERY_SWEEP_MS);
    let suspect_ids = suspect_pane_ids(&panes, pane_fingerprints, &tracked_panes, force_sweep);

    let mut agents = if let Some(agents) =
        detect::refresh_agents_incremental_from_panes(&panes, cached_agents, detect_cache)
    {
        agents
    } else {
        *last_discovery_sweep = Instant::now();
        let agents = detect::scan_agents(session, detect_cache);
        update_pane_fingerprints(pane_fingerprints, &panes);
        let stats = history.aggregated_stats(&agents);
        runtime_store.publish_snapshot(epoch, &agents, &stats);
        apply_agents_update(prev_states, unseen_done, recently_acked, &agents, active_window_id);
        *cached_agents = agents;
        *current_stats = stats;
        return true;
    };

    if !suspect_ids.is_empty() {
        let suspect_panes: Vec<_> = panes
            .iter()
            .filter(|pane| suspect_ids.contains(&pane.id))
            .cloned()
            .collect();
        let discovered = detect::discover_agents_in_panes(session, &suspect_panes, detect_cache);
        if !discovered.is_empty() || force_sweep {
            *last_discovery_sweep = Instant::now();
        }
        for discovered_agent in discovered {
            if let Some(existing) = agents
                .iter_mut()
                .find(|agent| agent.pane_id == discovered_agent.pane_id)
            {
                *existing = discovered_agent;
            } else {
                agents.push(discovered_agent);
            }
        }
        agents.sort_by_key(|agent| agent.process_elapsed_secs);
    }

    detect_cache.retain_agent_infos(&agents);
    update_pane_fingerprints(pane_fingerprints, &panes);
    let stats = history.aggregated_stats(&agents);
    runtime_store.publish_snapshot(epoch, &agents, &stats);
    apply_agents_update(prev_states, unseen_done, recently_acked, &agents, active_window_id);
    *cached_agents = agents;
    *current_stats = stats;
    true
}

fn apply_snapshot(
    snapshot: LiveSnapshot,
    cached_agents: &mut Vec<AgentInfo>,
    current_stats: &mut AggregatedStats,
    prev_states: &mut HashMap<String, AgentState>,
    unseen_done: &mut HashSet<String>,
    recently_acked: &mut HashSet<String>,
    active_window_id: Option<&str>,
) {
    apply_agents_update(prev_states, unseen_done, recently_acked, &snapshot.agents, active_window_id);
    *cached_agents = snapshot.agents;
    *current_stats = snapshot.stats;
}

fn apply_agents_update(
    prev_states: &mut HashMap<String, AgentState>,
    unseen_done: &mut HashSet<String>,
    recently_acked: &mut HashSet<String>,
    agents: &[AgentInfo],
    active_window_id: Option<&str>,
) {
    for agent in agents {
        let prev = prev_states.get(&agent.pane_id).copied();
        match (prev, agent.state) {
            (Some(AgentState::Working), AgentState::Idle) => {
                // Suppress if: user just acked this agent, OR it's in the currently active window.
                let acked = recently_acked.remove(&agent.pane_id);
                let in_active_window = active_window_id.is_some_and(|wid| agent.window_id == wid);
                if !acked && !in_active_window {
                    unseen_done.insert(agent.pane_id.clone());
                }
            }
            (_, AgentState::Working) => {
                // Agent is working again — clear any stale done-badge.
                unseen_done.remove(&agent.pane_id);
            }
            _ => {}
        }
        prev_states.insert(agent.pane_id.clone(), agent.state);
    }

    let current_ids: HashSet<&str> = agents.iter().map(|agent| agent.pane_id.as_str()).collect();
    prev_states.retain(|pane_id, _| current_ids.contains(pane_id.as_str()));
    unseen_done.retain(|pane_id| current_ids.contains(pane_id.as_str()));
    recently_acked.retain(|pane_id| current_ids.contains(pane_id.as_str()));

    if let Some(active_wid) = active_window_id {
        for agent in agents {
            if agent.window_id == active_wid {
                unseen_done.remove(&agent.pane_id);
            }
        }
    }
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn terminal_size() -> (u32, u32) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            (ws.ws_col as u32, ws.ws_row as u32)
        } else {
            (30, 24)
        }
    }
}

struct TerminalGuard {
    orig: libc::termios,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
        input::disable_mouse();
        print!("\x1b[?25h");
        flush();
    }
}

fn terminal_setup() -> TerminalGuard {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut orig);
        let mut raw = orig;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        input::enable_mouse();
        print!("\x1b[?25l");
        flush();
        TerminalGuard { orig }
    }
}

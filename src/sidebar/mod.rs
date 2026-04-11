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
const WIDTH_SAVE_THROTTLE_MS: u64 = 50;
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
    title: String,
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
        title: pane.title.clone(),
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

fn sync_selection_from_focus(
    selection: &mut Selection,
    is_active_window: bool,
    active_pane_id: Option<&str>,
    sidebar_pane_id: &str,
    agents: &[AgentInfo],
) -> bool {
    let next = if !is_active_window {
        Selection::None
    } else if let Some(pane_id) = active_pane_id {
        if agents.iter().any(|agent| agent.pane_id == pane_id) {
            Selection::Agent(pane_id.to_string())
        } else if pane_id == sidebar_pane_id {
            match selection {
                Selection::Header => Selection::Header,
                Selection::Agent(selected_pane)
                    if agents.iter().any(|agent| agent.pane_id == *selected_pane) =>
                {
                    Selection::Agent(selected_pane.clone())
                }
                Selection::None | Selection::Agent(_) => Selection::None,
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

fn activate_agent(agent: &AgentInfo, unseen_done: &mut HashSet<String>) {
    unseen_done.remove(&agent.pane_id);
    let in_current_window = tmux::current_window_id().is_some_and(|cw| cw == agent.window_id);
    if !in_current_window {
        tmux::select_window(&agent.window_id);
    }
    tmux::select_pane(&agent.pane_id);
}

#[cfg(test)]
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
    fn sync_selection_tracks_focused_agent_pane() {
        let mut selection = Selection::None;
        let agents = vec![agent("%2"), agent("%3")];

        let changed =
            sync_selection_from_focus(&mut selection, true, Some("%3"), "%sidebar", &agents);

        assert!(changed);
        assert_eq!(selection, Selection::Agent("%3".to_string()));
    }

    #[test]
    fn sync_selection_clears_when_focus_leaves_sidebar_and_agents() {
        let mut selection = Selection::Agent("%2".to_string());
        let agents = vec![agent("%2"), agent("%3")];

        let changed =
            sync_selection_from_focus(&mut selection, true, Some("%9"), "%sidebar", &agents);

        assert!(changed);
        assert_eq!(selection, Selection::None);
    }

    #[test]
    fn sync_selection_clears_header_when_sidebar_window_loses_focus() {
        let mut selection = Selection::Header;

        let changed =
            sync_selection_from_focus(&mut selection, false, Some("%sidebar"), "%sidebar", &[]);

        assert!(changed);
        assert_eq!(selection, Selection::None);
    }

    #[test]
    fn sync_selection_preserves_sidebar_local_agent_selection() {
        let mut selection = Selection::Agent("%2".to_string());
        let agents = vec![agent("%2"), agent("%3")];

        let changed =
            sync_selection_from_focus(&mut selection, true, Some("%sidebar"), "%sidebar", &agents);

        assert!(!changed);
        assert_eq!(selection, Selection::Agent("%2".to_string()));
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
    let mut cached_agents = detect::scan_agents_fast(&session);
    let mut current_stats = AggregatedStats::default();
    let mut role = SidebarRole::Inactive;
    let mut runtime_store = RuntimeStore::new(&session);
    let mut history = HistoryStore::start();
    let mut detect_cache = detect::SessionCache::new();
    let sidebar_pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
    let sidebar_window_id = tmux::pane_window_id(&sidebar_pane_id)
        .or_else(tmux::current_window_id)
        .unwrap_or_default();
    let mut last_active = false;
    let mut selection = Selection::None;
    let mut scroll_offset = 0usize;
    let mut last_width = 0u32;
    let mut last_width_save = Instant::now();
    let mut needs_render = true;
    let mut just_activated = false;
    let mut suppress_on_exit = false;
    let mut pane_fingerprints: HashMap<String, PaneFingerprint> = HashMap::new();
    let mut last_discovery_sweep = Instant::now() - Duration::from_millis(DISCOVERY_SWEEP_MS);

    let header_config = load_header_config();
    let mut header_expanded = header_config.start_mode == HeaderMode::Expanded;
    let mut header_user_toggled = false;
    let start_time = Instant::now();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        let is_active = tmux::is_pane_in_active_window();
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
                );
                just_activated = true;
            }
            last_active = is_active;
            needs_render = true;
        }

        let active_pane_id = if is_active && !sidebar_window_id.is_empty() {
            tmux::active_pane_in_window(&sidebar_window_id)
        } else {
            None
        };

        if sync_selection_from_focus(
            &mut selection,
            is_active,
            active_pane_id.as_deref(),
            &sidebar_pane_id,
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
                        &mut pane_fingerprints,
                        &mut last_discovery_sweep,
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
                    &mut pane_fingerprints,
                    &mut last_discovery_sweep,
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
            needs_render = true;
        }

        let header_selected = matches!(selection, Selection::Header);
        let selected_idx = selection_index(&selection, &cached_agents);

        let (_, height) = terminal_size();
        if let Some(selected_idx) = selected_idx {
            let visible =
                render::visible_item_count(height, &cached_agents, scroll_offset, header_expanded);
            if visible > 0 {
                if selected_idx < scroll_offset {
                    scroll_offset = selected_idx;
                } else if selected_idx >= scroll_offset + visible {
                    scroll_offset = selected_idx + 1 - visible;
                }
            }
        }

        if needs_render {
            let (mut width, height) = terminal_size();
            if width < tmux::MIN_WIDTH {
                tmux::resize_pane_width(tmux::MIN_WIDTH);
                width = tmux::MIN_WIDTH;
            }
            if last_width != 0
                && width != last_width
                && last_width_save.elapsed() >= Duration::from_millis(WIDTH_SAVE_THROTTLE_MS)
            {
                tmux::save_sidebar_width(&session, width);
                last_width_save = Instant::now();
            }
            last_width = width;
            print!(
                "{}",
                render::render_sidebar(
                    &cached_agents,
                    width,
                    height,
                    selected_idx,
                    scroll_offset,
                    &unseen_done,
                    &current_stats,
                    header_expanded,
                    header_selected,
                )
            );
            flush();
            needs_render = false;
        }

        match input::poll_input(Duration::from_millis(INPUT_POLL_MS)) {
            input::InputEvent::KeyUp if is_active => {
                let next_selection = move_selection_up(&selection, &cached_agents);
                if next_selection != selection {
                    selection = next_selection;
                    needs_render = true;
                }
            }
            input::InputEvent::KeyDown if is_active => {
                let next_selection = move_selection_down(&selection, &cached_agents);
                if next_selection != selection {
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
                    render::visible_item_count(
                        height,
                        &cached_agents,
                        scroll_offset,
                        header_expanded,
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
                    needs_render = true;
                }
                Selection::Agent(pane_id) => {
                    if let Some(agent) =
                        cached_agents.iter().find(|agent| agent.pane_id == *pane_id)
                    {
                        activate_agent(agent, &mut unseen_done);
                    }
                }
                Selection::None => {}
            },
            input::InputEvent::MouseClick { y } => {
                let hrows = render::header_rows(header_expanded);
                if y >= 4 && y <= hrows {
                    header_expanded = !header_expanded;
                    header_user_toggled = true;
                    selection = Selection::Header;
                    needs_render = true;
                } else if let Some(agent) =
                    input::click_to_agent_index(y, &cached_agents, scroll_offset, hrows)
                        .and_then(|idx| cached_agents.get(idx))
                {
                    selection = Selection::Agent(agent.pane_id.clone());
                    activate_agent(agent, &mut unseen_done);
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
        tmux::suppress_window(&window_id);
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
    pane_fingerprints: &mut HashMap<String, PaneFingerprint>,
    last_discovery_sweep: &mut Instant,
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
        detect::refresh_agents_incremental_from_panes(session, &panes, cached_agents, detect_cache)
    {
        agents
    } else {
        *last_discovery_sweep = Instant::now();
        let agents = detect::scan_agents(session, detect_cache);
        update_pane_fingerprints(pane_fingerprints, &panes);
        let stats = history.aggregated_stats(&agents);
        runtime_store.publish_snapshot(epoch, &agents, &stats);
        apply_agents_update(prev_states, unseen_done, &agents);
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
        agents.sort_by_key(|agent| agent.elapsed_secs);
    }

    detect_cache.retain_agent_infos(&agents);
    update_pane_fingerprints(pane_fingerprints, &panes);
    let stats = history.aggregated_stats(&agents);
    runtime_store.publish_snapshot(epoch, &agents, &stats);
    apply_agents_update(prev_states, unseen_done, &agents);
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
) {
    apply_agents_update(prev_states, unseen_done, &snapshot.agents);
    *cached_agents = snapshot.agents;
    *current_stats = snapshot.stats;
}

fn apply_agents_update(
    prev_states: &mut HashMap<String, AgentState>,
    unseen_done: &mut HashSet<String>,
    agents: &[AgentInfo],
) {
    for agent in agents {
        if prev_states
            .get(&agent.pane_id)
            .is_some_and(|&prev| prev == AgentState::Working && agent.state == AgentState::Idle)
        {
            unseen_done.insert(agent.pane_id.clone());
        }
        prev_states.insert(agent.pane_id.clone(), agent.state);
    }

    let current_ids: HashSet<&str> = agents.iter().map(|agent| agent.pane_id.as_str()).collect();
    prev_states.retain(|pane_id, _| current_ids.contains(pane_id.as_str()));
    unseen_done.retain(|pane_id| current_ids.contains(pane_id.as_str()));

    if let Some(current_window) = tmux::current_window_id() {
        for agent in agents {
            if agent.window_id == current_window {
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

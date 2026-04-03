pub mod input;
pub mod render;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::detect;
use crate::detect::AgentInfo;
use crate::detect::history::HistoryStore;
use crate::detect::state::AgentState;
use crate::tmux;

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

extern "C" fn exit_handler(_sig: libc::c_int) {
    SHOULD_EXIT.store(true, Ordering::Relaxed);
}

// No-op handler: installing it causes poll() to return EINTR on SIGWINCH,
// which triggers immediate re-render on resize.
extern "C" fn winch_handler(_sig: libc::c_int) {}

const SCAN_INTERVAL_MS: u64 = 3000;
const IDLE_CHECK_MS: u64 = 1000;
const WIDTH_SAVE_THROTTLE_MS: u64 = 50;

// --- Header config ---

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
        cfg.start_mode = if v.trim_matches('"') == "collapsed" {
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
        libc::signal(libc::SIGWINCH, winch_handler as *const () as libc::sighandler_t);
    }

    let _guard = terminal_setup();

    let mut prev_states: HashMap<String, AgentState> = HashMap::new();
    let mut unseen_done: HashSet<String> = HashSet::new();
    let mut session_cache = detect::SessionCache::new();
    let mut cached_agents: Vec<AgentInfo> = Vec::new();
    let mut last_selected_pane = String::new();
    let mut scroll_offset: usize = 0;
    let mut last_width: u32 = 0;
    let scan_interval = Duration::from_millis(SCAN_INTERVAL_MS);
    let mut last_scan = Instant::now()
        .checked_sub(scan_interval)
        .unwrap_or_else(Instant::now);
    let mut last_width_save = Instant::now();
    let mut needs_render = true;

    // Header expand/collapse state
    let header_config = load_header_config();
    let mut header_expanded = header_config.start_mode == HeaderMode::Expanded;
    let mut header_user_toggled = false;
    let mut header_selected = false;
    let start_time = Instant::now();

    let history = HistoryStore::start();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        let is_active = tmux::is_pane_in_active_window();
        let selected_pane = tmux::get_selected_pane();
        let selection_changed = selected_pane != last_selected_pane;
        last_selected_pane.clone_from(&selected_pane);
        if selection_changed {
            header_selected = false;
            needs_render = true;
        }

        if is_active && last_scan.elapsed() >= scan_interval {
            let agents = detect::scan_agents(&session, &mut session_cache);

            for agent in &agents {
                if prev_states.get(&agent.pane_id).is_some_and(|&prev| {
                    prev == AgentState::Working && agent.state == AgentState::Idle
                }) {
                    unseen_done.insert(agent.pane_id.clone());
                }
                prev_states.insert(agent.pane_id.clone(), agent.state);
            }
            let current_ids: HashSet<&str> = agents.iter().map(|a| a.pane_id.as_str()).collect();
            prev_states.retain(|k, _| current_ids.contains(k.as_str()));
            unseen_done.retain(|k| current_ids.contains(k.as_str()));

            if let Some(current_window) = tmux::current_window_id() {
                for agent in &agents {
                    if agent.window_id == current_window {
                        unseen_done.remove(&agent.pane_id);
                    }
                }
            }

            cached_agents = agents;
            last_scan = Instant::now();
            needs_render = true;
        }

        // Auto-collapse timer
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

        let selected_idx = if header_selected {
            0 // doesn't matter for agent selection when header is focused
        } else {
            cached_agents
                .iter()
                .position(|a| a.pane_id == selected_pane)
                .unwrap_or(0)
        };

        // Auto-scroll to keep selection visible
        let (_, height) = terminal_size();
        if !header_selected {
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
            // Enforce minimum width
            if width < tmux::MIN_WIDTH {
                tmux::resize_pane_width(tmux::MIN_WIDTH);
                width = tmux::MIN_WIDTH;
            }
            // Detect manual resize and sync to all sidebars (throttled)
            if last_width != 0
                && width != last_width
                && last_width_save.elapsed() >= Duration::from_millis(WIDTH_SAVE_THROTTLE_MS)
            {
                tmux::save_sidebar_width(&session, width);
                last_width_save = Instant::now();
            }
            last_width = width;
            let stats = history.aggregated_stats();
            print!(
                "{}",
                render::render_sidebar(
                    &cached_agents,
                    width,
                    height,
                    selected_idx,
                    scroll_offset,
                    &unseen_done,
                    &stats,
                    header_expanded,
                    header_selected,
                )
            );
            flush();
            needs_render = false;
        }

        if is_active {
            let elapsed_ms = last_scan.elapsed().as_millis() as u64;
            let remaining = SCAN_INTERVAL_MS.saturating_sub(elapsed_ms).max(50);
            match input::poll_input(Duration::from_millis(remaining)) {
                input::InputEvent::KeyUp => {
                    if header_selected {
                        // Already at top, no-op
                    } else if selected_idx == 0 {
                        // Move from first agent to header
                        header_selected = true;
                    } else {
                        let new_sel = selected_idx.saturating_sub(1);
                        set_selection(&cached_agents, new_sel);
                    }
                    needs_render = true;
                }
                input::InputEvent::KeyDown => {
                    if header_selected {
                        // Move from header to first agent
                        header_selected = false;
                        set_selection(&cached_agents, 0);
                    } else {
                        let max = cached_agents.len().saturating_sub(1);
                        let new_sel = if selected_idx < max {
                            selected_idx + 1
                        } else {
                            max
                        };
                        set_selection(&cached_agents, new_sel);
                    }
                    needs_render = true;
                }
                input::InputEvent::MouseScrollUp => {
                    scroll_offset = scroll_offset.saturating_sub(1);
                    needs_render = true;
                }
                input::InputEvent::MouseScrollDown => {
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
                input::InputEvent::KeyEnter => {
                    if header_selected {
                        header_expanded = !header_expanded;
                        header_user_toggled = true;
                        needs_render = true;
                    } else if let Some(agent) = cached_agents.get(selected_idx) {
                        unseen_done.remove(&agent.pane_id);
                        let in_current_window = tmux::current_window_id()
                            .is_some_and(|cw| cw == agent.window_id);
                        if !in_current_window {
                            tmux::select_window(&agent.window_id);
                            tmux::select_pane(&agent.pane_id);
                        }
                    }
                }
                input::InputEvent::MouseClick { y } => {
                    let hrows = render::header_rows(header_expanded);
                    if y >= 4 && y <= hrows {
                        // Click on header stats area (rows 4..=hrows are the table)
                        header_expanded = !header_expanded;
                        header_user_toggled = true;
                        header_selected = true;
                        needs_render = true;
                    } else if let Some(agent) =
                        input::click_to_agent_index(y, &cached_agents, scroll_offset, hrows)
                            .and_then(|idx| cached_agents.get(idx))
                    {
                        header_selected = false;
                        tmux::set_selected_pane(&agent.pane_id);
                        unseen_done.remove(&agent.pane_id);
                        let in_current_window = tmux::current_window_id()
                            .is_some_and(|cw| cw == agent.window_id);
                        if !in_current_window {
                            tmux::select_window(&agent.window_id);
                            tmux::select_pane(&agent.pane_id);
                        }
                        needs_render = true;
                    }
                }
                input::InputEvent::KeyQuit => break,
                input::InputEvent::Resize => {
                    needs_render = true;
                }
                input::InputEvent::None => {}
            }
        } else {
            let timeout = Duration::from_millis(IDLE_CHECK_MS);
            let _ = input::poll_input(timeout);
        }
    }
}

fn set_selection(agents: &[AgentInfo], idx: usize) {
    if let Some(agent) = agents.get(idx) {
        tmux::set_selected_pane(&agent.pane_id);
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

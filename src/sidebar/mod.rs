pub mod input;
pub mod render;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::detect;
use crate::detect::state::AgentState;
use crate::detect::AgentInfo;
use crate::tmux;

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

extern "C" fn exit_handler(_sig: libc::c_int) {
    SHOULD_EXIT.store(true, Ordering::Relaxed);
}

const SCAN_INTERVAL_MS: u64 = 3000;
const IDLE_CHECK_MS: u64 = 1000;

pub fn run() {
    let session = tmux::current_session().expect("not running inside tmux");

    unsafe {
        let exit_h = exit_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, exit_h);
        libc::signal(libc::SIGINT, exit_h);
        libc::signal(libc::SIGHUP, exit_h);
    }

    let _guard = terminal_setup();

    let mut prev_states: HashMap<String, AgentState> = HashMap::new();
    let mut unseen_done: HashSet<String> = HashSet::new();
    let mut session_cache = detect::SessionCache::new();
    let mut cached_agents: Vec<AgentInfo> = Vec::new();
    let mut last_selected_pane = String::new();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        let is_active = tmux::is_pane_in_active_window();
        let selected_pane = tmux::get_selected_pane();
        let selection_changed = selected_pane != last_selected_pane;
        last_selected_pane.clone_from(&selected_pane);

        if is_active {
            let agents = detect::scan_agents(&session, &mut session_cache);

            for agent in &agents {
                if prev_states
                    .get(&agent.pane_id)
                    .is_some_and(|&prev| prev == AgentState::Working && agent.state == AgentState::Idle)
                {
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
        }

        // Resolve selected index once — reused for render and input handling
        let selected_idx = cached_agents
            .iter()
            .position(|a| a.pane_id == selected_pane)
            .unwrap_or(0);

        if is_active || selection_changed {
            let (width, height) = terminal_size();
            print!(
                "{}",
                render::render_sidebar(&cached_agents, width, height, selected_idx, &unseen_done)
            );
            flush();
        }

        if is_active {
            let timeout = std::time::Duration::from_millis(SCAN_INTERVAL_MS);
            match input::poll_input(timeout) {
                input::InputEvent::KeyUp => {
                    let new_sel = selected_idx.saturating_sub(1);
                    set_selection(&cached_agents, new_sel);
                }
                input::InputEvent::KeyDown => {
                    let max = cached_agents.len().saturating_sub(1);
                    set_selection(&cached_agents, selected_idx.min(max - 1) + 1);
                }
                input::InputEvent::KeyEnter => {
                    if let Some(agent) = cached_agents.get(selected_idx) {
                        unseen_done.remove(&agent.pane_id);
                        tmux::select_window(&agent.window_id);
                        tmux::select_pane(&agent.pane_id);
                    }
                }
                input::InputEvent::MouseClick { y } => {
                    if let Some(agent) =
                        input::click_to_agent_index(y, cached_agents.len(), selected_idx)
                            .and_then(|idx| cached_agents.get(idx))
                    {
                        tmux::set_selected_pane(&agent.pane_id);
                        unseen_done.remove(&agent.pane_id);
                        tmux::select_window(&agent.window_id);
                        tmux::select_pane(&agent.pane_id);
                    }
                }
                input::InputEvent::KeyQuit => break,
                input::InputEvent::Resize | input::InputEvent::None => {}
            }
        } else {
            let timeout = std::time::Duration::from_millis(IDLE_CHECK_MS);
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

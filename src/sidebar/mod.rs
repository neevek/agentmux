pub mod input;
pub mod render;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::detect;
use crate::detect::state::AgentState;
use crate::detect::AgentInfo;
use crate::tmux;

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
static NEED_RESIZE: AtomicBool = AtomicBool::new(false);

extern "C" fn exit_handler(_sig: libc::c_int) {
    SHOULD_EXIT.store(true, Ordering::Relaxed);
}

extern "C" fn winch_handler(_sig: libc::c_int) {
    NEED_RESIZE.store(true, Ordering::Relaxed);
}

const POLL_SECS: u64 = 3;

pub fn run() {
    let session = tmux::current_session().expect("not running inside tmux");

    unsafe {
        let exit_h = exit_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, exit_h);
        libc::signal(libc::SIGINT, exit_h);
        libc::signal(libc::SIGHUP, exit_h);
        let winch_h = winch_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGWINCH, winch_h);
    }

    let _guard = terminal_setup();

    let mut selected: usize = 0;
    let mut prev_states: HashMap<String, AgentState> = HashMap::new();
    let mut unseen_done: HashSet<String> = HashSet::new();
    let mut session_cache = detect::SessionCache::new();
    let mut cached_agents: Vec<AgentInfo> = Vec::new();
    let mut current_window = tmux::current_window_id().unwrap_or_default();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        // Consume any pending resize
        NEED_RESIZE.store(false, Ordering::Relaxed);

        let is_active = tmux::is_pane_in_active_window();

        let agents = if is_active {
            // Active window: full scan
            let agents = detect::scan_agents(&session, &mut session_cache);

            // Update notification badges
            for agent in &agents {
                if let Some(&prev) = prev_states.get(&agent.pane_id) {
                    if prev == AgentState::Working && agent.state == AgentState::Idle {
                        unseen_done.insert(agent.pane_id.clone());
                    }
                }
                prev_states.insert(agent.pane_id.clone(), agent.state);
            }
            let current_ids: HashSet<&str> =
                agents.iter().map(|a| a.pane_id.as_str()).collect();
            prev_states.retain(|k, _| current_ids.contains(k.as_str()));
            unseen_done.retain(|k| current_ids.contains(k.as_str()));

            // Clear badges for agents in the current window (user can see them)
            for agent in &agents {
                if agent.window_id == current_window {
                    unseen_done.remove(&agent.pane_id);
                }
            }

            cached_agents = agents;
            &cached_agents
        } else {
            // Inactive window: reuse cached state, no scanning
            &cached_agents
        };

        if !agents.is_empty() && selected >= agents.len() {
            selected = agents.len() - 1;
        }

        let (width, height) = terminal_size();
        let output = render::render_sidebar(agents, width, height, selected, &unseen_done);
        print!("{output}");
        flush();

        if is_active {
            current_window = tmux::current_window_id().unwrap_or_default();
        }

        let timeout = std::time::Duration::from_secs(POLL_SECS);
        match input::poll_input(timeout) {
            input::InputEvent::KeyUp => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            input::InputEvent::KeyDown => {
                if !agents.is_empty() && selected < agents.len() - 1 {
                    selected += 1;
                }
            }
            input::InputEvent::KeyEnter => {
                if let Some(agent) = agents.get(selected) {
                    unseen_done.remove(&agent.pane_id);
                    tmux::select_window(&agent.window_id);
                    tmux::select_pane(&agent.pane_id);
                }
            }
            input::InputEvent::MouseClick { y } => {
                if let Some(idx) = input::click_to_agent_index(y, agents.len(), selected) {
                    if let Some(agent) = agents.get(idx) {
                        selected = idx;
                        unseen_done.remove(&agent.pane_id);
                        tmux::select_window(&agent.window_id);
                        tmux::select_pane(&agent.pane_id);
                    }
                }
            }
            input::InputEvent::KeyQuit => break,
            input::InputEvent::Resize => {
                // SIGWINCH often fires on window switch — loop immediately to
                // re-check active state and refresh if we just became active
            }
            input::InputEvent::None => {}
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

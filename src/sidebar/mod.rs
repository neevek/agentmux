pub mod input;
pub mod render;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::detect;
use crate::detect::state::AgentState;
use crate::tmux;

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
static NEED_RESIZE: AtomicBool = AtomicBool::new(false);

extern "C" fn exit_handler(_sig: libc::c_int) {
    SHOULD_EXIT.store(true, Ordering::Relaxed);
}

extern "C" fn winch_handler(_sig: libc::c_int) {
    NEED_RESIZE.store(true, Ordering::Relaxed);
}

pub fn run() {
    let session = tmux::current_session().expect("not running inside tmux");

    // Install signal handlers
    unsafe {
        let exit_h = exit_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, exit_h);
        libc::signal(libc::SIGINT, exit_h);
        let winch_h = winch_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGWINCH, winch_h);
    }

    let _guard = terminal_setup();

    let mut selected: usize = 0;
    let mut width: u32;
    let mut height: u32;

    // Notification tracking: agents that finished while not selected
    let mut prev_states: HashMap<String, AgentState> = HashMap::new();
    let mut unseen_done: HashSet<String> = HashSet::new();

    (width, height) = terminal_size();

    loop {
        if SHOULD_EXIT.load(Ordering::Relaxed) {
            break;
        }

        if NEED_RESIZE.swap(false, Ordering::Relaxed) {
            // Drain any queued SIGWINCH — only the final size matters
            std::thread::sleep(std::time::Duration::from_millis(50));
            NEED_RESIZE.store(false, Ordering::Relaxed);
            (width, height) = terminal_size();
            // This sidebar was resized by the user — save as the new target width
            tmux::save_sidebar_width(width);
        } else {
            // Check if another sidebar was resized — sync our width to match
            let target_width = tmux::get_sidebar_width();
            if target_width != width {
                if let Ok(pane) = std::env::var("TMUX_PANE") {
                    tmux::resize_pane(&pane, target_width);
                }
                // Swallow the SIGWINCH triggered by our own resize_pane call
                // so it doesn't get treated as a user-initiated resize next loop
                std::thread::sleep(std::time::Duration::from_millis(50));
                NEED_RESIZE.store(false, Ordering::Relaxed);
                (width, height) = terminal_size();
            }
        }

        let agents = detect::scan_agents(&session);

        // Update notification badges
        for agent in &agents {
            if let Some(&prev) = prev_states.get(&agent.pane_id) {
                if prev == AgentState::Working && agent.state == AgentState::Idle {
                    // Agent just finished — mark as unseen
                    unseen_done.insert(agent.pane_id.clone());
                }
            }
            prev_states.insert(agent.pane_id.clone(), agent.state);
        }
        // Clean stale entries from prev_states
        let current_ids: HashSet<&str> = agents.iter().map(|a| a.pane_id.as_str()).collect();
        prev_states.retain(|k, _| current_ids.contains(k.as_str()));
        unseen_done.retain(|k| current_ids.contains(k.as_str()));

        // Clamp selection
        if !agents.is_empty() && selected >= agents.len() {
            selected = agents.len() - 1;
        }

        let output = render::render_sidebar(&agents, width, height, selected, &unseen_done);
        print!("{output}");
        flush();

        let timeout = std::time::Duration::from_secs(2);
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
                    // Clear badge when navigating to this agent
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
                (width, height) = terminal_size();
                tmux::save_sidebar_width(width);
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

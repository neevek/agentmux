use std::process::Command;

const SIDEBAR_TITLE: &str = "tmux-agents-sidebar";
const DEFAULT_WIDTH: u32 = 30;
const WIDTH_OPTION: &str = "@tmux-agents-width";

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PaneInfo {
    pub id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub pid: u32,
    pub cwd: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

const PANE_FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_pid}\t#{pane_current_path}\t#{pane_title}\t#{pane_width}\t#{pane_height}";

fn parse_pane_line(line: &str) -> Option<PaneInfo> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 9 {
        return None;
    }
    Some(PaneInfo {
        id: parts[0].to_string(),
        session_name: parts[1].to_string(),
        window_id: parts[2].to_string(),
        window_index: parts[3].parse().unwrap_or(0),
        pid: parts[4].parse().unwrap_or(0),
        cwd: parts[5].to_string(),
        title: parts[6].to_string(),
        width: parts[7].parse().unwrap_or(0),
        height: parts[8].parse().unwrap_or(0),
    })
}

fn tmux_output(args: &[&str]) -> Option<String> {
    let output = Command::new("tmux").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the saved sidebar width, or default.
pub fn get_sidebar_width() -> u32 {
    tmux_output(&["show-option", "-gqv", WIDTH_OPTION])
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_WIDTH)
}

/// Save the sidebar width as a tmux global option.
pub fn save_sidebar_width(width: u32) {
    let _ = tmux_output(&[
        "set-option",
        "-g",
        WIDTH_OPTION,
        &width.to_string(),
    ]);
}

/// List all panes in a session.
pub fn list_session_panes(session: &str) -> Vec<PaneInfo> {
    let Some(out) = tmux_output(&["list-panes", "-s", "-t", session, "-F", PANE_FORMAT]) else {
        return Vec::new();
    };
    out.lines().filter_map(parse_pane_line).collect()
}

/// Find ALL sidebar panes across the session. Returns vec of (pane_id, window_id).
pub fn find_all_sidebar_panes(session: &str) -> Vec<(String, String)> {
    let panes = list_session_panes(session);
    panes
        .into_iter()
        .filter(|p| p.title == SIDEBAR_TITLE)
        .map(|p| (p.id, p.window_id))
        .collect()
}

/// Check if the sidebar pane exists in a specific window.
pub fn sidebar_in_window(window_id: &str) -> bool {
    let Some(out) = tmux_output(&["list-panes", "-t", window_id, "-F", "#{pane_title}"]) else {
        return false;
    };
    out.lines().any(|line| line == SIDEBAR_TITLE)
}

/// Check if this sidebar's window is the active window.
/// Uses $TMUX_PANE to identify our pane, then checks if its window is active.
pub fn is_pane_in_active_window() -> bool {
    let Some(pane_id) = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()) else {
        return true; // assume active if we can't determine
    };
    tmux_output(&[
        "display-message",
        "-t",
        &pane_id,
        "-p",
        "#{window_active}",
    ])
    .is_some_and(|s| s == "1")
}

/// List all window IDs in a session.
pub fn list_window_ids(session: &str) -> Vec<String> {
    let Some(out) = tmux_output(&["list-windows", "-t", session, "-F", "#{window_id}"]) else {
        return Vec::new();
    };
    out.lines().map(|s| s.to_string()).collect()
}

/// Map of window_id → window_name for all windows in a session.
pub fn list_window_names(session: &str) -> std::collections::HashMap<String, String> {
    let Some(out) = tmux_output(&[
        "list-windows", "-t", session, "-F", "#{window_id}\t#{window_name}",
    ]) else {
        return std::collections::HashMap::new();
    };
    out.lines()
        .filter_map(|line| {
            let (id, name) = line.split_once('\t')?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

/// Create a sidebar split on the left side of a window.
/// For simple horizontal layouts: no `-f`, no squash, no flicker.
/// For complex layouts (no leftmost pane spans full height): uses `-f` for correct height.
pub fn create_sidebar_in(window_id: &str, cmd: &str) -> Option<String> {
    let sidebar_width = get_sidebar_width();
    let width_str = sidebar_width.to_string();

    let (target, use_full) = find_split_target(window_id)?;

    let mut args = vec![
        "split-window",
        "-hb",
        "-l",
        &width_str,
        "-t",
        &target,
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
    ];
    if use_full {
        args.insert(2, "-f");
    }
    args.push(cmd);
    tmux_output(&args)
}

/// Find the best pane to split for the sidebar.
/// Returns (pane_id, needs_full_split).
/// If any leftmost pane spans full window height, split it without -f.
/// Otherwise, use -f for full height (accepts proportional redistribution).
fn find_split_target(window_id: &str) -> Option<(String, bool)> {
    let fmt = "#{pane_id}\t#{pane_left}\t#{pane_top}\t#{pane_height}";
    let out = tmux_output(&["list-panes", "-t", window_id, "-F", fmt])?;
    let win_height: u32 = tmux_output(&[
        "display-message",
        "-t",
        window_id,
        "-p",
        "#{window_height}",
    ])
    .and_then(|s| s.parse().ok())
    .unwrap_or(0);

    let panes: Vec<(String, u32, u32, u32)> = out
        .lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split('\t').collect();
            if p.len() < 4 {
                return None;
            }
            Some((
                p[0].to_string(),
                p[1].parse().ok()?,
                p[2].parse().ok()?,
                p[3].parse().ok()?,
            ))
        })
        .collect();

    // Look for a leftmost pane (left == 0) that spans full height
    let full_height_left = panes.iter().find(|(_, left, top, height)| {
        *left == 0 && *top == 0 && (*height + 1 >= win_height || win_height == 0)
    });

    if let Some((id, _, _, _)) = full_height_left {
        Some((id.clone(), false)) // no -f needed
    } else {
        // No single pane spans full height — use -f, target first pane
        let first = panes.first()?;
        Some((first.0.clone(), true))
    }
}

/// Resize a pane to a specific width.
pub fn resize_pane(pane_id: &str, width: u32) {
    let _ = tmux_output(&["resize-pane", "-t", pane_id, "-x", &width.to_string()]);
}

/// Kill a pane by ID.
pub fn kill_pane(pane_id: &str) {
    let _ = tmux_output(&["kill-pane", "-t", pane_id]);
}

/// Focus a pane.
pub fn select_pane(pane_id: &str) {
    let _ = tmux_output(&["select-pane", "-t", pane_id]);
}

/// Switch to a window.
pub fn select_window(window_id: &str) {
    let _ = tmux_output(&["select-window", "-t", window_id]);
}

/// Set the title of a pane.
pub fn set_pane_title(pane_id: &str, title: &str) {
    let _ = tmux_output(&["select-pane", "-t", pane_id, "-T", title]);
}

/// Get the current session name.
pub fn current_session() -> Option<String> {
    tmux_output(&["display-message", "-p", "#{session_name}"])
}

/// Get the current window ID.
pub fn current_window_id() -> Option<String> {
    tmux_output(&["display-message", "-p", "#{window_id}"])
}

/// Get the current pane ID.
pub fn current_pane_id() -> Option<String> {
    if let Ok(pane) = std::env::var("TMUX_PANE") {
        if !pane.is_empty() {
            return Some(pane);
        }
    }
    tmux_output(&["display-message", "-p", "#{pane_id}"])
}

/// Register a global tmux hook.
pub fn set_hook(hook_name: &str, cmd: &str) {
    let _ = tmux_output(&["set-hook", "-g", hook_name, cmd]);
}

/// Remove a global tmux hook.
pub fn remove_hook(hook_name: &str) {
    let _ = tmux_output(&["set-hook", "-gu", hook_name]);
}

/// Get the path to our own binary.
pub fn self_binary() -> String {
    std::env::current_exe()
        .unwrap_or_else(|_| "tmux-agents".into())
        .display()
        .to_string()
}

/// Get the first pane ID in a window (used as split target).
pub fn first_pane_in_window(window_id: &str) -> Option<String> {
    tmux_output(&["list-panes", "-t", window_id, "-F", "#{pane_id}"])
        .and_then(|out| out.lines().next().map(|s| s.to_string()))
}

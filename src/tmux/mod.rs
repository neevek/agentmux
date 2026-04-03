use std::process::Command;

pub const SIDEBAR_TITLE: &str = "agentmux-sidebar";
const DEFAULT_WIDTH: u32 = 50;
pub const MIN_WIDTH: u32 = 50;
const WIDTH_OPTION: &str = "@agentmux-width";
const SELECTED_OPTION: &str = "@agentmux-selected";

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

pub fn get_sidebar_width() -> u32 {
    // Try persistent config first, then tmux option, then default
    load_persisted_width()
        .or_else(|| {
            tmux_output(&["show-option", "-gqv", WIDTH_OPTION])
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(DEFAULT_WIDTH)
}

/// Save width to tmux option, persistent config, and resize all other sidebar panes.
pub fn save_sidebar_width(session: &str, width: u32) {
    let w = width.to_string();
    let _ = tmux_output(&["set-option", "-g", WIDTH_OPTION, &w]);
    persist_width(width);

    // Resize all other sidebar panes in the session to match
    let my_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    for (pane_id, _) in find_all_sidebar_panes(session) {
        if pane_id != my_pane {
            let _ = tmux_output(&["resize-pane", "-t", &pane_id, "-x", &w]);
        }
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".config").join("agentmux").join("config.toml"))
}

fn persist_width(width: u32) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Read existing config, update width line, preserve other settings
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with("width"))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("width = {width}"));
    let _ = std::fs::write(path, lines.join("\n") + "\n");
}

fn load_persisted_width() -> Option<u32> {
    let content = std::fs::read_to_string(config_path()?).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("width") {
            return val.trim().strip_prefix('=')?.trim().parse().ok();
        }
    }
    None
}

pub fn list_session_panes(session: &str) -> Vec<PaneInfo> {
    let Some(out) = tmux_output(&["list-panes", "-s", "-t", session, "-F", PANE_FORMAT]) else {
        return Vec::new();
    };
    out.lines().filter_map(parse_pane_line).collect()
}

pub fn find_all_sidebar_panes(session: &str) -> Vec<(String, String)> {
    let panes = list_session_panes(session);
    panes
        .into_iter()
        .filter(|p| p.title == SIDEBAR_TITLE)
        .map(|p| (p.id, p.window_id))
        .collect()
}

pub fn get_selected_pane() -> String {
    tmux_output(&["show-option", "-gqv", SELECTED_OPTION]).unwrap_or_default()
}

pub fn set_selected_pane(pane_id: &str) {
    let _ = tmux_output(&["set-option", "-g", SELECTED_OPTION, pane_id]);
}

pub fn sidebar_pid_in_window(window_id: &str) -> Option<u32> {
    let fmt = "#{pane_title}\t#{pane_pid}";
    let out = tmux_output(&["list-panes", "-t", window_id, "-F", fmt])?;
    out.lines().find_map(|line| {
        let (title, pid) = line.split_once('\t')?;
        if title == SIDEBAR_TITLE {
            pid.parse().ok()
        } else {
            None
        }
    })
}

pub fn is_pane_in_active_window() -> bool {
    let Some(pane_id) = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()) else {
        return true;
    };
    tmux_output(&["display-message", "-t", &pane_id, "-p", "#{window_active}"])
        .is_some_and(|s| s == "1")
}

pub fn list_window_names(session: &str) -> std::collections::HashMap<String, String> {
    let Some(out) = tmux_output(&[
        "list-windows",
        "-t",
        session,
        "-F",
        "#{window_id}\t#{window_name}",
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

pub fn create_sidebar_in(window_id: &str, cmd: &str) -> Option<String> {
    let sidebar_width = get_sidebar_width();
    let width_str = sidebar_width.to_string();

    let (target, use_full) = find_split_target(window_id)?;

    // When using -f, snapshot non-left pane widths and restore them in the
    // SAME tmux command (chained with ";") to avoid visible flicker.
    let saved_widths: Vec<(String, String)> = if use_full {
        let fmt = "#{pane_id}\t#{pane_left}\t#{pane_width}";
        tmux_output(&["list-panes", "-t", window_id, "-F", fmt])
            .map(|out| {
                out.lines()
                    .filter_map(|line| {
                        let p: Vec<&str> = line.split('\t').collect();
                        if p.len() < 3 { return None; }
                        let left: u32 = p[1].parse().ok()?;
                        if left > 0 {
                            Some((p[0].to_string(), p[2].to_string()))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut args: Vec<String> = vec![
        "split-window".into(),
        "-hb".into(),
        "-l".into(),
        width_str,
        "-t".into(),
        target,
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
    ];
    if use_full {
        args.insert(2, "-f".into());
    }
    args.push(cmd.to_string());

    // Chain resize commands with ";" so tmux executes them atomically
    for (pane_id, width) in &saved_widths {
        args.extend([
            ";".to_string(),
            "resize-pane".to_string(),
            "-t".to_string(),
            pane_id.clone(),
            "-x".to_string(),
            width.clone(),
        ]);
    }

    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tmux_output(&refs)
}

fn find_split_target(window_id: &str) -> Option<(String, bool)> {
    let fmt = "#{pane_id}\t#{pane_left}\t#{pane_top}";
    let out = tmux_output(&["list-panes", "-t", window_id, "-F", fmt])?;

    let panes: Vec<(String, u32, u32)> = out
        .lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split('\t').collect();
            if p.len() < 3 {
                return None;
            }
            Some((
                p[0].to_string(),
                p[1].parse().ok()?,
                p[2].parse().ok()?,
            ))
        })
        .collect();

    // Find panes in the leftmost column (left=0).
    let left_panes: Vec<_> = panes.iter().filter(|(_, left, _)| *left == 0).collect();

    if left_panes.len() == 1 {
        // Single pane spans the full height of the leftmost column.
        // Split without -f: sidebar inherits full height, only this column shrinks.
        Some((left_panes[0].0.clone(), false))
    } else if !left_panes.is_empty() {
        // Multiple panes in left column (vertical splits). Must use -f for
        // full-height sidebar. This shrinks all columns proportionally.
        let topmost = left_panes.iter().min_by_key(|(_, _, top)| *top).unwrap();
        Some((topmost.0.clone(), true))
    } else {
        let first = panes.first()?;
        Some((first.0.clone(), true))
    }
}

pub fn resize_pane_width(width: u32) {
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if !pane.is_empty() {
        let w = width.to_string();
        let _ = tmux_output(&["resize-pane", "-t", &pane, "-x", &w]);
    }
}

pub fn kill_pane(pane_id: &str) {
    let _ = tmux_output(&["kill-pane", "-t", pane_id]);
}

pub fn select_pane(pane_id: &str) {
    let _ = tmux_output(&["select-pane", "-t", pane_id]);
}

pub fn select_window(window_id: &str) {
    let _ = tmux_output(&["select-window", "-t", window_id]);
}

pub fn set_pane_title(pane_id: &str, title: &str) {
    let _ = tmux_output(&["select-pane", "-t", pane_id, "-T", title]);
}

pub fn current_session() -> Option<String> {
    tmux_output(&["display-message", "-p", "#{session_name}"])
}

pub fn current_window_id() -> Option<String> {
    tmux_output(&["display-message", "-p", "#{window_id}"])
}

pub fn set_hook(hook_name: &str, cmd: &str) {
    let _ = tmux_output(&["set-hook", "-g", hook_name, cmd]);
}

pub fn remove_hook(hook_name: &str) {
    let _ = tmux_output(&["set-hook", "-gu", hook_name]);
}

pub fn self_binary() -> String {
    std::env::current_exe()
        .unwrap_or_else(|_| "agentmux".into())
        .display()
        .to_string()
}

use std::process::Command;

pub const SIDEBAR_TITLE: &str = "agentpane-sidebar";
const DEFAULT_WIDTH: u32 = 60;
const WIDTH_OPTION: &str = "@agentpane-width";
const SELECTED_OPTION: &str = "@agentpane-selected";

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
    dirs::home_dir().map(|h| h.join(".config").join("agentpane").join("config.toml"))
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

    let target = find_split_target(window_id)?;

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
    args.push(cmd);
    tmux_output(&args)
}

fn find_split_target(window_id: &str) -> Option<String> {
    let fmt = "#{pane_id}\t#{pane_left}\t#{pane_top}\t#{pane_height}";
    let out = tmux_output(&["list-panes", "-t", window_id, "-F", fmt])?;
    let win_height: u32 =
        tmux_output(&["display-message", "-t", window_id, "-p", "#{window_height}"])
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

    // Prefer a full-height pane at left=0 (ideal: no -f needed)
    let full_height_left = panes.iter().find(|(_, left, top, height)| {
        *left == 0 && *top == 0 && (*height + 1 >= win_height || win_height == 0)
    });

    if let Some((id, _, _, _)) = full_height_left {
        return Some(id.clone());
    }

    // Otherwise pick the tallest pane at left=0; never use -f to avoid squashing
    // all other panes. The sidebar only takes space from the pane it splits from.
    if let Some((id, _, _, _)) = panes
        .iter()
        .filter(|(_, left, _, _)| *left == 0)
        .max_by_key(|(_, _, _, height)| *height)
    {
        return Some(id.clone());
    }

    // No pane at left=0 — just use the first pane
    Some(panes.first()?.0.clone())
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
        .unwrap_or_else(|_| "agentpane".into())
        .display()
        .to_string()
}

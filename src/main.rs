mod config;
mod detect;
mod sidebar;
mod tmux;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agentmux", version, about = "Tmux sidebar for coding agent sessions")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the sidebar TUI (internal: called by split-window)
    Sidebar,
    /// Toggle sidebar visibility
    Toggle,
    /// Open sidebar if not already open
    Open,
    /// Close sidebar if open
    Close,
    /// Ensure sidebar exists in current window (called by tmux hook)
    Ensure,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Sidebar => sidebar::run(),
        Commands::Toggle => cmd_toggle(),
        Commands::Open => cmd_open(),
        Commands::Close => cmd_close(),
        Commands::Ensure => cmd_ensure(),
    }
}

fn cmd_toggle() {
    let session = tmux::current_session().expect("not running inside tmux");
    let sidebars = tmux::find_all_sidebar_panes(&session);
    let current_window = tmux::current_window_id().expect("no current window");
    if sidebars.is_empty() {
        // Create sidebar only in current window, hooks handle the rest lazily
        create_sidebar_in_window(&current_window);
        install_hooks();
    } else if tmux::sidebar_pid_in_window(&current_window).is_none() {
        create_sidebar_in_window(&current_window);
    } else {
        close_all_sidebars(&sidebars);
        uninstall_hooks();
    }
}

fn cmd_open() {
    let session = tmux::current_session().expect("not running inside tmux");
    let current_window = tmux::current_window_id().expect("no current window");
    if tmux::sidebar_pid_in_window(&current_window).is_some() {
        return;
    }
    create_sidebar_in_window(&current_window);
    if tmux::find_all_sidebar_panes(&session).len() == 1 {
        install_hooks();
    }
}

fn cmd_close() {
    let session = tmux::current_session().expect("not running inside tmux");
    let sidebars = tmux::find_all_sidebar_panes(&session);
    if !sidebars.is_empty() {
        close_all_sidebars(&sidebars);
        uninstall_hooks();
    }
}

/// Called by tmux hook on window switch or new window.
/// Creates a sidebar lazily if the current window doesn't have one
/// and the sidebar feature is "on" (at least one sidebar exists elsewhere).
fn cmd_ensure() {
    let current_window = tmux::current_window_id().expect("no current window");
    if tmux::sidebar_pid_in_window(&current_window).is_some() {
        return; // sidebar already exists in this window
    }
    if tmux::is_window_suppressed(&current_window) {
        return;
    }
    // Only create if sidebar is "on" (at least one exists in another window)
    let session = tmux::current_session().expect("not running inside tmux");
    if tmux::find_all_sidebar_panes(&session).is_empty() {
        return;
    }
    create_sidebar_in_window(&current_window);
}

fn close_all_sidebars(sidebars: &[(String, String)]) {
    for (pane_id, _) in sidebars {
        tmux::kill_pane(pane_id);
    }
}

fn create_sidebar_in_window(window_id: &str) {
    tmux::clear_window_suppressed(window_id);
    let binary = tmux::self_binary();
    let cmd = format!("{} sidebar", binary);
    if let Some(new_pane_id) = tmux::create_sidebar_in(window_id, &cmd) {
        tmux::set_pane_title(&new_pane_id, tmux::SIDEBAR_TITLE);
    }
}

fn install_hooks() {
    let binary = tmux::self_binary();
    let ensure_cmd = format!("run-shell -b '{} ensure'", binary);
    tmux::set_hook("after-select-window", &ensure_cmd);
    tmux::set_hook("after-new-window", &ensure_cmd);
}

fn uninstall_hooks() {
    tmux::remove_hook("after-select-window");
    tmux::remove_hook("after-new-window");
}

mod detect;
mod sidebar;
mod tmux;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tmux-agents", about = "Tmux sidebar for coding agent sessions")]
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
    if sidebars.is_empty() {
        // Open: create sidebar in every window
        open_all_sidebars(&session);
        install_hooks();
    } else {
        // Close: kill all sidebars
        close_all_sidebars(&sidebars);
        uninstall_hooks();
    }
}

fn cmd_open() {
    let session = tmux::current_session().expect("not running inside tmux");
    if !tmux::find_all_sidebar_panes(&session).is_empty() {
        return;
    }
    open_all_sidebars(&session);
    install_hooks();
}

fn cmd_close() {
    let session = tmux::current_session().expect("not running inside tmux");
    let sidebars = tmux::find_all_sidebar_panes(&session);
    if !sidebars.is_empty() {
        close_all_sidebars(&sidebars);
        uninstall_hooks();
    }
}

/// Called by tmux hook on after-select-window and after-new-window.
/// Only creates a sidebar if the current window doesn't have one yet.
fn cmd_ensure() {
    let current_window = tmux::current_window_id().expect("no current window");
    if tmux::sidebar_in_window(&current_window) {
        return; // already has one, no-op
    }
    // This window is new or was created before sidebar was opened — add one
    create_sidebar_in_window(&current_window);
}

/// Create a sidebar pane in every window of the session.
fn open_all_sidebars(session: &str) {
    let windows = tmux::list_window_ids(session);
    for win_id in &windows {
        if !tmux::sidebar_in_window(win_id) {
            create_sidebar_in_window(win_id);
        }
    }
}

/// Kill all sidebar panes.
fn close_all_sidebars(sidebars: &[(String, String)]) {
    for (pane_id, _) in sidebars {
        tmux::kill_pane(pane_id);
    }
}

/// Create a sidebar split in a specific window.
fn create_sidebar_in_window(window_id: &str) {
    let Some(target_pane) = tmux::first_pane_in_window(window_id) else {
        return;
    };
    let binary = tmux::self_binary();
    let cmd = format!("{} sidebar", binary);

    if let Some(new_pane_id) = tmux::create_sidebar_split(&target_pane, &cmd) {
        tmux::set_pane_title(&new_pane_id, "tmux-agents-sidebar");
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

use anyhow::Result;
use tmux_agent_dock::{
    dock_follow, dock_toggle, execute_action, load_cards, run_dock, run_status_daemon, run_tui,
};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("status-daemon") => return run_status_daemon(),
        Some("dock") => return run_dock(load_cards()?),
        Some("dock-toggle") => return dock_toggle(),
        Some("dock-follow") => return dock_follow(),
        _ => {}
    }

    if let Some(action) = run_tui(load_cards()?)? {
        execute_action(action)?;
    }

    Ok(())
}

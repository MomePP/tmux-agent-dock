use anyhow::Result;
use tmux_agent_switcher::{execute_action, load_cards, run_status_daemon, run_tui};

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("status-daemon") {
        return run_status_daemon();
    }

    if let Some(action) = run_tui(load_cards()?)? {
        execute_action(action)?;
    }

    Ok(())
}

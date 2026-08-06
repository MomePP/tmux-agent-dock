//! Builders shared by the unit tests of several modules.

use crate::model::{AgentStatus, WindowCard};

pub(crate) fn test_card(session_name: &str, window_index: &str) -> WindowCard {
    WindowCard {
        window_id: format!("@{session_name}-{window_index}"),
        target_pane_id: format!("%{session_name}-{window_index}"),
        session_name: session_name.to_owned(),
        window_index: window_index.to_owned(),
        window_name: format!("window-{window_index}"),
        window_flags: String::new(),
        command: "zsh".to_owned(),
        path: "/tmp".to_owned(),
        title: String::new(),
        preview: Vec::new(),
        codex_unread: false,
        agent_status: AgentStatus::unknown(),
        folded_agents: Vec::new(),
    }
}

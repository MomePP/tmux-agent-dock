//! Core data types shared across the crate: the agents being observed, their
//! status, and the tmux windows/panes/cards the switcher works with.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentKind {
    Codex,
    Claude,
    OpenCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentStatus {
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    pub seen: bool,
    pub run_started_at: Option<u64>,
}

impl AgentStatus {
    pub fn unknown() -> Self {
        Self {
            agent: None,
            state: AgentState::Unknown,
            seen: true,
            run_started_at: None,
        }
    }

    pub(crate) fn done(agent: Option<AgentKind>) -> Self {
        Self {
            agent,
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        }
    }

    pub(crate) fn is_done(self) -> bool {
        self.state == AgentState::Idle && !self.seen
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEvidence {
    pub screen_tail: String,
    pub osc_title: String,
    pub osc_progress: String,
    pub process_exited: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxWindow {
    pub window_id: String,
    pub session_name: String,
    pub window_index: String,
    pub window_name: String,
    pub window_flags: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxPane {
    pub pane_id: String,
    pub window_id: String,
    pub pane_active: bool,
    pub pane_current_command: String,
    pub pane_current_path: String,
    pub pane_title: String,
    pub pane_pid: Option<u32>,
    pub agent_status: AgentStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowCard {
    pub window_id: String,
    pub target_pane_id: String,
    pub session_name: String,
    pub window_index: String,
    pub window_name: String,
    pub window_flags: String,
    pub command: String,
    pub path: String,
    pub title: String,
    pub preview: Vec<String>,
    pub codex_unread: bool,
    pub agent_status: AgentStatus,
    /// Agents in embedded sessions that this card hosts. They have no card of
    /// their own, and several can share one host pane — that is exactly what
    /// happens when sidekick spawns `claude_1` and `claude_2` from the same
    /// Neovim — so each keeps its own status and label here instead of being
    /// flattened into the card's rolled-up one. See [`crate::embed`].
    pub folded_agents: Vec<FoldedAgent>,
}

/// One agent running in an embedded session, attributed to its host card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FoldedAgent {
    pub pane_id: String,
    pub status: AgentStatus,
    /// What to call it in the Agents list. The embedded session is named
    /// `<clone> <cwd-hash>`, and the clone (`claude_1`) is the half that tells
    /// two agents in the same window apart.
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionGroup {
    pub session_name: String,
    pub cards: Vec<WindowCard>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitcherAction {
    Select(WindowCard),
    /// A specific agent inside an embedded session. Several share one host
    /// pane, so focusing the pane is not enough to say which — the clone name
    /// is what distinguishes them.
    SelectAgent {
        card: WindowCard,
        clone: String,
    },
    RenameWindow {
        window_id: String,
        window_name: String,
    },
    NewWindow {
        session_name: String,
        window_name: String,
    },
    NewSession {
        session_name: String,
    },
}

pub(crate) fn parse_agent_kind(value: &str) -> Option<AgentKind> {
    match value {
        "codex" => Some(AgentKind::Codex),
        "claude" => Some(AgentKind::Claude),
        "opencode" => Some(AgentKind::OpenCode),
        _ => None,
    }
}

pub(crate) fn format_agent_kind(agent: Option<AgentKind>) -> &'static str {
    match agent {
        Some(AgentKind::Codex) => "codex",
        Some(AgentKind::Claude) => "claude",
        Some(AgentKind::OpenCode) => "opencode",
        None => "",
    }
}

pub(crate) fn parse_agent_state(value: &str) -> Option<AgentState> {
    match value {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "unknown" => Some(AgentState::Unknown),
        _ => None,
    }
}

pub(crate) fn format_agent_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Unknown => "unknown",
    }
}

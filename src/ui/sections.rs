//! The two sidebar sections: what rows each one contains, and how the body is
//! divided between them.
//!
//! Everything here is pure — rows are built from the already-loaded cards, so
//! the whole layer is testable without a terminal or a tmux server.

use std::collections::HashSet;

use ratatui::layout::Rect;

use crate::{
    cards::rollup_agent_status,
    model::{format_agent_kind, AgentStatus, SessionGroup, WindowCard},
};

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RowKind {
    Session {
        name: String,
        window_count: usize,
        attached: bool,
        expanded: bool,
    },
    Window {
        index: String,
        name: String,
        /// Draws `└─>` instead of `├─>`.
        last_child: bool,
    },
    Agent {
        window_name: String,
        tool: String,
    },
}

/// One line in either section. Every row carries the window it acts on, so
/// selecting is a single code path regardless of which section or kind it is.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Row {
    pub(crate) kind: RowKind,
    pub(crate) status: AgentStatus,
    pub(crate) target: WindowCard,
}

/// The identity a cursor is restored onto after the rows are rebuilt. Session
/// rows key on the session name because their target window can change under
/// them; every other row keys on its window.
#[allow(dead_code)]
pub(crate) fn row_key(row: &Row) -> &str {
    match &row.kind {
        RowKind::Session { name, .. } => name.as_str(),
        _ => row.target.window_id.as_str(),
    }
}

/// The window a session row acts on: the session's current window, or its
/// first if tmux reports no current flag.
#[allow(dead_code)]
fn active_card(session: &SessionGroup) -> Option<&WindowCard> {
    session
        .cards
        .iter()
        .find(|card| card.window_flags.contains('*'))
        .or_else(|| session.cards.first())
}

#[allow(dead_code)]
pub(crate) fn session_rows(
    sessions: &[SessionGroup],
    expanded: &HashSet<String>,
    current_window_id: Option<&str>,
) -> Vec<Row> {
    let mut rows = Vec::new();

    for session in sessions {
        let Some(active) = active_card(session) else {
            continue;
        };
        let is_expanded = expanded.contains(&session.session_name);
        // "Attached" means the client that opened the switcher is in this
        // session. WindowCard carries no session-level attach flag — that
        // lives on `list-sessions` — and the current window identifies the
        // same session without another tmux query.
        let attached = current_window_id
            .map(|window_id| {
                session
                    .cards
                    .iter()
                    .any(|card| card.window_id == window_id)
            })
            .unwrap_or(false);

        rows.push(Row {
            kind: RowKind::Session {
                name: session.session_name.clone(),
                window_count: session.cards.len(),
                attached,
                expanded: is_expanded,
            },
            status: rollup_agent_status(session.cards.iter().map(|card| card.agent_status)),
            target: active.clone(),
        });

        if !is_expanded {
            continue;
        }

        let last_index = session.cards.len().saturating_sub(1);
        for (index, card) in session.cards.iter().enumerate() {
            rows.push(Row {
                kind: RowKind::Window {
                    index: card.window_index.clone(),
                    name: card.window_name.clone(),
                    last_child: index == last_index,
                },
                status: card.agent_status,
                target: card.clone(),
            });
        }
    }

    rows
}

/// Every window running an agent, in session-then-window order. The order is
/// deliberately independent of status: sorting by urgency would move rows out
/// from under the cursor every time an agent changed state.
#[allow(dead_code)]
pub(crate) fn agent_rows(sessions: &[SessionGroup]) -> Vec<Row> {
    sessions
        .iter()
        .flat_map(|session| session.cards.iter())
        .filter(|card| card.agent_status.agent.is_some())
        .map(|card| Row {
            kind: RowKind::Agent {
                window_name: card.window_name.clone(),
                tool: format_agent_kind(card.agent_status.agent).to_owned(),
            },
            status: card.agent_status,
            target: card.clone(),
        })
        .collect()
}

/// Rows the Agents section needs below its title before it starts scrolling.
const MIN_AGENTS_ROWS: u16 = 1;
/// Below this the body cannot carry two titles plus a row each, so the split is
/// abandoned and Sessions keeps everything.
const MIN_SPLIT_HEIGHT: u16 = 4;

/// Divides the body between the two sections. Agents is sized to its content
/// but never more than half, so a couple of agents cannot strand half the
/// sidebar empty while the session list scrolls.
#[allow(dead_code)]
pub(crate) fn section_heights(body: Rect, agent_row_count: usize) -> (Rect, Option<Rect>) {
    if agent_row_count == 0 || body.height < MIN_SPLIT_HEIGHT {
        return (body, None);
    }

    let wanted = u16::try_from(agent_row_count)
        .unwrap_or(u16::MAX)
        .saturating_add(1); // title row
    let agents_height = wanted.clamp(MIN_AGENTS_ROWS.saturating_add(1), body.height / 2);
    let sessions_height = body.height.saturating_sub(agents_height);

    let sessions = Rect {
        height: sessions_height,
        ..body
    };
    let agents = Rect {
        y: body.y.saturating_add(sessions_height),
        height: agents_height,
        ..body
    };

    (sessions, Some(agents))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentState};
    use crate::test_support::test_card;

    fn working(agent: AgentKind) -> AgentStatus {
        AgentStatus {
            agent: Some(agent),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        }
    }

    /// Two sessions: "dotfiles" with three windows (window 0 running Claude),
    /// and "gogo" with one window and no agent.
    fn fixture() -> Vec<SessionGroup> {
        let mut dotfiles: Vec<WindowCard> = (0..3)
            .map(|index| test_card("dotfiles", &index.to_string()))
            .collect();
        dotfiles[0].window_name = "config".to_owned();
        dotfiles[0].agent_status = working(AgentKind::Claude);
        dotfiles[1].window_name = "nvim".to_owned();
        dotfiles[2].window_name = "plugins".to_owned();

        let mut gogo = vec![test_card("gogo", "0")];
        gogo[0].window_name = "gogo-code".to_owned();

        vec![
            SessionGroup {
                session_name: "dotfiles".to_owned(),
                cards: dotfiles,
            },
            SessionGroup {
                session_name: "gogo".to_owned(),
                cards: gogo,
            },
        ]
    }

    #[test]
    fn collapsed_sessions_are_one_row_each_with_rolled_up_status() {
        let rows = session_rows(&fixture(), &HashSet::new(), None);

        assert_eq!(rows.len(), 2);
        match &rows[0].kind {
            RowKind::Session {
                name,
                window_count,
                attached,
                expanded,
            } => {
                assert_eq!(name, "dotfiles");
                assert_eq!(*window_count, 3);
                assert!(!attached);
                assert!(!expanded);
            }
            other => panic!("expected a session row, got {other:?}"),
        }
        // The session dot carries the state of the agent inside it.
        assert_eq!(rows[0].status.agent, Some(AgentKind::Claude));
        assert_eq!(rows[1].status.agent, None);
    }

    #[test]
    fn expanding_a_session_inserts_its_windows_below_it() {
        let expanded = HashSet::from(["dotfiles".to_owned()]);
        let rows = session_rows(&fixture(), &expanded, None);

        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0].kind, RowKind::Session { expanded: true, .. }));
        match &rows[1].kind {
            RowKind::Window {
                index,
                name,
                last_child,
            } => {
                assert_eq!(index, "0");
                assert_eq!(name, "config");
                assert!(!last_child);
            }
            other => panic!("expected a window row, got {other:?}"),
        }
        assert!(matches!(
            rows[3].kind,
            RowKind::Window { last_child: true, .. }
        ));
        // The next session follows the expanded block.
        assert!(matches!(rows[4].kind, RowKind::Session { .. }));
    }

    #[test]
    fn the_session_holding_the_current_window_is_marked_attached() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();
        let rows = session_rows(&sessions, &HashSet::new(), Some(&current));

        assert!(matches!(rows[0].kind, RowKind::Session { attached: false, .. }));
        assert!(matches!(rows[1].kind, RowKind::Session { attached: true, .. }));
    }

    #[test]
    fn session_rows_target_the_sessions_active_window() {
        let mut sessions = fixture();
        sessions[0].cards[1].window_flags = "*".to_owned();
        let rows = session_rows(&sessions, &HashSet::new(), None);

        assert_eq!(rows[0].target.window_id, sessions[0].cards[1].window_id);
    }

    #[test]
    fn session_rows_fall_back_to_the_first_window_when_none_is_current() {
        let sessions = fixture();
        let rows = session_rows(&sessions, &HashSet::new(), None);

        assert_eq!(rows[0].target.window_id, sessions[0].cards[0].window_id);
    }

    #[test]
    fn agent_rows_list_only_cards_running_an_agent() {
        let rows = agent_rows(&fixture());

        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            RowKind::Agent { window_name, tool } => {
                assert_eq!(window_name, "config");
                assert_eq!(tool, "claude");
            }
            other => panic!("expected an agent row, got {other:?}"),
        }
    }

    #[test]
    fn agent_rows_keep_their_order_when_a_status_changes() {
        let mut sessions = fixture();
        sessions[1].cards[0].agent_status = working(AgentKind::Codex);
        let before: Vec<String> = agent_rows(&sessions)
            .iter()
            .map(|row| row.target.window_id.clone())
            .collect();

        // The second agent becomes blocked — the most urgent state there is.
        sessions[1].cards[0].agent_status.state = AgentState::Blocked;
        let after: Vec<String> = agent_rows(&sessions)
            .iter()
            .map(|row| row.target.window_id.clone())
            .collect();

        // Order is session-then-window, so urgency must not reshuffle the rows
        // out from under the cursor.
        assert_eq!(before, after);
    }

    fn body(height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 28,
            height,
        }
    }

    #[test]
    fn no_agents_gives_the_whole_body_to_sessions() {
        let (sessions, agents) = section_heights(body(20), 0);

        assert_eq!(sessions, body(20));
        assert_eq!(agents, None);
    }

    #[test]
    fn agents_take_only_what_they_need() {
        // Two agents: one title row plus two rows.
        let (sessions, agents) = section_heights(body(20), 2);
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 3);
        assert_eq!(sessions.height, 17);
        assert_eq!(sessions.y, 0);
        assert_eq!(agents.y, 17);
    }

    #[test]
    fn agents_never_exceed_half_the_body() {
        // Twenty agents would want 21 rows; half of 20 is 10.
        let (sessions, agents) = section_heights(body(20), 20);
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 10);
        assert_eq!(sessions.height, 10);
    }

    #[test]
    fn a_body_too_short_to_split_stays_one_section() {
        let (sessions, agents) = section_heights(body(3), 5);

        assert_eq!(sessions, body(3));
        assert_eq!(agents, None);
    }
}

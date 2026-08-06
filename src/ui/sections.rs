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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Row {
    pub(crate) kind: RowKind,
    pub(crate) status: AgentStatus,
    pub(crate) target: WindowCard,
}

/// The identity a cursor is restored onto after the rows are rebuilt. Session
/// rows key on the session name because their target window can change under
/// them; every other row keys on its window.
pub(crate) fn row_key(row: &Row) -> &str {
    match &row.kind {
        RowKind::Session { name, .. } => name.as_str(),
        _ => row.target.window_id.as_str(),
    }
}

/// The window a session row acts on: the session's current window, or its
/// first if tmux reports no current flag.
fn active_card(session: &SessionGroup) -> Option<&WindowCard> {
    session
        .cards
        .iter()
        .find(|card| card.window_flags.contains('*'))
        .or_else(|| session.cards.first())
}

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

/// Every running agent, in session-then-window order. The order is deliberately
/// independent of status: sorting by urgency would move rows out from under the
/// cursor every time an agent changed state.
///
/// One row per *agent*, not per window. A window hosting embedded sessions can
/// hold several — sidekick's `claude_1` and `claude_2` spawned from one Neovim
/// share a host pane — and listing the window once would show a single row
/// carrying their rolled-up status, which is what it used to do.
pub(crate) fn agent_rows(sessions: &[SessionGroup]) -> Vec<Row> {
    let mut rows = Vec::new();

    for card in sessions.iter().flat_map(|session| session.cards.iter()) {
        for agent in &card.folded_agents {
            rows.push(Row {
                kind: RowKind::Agent {
                    window_name: card.window_name.clone(),
                    tool: agent.label.clone(),
                },
                status: agent.status,
                // The host card: the embedded session has no card of its own,
                // and focusing the host is the only way to reach the agent.
                target: card.clone(),
            });
        }

        // A window running an agent directly. Its status is its own, not a
        // rollup, so it must not be listed again when it also hosts folded ones.
        if card.folded_agents.is_empty() && card.agent_status.agent.is_some() {
            rows.push(Row {
                kind: RowKind::Agent {
                    window_name: card.window_name.clone(),
                    tool: format_agent_kind(card.agent_status.agent).to_owned(),
                },
                status: card.agent_status,
                target: card.clone(),
            });
        }
    }

    rows
}

/// Screen lines one row occupies: a name line and a dim detail line beneath it.
///
/// Every row in both sections is the same height, which is what lets the
/// renderer, the click resolver and the scroll clamp share one conversion
/// instead of each carrying its own idea of where a row starts.
pub(crate) const ROW_LINES: u16 = 2;

/// How many rows fit in `height` screen lines, given each row's title line is
/// already accounted for by the caller.
pub(crate) fn rows_per_height(height: u16) -> usize {
    (height / ROW_LINES) as usize
}

/// Below this the body cannot carry two titles plus a row each, so the split is
/// abandoned and Sessions keeps everything.
const MIN_SPLIT_HEIGHT: u16 = 6;

/// Divides the body in half between the two sections. The split does not depend
/// on how many agents are running: an Agents section that resizes itself moves
/// the boundary under the user every time an agent starts or exits, and with no
/// agents at all the section is worth keeping as a visible "none running"
/// statement rather than silently vanishing.
pub(crate) fn section_heights(body: Rect) -> (Rect, Option<Rect>) {
    if body.height < MIN_SPLIT_HEIGHT {
        return (body, None);
    }

    // A fixed half each, so the Agents title sits on the body's midpoint and
    // stays there. Sizing the section to its content instead parked the title
    // just above the search bar and moved it every time an agent came or went,
    // which read as the section drifting rather than as a boundary.
    // Halved from the Agents side so an odd body gives its spare row to
    // Sessions, which holds the longer list.
    let agents_height = body.height / 2;
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

/// The session holding the window the switcher was opened from.
fn attached_session_name(
    sessions: &[SessionGroup],
    current_window_id: Option<&str>,
) -> Option<String> {
    let window_id = current_window_id?;
    sessions
        .iter()
        .find(|session| session.cards.iter().any(|card| card.window_id == window_id))
        .map(|session| session.session_name.clone())
}

/// What a fresh tmux server starts with, from `@agent_switcher_expand_default`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub(crate) enum ExpandDefault {
    /// Every session expanded — the whole tree at once, and the default. The
    /// sidebar exists to show where everything is, and opening it folded hides
    /// exactly that until you go looking for it.
    #[default]
    All,
    /// Only the session you are attached to.
    Attached,
    /// Everything collapsed: one row per session.
    None,
}

pub(crate) fn parse_expand_default(value: &str) -> ExpandDefault {
    match value.trim() {
        "attached" | "current" => ExpandDefault::Attached,
        "none" | "collapsed" | "closed" => ExpandDefault::None,
        // Includes "all" and anything unrecognised: an unreadable setting
        // should land on the documented default, not on a surprise.
        _ => ExpandDefault::All,
    }
}

/// The expansion set the switcher opens with.
///
/// The default only decides what a *fresh* server looks like. Once the
/// persisted option carries a set it is the user's own choice, and re-applying
/// a default over it would fight them on every open.
///
/// `persisted` is `None` only when the option was never written. An explicitly
/// empty set arrives as `Some(empty)` and is honoured as-is — collapsing every
/// session is a deliberate act, and seeding anything back in would undo it on
/// the very next open.
pub(crate) fn initial_expanded_set(
    persisted: Option<HashSet<String>>,
    sessions: &[SessionGroup],
    current_window_id: Option<&str>,
    default: ExpandDefault,
) -> HashSet<String> {
    if let Some(expanded) = persisted {
        return expanded;
    }

    match default {
        ExpandDefault::All => sessions
            .iter()
            .map(|session| session.session_name.clone())
            .collect(),
        ExpandDefault::None => HashSet::new(),
        ExpandDefault::Attached => attached_session_name(sessions, current_window_id)
            .into_iter()
            .collect(),
    }
}

/// Session names may contain spaces, so the persisted set is tab-separated.
const EXPANDED_SEPARATOR: char = '\t';

pub(crate) fn parse_expanded(value: &str) -> HashSet<String> {
    value
        .split(EXPANDED_SEPARATOR)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn format_expanded(expanded: &HashSet<String>) -> String {
    let mut names: Vec<&str> = expanded.iter().map(String::as_str).collect();
    names.sort_unstable(); // stable option value, so no pointless tmux writes
    names.join(&EXPANDED_SEPARATOR.to_string())
}

/// Sessions whose window list was narrowed by the active query. Those get
/// expanded for as long as the query stands — a match hidden inside a
/// collapsed session looks like a search that does not work.
pub(crate) fn sessions_matching_windows(
    filtered: &[SessionGroup],
    all: &[SessionGroup],
) -> HashSet<String> {
    filtered
        .iter()
        .filter(|session| {
            all.iter()
                .find(|candidate| candidate.session_name == session.session_name)
                .is_some_and(|candidate| candidate.cards.len() > session.cards.len())
        })
        .map(|session| session.session_name.clone())
        .collect()
}

/// Which section the keyboard is driving. The other keeps its cursor and
/// scroll position and renders dim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SectionFocus {
    Sessions,
    Agents,
}

impl SectionFocus {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Sessions => Self::Agents,
            Self::Agents => Self::Sessions,
        }
    }
}

/// What a click landed on.
#[allow(dead_code)] // Task 3: mouse event handler
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClickTarget {
    /// A row: focus that section and select item `index`.
    Row {
        section: SectionFocus,
        index: usize,
    },
    /// A title or the empty space below the last row: focus the section, select
    /// nothing. Clicking a heading or a gap should not move you anywhere.
    Section(SectionFocus),
    /// Outside both sections.
    None,
}

/// Resolves a click's row within the body to a section and item index, using
/// the same split and the same scroll offsets the renderer used to draw it.
#[allow(dead_code)] // Task 3: mouse event handler
pub(crate) fn row_at(
    body: Rect,
    click_y: u16,
    sessions_len: usize,
    sessions_offset: usize,
    agents_len: usize,
    agents_offset: usize,
) -> ClickTarget {
    let (sessions_area, agents_area) = section_heights(body);

    if let Some(target) = hit(sessions_area, click_y, sessions_len, sessions_offset) {
        return match target {
            Some(index) => ClickTarget::Row {
                section: SectionFocus::Sessions,
                index,
            },
            None => ClickTarget::Section(SectionFocus::Sessions),
        };
    }

    let Some(agents_area) = agents_area else {
        return ClickTarget::None;
    };
    match hit(agents_area, click_y, agents_len, agents_offset) {
        Some(Some(index)) => ClickTarget::Row {
            section: SectionFocus::Agents,
            index,
        },
        Some(None) => ClickTarget::Section(SectionFocus::Agents),
        None => ClickTarget::None,
    }
}

/// `None` when the click is outside this section; `Some(None)` when it is on
/// the title or past the last row; `Some(Some(index))` when it is on a row.
#[allow(dead_code)] // Task 3: mouse event handler
fn hit(area: Rect, click_y: u16, len: usize, offset: usize) -> Option<Option<usize>> {
    if area.height == 0 || click_y < area.y || click_y >= area.y.saturating_add(area.height) {
        return None;
    }
    // Line 0 of the section is its title.
    let Some(line) = click_y.checked_sub(area.y).filter(|line| *line > 0) else {
        return Some(None);
    };
    // A row spans ROW_LINES lines, so either of them resolves to the same item:
    // clicking an agent's `working · claude_1` detail means its name.
    let index = offset.saturating_add(usize::from((line - 1) / ROW_LINES));
    Some((index < len).then_some(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentState, FoldedAgent};
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

    /// Two agents spawned from one Neovim (`<leader>s` and `2<leader>s`) live in
    /// separate embedded sessions that fold into the same host window. Listing
    /// windows instead of agents showed one row carrying their rolled-up
    /// status, so the second was invisible.
    #[test]
    fn two_agents_in_one_host_window_get_a_row_each() {
        let mut host = test_card("dotfiles", "0");
        host.window_name = "config".to_owned();
        host.agent_status = working(AgentKind::Claude); // the rollup
        host.folded_agents = vec![
            FoldedAgent {
                pane_id: "%20".to_owned(),
                status: working(AgentKind::Claude),
                label: "claude_1".to_owned(),
            },
            FoldedAgent {
                pane_id: "%21".to_owned(),
                status: AgentStatus {
                    agent: Some(AgentKind::Claude),
                    state: AgentState::Idle,
                    seen: true,
                    run_started_at: None,
                },
                label: "claude_2".to_owned(),
            },
        ];
        let sessions = vec![SessionGroup {
            session_name: "dotfiles".to_owned(),
            cards: vec![host],
        }];

        let rows = agent_rows(&sessions);

        assert_eq!(rows.len(), 2, "both agents should be listed");
        let labels: Vec<&str> = rows
            .iter()
            .map(|row| match &row.kind {
                RowKind::Agent { tool, .. } => tool.as_str(),
                other => panic!("expected an agent row, got {other:?}"),
            })
            .collect();
        assert_eq!(labels, vec!["claude_1", "claude_2"]);
        // Each keeps its own state rather than the host's rolled-up one.
        assert_eq!(rows[0].status.state, AgentState::Working);
        assert_eq!(rows[1].status.state, AgentState::Idle);
        // Both resolve to the host, the only pane that can reach them.
        assert_eq!(rows[0].target.window_id, rows[1].target.window_id);
    }

    /// A window running an agent directly is still listed once, and is not
    /// double-counted against its own folded list.
    #[test]
    fn a_direct_agent_window_is_listed_once() {
        let rows = agent_rows(&fixture());

        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].kind, RowKind::Agent { .. }));
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

    /// The boundary is the point of the split, so it holds still even with no
    /// agents running — a section that appears and disappears moves every row
    /// beneath it.
    #[test]
    fn no_agents_still_gets_its_half() {
        let (sessions, agents) = section_heights(body(20));
        let agents = agents.expect("agents section");

        assert_eq!(sessions.height, 10);
        assert_eq!(agents.height, 10);
        assert_eq!(agents.y, 10);
    }

    /// The split is fixed: one agent and twenty produce the same boundary, so
    /// an agent starting or exiting never shifts the Sessions list.
    #[test]
    fn the_boundary_does_not_move_with_the_agent_count() {
        let few = section_heights(body(20));
        let many = section_heights(body(20));

        assert_eq!(few, many);
        assert_eq!(few.1.expect("agents section").y, 10);
    }

    /// The shortest body that still splits: a title plus one two-line row in
    /// each half.
    #[test]
    fn the_shortest_splittable_body_gives_each_section_half() {
        let (sessions, agents) = section_heights(body(6));
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 3);
        assert_eq!(sessions.height, 3);
        assert_eq!(agents.y, 3);
    }

    /// An odd body cannot halve evenly; the extra row goes to Sessions, which
    /// holds the longer list.
    #[test]
    fn an_odd_body_gives_the_spare_row_to_sessions() {
        let (sessions, agents) = section_heights(body(21));
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 10);
        assert_eq!(sessions.height, 11);
        assert_eq!(agents.y, 11);
    }

    #[test]
    fn a_body_too_short_to_split_stays_one_section() {
        let (sessions, agents) = section_heights(body(5));

        assert_eq!(sessions, body(5));
        assert_eq!(agents, None);
    }

    #[test]
    fn expanded_state_round_trips_through_a_tab_separated_option() {
        let expanded = HashSet::from([
            "dotfiles-config".to_owned(),
            // Session names can contain spaces, which is why the separator is
            // a tab rather than whitespace.
            "claude_1 b9f9f91c".to_owned(),
        ]);

        let restored = parse_expanded(&format_expanded(&expanded));

        assert_eq!(restored, expanded);
    }

    #[test]
    fn parsing_an_empty_option_yields_nothing_expanded() {
        assert!(parse_expanded("").is_empty());
        assert!(parse_expanded("\t\t").is_empty());
    }

    /// Spec §4: "The attached session starts expanded; everything else starts
    /// collapsed." Task 4 built only the persistence half, so on a fresh tmux
    /// server everything opened collapsed.
    #[test]
    fn a_fresh_server_opens_with_the_attached_session_expanded() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();

        let expanded =
            initial_expanded_set(None, &sessions, Some(&current), ExpandDefault::Attached);

        assert_eq!(expanded, HashSet::from(["gogo".to_owned()]));
    }

    /// Once something was remembered, the remembered set is the whole answer —
    /// re-adding the attached session would undo a deliberate collapse.
    #[test]
    fn a_remembered_set_is_left_exactly_as_it_was() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();
        let persisted = HashSet::from(["dotfiles".to_owned()]);

        let expanded = initial_expanded_set(
            Some(persisted.clone()),
            &sessions,
            Some(&current),
            ExpandDefault::All,
        );

        assert_eq!(expanded, persisted);
    }

    /// `@agent_switcher_expand_default` decides what a fresh server shows.
    #[test]
    fn the_expand_default_decides_a_fresh_servers_tree() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();

        assert_eq!(
            initial_expanded_set(None, &sessions, Some(&current), ExpandDefault::All),
            HashSet::from(["dotfiles".to_owned(), "gogo".to_owned()])
        );
        assert!(initial_expanded_set(None, &sessions, Some(&current), ExpandDefault::None).is_empty());
        assert_eq!(
            initial_expanded_set(None, &sessions, Some(&current), ExpandDefault::Attached),
            HashSet::from(["gogo".to_owned()])
        );
    }

    /// An unreadable setting lands on the documented default rather than a
    /// surprise, and the spellings people actually try are accepted.
    #[test]
    fn the_expand_default_parses_its_spellings() {
        assert_eq!(parse_expand_default("attached"), ExpandDefault::Attached);
        assert_eq!(parse_expand_default(" current "), ExpandDefault::Attached);
        assert_eq!(parse_expand_default("none"), ExpandDefault::None);
        assert_eq!(parse_expand_default("collapsed"), ExpandDefault::None);
        assert_eq!(parse_expand_default("all"), ExpandDefault::All);
        // Unset and unrecognised both land on the default, which is `all`.
        assert_eq!(parse_expand_default(""), ExpandDefault::All);
        assert_eq!(parse_expand_default("nonsense"), ExpandDefault::All);
        assert_eq!(ExpandDefault::default(), ExpandDefault::All);
    }

    /// The default only shapes a fresh server. Once anything is remembered it is
    /// the whole answer, or `all` would re-expand what the user just collapsed.
    #[test]
    fn a_remembered_set_outranks_the_default() {
        let sessions = fixture();
        let persisted = HashSet::from(["dotfiles".to_owned()]);

        let expanded =
            initial_expanded_set(Some(persisted.clone()), &sessions, None, ExpandDefault::All);

        assert_eq!(expanded, persisted);
    }

    /// Collapsing every session persists an empty set, which is NOT the same as
    /// never having been written. Seeding the attached session back in here
    /// would silently re-expand it on the very next open.
    #[test]
    fn collapsing_every_session_survives_the_next_open() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();

        let expanded = initial_expanded_set(
            Some(HashSet::new()),
            &sessions,
            Some(&current),
            ExpandDefault::All,
        );

        assert!(
            expanded.is_empty(),
            "an explicitly emptied set was re-seeded: {expanded:?}"
        );
    }

    #[test]
    fn nothing_is_expanded_when_the_current_window_is_unknown() {
        assert!(
            initial_expanded_set(None, &fixture(), None, ExpandDefault::Attached).is_empty()
        );
    }

    #[test]
    fn focus_toggles_between_the_two_sections() {
        assert_eq!(SectionFocus::Sessions.toggled(), SectionFocus::Agents);
        assert_eq!(SectionFocus::Agents.toggled(), SectionFocus::Sessions);
    }

    #[test]
    fn a_query_that_matched_only_some_windows_marks_that_session_for_expansion() {
        let all = fixture();
        // Simulate a filter that kept only window 1 of "dotfiles".
        let filtered = vec![SessionGroup {
            session_name: "dotfiles".to_owned(),
            cards: vec![all[0].cards[1].clone()],
        }];

        let expand = sessions_matching_windows(&filtered, &all);

        assert!(expand.contains("dotfiles"));
    }

    #[test]
    fn an_unfiltered_session_is_not_force_expanded() {
        let all = fixture();
        let expand = sessions_matching_windows(&all, &all);

        assert!(expand.is_empty());
    }

    /// body(20) splits into Sessions rows 0..10 and Agents rows 10..20, each
    /// with a title on its first row.
    #[test]
    fn a_click_on_a_row_resolves_to_that_row() {
        let area = body(20);

        // Sessions title is y=0; its first row is y=1.
        assert_eq!(
            row_at(area, 1, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 0
            }
        );
        // Rows are two lines tall: lines 1-2 are item 0, lines 3-4 item 1.
        assert_eq!(
            row_at(area, 3, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 1
            }
        );
        // Agents starts at y=10; its title is y=10, first row y=11.
        assert_eq!(
            row_at(area, 11, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Agents,
                index: 0
            }
        );
    }

    /// Clicking a title focuses that section without moving its cursor —
    /// clicking a heading should not teleport you to a window.
    #[test]
    fn a_click_on_a_title_focuses_without_selecting() {
        let area = body(20);

        assert_eq!(row_at(area, 0, 5, 0, 3, 0), ClickTarget::Section(SectionFocus::Sessions));
        assert_eq!(row_at(area, 10, 5, 0, 3, 0), ClickTarget::Section(SectionFocus::Agents));
    }

    /// Both lines of a row mean the same entry — clicking an agent's
    /// `working · claude_1` detail is clicking the agent.
    #[test]
    fn either_line_of_a_row_resolves_to_the_same_item() {
        let area = body(20);

        for line in [1, 2] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Row {
                    section: SectionFocus::Sessions,
                    index: 0
                },
                "line {line} should be item 0"
            );
        }
        for line in [3, 4] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Row {
                    section: SectionFocus::Sessions,
                    index: 1
                },
                "line {line} should be item 1"
            );
        }
    }

    /// Empty space past the last row is still that section, but selects nothing.
    #[test]
    fn a_click_past_the_last_row_focuses_the_section_only() {
        let area = body(20);

        // Sessions holds 2 rows: y=1 and y=2. y=5 is past them.
        assert_eq!(row_at(area, 5, 2, 0, 3, 0), ClickTarget::Section(SectionFocus::Sessions));
        // Agents holds 1 row at y=11. y=15 is past it.
        assert_eq!(row_at(area, 15, 2, 0, 1, 0), ClickTarget::Section(SectionFocus::Agents));
    }

    /// A scrolled section resolves through its offset: the first visible row is
    /// item `offset`, not item 0.
    #[test]
    fn a_click_in_a_scrolled_section_resolves_through_its_offset() {
        let area = body(20);

        assert_eq!(
            row_at(area, 1, 40, 12, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 12
            }
        );
        assert_eq!(
            row_at(area, 12, 40, 12, 30, 7),
            ClickTarget::Row {
                section: SectionFocus::Agents,
                index: 7
            }
        );
    }

    /// A body too short to split has no Agents section to click into.
    #[test]
    fn a_click_in_an_unsplit_body_can_only_hit_sessions() {
        let area = body(5);

        assert_eq!(
            row_at(area, 1, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 0
            }
        );
        assert_eq!(row_at(area, 99, 5, 0, 3, 0), ClickTarget::None);
    }
}

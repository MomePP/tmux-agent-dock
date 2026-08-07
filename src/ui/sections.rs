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
        /// The window the client is actually in — not merely inside the
        /// attached session. Takes the accent colour, like its session row.
        attached: bool,
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
                    attached: current_window_id == Some(card.window_id.as_str()),
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

/// Screen lines one row occupies, which differs by section.
///
/// Sessions is a tree you scan: with a dozen sessions expanded, a second line
/// per row halves how much of it fits on screen, and the only thing that line
/// carried — the window count and the fold arrow — fits on the name line's
/// right edge. Agents keeps its pair, because `working · claude_2` is the
/// whole point of the row and there is nowhere else for it to go.
///
/// This is the one authority on row height: the renderer, the click resolver
/// and the scroll clamp all convert through it, so none of them can disagree
/// about where a row starts.
pub(crate) fn row_lines(section: SectionFocus) -> u16 {
    match section {
        SectionFocus::Sessions => 1,
        SectionFocus::Agents => 2,
    }
}

/// How many rows fit in `height` screen lines.
pub(crate) fn rows_per_height(height: u16, section: SectionFocus) -> usize {
    (height / row_lines(section)) as usize
}

/// Lines a section spends above its first row: a leading line, the title, and
/// a blank line under it.
///
/// Both sections spend the same three, so the two halves are built the same
/// way. The leading line is blank for Sessions — padding, so the title is not
/// jammed against whatever sits above the sidebar — and is the rule for
/// Agents. The rule belongs to the half below it rather than hanging off the
/// end of the session list, which would move it every time a session was
/// expanded.
pub(crate) fn header_lines(_section: SectionFocus) -> u16 {
    3
}

/// The part of a section's area its rows are drawn into, below the header.
/// The renderer, the click resolver and the scroll clamp all go through this,
/// so none of them can disagree about where row 0 starts.
pub(crate) fn rows_area(area: Rect, section: SectionFocus) -> Rect {
    let header = header_lines(section);
    Rect {
        y: area.y.saturating_add(header),
        height: area.height.saturating_sub(header),
        ..area
    }
}

/// The share of the body the Agents section takes.
///
/// Not half. There are only ever a handful of agents — two, on the sidebar
/// this was tuned against — while an expanded session tree runs to dozens of
/// rows, so an even split spent most of the lower half on blank space that the
/// upper half was scrolling for want of.
const AGENTS_PERCENT: u16 = 40;

/// Below this the body cannot carry both headers plus a row each, so the split
/// is abandoned and Sessions keeps everything. Agents is the binding section:
/// it needs three header lines and a two-line row, and 40% of 13 is exactly
/// that.
const MIN_SPLIT_HEIGHT: u16 = 13;

/// Divides the body between the two sections, Sessions on top. The split does
/// not depend on how many agents are running: an Agents section that resizes
/// itself moves the boundary under the user every time an agent starts or
/// exits, and with no agents at all the section is worth keeping as a visible
/// "none running" statement rather than silently vanishing.
pub(crate) fn section_heights(body: Rect) -> (Rect, Option<Rect>) {
    if body.height < MIN_SPLIT_HEIGHT {
        return (body, None);
    }

    // A fixed share each, so the Agents title lands in one place and stays
    // there. Sizing the section to its content instead parked the title just
    // above the search bar and moved it every time an agent came or went, which
    // read as the section drifting rather than as a boundary.
    // Rounded down from the Agents side, so a body that does not divide evenly
    // gives its spare rows to Sessions, which holds the longer list.
    let agents_height = body.height * AGENTS_PERCENT / 100;
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

/// The expansion set the switcher opens with, decided one session at a time.
///
/// Two sets are remembered, not one: `expanded` is what was left open, and
/// `known` is every session the switcher had an opinion about when it last
/// closed. A session in `known` keeps whatever the user left it as — including
/// collapsed, which is why collapsing everything survives the next open. A
/// session that is *not* in `known` is new since then, and follows `default`.
///
/// Remembering only `expanded` conflated "you collapsed this" with "this did
/// not exist yet", so `@agent_switcher_expand_default = all` never applied to a
/// session created after the first close — and a server whose remembered set
/// had gone empty stayed fully collapsed forever, with no way back short of
/// unsetting the option by hand.
pub(crate) fn initial_expanded_set(
    expanded: &HashSet<String>,
    known: &HashSet<String>,
    sessions: &[SessionGroup],
    current_window_id: Option<&str>,
    default: ExpandDefault,
) -> HashSet<String> {
    let attached = attached_session_name(sessions, current_window_id);

    sessions
        .iter()
        .map(|session| &session.session_name)
        .filter(|name| {
            if known.contains(*name) {
                return expanded.contains(*name);
            }
            match default {
                ExpandDefault::All => true,
                ExpandDefault::None => false,
                ExpandDefault::Attached => attached.as_ref() == Some(*name),
            }
        })
        .cloned()
        .collect()
}

/// Session names may contain spaces, so the persisted sets are tab-separated.
///
/// Expanded and known live in two tmux options rather than one marked-up
/// value: tmux accepts `:`, `.` and `!` in a session name, so there is no
/// character an in-band "collapsed" marker could safely use.
const EXPANDED_SEPARATOR: char = '\t';

pub(crate) fn parse_expanded(value: &str) -> HashSet<String> {
    value
        .split(EXPANDED_SEPARATOR)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn format_expanded(names: impl IntoIterator<Item = String>) -> String {
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort_unstable(); // stable option value, so no pointless tmux writes
    names.join(&EXPANDED_SEPARATOR.to_string())
}

/// Every session on the server: what gets remembered as "had an opinion about"
/// so the next open can tell a collapsed session from a brand new one.
pub(crate) fn known_session_names(sessions: &[SessionGroup]) -> HashSet<String> {
    sessions
        .iter()
        .map(|session| session.session_name.clone())
        .collect()
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

    if let Some(target) = hit(
        sessions_area,
        click_y,
        sessions_len,
        sessions_offset,
        SectionFocus::Sessions,
    ) {
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
    match hit(
        agents_area,
        click_y,
        agents_len,
        agents_offset,
        SectionFocus::Agents,
    ) {
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
fn hit(
    area: Rect,
    click_y: u16,
    len: usize,
    offset: usize,
    section: SectionFocus,
) -> Option<Option<usize>> {
    if area.height == 0 || click_y < area.y || click_y >= area.y.saturating_add(area.height) {
        return None;
    }
    // The section's header — its rule, title and the blank line under them —
    // is not a row: clicking it focuses the section and selects nothing.
    let Some(line) = click_y
        .checked_sub(area.y)
        .and_then(|line| line.checked_sub(header_lines(section)))
    else {
        return Some(None);
    };
    // An Agents row spans two lines, so either of them resolves to the same
    // item: clicking `working · claude_1` means the agent above it.
    let index = offset.saturating_add(usize::from(line / row_lines(section)));
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
                ..
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
    /// beneath it. Agents takes 40%, not half: a handful of agents against a
    /// tree of dozens of rows.
    #[test]
    fn no_agents_still_gets_its_share() {
        let (sessions, agents) = section_heights(body(20));
        let agents = agents.expect("agents section");

        assert_eq!(sessions.height, 12);
        assert_eq!(agents.height, 8);
        assert_eq!(agents.y, 12);
    }

    /// The split is fixed: one agent and twenty produce the same boundary, so
    /// an agent starting or exiting never shifts the Sessions list.
    #[test]
    fn the_boundary_does_not_move_with_the_agent_count() {
        let few = section_heights(body(20));
        let many = section_heights(body(20));

        assert_eq!(few, many);
        assert_eq!(few.1.expect("agents section").y, 12);
    }

    /// The shortest body that still splits. Agents is the binding section: its
    /// rule, title and blank line, plus a two-line row, is five — which is what
    /// 40% of 13 rounds to.
    #[test]
    fn the_shortest_splittable_body_still_fits_a_row_in_each() {
        let (sessions, agents) = section_heights(body(13));
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 5);
        assert_eq!(sessions.height, 8);
        assert_eq!(agents.y, 8);
        // Each half has exactly one row's worth of space left under its header.
        assert_eq!(
            rows_per_height(rows_area(agents, SectionFocus::Agents).height, SectionFocus::Agents),
            1
        );
        assert!(
            rows_per_height(
                rows_area(sessions, SectionFocus::Sessions).height,
                SectionFocus::Sessions
            ) >= 1
        );
    }

    /// The share rounds down from the Agents side, so whatever does not divide
    /// evenly goes to Sessions, which holds the longer list.
    #[test]
    fn the_spare_rows_go_to_sessions() {
        let (sessions, agents) = section_heights(body(21));
        let agents = agents.expect("agents section");

        assert_eq!(agents.height, 8); // 21 * 40 / 100 == 8.4, rounded down
        assert_eq!(sessions.height, 13);
        assert_eq!(agents.y, 13);
    }

    #[test]
    fn a_body_too_short_to_split_stays_one_section() {
        // One line short of splitting: Agents could not have fit a row under
        // its header, so Sessions keeps the whole body instead.
        let (sessions, agents) = section_heights(body(12));

        assert_eq!(sessions, body(12));
        assert_eq!(agents, None);
    }

    fn names(values: &[&str]) -> HashSet<String> {
        values.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Nothing remembered yet: every session follows the default.
    fn fresh() -> (HashSet<String>, HashSet<String>) {
        (HashSet::new(), HashSet::new())
    }

    #[test]
    fn expanded_state_round_trips_through_a_tab_separated_option() {
        let expanded = names(&[
            "dotfiles-config",
            // Session names can contain spaces, which is why the separator is
            // a tab rather than whitespace.
            "claude_1 b9f9f91c",
        ]);

        let restored = parse_expanded(&format_expanded(expanded.iter().cloned()));

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
        let (expanded, known) = fresh();

        let opened = initial_expanded_set(
            &expanded,
            &known,
            &sessions,
            Some(&current),
            ExpandDefault::Attached,
        );

        assert_eq!(opened, names(&["gogo"]));
    }

    /// A session the switcher has seen keeps whatever the user left it as, and
    /// the default does not get a second vote on it.
    #[test]
    fn a_remembered_session_keeps_what_the_user_left_it_as() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();

        let opened = initial_expanded_set(
            &names(&["dotfiles"]),
            &names(&["dotfiles", "gogo"]),
            &sessions,
            Some(&current),
            ExpandDefault::All,
        );

        assert_eq!(
            opened,
            names(&["dotfiles"]),
            "`gogo` was remembered as collapsed and must stay collapsed"
        );
    }

    /// `@agent_switcher_expand_default` decides what a fresh server shows.
    #[test]
    fn the_expand_default_decides_a_fresh_servers_tree() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();
        let (expanded, known) = fresh();
        let open = |default| {
            initial_expanded_set(&expanded, &known, &sessions, Some(&current), default)
        };

        assert_eq!(open(ExpandDefault::All), names(&["dotfiles", "gogo"]));
        assert!(open(ExpandDefault::None).is_empty());
        assert_eq!(open(ExpandDefault::Attached), names(&["gogo"]));
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

    /// Collapsing every session must survive the next open. It is remembered as
    /// an empty `expanded` against a full `known`, which is what tells it apart
    /// from a server that has never been opened.
    #[test]
    fn collapsing_every_session_survives_the_next_open() {
        let sessions = fixture();
        let current = sessions[1].cards[0].window_id.clone();

        let opened = initial_expanded_set(
            &HashSet::new(),
            &known_session_names(&sessions),
            &sessions,
            Some(&current),
            ExpandDefault::All,
        );

        assert!(
            opened.is_empty(),
            "a deliberate full collapse was re-seeded: {opened:?}"
        );
    }

    /// The bug this two-set memory exists for. A session created after the last
    /// close was never collapsed by anyone — it simply did not exist — so it
    /// follows the default instead of inheriting "absent means collapsed".
    #[test]
    fn a_session_created_since_the_last_close_follows_the_default() {
        let sessions = fixture();

        let opened = initial_expanded_set(
            &HashSet::new(),
            &names(&["dotfiles"]),
            &sessions,
            None,
            ExpandDefault::All,
        );

        assert_eq!(
            opened,
            names(&["gogo"]),
            "`dotfiles` was collapsed on purpose; `gogo` is new and takes the default"
        );
    }

    /// A server whose remembered state was written by an older build, or lost,
    /// reads as "nothing known" and heals back to the default rather than
    /// staying collapsed forever with no way out but unsetting the option.
    #[test]
    fn a_server_with_no_memory_heals_to_the_default() {
        let sessions = fixture();
        let (expanded, known) = fresh();

        let opened =
            initial_expanded_set(&expanded, &known, &sessions, None, ExpandDefault::All);

        assert_eq!(opened, names(&["dotfiles", "gogo"]));
    }

    #[test]
    fn known_names_are_every_session_on_the_server() {
        assert_eq!(known_session_names(&fixture()), names(&["dotfiles", "gogo"]));
    }

    #[test]
    fn nothing_is_expanded_when_the_current_window_is_unknown() {
        let (expanded, known) = fresh();
        assert!(initial_expanded_set(
            &expanded,
            &known,
            &fixture(),
            None,
            ExpandDefault::Attached
        )
        .is_empty());
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

    /// body(20) splits 12/8: Sessions 0..12 and Agents 12..20. Each opens with
    /// three header lines — a leading line (blank for Sessions, the rule for
    /// Agents), the title, then a blank — so Sessions rows start at y=3 and
    /// Agents rows at y=15.
    #[test]
    fn a_click_on_a_row_resolves_to_that_row() {
        let area = body(20);

        assert_eq!(
            row_at(area, 3, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 0
            }
        );
        // Sessions rows are one line tall, so the next line is the next item.
        assert_eq!(
            row_at(area, 5, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 2
            }
        );
        assert_eq!(
            row_at(area, 15, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Agents,
                index: 0
            }
        );
    }

    /// Clicking a header — the rule, the title, or the blank line under it —
    /// focuses that section without moving its cursor. Clicking a heading
    /// should not teleport you to a window.
    #[test]
    fn a_click_on_a_header_focuses_without_selecting() {
        let area = body(20);

        for line in [0, 1, 2] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Section(SectionFocus::Sessions),
                "line {line} is Sessions header"
            );
        }
        for line in [12, 13, 14] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Section(SectionFocus::Agents),
                "line {line} is Agents header"
            );
        }
    }

    /// Both lines of an Agents row mean the same entry — clicking an agent's
    /// `working · claude_1` detail is clicking the agent. Sessions rows are a
    /// single line, so there is no second line to get this wrong on.
    #[test]
    fn either_line_of_an_agent_row_resolves_to_the_same_item() {
        let area = body(20);

        // Agents rows start at y=15: item 0 spans y=15..=16, item 1 y=17..=18.
        for line in [15, 16] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Row {
                    section: SectionFocus::Agents,
                    index: 0
                },
                "line {line} should be item 0"
            );
        }
        for line in [17, 18] {
            assert_eq!(
                row_at(area, line, 5, 0, 3, 0),
                ClickTarget::Row {
                    section: SectionFocus::Agents,
                    index: 1
                },
                "line {line} should be item 1"
            );
        }
    }

    /// Each Sessions line is its own row, which is the whole point of the
    /// section being one line tall: a click never lands an item short.
    #[test]
    fn consecutive_session_lines_are_consecutive_items() {
        let area = body(20);

        for (line, index) in [(3u16, 0usize), (4, 1), (5, 2), (6, 3)] {
            assert_eq!(
                row_at(area, line, 8, 0, 3, 0),
                ClickTarget::Row {
                    section: SectionFocus::Sessions,
                    index
                },
                "line {line} should be item {index}"
            );
        }
    }

    /// Empty space past the last row is still that section, but selects nothing.
    #[test]
    fn a_click_past_the_last_row_focuses_the_section_only() {
        let area = body(20);

        // Sessions holds 2 rows: y=3 and y=4. y=6 is past them.
        assert_eq!(row_at(area, 6, 2, 0, 3, 0), ClickTarget::Section(SectionFocus::Sessions));
        // Agents holds 1 row, y=15..=16. y=19 is past it.
        assert_eq!(row_at(area, 19, 2, 0, 1, 0), ClickTarget::Section(SectionFocus::Agents));
    }

    /// A scrolled section resolves through its offset: the first visible row is
    /// item `offset`, not item 0.
    #[test]
    fn a_click_in_a_scrolled_section_resolves_through_its_offset() {
        let area = body(20);

        assert_eq!(
            row_at(area, 3, 40, 12, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 12
            }
        );
        assert_eq!(
            row_at(area, 15, 40, 12, 30, 7),
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
            row_at(area, 3, 5, 0, 3, 0),
            ClickTarget::Row {
                section: SectionFocus::Sessions,
                index: 0
            }
        );
        assert_eq!(row_at(area, 99, 5, 0, 3, 0), ClickTarget::None);
    }
}

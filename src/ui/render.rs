//! Rendering the switcher: the modal frame, search bar, compact session/window
//! list, help panel, and prompts.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::{
    layout::{dock_layout, inset_rect, switcher_layout_for_input},
    pane::Pane,
    sections::{rows_per_height, section_heights, Row, RowKind, SectionFocus},
    state::{
        char_byte_index, compact_card_positions, compact_lines, numbered_session_index,
        CompactLine, GridState, InputMode, PromptState, ViewMode,
    },
};
use crate::{
    cards::compact_tab_process_text,
    model::{format_agent_state, AgentState, AgentStatus, SessionGroup, WindowCard},
    preview::PreviewMirror,
    tmux::unix_timestamp,
    TMUX_ORANGE,
};

const SEARCH_PLACEHOLDER: &str = "type to filter";
/// Counts only address rows the compact list numbers, so the `[n]j/k` hint
/// belongs to the palette alone.
const KEYS_PLACEHOLDER: &str = "j/k move · [n]j/k open";
const SECTIONS_KEYS_PLACEHOLDER: &str = "j/k move · tab section";
// Shown in the modal top bar; the short form fits beside "[?] Help" within
// the narrow sidebar's width.
const SWITCHER_NAME: &str = "agent-switcher";
const HELP_LABEL: &str = "[?] Help";

/// Which host the switcher is rendering into. The popup owns the whole screen
/// and draws a modal frame over it; the dock is one pane beside the work you
/// are doing and draws none of that.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Consumed by Task 4.
pub(crate) enum Surface {
    Popup,
    Dock,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    frame: &mut Frame,
    sessions: &[SessionGroup],
    state: &GridState,
    view: ViewMode,
    input: InputMode,
    show_help: bool,
    query: &str,
    pending_vim_count: Option<usize>,
    prompt: Option<&PromptState>,
    preview: &PreviewMirror,
    spinner_frame: usize,
    sessions_pane: &Pane<Row>,
    agents_pane: &Pane<Row>,
    focus: SectionFocus,
    surface: Surface,
) {
    let layout = match surface {
        Surface::Dock => dock_layout(frame.size(), show_help, input),
        Surface::Popup => switcher_layout_for_input(
            frame.size(),
            show_help,
            view,
            compact_lines(sessions).len(),
            input,
        ),
    };

    // The dock is one pane beside the work, not an overlay: no preview to put
    // next to it, and no modal frame to draw over anything.
    if surface == Surface::Popup {
        render_selected_preview(frame, layout.preview, preview);
        frame.render_widget(Clear, layout.list_overlay);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::DarkGray)),
            layout.list_overlay,
        );
        render_modal_top_bar(frame, layout.list_overlay);
    }
    render_search_bar(frame, layout.search, query, input, view);
    // A search filtering everything out is checked first, ahead of the
    // sections/compact split, so the sidebar shows the same feedback the
    // palette always has rather than a bare "Sessions" title with no rows.
    if sessions.is_empty() {
        render_no_matches(frame, layout.sessions);
    } else if uses_sections(view) {
        render_sections(
            frame,
            layout.sessions,
            sessions_pane,
            agents_pane,
            focus,
            spinner_frame,
        );
    } else {
        render_compact_with_mode(
            frame,
            layout.sessions,
            sessions,
            state,
            input,
            query,
            pending_vim_count,
            spinner_frame,
        );
    }
    if let Some(help) = layout.help {
        render_help(frame, help, view);
    }
    if let Some(prompt) = prompt {
        render_prompt(frame, frame.size(), layout.list_overlay, prompt);
    }
}

/// Whether this view draws the two sections rather than the flat compact list.
/// Mirrors `SwitcherUi::uses_sections`; the keymap the help panel and the Keys
/// hint describe differs between the two.
fn uses_sections(view: ViewMode) -> bool {
    matches!(view, ViewMode::Sidebar | ViewMode::SidebarRight)
}

/// The telescope-style prompt line on the bottom row of the list box, with a
/// separator rule above it: `❯ query▏`, or a dim hint while the query is empty.
fn render_search_bar(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    input: InputMode,
    view: ViewMode,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    if area.height > 1 {
        frame.render_widget(
            Paragraph::new("─".repeat(area.width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            Rect { height: 1, ..area },
        );
    }
    let prompt_row = Rect {
        y: area.y.saturating_add(area.height.saturating_sub(1)),
        height: 1,
        ..area
    };

    // Prompt (2 cells) + cursor block (1 cell); keep the tail of an overlong
    // query visible, like a real input field.
    let budget = area.width.saturating_sub(3) as usize;
    let value = query;
    let value_width = value.chars().count();
    let visible: String = if value_width > budget {
        let tail: String = value
            .chars()
            .skip(value_width + 1 - budget.max(1))
            .collect();
        format!("…{tail}")
    } else {
        value.to_owned()
    };

    let prompt_style = Style::default()
        .fg(TMUX_ORANGE)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled("❯ ", prompt_style)];
    if !visible.is_empty() {
        spans.push(Span::styled(
            visible.clone(),
            Style::default().fg(Color::White),
        ));
    }
    if input == InputMode::Search {
        spans.push(Span::styled(" ", Style::default().bg(Color::Gray)));
    }
    let hint = match (input, visible.is_empty()) {
        (InputMode::Search, true) => Some(SEARCH_PLACEHOLDER),
        (InputMode::Keys, true) => Some(if uses_sections(view) {
            SECTIONS_KEYS_PLACEHOLDER
        } else {
            KEYS_PLACEHOLDER
        }),
        (InputMode::Numbers, true) => None,
        (InputMode::Keys | InputMode::Numbers, false) => None,
        (InputMode::Search, false) => None,
    };
    if let Some(hint) = hint {
        spans.push(Span::styled(
            truncate_chars(hint, budget.saturating_sub(visible.chars().count() + 1)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), prompt_row);
}

fn render_no_matches(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(" no matching windows").style(Style::default().fg(Color::DarkGray)),
        Rect { height: 1, ..area },
    );
}

fn render_selected_preview(frame: &mut Frame, area: Rect, preview: &PreviewMirror) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Paragraph::new(preview.text.clone()), area);
}

/// Draws the Sessions and Agents sections into `body`. The unfocused section
/// is dimmed so it is obvious which one the keyboard drives.
pub(crate) fn render_sections(
    frame: &mut Frame,
    body: Rect,
    sessions: &Pane<Row>,
    agents: &Pane<Row>,
    focus: SectionFocus,
    spinner_frame: usize,
) {
    let (sessions_area, agents_area) = section_heights(body);

    render_section(
        frame,
        sessions_area,
        "Sessions",
        sessions,
        focus == SectionFocus::Sessions,
        spinner_frame,
        "no sessions",
    );

    if let Some(agents_area) = agents_area {
        render_section(
            frame,
            agents_area,
            "Agents",
            agents,
            focus == SectionFocus::Agents,
            spinner_frame,
            "none running",
        );
    }
}

fn render_section(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    pane: &Pane<Row>,
    focused: bool,
    spinner_frame: usize,
    empty_hint: &str,
) {
    if area.height == 0 {
        return;
    }

    // Both titles stay white whether or not their section has focus. They are
    // headings, not state: dimming one made the sidebar look half switched-off,
    // and which section holds the keyboard is already said by its rows going
    // grey and by the cursor being drawn there.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title.to_owned(),
            Style::default().fg(Color::White),
        ))),
        Rect {
            height: 1,
            ..area
        },
    );

    let rows_area = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };
    if rows_area.height == 0 {
        return;
    }

    // The section keeps its half whether or not it has rows, so an empty one
    // has to say why it is empty — a reserved but blank half reads as broken.
    if pane.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {empty_hint}"),
                Style::default().fg(Color::DarkGray),
            ))),
            rows_area,
        );
        return;
    }

    // Rows are two screen lines tall, so the pane is asked how many *rows*
    // fit rather than how many lines.
    let visible = pane.visible_range(rows_per_height(rows_area.height));
    let lines: Vec<Line> = pane.items()[visible.clone()]
        .iter()
        .enumerate()
        .flat_map(|(offset, row)| {
            let selected = focused && visible.start + offset == pane.cursor;
            section_row_lines(row, selected, focused, spinner_frame, rows_area.width)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), rows_area);
}

/// [`truncate_chars`] with a trailing `…` marking what was cut, per spec §4.
fn truncate_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.chars().count() <= max_width {
        return text.to_owned();
    }
    format!("{}…", truncate_chars(text, max_width - 1))
}

/// Fits a row into `width` columns. The right-hand cell — a session's window
/// count and attached marker, or an agent's tool — is the whole reason the row
/// carries more than a name, so it stays whole and right-aligned at the edge
/// (spec §3) while the name absorbs the truncation.
fn fit_row_text(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if right.is_empty() {
        return truncate_ellipsis(left, width);
    }

    let right_width = right.chars().count();
    // At least one space separates the two cells; with less room than that the
    // right cell is all there is space for.
    let Some(left_budget) = width
        .checked_sub(right_width + 1)
        .filter(|budget| *budget > 0)
    else {
        return truncate_ellipsis(right, width);
    };

    let left = truncate_ellipsis(left, left_budget);
    let padding = width - right_width - left.chars().count();
    format!("{left}{}{right}", " ".repeat(padding))
}

/// One row as its [`ROW_LINES`] screen lines: a name line, and a dim detail
/// line beneath it. The pair reads as one entry, which is what gives the list
/// its rhythm — a status and a tool crammed onto the name line cost more than
/// they explained.
fn section_row_lines(
    row: &Row,
    selected: bool,
    focused: bool,
    spinner_frame: usize,
    width: u16,
) -> Vec<Line<'static>> {
    let icon = agent_status_icon(row.status, spinner_frame);
    // The icon and the space after it are drawn as their own span, so they sit
    // outside the text budget.
    let body_width = (width as usize).saturating_sub(icon.chars().count() + 1);
    let body = match &row.kind {
        RowKind::Session {
            name, attached, ..
        } => fit_row_text(
            name,
            if *attached { "\u{25b8}" } else { "" },
            body_width,
        ),
        RowKind::Window {
            index,
            name,
            last_child,
        } => fit_row_text(
            &format!(
                "  {} {index}: {name}",
                if *last_child { "\u{2514}\u{2500}>" } else { "\u{251c}\u{2500}>" }
            ),
            "",
            body_width,
        ),
        RowKind::Agent { window_name, .. } => truncate_ellipsis(window_name, body_width),
    };

    // The second line: what the entry is doing, not what it is called.
    let detail = match &row.kind {
        RowKind::Session {
            window_count,
            expanded,
            ..
        } => format!(
            "{window_count} window{} {}",
            if *window_count == 1 { "" } else { "s" },
            if *expanded { "\u{25be}" } else { "\u{25b8}" }
        ),
        RowKind::Window { .. } => String::new(),
        RowKind::Agent { tool, .. } => {
            format!("{} \u{b7} {tool}", format_agent_state(row.status.state))
        }
    };

    // The name line: full contrast when the section has the keyboard, one step
    // down when it does not — but still clearly readable, since the unfocused
    // section is the one you are most likely reading rather than driving.
    let mut style = if focused {
        Style::default()
    } else {
        Style::default().fg(Color::Gray)
    };
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }

    // The status icon keeps its colour in the unfocused section. Dimming it too
    // de-emphasised the one thing the Agents section exists to show — whether an
    // agent is blocked — and the row text going grey already says which section
    // has the keyboard.
    let icon_style = agent_status_style(row.status);

    let indent = " ".repeat(icon.chars().count() + 1);
    vec![
        Line::from(vec![
            Span::styled(format!("{icon} "), icon_style),
            Span::styled(body, style),
        ]),
        Line::from(Span::styled(
            format!("{indent}{}", truncate_ellipsis(&detail, body_width)),
            detail_style(row, focused),
        )),
    ]
}

/// The detail line sits one step below the name above it, and never below
/// `DarkGray`.
///
/// It used to render `Color::Black` in an unfocused section, which on a dark
/// background is not dim — it is gone. Two steps of contrast say "secondary"
/// well enough without making the text unreadable, and the section that has
/// the keyboard is already obvious from its cursor.
///
/// A blocked agent keeps its colour here either way, because "blocked" is the
/// word worth noticing whether or not you are looking at that section.
fn detail_style(row: &Row, focused: bool) -> Style {
    if matches!(row.kind, RowKind::Agent { .. }) && row.status.state == AgentState::Blocked {
        return agent_status_style(row.status);
    }
    if focused {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_compact_with_mode(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionGroup],
    state: &GridState,
    input: InputMode,
    numbered_input: &str,
    pending_vim_count: Option<usize>,
    spinner_frame: usize,
) {
    let lines = compact_lines(sessions);
    let card_positions = compact_card_positions(sessions);
    let selected_position = card_positions
        .iter()
        .position(|&(row, column)| row == state.selected_row && column == state.selected_column)
        .unwrap_or(0);
    let numbered_windows = input == InputMode::Numbers;
    let show_session_numbers = input == InputMode::Numbers;
    let number_width = if numbered_windows {
        sessions
            .iter()
            .map(|session| session.cards.len())
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
    } else {
        card_positions.len().saturating_sub(1).to_string().len()
    };
    let highlighted_session = if numbered_windows && numbered_input.ends_with(',') {
        numbered_session_index(numbered_input, sessions)
    } else {
        None
    };
    let pending_vim_prefix = match (input, pending_vim_count) {
        (InputMode::Keys, Some(count)) => Some(count.to_string()),
        _ => None,
    };
    let start = state.row_offset.min(lines.len().saturating_sub(1));
    let visible_rows = (area.height as usize).min(lines.len().saturating_sub(start));
    if visible_rows == 0 {
        return;
    }

    let bottom_padding = compact_bottom_padding(&lines, start, visible_rows, area.height as usize);
    let mut row_y = area.y.saturating_add(bottom_padding);
    let mut rendered_rows = 0;
    if area.height > 1 {
        if let Some(CompactLine::Card { session_index, .. }) = lines.get(start).copied() {
            if let Some(session) = sessions.get(session_index) {
                render_compact_session(
                    frame,
                    area,
                    row_y,
                    session,
                    session_index,
                    show_session_numbers,
                    highlighted_session == Some(session_index),
                );
                row_y = row_y.saturating_add(1);
                rendered_rows += 1;
            }
        }
    }

    for visible_row_index in 0..visible_rows {
        if rendered_rows >= area.height as usize {
            break;
        }
        let line = lines[start + visible_row_index];
        let row_area = Rect {
            x: area.x,
            y: row_y,
            width: area.width,
            height: 1,
        };

        match line {
            CompactLine::Session { session_index } => {
                let Some(session) = sessions.get(session_index) else {
                    continue;
                };
                render_compact_session(
                    frame,
                    row_area,
                    row_area.y,
                    session,
                    session_index,
                    show_session_numbers,
                    highlighted_session == Some(session_index),
                );
            }
            CompactLine::Card {
                session_index,
                card_index,
            } => {
                let Some(card) = sessions
                    .get(session_index)
                    .and_then(|session| session.cards.get(card_index))
                else {
                    continue;
                };
                let selected = highlighted_session.is_none()
                    && session_index == state.selected_row
                    && card_index == state.selected_column;
                let card_position = card_positions
                    .iter()
                    .position(|&(row, column)| row == session_index && column == card_index)
                    .unwrap_or(0);
                let display_number = if numbered_windows {
                    card_index + 1
                } else {
                    card_position.abs_diff(selected_position)
                };
                let matching_prefix_len = pending_vim_prefix.as_ref().and_then(|prefix| {
                    display_number
                        .to_string()
                        .starts_with(prefix.as_str())
                        .then_some(prefix.len())
                });
                let vim_count_hint = match (input, matching_prefix_len) {
                    (InputMode::Keys, Some(prefix_len)) => {
                        match card_position.cmp(&selected_position) {
                            std::cmp::Ordering::Less => VimCountHint::Up { prefix_len },
                            std::cmp::Ordering::Greater => VimCountHint::Down { prefix_len },
                            std::cmp::Ordering::Equal => VimCountHint::Idle,
                        }
                    }
                    (InputMode::Keys, None) => VimCountHint::Idle,
                    (InputMode::Search | InputMode::Numbers, _) => VimCountHint::Hidden,
                };
                frame.render_widget(
                    Paragraph::new(compact_tab_line(
                        card,
                        selected,
                        display_number,
                        number_width,
                        vim_count_hint,
                        row_area.width,
                        spinner_frame,
                    ))
                    .style(compact_tab_style(selected, card.codex_unread)),
                    row_area,
                );
            }
        }

        row_y = row_y.saturating_add(1);
        rendered_rows += 1;
    }
}

fn compact_bottom_padding(
    lines: &[CompactLine],
    start: usize,
    visible_rows: usize,
    area_height: usize,
) -> u16 {
    if start == 0 && lines.len() <= visible_rows && lines.len() < area_height {
        (area_height - lines.len()).min(u16::MAX as usize) as u16
    } else {
        0
    }
}

fn render_compact_session(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    session: &SessionGroup,
    session_index: usize,
    show_number: bool,
    highlighted: bool,
) {
    let label = if show_number {
        format!("{} {}", session_index + 1, compact_session_label(session))
    } else {
        compact_session_label(session)
    };
    let style = if highlighted {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(label).style(style),
        Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        },
    );
}

fn compact_session_label(session: &SessionGroup) -> String {
    display_session_name(&session.session_name)
}

fn display_session_name(session_name: &str) -> String {
    session_name.replace('-', " ")
}

fn text_width(text: &str) -> u16 {
    text.chars().count().min(u16::MAX as usize) as u16
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VimCountHint {
    Hidden,
    Idle,
    Up { prefix_len: usize },
    Down { prefix_len: usize },
}

impl VimCountHint {
    fn motion(self) -> &'static str {
        match self {
            Self::Hidden => "",
            Self::Idle => " ",
            Self::Up { .. } => "k",
            Self::Down { .. } => "j",
        }
    }

    fn highlighted_prefix_len(self) -> usize {
        match self {
            Self::Up { prefix_len } | Self::Down { prefix_len } => prefix_len,
            Self::Hidden | Self::Idle => 0,
        }
    }
}

fn compact_tab_line(
    card: &WindowCard,
    selected: bool,
    relative_number: usize,
    number_width: usize,
    vim_count_hint: VimCountHint,
    width: u16,
    spinner_frame: usize,
) -> Line<'static> {
    let now = unix_timestamp();
    let label = compact_tab_left_text_at(
        card,
        relative_number,
        number_width,
        vim_count_hint,
        spinner_frame,
        now,
    );
    let runtime = agent_runtime_cell(card.agent_status, now);
    let process = compact_tab_process_text(card);
    let right_width = text_width(&compact_tab_right_text(card, now)) as usize;
    let label_width = text_width(&label) as usize;
    let width = width as usize;
    let padding = if width > label_width + right_width {
        width - label_width - right_width
    } else {
        1
    };
    let icon_style = if selected {
        Style::default().fg(Color::Black)
    } else {
        agent_status_style(card.agent_status)
    };
    let number = relative_number.to_string();
    let highlighted_prefix_len = vim_count_hint.highlighted_prefix_len().min(number.len());
    let (highlighted_prefix, number_suffix) = number.split_at(highlighted_prefix_len);
    let number_padding = format!(" {}", " ".repeat(number_width.saturating_sub(number.len())));

    Line::from(vec![
        Span::raw(number_padding),
        Span::styled(
            highlighted_prefix.to_owned(),
            Style::default()
                .fg(TMUX_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(number_suffix.to_owned()),
        Span::styled(
            vim_count_hint.motion(),
            Style::default()
                .fg(TMUX_ORANGE)
                .add_modifier(Modifier::DIM)
                .remove_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            agent_status_icon(card.agent_status, spinner_frame).to_owned(),
            icon_style,
        ),
        Span::raw(format!(" {}", card.window_name)),
        Span::raw(" ".repeat(padding)),
        Span::raw(runtime),
        Span::raw(" "),
        Span::raw(process),
    ])
}

fn compact_tab_left_text_at(
    card: &WindowCard,
    relative_number: usize,
    number_width: usize,
    vim_count_hint: VimCountHint,
    spinner_frame: usize,
    _now: u64,
) -> String {
    let hint = vim_count_hint.motion();
    format!(
        " {:>number_width$}{hint} {} {}",
        relative_number,
        agent_status_icon(card.agent_status, spinner_frame),
        card.window_name
    )
}

fn compact_tab_right_text(card: &WindowCard, now: u64) -> String {
    format!(
        "{} {}",
        agent_runtime_cell(card.agent_status, now),
        compact_tab_process_text(card)
    )
}

fn agent_runtime_label(status: AgentStatus, now: u64) -> Option<String> {
    if !matches!(status.state, AgentState::Working | AgentState::Blocked) {
        return None;
    }

    let elapsed = now.saturating_sub(status.run_started_at?);
    Some(if elapsed < 60 {
        "0m".to_owned()
    } else if elapsed < 60 * 60 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 48 * 60 * 60 {
        format!("{}h", elapsed / 3600)
    } else {
        format!("{}d", elapsed / 86_400)
    })
}

fn agent_runtime_cell(status: AgentStatus, now: u64) -> String {
    let label = agent_runtime_label(status, now).unwrap_or_default();
    let width = text_width(&label) as usize;
    if width >= 3 {
        label
    } else {
        format!("{}{}", label, " ".repeat(3 - width))
    }
}

fn agent_status_icon(status: AgentStatus, spinner_frame: usize) -> &'static str {
    const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    match status.state {
        AgentState::Blocked => "◉",
        AgentState::Working => SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
        AgentState::Idle if !status.seen => "●",
        AgentState::Idle => "✓",
        AgentState::Unknown => "○",
    }
}

fn agent_status_style(status: AgentStatus) -> Style {
    match status.state {
        AgentState::Blocked => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentState::Working => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        AgentState::Idle if !status.seen => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        AgentState::Idle => Style::default().fg(Color::Green),
        AgentState::Unknown => Style::default().fg(Color::DarkGray),
    }
}

fn compact_tab_style(selected: bool, codex_unread: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if codex_unread {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    }
}

fn truncate_chars(text: &str, max_width: usize) -> String {
    text.chars().take(max_width).collect()
}

fn render_modal_top_bar(frame: &mut Frame, area: Rect) {
    if area.width < 4 || area.height == 0 {
        return;
    }

    let inner_width = area.width.saturating_sub(2) as usize;
    let help_width = HELP_LABEL.chars().count();
    let right = if inner_width >= help_width {
        HELP_LABEL.to_owned()
    } else {
        truncate_chars(HELP_LABEL, inner_width)
    };
    let left_budget = inner_width.saturating_sub(right.chars().count());
    let left = truncate_chars(SWITCHER_NAME, left_budget);

    frame.render_widget(
        Paragraph::new(left.clone()).style(
            Style::default()
                .fg(TMUX_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Rect {
            x: area.x.saturating_add(1),
            y: area.y,
            width: left.chars().count() as u16,
            height: 1,
        },
    );

    if !right.is_empty() {
        let right_width = right.chars().count() as u16;
        frame.render_widget(
            Paragraph::new(right).style(Style::default().fg(Color::Gray)),
            Rect {
                x: area
                    .x
                    .saturating_add(area.width.saturating_sub(right_width + 1)),
                y: area.y,
                width: right_width,
                height: 1,
            },
        );
    }
}

/// The two sidebar views and the palette no longer share a keymap: `tab`
/// focuses a section in the sidebar but cycles the input mode in the palette,
/// and `h`/`l` collapse and expand rather than moving between sessions. The
/// panel documents whichever one is on screen.
const SECTIONS_HELP: [&str; 14] = [
    "Shortcuts",
    "tab: focus section",
    "S-tab: keys/search",
    "v: cycle view",
    "j/k · ↑/↓: move",
    "h/l ←/→ C-h/C-l: fold",
    "enter/space: open",
    "C-j/C-k: move and open",
    "S-j/S-k: reorder session",
    "M-j/M-k: reorder window",
    "r: rename window",
    "C-t/C-s: new win/sess",
    "C-u: clear filter",
    "esc: clear, then close",
];

const PALETTE_HELP: [&str; 14] = [
    "Shortcuts",
    "tab: vim / nums / search",
    "v: cycle view",
    "nums: session → window",
    "vim: j/k · [n]j/k open",
    "S-j/S-k: reorder session",
    "M-j/M-k: reorder window",
    "H/L: previous/next edge",
    "↑/↓: move, C-j/C-k: open",
    "←/→: switch session",
    "enter/r: open/rename",
    "C-t/C-s: new win/sess",
    "C-u: clear filter",
    "esc: clear, then close",
];

fn render_help(frame: &mut Frame, area: Rect, view: ViewMode) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let lines = if uses_sections(view) {
        SECTIONS_HELP
    } else {
        PALETTE_HELP
    };
    let text = lines
        .into_iter()
        .take(area.height as usize)
        .map(|line| truncate_chars(line, area.width as usize))
        .collect::<Vec<_>>()
        .join("\n");

    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn render_prompt(frame: &mut Frame, screen: Rect, sidebar: Rect, prompt: &PromptState) {
    if sidebar.width < 8 || screen.height < 3 {
        return;
    }

    let available_width = screen
        .width
        .saturating_sub(sidebar.x.saturating_sub(screen.x))
        .saturating_sub(4);
    let width = available_width
        .min(44)
        .max(sidebar.width.saturating_sub(4))
        .max(1);
    // Hang the prompt right under the list box when it floats (palette view);
    // when the list spans the full screen height (sidebar view) fall back to
    // the bottom edge.
    let below_list = sidebar.y.saturating_add(sidebar.height);
    let y = if below_list.saturating_add(3) <= screen.y.saturating_add(screen.height) {
        below_list
    } else {
        screen.y.saturating_add(screen.height.saturating_sub(3))
    };
    let area = Rect {
        x: sidebar.x.saturating_add(2),
        y,
        width,
        height: 3,
    };
    let mut editable_input = prompt.input.clone();
    editable_input.insert(char_byte_index(&editable_input, prompt.cursor), '▏');
    let line = format!("{}: {editable_input}", prompt.title());

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(TMUX_ORANGE)),
        area,
    );
    frame.render_widget(
        Paragraph::new(truncate_chars(&line, area.width.saturating_sub(2) as usize))
            .style(Style::default().fg(Color::White)),
        inset_rect(area, 1),
    );
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::text::Text;
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    use super::*;
    use crate::{
        cards::group_cards_by_session,
        model::{AgentKind, AgentStatus},
        test_support::test_card,
        ui::state::PromptKind,
    };
    use crate::ui::pane::Pane;
    use crate::ui::sections::{agent_rows, session_rows, SectionFocus};
    use std::collections::HashSet;

    fn test_preview() -> PreviewMirror {
        PreviewMirror {
            text: Text::from("preview"),
            ..PreviewMirror::default()
        }
    }

    /// The Sessions/Agents panes `draw` now expects, built the same way
    /// `SwitcherUi::rebuild_panes` builds them from the loaded cards.
    fn panes_from(groups: &[SessionGroup]) -> (Pane<Row>, Pane<Row>) {
        (
            Pane::new(session_rows(groups, &HashSet::new(), None)),
            Pane::new(agent_rows(groups)),
        )
    }

    fn render_compact(
        frame: &mut Frame,
        area: Rect,
        sessions: &[SessionGroup],
        state: &GridState,
        spinner_frame: usize,
    ) {
        render_compact_with_mode(
            frame,
            area,
            sessions,
            state,
            InputMode::Search,
            "",
            None,
            spinner_frame,
        );
    }

    fn compact_session_label_width<'a>(
        sessions: impl Iterator<Item = (usize, &'a SessionGroup)>,
    ) -> u16 {
        sessions
            .map(|(_, session)| compact_session_label(session))
            .map(|label| text_width(&label))
            .max()
            .unwrap_or(0)
    }

    fn compact_tab_label(card: &WindowCard, relative_number: usize, width: u16) -> String {
        compact_tab_label_at(card, relative_number, width, 0, unix_timestamp())
    }

    fn compact_tab_label_at(
        card: &WindowCard,
        relative_number: usize,
        width: u16,
        spinner_frame: usize,
        now: u64,
    ) -> String {
        let label = compact_tab_left_text_at(
            card,
            relative_number,
            relative_number.to_string().len(),
            VimCountHint::Hidden,
            spinner_frame,
            now,
        );
        let process = compact_tab_right_text(card, now);
        let label_width = text_width(&label) as usize;
        let process_width = text_width(&process) as usize;
        let width = width as usize;

        if width > label_width + process_width {
            format!(
                "{}{}{}",
                label,
                " ".repeat(width - label_width - process_width),
                process
            )
        } else {
            format!("{label} {process}")
        }
    }

    #[test]
    fn selected_compact_tab_uses_white_background() {
        let style = compact_tab_style(true, false);

        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::White));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(!style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn draw_uses_modal_top_bar_without_bottom_status_line() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let modal_top = (0..100)
            .map(|x| buffer.get(x, 0).symbol())
            .collect::<String>();
        let footer = (0..100)
            .map(|x| buffer.get(x, 39).symbol())
            .collect::<String>();

        assert!(modal_top.contains("agent-switcher"));
        assert!(!footer.contains("agent-switcher"));
        assert!(!footer.contains("j/k=window"));
    }

    #[test]
    fn draw_renders_shortcuts_under_sessions_when_help_is_open() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    true,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("Shortcuts"));
        assert!(rendered.contains("tab: focus section"));
        assert!(rendered.contains("enter"));
    }

    #[test]
    fn sidebar_sections_render_top_anchored_inside_the_modal() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "┌");
        assert_eq!(buffer.get(27, 0).symbol(), "┐");
        assert_eq!(buffer.get(0, 39).symbol(), "└");
        assert_eq!(buffer.get(27, 39).symbol(), "┘");
        // Sections render top-anchored (unlike the old compact list, which
        // pushed short content down to the search bar): the "Sessions" title
        // sits right under the top border, with the row directly beneath it.
        assert_eq!(buffer.get(2, 2).symbol(), "S");
        assert_eq!(buffer.get(2, 3).symbol(), "○");
        assert_eq!(buffer.get(4, 3).symbol(), "w");
        assert_eq!(buffer.get(2, 36).symbol(), "─");
        assert_eq!(buffer.get(2, 37).symbol(), "❯");
    }

    #[test]
    fn draw_renders_palette_box_over_fullscreen_preview() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Palette,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        // The preview fills the screen behind the palette, from the top-left.
        assert_eq!(buffer.get(0, 0).symbol(), "p");
        // The palette floats centered, bottom edge anchored above mid-screen.
        assert_eq!(buffer.get(22, 14).symbol(), "┌");
        assert_eq!(buffer.get(76, 14).symbol(), "┐");
        assert_eq!(buffer.get(22, 21).symbol(), "└");
        assert_eq!(buffer.get(76, 21).symbol(), "┘");
        // List content on top: session header, then window row.
        assert_eq!(buffer.get(24, 16).symbol(), "w");
        assert_eq!(buffer.get(25, 17).symbol(), "0");
        // Search bar at the bottom: separator rule, then the prompt.
        assert_eq!(buffer.get(24, 18).symbol(), "─");
        assert_eq!(buffer.get(24, 19).symbol(), "❯");

        let box_top = (22..77)
            .map(|x| buffer.get(x, 14).symbol())
            .collect::<String>();
        assert!(box_top.contains("agent-switcher"));
    }

    #[test]
    fn draw_renders_query_text_and_no_matches_hint() {
        let groups = group_cards_by_session(Vec::new());
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Palette,
                    InputMode::Search,
                    false,
                    "zzz",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("❯ zzz"));
        assert!(rendered.contains("no matching windows"));
    }

    #[test]
    fn help_renders_one_shortcut_per_row() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    true,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let help_rows = (0..40)
            .map(|y| {
                (0..100)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect::<String>()
            })
            .filter(|row| {
                row.contains("tab:")
                    || row.contains("v: cycle")
                    || row.contains("↑/↓")
                    || row.contains("←/→")
                    || row.contains("enter/")
                    || row.contains("C-j/C-k")
                    || row.contains("S-j/S-k:")
                    || row.contains("M-j/M-k:")
                    || row.contains("r: rename")
                    || row.contains("C-t/C-s:")
                    || row.contains("C-u:")
                    || row.contains("esc:")
            })
            .collect::<Vec<_>>();

        // The sidebar's own keymap, not the palette's: `tab` focuses a
        // section here, the input-mode cycle moved to `S-tab`, the view cycle
        // to `v`, and `h`/`l` collapse and expand rather than moving between
        // sessions.
        assert_eq!(help_rows.len(), 13);
        assert!(help_rows
            .iter()
            .any(|row| row.contains("tab: focus section")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("S-tab: keys/search")));
        assert!(help_rows.iter().any(|row| row.contains("v: cycle view")));
        assert!(help_rows.iter().any(|row| row.contains("j/k · ↑/↓: move")));
        // C-h/C-l fold too, and reach the sections from every input mode —
        // documenting them is the whole reason this row lists four key forms.
        assert!(help_rows
            .iter()
            .any(|row| row.contains("h/l ←/→ C-h/C-l: fold")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("enter/space: open")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("C-j/C-k: move and open")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("S-j/S-k: reorder session")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("M-j/M-k: reorder window")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("r: rename window")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("C-t/C-s: new win/sess")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("C-u: clear filter")));
        assert!(help_rows.iter().any(|row| row.contains("esc: clear")));
        // The keys the sections do not bind must not be advertised.
        assert!(!help_rows.iter().any(|row| row.contains("H/L:")));
        assert!(!help_rows.iter().any(|row| row.contains("[n]j/k")));
    }

    /// The palette still runs the flat `GridState` list, so its help panel
    /// keeps documenting the keymap the sections retired.
    #[test]
    fn palette_help_keeps_the_flat_list_keymap() {
        let backend = TestBackend::new(30, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 26,
            height: 20,
        };
        terminal
            .draw(|frame| render_help(frame, area, ViewMode::Palette))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..20)
            .map(|y| {
                (0..30)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("tab: vim / nums / search"));
        assert!(rendered.contains("H/L: previous/next edge"));
        assert!(rendered.contains("←/→: switch session"));
        assert!(rendered.contains("vim: j/k · [n]j/k open"));
        assert!(!rendered.contains("tab: focus section"));
    }

    /// Spec §3 puts the window count (and the attached marker) at the right
    /// edge of a session row; spec §4 truncates the name with `…`. Before the
    /// row was width-budgeted both fell off the end of a real 28-column
    /// sidebar, taking away everything the row carried beyond its name.
    #[test]
    fn a_long_session_name_truncates_and_keeps_its_count_and_marker() {
        let mut card = test_card("a-very-long-session-name-indeed", "0");
        card.window_flags = "*".to_owned();
        let groups = group_cards_by_session(vec![card]);
        let rows = session_rows(&groups, &HashSet::new(), Some(&groups[0].cards[0].window_id));

        let lines = section_row_lines(&rows[0], false, true, 0, 26);
        let text = |line: &Line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        assert_eq!(lines.len(), 2, "a row is a name line and a detail line");
        let name = text(&lines[0]);
        assert_eq!(name.chars().count(), 26);
        assert!(name.ends_with('▸'), "the attached marker survives: {name:?}");
        assert!(name.contains('…'), "the name truncates: {name:?}");
        assert!(name.contains("a-very-long"));
        // The count moved to the detail line, where it has room to spell itself.
        assert!(text(&lines[1]).contains("1 window"), "{:?}", text(&lines[1]));
    }

    /// The agent row's tool name is its right-hand cell for the same reason.
    #[test]
    fn a_long_agent_window_name_truncates_and_keeps_its_tool() {
        let mut card = test_card("work", "0");
        card.window_name = "an-extremely-long-window-name".to_owned();
        card.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            ..AgentStatus::unknown()
        };
        let groups = group_cards_by_session(vec![card]);
        let rows = agent_rows(&groups);

        let lines = section_row_lines(&rows[0], false, true, 0, 26);
        let text = |line: &Line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        assert_eq!(lines.len(), 2);
        let name = text(&lines[0]);
        assert!(name.contains('…'), "the window name truncates: {name:?}");
        // The tool is on the detail line now, beside the agent's state, so a
        // long window name can no longer crowd it out.
        let detail = text(&lines[1]);
        assert!(detail.contains("claude"), "tool survives: {detail:?}");
    }

    /// A width too small for both cells keeps the right-hand one, and never
    /// overruns the row.
    #[test]
    fn row_text_never_exceeds_its_width() {
        assert_eq!(fit_row_text("session", "12 ▸", 4), "12 ▸");
        assert_eq!(fit_row_text("session", "12 ▸", 3), "12…");
        assert_eq!(fit_row_text("session", "12", 0), "");
        assert_eq!(fit_row_text("session", "", 4), "ses…");
        assert_eq!(fit_row_text("ab", "12", 10), "ab      12");
    }

    #[test]
    fn draw_renders_new_window_prompt_inside_modal() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let prompt = PromptState::with_input(
            PromptKind::NewWindow {
                session_name: "work".to_owned(),
            },
            "server".to_owned(),
        );
        let (sessions_pane, agents_pane) = panes_from(&groups);

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    Some(&prompt),
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("New window name: server▏"));
    }

    #[test]
    fn selected_compact_tab_label_is_a_plain_highlight_span() {
        let card = test_card("work", "2");

        assert_eq!(compact_tab_label(&card, 0, 24), " 0 ○ window-2        zsh");
        assert_eq!(compact_tab_label(&card, 0, 0), " 0 ○ window-2     zsh");
    }

    #[test]
    fn compact_tab_process_label_shortens_codex_arch_binary() {
        let mut card = test_card("work", "2");
        card.command = "codex-aarch64-a".to_owned();

        assert_eq!(compact_tab_label(&card, 0, 24), " 0 ○ window-2      codex");
    }

    #[test]
    fn compact_tab_process_label_normalizes_claude_version_binary() {
        let mut card = test_card("work", "2");
        card.command = "2.1.197".to_owned();

        assert_eq!(compact_tab_label(&card, 0, 24), " 0 ○ window-2     claude");
    }

    #[test]
    fn compact_tab_timer_sits_after_title_before_process_name() {
        let mut card = test_card("work", "2");
        card.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(880),
        };

        assert_eq!(
            compact_tab_label_at(&card, 0, 32, 0, 1000),
            " 0 ⠋ window-2            2m  zsh"
        );
    }

    #[test]
    fn compact_tab_timer_shows_zero_minutes_for_sub_minute_runs() {
        let mut card = test_card("work", "2");
        card.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(970),
        };

        assert_eq!(
            compact_tab_label_at(&card, 0, 32, 0, 1000),
            " 0 ⠋ window-2            0m  zsh"
        );
    }

    #[test]
    fn compact_tab_timer_uses_process_label_style() {
        let mut card = test_card("work", "2");
        card.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(unix_timestamp()),
        };

        let line = compact_tab_line(&card, false, 0, 1, VimCountHint::Hidden, 32, 0);
        let runtime = &line.spans[8];
        let process = &line.spans[10];

        assert!(runtime.content.ends_with(' '));
        assert_eq!(process.content, "zsh");
        assert_eq!(runtime.style, process.style);
    }

    #[test]
    fn working_status_icon_advances_with_spinner_frame() {
        let working = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };
        let idle = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };

        assert_ne!(agent_status_icon(working, 0), agent_status_icon(working, 1));
        assert_eq!(agent_status_icon(idle, 0), agent_status_icon(idle, 1));
    }

    #[test]
    fn compact_labels_include_relative_numbers_and_right_aligned_processes() {
        let groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
        ]);

        assert_eq!(compact_session_label(&groups[1]), "work");
        assert_eq!(
            compact_tab_label(&groups[1].cards[2], 3, 28),
            " 3 ○ window-3            zsh"
        );
    }

    #[test]
    fn compact_session_label_uses_natural_width() {
        let groups = group_cards_by_session(vec![test_card("agent-proxy", "1")]);

        assert_eq!(compact_session_label(&groups[0]), "agent proxy");
    }

    #[test]
    fn compact_session_label_width_uses_longest_visible_label() {
        let groups = group_cards_by_session(vec![
            test_card("a", "1"),
            test_card("long-session", "1"),
            test_card("mid", "1"),
        ]);

        assert_eq!(compact_session_label_width(groups.iter().enumerate()), 12);
    }

    #[test]
    fn draw_renders_preview_with_left_sidebar() {
        let mut selected = test_card("work", "1");
        selected.preview = vec!["preview line".to_owned()];
        selected.path = "/Users/example/project".to_owned();
        let groups = group_cards_by_session(vec![selected, test_card("ops", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(28, 0).symbol(), "p");
        assert_eq!(buffer.get(29, 0).symbol(), "r");
        assert_eq!(buffer.get(0, 0).symbol(), "┌");
        assert_eq!(buffer.get(27, 0).symbol(), "┐");
        assert_eq!(buffer.get(0, 39).symbol(), "└");
        assert_eq!(buffer.get(27, 39).symbol(), "┘");
        // Sections render top-anchored: "Sessions" title, then two-line rows —
        // a name line with the status icon, and a dim detail line beneath it.
        assert_eq!(buffer.get(2, 2).symbol(), "S");
        assert_eq!(buffer.get(2, 3).symbol(), "○");
        assert_eq!(buffer.get(4, 3).symbol(), "w");
        let detail = (0..28)
            .map(|x| buffer.get(x, 4).symbol())
            .collect::<String>();
        assert!(
            detail.contains("window"),
            "row 0's detail line should sit directly under its name: {detail:?}"
        );

        let modal_top = (0..100)
            .map(|x| buffer.get(x, 0).symbol())
            .collect::<String>();
        assert!(modal_top.contains("agent-switcher"));
        assert!(modal_top.contains("[?] Help"));
    }

    #[test]
    fn draw_clears_preview_through_right_edge() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let stale = "x".repeat(100);
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(stale), frame.size()))
            .unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(99, 0).symbol(), " ");
        assert_eq!(buffer.get(99, 7).symbol(), " ");
        assert_eq!(buffer.get(99, 0).bg, Color::Reset);
        assert_eq!(buffer.get(99, 7).bg, Color::Reset);
    }

    #[test]
    fn draw_renders_preview_mirror_from_terminal_top() {
        let mut selected = test_card("work", "1");
        selected.path = "/project".to_owned();
        selected.preview = vec!["last shell line".to_owned()];
        let groups = group_cards_by_session(vec![selected]);
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let top_row = (0..80)
            .map(|x| buffer.get(x, 0).symbol())
            .collect::<String>();

        assert!(top_row.contains("preview"));
        assert!(!top_row.contains("1:window-1"));
    }

    #[test]
    fn selected_compact_tab_renders_a_filled_background_chip() {
        let groups = group_cards_by_session(vec![test_card("work", "1"), test_card("work", "2")]);
        let mut state = GridState::new();
        state.selected_column = 1;
        state.preferred_column = 1;

        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_compact(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 3,
                    },
                    &groups,
                    &state,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in 0..80 {
            assert_eq!(buffer.get(x, 2).bg, Color::White, "cell {x} was not filled");
        }
    }

    #[test]
    fn compact_window_rows_start_at_left_edge_under_session_headers() {
        let groups =
            group_cards_by_session(vec![test_card("a", "1"), test_card("longer-name", "1")]);
        let mut state = GridState::new();
        state.selected_row = 1;

        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_compact(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 4,
                    },
                    &groups,
                    &state,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "a");
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(3, 1).symbol(), "○");
        assert_eq!(buffer.get(5, 1).symbol(), "w");
        assert_eq!(buffer.get(0, 2).symbol(), "l");
        assert_eq!(buffer.get(1, 3).symbol(), "0");
        assert_eq!(buffer.get(3, 3).symbol(), "○");
        assert_eq!(buffer.get(5, 3).symbol(), "w");
        assert_eq!(buffer.get(0, 3).bg, Color::White);
    }

    #[test]
    fn compact_session_label_is_not_highlighted_when_row_is_selected() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();

        let backend = TestBackend::new(40, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_compact(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 40,
                        height: 2,
                    },
                    &groups,
                    &state,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in 0..4 {
            assert_eq!(
                buffer.get(x, 0).bg,
                Color::Reset,
                "session label cell {x} was highlighted"
            );
        }
        assert_eq!(buffer.get(0, 1).bg, Color::White);
    }

    #[test]
    fn compact_mode_renders_sessions_as_a_vertical_tree() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;

        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_compact(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 5,
                    },
                    &groups,
                    &state,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "w");
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(3, 1).symbol(), "○");
        assert_eq!(buffer.get(5, 1).symbol(), "w");
        assert_eq!(buffer.get(1, 2).symbol(), "0");
        assert_eq!(buffer.get(3, 2).symbol(), "○");
        assert_eq!(buffer.get(5, 2).symbol(), "w");
        assert_eq!(buffer.get(0, 2).bg, Color::White);
        assert_eq!(buffer.get(0, 3).symbol(), "o");
        assert_eq!(buffer.get(1, 4).symbol(), "1");
    }

    #[test]
    fn keys_mode_keeps_relative_window_numbers_without_session_numbers() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;
        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_compact_with_mode(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 5,
                    },
                    &groups,
                    &state,
                    InputMode::Keys,
                    "",
                    None,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "w");
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(1, 2).symbol(), "0");
        assert_eq!(buffer.get(0, 3).symbol(), "o");
        assert_eq!(buffer.get(1, 4).symbol(), "1");
    }

    #[test]
    fn pending_vim_count_highlights_matching_prefixes_and_ghosts_valid_motions() {
        let cards = (1..=21)
            .map(|index| test_card("work", &index.to_string()))
            .collect();
        let groups = group_cards_by_session(cards);
        let mut state = GridState::new();
        state.selected_column = 10;
        let backend = TestBackend::new(80, 22);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_compact_with_mode(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 22,
                    },
                    &groups,
                    &state,
                    InputMode::Keys,
                    "",
                    Some(1),
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        // Distance 10 above: only the typed `1` prefix is highlighted.
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(1, 1).fg, TMUX_ORANGE);
        assert_eq!(buffer.get(2, 1).symbol(), "0");
        assert_eq!(buffer.get(2, 1).fg, Color::White);
        assert_eq!(buffer.get(3, 1).symbol(), "k");
        assert_eq!(buffer.get(3, 1).fg, TMUX_ORANGE);
        assert!(buffer.get(3, 1).modifier.contains(Modifier::DIM));

        // Exact distance 1 is also a match in both directions.
        assert_eq!(buffer.get(2, 10).symbol(), "1");
        assert_eq!(buffer.get(2, 10).fg, TMUX_ORANGE);
        assert_eq!(buffer.get(3, 10).symbol(), "k");
        assert_eq!(buffer.get(2, 12).symbol(), "1");
        assert_eq!(buffer.get(2, 12).fg, TMUX_ORANGE);
        assert_eq!(buffer.get(3, 12).symbol(), "j");

        // Nonmatching distances stay quiet.
        assert_eq!(buffer.get(2, 9).symbol(), "2");
        assert_eq!(buffer.get(2, 9).fg, Color::White);
        assert_eq!(buffer.get(3, 9).symbol(), " ");

        // Distance 10 below gets the same prefix-only treatment.
        assert_eq!(buffer.get(1, 21).symbol(), "1");
        assert_eq!(buffer.get(1, 21).fg, TMUX_ORANGE);
        assert_eq!(buffer.get(2, 21).symbol(), "0");
        assert_eq!(buffer.get(2, 21).fg, Color::White);
        assert_eq!(buffer.get(3, 21).symbol(), "j");

        terminal
            .draw(|frame| {
                render_compact_with_mode(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 22,
                    },
                    &groups,
                    &state,
                    InputMode::Keys,
                    "",
                    None,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(1, 1).fg, Color::White);
        assert_eq!(buffer.get(3, 1).symbol(), " ");
        assert_eq!(buffer.get(1, 21).fg, Color::White);
        assert_eq!(buffer.get(3, 21).symbol(), " ");
    }

    #[test]
    fn numbers_mode_labels_sessions_and_windows_from_one() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let state = GridState::new();
        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_compact_with_mode(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 5,
                    },
                    &groups,
                    &state,
                    InputMode::Numbers,
                    "",
                    None,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "1");
        assert_eq!(buffer.get(2, 0).symbol(), "w");
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(1, 2).symbol(), "2");
        assert_eq!(buffer.get(0, 3).symbol(), "2");
        assert_eq!(buffer.get(2, 3).symbol(), "o");
        assert_eq!(buffer.get(1, 4).symbol(), "1");
    }

    #[test]
    fn numbers_mode_highlights_the_session_before_choosing_a_window() {
        let groups = group_cards_by_session(vec![test_card("work", "1"), test_card("ops", "1")]);
        let mut state = GridState::new();
        state.selected_row = 1;
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_compact_with_mode(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 4,
                    },
                    &groups,
                    &state,
                    InputMode::Numbers,
                    "2,",
                    None,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 2).bg, Color::White);
        assert_eq!(buffer.get(0, 3).bg, Color::Reset);
    }

    #[test]
    fn compact_scrolled_session_header_stays_inside_viewport() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("work", "4"),
            test_card("work", "5"),
            test_card("work", "6"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 5;
        state.preferred_column = 5;
        crate::ui::state::keep_compact_selection_visible(&mut state, &groups, 4);

        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_compact(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 3,
                    },
                    &groups,
                    &state,
                    0,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "w");
        assert_eq!(buffer.get(1, 1).symbol(), "1");
        assert_eq!(buffer.get(3, 1).symbol(), "○");
        assert_eq!(buffer.get(5, 1).symbol(), "w");
        assert_eq!(buffer.get(1, 2).symbol(), "0");
        assert_eq!(buffer.get(3, 2).symbol(), "○");
        assert_eq!(buffer.get(5, 2).symbol(), "w");
        assert_eq!(buffer.get(0, 2).bg, Color::White);
    }

    #[test]
    fn sections_render_titles_and_rows_in_both_halves() {
        let mut agent_card = crate::test_support::test_card("dotfiles", "0");
        agent_card.window_name = "config".to_owned();
        agent_card.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let sessions_group = vec![SessionGroup {
            session_name: "dotfiles".to_owned(),
            cards: vec![agent_card],
        }];
        let sessions = Pane::new(session_rows(&sessions_group, &HashSet::new(), None));
        let agents = Pane::new(agent_rows(&sessions_group));

        let backend = ratatui::backend::TestBackend::new(28, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_sections(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 28,
                        height: 12,
                    },
                    &sessions,
                    &agents,
                    SectionFocus::Sessions,
                    0,
                );
            })
            .unwrap();

        let rendered = terminal.backend().buffer().content();
        let text: String = rendered.iter().map(|cell| cell.symbol()).collect();

        assert!(text.contains("Sessions"), "missing Sessions title: {text}");
        assert!(text.contains("Agents"), "missing Agents title: {text}");
        assert!(text.contains("dotfiles"), "missing session row: {text}");
        assert!(text.contains("claude"), "missing agent row: {text}");
    }

    /// Regression test for a review finding: a search that filters the
    /// sidebar's session list to zero results used to render a bare
    /// "Sessions" title with no rows and no feedback. `draw` must route
    /// through the same `render_no_matches` hint the palette view shows,
    /// instead of `render_sections`.
    #[test]
    fn sidebar_view_shows_no_matches_hint_when_filtered_to_zero() {
        let groups: Vec<SessionGroup> = Vec::new();
        let state = GridState::new();
        let (sessions_pane, agents_pane) = panes_from(&groups);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &groups,
                    &state,
                    ViewMode::Sidebar,
                    InputMode::Search,
                    false,
                    "zzz",
                    None,
                    None,
                    &test_preview(),
                    0,
                    &sessions_pane,
                    &agents_pane,
                    SectionFocus::Sessions,
                    Surface::Popup,
                )
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("no matching windows"));
        assert!(!rendered.contains("Sessions"));
        assert!(!rendered.contains("Agents"));
    }

    /// The unfocused section steps its title and row text down to a dimmer
    /// colour rather than stacking `Modifier::DIM` on an already-dark grey,
    /// which some terminals render almost invisibly. The status icon keeps its
    /// colour either way — whether an agent is blocked is the one thing the
    /// Agents section exists to show.
    #[test]
    fn unfocused_section_dims_its_text_but_keeps_status_colour() {
        let mut agent_card = crate::test_support::test_card("dotfiles", "0");
        agent_card.window_name = "config".to_owned();
        agent_card.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let sessions_group = vec![SessionGroup {
            session_name: "dotfiles".to_owned(),
            cards: vec![agent_card],
        }];
        let sessions = Pane::new(session_rows(&sessions_group, &HashSet::new(), None));
        let agents = Pane::new(agent_rows(&sessions_group));

        let body = Rect {
            x: 0,
            y: 0,
            width: 28,
            height: 12,
        };
        let (_, agents_area) = section_heights(body);
        let agents_area = agents_area.expect("agents section");

        let backend = ratatui::backend::TestBackend::new(28, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                // Sessions is focused, so the Agents section below it must
                // render fully dimmed: title and icon included.
                render_sections(frame, body, &sessions, &agents, SectionFocus::Sessions, 0);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let title_cell = buffer.get(agents_area.x, agents_area.y);
        let icon_cell = buffer.get(agents_area.x, agents_area.y.saturating_add(1));

        // Headings are not state: both titles stay white whether or not their
        // section has focus.
        assert_eq!(
            title_cell.fg,
            Color::White,
            "the Agents title should stay white while unfocused"
        );
        assert!(
            !title_cell.modifier.contains(Modifier::DIM),
            "a title should never be dimmed"
        );
        // Working -> yellow. The icon must survive the section losing focus.
        assert_eq!(
            icon_cell.fg,
            Color::Yellow,
            "unfocused Agents status icon lost its colour"
        );
        assert!(
            !icon_cell.modifier.contains(Modifier::DIM),
            "status icon should not be dimmed"
        );

        // The detail line beneath the name. It rendered `Color::Black` here
        // once, which on a dark background is not dim — it is invisible.
        let detail_cell = buffer.get(agents_area.x + 2, agents_area.y.saturating_add(2));
        assert_ne!(
            detail_cell.fg,
            Color::Black,
            "an unfocused detail line must stay readable, not vanish into the background"
        );
        assert_eq!(detail_cell.fg, Color::DarkGray);
    }
}

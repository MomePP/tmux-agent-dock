//! The interactive switcher: terminal setup, the event loop, and the key
//! handling for each input mode.

pub(crate) mod layout;
pub(crate) mod pane;
pub(crate) mod render;
pub(crate) mod sections;
pub(crate) mod state;

use std::{
    collections::HashSet,
    io,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseEventKind,
    },
    execute, queue,
    style::force_color_output,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};

use crate::{
    cards::{
        apply_session_order, group_cards_by_session, load_cards, load_session_order,
        persist_session_order,
    },
    model::{SessionGroup, SwitcherAction, WindowCard},
    preview::PreviewMirror,
    search::{apply_query, delete_query_word, filter_sessions},
    tmux::{current_window_id, env_tmux_value, rename_window, swap_windows, tmux_output, tmux_status},
};
use layout::{compact_navigation_height, switcher_layout, switcher_layout_for_input};
use pane::Pane;
use render::draw;
use sections::{
    agent_rows, format_expanded, parse_expanded, row_key, section_heights, session_rows, Row,
    RowKind, SectionFocus,
};
use state::{
    accept_numbered_session, compact_lines, format_input_mode, format_view_mode, handle_prompt_key,
    initial_grid_state, keep_compact_selection_visible, move_compact_selection,
    move_compact_session_edge, parse_input_mode, parse_view_mode, push_matching_movement_count,
    push_numbered_choice, refresh_sessions_from_cards, rename_card_in_place,
    select_compact_relative, select_key_action, swap_selected_session, swap_selected_window,
    sync_numbered_selection, take_counted_open_motion, Direction, GridState, InputMode,
    NumberedOpen, PromptKind, PromptState, ViewMode,
};

const CARD_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
/// How often the whole screen is forcibly repainted. Ratatui only rewrites
/// cells it believes changed, so anything that scribbles on the terminal
/// behind its back — tmux compositing glitches while a busy pane redraws
/// under the popup, wide glyphs in mirrored pane content nudging the cursor —
/// would otherwise stay smeared across the modal until that cell happens to
/// change. A periodic full redraw self-heals within half a second.
const FULL_REDRAW_INTERVAL: Duration = Duration::from_millis(500);
const TUI_TICK_INTERVAL: Duration = Duration::from_millis(50);
const VIEW_MODE_OPTION: &str = "@tmux_agent_switcher_view";
const INPUT_MODE_OPTION: &str = "@tmux_agent_switcher_input";
const EXPANDED_OPTION: &str = "@tmux_agent_switcher_expanded";

pub fn run_tui(cards: Vec<WindowCard>) -> Result<Option<SwitcherAction>> {
    if cards.is_empty() {
        return Ok(None);
    }
    force_color_output(true);

    let current_window_id = current_window_id();

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    // Capture the mouse so wheel scrolls drive the list instead of tmux
    // scrolling (and redrawing) whatever sits behind the popup.
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_tui_loop(&mut terminal, cards, current_window_id.as_deref());
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

/// An initial list move to apply as soon as the switcher opens, so a key binding
/// can drop the user straight into navigating (e.g. Ctrl+j opens moved one down).
/// Driven by the `TMUX_AGENT_SWITCHER_INITIAL_MOVE` env var set by the launcher.
fn initial_move_direction() -> Option<Direction> {
    match env_tmux_value("TMUX_AGENT_SWITCHER_INITIAL_MOVE").as_deref() {
        Some("down") => Some(Direction::Down),
        Some("up") => Some(Direction::Up),
        _ => None,
    }
}

/// The view style the switcher opens with. The launcher passes the configured
/// (`@agent_switcher_view`) or last-toggled style via the
/// `TMUX_AGENT_SWITCHER_VIEW` env var.
fn initial_view_mode() -> ViewMode {
    env_tmux_value("TMUX_AGENT_SWITCHER_VIEW")
        .as_deref()
        .and_then(parse_view_mode)
        .unwrap_or(ViewMode::Sidebar)
}

/// Remember a toggled view style for the tmux server's lifetime so the next
/// open reuses it (the launcher reads this option back). Best-effort: a failure
/// only loses the stickiness.
fn persist_view_mode(view: ViewMode) {
    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        VIEW_MODE_OPTION,
        format_view_mode(view),
    ]));
}

/// The input mode the switcher opens with: the launcher passes the configured
/// (`@agent_switcher_input`) or last-toggled mode via `TMUX_AGENT_SWITCHER_INPUT`.
fn initial_input_mode() -> InputMode {
    env_tmux_value("TMUX_AGENT_SWITCHER_INPUT")
        .as_deref()
        .and_then(parse_input_mode)
        .unwrap_or(InputMode::Keys)
}

/// Same stickiness as [`persist_view_mode`], for the Tab-toggled input mode.
fn persist_input_mode(input: InputMode) {
    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        INPUT_MODE_OPTION,
        format_input_mode(input),
    ]));
}

/// Which sessions were left expanded, remembered for the tmux server's
/// lifetime the same way the view and input modes are.
fn initial_expanded() -> HashSet<String> {
    let value = tmux_output(&["show-option", "-gqv", EXPANDED_OPTION]).unwrap_or_default();
    parse_expanded(&value)
}

fn persist_expanded(expanded: &HashSet<String>) {
    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        EXPANDED_OPTION,
        &format_expanded(expanded),
    ]));
}

/// Everything the switcher tracks while open: the full and filtered session
/// lists, the selection, and the view/input modes.
struct SwitcherUi {
    sessions: Vec<SessionGroup>,
    filtered: Vec<SessionGroup>,
    query: String,
    state: GridState,
    view: ViewMode,
    input: InputMode,
    movement_count: Option<usize>,
    numbered_input: String,
    show_help: bool,
    prompt: Option<PromptState>,
    sessions_pane: Pane<Row>,
    agents_pane: Pane<Row>,
    focus: SectionFocus,
    expanded: HashSet<String>,
    current_window_id: Option<String>,
}

impl SwitcherUi {
    fn new(cards: Vec<WindowCard>, current_window_id: Option<&str>, terminal_size: Rect) -> Self {
        let mut sessions = group_cards_by_session(cards);
        if let Ok(order) = load_session_order() {
            apply_session_order(&mut sessions, &order);
        }
        let filtered = filter_sessions(&sessions, "");
        let mut ui = Self {
            sessions,
            filtered,
            query: String::new(),
            state: GridState::new(),
            view: initial_view_mode(),
            input: initial_input_mode(),
            movement_count: None,
            numbered_input: String::new(),
            show_help: false,
            prompt: None,
            sessions_pane: Pane::new(Vec::new()),
            agents_pane: Pane::new(Vec::new()),
            focus: SectionFocus::Sessions,
            expanded: initial_expanded(),
            current_window_id: current_window_id.map(str::to_owned),
        };
        ui.rebuild_panes();
        ui.state = initial_grid_state(
            &ui.filtered,
            current_window_id,
            ui.navigation_height(terminal_size),
        );
        if let Some(direction) = initial_move_direction() {
            let navigation_height = ui.navigation_height(terminal_size);
            move_compact_selection(&mut ui.state, &ui.filtered, direction, navigation_height);
        }
        ui
    }

    /// True while a view backed by the two sections is active. `palette` keeps
    /// the flat `GridState` list, so every sections-specific key path checks
    /// this first.
    fn uses_sections(&self) -> bool {
        matches!(self.view, ViewMode::Sidebar | ViewMode::SidebarRight)
    }

    /// Rebuilds both row lists from the filtered cards, holding each cursor on
    /// the row it was on.
    fn rebuild_panes(&mut self) {
        let keep_session = self
            .sessions_pane
            .selected()
            .map(|row| row_key(row).to_owned());
        let keep_agent = self
            .agents_pane
            .selected()
            .map(|row| row_key(row).to_owned());

        let sessions = session_rows(
            &self.filtered,
            &self.expanded,
            self.current_window_id.as_deref(),
        );
        let agents = agent_rows(&self.filtered);

        self.sessions_pane
            .set_items(sessions, keep_session.as_deref(), |row| row_key(row));
        self.agents_pane
            .set_items(agents, keep_agent.as_deref(), |row| row_key(row));

        if self.agents_pane.is_empty() {
            self.focus = SectionFocus::Sessions;
        }
    }

    #[allow(dead_code)] // Consumed by a later task (open/select the focused row)
    fn focused_pane(&self) -> &Pane<Row> {
        match self.focus {
            SectionFocus::Sessions => &self.sessions_pane,
            SectionFocus::Agents => &self.agents_pane,
        }
    }

    fn focused_pane_mut(&mut self) -> &mut Pane<Row> {
        match self.focus {
            SectionFocus::Sessions => &mut self.sessions_pane,
            SectionFocus::Agents => &mut self.agents_pane,
        }
    }

    #[allow(dead_code)] // Consumed by Task 6 (key handling)
    fn selected_row(&self) -> Option<&Row> {
        self.focused_pane().selected()
    }

    /// Expands the selected session. On a window row this does nothing — the
    /// row is already the expansion.
    fn toggle_expanded(&mut self) {
        let Some(Row {
            kind: RowKind::Session { name, .. },
            ..
        }) = self.sessions_pane.selected()
        else {
            return;
        };
        let name = name.clone();
        self.expanded.insert(name);
        persist_expanded(&self.expanded);
        self.rebuild_panes();
    }

    /// Collapses the selected session, or the session owning the selected
    /// window row.
    fn collapse_selected(&mut self) {
        let Some(row) = self.sessions_pane.selected() else {
            return;
        };
        let name = match &row.kind {
            RowKind::Session { name, .. } => name.clone(),
            _ => row.target.session_name.clone(),
        };
        self.expanded.remove(&name);
        persist_expanded(&self.expanded);
        self.rebuild_panes();
    }

    /// The list viewport height used for scrolling, derived from the current
    /// layout (and therefore from the view/input modes and help visibility).
    fn navigation_height(&self, terminal_size: Rect) -> u16 {
        compact_navigation_height(
            terminal_size,
            self.show_help,
            self.view,
            compact_lines(&self.filtered).len(),
            self.input,
        )
    }

    fn refilter(&mut self, navigation_height: u16) {
        apply_query(
            &mut self.filtered,
            &mut self.state,
            &self.sessions,
            &self.query,
            navigation_height,
        );
    }

    fn refresh_cards(&mut self, cards: Vec<WindowCard>, navigation_height: u16) {
        refresh_sessions_from_cards(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            cards,
            &self.query,
            navigation_height,
        );
        self.rebuild_panes();
    }

    fn toggle_help(&mut self, terminal_size: Rect) {
        self.show_help = !self.show_help;
        let navigation_height = self.navigation_height(terminal_size);
        keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
    }

    fn open_new_window_prompt(&mut self) {
        if let Some(card) = self.state.selected_card(&self.filtered) {
            let session_name = card.session_name.clone();
            self.show_help = false;
            self.prompt = Some(PromptState::new(PromptKind::NewWindow { session_name }));
        }
    }

    fn open_new_session_prompt(&mut self) {
        self.show_help = false;
        self.prompt = Some(PromptState::new(PromptKind::NewSession));
    }

    fn open_rename_prompt(&mut self) {
        if let Some(card) = self.state.selected_card(&self.filtered) {
            let kind = PromptKind::RenameWindow {
                window_id: card.window_id.clone(),
            };
            let window_name = card.window_name.clone();
            self.show_help = false;
            self.prompt = Some(PromptState::with_input(kind, window_name));
        }
    }

    /// Renames the window in tmux and patches the cached card lists so the
    /// sidebar shows the new name immediately (the periodic card refresh would
    /// otherwise lag by one interval).
    fn apply_rename(&mut self, window_id: &str, window_name: &str) {
        if rename_window(window_id, window_name).is_err() {
            return;
        }
        rename_card_in_place(
            &mut self.sessions,
            &mut self.filtered,
            window_id,
            window_name,
        );
    }

    /// Moves the selected window one slot up or down within its session,
    /// reordering the cached lists immediately and mirroring the swap in
    /// tmux. Best-effort: if tmux rejects the swap (e.g. a window vanished),
    /// the periodic card refresh restores tmux's real order within a beat.
    fn move_selected_window(&mut self, direction: Direction, navigation_height: u16) {
        if let Some((source, target)) = swap_selected_window(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            &self.query,
            direction,
            navigation_height,
        ) {
            let _ = swap_windows(&source, &target);
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, navigation_height: u16) {
        match kind {
            MouseEventKind::ScrollDown => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            MouseEventKind::ScrollUp => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            _ => {}
        }
    }

    /// Feeds one key into the switcher. `Some(result)` closes it with that
    /// outcome (an action to run, or None for a plain quit); `None` keeps it
    /// open.
    fn handle_key(&mut self, key: KeyEvent, terminal_size: Rect) -> Option<Option<SwitcherAction>> {
        if let Some(active_prompt) = self.prompt.as_mut() {
            if let Some(result) = handle_prompt_key(active_prompt, key) {
                match result {
                    // Renames run in place so the switcher stays open on the
                    // updated list; other prompt actions close it and execute
                    // after the TUI has torn down.
                    Some(SwitcherAction::RenameWindow {
                        window_id,
                        window_name,
                    }) => {
                        self.prompt = None;
                        self.apply_rename(&window_id, &window_name);
                    }
                    Some(action) => return Some(Some(action)),
                    None => self.prompt = None,
                }
            }
            return None;
        }

        let navigation_height = self.navigation_height(terminal_size);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // A Vim-style count survives until j/k consumes it or another digit
        // leaves it with no matching relative target.
        let keys_count_key = self.input == InputMode::Keys
            && !ctrl
            && matches!(key.code, KeyCode::Char(ch) if ch.is_ascii_digit());
        let keys_count_motion = self.input == InputMode::Keys
            && !ctrl
            && !alt
            && matches!(key.code, KeyCode::Char('j' | 'k'));
        if !keys_count_key && !keys_count_motion {
            self.movement_count = None;
        }

        match key.code {
            KeyCode::Esc => {
                // Telescope-style: first Esc clears the filter, a second
                // one closes the switcher. Numbers similarly clears a
                // partial numbered address before closing.
                if self.input == InputMode::Numbers && !self.numbered_input.is_empty() {
                    self.numbered_input.clear();
                    return None;
                }
                if self.input != InputMode::Search || self.query.is_empty() {
                    return Some(None);
                }
                self.query.clear();
                self.refilter(navigation_height);
            }
            KeyCode::Enter => {
                if self.input == InputMode::Numbers {
                    if self.numbered_input.contains(',') {
                        if let Some(card) = sync_numbered_selection(
                            &self.numbered_input,
                            &self.filtered,
                            &mut self.state,
                            NumberedOpen::Force,
                        ) {
                            return Some(Some(SwitcherAction::Select(card)));
                        }
                    } else {
                        accept_numbered_session(&mut self.numbered_input, &self.filtered);
                        sync_numbered_selection(
                            &self.numbered_input,
                            &self.filtered,
                            &mut self.state,
                            NumberedOpen::Never,
                        );
                    }
                    return None;
                }
                if let Some(action) = select_key_action(key, &self.state, &self.filtered) {
                    return Some(Some(action));
                }
            }
            KeyCode::Tab if self.uses_sections() => {
                if !self.agents_pane.is_empty() {
                    self.focus = self.focus.toggled();
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                // Shift-Tab cycles the input mode, and plain Tab still does in
                // the palette, which has no sections to focus. This must stay
                // non-printable: in Search mode every printable key is text and
                // Esc closes the switcher rather than leaving the mode, so this
                // is the only way out.
                self.input = self.input.toggled();
                self.numbered_input.clear();
                persist_input_mode(self.input);
                let navigation_height = self.navigation_height(terminal_size);
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            KeyCode::Backspace => {
                if self.input == InputMode::Numbers {
                    self.numbered_input.clear();
                    return None;
                }
                if self.query.pop().is_some() {
                    self.refilter(navigation_height);
                }
            }
            KeyCode::Down | KeyCode::Up if alt => {
                self.numbered_input.clear();
                let direction = if key.code == KeyCode::Down {
                    Direction::Down
                } else {
                    Direction::Up
                };
                self.move_selected_window(direction, navigation_height);
            }
            KeyCode::Down => {
                self.numbered_input.clear();
                if self.uses_sections() {
                    self.focused_pane_mut().move_by(1);
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Down,
                        navigation_height,
                    );
                }
            }
            KeyCode::Up => {
                self.numbered_input.clear();
                if self.uses_sections() {
                    self.focused_pane_mut().move_by(-1);
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Up,
                        navigation_height,
                    );
                }
            }
            KeyCode::Left => {
                self.numbered_input.clear();
                if self.uses_sections() {
                    self.collapse_selected();
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Left,
                        navigation_height,
                    );
                }
            }
            KeyCode::Right => {
                self.numbered_input.clear();
                if self.uses_sections() {
                    self.toggle_expanded();
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Right,
                        navigation_height,
                    );
                }
            }
            KeyCode::Char(ch) if ctrl => {
                self.numbered_input.clear();
                return self.handle_ctrl_char(ch, navigation_height);
            }
            KeyCode::Char('j' | 'k') if alt => {
                self.numbered_input.clear();
                let direction = if key.code == KeyCode::Char('j') {
                    Direction::Down
                } else {
                    Direction::Up
                };
                self.move_selected_window(direction, navigation_height);
            }
            KeyCode::Char('J' | 'K') => {
                self.numbered_input.clear();
                let direction = if key.code == KeyCode::Char('J') {
                    Direction::Down
                } else {
                    Direction::Up
                };
                if swap_selected_session(
                    &mut self.sessions,
                    &mut self.filtered,
                    &mut self.state,
                    &self.query,
                    direction,
                    navigation_height,
                ) {
                    persist_session_order(&self.sessions);
                }
            }
            KeyCode::Char('r') if self.input != InputMode::Search => {
                self.numbered_input.clear();
                self.open_rename_prompt();
            }
            KeyCode::Char('j' | 'k') if self.input == InputMode::Numbers => {
                self.numbered_input.clear();
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    if key.code == KeyCode::Char('j') {
                        Direction::Down
                    } else {
                        Direction::Up
                    },
                    navigation_height,
                );
            }
            KeyCode::Char(ch) if self.input == InputMode::Numbers && ch.is_ascii_digit() => {
                if let Some(card) = push_numbered_choice(
                    &mut self.numbered_input,
                    ch,
                    &self.filtered,
                    &mut self.state,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            KeyCode::Char(',') if self.input == InputMode::Numbers => {
                if accept_numbered_session(&mut self.numbered_input, &self.filtered) {
                    sync_numbered_selection(
                        &self.numbered_input,
                        &self.filtered,
                        &mut self.state,
                        NumberedOpen::Never,
                    );
                    keep_compact_selection_visible(
                        &mut self.state,
                        &self.filtered,
                        navigation_height,
                    );
                }
            }
            KeyCode::Char('?') if self.input == InputMode::Numbers => {
                self.toggle_help(terminal_size);
            }
            KeyCode::Char(_) if self.input == InputMode::Numbers => {}
            KeyCode::Char(ch) if self.input == InputMode::Keys => {
                return self.handle_keys_mode_char(ch, navigation_height, terminal_size);
            }
            KeyCode::Char('?') if self.query.is_empty() => {
                self.toggle_help(terminal_size);
            }
            KeyCode::Char(ch) => {
                self.query.push(ch);
                self.refilter(navigation_height);
            }
            _ => {}
        }

        None
    }

    /// Ctrl-modified keys, active in every input mode.
    fn handle_ctrl_char(
        &mut self,
        ch: char,
        navigation_height: u16,
    ) -> Option<Option<SwitcherAction>> {
        match ch {
            'c' => return Some(None),
            'j' => {
                if let Some(card) = select_compact_relative(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    1,
                    navigation_height,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
            }
            'k' => {
                if let Some(card) = select_compact_relative(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    1,
                    navigation_height,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
            }
            'n' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            'p' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            'h' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Left,
                    navigation_height,
                );
            }
            'l' => {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Right,
                    navigation_height,
                );
            }
            'u' => {
                self.query.clear();
                self.refilter(navigation_height);
            }
            'w' => {
                delete_query_word(&mut self.query);
                self.refilter(navigation_height);
            }
            't' => self.open_new_window_prompt(),
            's' => self.open_new_session_prompt(),
            _ => {}
        }
        None
    }

    /// Unmodified characters in Keys (Vim) mode: motions, counts, and the
    /// prompt/select/help shortcuts.
    fn handle_keys_mode_char(
        &mut self,
        ch: char,
        navigation_height: u16,
        terminal_size: Rect,
    ) -> Option<Option<SwitcherAction>> {
        match ch {
            'q' => return Some(None),
            ' ' => {
                if let Some(card) = self.state.selected_card(&self.filtered) {
                    return Some(Some(SwitcherAction::Select(card.clone())));
                }
            }
            '?' => self.toggle_help(terminal_size),
            'n' => self.open_new_window_prompt(),
            'N' => self.open_new_session_prompt(),
            'v' => {
                self.numbered_input.clear();
                self.view = self.view.toggled();
                persist_view_mode(self.view);
                let navigation_height = self.navigation_height(terminal_size);
                keep_compact_selection_visible(&mut self.state, &self.filtered, navigation_height);
            }
            'h' => {
                if self.uses_sections() {
                    self.collapse_selected();
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Left,
                        navigation_height,
                    );
                }
            }
            'H' => {
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            'j' | 'k' => {
                if self.uses_sections() {
                    let delta = if ch == 'j' { 1 } else { -1 };
                    self.focused_pane_mut().move_by(delta);
                    return None;
                }
                if let Some((direction, count)) =
                    take_counted_open_motion(&mut self.movement_count, ch)
                {
                    if let Some(card) = select_compact_relative(
                        &mut self.state,
                        &self.filtered,
                        direction,
                        count,
                        navigation_height,
                    ) {
                        return Some(Some(SwitcherAction::Select(card)));
                    }
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        if ch == 'j' {
                            Direction::Down
                        } else {
                            Direction::Up
                        },
                        navigation_height,
                    );
                }
            }
            'l' => {
                if self.uses_sections() {
                    self.toggle_expanded();
                } else {
                    move_compact_selection(
                        &mut self.state,
                        &self.filtered,
                        Direction::Right,
                        navigation_height,
                    );
                }
            }
            'L' => {
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            _ if ch.is_ascii_digit() => {
                push_matching_movement_count(
                    &mut self.movement_count,
                    ch,
                    &self.state,
                    &self.filtered,
                );
            }
            _ => {}
        }
        None
    }
}

/// Arranges for the next `draw` to repaint every cell, without showing a blank
/// frame first.
///
/// `Terminal::clear` emits its escape through `execute!`, which flushes on the
/// spot — the terminal blanks immediately and then stays empty until the next
/// `draw` has rendered and flushed. At [`FULL_REDRAW_INTERVAL`] that reads as
/// the popup blinking twice a second. Queueing the escape instead leaves it in
/// the buffer, so the terminal receives the clear and the repaint as one write
/// and never presents the gap between them. `swap_buffers` then empties the
/// diff baseline the way `clear` does, so `draw` really does rewrite every
/// cell rather than trusting what it believes is still on screen.
fn queue_full_repaint(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    queue!(terminal.backend_mut(), Clear(ClearType::All))?;
    terminal.swap_buffers();
    Ok(())
}

/// Scrolls the Sessions and Agents panes so their cursors stay inside the
/// window `render_sections` is about to draw.
///
/// `Pane::move_by` only moves the cursor; nothing calls `Pane::keep_visible`
/// to follow it with the scroll offset, and `Pane::visible_range` is a pure
/// function of that offset — so without this, driving a cursor past the
/// bottom of a section walks it out of the visible rows with no way for
/// rendering alone to bring it back. Must run before every `draw` while a
/// sections view is active.
fn keep_sections_visible(ui: &mut SwitcherUi, terminal_size: Rect) {
    if !ui.uses_sections() {
        return;
    }

    let body = switcher_layout_for_input(
        terminal_size,
        ui.show_help,
        ui.view,
        compact_lines(&ui.filtered).len(),
        ui.input,
    )
    .sessions;
    let (sessions_area, agents_area) = section_heights(body, ui.agents_pane.len());

    // Each section spends one row on its title before the rows start.
    ui.sessions_pane
        .keep_visible(sessions_area.height.saturating_sub(1) as usize);
    if let Some(agents_area) = agents_area {
        ui.agents_pane
            .keep_visible(agents_area.height.saturating_sub(1) as usize);
    }
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cards: Vec<WindowCard>,
    current_window_id: Option<&str>,
) -> Result<Option<SwitcherAction>> {
    let mut ui = SwitcherUi::new(cards, current_window_id, terminal.size()?);
    let spinner_started_at = Instant::now();
    let mut last_card_refresh = Instant::now();
    let mut last_full_redraw = Instant::now();
    let mut preview = PreviewMirror::default();

    loop {
        let now = Instant::now();
        if now.duration_since(last_card_refresh) >= CARD_REFRESH_INTERVAL {
            if let Ok(cards) = load_cards() {
                let navigation_height = ui.navigation_height(terminal.size()?);
                ui.refresh_cards(cards, navigation_height);
            }
            last_card_refresh = now;
        }
        let preview_area = switcher_layout(
            terminal.size()?,
            ui.show_help,
            ui.view,
            compact_lines(&ui.filtered).len(),
        )
        .preview;
        preview.refresh_for(ui.state.selected_card(&ui.filtered), preview_area, now);
        if now.duration_since(last_full_redraw) >= FULL_REDRAW_INTERVAL {
            queue_full_repaint(terminal)?;
            last_full_redraw = now;
        }
        let spinner_frame = spinner_started_at.elapsed().as_millis() as usize / 120;
        keep_sections_visible(&mut ui, terminal.size()?);
        let input_value = if ui.input == InputMode::Numbers {
            ui.numbered_input.as_str()
        } else {
            ui.query.as_str()
        };
        terminal.draw(|frame| {
            draw(
                frame,
                &ui.filtered,
                &ui.state,
                ui.view,
                ui.input,
                ui.show_help,
                input_value,
                ui.movement_count,
                ui.prompt.as_ref(),
                &preview,
                spinner_frame,
                &ui.sessions_pane,
                &ui.agents_pane,
                ui.focus,
            )
        })?;

        if !event::poll(TUI_TICK_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Mouse(mouse) if ui.prompt.is_none() => {
                let navigation_height = ui.navigation_height(terminal.size()?);
                ui.handle_mouse(mouse.kind, navigation_height);
            }
            Event::Resize(_, _) => terminal.clear()?,
            Event::Key(key) => {
                if let Some(result) = ui.handle_key(key, terminal.size()?) {
                    return Ok(result);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentState, AgentStatus};
    use crate::test_support::test_card;

    // `SwitcherUi::new` reads the real `EXPANDED_OPTION` tmux global (via
    // `initial_expanded`) and shells out to load the session order, so tests
    // must not depend on the developer's live tmux state. Reset the
    // expansion set and rebuild before returning, so every assertion starts
    // from a known collapsed state regardless of what tmux happens to have.
    fn ui_with(cards: Vec<WindowCard>) -> SwitcherUi {
        let mut ui = SwitcherUi::new(
            cards,
            None,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 40,
            },
        );
        ui.expanded.clear();
        ui.rebuild_panes();
        ui
    }

    #[test]
    fn panes_are_populated_from_the_loaded_cards() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let plain = test_card("gogo", "0");

        let ui = ui_with(vec![agent, plain]);

        // One row per session while everything is collapsed.
        assert_eq!(ui.sessions_pane.len(), 2);
        // Only the card running an agent reaches the Agents pane.
        assert_eq!(ui.agents_pane.len(), 1);
        assert_eq!(ui.focus, SectionFocus::Sessions);
    }

    #[test]
    fn expanding_a_session_adds_its_windows_to_the_sessions_pane() {
        let ui_cards = vec![test_card("dotfiles", "0"), test_card("dotfiles", "1")];
        let mut ui = ui_with(ui_cards);
        assert_eq!(ui.sessions_pane.len(), 1);

        ui.expanded.insert("dotfiles".to_owned());
        ui.rebuild_panes();

        assert_eq!(ui.sessions_pane.len(), 3);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn size() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 40,
        }
    }

    #[test]
    fn tab_moves_focus_between_the_sections() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let mut ui = ui_with(vec![agent]);

        ui.handle_key(key(KeyCode::Tab), size());
        assert_eq!(ui.focus, SectionFocus::Agents);

        ui.handle_key(key(KeyCode::Tab), size());
        assert_eq!(ui.focus, SectionFocus::Sessions);
    }

    #[test]
    fn tab_does_nothing_when_no_agent_is_running() {
        let mut ui = ui_with(vec![test_card("dotfiles", "0")]);

        ui.handle_key(key(KeyCode::Tab), size());

        assert_eq!(ui.focus, SectionFocus::Sessions);
    }

    #[test]
    fn l_expands_and_h_collapses_the_selected_session() {
        let mut ui = ui_with(vec![test_card("dotfiles", "0"), test_card("dotfiles", "1")]);
        assert_eq!(ui.sessions_pane.len(), 1);

        ui.handle_key(key(KeyCode::Char('l')), size());
        assert_eq!(ui.sessions_pane.len(), 3);
        assert!(ui.expanded.contains("dotfiles"));

        ui.handle_key(key(KeyCode::Char('h')), size());
        assert_eq!(ui.sessions_pane.len(), 1);
        assert!(!ui.expanded.contains("dotfiles"));
    }

    #[test]
    fn h_on_a_child_row_collapses_its_parent_session() {
        let mut ui = ui_with(vec![test_card("dotfiles", "0"), test_card("dotfiles", "1")]);
        ui.handle_key(key(KeyCode::Char('l')), size());
        // Move onto the first window row.
        ui.handle_key(key(KeyCode::Char('j')), size());

        ui.handle_key(key(KeyCode::Char('h')), size());

        assert!(!ui.expanded.contains("dotfiles"));
        assert_eq!(ui.sessions_pane.len(), 1);
    }

    #[test]
    fn j_and_k_stay_inside_the_focused_section() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let mut ui = ui_with(vec![agent, test_card("gogo", "0")]);

        // Two session rows; j moves to the second and stops there.
        ui.handle_key(key(KeyCode::Char('j')), size());
        assert_eq!(ui.sessions_pane.cursor, 1);
        ui.handle_key(key(KeyCode::Char('j')), size());
        assert_eq!(ui.sessions_pane.cursor, 1);
        assert_eq!(ui.focus, SectionFocus::Sessions);
    }

    /// `Pane::move_by` only moves the cursor — nothing calls `Pane::keep_visible`
    /// to scroll the offset along with it — so a cursor driven past the bottom
    /// of a short viewport would otherwise walk off the visible rows with no
    /// way for rendering (which only sees `&Pane`) to recover it. This drives
    /// the cursor to the last of many session rows in a viewport far too short
    /// to show them all, then asserts `keep_sections_visible` scrolled the pane
    /// so the cursor is still inside `visible_range`.
    #[test]
    fn scrolling_the_cursor_past_the_viewport_keeps_it_visible() {
        let cards: Vec<WindowCard> = (0..20)
            .map(|index| test_card(&format!("session-{index}"), "0"))
            .collect();
        let mut ui = ui_with(cards);
        let small = Rect {
            x: 0,
            y: 0,
            width: 28,
            height: 12,
        };
        assert_eq!(ui.sessions_pane.len(), 20);

        for _ in 0..19 {
            ui.handle_key(key(KeyCode::Char('j')), small);
        }
        assert_eq!(ui.sessions_pane.cursor, 19);

        keep_sections_visible(&mut ui, small);

        let body = switcher_layout_for_input(
            small,
            ui.show_help,
            ui.view,
            compact_lines(&ui.filtered).len(),
            ui.input,
        )
        .sessions;
        let (sessions_area, _) = section_heights(body, ui.agents_pane.len());
        let row_height = sessions_area.height.saturating_sub(1) as usize;

        assert!(ui
            .sessions_pane
            .visible_range(row_height)
            .contains(&ui.sessions_pane.cursor));
    }
}

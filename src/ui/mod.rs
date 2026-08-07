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
        MouseButton, MouseEvent, MouseEventKind,
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
    tmux::{
        current_window_id, env_tmux_value, execute_action, rename_window, swap_windows,
        tmux_output, tmux_status,
    },
};
use layout::{compact_navigation_height, dock_layout, switcher_layout, switcher_layout_for_input};
use pane::Pane;
use render::{draw, Surface};
use sections::{
    agent_rows, format_expanded, initial_expanded_set, known_session_names, parse_expand_default, parse_expanded, row_at, row_key,
    rows_area, rows_per_height, section_heights, session_rows, sessions_matching_windows, ClickTarget, ExpandDefault, Row, RowKind,
    SectionFocus,
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
/// Every session the switcher had an opinion about when it last closed.
const KNOWN_OPTION: &str = "@tmux_agent_switcher_known";
const EXPAND_DEFAULT_OPTION: &str = "@agent_switcher_expand_default";

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
    let result = run_tui_loop(&mut terminal, cards, current_window_id.as_deref(), Surface::Popup);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

/// Runs the switcher as a persistent pane. Unlike [`run_tui`] this never
/// returns an action for the caller to execute — a dock that exited on
/// selection would just be the popup.
pub fn run_dock(cards: Vec<WindowCard>) -> Result<()> {
    force_color_output(true);
    let current_window_id = current_window_id();

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_tui_loop(
        &mut terminal,
        cards,
        current_window_id.as_deref(),
        Surface::Dock,
    );
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result.map(|_| ())
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

/// Writes global tmux options, chaining several into one round trip so a
/// reader can never catch them half-applied.
///
/// A no-op under `cfg(test)`. The unit tests drive the very keys that persist —
/// `v`, `S-tab`, `h`/`l` — against a `SwitcherUi` built from fixtures, so left
/// ungated a plain `cargo test` rewrites the developer's own live tmux globals
/// with fixture data. It really does: a test run set
/// `@tmux_agent_switcher_known` to `dotfiles`, a session that exists only in
/// `sections::tests::fixture`.
fn set_global_options(pairs: &[(&str, &str)]) {
    if cfg!(test) {
        return;
    }

    let mut args: Vec<&str> = Vec::new();
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            args.push(";");
        }
        args.extend(["set-option", "-g", name, value]);
    }
    let _ = tmux_status(Command::new("tmux").args(args));
}

/// Remember a toggled view style for the tmux server's lifetime so the next
/// open reuses it (the launcher reads this option back). Best-effort: a failure
/// only loses the stickiness.
fn persist_view_mode(view: ViewMode) {
    set_global_options(&[(VIEW_MODE_OPTION, format_view_mode(view))]);
}

/// The input mode the switcher opens with: the launcher passes the configured
/// (`@agent_switcher_input`) or last-toggled mode via `TMUX_AGENT_SWITCHER_INPUT`.
fn initial_input_mode() -> InputMode {
    env_tmux_value("TMUX_AGENT_SWITCHER_INPUT")
        .as_deref()
        .and_then(parse_input_mode)
        .unwrap_or(InputMode::Keys)
}

/// What a fresh tmux server opens with: `all`, `attached` (the default) or
/// `none`. It only applies until something is remembered — once the user has
/// expanded or collapsed anything, that is the answer.
fn initial_expand_default() -> ExpandDefault {
    parse_expand_default(&tmux_output(&["show-option", "-gqv", EXPAND_DEFAULT_OPTION]).unwrap_or_default())
}

/// Same stickiness as [`persist_view_mode`], for the Tab-toggled input mode.
fn persist_input_mode(input: InputMode) {
    set_global_options(&[(INPUT_MODE_OPTION, format_input_mode(input))]);
}

/// Which sessions were left expanded, and which the switcher had an opinion
/// about at all — remembered for the tmux server's lifetime the same way the
/// view and input modes are. See [`initial_expanded_set`] for why both are
/// needed; an empty pair simply means nothing is remembered yet, so every
/// session follows `@agent_switcher_expand_default`.
fn initial_expanded() -> (HashSet<String>, HashSet<String>) {
    let read = |option| parse_expanded(&tmux_output(&["show-option", "-gqv", option]).unwrap_or_default());
    (read(EXPANDED_OPTION), read(KNOWN_OPTION))
}

/// Hands the keyboard back to the pane the user was working in. `select-pane -l`
/// is the last-active pane, which is where they came from; the `:.+` fallback
/// covers a dock that has no last pane yet. Best-effort — failing to move focus
/// is not worth an error.
fn focus_work_pane() {
    if tmux_status(Command::new("tmux").args(["select-pane", "-l"])).is_err() {
        let _ = tmux_status(Command::new("tmux").args(["select-pane", "-t", ":.+"]));
    }
}

/// Writes both halves of the memory in one tmux round trip. They must move
/// together: a `known` that lagged behind `expanded` would read a session the
/// user just collapsed as one that had never been seen, and re-expand it.
fn persist_expanded(expanded: &HashSet<String>, sessions: &[SessionGroup]) {
    let expanded = format_expanded(expanded.iter().cloned());
    let known = format_expanded(known_session_names(sessions));
    set_global_options(&[(EXPANDED_OPTION, &expanded), (KNOWN_OPTION, &known)]);
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
    surface: Surface,
    sessions_pane: Pane<Row>,
    agents_pane: Pane<Row>,
    focus: SectionFocus,
    expanded: HashSet<String>,
    /// Sessions auto-expanded because the active query narrowed their window
    /// list. Transient — never persisted, and unioned with `expanded` only
    /// when building rows, so a search never rewrites the remembered
    /// collapse state.
    search_expanded: HashSet<String>,
    current_window_id: Option<String>,
}

impl SwitcherUi {
    fn new(cards: Vec<WindowCard>, current_window_id: Option<&str>, terminal_size: Rect) -> Self {
        Self::with_settings(
            cards,
            current_window_id,
            terminal_size,
            initial_expanded(),
            initial_expand_default(),
            initial_view_mode(),
            initial_input_mode(),
        )
    }

    /// [`SwitcherUi::new`] with every tmux-derived setting injected, so tests
    /// can build a switcher whose view, input mode and expansion set do not
    /// depend on whatever the developer's live tmux server happens to hold.
    fn with_settings(
        cards: Vec<WindowCard>,
        current_window_id: Option<&str>,
        terminal_size: Rect,
        remembered: (HashSet<String>, HashSet<String>),
        expand_default: ExpandDefault,
        view: ViewMode,
        input: InputMode,
    ) -> Self {
        let mut sessions = group_cards_by_session(cards);
        if let Ok(order) = load_session_order() {
            apply_session_order(&mut sessions, &order);
        }
        let filtered = filter_sessions(&sessions, "");
        let (remembered_expanded, remembered_known) = remembered;
        let expanded = initial_expanded_set(
            &remembered_expanded,
            &remembered_known,
            &sessions,
            current_window_id,
            expand_default,
        );
        // Numbers is unusable in the sections views and its own cycle no longer
        // reaches it — but the mode is persisted across opens, so anyone who
        // landed there before is still carrying it. Coerce on the way in rather
        // than opening a sidebar that ignores every letter.
        let input = if matches!(view, ViewMode::Sidebar | ViewMode::SidebarRight)
            && input == InputMode::Numbers
        {
            InputMode::Keys
        } else {
            input
        };
        let mut ui = Self {
            sessions,
            filtered,
            query: String::new(),
            state: GridState::new(),
            view,
            input,
            movement_count: None,
            numbered_input: String::new(),
            show_help: false,
            prompt: None,
            surface: Surface::Popup,
            sessions_pane: Pane::new(Vec::new()),
            agents_pane: Pane::new(Vec::new()),
            focus: SectionFocus::Sessions,
            expanded,
            search_expanded: HashSet::new(),
            current_window_id: current_window_id.map(str::to_owned),
        };
        ui.rebuild_panes();
        ui.focus_current_window();
        ui.state = initial_grid_state(
            &ui.filtered,
            current_window_id,
            ui.navigation_height(terminal_size),
        );
        if let Some(direction) = initial_move_direction() {
            ui.apply_initial_move(direction, terminal_size);
        }
        ui
    }

    /// Opens with the Sessions cursor on the row for the window the client is
    /// in, rather than on row 0. `GridState` has always done this for the
    /// palette; without it the sidebar's cursor no longer starts where you are.
    fn focus_current_window(&mut self) {
        let Some(window_id) = self.current_window_id.clone() else {
            return;
        };
        // A session row precedes its children and shares their target when the
        // session's active window is the current one, so the *last* match is
        // the window's own row whenever its session is expanded, and the
        // session row when it is not.
        if let Some(index) = self
            .sessions_pane
            .items()
            .iter()
            .rposition(|row| row.target.window_id == window_id)
        {
            self.sessions_pane.cursor = index;
        }
    }

    /// The list move a launcher binding asked for (`Ctrl+j` opening the
    /// switcher already moved one down), applied to whichever cursor the view
    /// actually renders.
    fn apply_initial_move(&mut self, direction: Direction, terminal_size: Rect) {
        if self.uses_sections() {
            self.move_focused_pane(direction);
            return;
        }
        let navigation_height = self.navigation_height(terminal_size);
        move_compact_selection(&mut self.state, &self.filtered, direction, navigation_height);
    }

    /// The rect the sections are drawn into, from whichever layout this surface
    /// uses. Click resolution and scroll clamping both read it, so neither can
    /// drift from what was rendered.
    fn body_rect(&self, terminal_size: Rect) -> Rect {
        match self.surface {
            Surface::Dock => dock_layout(terminal_size, self.show_help, self.input),
            Surface::Popup => switcher_layout_for_input(
                terminal_size,
                self.show_help,
                self.view,
                compact_lines(&self.filtered).len(),
                self.input,
            ),
        }
        .sessions
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
        // Recomputed here rather than at the one call site that changes the
        // query, so the 300ms card refresh sees expansion for the query that
        // is actually live. Held apart from `expanded`, which is persisted to
        // a tmux global: a query must never rewrite the remembered state.
        self.search_expanded = if self.query.trim().is_empty() {
            HashSet::new()
        } else {
            sessions_matching_windows(&self.filtered, &self.sessions)
        };

        let keep_session = self
            .sessions_pane
            .selected()
            .map(|row| row_key(row).to_owned());
        let keep_agent = self
            .agents_pane
            .selected()
            .map(|row| row_key(row).to_owned());

        let effective: HashSet<String> = self
            .expanded
            .union(&self.search_expanded)
            .cloned()
            .collect();
        let sessions = session_rows(
            &self.filtered,
            &effective,
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

    fn selected_row(&self) -> Option<&Row> {
        self.focused_pane().selected()
    }

    /// The window every key acts on: the focused section's row target while a
    /// sections view is up, the grid cursor in the palette. Resolving it in one
    /// place is what keeps the two selection models from drifting apart — the
    /// sidebar's cursor is the only one it renders, so anything reading
    /// `GridState` there acts on a window the user cannot see.
    fn selected_target(&self) -> Option<&WindowCard> {
        if self.uses_sections() {
            self.selected_row().map(|row| &row.target)
        } else {
            self.state.selected_card(&self.filtered)
        }
    }

    /// Closes the switcher on whatever [`Self::selected_target`] resolves to;
    /// `None` (stay open) when nothing is selected.
    ///
    /// An agent row whose card hosts folded agents resolves to that agent by
    /// name. They share a host pane, so focusing it alone lands on whichever
    /// slot Neovim last had open rather than the one that was clicked.
    fn select_target(&self) -> Option<Option<SwitcherAction>> {
        if self.uses_sections() {
            if let Some(row) = self.selected_row() {
                if let RowKind::Agent { tool, .. } = &row.kind {
                    if row
                        .target
                        .folded_agents
                        .iter()
                        .any(|agent| &agent.label == tool)
                    {
                        return Some(Some(SwitcherAction::SelectAgent {
                            card: row.target.clone(),
                            clone: tool.clone(),
                        }));
                    }
                }
            }
        }

        self.selected_target()
            .map(|card| Some(SwitcherAction::Select(card.clone())))
    }

    fn move_focused_pane(&mut self, direction: Direction) {
        let delta = match direction {
            Direction::Down => 1,
            Direction::Up => -1,
            // The sections have no lateral movement; `h`/`l` collapse and
            // expand instead (spec §5).
            Direction::Left | Direction::Right => return,
        };
        self.focused_pane_mut().move_by(delta);
    }

    /// Points `GridState` at the row the sections cursor is on, so the
    /// selection-driven mutations in `state.rs` — window and session
    /// reordering — act on the row the user can actually see. A no-op in the
    /// palette, where `GridState` *is* the visible cursor.
    fn sync_grid_to_sections(&mut self) {
        if !self.uses_sections() {
            return;
        }
        let Some(window_id) = self.selected_row().map(|row| row.target.window_id.clone()) else {
            return;
        };
        let synced = GridState::for_window_id(&self.filtered, &window_id);
        // `for_window_id` falls back to (0, 0) for an id it cannot find; only
        // adopt it when it really landed on the row's target, so a stale row
        // can never quietly redirect a reorder onto the first window.
        if synced
            .selected_card(&self.filtered)
            .map(|card| card.window_id.as_str())
            == Some(window_id.as_str())
        {
            self.state = synced;
        }
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
        persist_expanded(&self.expanded, &self.sessions);
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
        persist_expanded(&self.expanded, &self.sessions);
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

    /// Re-applies the query to the palette's grid selection and always rebuilds
    /// the section rows behind it — including the search auto-expansion, which
    /// `rebuild_panes` owns.
    fn refilter(&mut self, navigation_height: u16) {
        apply_query(
            &mut self.filtered,
            &mut self.state,
            &self.sessions,
            &self.query,
            navigation_height,
        );
        self.rebuild_panes();
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
        if let Some(card) = self.selected_target() {
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
        if let Some(card) = self.selected_target() {
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
        self.sync_grid_to_sections();
        if let Some((source, target)) = swap_selected_window(
            &mut self.sessions,
            &mut self.filtered,
            &mut self.state,
            &self.query,
            direction,
            navigation_height,
        ) {
            let _ = swap_windows(&source, &target);
            // The rows are built from the cached order, so they have to be
            // rebuilt for the move to show before the next card refresh.
            self.rebuild_panes();
        }
    }

    /// Feeds one mouse event into the switcher. Same contract as
    /// [`SwitcherUi::handle_key`]: `Some(..)` closes with that outcome, `None`
    /// keeps it open.
    ///
    /// tmux's default `MouseDown1Pane` binding is `select-pane -t = ; send -M`,
    /// so a single click both focuses the pane and arrives here — clicking a
    /// row is one action, not two.
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        navigation_height: u16,
        terminal_size: Rect,
    ) -> Option<Option<SwitcherAction>> {
        self.numbered_input.clear();

        if let Some(direction) = match mouse.kind {
            MouseEventKind::ScrollDown => Some(Direction::Down),
            MouseEventKind::ScrollUp => Some(Direction::Up),
            _ => None,
        } {
            if self.uses_sections() {
                self.move_focused_pane(direction);
            } else {
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    direction,
                    navigation_height,
                );
            }
            return None;
        }

        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) || !self.uses_sections() {
            return None;
        }

        // Resolve against the geometry this surface actually drew. The dock has
        // no modal inset, so using the popup's rect here would land every click
        // two rows off — silently selecting the wrong window.
        let body = self.body_rect(terminal_size);

        match row_at(
            body,
            mouse.row,
            self.sessions_pane.items().len(),
            self.sessions_pane.offset,
            self.agents_pane.items().len(),
            self.agents_pane.offset,
        ) {
            ClickTarget::Row { section, index } => {
                self.focus = section;
                self.focused_pane_mut().cursor = index;
                self.select_target()
            }
            ClickTarget::Section(section) => {
                self.focus = section;
                None
            }
            ClickTarget::None => None,
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
        // leaves it with no matching relative target. The sections draw no row
        // numbers, so nothing there can be counted to and the count never
        // starts (see the digit arm in `handle_keys_mode_char`).
        let keys_count_key = self.input == InputMode::Keys
            && !ctrl
            && !self.uses_sections()
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
                    // The dock is a pane, not a modal: closing it would kill
                    // the pane and collapse the window layout, and `prefix + b`
                    // owns that. Esc keeps its other half — clearing the query
                    // — and otherwise hands the keyboard back to the work pane.
                    if self.surface == Surface::Dock {
                        focus_work_pane();
                        return None;
                    }
                    return Some(None);
                }
                self.query.clear();
                self.refilter(navigation_height);
            }
            KeyCode::Enter => {
                if self.uses_sections() {
                    return self.select_target();
                }
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
                self.input = if self.uses_sections() {
                    self.input.toggled_without_numbers()
                } else {
                    self.input.toggled()
                };
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
                self.sync_grid_to_sections();
                if swap_selected_session(
                    &mut self.sessions,
                    &mut self.filtered,
                    &mut self.state,
                    &self.query,
                    direction,
                    navigation_height,
                ) {
                    persist_session_order(&self.sessions);
                    // The section rows carry the old order until they are
                    // rebuilt; `row_key` then walks the cursor along with the
                    // session it moved.
                    self.rebuild_panes();
                }
            }
            KeyCode::Char('r') if self.input != InputMode::Search => {
                self.numbered_input.clear();
                self.open_rename_prompt();
            }
            KeyCode::Char('j' | 'k') if self.input == InputMode::Numbers => {
                self.numbered_input.clear();
                let direction = if key.code == KeyCode::Char('j') {
                    Direction::Down
                } else {
                    Direction::Up
                };
                if self.uses_sections() {
                    self.move_focused_pane(direction);
                    return None;
                }
                move_compact_selection(&mut self.state, &self.filtered, direction, navigation_height);
            }
            // Numbered addressing is a property of the compact list, which
            // renders the row numbers it reads. `render_sections` draws none,
            // so in the sidebar a digit would address rows nobody can see —
            // spec §5's "numbers: address rows in the focused section" is
            // unbuilt, and until it is, digits are swallowed rather than
            // steering an invisible cursor onto an unrendered window.
            KeyCode::Char(ch)
                if self.input == InputMode::Numbers
                    && ch.is_ascii_digit()
                    && !self.uses_sections() =>
            {
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
            KeyCode::Char(',') if self.input == InputMode::Numbers && !self.uses_sections() => {
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
            'j' | 'k' => {
                let direction = if ch == 'j' {
                    Direction::Down
                } else {
                    Direction::Up
                };
                if self.uses_sections() {
                    self.move_focused_pane(direction);
                    return self.select_target();
                }
                if let Some(card) = select_compact_relative(
                    &mut self.state,
                    &self.filtered,
                    direction,
                    1,
                    navigation_height,
                ) {
                    return Some(Some(SwitcherAction::Select(card)));
                }
            }
            'n' | 'p' => {
                let direction = if ch == 'n' {
                    Direction::Down
                } else {
                    Direction::Up
                };
                if self.uses_sections() {
                    self.move_focused_pane(direction);
                    return None;
                }
                move_compact_selection(&mut self.state, &self.filtered, direction, navigation_height);
            }
            'h' => {
                if self.uses_sections() {
                    self.collapse_selected();
                    return None;
                }
                move_compact_selection(
                    &mut self.state,
                    &self.filtered,
                    Direction::Left,
                    navigation_height,
                );
            }
            'l' => {
                if self.uses_sections() {
                    self.toggle_expanded();
                    return None;
                }
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
            'q' => {
                if self.surface == Surface::Dock {
                    focus_work_pane();
                    return None;
                }
                return Some(None);
            }
            ' ' => return self.select_target(),
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
            // Spec §5 leaves the session-edge jumps unbound in the Sessions
            // section: once a session is itself a row, `j`/`k` already walks
            // between sessions and an "edge" has nothing left to mean. An
            // explicit no-op, not a silent move of a cursor nothing draws.
            'H' => {
                if self.uses_sections() {
                    return None;
                }
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Up,
                    navigation_height,
                );
            }
            'j' | 'k' => {
                if self.uses_sections() {
                    self.move_focused_pane(if ch == 'j' {
                        Direction::Down
                    } else {
                        Direction::Up
                    });
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
                if self.uses_sections() {
                    return None;
                }
                move_compact_session_edge(
                    &mut self.state,
                    &self.filtered,
                    Direction::Down,
                    navigation_height,
                );
            }
            // The count addresses the compact list's relative row numbers, and
            // the sections render none — so `j`/`k` there could never consume
            // one. Swallow the digit rather than accumulating a count that
            // silently disappears at the next motion.
            _ if ch.is_ascii_digit() => {
                if self.uses_sections() {
                    return None;
                }
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

    // The same rect the renderer and the click resolver use — scrolling that
    // disagreed with either would clamp the cursor against the wrong height.
    let (sessions_area, agents_area) = section_heights(ui.body_rect(terminal_size));

    // `rows_area` strips each section's header and `rows_per_height` converts
    // what is left — the same two the renderer and the click resolver go
    // through, so scrolling cannot clamp against a height none of them drew.
    ui.sessions_pane.keep_visible(rows_per_height(
        rows_area(sessions_area, SectionFocus::Sessions).height,
        SectionFocus::Sessions,
    ));
    if let Some(agents_area) = agents_area {
        ui.agents_pane.keep_visible(rows_per_height(
            rows_area(agents_area, SectionFocus::Agents).height,
            SectionFocus::Agents,
        ));
    }
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cards: Vec<WindowCard>,
    current_window_id: Option<&str>,
    surface: Surface,
) -> Result<Option<SwitcherAction>> {
    let mut ui = SwitcherUi::new(cards, current_window_id, terminal.size()?);
    ui.surface = surface;
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
            // Cheap, and it has to be re-done after every follow: the dock has
            // moved to a new window with a different work pane by then.
            if surface == Surface::Dock {
                crate::dock::match_host_cwd();
            }
            last_card_refresh = now;
        }
        // The dock draws no preview — the work pane beside it *is* the preview —
        // so refreshing one is pure cost, and the cost lands exactly where it
        // shows. `refresh_for` re-captures whenever the selected target changes,
        // so every `j`/`k` was shelling out to `capture-pane` for panes nothing
        // would render, stalling the draw loop mid-navigation. The area it
        // measured came from `switcher_layout` — the *popup's* geometry — so the
        // dock was sizing captures to a rect it does not even have.
        if surface == Surface::Popup {
            let preview_area = switcher_layout(
                terminal.size()?,
                ui.show_help,
                ui.view,
                compact_lines(&ui.filtered).len(),
            )
            .preview;
            preview.refresh_for(ui.selected_target(), preview_area, now);
        }
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
                ui.surface,
            )
        })?;

        if !event::poll(TUI_TICK_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Mouse(mouse) if ui.prompt.is_none() => {
                let terminal_size = terminal.size()?;
                let navigation_height = ui.navigation_height(terminal_size);
                if let Some(result) = ui.handle_mouse(mouse, navigation_height, terminal_size) {
                    // The dock performs the switch and stays open; the popup
                    // returns the action and tears down first.
                    if surface == Surface::Dock {
                        if let Some(action) = result {
                            let _ = execute_action(action);
                        }
                    } else {
                        return Ok(result);
                    }
                }
            }
            // Not `terminal.clear()`: that flushes its escape immediately, so
            // the pane blanks and stays blank until the next draw. The dock
            // resizes every time it is carried into another window, which made
            // that blank flash the visible cost of switching.
            Event::Resize(_, _) => queue_full_repaint(terminal)?,
            Event::Key(key) => {
                if let Some(result) = ui.handle_key(key, terminal.size()?) {
                    if surface == Surface::Dock {
                        if let Some(action) = result {
                            let _ = execute_action(action);
                        }
                    } else {
                        return Ok(result);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentState, AgentStatus, FoldedAgent};
    use crate::test_support::test_card;

    // `SwitcherUi::new` reads the real tmux globals for the view, the input
    // mode and the expansion set, so tests go through `with_settings` and
    // inject all three: every assertion then starts from the default sidebar,
    // in Keys mode, with nothing remembered as expanded, regardless of what
    // the developer's tmux server happens to hold.
    fn ui_with(cards: Vec<WindowCard>) -> SwitcherUi {
        ui_with_current(cards, None)
    }

    fn ui_with_current(cards: Vec<WindowCard>, current_window_id: Option<&str>) -> SwitcherUi {
        SwitcherUi::with_settings(
            cards,
            current_window_id,
            size(),
            (HashSet::new(), HashSet::new()),
            ExpandDefault::None,
            ViewMode::Sidebar,
            InputMode::Keys,
        )
    }

    /// The view the branch deliberately left on the flat `GridState` list.
    fn palette_ui(cards: Vec<WindowCard>) -> SwitcherUi {
        SwitcherUi::with_settings(
            cards,
            None,
            size(),
            (HashSet::new(), HashSet::new()),
            ExpandDefault::None,
            ViewMode::Palette,
            InputMode::Keys,
        )
    }

    /// Three single-window sessions, so section rows and grid rows line up
    /// one-to-one and a test can name exactly which window a key acted on.
    fn three_sessions() -> Vec<WindowCard> {
        ["alpha", "bravo", "charlie"]
            .into_iter()
            .map(|name| {
                let mut card = test_card(name, "0");
                card.window_name = format!("{name}-win");
                card
            })
            .collect()
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn dock_ui(cards: Vec<WindowCard>) -> SwitcherUi {
        let mut ui = SwitcherUi::with_settings(
            cards,
            None,
            size(),
            (HashSet::new(), HashSet::new()),
            ExpandDefault::None,
            ViewMode::Sidebar,
            InputMode::Keys,
        );
        ui.surface = Surface::Dock;
        ui
    }

    /// A wheel event. Its coordinates never matter — scrolling drives whichever
    /// section has focus, not whatever is under the pointer.
    fn wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// The body rect the renderer hands the sections — the same rect
    /// `handle_mouse` resolves clicks against.
    fn sections_body(ui: &SwitcherUi) -> Rect {
        switcher_layout_for_input(
            size(),
            ui.show_help,
            ui.view,
            compact_lines(&ui.filtered).len(),
            ui.input,
        )
        .sessions
    }

    /// The window a closing result opens, by session name.
    fn opened(result: &Option<Option<SwitcherAction>>) -> Option<&str> {
        match result {
            Some(Some(SwitcherAction::Select(card))) => Some(card.session_name.as_str()),
            _ => None,
        }
    }

    fn session_names(sessions: &[SessionGroup]) -> Vec<&str> {
        sessions
            .iter()
            .map(|session| session.session_name.as_str())
            .collect()
    }

    fn window_ids(session: &SessionGroup) -> Vec<&str> {
        session
            .cards
            .iter()
            .map(|card| card.window_id.as_str())
            .collect()
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
        let (sessions_area, _) = section_heights(body);
        let row_height = sessions_area.height.saturating_sub(1) as usize;

        assert!(ui
            .sessions_pane
            .visible_range(row_height)
            .contains(&ui.sessions_pane.cursor));
    }

    /// A query narrowing a session's windows must auto-expand it for as long
    /// as the query stands, without ever writing into `expanded` — that field
    /// is persisted to a tmux global option, so a query leaking into it would
    /// silently rewrite the user's remembered collapse state.
    #[test]
    fn typing_a_query_expands_the_matching_session_without_touching_persisted_state() {
        let mut alpha = test_card("dotfiles", "0");
        alpha.window_name = "alpha".to_owned();
        let mut needle = test_card("dotfiles", "1");
        needle.window_name = "zzzneedle".to_owned();
        let other = test_card("other", "0");
        let mut ui = ui_with(vec![alpha, needle, other]);
        assert!(ui.expanded.is_empty());
        assert!(ui.search_expanded.is_empty());

        ui.query = "zzzneedle".to_owned();
        ui.refilter(40);

        assert!(ui.search_expanded.contains("dotfiles"));
        assert!(ui.expanded.is_empty());
        assert_eq!(ui.sessions_pane.len(), 2);
        assert!(matches!(
            ui.sessions_pane.items()[0].kind,
            RowKind::Session { expanded: true, .. }
        ));
        assert!(matches!(
            ui.sessions_pane.items()[1].kind,
            RowKind::Window { .. }
        ));

        ui.query.clear();
        ui.refilter(40);

        assert!(ui.search_expanded.is_empty());
        assert!(ui.expanded.is_empty());
        assert!(matches!(
            ui.sessions_pane.items()[0].kind,
            RowKind::Session { expanded: false, .. }
        ));
    }

    #[test]
    fn enter_selects_the_focused_session_row() {
        let card = test_card("dotfiles", "0");
        let mut ui = ui_with(vec![card.clone()]);

        let result = ui.handle_key(key(KeyCode::Enter), size());

        assert_eq!(result, Some(Some(SwitcherAction::Select(card))));
    }

    #[test]
    fn enter_selects_the_focused_agent_row_after_tab() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let plain = test_card("dotfiles", "1");
        let mut ui = ui_with(vec![agent.clone(), plain]);
        ui.handle_key(key(KeyCode::Tab), size());
        assert_eq!(ui.focus, SectionFocus::Agents);

        let result = ui.handle_key(key(KeyCode::Enter), size());

        assert_eq!(result, Some(Some(SwitcherAction::Select(agent))));
    }

    #[test]
    fn space_selects_the_focused_row_in_keys_mode() {
        let card = test_card("dotfiles", "0");
        let mut ui = ui_with(vec![card.clone()]);
        assert_eq!(ui.input, InputMode::Keys);

        let result = ui.handle_key(key(KeyCode::Char(' ')), size());

        assert_eq!(result, Some(Some(SwitcherAction::Select(card))));
    }

    // ---- The sections cursor is the one every key acts on --------------
    //
    // Tasks 5-8 moved navigation and selection onto the two panes, but every
    // other key kept reading `GridState` — a cursor section navigation never
    // touches and the sidebar never draws. Each test below drives the visible
    // cursor off row 0 first, so a key still reading `GridState` acts on a
    // different, nameable window.

    /// `C-l`/`C-h` mirror `l`/`h`, but reach the sections through
    /// `handle_ctrl_char`, which runs in every input mode — including Search,
    /// where a plain `l` is text. Without these two tests, dropping either
    /// `uses_sections()` branch would still pass the whole suite.
    /// Numbers swallows every plain character (`h`, `l`, even `q`), so in the
    /// sections views it is a mode where the sidebar answers only the arrows
    /// and looks broken. Shift-Tab must step straight over it.
    #[test]
    fn the_sections_input_cycle_skips_the_unusable_numbers_mode() {
        let mut ui = ui_with(three_sessions());
        assert_eq!(ui.input, InputMode::Keys);

        ui.handle_key(key(KeyCode::BackTab), size());
        assert_eq!(ui.input, InputMode::Search);

        ui.handle_key(key(KeyCode::BackTab), size());
        assert_eq!(ui.input, InputMode::Keys);
    }

    /// The palette renders the row numbers Numbers mode addresses, so it keeps
    /// the full three-way cycle.
    #[test]
    fn the_palette_input_cycle_still_offers_numbers() {
        let mut ui = palette_ui(three_sessions());
        assert_eq!(ui.input, InputMode::Keys);

        ui.handle_key(key(KeyCode::BackTab), size());

        assert_eq!(ui.input, InputMode::Numbers);
    }

    /// Numbers is persisted across opens, so someone who reached it before the
    /// cycle skipped it would otherwise open a sidebar that ignores `h`/`l`.
    #[test]
    fn a_persisted_numbers_mode_is_coerced_out_of_the_sections_views() {
        let ui = SwitcherUi::with_settings(
            three_sessions(),
            None,
            size(),
            (HashSet::new(), HashSet::new()),
            ExpandDefault::None,
            ViewMode::Sidebar,
            InputMode::Numbers,
        );

        assert_eq!(ui.input, InputMode::Keys);
    }

    /// The dock is not a modal — nothing typed inside it may tear down the
    /// pane, because that collapses the window layout. `prefix + b` is the only
    /// way out, and that is a tmux binding this loop never sees.
    #[test]
    fn q_and_esc_do_not_close_the_dock() {
        let mut ui = dock_ui(three_sessions());

        assert!(ui.handle_key(key(KeyCode::Char('q')), size()).is_none());
        assert!(ui.handle_key(key(KeyCode::Esc), size()).is_none());
    }

    /// They keep their useful half: Esc still clears the query.
    #[test]
    fn esc_clears_the_query_in_the_dock() {
        let mut ui = dock_ui(three_sessions());
        ui.input = InputMode::Search;
        ui.query = "brav".to_owned();
        ui.refilter(40);

        assert!(ui.handle_key(key(KeyCode::Esc), size()).is_none());

        assert!(ui.query.is_empty());
    }

    #[test]
    fn q_and_esc_still_close_the_popup() {
        let mut ui = ui_with(three_sessions());
        assert_eq!(ui.handle_key(key(KeyCode::Char('q')), size()), Some(None));

        let mut ui = ui_with(three_sessions());
        assert_eq!(ui.handle_key(key(KeyCode::Esc), size()), Some(None));
    }

    /// The dock has no modal inset, so its rows sit two higher than the
    /// popup's. Resolving a dock click against the popup's geometry would
    /// silently select the wrong row.
    #[test]
    fn a_dock_click_resolves_against_the_dock_geometry() {
        let mut ui = dock_ui(three_sessions());
        let body = dock_layout(size(), ui.show_help, ui.input).sessions;

        let result = ui.handle_mouse(click(2, body.y + 3), 40, size());

        assert_eq!(ui.sessions_pane.cursor, 0);
        assert_eq!(opened(&result), Some("alpha"));
    }

    /// The two surfaces really do disagree, which is why `body_rect` exists.
    #[test]
    fn the_dock_and_popup_bodies_differ() {
        let dock = dock_ui(three_sessions());
        let popup = ui_with(three_sessions());

        assert_ne!(dock.body_rect(size()), popup.body_rect(size()));
    }

    #[test]
    fn clicking_a_session_row_selects_that_session() {
        let mut ui = ui_with(three_sessions());
        let body = sections_body(&ui);

        // Sessions spends three lines on its header and its rows are one line
        // each: line 3 is the first session, line 4 the second.
        let result = ui.handle_mouse(click(2, body.y + 4), 40, size());

        assert_eq!(ui.focus, SectionFocus::Sessions);
        assert_eq!(ui.sessions_pane.cursor, 1);
        assert_eq!(opened(&result), Some("bravo"));
    }

    #[test]
    fn clicking_a_title_focuses_the_section_without_selecting() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let mut ui = ui_with(vec![agent, test_card("gogo", "0")]);
        let body = sections_body(&ui);
        let agents_area = section_heights(body).1.expect("agents section");

        let result = ui.handle_mouse(click(2, agents_area.y), 40, size());

        assert_eq!(ui.focus, SectionFocus::Agents);
        assert!(result.is_none(), "a title click must not select");
    }

    /// The palette keeps the flat `GridState` list, so a click there must not
    /// reach the sections resolver at all.
    #[test]
    fn clicking_in_the_palette_selects_nothing() {
        let mut ui = palette_ui(three_sessions());
        let before = ui.state.clone();

        let result = ui.handle_mouse(click(2, 3), 40, size());

        assert!(result.is_none());
        assert_eq!(ui.state, before);
    }

    /// Two agents from one Neovim share a host pane, so selecting either used
    /// to produce the same action and land on whichever slot was already open.
    /// Each now resolves to its own clone by name.
    #[test]
    fn each_folded_agent_selects_itself_not_just_its_host() {
        let mut host = test_card("dotfiles", "0");
        host.window_name = "config".to_owned();
        host.folded_agents = vec![
            FoldedAgent {
                pane_id: "%20".to_owned(),
                status: AgentStatus {
                    agent: Some(AgentKind::Claude),
                    state: AgentState::Idle,
                    seen: true,
                    run_started_at: None,
                },
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
        let mut ui = ui_with(vec![host]);
        ui.focus = SectionFocus::Agents;

        let clone_of = |ui: &SwitcherUi| match ui.select_target() {
            Some(Some(SwitcherAction::SelectAgent { clone, .. })) => clone,
            other => panic!("expected SelectAgent, got {other:?}"),
        };

        ui.agents_pane.cursor = 0;
        assert_eq!(clone_of(&ui), "claude_1");
        ui.agents_pane.cursor = 1;
        assert_eq!(clone_of(&ui), "claude_2");
    }

    /// A window running an agent directly needs no clone: focusing the window
    /// is the whole job, so it stays a plain `Select`.
    #[test]
    fn a_direct_agent_stays_a_plain_select() {
        let mut card = test_card("work", "0");
        card.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };
        let mut ui = ui_with(vec![card]);
        ui.focus = SectionFocus::Agents;

        assert!(matches!(
            ui.select_target(),
            Some(Some(SwitcherAction::Select(_)))
        ));
    }

    #[test]
    fn ctrl_l_expands_and_ctrl_h_collapses_the_selected_session() {
        let mut ui = ui_with(vec![test_card("dotfiles", "0"), test_card("dotfiles", "1")]);
        assert_eq!(ui.sessions_pane.len(), 1);

        ui.handle_key(ctrl(KeyCode::Char('l')), size());

        assert!(ui.expanded.contains("dotfiles"));
        assert_eq!(ui.sessions_pane.len(), 3);

        ui.handle_key(ctrl(KeyCode::Char('h')), size());

        assert!(!ui.expanded.contains("dotfiles"));
        assert_eq!(ui.sessions_pane.len(), 1);
    }

    #[test]
    fn ctrl_h_and_ctrl_l_still_move_the_grid_cursor_in_the_palette() {
        let mut ui = palette_ui(three_sessions());
        let start = ui.state.clone();

        ui.handle_key(ctrl(KeyCode::Char('l')), size());
        let after_l = ui.state.clone();
        assert_ne!(after_l, start, "C-l should move the palette's grid cursor");

        ui.handle_key(ctrl(KeyCode::Char('h')), size());

        assert_eq!(ui.state, start, "C-h should move it back");
        assert!(
            ui.expanded.is_empty(),
            "the palette must not touch the expansion set"
        );
    }

    #[test]
    fn ctrl_j_opens_the_window_below_the_sections_cursor() {
        let mut ui = ui_with(three_sessions());
        ui.handle_key(key(KeyCode::Char('j')), size());
        assert_eq!(ui.sessions_pane.cursor, 1);

        let result = ui.handle_key(ctrl(KeyCode::Char('j')), size());

        assert_eq!(opened(&result), Some("charlie"));
    }

    #[test]
    fn ctrl_k_opens_the_window_above_the_sections_cursor() {
        let mut ui = ui_with(three_sessions());
        ui.handle_key(key(KeyCode::Char('j')), size());
        ui.handle_key(key(KeyCode::Char('j')), size());
        assert_eq!(ui.sessions_pane.cursor, 2);

        let result = ui.handle_key(ctrl(KeyCode::Char('k')), size());

        assert_eq!(opened(&result), Some("bravo"));
    }

    /// The destructive one: the prompt pre-filled from `GridState` and renamed
    /// that window id, so `r` silently renamed a window the user was not
    /// looking at.
    #[test]
    fn r_prefills_the_rename_prompt_from_the_row_under_the_cursor() {
        let mut ui = ui_with(three_sessions());
        ui.handle_key(key(KeyCode::Char('j')), size());

        ui.handle_key(key(KeyCode::Char('r')), size());

        let prompt = ui.prompt.expect("a rename prompt");
        assert_eq!(
            prompt.kind,
            PromptKind::RenameWindow {
                window_id: "@bravo-0".to_owned()
            }
        );
        assert_eq!(prompt.input, "bravo-win");
    }

    #[test]
    fn n_opens_a_new_window_in_the_session_under_the_cursor() {
        let mut ui = ui_with(three_sessions());
        ui.handle_key(key(KeyCode::Char('j')), size());

        ui.handle_key(key(KeyCode::Char('n')), size());

        assert_eq!(
            ui.prompt.expect("a new-window prompt").kind,
            PromptKind::NewWindow {
                session_name: "bravo".to_owned()
            }
        );
    }

    #[test]
    fn shift_j_reorders_the_session_under_the_cursor() {
        let mut ui = ui_with(three_sessions());
        ui.handle_key(key(KeyCode::Char('j')), size());

        ui.handle_key(key(KeyCode::Char('J')), size());

        assert_eq!(session_names(&ui.sessions), ["alpha", "charlie", "bravo"]);
        // The rows are rebuilt, and the cursor rides along with the session it
        // moved rather than staying on the index it used to sit at.
        assert_eq!(ui.sessions_pane.cursor, 2);
        assert_eq!(
            row_key(ui.sessions_pane.selected().expect("a selected row")),
            "bravo"
        );
    }

    #[test]
    fn alt_j_swaps_the_window_under_the_cursor() {
        let cards = vec![
            test_card("alpha", "0"),
            test_card("alpha", "1"),
            test_card("alpha", "2"),
        ];
        let mut ui = ui_with(cards);
        ui.handle_key(key(KeyCode::Char('l')), size());
        ui.handle_key(key(KeyCode::Char('j')), size());
        ui.handle_key(key(KeyCode::Char('j')), size());

        ui.handle_key(alt(KeyCode::Char('j')), size());

        assert_eq!(
            window_ids(&ui.sessions[0]),
            ["@alpha-0", "@alpha-2", "@alpha-1"]
        );
        // Rebuilt rows, so the sidebar shows the move before the next refresh.
        assert_eq!(
            ui.sessions_pane
                .selected()
                .expect("a selected row")
                .target
                .window_id,
            "@alpha-1"
        );
    }

    /// Spec §5 leaves the session-edge jumps unbound in the Sessions section.
    /// They were not unbound — they moved the cursor nothing draws.
    #[test]
    fn shift_h_and_shift_l_are_unbound_in_the_sections() {
        // Opened from bravo, so the grid cursor starts on row 1 and an edge
        // jump in *either* direction would visibly move it. (Asserted after
        // each key: L and H are inverses, so a single L-then-H pair would come
        // back to where it started and hide the movement.)
        let mut ui = ui_with_current(three_sessions(), Some("@bravo-0"));
        let grid = ui.state.clone();
        let cursor = ui.sessions_pane.cursor;
        assert_eq!(grid.selected_row, 1);

        ui.handle_key(key(KeyCode::Char('L')), size());
        assert_eq!(ui.state, grid);

        ui.handle_key(key(KeyCode::Char('H')), size());
        assert_eq!(ui.state, grid);
        assert_eq!(ui.sessions_pane.cursor, cursor);
    }

    /// Scrolling worked before the branch and stopped working with it: two
    /// wheel events left the sections cursor at 0.
    #[test]
    fn the_mouse_wheel_scrolls_the_focused_section() {
        let mut ui = ui_with(three_sessions());

        assert!(ui.handle_mouse(wheel(MouseEventKind::ScrollDown), 40, size()).is_none());
        ui.handle_mouse(wheel(MouseEventKind::ScrollDown), 40, size());
        assert_eq!(ui.sessions_pane.cursor, 2);

        ui.handle_mouse(wheel(MouseEventKind::ScrollUp), 40, size());
        assert_eq!(ui.sessions_pane.cursor, 1);
    }

    /// `render_sections` draws no row numbers, so a digit has nothing on
    /// screen to address. It still drove the hidden grid, and `2,1` opened a
    /// window the sidebar never showed.
    #[test]
    fn numbered_addressing_is_inert_in_the_sections() {
        let mut ui = ui_with(three_sessions());
        ui.input = InputMode::Numbers;
        let grid = ui.state.clone();

        assert_eq!(ui.handle_key(key(KeyCode::Char('2')), size()), None);
        assert_eq!(ui.handle_key(key(KeyCode::Char(',')), size()), None);
        assert_eq!(ui.handle_key(key(KeyCode::Char('1')), size()), None);

        assert!(ui.numbered_input.is_empty());
        assert_eq!(ui.state, grid);
        assert_eq!(ui.sessions_pane.cursor, 0);
    }

    /// j/k still navigate in Numbers mode, and there too they drive the
    /// visible cursor.
    #[test]
    fn j_moves_the_focused_section_in_numbers_mode() {
        let mut ui = ui_with(three_sessions());
        ui.input = InputMode::Numbers;

        ui.handle_key(key(KeyCode::Char('j')), size());

        assert_eq!(ui.sessions_pane.cursor, 1);
    }

    /// The count prefix accumulated but `j`/`k` short-circuited before
    /// consuming it, so `2` then `j` moved one row and swallowed the count.
    #[test]
    fn a_count_prefix_is_swallowed_in_the_sections() {
        let mut ui = ui_with(three_sessions());

        ui.handle_key(key(KeyCode::Char('2')), size());
        assert_eq!(ui.movement_count, None);

        ui.handle_key(key(KeyCode::Char('j')), size());
        assert_eq!(ui.sessions_pane.cursor, 1);
    }

    /// The "open pre-moved" launcher binding (`Ctrl+j` opens the switcher
    /// already moved one down) applied to `GridState` only, leaving it inert
    /// in the sidebar.
    #[test]
    fn the_initial_move_drives_the_sections_cursor() {
        let mut ui = ui_with(three_sessions());

        ui.apply_initial_move(Direction::Down, size());

        assert_eq!(ui.sessions_pane.cursor, 1);
    }

    /// Spec §4: the attached session starts expanded, and the switcher opens
    /// on the window you are in — the sections cursor used to start at 0
    /// however deep in the list that window was. `None` is a fresh server,
    /// which is what makes the seeding fire.
    #[test]
    fn opening_puts_the_cursor_on_the_current_window() {
        let cards = vec![
            test_card("alpha", "0"),
            test_card("bravo", "0"),
            test_card("bravo", "1"),
        ];

        let ui = SwitcherUi::with_settings(
            cards,
            Some("@bravo-1"),
            size(),
            (HashSet::new(), HashSet::new()),
            ExpandDefault::Attached,
            ViewMode::Sidebar,
            InputMode::Keys,
        );

        assert!(ui.expanded.contains("bravo"));
        let selected = ui.sessions_pane.selected().expect("a selected row");
        assert_eq!(selected.target.window_id, "@bravo-1");
        assert!(matches!(selected.kind, RowKind::Window { .. }));
    }

    /// With the session collapsed there is no window row to land on, so the
    /// cursor takes the session row that stands in for it.
    #[test]
    fn a_collapsed_current_session_takes_the_cursor_on_its_session_row() {
        let cards = vec![
            test_card("alpha", "0"),
            test_card("bravo", "0"),
            test_card("bravo", "1"),
        ];

        let ui = SwitcherUi::with_settings(
            cards,
            Some("@bravo-0"),
            size(),
            // Both sessions are remembered, "alpha" expanded and "bravo" not,
            // so the default gets no vote and "bravo" stays collapsed.
            (
                HashSet::from(["alpha".to_owned()]),
                HashSet::from(["alpha".to_owned(), "bravo".to_owned()]),
            ),
            ExpandDefault::Attached,
            ViewMode::Sidebar,
            InputMode::Keys,
        );

        assert!(!ui.expanded.contains("bravo"));
        let selected = ui.sessions_pane.selected().expect("a selected row");
        assert_eq!(selected.target.window_id, "@bravo-0");
        assert!(matches!(selected.kind, RowKind::Session { .. }));
    }

    /// The palette is deliberately still on `GridState`, and none of the
    /// above may have leaked into it: counts accumulate, `L` jumps to the
    /// session edge, and `Ctrl-j` opens relative to the grid cursor.
    #[test]
    fn the_palette_still_drives_the_grid_cursor() {
        let mut ui = palette_ui(three_sessions());

        ui.handle_key(key(KeyCode::Char('2')), size());
        assert_eq!(ui.movement_count, Some(2));

        ui.handle_key(key(KeyCode::Char('L')), size());
        assert_eq!(ui.state.selected_row, 1);

        let result = ui.handle_key(ctrl(KeyCode::Char('j')), size());
        assert_eq!(opened(&result), Some("charlie"));
    }

    /// The 300ms card refresh rebuilt the rows with the auto-expansion set
    /// `refilter` had computed for the *previous* card list, so a window that
    /// appeared while a query stood stayed hidden inside a collapsed session —
    /// exactly what auto-expansion exists to prevent.
    #[test]
    fn a_refresh_expands_a_session_the_live_query_has_just_narrowed() {
        let mut alpha = test_card("dotfiles", "0");
        alpha.window_name = "alpha".to_owned();
        let other = test_card("other", "0");
        let mut ui = ui_with(vec![alpha.clone(), other.clone()]);

        ui.query = "zzzneedle".to_owned();
        ui.refilter(40);
        assert!(ui.search_expanded.is_empty());

        let mut needle = test_card("dotfiles", "1");
        needle.window_name = "zzzneedle".to_owned();
        ui.refresh_cards(vec![alpha, needle.clone(), other], 40);

        assert!(ui.search_expanded.contains("dotfiles"));
        // Never into the persisted set.
        assert!(ui.expanded.is_empty());
        assert!(ui.sessions_pane.items().iter().any(|row| {
            matches!(row.kind, RowKind::Window { .. }) && row.target.window_id == needle.window_id
        }));
    }

    /// Focus must not strand on a section that is no longer rendered. The
    /// reset lives in `rebuild_panes`; this drives it through the refresh
    /// path, which is how an agent actually disappears.
    #[test]
    fn focus_returns_to_sessions_when_the_last_agent_exits() {
        let mut agent = test_card("dotfiles", "0");
        agent.agent_status = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        };
        let plain = test_card("dotfiles", "1");
        let mut ui = ui_with(vec![agent.clone(), plain.clone()]);
        ui.handle_key(key(KeyCode::Tab), size());
        assert_eq!(ui.focus, SectionFocus::Agents);

        let mut stopped = agent;
        stopped.agent_status = AgentStatus::unknown();
        ui.refresh_cards(vec![stopped, plain], 40);

        assert!(ui.agents_pane.is_empty());
        assert_eq!(ui.focus, SectionFocus::Sessions);
    }

    /// No row is focused when the query has filtered the focused section to
    /// nothing. Enter then does nothing and leaves the switcher open, the
    /// same as the palette's `select_key_action` does when nothing is
    /// selected.
    #[test]
    fn enter_does_nothing_when_the_focused_pane_is_empty() {
        let mut ui = ui_with(vec![test_card("dotfiles", "0")]);
        ui.query = "nomatch-zzz".to_owned();
        ui.refilter(40);
        assert!(ui.sessions_pane.is_empty());

        let result = ui.handle_key(key(KeyCode::Enter), size());

        assert_eq!(result, None);
    }
}

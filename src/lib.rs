use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseEventKind,
    },
    execute,
    style::force_color_output,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};

pub const TMUX_ORANGE: Color = Color::Indexed(202);
const SIDEBAR_WIDTH_PERCENT: u16 = 25;
const SIDEBAR_MIN_WIDTH: u16 = 28;
const SIDEBAR_MAX_WIDTH: u16 = 64;
const FLOATING_LIST_INSET: u16 = 2;
const PALETTE_WIDTH_PERCENT: u16 = 55;
const PALETTE_MIN_WIDTH: u16 = 44;
const PALETTE_MAX_WIDTH: u16 = 80;
const PALETTE_TOP_MARGIN: u16 = 1;
/// Where the palette box's bottom edge sits, as a percentage of the screen
/// height. The search prompt lives on that edge, a bit above center, and the
/// list grows upward from it.
const PALETTE_BOTTOM_PERCENT: u16 = 55;
/// Rows the search bar occupies at the top of the list: the prompt line plus a
/// separator rule under it.
const SEARCH_BAR_ROWS: u16 = 2;
const SEARCH_PLACEHOLDER: &str = "type to filter";
const KEYS_PLACEHOLDER: &str = "j/k move · [n]j/k open";
// Shown in the modal top bar; the short form fits beside "[?] Help" within
// the narrow sidebar's width.
const SWITCHER_NAME: &str = "agent-switcher";
const HELP_LABEL: &str = "[?] Help";
const HELP_LINE_COUNT: u16 = 13;
const PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const CARD_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
/// How often the whole screen is forcibly repainted. Ratatui only rewrites
/// cells it believes changed, so anything that scribbles on the terminal
/// behind its back — tmux compositing glitches while a busy pane redraws
/// under the popup, wide glyphs in mirrored pane content nudging the cursor —
/// would otherwise stay smeared across the modal until that cell happens to
/// change. A periodic full redraw self-heals within half a second.
const FULL_REDRAW_INTERVAL: Duration = Duration::from_millis(500);
const TUI_TICK_INTERVAL: Duration = Duration::from_millis(50);
const STATUS_DAEMON_INTERVAL: Duration = Duration::from_millis(300);
const STATUS_DAEMON_OWNERSHIP_CHECK_POLLS: u32 = 10;
const STATUS_CAPTURE_LINES: usize = 25;
/// Consecutive idle polls required before committing a Working/Blocked -> Idle
/// transition, so a single stray sample can't flash a spurious "done" or reset
/// the run timer. At STATUS_DAEMON_INTERVAL this is roughly a 1s settle window.
const IDLE_DEBOUNCE_POLLS: u32 = 4;
/// Consecutive polls required before committing a settled Idle pane into a busy
/// state, so a single stray Working/Blocked sample can't wipe a committed "done"
/// or restart its timer. Kept short so real work still shows promptly.
const BUSY_DEBOUNCE_POLLS: u32 = 2;
const STATUS_DAEMON_PID_OPTION: &str = "@tmux_agent_switcher_status_daemon_pid";
const STATUS_AGENT_OPTION: &str = "@tmux_agent_switcher_agent";
const STATUS_STATE_OPTION: &str = "@tmux_agent_switcher_state";
const STATUS_SEEN_OPTION: &str = "@tmux_agent_switcher_seen";
const STATUS_RUN_STARTED_OPTION: &str = "@tmux_agent_switcher_run_started_at";
const STATUS_UPDATED_OPTION: &str = "@tmux_agent_switcher_updated";
const STATUS_WINDOW_ICON_OPTION: &str = "@tmux_agent_switcher_window_icon";
const VIEW_MODE_OPTION: &str = "@tmux_agent_switcher_view";
const INPUT_MODE_OPTION: &str = "@tmux_agent_switcher_input";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentKind {
    Codex,
    Claude,
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

    fn done(agent: Option<AgentKind>) -> Self {
        Self {
            agent,
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        }
    }

    fn is_done(self) -> bool {
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionGroup {
    pub session_name: String,
    pub cards: Vec<WindowCard>,
}

#[derive(Clone, Debug)]
struct PreviewMirror {
    window_id: Option<String>,
    area: Option<Rect>,
    text: Text<'static>,
    refreshed_at: Option<Instant>,
}

impl Default for PreviewMirror {
    fn default() -> Self {
        Self {
            window_id: None,
            area: None,
            text: Text::default(),
            refreshed_at: None,
        }
    }
}

impl PreviewMirror {
    fn should_refresh(&self, window_id: &str, area: Rect, now: Instant) -> bool {
        if self.window_id.as_deref() != Some(window_id) || self.area != Some(area) {
            return true;
        }

        self.refreshed_at
            .map(|refreshed_at| now.duration_since(refreshed_at) >= PREVIEW_REFRESH_INTERVAL)
            .unwrap_or(true)
    }

    fn refresh_for(&mut self, card: Option<&WindowCard>, area: Rect, now: Instant) {
        let Some(card) = card else {
            return;
        };
        if !self.should_refresh(&card.window_id, area, now) {
            return;
        }

        if let Ok(text) = capture_window_preview_text(&card.window_id, area)
            .or_else(|_| capture_preview_text(&card.target_pane_id))
        {
            self.record_success(&card.window_id, area, text, now);
        }
    }

    fn record_success(&mut self, window_id: &str, area: Rect, text: Text<'static>, now: Instant) {
        self.window_id = Some(window_id.to_owned());
        self.area = Some(area);
        self.text = text;
        self.refreshed_at = Some(now);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowPreviewPane {
    pane_id: String,
    pane_index: String,
    pane_active: bool,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    pane_title: String,
    pane_current_command: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitcherAction {
    Select(WindowCard),
    NewWindow {
        session_name: String,
        window_name: String,
    },
    NewSession {
        session_name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactLine {
    Session {
        session_index: usize,
    },
    Card {
        session_index: usize,
        card_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// Where the window list sits: docked to the left or right edge with the
/// preview beside it (Sidebar / SidebarRight), or floating around the upper
/// middle of the screen with the preview filling the whole screen behind it,
/// like an editor command palette (Palette).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewMode {
    Sidebar,
    SidebarRight,
    Palette,
}

impl ViewMode {
    fn toggled(self) -> Self {
        match self {
            ViewMode::Sidebar => ViewMode::SidebarRight,
            ViewMode::SidebarRight => ViewMode::Palette,
            ViewMode::Palette => ViewMode::Sidebar,
        }
    }
}

fn parse_view_mode(value: &str) -> Option<ViewMode> {
    match value {
        "sidebar" | "left" => Some(ViewMode::Sidebar),
        "sidebar-right" | "right" => Some(ViewMode::SidebarRight),
        "palette" | "center" => Some(ViewMode::Palette),
        _ => None,
    }
}

fn format_view_mode(view: ViewMode) -> &'static str {
    match view {
        ViewMode::Sidebar => "sidebar",
        ViewMode::SidebarRight => "sidebar-right",
        ViewMode::Palette => "palette",
    }
}

/// How keystrokes are interpreted, toggled with Tab: Search (the default)
/// sends typed characters to the filter query, telescope-style; Keys restores
/// Vim-style single-key bindings (j/k movement with optional counts, n/N, q).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
    Search,
    Keys,
}

impl InputMode {
    fn toggled(self) -> Self {
        match self {
            InputMode::Search => InputMode::Keys,
            InputMode::Keys => InputMode::Search,
        }
    }
}

fn parse_input_mode(value: &str) -> Option<InputMode> {
    match value {
        "search" => Some(InputMode::Search),
        "keys" => Some(InputMode::Keys),
        _ => None,
    }
}

fn format_input_mode(input: InputMode) -> &'static str {
    match input {
        InputMode::Search => "search",
        InputMode::Keys => "keys",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PromptKind {
    NewWindow { session_name: String },
    NewSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PromptState {
    kind: PromptKind,
    input: String,
}

impl PromptState {
    fn new(kind: PromptKind) -> Self {
        Self {
            kind,
            input: String::new(),
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            PromptKind::NewWindow { .. } => "New window name",
            PromptKind::NewSession => "New session name",
        }
    }

    fn submit(&self) -> Option<SwitcherAction> {
        let name = self.input.trim();
        if name.is_empty() {
            return None;
        }

        match &self.kind {
            PromptKind::NewWindow { session_name } => Some(SwitcherAction::NewWindow {
                session_name: session_name.clone(),
                window_name: name.to_owned(),
            }),
            PromptKind::NewSession => Some(SwitcherAction::NewSession {
                session_name: name.to_owned(),
            }),
        }
    }
}

fn handle_prompt_key(prompt: &mut PromptState, key: KeyEvent) -> Option<Option<SwitcherAction>> {
    match key.code {
        KeyCode::Esc => Some(None),
        KeyCode::Enter => prompt.submit().map(Some),
        KeyCode::Backspace => {
            prompt.input.pop();
            None
        }
        KeyCode::Char(ch) => {
            prompt.input.push(ch);
            None
        }
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridState {
    pub selected_row: usize,
    pub selected_column: usize,
    pub preferred_column: usize,
    pub row_offset: usize,
}

impl GridState {
    pub fn new() -> Self {
        Self {
            selected_row: 0,
            selected_column: 0,
            preferred_column: 0,
            row_offset: 0,
        }
    }

    pub fn selected_card<'a>(&self, sessions: &'a [SessionGroup]) -> Option<&'a WindowCard> {
        sessions
            .get(self.selected_row)
            .and_then(|session| session.cards.get(self.selected_column))
    }

    pub fn for_window_id(sessions: &[SessionGroup], window_id: &str) -> Self {
        let mut state = Self::new();

        for (row, session) in sessions.iter().enumerate() {
            if let Some(column) = session
                .cards
                .iter()
                .position(|card| card.window_id == window_id)
            {
                state.selected_row = row;
                state.selected_column = column;
                state.preferred_column = column;
                return state;
            }
        }

        state
    }
}

impl Default for GridState {
    fn default() -> Self {
        Self::new()
    }
}

fn select_key_action(
    key: KeyEvent,
    state: &GridState,
    sessions: &[SessionGroup],
) -> Option<SwitcherAction> {
    match key.code {
        KeyCode::Enter => state
            .selected_card(sessions)
            .cloned()
            .map(SwitcherAction::Select),
        _ => None,
    }
}

pub fn parse_windows(output: &str) -> Result<Vec<TmuxWindow>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 5)?;
            Ok(TmuxWindow {
                window_id: fields[0].to_owned(),
                session_name: fields[1].to_owned(),
                window_index: fields[2].to_owned(),
                window_name: fields[3].to_owned(),
                window_flags: fields[4].to_owned(),
            })
        })
        .collect()
}

pub fn parse_panes(output: &str) -> Result<Vec<TmuxPane>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 6)?;
            let pane_pid = fields.get(6).and_then(|value| value.parse().ok());
            let cached_status = parse_cached_agent_status(
                fields.get(7).copied().unwrap_or_default(),
                fields.get(8).copied().unwrap_or_default(),
                fields.get(9).copied().unwrap_or_default(),
                fields.get(10).copied().unwrap_or_default(),
            )
            .or_else(|| {
                parse_codex_hook_status(
                    fields.get(11).copied().unwrap_or_default(),
                    fields.get(12).copied().unwrap_or_default(),
                )
            })
            .or_else(|| {
                parse_codex_hook_status(
                    fields.get(10).copied().unwrap_or_default(),
                    fields.get(11).copied().unwrap_or_default(),
                )
            })
            .unwrap_or_else(AgentStatus::unknown);

            Ok(TmuxPane {
                pane_id: fields[0].to_owned(),
                window_id: fields[1].to_owned(),
                pane_active: fields[2] == "1",
                pane_current_command: fields[3].to_owned(),
                pane_current_path: fields[4].to_owned(),
                pane_title: fields[5].to_owned(),
                pane_pid,
                agent_status: cached_status,
            })
        })
        .collect()
}

fn parse_window_preview_panes(output: &str) -> Result<Vec<WindowPreviewPane>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 9)?;
            Ok(WindowPreviewPane {
                pane_id: fields[0].to_owned(),
                pane_index: fields[1].to_owned(),
                pane_active: fields[2] == "1",
                left: fields[3].parse().unwrap_or(0),
                top: fields[4].parse().unwrap_or(0),
                width: fields[5].parse().unwrap_or(0),
                height: fields[6].parse().unwrap_or(0),
                pane_title: fields[7].to_owned(),
                pane_current_command: fields[8].to_owned(),
            })
        })
        .collect()
}

fn split_tmux_fields(line: &str, expected: usize) -> Result<Vec<&str>> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < expected {
        return Err(anyhow!(
            "expected at least {expected} tab-separated fields, got {line:?}"
        ));
    }
    Ok(fields)
}

fn parse_cached_agent_status(
    agent: &str,
    state: &str,
    seen: &str,
    run_started_at: &str,
) -> Option<AgentStatus> {
    let state = parse_agent_state(state)?;
    Some(AgentStatus {
        agent: parse_agent_kind(agent),
        state,
        seen: seen != "0",
        run_started_at: run_started_at.parse().ok(),
    })
}

fn parse_codex_hook_status(state: &str, unread: &str) -> Option<AgentStatus> {
    match state {
        "blocked" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Blocked,
            seen: true,
            run_started_at: None,
        }),
        "busy" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: None,
        }),
        "ready" if unread == "1" => Some(AgentStatus::done(Some(AgentKind::Codex))),
        "ready" | "idle" => Some(AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        }),
        _ => None,
    }
}

fn parse_agent_kind(value: &str) -> Option<AgentKind> {
    match value {
        "codex" => Some(AgentKind::Codex),
        "claude" => Some(AgentKind::Claude),
        _ => None,
    }
}

fn format_agent_kind(agent: Option<AgentKind>) -> &'static str {
    match agent {
        Some(AgentKind::Codex) => "codex",
        Some(AgentKind::Claude) => "claude",
        None => "",
    }
}

fn parse_agent_state(value: &str) -> Option<AgentState> {
    match value {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "blocked" => Some(AgentState::Blocked),
        "unknown" => Some(AgentState::Unknown),
        _ => None,
    }
}

fn format_agent_state(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Unknown => "unknown",
    }
}

pub fn codex_unread_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".local/state")
        })
        .join("tmux-codex-unread")
}

pub fn codex_unread_file(state_dir: &Path, pane_id: &str) -> PathBuf {
    state_dir.join(format!("{}.json", pane_id.trim_start_matches('%')))
}

pub fn build_cards(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    unread_dir: &Path,
) -> Vec<WindowCard> {
    build_cards_with_previews(windows, panes, &HashMap::new(), unread_dir)
}

pub fn build_cards_with_previews(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    previews: &HashMap<String, Vec<String>>,
    unread_dir: &Path,
) -> Vec<WindowCard> {
    windows
        .iter()
        .filter_map(|window| {
            let active_pane = panes
                .iter()
                .find(|pane| pane.window_id == window.window_id && pane.pane_active)
                .or_else(|| panes.iter().find(|pane| pane.window_id == window.window_id))?;

            let window_panes: Vec<&TmuxPane> = panes
                .iter()
                .filter(|pane| pane.window_id == window.window_id)
                .collect();
            let codex_unread = window_panes
                .iter()
                .any(|pane| codex_unread_file(unread_dir, &pane.pane_id).exists());
            let agent_status =
                rollup_agent_status(window_panes.iter().map(|pane| pane.agent_status));

            Some(WindowCard {
                window_id: window.window_id.clone(),
                target_pane_id: active_pane.pane_id.clone(),
                session_name: window.session_name.clone(),
                window_index: window.window_index.clone(),
                window_name: window.window_name.clone(),
                window_flags: window.window_flags.clone(),
                command: active_pane.pane_current_command.clone(),
                path: active_pane.pane_current_path.clone(),
                title: active_pane.pane_title.clone(),
                preview: previews
                    .get(&active_pane.pane_id)
                    .cloned()
                    .unwrap_or_default(),
                codex_unread: codex_unread || agent_status.is_done(),
                agent_status,
            })
        })
        .collect()
}

fn rollup_agent_status(statuses: impl Iterator<Item = AgentStatus>) -> AgentStatus {
    let mut best = AgentStatus::unknown();
    let mut best_priority = 0;

    for status in statuses {
        let priority = agent_status_priority(status);
        if priority > best_priority {
            best = status;
            best_priority = priority;
        }
    }

    best
}

fn agent_status_priority(status: AgentStatus) -> u8 {
    match status.state {
        AgentState::Blocked => 5,
        AgentState::Idle if !status.seen => 4,
        AgentState::Working => 3,
        AgentState::Idle => 2,
        AgentState::Unknown => 1,
    }
}

pub fn group_cards_by_session(cards: Vec<WindowCard>) -> Vec<SessionGroup> {
    let mut sessions: Vec<SessionGroup> = Vec::new();

    for card in cards {
        if let Some(session) = sessions
            .iter_mut()
            .find(|session| session.session_name == card.session_name)
        {
            session.cards.push(card);
        } else {
            sessions.push(SessionGroup {
                session_name: card.session_name.clone(),
                cards: vec![card],
            });
        }
    }

    sessions
}

/// Case-insensitive subsequence match, telescope-style. Returns a score —
/// higher is better — or None when `query` isn't a subsequence of `haystack`.
/// Scored over the best alignment (not the leftmost): each hit counts 1, a hit
/// right after the previous one +2, a hit at a word start +3, and a small
/// length penalty makes tighter names win ties. The lists involved are tiny,
/// so the O(query × haystack²) dynamic program is fine.
fn fuzzy_score(haystack: &str, query: &str) -> Option<i32> {
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    if needle.is_empty() {
        return Some(0);
    }

    let at_word_start = |position: usize| {
        position == 0
            || matches!(
                hay.get(position - 1),
                Some(' ' | '-' | '_' | '/' | '.' | ':')
            )
    };

    // previous[j]: best score matching the needle so far with its last hit on
    // hay[j]; rebuilt for each needle char.
    let mut previous: Vec<Option<i32>> = vec![None; hay.len()];
    for (needle_index, &needle_char) in needle.iter().enumerate() {
        let mut current: Vec<Option<i32>> = vec![None; hay.len()];
        // Best previous[k] over all k < j, maintained as j advances.
        let mut best_prior: Option<i32> = None;
        for (position, &hay_char) in hay.iter().enumerate() {
            if hay_char == needle_char {
                let base = if needle_index == 0 {
                    Some(0)
                } else {
                    let consecutive = position
                        .checked_sub(1)
                        .and_then(|prior| previous[prior])
                        .map(|score| score + 2);
                    match (best_prior, consecutive) {
                        (Some(any), Some(run)) => Some(any.max(run)),
                        (any, run) => any.or(run),
                    }
                };
                if let Some(base) = base {
                    let mut score = base + 1;
                    if at_word_start(position) {
                        score += 3;
                    }
                    current[position] = Some(score);
                }
            }
            if let Some(score) = previous[position] {
                best_prior = Some(best_prior.map_or(score, |best| best.max(score)));
            }
        }
        previous = current;
    }

    let best = previous.into_iter().flatten().max()?;
    Some(best.saturating_mul(8) - hay.len().min(i32::MAX as usize) as i32 / 8)
}

/// The text a window is matched against when filtering: its session, name,
/// (normalized) process, and the last path segment, so a query can hit any of
/// the ways a user thinks of a window.
fn card_filter_haystack(session_name: &str, card: &WindowCard) -> String {
    let path_base = card.path.rsplit('/').next().unwrap_or("");
    format!(
        "{} {} {} {}",
        session_name,
        card.window_name,
        compact_tab_process_text(card),
        path_base
    )
}

/// Prunes the session groups down to windows fuzzy-matching `query`, dropping
/// sessions with no matches. An empty (or blank) query keeps everything.
pub fn filter_sessions(sessions: &[SessionGroup], query: &str) -> Vec<SessionGroup> {
    if query.trim().is_empty() {
        return sessions.to_vec();
    }

    sessions
        .iter()
        .filter_map(|session| {
            let cards: Vec<WindowCard> = session
                .cards
                .iter()
                .filter(|card| {
                    fuzzy_score(&card_filter_haystack(&session.session_name, card), query).is_some()
                })
                .cloned()
                .collect();
            (!cards.is_empty()).then(|| SessionGroup {
                session_name: session.session_name.clone(),
                cards,
            })
        })
        .collect()
}

/// The (row, column) of the best-scoring card for `query`, or None for a blank
/// query (where "best" is meaningless and the caller keeps its selection).
fn best_match_position(sessions: &[SessionGroup], query: &str) -> Option<(usize, usize)> {
    if query.trim().is_empty() {
        return None;
    }

    let mut best: Option<(i32, (usize, usize))> = None;
    for (row, session) in sessions.iter().enumerate() {
        for (column, card) in session.cards.iter().enumerate() {
            let Some(score) =
                fuzzy_score(&card_filter_haystack(&session.session_name, card), query)
            else {
                continue;
            };
            if best
                .map(|(best_score, _)| score > best_score)
                .unwrap_or(true)
            {
                best = Some((score, (row, column)));
            }
        }
    }

    best.map(|(_, position)| position)
}

/// Re-filters the full session list after the query changed and repositions the
/// selection: onto the best match while filtering, otherwise back onto the
/// previously selected window when it survived the change.
fn apply_query(
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    sessions: &[SessionGroup],
    query: &str,
    terminal_height: u16,
) {
    let previous = state
        .selected_card(filtered)
        .map(|card| card.window_id.clone());
    *filtered = filter_sessions(sessions, query);

    if let Some((row, column)) = best_match_position(filtered, query) {
        state.selected_row = row;
        state.selected_column = column;
        state.preferred_column = column;
    } else if let Some(window_id) = previous.filter(|window_id| {
        filtered
            .iter()
            .flat_map(|session| session.cards.iter())
            .any(|card| &card.window_id == window_id)
    }) {
        *state = GridState::for_window_id(filtered, &window_id);
    } else {
        *state = fallback_grid_state(filtered, state.selected_row, state.selected_column);
    }

    keep_compact_selection_visible(state, filtered, terminal_height);
}

/// Readline-style Ctrl+W: drop trailing spaces, then the last word.
fn delete_query_word(query: &mut String) {
    while query.ends_with(' ') {
        query.pop();
    }
    while query.chars().last().is_some_and(|ch| ch != ' ') {
        query.pop();
    }
}

pub fn detect_agent_from_process_name(name: &str) -> Option<AgentKind> {
    let basename = name.rsplit('/').next().unwrap_or(name);
    if basename == "codex" || basename.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else if basename == "claude"
        || basename == "claude-code"
        || basename.starts_with("claude-")
        || is_claude_version_name(basename)
    {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

/// Claude Code's native installer runs the versioned binary at
/// `~/.local/share/claude/versions/<version>`, and Claude also sets its
/// `process.title` to that same version string. Either way tmux reports the
/// pane's current command as a bare `MAJOR.MINOR.PATCH` semver (e.g. `2.1.197`)
/// rather than `claude` (see anthropics/claude-code#49852). Treat that shape as
/// Claude Code so agent detection still fires.
fn is_claude_version_name(name: &str) -> bool {
    let mut parts = 0;
    for part in name.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
}

pub fn detect_agent_state(agent: AgentKind, evidence: &AgentEvidence) -> AgentState {
    if evidence.process_exited {
        return AgentState::Idle;
    }

    match agent {
        AgentKind::Codex => detect_codex_state(evidence),
        AgentKind::Claude => detect_claude_state(evidence),
    }
}

fn detect_agent_state_from_title(agent: AgentKind, title: &str) -> Option<AgentState> {
    let title = title.trim();
    match agent {
        AgentKind::Codex if title.contains("Action Required") => Some(AgentState::Blocked),
        AgentKind::Codex if starts_with_braille_status(title) => Some(AgentState::Working),
        AgentKind::Codex if !title.is_empty() => Some(AgentState::Idle),
        AgentKind::Claude if starts_with_braille_status(title) => Some(AgentState::Working),
        _ => None,
    }
}

fn detect_codex_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    let tail = evidence.screen_tail.to_lowercase();

    if title.contains("Action Required")
        || contains_any(
            &tail,
            &[
                "press enter to confirm or esc to cancel",
                "enter to submit answer",
                "allow command?",
                "[y/n]",
                "yes (y)",
                "no (n)",
            ],
        )
    {
        return AgentState::Blocked;
    }

    if starts_with_braille_status(title) {
        return AgentState::Working;
    }

    if !title.is_empty() {
        return AgentState::Idle;
    }

    AgentState::Idle
}

fn detect_claude_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    // Claude's prompts always sit at the bottom of the screen; only match the
    // last handful of lines so stale scrollback can't pin a state (e.g. an old
    // "do you want" line keeping a working pane marked Blocked).
    let recent = recent_screen(&evidence.screen_tail, 25);
    let recent_lower = recent.to_lowercase();

    // Blocked: a modal selection menu is on screen (the cursor is resting on one
    // of several numbered options), waiting for the user to choose. With
    // `--dangerously-skip-permissions` this is plan-mode approval, AskUserQuestion
    // menus and trust prompts rather than per-command permission asks. The match is
    // structural (wording-agnostic), plus the selection-list footer as a fallback.
    if has_selection_prompt(&recent)
        || contains_all(&recent_lower, &["enter to select", "esc to cancel"])
    {
        return AgentState::Blocked;
    }

    // Working: Claude prefixes its OSC title with a braille spinner while active.
    if starts_with_braille_status(title) {
        return AgentState::Working;
    }

    // Otherwise Claude is idle at its input prompt (title starts with ✳). The `❯`
    // input box is present while working too, so it is not an idle signal on its own.
    AgentState::Idle
}

/// True when the screen shows a Claude selection menu: the cursor (`❯`) rests on
/// a numbered option AND at least two numbered options are present. Requiring a
/// second option distinguishes a real menu from the bare `❯` input box (even when
/// the user types a single line like "1. …" into it), and stripping the box border
/// makes it work on Claude's real bordered rendering (`│ ❯ 1. Yes │`), where the
/// option no longer begins the line.
///
/// Known ambiguity: a user composing a *multi-line* numbered list in the input box
/// is structurally identical to a menu and can read as Blocked. Anchoring on a menu
/// footer would remove it, but Claude's permission/plan modals don't render one, so
/// that would miss the real prompts this exists to catch. The idle->busy debounce
/// ([`BUSY_DEBOUNCE_POLLS`]) already absorbs the common fast-typed case.
fn has_selection_prompt(text: &str) -> bool {
    let mut cursor_on_option = false;
    let mut option_lines = 0;
    for line in text.lines() {
        let line = strip_border(line);
        let (has_cursor, rest) = match line.strip_prefix('❯') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits == 0 {
            continue;
        }
        let after = &rest[digits..];
        if after.starts_with('.') || after.starts_with(')') {
            option_lines += 1;
            cursor_on_option |= has_cursor;
        }
    }
    cursor_on_option && option_lines >= 2
}

/// Strips a line's leading whitespace and box-drawing verticals so matching works
/// whether or not the content is wrapped in a border.
fn strip_border(line: &str) -> &str {
    line.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '│' | '┃' | '║' | '╎' | '┆' | '┊' | '|')
    })
}

fn recent_screen(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

fn starts_with_braille_status(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(ch) if ('\u{2800}'..='\u{28ff}').contains(&ch))
        && matches!(chars.next(), Some(' '))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

fn compact_visible_rows(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(1).max(1) as usize
}

pub fn initial_grid_state(
    sessions: &[SessionGroup],
    current_window_id: Option<&str>,
    terminal_height: u16,
) -> GridState {
    let mut state = current_window_id
        .map(|window_id| GridState::for_window_id(sessions, window_id))
        .unwrap_or_default();

    keep_compact_selection_visible(&mut state, sessions, terminal_height);

    state
}

fn compact_lines(sessions: &[SessionGroup]) -> Vec<CompactLine> {
    sessions
        .iter()
        .enumerate()
        .flat_map(|(session_index, session)| {
            std::iter::once(CompactLine::Session { session_index }).chain(
                (0..session.cards.len()).map(move |card_index| CompactLine::Card {
                    session_index,
                    card_index,
                }),
            )
        })
        .collect()
}

pub fn compact_selected_line_index(sessions: &[SessionGroup], state: &GridState) -> Option<usize> {
    let mut line_index = 0;

    for (session_index, session) in sessions.iter().enumerate() {
        if session_index == state.selected_row {
            if state.selected_column < session.cards.len() {
                return Some(line_index + 1 + state.selected_column);
            }
            return Some(line_index);
        }
        line_index += 1 + session.cards.len();
    }

    None
}

fn compact_card_positions(sessions: &[SessionGroup]) -> Vec<(usize, usize)> {
    sessions
        .iter()
        .enumerate()
        .flat_map(|(session_index, session)| {
            (0..session.cards.len()).map(move |card_index| (session_index, card_index))
        })
        .collect()
}

fn compact_rendered_line_capacity(
    lines: &[CompactLine],
    row_offset: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows == 0 {
        return 0;
    }

    if visible_rows > 1 && matches!(lines.get(row_offset), Some(CompactLine::Card { .. })) {
        visible_rows - 1
    } else {
        visible_rows
    }
}

fn compact_bottom_aligned_offset(
    lines: &[CompactLine],
    selected_line: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows <= 1 {
        return selected_line;
    }

    let mut row_offset = selected_line.saturating_add(1).saturating_sub(visible_rows);
    if matches!(lines.get(row_offset), Some(CompactLine::Card { .. })) {
        row_offset = selected_line
            .saturating_add(1)
            .saturating_sub(visible_rows - 1);
    }
    row_offset
}

fn keep_compact_selection_visible(
    state: &mut GridState,
    sessions: &[SessionGroup],
    terminal_height: u16,
) {
    let Some(selected_line) = compact_selected_line_index(sessions, state) else {
        state.row_offset = 0;
        return;
    };
    let lines = compact_lines(sessions);
    let visible_rows = compact_visible_rows(terminal_height);
    let row_offset = state.row_offset.min(lines.len().saturating_sub(1));
    let visible_capacity = compact_rendered_line_capacity(&lines, row_offset, visible_rows);

    if selected_line < row_offset {
        state.row_offset = selected_line;
    } else if selected_line >= row_offset + visible_capacity {
        state.row_offset = compact_bottom_aligned_offset(&lines, selected_line, visible_rows);
    } else {
        state.row_offset = row_offset;
    }
}

fn push_movement_count(count: &mut Option<usize>, ch: char) -> bool {
    let Some(digit) = ch.to_digit(10).map(|digit| digit as usize) else {
        return false;
    };
    if count.is_none() && digit == 0 {
        return false;
    }

    *count = Some(count.unwrap_or(0).saturating_mul(10).saturating_add(digit));
    true
}

fn take_counted_open_motion(count: &mut Option<usize>, ch: char) -> Option<(Direction, usize)> {
    let direction = match ch {
        'j' | 'J' => Direction::Down,
        'k' | 'K' => Direction::Up,
        _ => return None,
    };
    Some((direction, count.take()?))
}

pub fn move_compact_selection(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    terminal_height: u16,
) {
    move_compact_selection_by(state, sessions, direction, 1, terminal_height);
}

fn move_compact_selection_by(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    count: usize,
    terminal_height: u16,
) {
    if sessions.is_empty() {
        *state = GridState::new();
        return;
    }

    match direction {
        Direction::Up | Direction::Down => {
            let positions = compact_card_positions(sessions);
            if positions.is_empty() {
                *state = GridState::new();
                return;
            }
            let current = positions
                .iter()
                .position(|&(row, column)| {
                    row == state.selected_row && column == state.selected_column
                })
                .unwrap_or(0);
            let next = match direction {
                Direction::Up => current.saturating_sub(count),
                Direction::Down => current.saturating_add(count).min(positions.len() - 1),
                Direction::Left | Direction::Right => unreachable!(),
            };
            let (row, column) = positions[next];
            state.selected_row = row;
            state.selected_column = column;
            state.preferred_column = column;
        }
        Direction::Left | Direction::Right => {
            let session_indices: Vec<usize> = sessions
                .iter()
                .enumerate()
                .filter_map(|(index, session)| (!session.cards.is_empty()).then_some(index))
                .collect();
            if session_indices.is_empty() {
                *state = GridState::new();
                return;
            }
            let current = session_indices
                .iter()
                .position(|&index| index == state.selected_row)
                .unwrap_or(0);
            let next = match direction {
                Direction::Left if current == 0 => session_indices.len() - 1,
                Direction::Left => current - 1,
                Direction::Right => (current + 1) % session_indices.len(),
                Direction::Up | Direction::Down => unreachable!(),
            };
            state.selected_row = session_indices[next];
            let max_column = sessions[state.selected_row].cards.len().saturating_sub(1);
            state.selected_column = state.preferred_column.min(max_column);
        }
    }

    keep_compact_selection_visible(state, sessions, terminal_height);
}

fn select_compact_relative(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    count: usize,
    terminal_height: u16,
) -> Option<WindowCard> {
    move_compact_selection_by(state, sessions, direction, count, terminal_height);
    state.selected_card(sessions).cloned()
}

fn move_compact_session_edge(
    state: &mut GridState,
    sessions: &[SessionGroup],
    direction: Direction,
    terminal_height: u16,
) {
    let Some(session) = sessions.get(state.selected_row) else {
        return;
    };
    if session.cards.is_empty() {
        return;
    }

    match direction {
        Direction::Down => {
            let last_column = session.cards.len() - 1;
            if state.selected_column < last_column {
                state.selected_column = last_column;
            } else if let Some((next_row, _)) = sessions
                .iter()
                .enumerate()
                .skip(state.selected_row + 1)
                .find(|(_, session)| !session.cards.is_empty())
            {
                state.selected_row = next_row;
                state.selected_column = 0;
            }
        }
        Direction::Up => {
            if state.selected_column > 0 {
                state.selected_column = 0;
            } else if let Some((previous_row, previous_session)) = sessions
                .iter()
                .enumerate()
                .take(state.selected_row)
                .rev()
                .find(|(_, session)| !session.cards.is_empty())
            {
                state.selected_row = previous_row;
                state.selected_column = previous_session.cards.len() - 1;
            }
        }
        Direction::Left | Direction::Right => return,
    }

    state.preferred_column = state.selected_column;
    keep_compact_selection_visible(state, sessions, terminal_height);
}

pub fn load_cards() -> Result<Vec<WindowCard>> {
    let _ = ensure_status_daemon();
    let windows = parse_windows(&tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        "#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_flags}",
    ])?)?;
    let panes = parse_panes(&tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}\t#{@tmux_agent_switcher_agent}\t#{@tmux_agent_switcher_state}\t#{@tmux_agent_switcher_seen}\t#{@tmux_agent_switcher_run_started_at}\t#{@codex_status_state}\t#{@codex_status_unread}",
    ])?)?;
    Ok(build_cards_with_previews(
        &windows,
        &panes,
        &HashMap::new(),
        &codex_unread_dir(),
    ))
}

pub fn current_window_id() -> Option<String> {
    if let Some(window_id) = env_tmux_value("TMUX_AGENT_SWITCHER_CURRENT") {
        return Some(window_id);
    }

    tmux_output(&["display-message", "-p", "#{window_id}"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn env_tmux_value(name: &str) -> Option<String> {
    std::env::var_os(name)
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty() && !value.contains("#{"))
}

fn preview_capture_args(pane_id: &str) -> Vec<&str> {
    vec!["capture-pane", "-epN", "-t", pane_id]
}

fn capture_preview_text(pane_id: &str) -> Result<Text<'static>> {
    ansi_preview_text(&tmux_output(&preview_capture_args(pane_id))?)
}

fn capture_window_preview_text(window_id: &str, area: Rect) -> Result<Text<'static>> {
    let panes = parse_window_preview_panes(&tmux_output(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_title}\t#{pane_current_command}",
    ])?)?;

    if panes.is_empty() {
        return Err(anyhow!("window {window_id} has no panes"));
    }

    if panes.len() == 1 {
        return capture_preview_text(&panes[0].pane_id);
    }

    let mut captures = HashMap::new();
    for pane in &panes {
        if let Ok(text) = capture_preview_text(&pane.pane_id) {
            captures.insert(pane.pane_id.clone(), text);
        }
    }

    Ok(compose_window_preview_text(&panes, &captures, area))
}

#[derive(Clone)]
struct PreviewCell {
    symbol: char,
    style: Style,
}

impl Default for PreviewCell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: Style::default(),
        }
    }
}

fn compose_window_preview_text(
    panes: &[WindowPreviewPane],
    captures: &HashMap<String, Text<'static>>,
    area: Rect,
) -> Text<'static> {
    if area.width == 0 || area.height == 0 {
        return Text::default();
    }

    let width = area.width as usize;
    let height = area.height as usize;
    let mut cells = vec![vec![PreviewCell::default(); width]; height];
    let pane_rects = geometry_preview_rects(panes, area);
    for (pane, rect) in panes.iter().zip(pane_rects) {
        draw_preview_pane(&mut cells, rect, pane, captures.get(&pane.pane_id));
    }

    preview_cells_to_text(cells)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn geometry_preview_rects(panes: &[WindowPreviewPane], area: Rect) -> Vec<PaneRect> {
    if panes.is_empty() || area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let source_left = panes.iter().map(|pane| pane.left).min().unwrap_or(0);
    let source_top = panes.iter().map(|pane| pane.top).min().unwrap_or(0);
    let source_right = panes
        .iter()
        .map(|pane| pane.left.saturating_add(pane.width))
        .max()
        .unwrap_or(source_left)
        .max(source_left.saturating_add(1));
    let source_bottom = panes
        .iter()
        .map(|pane| pane.top.saturating_add(pane.height))
        .max()
        .unwrap_or(source_top)
        .max(source_top.saturating_add(1));
    let source_width = source_right.saturating_sub(source_left).max(1);
    let source_height = source_bottom.saturating_sub(source_top).max(1);

    panes
        .iter()
        .map(|pane| {
            let left = scale_position(
                pane.left.saturating_sub(source_left),
                source_width,
                area.width,
            );
            let top = scale_position(
                pane.top.saturating_sub(source_top),
                source_height,
                area.height,
            );
            let right = scale_position(
                pane.left
                    .saturating_add(pane.width)
                    .saturating_sub(source_left),
                source_width,
                area.width,
            )
            .max(left.saturating_add(3))
            .min(area.width);
            let bottom = scale_position(
                pane.top
                    .saturating_add(pane.height)
                    .saturating_sub(source_top),
                source_height,
                area.height,
            )
            .max(top.saturating_add(3))
            .min(area.height);

            PaneRect {
                x: left as usize,
                y: top as usize,
                width: right.saturating_sub(left) as usize,
                height: bottom.saturating_sub(top) as usize,
            }
        })
        .collect()
}

fn scale_position(value: u16, source: u16, target: u16) -> u16 {
    if source == 0 || target == 0 {
        return 0;
    }

    ((value as u32 * target as u32) / source as u32) as u16
}

fn draw_preview_pane(
    cells: &mut [Vec<PreviewCell>],
    rect: PaneRect,
    pane: &WindowPreviewPane,
    capture: Option<&Text<'static>>,
) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }

    let border_style = if pane.pane_active {
        Style::default().fg(TMUX_ORANGE)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let x2 = rect.x + rect.width - 1;
    let y2 = rect.y + rect.height - 1;

    put_preview_cell(cells, rect.x, rect.y, '┌', border_style);
    put_preview_cell(cells, x2, rect.y, '┐', border_style);
    put_preview_cell(cells, rect.x, y2, '└', border_style);
    put_preview_cell(cells, x2, y2, '┘', border_style);
    for x in rect.x + 1..x2 {
        put_preview_cell(cells, x, rect.y, '─', border_style);
        put_preview_cell(cells, x, y2, '─', border_style);
    }
    for y in rect.y + 1..y2 {
        put_preview_cell(cells, rect.x, y, '│', border_style);
        put_preview_cell(cells, x2, y, '│', border_style);
    }

    let marker = if pane.pane_active { "*" } else { " " };
    let command = if pane.pane_current_command.is_empty() {
        pane.pane_title.as_str()
    } else {
        pane.pane_current_command.as_str()
    };
    let title = format!("{marker}{} {command}", pane.pane_index);
    for (offset, ch) in title.chars().take(rect.width.saturating_sub(2)).enumerate() {
        put_preview_cell(cells, rect.x + 1 + offset, rect.y, ch, border_style);
    }

    if let Some(capture) = capture {
        let content = preview_text_to_cells(capture);
        let inner_width = rect.width.saturating_sub(2);
        let inner_height = rect.height.saturating_sub(2);
        for (line_index, line) in content.iter().take(inner_height).enumerate() {
            for (column, cell) in line.iter().take(inner_width).enumerate() {
                put_preview_cell(
                    cells,
                    rect.x + 1 + column,
                    rect.y + 1 + line_index,
                    cell.symbol,
                    cell.style,
                );
            }
        }
    }
}

fn put_preview_cell(
    cells: &mut [Vec<PreviewCell>],
    x: usize,
    y: usize,
    symbol: char,
    style: Style,
) {
    if let Some(row) = cells.get_mut(y) {
        if let Some(cell) = row.get_mut(x) {
            cell.symbol = symbol;
            cell.style = style;
        }
    }
}

fn preview_text_to_cells(text: &Text<'static>) -> Vec<Vec<PreviewCell>> {
    text.lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .flat_map(|span| {
                    span.content.chars().map(|symbol| PreviewCell {
                        symbol,
                        style: span.style,
                    })
                })
                .collect()
        })
        .collect()
}

fn preview_cells_to_text(cells: Vec<Vec<PreviewCell>>) -> Text<'static> {
    let lines = cells
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for cell in row {
                if let Some(last) = spans.last_mut() {
                    if last.style == cell.style {
                        last.content.to_mut().push(cell.symbol);
                        continue;
                    }
                }
                spans.push(Span::styled(cell.symbol.to_string(), cell.style));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    Text::from(lines)
}

fn ansi_preview_text(output: &str) -> Result<Text<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = output.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                flush_preview_span(&mut spans, &mut text, style);
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            '\r' => {}
            '\u{1b}' => {
                flush_preview_span(&mut spans, &mut text, style);
                match chars.peek().copied() {
                    Some('[') => {
                        chars.next();
                        let mut sequence = String::new();
                        for next in chars.by_ref() {
                            let done = ('@'..='~').contains(&next);
                            sequence.push(next);
                            if done {
                                break;
                            }
                        }
                        if sequence.ends_with('m') {
                            style = apply_sgr_sequence(style, &sequence[..sequence.len() - 1]);
                        }
                    }
                    Some(']') => {
                        chars.next();
                        let mut previous_was_escape = false;
                        for next in chars.by_ref() {
                            if next == '\u{7}' || (previous_was_escape && next == '\\') {
                                break;
                            }
                            previous_was_escape = next == '\u{1b}';
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            // A literal tab would make the terminal jump to the next tab stop
            // while ratatui counts it as one cell, desyncing every cell after
            // it on the line — smearing content across the modal.
            '\t' => text.push(' '),
            _ if ch.is_control() => {}
            _ => text.push(ch),
        }
    }

    flush_preview_span(&mut spans, &mut text, style);
    lines.push(Line::from(spans));
    Ok(Text::from(lines))
}

fn flush_preview_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn apply_sgr_sequence(mut style: Style, sequence: &str) -> Style {
    let mut codes = sequence
        .split(';')
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .peekable();

    if sequence.is_empty() {
        return Style::default();
    }

    while let Some(code) = codes.next() {
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(basic_ansi_color(code - 30)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(basic_ansi_color(code - 40)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(bright_ansi_color(code - 90)),
            100..=107 => style.bg = Some(bright_ansi_color(code - 100)),
            38 => {
                if let Some(color) = parse_extended_ansi_color(&mut codes) {
                    style.fg = Some(color);
                }
            }
            48 => {
                if let Some(color) = parse_extended_ansi_color(&mut codes) {
                    style.bg = Some(color);
                }
            }
            _ => {}
        }
    }

    style
}

fn parse_extended_ansi_color(
    codes: &mut std::iter::Peekable<impl Iterator<Item = u16>>,
) -> Option<Color> {
    match codes.next()? {
        5 => codes.next().map(|value| Color::Indexed(value as u8)),
        2 => {
            let red = codes.next()? as u8;
            let green = codes.next()? as u8;
            let blue = codes.next()? as u8;
            Some(Color::Rgb(red, green, blue))
        }
        _ => None,
    }
}

fn basic_ansi_color(index: u16) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn bright_ansi_color(index: u16) -> Color {
    match index {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

pub fn normalize_preview_line(line: &str) -> String {
    strip_ansi_and_controls(line).trim().to_owned()
}

fn strip_ansi_and_controls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut previous_was_escape = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_was_escape && next == '\\') {
                            break;
                        }
                        previous_was_escape = next == '\u{1b}';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }

        if ch == '\t' {
            out.push(' ');
            continue;
        }
        if ch.is_control() {
            continue;
        }

        out.push(ch);
    }

    out
}

pub fn select_card(card: &WindowCard) -> Result<()> {
    tmux_status(Command::new("tmux").args(["switch-client", "-t", &card.session_name]))?;
    tmux_status(Command::new("tmux").args(["select-window", "-t", &card.window_id]))?;
    tmux_status(Command::new("tmux").args(["select-pane", "-t", &card.target_pane_id]))?;
    clear_unread_for_pane(&card.target_pane_id);
    mark_window_seen(&card.window_id);
    Ok(())
}

pub fn execute_action(action: SwitcherAction) -> Result<()> {
    match action {
        SwitcherAction::Select(card) => select_card(&card),
        SwitcherAction::NewWindow {
            session_name,
            window_name,
        } => create_window(&session_name, &window_name),
        SwitcherAction::NewSession { session_name } => create_session(&session_name),
    }
}

pub fn create_window(session_name: &str, window_name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "new-window",
            "-P",
            "-F",
            "#{window_id}",
            "-t",
            &format!("{session_name}:"),
            "-n",
            window_name,
        ])
        .output()
        .context("failed to create tmux window")?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux new-window failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let window_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    tmux_status(Command::new("tmux").args(["switch-client", "-t", session_name]))?;
    if !window_id.is_empty() {
        tmux_status(Command::new("tmux").args(["select-window", "-t", &window_id]))?;
    }
    Ok(())
}

pub fn create_session(session_name: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["new-session", "-d", "-s", session_name]))?;
    tmux_status(Command::new("tmux").args(["switch-client", "-t", session_name]))
}

pub fn clear_unread_for_pane(pane_id: &str) {
    let _ = fs::remove_file(codex_unread_file(&codex_unread_dir(), pane_id));
}

pub fn ensure_status_daemon() -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let pid = current_status_daemon_pid();
    if !pid.is_empty() && status_daemon_process_matches(&pid, &current_exe) {
        return Ok(());
    }

    let command = format!(
        "{} status-daemon",
        shell_quote(&current_exe.to_string_lossy())
    );
    tmux_status(Command::new("tmux").args(["run-shell", "-b", &command]))
}

pub fn run_status_daemon() -> Result<()> {
    let pid = std::process::id().to_string();
    tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        "-q",
        STATUS_DAEMON_PID_OPTION,
        &pid,
    ]))?;

    let mut debounce: HashMap<String, Debounce> = HashMap::new();
    let mut ownership_check = 0;
    loop {
        if ownership_check == 0 && current_status_daemon_pid() != pid {
            break;
        }
        ownership_check = (ownership_check + 1) % STATUS_DAEMON_OWNERSHIP_CHECK_POLLS;

        if poll_agent_status_once(&mut debounce).is_err() {
            break;
        }
        thread::sleep(STATUS_DAEMON_INTERVAL);
    }

    let _ = tmux_status(Command::new("tmux").args([
        "set-option",
        "-g",
        "-u",
        "-q",
        STATUS_DAEMON_PID_OPTION,
    ]));
    Ok(())
}

pub fn poll_agent_status_once(debounce: &mut HashMap<String, Debounce>) -> Result<()> {
    let mut panes = parse_panes(&tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}\t#{@tmux_agent_switcher_agent}\t#{@tmux_agent_switcher_state}\t#{@tmux_agent_switcher_seen}\t#{@tmux_agent_switcher_run_started_at}",
    ])?)?;

    let now = unix_timestamp();
    let live: HashSet<String> = panes.iter().map(|pane| pane.pane_id.clone()).collect();
    // Built lazily: only needed when a pane that was an agent no longer reports
    // one as its foreground command, to tell an exit from a foreground subprocess.
    let mut processes: Option<ProcessTree> = None;

    for pane in &mut panes {
        let previous = pane.agent_status;
        let agent = match detect_agent_from_process_name(&pane.pane_current_command) {
            Some(agent) => Some(agent),
            // The foreground command is no longer an agent. Keep the previous agent
            // only while the agent process is genuinely still alive under this pane
            // (e.g. it spawned a foreground child); otherwise treat it as exited so
            // the pane doesn't latch to a stale "claude idle". If the process table
            // can't be read this poll, keep the previous agent rather than clearing
            // it — a transient `ps` failure shouldn't drop a live agent to unknown.
            None => previous.agent.filter(|_| {
                let tree = processes.get_or_insert_with(ProcessTree::snapshot);
                tree.is_empty() || tree.has_agent_descendant(pane.pane_pid)
            }),
        };
        let next = if let Some(agent) = agent {
            let raw = detect_agent_state_from_title(agent, &pane.pane_title).unwrap_or_else(|| {
                let evidence = AgentEvidence {
                    screen_tail: capture_pane_tail(&pane.pane_id, STATUS_CAPTURE_LINES),
                    osc_title: pane.pane_title.clone(),
                    osc_progress: String::new(),
                    process_exited: false,
                };
                detect_agent_state(agent, &evidence)
            });
            let pane_debounce = debounce
                .entry(pane.pane_id.clone())
                .or_insert_with(|| Debounce::new(raw));
            debounce_state(previous, agent, raw, pane_debounce, now)
        } else {
            debounce.remove(&pane.pane_id);
            AgentStatus::unknown()
        };
        write_agent_status(&pane.pane_id, previous, next)?;
        pane.agent_status = next;
    }

    write_window_status_icons(&panes)?;
    debounce.retain(|pane_id, _| live.contains(pane_id));
    Ok(())
}

fn current_status_daemon_pid() -> String {
    tmux_output(&["show-option", "-gqv", STATUS_DAEMON_PID_OPTION])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn status_daemon_process_matches(pid: &str, current_exe: &Path) -> bool {
    if !process_exists(pid) {
        return false;
    }

    let output = Command::new("ps")
        .args(["-p", pid, "-o", "command="])
        .output();
    let Ok(output) = output else {
        return true;
    };
    if !output.status.success() {
        return false;
    }

    let command = String::from_utf8_lossy(&output.stdout);
    command.contains(" status-daemon") && command.contains(current_exe.to_string_lossy().as_ref())
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A snapshot of the process table used to decide whether an agent is still
/// running under a pane once its foreground command stops looking like one.
struct ProcessTree {
    children: HashMap<u32, Vec<u32>>,
    agent_pids: HashSet<u32>,
}

impl ProcessTree {
    fn snapshot() -> Self {
        let output = Command::new("ps")
            .args(["-Ao", "pid=,ppid=,comm="])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        Self::parse(&output)
    }

    fn parse(output: &str) -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut agent_pids = HashSet::new();
        for line in output.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
            if detect_agent_from_process_name(fields.next().unwrap_or_default()).is_some() {
                agent_pids.insert(pid);
            }
        }
        Self {
            children,
            agent_pids,
        }
    }

    /// True when the snapshot captured no processes at all, i.e. `ps` failed or
    /// produced nothing — a signal to treat its answers as unavailable.
    fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// True if `root` or any of its descendants is an agent process.
    fn has_agent_descendant(&self, root: Option<u32>) -> bool {
        let Some(root) = root else {
            return false;
        };
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if self.agent_pids.contains(&pid) {
                return true;
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        false
    }
}

/// Per-pane transition debounce carried across polls in memory by the daemon.
/// Tracks the candidate raw state and how many consecutive polls have seen it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Debounce {
    candidate: AgentState,
    count: u32,
}

impl Debounce {
    fn new(state: AgentState) -> Self {
        Self {
            candidate: state,
            count: 0,
        }
    }
}

/// Polls required before a `committed -> raw` transition is committed. Both
/// directions across the idle boundary are debounced so a single noisy sample
/// (a one-frame braille title, a transient menu-shaped line) can neither flash a
/// premature "done"/timer reset nor re-arm a settled "done". Other transitions
/// (fresh detection from Unknown, Working<->Blocked) commit promptly.
fn debounce_threshold(committed: AgentState, raw: AgentState) -> u32 {
    use AgentState::{Blocked, Idle, Working};
    match (committed, raw) {
        (Working | Blocked, Idle) => IDLE_DEBOUNCE_POLLS,
        (Idle, Working | Blocked) => BUSY_DEBOUNCE_POLLS,
        _ => 1,
    }
}

/// Debounces a raw state sample against the pane's committed state on top of
/// [`stabilize_agent_status_at`]. A differing sample must persist for
/// [`debounce_threshold`] consecutive polls before it is committed; until then
/// the previously committed status (including its run timer and seen flag) is
/// held unchanged.
fn debounce_state(
    previous: AgentStatus,
    agent: AgentKind,
    raw: AgentState,
    debounce: &mut Debounce,
    now: u64,
) -> AgentStatus {
    if raw == previous.state {
        *debounce = Debounce::new(raw);
        return stabilize_agent_status_at(previous, agent, raw, now);
    }

    if debounce.candidate == raw {
        debounce.count += 1;
    } else {
        debounce.candidate = raw;
        debounce.count = 1;
    }

    if debounce.count >= debounce_threshold(previous.state, raw) {
        debounce.count = 0;
        stabilize_agent_status_at(previous, agent, raw, now)
    } else {
        AgentStatus {
            agent: Some(agent),
            ..previous
        }
    }
}

fn stabilize_agent_status_at(
    previous: AgentStatus,
    agent: AgentKind,
    state: AgentState,
    now: u64,
) -> AgentStatus {
    let seen = match state {
        AgentState::Idle
            if previous.state == AgentState::Working || previous.state == AgentState::Blocked =>
        {
            false
        }
        AgentState::Idle => previous.seen,
        AgentState::Working | AgentState::Blocked | AgentState::Unknown => true,
    };
    let run_started_at = match state {
        AgentState::Working | AgentState::Blocked => previous.run_started_at.or(Some(now)),
        AgentState::Idle | AgentState::Unknown => None,
    };

    AgentStatus {
        agent: Some(agent),
        state,
        seen,
        run_started_at,
    }
}

fn capture_pane_tail(pane_id: &str, lines: usize) -> String {
    // No `-e`: state detection matches plain text and glyphs (e.g. the `❯`
    // selection cursor), which ANSI escape sequences would otherwise split.
    tmux_output(&[
        "capture-pane",
        "-pJ",
        "-t",
        pane_id,
        "-S",
        &format!("-{lines}"),
    ])
    .unwrap_or_default()
}

fn write_agent_status(pane_id: &str, previous: AgentStatus, status: AgentStatus) -> Result<()> {
    let updates = status_option_updates(previous, status);
    for (option, value) in &updates {
        set_pane_option(pane_id, option, value)?;
    }
    if updates.is_empty() {
        return Ok(());
    }
    set_pane_option(
        pane_id,
        STATUS_UPDATED_OPTION,
        &unix_timestamp().to_string(),
    )
}

fn status_option_updates(
    previous: AgentStatus,
    status: AgentStatus,
) -> Vec<(&'static str, String)> {
    let mut updates = Vec::new();
    if previous.agent != status.agent {
        updates.push((
            STATUS_AGENT_OPTION,
            format_agent_kind(status.agent).to_owned(),
        ));
    }
    if previous.state != status.state {
        updates.push((
            STATUS_STATE_OPTION,
            format_agent_state(status.state).to_owned(),
        ));
    }
    if previous.seen != status.seen {
        updates.push((
            STATUS_SEEN_OPTION,
            if status.seen { "1" } else { "0" }.to_owned(),
        ));
    }
    if previous.run_started_at != status.run_started_at {
        updates.push((
            STATUS_RUN_STARTED_OPTION,
            status
                .run_started_at
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ));
    }
    updates
}

fn set_pane_option(pane_id: &str, option: &str, value: &str) -> Result<()> {
    tmux_status(Command::new("tmux").args(["set-option", "-p", "-q", "-t", pane_id, option, value]))
}

fn window_status_icons(panes: &[TmuxPane]) -> HashMap<String, &'static str> {
    let mut statuses: HashMap<&str, Vec<AgentStatus>> = HashMap::new();
    for pane in panes {
        statuses
            .entry(&pane.window_id)
            .or_default()
            .push(pane.agent_status);
    }

    statuses
        .into_iter()
        .map(|(window_id, statuses)| {
            let status = rollup_agent_status(statuses.into_iter());
            (window_id.to_owned(), tmux_window_status_icon(status))
        })
        .collect()
}

fn tmux_window_status_icon(status: AgentStatus) -> &'static str {
    if status.agent.is_none() {
        return "";
    }

    match status.state {
        AgentState::Blocked => " #[fg=red,bold]◉#[default]",
        AgentState::Working => " #[fg=yellow,bold]⠋#[default]",
        AgentState::Idle if !status.seen => " #[fg=cyan,bold]●#[default]",
        AgentState::Idle => " #[fg=green]✓#[default]",
        AgentState::Unknown => " #[fg=colour8]○#[default]",
    }
}

fn write_window_status_icons(panes: &[TmuxPane]) -> Result<()> {
    let desired = window_status_icons(panes);
    let current = tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        "#{window_id}\t#{@tmux_agent_switcher_window_icon}",
    ])?;

    for line in current.lines() {
        let Some((window_id, current_icon)) = line.split_once('\t') else {
            continue;
        };
        let desired_icon = desired.get(window_id).copied().unwrap_or_default();
        if desired_icon == current_icon {
            continue;
        }

        if desired_icon.is_empty() {
            tmux_status(Command::new("tmux").args([
                "set-option",
                "-w",
                "-u",
                "-q",
                "-t",
                window_id,
                STATUS_WINDOW_ICON_OPTION,
            ]))?;
        } else {
            tmux_status(Command::new("tmux").args([
                "set-option",
                "-w",
                "-q",
                "-t",
                window_id,
                STATUS_WINDOW_ICON_OPTION,
                desired_icon,
            ]))?;
        }
    }
    Ok(())
}

fn mark_window_seen(window_id: &str) {
    let output =
        tmux_output(&["list-panes", "-t", window_id, "-F", "#{pane_id}"]).unwrap_or_default();
    for pane_id in output.lines().filter(|line| !line.trim().is_empty()) {
        let _ = set_pane_option(pane_id, STATUS_SEEN_OPTION, "1");
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn tmux_output(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn tmux_status(command: &mut Command) -> Result<()> {
    let status = command.status().context("failed to run tmux command")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("tmux command exited with {status}"))
    }
}

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
        .unwrap_or(InputMode::Search)
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

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cards: Vec<WindowCard>,
    current_window_id: Option<&str>,
) -> Result<Option<SwitcherAction>> {
    let mut sessions = group_cards_by_session(cards);
    let mut query = String::new();
    let mut filtered = filter_sessions(&sessions, &query);
    let mut view = initial_view_mode();
    let mut state = initial_grid_state(
        &filtered,
        current_window_id,
        compact_navigation_height(
            terminal.size()?,
            false,
            view,
            compact_lines(&filtered).len(),
        ),
    );
    if let Some(direction) = initial_move_direction() {
        let navigation_height = compact_navigation_height(
            terminal.size()?,
            false,
            view,
            compact_lines(&filtered).len(),
        );
        move_compact_selection(&mut state, &filtered, direction, navigation_height);
    }
    let mut input = initial_input_mode();
    let mut movement_count: Option<usize> = None;
    let mut show_help = false;
    let mut prompt: Option<PromptState> = None;
    let spinner_started_at = Instant::now();
    let mut last_card_refresh = Instant::now();
    let mut last_full_redraw = Instant::now();
    let mut preview = PreviewMirror::default();

    loop {
        let now = Instant::now();
        if now.duration_since(last_card_refresh) >= CARD_REFRESH_INTERVAL {
            if let Ok(cards) = load_cards() {
                let navigation_height = compact_navigation_height(
                    terminal.size()?,
                    show_help,
                    view,
                    compact_lines(&filtered).len(),
                );
                refresh_sessions_from_cards(
                    &mut sessions,
                    &mut filtered,
                    &mut state,
                    cards,
                    &query,
                    navigation_height,
                );
            }
            last_card_refresh = now;
        }
        let preview_area = switcher_layout(
            terminal.size()?,
            show_help,
            view,
            compact_lines(&filtered).len(),
        )
        .preview;
        preview.refresh_for(state.selected_card(&filtered), preview_area, now);
        if now.duration_since(last_full_redraw) >= FULL_REDRAW_INTERVAL {
            terminal.clear()?;
            last_full_redraw = now;
        }
        let spinner_frame = spinner_started_at.elapsed().as_millis() as usize / 120;
        terminal.draw(|frame| {
            draw(
                frame,
                &filtered,
                &state,
                view,
                input,
                show_help,
                &query,
                prompt.as_ref(),
                &preview,
                spinner_frame,
            )
        })?;

        if !event::poll(TUI_TICK_INTERVAL)? {
            continue;
        }

        let key = match event::read()? {
            Event::Mouse(mouse) if prompt.is_none() => {
                let navigation_height = compact_navigation_height(
                    terminal.size()?,
                    show_help,
                    view,
                    compact_lines(&filtered).len(),
                );
                match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Down,
                            navigation_height,
                        );
                    }
                    MouseEventKind::ScrollUp => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Up,
                            navigation_height,
                        );
                    }
                    _ => {}
                }
                continue;
            }
            Event::Resize(_, _) => {
                terminal.clear()?;
                continue;
            }
            Event::Key(key) => key,
            _ => continue,
        };

        {
            if let Some(active_prompt) = prompt.as_mut() {
                if let Some(result) = handle_prompt_key(active_prompt, key) {
                    match result {
                        Some(action) => return Ok(Some(action)),
                        None => prompt = None,
                    }
                }
                continue;
            }

            let navigation_height = compact_navigation_height(
                terminal.size()?,
                show_help,
                view,
                compact_lines(&filtered).len(),
            );
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

            // A Vim-style count survives until j/k consumes it and opens the target.
            let keys_count_key = input == InputMode::Keys
                && !ctrl
                && matches!(key.code, KeyCode::Char(ch) if ch.is_ascii_digit());
            let keys_count_motion = input == InputMode::Keys
                && !ctrl
                && matches!(key.code, KeyCode::Char('j' | 'k' | 'J' | 'K'));
            if !keys_count_key && !keys_count_motion {
                movement_count = None;
            }

            match key.code {
                KeyCode::Esc => {
                    // Telescope-style: first Esc clears the filter, a second
                    // one closes the switcher. In Keys mode Esc just closes.
                    if input == InputMode::Keys || query.is_empty() {
                        return Ok(None);
                    }
                    query.clear();
                    apply_query(
                        &mut filtered,
                        &mut state,
                        &sessions,
                        &query,
                        navigation_height,
                    );
                }
                KeyCode::Enter => {
                    if let Some(action) = select_key_action(key, &state, &filtered) {
                        return Ok(Some(action));
                    }
                }
                KeyCode::Tab => {
                    input = input.toggled();
                    persist_input_mode(input);
                }
                KeyCode::BackTab => {
                    view = view.toggled();
                    persist_view_mode(view);
                    let navigation_height = compact_navigation_height(
                        terminal.size()?,
                        show_help,
                        view,
                        compact_lines(&filtered).len(),
                    );
                    keep_compact_selection_visible(&mut state, &filtered, navigation_height);
                }
                KeyCode::Backspace => {
                    if query.pop().is_some() {
                        apply_query(
                            &mut filtered,
                            &mut state,
                            &sessions,
                            &query,
                            navigation_height,
                        );
                    }
                }
                KeyCode::Down => {
                    move_compact_selection(
                        &mut state,
                        &filtered,
                        Direction::Down,
                        navigation_height,
                    );
                }
                KeyCode::Up => {
                    move_compact_selection(&mut state, &filtered, Direction::Up, navigation_height);
                }
                KeyCode::Left => {
                    move_compact_selection(
                        &mut state,
                        &filtered,
                        Direction::Left,
                        navigation_height,
                    );
                }
                KeyCode::Right => {
                    move_compact_selection(
                        &mut state,
                        &filtered,
                        Direction::Right,
                        navigation_height,
                    );
                }
                KeyCode::Char(ch) if ctrl => match ch {
                    'c' => return Ok(None),
                    'j' => {
                        if let Some(card) = select_compact_relative(
                            &mut state,
                            &filtered,
                            Direction::Down,
                            1,
                            navigation_height,
                        ) {
                            return Ok(Some(SwitcherAction::Select(card)));
                        }
                    }
                    'k' => {
                        if let Some(card) = select_compact_relative(
                            &mut state,
                            &filtered,
                            Direction::Up,
                            1,
                            navigation_height,
                        ) {
                            return Ok(Some(SwitcherAction::Select(card)));
                        }
                    }
                    'n' => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Down,
                            navigation_height,
                        );
                    }
                    'p' => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Up,
                            navigation_height,
                        );
                    }
                    'h' => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Left,
                            navigation_height,
                        );
                    }
                    'l' => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Right,
                            navigation_height,
                        );
                    }
                    'u' => {
                        query.clear();
                        apply_query(
                            &mut filtered,
                            &mut state,
                            &sessions,
                            &query,
                            navigation_height,
                        );
                    }
                    'w' => {
                        delete_query_word(&mut query);
                        apply_query(
                            &mut filtered,
                            &mut state,
                            &sessions,
                            &query,
                            navigation_height,
                        );
                    }
                    't' => {
                        if let Some(card) = state.selected_card(&filtered) {
                            show_help = false;
                            prompt = Some(PromptState::new(PromptKind::NewWindow {
                                session_name: card.session_name.clone(),
                            }));
                        }
                    }
                    's' => {
                        show_help = false;
                        prompt = Some(PromptState::new(PromptKind::NewSession));
                    }
                    _ => {}
                },
                // Keys mode: Vim-style single-key bindings with optional counts.
                KeyCode::Char(ch) if input == InputMode::Keys => match ch {
                    'q' => return Ok(None),
                    ' ' => {
                        if let Some(card) = state.selected_card(&filtered) {
                            return Ok(Some(SwitcherAction::Select(card.clone())));
                        }
                    }
                    '?' => {
                        show_help = !show_help;
                        let navigation_height = compact_navigation_height(
                            terminal.size()?,
                            show_help,
                            view,
                            compact_lines(&filtered).len(),
                        );
                        keep_compact_selection_visible(&mut state, &filtered, navigation_height);
                    }
                    'n' => {
                        if let Some(card) = state.selected_card(&filtered) {
                            show_help = false;
                            prompt = Some(PromptState::new(PromptKind::NewWindow {
                                session_name: card.session_name.clone(),
                            }));
                        }
                    }
                    'N' => {
                        show_help = false;
                        prompt = Some(PromptState::new(PromptKind::NewSession));
                    }
                    'h' => {
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Left,
                            navigation_height,
                        );
                    }
                    'H' => {
                        move_compact_session_edge(
                            &mut state,
                            &filtered,
                            Direction::Up,
                            navigation_height,
                        );
                    }
                    'j' | 'k' | 'J' | 'K' => {
                        if let Some((direction, count)) =
                            take_counted_open_motion(&mut movement_count, ch)
                        {
                            if let Some(card) = select_compact_relative(
                                &mut state,
                                &filtered,
                                direction,
                                count,
                                navigation_height,
                            ) {
                                return Ok(Some(SwitcherAction::Select(card)));
                            }
                        } else if ch == 'j' || ch == 'k' {
                            move_compact_selection(
                                &mut state,
                                &filtered,
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
                        move_compact_selection(
                            &mut state,
                            &filtered,
                            Direction::Right,
                            navigation_height,
                        );
                    }
                    'L' => {
                        move_compact_session_edge(
                            &mut state,
                            &filtered,
                            Direction::Down,
                            navigation_height,
                        );
                    }
                    _ if ch.is_ascii_digit() => {
                        push_movement_count(&mut movement_count, ch);
                    }
                    _ => {}
                },
                KeyCode::Char('?') if query.is_empty() => {
                    show_help = !show_help;
                    let navigation_height = compact_navigation_height(
                        terminal.size()?,
                        show_help,
                        view,
                        compact_lines(&filtered).len(),
                    );
                    keep_compact_selection_visible(&mut state, &filtered, navigation_height);
                }
                KeyCode::Char(ch) => {
                    query.push(ch);
                    apply_query(
                        &mut filtered,
                        &mut state,
                        &sessions,
                        &query,
                        navigation_height,
                    );
                }
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    sessions: &[SessionGroup],
    state: &GridState,
    view: ViewMode,
    input: InputMode,
    show_help: bool,
    query: &str,
    prompt: Option<&PromptState>,
    preview: &PreviewMirror,
    spinner_frame: usize,
) {
    let layout = switcher_layout(frame.size(), show_help, view, compact_lines(sessions).len());

    render_selected_preview(frame, layout.preview, preview);
    frame.render_widget(Clear, layout.list_overlay);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::DarkGray)),
        layout.list_overlay,
    );
    render_modal_top_bar(frame, layout.list_overlay);
    render_search_bar(frame, layout.search, query, input);
    if sessions.is_empty() && !query.trim().is_empty() {
        render_no_matches(frame, layout.sessions);
    } else {
        render_compact(frame, layout.sessions, sessions, state, spinner_frame);
    }
    if let Some(help) = layout.help {
        render_help(frame, help);
    }
    if let Some(prompt) = prompt {
        render_prompt(frame, frame.size(), layout.list_overlay, prompt);
    }
}

/// The telescope-style prompt line on the bottom row of the list box, with a
/// separator rule above it: `❯ query▏`, or a dim hint while the query is
/// empty. In Keys mode the prompt dims and hints at Tab instead.
fn render_search_bar(frame: &mut Frame, area: Rect, query: &str, input: InputMode) {
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
    let query_width = query.chars().count();
    let visible: String = if query_width > budget {
        let tail: String = query
            .chars()
            .skip(query_width + 1 - budget.max(1))
            .collect();
        format!("…{tail}")
    } else {
        query.to_owned()
    };

    let prompt_style = match input {
        InputMode::Search => Style::default()
            .fg(TMUX_ORANGE)
            .add_modifier(Modifier::BOLD),
        InputMode::Keys => Style::default().fg(Color::DarkGray),
    };
    let mut spans = vec![Span::styled("❯ ", prompt_style)];
    if !visible.is_empty() {
        let query_style = match input {
            InputMode::Search => Style::default().fg(Color::White),
            InputMode::Keys => Style::default().fg(Color::Gray),
        };
        spans.push(Span::styled(visible.clone(), query_style));
    }
    if input == InputMode::Search {
        spans.push(Span::styled(" ", Style::default().bg(Color::Gray)));
    }
    let hint = match (input, visible.is_empty()) {
        (InputMode::Search, true) => Some(SEARCH_PLACEHOLDER),
        (InputMode::Keys, true) => Some(KEYS_PLACEHOLDER),
        (InputMode::Keys, false) => Some("  tab: search"),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SwitcherLayout {
    list_overlay: Rect,
    search: Rect,
    sessions: Rect,
    help: Option<Rect>,
    preview: Rect,
}

fn switcher_layout(
    area: Rect,
    show_help: bool,
    view: ViewMode,
    line_count: usize,
) -> SwitcherLayout {
    let (list_overlay, preview) = match view {
        ViewMode::Sidebar => {
            let sidebar_width = sidebar_width(area.width);
            let list_overlay = Rect {
                x: area.x,
                y: area.y,
                width: sidebar_width,
                height: area.height,
            };
            let preview = Rect {
                x: area.x.saturating_add(sidebar_width),
                y: area.y,
                width: area.width.saturating_sub(sidebar_width),
                height: area.height,
            };
            (list_overlay, preview)
        }
        ViewMode::SidebarRight => {
            let sidebar_width = sidebar_width(area.width);
            let preview_width = area.width.saturating_sub(sidebar_width);
            let list_overlay = Rect {
                x: area.x.saturating_add(preview_width),
                y: area.y,
                width: sidebar_width,
                height: area.height,
            };
            let preview = Rect {
                x: area.x,
                y: area.y,
                width: preview_width,
                height: area.height,
            };
            (list_overlay, preview)
        }
        ViewMode::Palette => (palette_overlay(area, show_help, line_count), area),
    };
    // Bottom-up inside the box: search prompt at the very bottom (fzf-style),
    // help above it, the session list on top.
    let list = inset_rect(list_overlay, FLOATING_LIST_INSET);
    let search_height = list.height.min(SEARCH_BAR_ROWS);
    let body_height = list.height.saturating_sub(search_height);
    let help_height = if show_help {
        HELP_LINE_COUNT.min(body_height)
    } else {
        0
    };
    let sessions = Rect {
        x: list.x,
        y: list.y,
        width: list.width,
        height: body_height.saturating_sub(help_height),
    };
    let help = (help_height > 0).then_some(Rect {
        x: list.x,
        y: sessions.y.saturating_add(sessions.height),
        width: list.width,
        height: help_height,
    });
    let search = Rect {
        x: list.x,
        y: list.y.saturating_add(body_height),
        width: list.width,
        height: search_height,
    };

    SwitcherLayout {
        list_overlay,
        search,
        sessions,
        help,
        preview,
    }
}

fn sidebar_width(total_width: u16) -> u16 {
    if total_width == 0 {
        return 0;
    }

    percentage_length(total_width, SIDEBAR_WIDTH_PERCENT).clamp(
        SIDEBAR_MIN_WIDTH.min(total_width),
        SIDEBAR_MAX_WIDTH.min(total_width),
    )
}

/// The floating list box of the palette view: centered horizontally, sized to
/// its content up to a height cap (past which the list scrolls). The box's
/// bottom edge — where the search prompt lives — is anchored around the upper
/// middle of the screen, so the prompt stays put while the list above it grows
/// and shrinks with the filter.
fn palette_overlay(area: Rect, show_help: bool, line_count: usize) -> Rect {
    let width = percentage_length(area.width, PALETTE_WIDTH_PERCENT).clamp(
        PALETTE_MIN_WIDTH.min(area.width),
        PALETTE_MAX_WIDTH.min(area.width),
    );
    let anchor_bottom = percentage_length(area.height, PALETTE_BOTTOM_PERCENT);
    let inset = FLOATING_LIST_INSET.saturating_mul(2);
    let help_height = if show_help { HELP_LINE_COUNT } else { 0 };
    // At least one body row so the "no matching windows" hint has a home.
    let height = (line_count.max(1).min(u16::MAX as usize) as u16)
        .saturating_add(SEARCH_BAR_ROWS)
        .saturating_add(help_height)
        .saturating_add(inset)
        .max(inset.saturating_add(1))
        .min(anchor_bottom.saturating_sub(PALETTE_TOP_MARGIN.min(anchor_bottom)))
        .min(area.height);

    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area.y.saturating_add(anchor_bottom.saturating_sub(height)),
        width,
        height,
    }
}

fn compact_navigation_height(
    terminal_size: Rect,
    show_help: bool,
    view: ViewMode,
    line_count: usize,
) -> u16 {
    switcher_layout(terminal_size, show_help, view, line_count)
        .sessions
        .height
        .saturating_add(1)
}

fn refresh_sessions_from_cards(
    sessions: &mut Vec<SessionGroup>,
    filtered: &mut Vec<SessionGroup>,
    state: &mut GridState,
    cards: Vec<WindowCard>,
    query: &str,
    terminal_height: u16,
) {
    if cards.is_empty() {
        return;
    }

    let selected_window_id = state
        .selected_card(filtered)
        .map(|card| card.window_id.clone());
    let fallback_row = state.selected_row;
    let fallback_column = state.selected_column;
    let fallback_offset = state.row_offset;
    let next_sessions = group_cards_by_session(cards);
    if next_sessions.is_empty() {
        return;
    }
    let next_filtered = filter_sessions(&next_sessions, query);

    let mut next_state = if let Some(window_id) = selected_window_id.as_deref() {
        if next_filtered
            .iter()
            .flat_map(|session| session.cards.iter())
            .any(|card| card.window_id == window_id)
        {
            GridState::for_window_id(&next_filtered, window_id)
        } else {
            fallback_grid_state(&next_filtered, fallback_row, fallback_column)
        }
    } else {
        fallback_grid_state(&next_filtered, fallback_row, fallback_column)
    };
    next_state.row_offset = fallback_offset;

    *sessions = next_sessions;
    *filtered = next_filtered;
    *state = next_state;
    keep_compact_selection_visible(state, filtered, terminal_height);
}

fn fallback_grid_state(sessions: &[SessionGroup], row: usize, column: usize) -> GridState {
    let mut state = GridState::new();
    state.selected_row = row.min(sessions.len().saturating_sub(1));
    let max_column = sessions
        .get(state.selected_row)
        .map(|session| session.cards.len().saturating_sub(1))
        .unwrap_or(0);
    state.selected_column = column.min(max_column);
    state.preferred_column = state.selected_column;
    state
}

fn percentage_length(total: u16, percent: u16) -> u16 {
    if total == 0 {
        return 0;
    }

    ((total as u32 * percent as u32) / 100)
        .clamp(1, total as u32)
        .try_into()
        .unwrap_or(total)
}

fn inset_rect(area: Rect, inset: u16) -> Rect {
    let doubled = inset.saturating_mul(2);
    Rect {
        x: area.x.saturating_add(inset),
        y: area.y.saturating_add(inset),
        width: area.width.saturating_sub(doubled),
        height: area.height.saturating_sub(doubled),
    }
}

fn render_selected_preview(frame: &mut Frame, area: Rect, preview: &PreviewMirror) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Paragraph::new(preview.text.clone()), area);
}

fn render_compact(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionGroup],
    state: &GridState,
    spinner_frame: usize,
) {
    let lines = compact_lines(sessions);
    let card_positions = compact_card_positions(sessions);
    let selected_position = card_positions
        .iter()
        .position(|&(row, column)| row == state.selected_row && column == state.selected_column)
        .unwrap_or(0);
    let number_width = card_positions.len().saturating_sub(1).to_string().len();
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
                render_compact_session(frame, area, row_y, session);
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
                render_compact_session(frame, row_area, row_area.y, session);
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
                let selected =
                    session_index == state.selected_row && card_index == state.selected_column;
                let card_position = card_positions
                    .iter()
                    .position(|&(row, column)| row == session_index && column == card_index)
                    .unwrap_or(0);
                let relative_number = card_position.abs_diff(selected_position);
                frame.render_widget(
                    Paragraph::new(compact_tab_line(
                        card,
                        selected,
                        relative_number,
                        number_width,
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

fn render_compact_session(frame: &mut Frame, area: Rect, y: u16, session: &SessionGroup) {
    frame.render_widget(
        Paragraph::new(compact_session_label(session)).style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
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

#[cfg(test)]
fn compact_session_label_width<'a>(
    sessions: impl Iterator<Item = (usize, &'a SessionGroup)>,
) -> u16 {
    sessions
        .map(|(_, session)| compact_session_label(session))
        .map(|label| text_width(&label))
        .max()
        .unwrap_or(0)
}

fn text_width(text: &str) -> u16 {
    text.chars().count().min(u16::MAX as usize) as u16
}

#[cfg(test)]
fn compact_tab_label(card: &WindowCard, relative_number: usize, width: u16) -> String {
    compact_tab_label_at(card, relative_number, width, 0, unix_timestamp())
}

#[cfg(test)]
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

fn compact_tab_line(
    card: &WindowCard,
    selected: bool,
    relative_number: usize,
    number_width: usize,
    width: u16,
    spinner_frame: usize,
) -> Line<'static> {
    let now = unix_timestamp();
    let label = compact_tab_left_text_at(card, relative_number, number_width, spinner_frame, now);
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

    Line::from(vec![
        Span::raw(format!(" {:>number_width$}", relative_number)),
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
    spinner_frame: usize,
    _now: u64,
) -> String {
    format!(
        " {:>number_width$} {} {}",
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

fn compact_tab_process_text(card: &WindowCard) -> String {
    match detect_agent_from_process_name(&card.command) {
        Some(AgentKind::Codex) => "codex".to_owned(),
        Some(AgentKind::Claude) => "claude".to_owned(),
        None => card.command.clone(),
    }
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

fn render_help(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let lines = [
        "Shortcuts",
        "tab: search / keys",
        "S-tab: palette/sidebar",
        "search: type filters",
        "keys: j/k move, n/N, q",
        "count+j/k: move & open",
        "H/L: previous/next edge",
        "↑/↓: move, C-j/C-k: open",
        "←/→: switch session",
        "enter: open selected",
        "C-t/C-s: new win/sess",
        "C-u: clear filter",
        "esc: clear, then close",
    ];
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
    let line = format!("{}: {}", prompt.title(), prompt.input);

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
    use std::{
        collections::HashMap,
        fs,
        time::{Duration, Instant},
    };

    use crate::{
        agent_status_icon, ansi_preview_text, best_match_position, build_cards, codex_unread_file,
        compact_selected_line_index, compact_session_label, compact_session_label_width,
        compact_tab_label, compact_tab_label_at, compact_tab_line, compact_tab_style,
        compose_window_preview_text, debounce_state, debounce_threshold,
        detect_agent_from_process_name, detect_agent_state, detect_agent_state_from_title, draw,
        env_tmux_value, filter_sessions, format_view_mode, fuzzy_score, group_cards_by_session,
        handle_prompt_key, has_selection_prompt, initial_grid_state,
        keep_compact_selection_visible, move_compact_selection, move_compact_selection_by,
        move_compact_session_edge, normalize_preview_line, parse_panes, parse_view_mode,
        parse_window_preview_panes, parse_windows, preview_capture_args, push_movement_count,
        refresh_sessions_from_cards, render_compact, select_compact_relative, select_key_action,
        stabilize_agent_status_at, status_option_updates, switcher_layout,
        take_counted_open_motion, tmux_window_status_icon, window_status_icons, AgentEvidence,
        AgentKind, AgentState, AgentStatus, Debounce, Direction, GridState, InputMode,
        PreviewMirror, ProcessTree, PromptKind, PromptState, SwitcherAction, ViewMode, WindowCard,
        BUSY_DEBOUNCE_POLLS, HELP_LINE_COUNT, IDLE_DEBOUNCE_POLLS, PREVIEW_REFRESH_INTERVAL,
        STATUS_AGENT_OPTION, STATUS_RUN_STARTED_OPTION, STATUS_SEEN_OPTION, STATUS_STATE_OPTION,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Text;
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    #[test]
    fn parses_tmux_window_rows() {
        let windows = parse_windows("@1\twork\t1\teditor\t*\n@2\twork\t2\tagents\t-\n").unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].window_id, "@1");
        assert_eq!(windows[0].session_name, "work");
        assert_eq!(windows[0].window_index, "1");
        assert_eq!(windows[0].window_name, "editor");
        assert_eq!(windows[0].window_flags, "*");
    }

    #[test]
    fn parses_pane_rows_with_empty_fields() {
        let panes =
            parse_panes("%1\t@1\t1\tnvim\t/Users/example\t\n%2\t@1\t0\tcodex\t/tmp\tagent\n")
                .unwrap();

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[0].window_id, "@1");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].pane_title, "");
        assert_eq!(panes[1].pane_title, "agent");
    }

    #[test]
    fn parses_cached_agent_status_from_pane_rows() {
        let panes = parse_panes("%1\t@1\t1\tcodex\t/tmp\t⠋ working\t123\tcodex\tworking\t1\t1000\t\t\n%2\t@1\t0\tzsh\t/tmp\t\t124\tclaude\tidle\t0\t\t\t\n").unwrap();

        assert_eq!(panes[0].pane_pid, Some(123));
        assert_eq!(
            panes[0].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Working,
                seen: true,
                run_started_at: Some(1000),
            }
        );
        assert_eq!(
            panes[1].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
                seen: false,
                run_started_at: None,
            }
        );
    }

    #[test]
    fn status_option_updates_only_includes_changed_fields() {
        let previous = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };

        assert!(status_option_updates(previous, previous).is_empty());

        let done = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        };
        assert_eq!(
            status_option_updates(previous, done),
            vec![
                (STATUS_STATE_OPTION, "idle".to_owned()),
                (STATUS_SEEN_OPTION, "0".to_owned()),
                (STATUS_RUN_STARTED_OPTION, String::new()),
            ]
        );

        let claude = AgentStatus {
            agent: Some(AgentKind::Claude),
            ..previous
        };
        assert_eq!(
            status_option_updates(previous, claude),
            vec![(STATUS_AGENT_OPTION, "claude".to_owned())]
        );
    }

    #[test]
    fn tmux_tab_icons_match_sidebar_status_meanings() {
        assert_eq!(tmux_window_status_icon(AgentStatus::unknown()), "");
        assert_eq!(
            tmux_window_status_icon(AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Blocked,
                seen: true,
                run_started_at: Some(1000),
            }),
            " #[fg=red,bold]◉#[default]"
        );
        assert_eq!(
            tmux_window_status_icon(AgentStatus::done(Some(AgentKind::Claude))),
            " #[fg=cyan,bold]●#[default]"
        );
    }

    #[test]
    fn tmux_tab_icon_rolls_up_all_panes_in_a_window() {
        let panes = parse_panes(
            "%1\t@1\t1\tcodex\t/tmp\tone\t101\tcodex\tworking\t1\t1000\n\
             %2\t@1\t0\tclaude\t/tmp\ttwo\t102\tclaude\tblocked\t1\t1000\n\
             %3\t@2\t1\tzsh\t/tmp\tthree\t103\t\tunknown\t1\t\n",
        )
        .unwrap();

        let icons = window_status_icons(&panes);
        assert_eq!(icons.get("@1"), Some(&" #[fg=red,bold]◉#[default]"));
        assert_eq!(icons.get("@2"), Some(&""));
    }

    #[test]
    fn codex_hook_status_is_a_fallback_signal() {
        let panes = parse_panes("%1\t@1\t1\tcodex\t/tmp\t\t123\t\t\t\tready\t1\n").unwrap();

        assert_eq!(
            panes[0].agent_status,
            AgentStatus {
                agent: Some(AgentKind::Codex),
                state: AgentState::Idle,
                seen: false,
                run_started_at: None,
            }
        );
    }

    #[test]
    fn daemon_timer_starts_on_working_and_survives_blocked_until_idle() {
        let idle = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };

        let working = stabilize_agent_status_at(idle, AgentKind::Codex, AgentState::Working, 2000);
        assert_eq!(working.run_started_at, Some(2000));

        let blocked =
            stabilize_agent_status_at(working, AgentKind::Codex, AgentState::Blocked, 2030);
        assert_eq!(blocked.run_started_at, Some(2000));

        let done = stabilize_agent_status_at(blocked, AgentKind::Codex, AgentState::Idle, 2040);
        assert_eq!(done.run_started_at, None);
    }

    #[test]
    fn codex_unread_file_strips_tmux_pane_prefix() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            codex_unread_file(dir.path(), "%42"),
            dir.path().join("42.json")
        );
    }

    #[test]
    fn builds_cards_from_windows_and_active_panes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("2.json"), "{}").unwrap();

        let windows = parse_windows("@1\twork\t1\teditor\t*\n@2\twork\t2\tagents\t-\n").unwrap();
        let panes = parse_panes(
            "%1\t@1\t1\tnvim\t/Users/example/project\teditor\n%2\t@2\t1\tcodex\t/tmp\tagent\n",
        )
        .unwrap();
        let cards = build_cards(&windows, &panes, dir.path());

        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].target_pane_id, "%1");
        assert!(!cards[0].codex_unread);
        assert_eq!(cards[1].target_pane_id, "%2");
        assert!(cards[1].codex_unread);
    }

    #[test]
    fn rolls_window_status_up_from_panes() {
        let dir = tempfile::tempdir().unwrap();
        let windows = parse_windows("@1\twork\t1\tagents\t*\n").unwrap();
        let panes = parse_panes(
            "%1\t@1\t1\tzsh\t/tmp\t\t11\tcodex\tworking\t1\n%2\t@1\t0\tzsh\t/tmp\t\t12\tclaude\tblocked\t1\n",
        )
        .unwrap();
        let cards = build_cards(&windows, &panes, dir.path());

        assert_eq!(cards[0].agent_status.state, AgentState::Blocked);
    }

    #[test]
    fn refreshing_sessions_updates_statuses_and_preserves_selected_window() {
        let mut first = test_card("work", "1");
        first.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };
        let mut selected = test_card("work", "2");
        selected.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Idle,
            seen: true,
            run_started_at: None,
        };
        let mut sessions = group_cards_by_session(vec![first.clone(), selected.clone()]);
        let mut filtered = sessions.clone();
        let mut state = GridState::new();
        state.selected_column = 1;
        state.preferred_column = 1;

        selected.agent_status = AgentStatus {
            agent: Some(AgentKind::Codex),
            state: AgentState::Blocked,
            seen: true,
            run_started_at: Some(1000),
        };

        refresh_sessions_from_cards(
            &mut sessions,
            &mut filtered,
            &mut state,
            vec![first, selected],
            "",
            20,
        );

        assert_eq!(state.selected_row, 0);
        assert_eq!(state.selected_column, 1);
        assert_eq!(sessions[0].cards[1].agent_status.state, AgentState::Blocked);
        assert_eq!(filtered, sessions);
    }

    #[test]
    fn refreshing_sessions_keeps_the_active_filter_applied() {
        let mut sessions = group_cards_by_session(vec![test_card("work", "1")]);
        let mut filtered = filter_sessions(&sessions, "ops");
        let mut state = GridState::new();
        assert!(filtered.is_empty());

        let mut ops = test_card("ops", "1");
        ops.window_name = "server".to_owned();
        refresh_sessions_from_cards(
            &mut sessions,
            &mut filtered,
            &mut state,
            vec![test_card("work", "1"), ops],
            "ops",
            20,
        );

        assert_eq!(sessions.len(), 2);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_name, "ops");
    }

    #[test]
    fn detects_codex_and_claude_agent_states() {
        assert_eq!(
            detect_agent_from_process_name("/opt/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            detect_agent_from_process_name("codex-aarch64-a"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            detect_agent_from_process_name("claude-code"),
            Some(AgentKind::Claude)
        );
        // Native-installer / process.title path: tmux sees a bare semver.
        assert_eq!(
            detect_agent_from_process_name("2.1.197"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            detect_agent_from_process_name("/Users/x/.local/share/claude/versions/2.1.197"),
            Some(AgentKind::Claude)
        );
        // Non-semver commands must not be mistaken for Claude.
        assert_eq!(detect_agent_from_process_name("zsh"), None);
        assert_eq!(detect_agent_from_process_name("2.1"), None);
        assert_eq!(detect_agent_from_process_name("node"), None);

        let codex = AgentEvidence {
            screen_tail: "press enter to confirm or esc to cancel".to_owned(),
            osc_title: String::new(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Codex, &codex),
            AgentState::Blocked
        );

        let claude = AgentEvidence {
            screen_tail: "anything".to_owned(),
            osc_title: "⠋ thinking".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &claude),
            AgentState::Working
        );
    }

    #[test]
    fn title_fast_path_detects_unambiguous_agent_states() {
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "[ ! ] Action Required | repo"),
            Some(AgentState::Blocked)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "⠋ working"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Codex, "repo"),
            Some(AgentState::Idle)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "⠋ thinking"),
            Some(AgentState::Working)
        );
        assert_eq!(
            detect_agent_state_from_title(AgentKind::Claude, "✳ review this"),
            None
        );
    }

    #[test]
    fn claude_selection_menu_is_blocked_even_with_idle_title() {
        let evidence = AgentEvidence {
            screen_tail: [
                "│ Would you like to proceed?              │",
                "│ ❯ 1. Yes, and auto-accept edits         │",
                "│   2. Yes, and manually approve edits    │",
                "│   3. No, keep planning                  │",
            ]
            .join("\n"),
            osc_title: "✳ design the thing".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Blocked
        );
    }

    #[test]
    fn claude_idle_input_box_is_not_blocked() {
        let evidence = AgentEvidence {
            screen_tail: [
                "※ recap: did the thing. next: your review.",
                "──────────────── ultracode ─",
                "❯ ",
                "────────────────",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            ]
            .join("\n"),
            osc_title: "✳ clarify the logic".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Idle
        );
    }

    #[test]
    fn claude_stale_prompt_scrolled_off_does_not_block() {
        let mut lines = vec!["Do you want to proceed?".to_owned()];
        for index in 0..30 {
            lines.push(format!("build output line {index}"));
        }
        let evidence = AgentEvidence {
            screen_tail: lines.join("\n"),
            osc_title: "⠙ working".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Working
        );
    }

    #[test]
    fn claude_bordered_menu_without_known_phrase_is_blocked() {
        // A custom AskUserQuestion menu: no wording from any phrase list, non-1
        // numbering, ')' delimiter, drawn inside a border.
        let evidence = AgentEvidence {
            screen_tail: [
                "│ Which database should we use?           │",
                "│ ❯ 2) Postgres                           │",
                "│   3) SQLite                             │",
            ]
            .join("\n"),
            osc_title: "✳ pick a database".to_owned(),
            osc_progress: String::new(),
            process_exited: false,
        };
        assert_eq!(
            detect_agent_state(AgentKind::Claude, &evidence),
            AgentState::Blocked
        );
    }

    #[test]
    fn selection_prompt_requires_cursor_and_a_second_option() {
        // Real bordered menu: cursor on one of several options.
        assert!(has_selection_prompt("│ ❯ 1. Yes │\n│   2. No │"));
        // Unbordered menu also matches.
        assert!(has_selection_prompt("❯ 10) ten\n  11) eleven"));
        // Bare input box, or the user typing a single "1." line into it, is not a menu.
        assert!(!has_selection_prompt("❯ "));
        assert!(!has_selection_prompt("❯ 1. fix the parser and then rebase"));
        // A plain numbered list in output (no cursor) is not a menu.
        assert!(!has_selection_prompt("1. first\n2. second"));
    }

    #[test]
    fn debounce_holds_busy_until_idle_streak_then_completes() {
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
        };
        let mut debounce = Debounce::new(AgentState::Working);

        // Early idle samples hold Working and keep the run timer intact.
        for poll in 1..IDLE_DEBOUNCE_POLLS {
            let held = debounce_state(
                working,
                AgentKind::Claude,
                AgentState::Idle,
                &mut debounce,
                1000 + poll as u64,
            );
            assert_eq!(held.state, AgentState::Working);
            assert_eq!(held.run_started_at, Some(1000));
            assert!(held.seen);
        }

        // The threshold sample commits Idle and flags it unseen ("done").
        let done = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            2000,
        );
        assert_eq!(done.state, AgentState::Idle);
        assert!(!done.seen);
        assert_eq!(done.run_started_at, None);
    }

    #[test]
    fn debounce_ignores_a_lone_busy_sample_after_done() {
        // A committed "done" (unread) pane.
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        };
        let mut debounce = Debounce::new(AgentState::Idle);

        // A lone stray Working sample must not wipe the done or start a timer.
        let held = debounce_state(
            done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3000,
        );
        assert_eq!(held.state, AgentState::Idle);
        assert!(!held.seen);
        assert_eq!(held.run_started_at, None);

        // Sustained work reaches the busy threshold and commits a fresh run.
        assert_eq!(BUSY_DEBOUNCE_POLLS, 2);
        let working = debounce_state(
            done,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            3005,
        );
        assert_eq!(working.state, AgentState::Working);
        assert!(working.seen);
        assert_eq!(working.run_started_at, Some(3005));
    }

    #[test]
    fn debounce_absorbs_single_idle_blip_without_false_done() {
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(500),
        };
        let mut debounce = Debounce::new(AgentState::Working);

        // One idle blip is held as Working with the timer intact.
        let held = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            510,
        );
        assert_eq!(held.state, AgentState::Working);
        assert_eq!(held.run_started_at, Some(500));
        assert!(held.seen);

        // Work resumes before the streak completes: no "done" is ever committed.
        let resumed = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Working,
            &mut debounce,
            520,
        );
        assert_eq!(resumed.state, AgentState::Working);
        assert!(resumed.seen);
        assert_eq!(resumed.run_started_at, Some(500));
    }

    #[test]
    fn debounce_commits_promptly_for_fresh_and_cross_busy_transitions() {
        // Fresh detection out of Unknown shows immediately (no settle delay).
        let mut debounce = Debounce::new(AgentState::Unknown);
        let first = debounce_state(
            AgentStatus::unknown(),
            AgentKind::Claude,
            AgentState::Idle,
            &mut debounce,
            10,
        );
        assert_eq!(first.state, AgentState::Idle);

        // Blocked appearing while Working is not delayed, and the timer carries over.
        let working = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(100),
        };
        let mut debounce = Debounce::new(AgentState::Working);
        let blocked = debounce_state(
            working,
            AgentKind::Claude,
            AgentState::Blocked,
            &mut debounce,
            150,
        );
        assert_eq!(blocked.state, AgentState::Blocked);
        assert_eq!(blocked.run_started_at, Some(100));
    }

    #[test]
    fn debounce_threshold_is_directional() {
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Idle),
            IDLE_DEBOUNCE_POLLS
        );
        assert_eq!(
            debounce_threshold(AgentState::Idle, AgentState::Working),
            BUSY_DEBOUNCE_POLLS
        );
        // Fresh detection and busy<->busy commit on the first sample.
        assert_eq!(debounce_threshold(AgentState::Unknown, AgentState::Idle), 1);
        assert_eq!(
            debounce_threshold(AgentState::Working, AgentState::Blocked),
            1
        );
    }

    #[test]
    fn process_tree_distinguishes_live_agent_from_exit() {
        // pane shell 100 -> claude 200 -> its bash subprocess 300.
        let running = ProcessTree::parse("100 1 zsh\n200 100 claude\n300 200 bash\n");
        assert!(running.has_agent_descendant(Some(100)));
        assert!(running.has_agent_descendant(Some(200)));

        // Same pane shell once Claude has exited: no agent left underneath.
        let exited = ProcessTree::parse("100 1 zsh\n400 100 nvim\n");
        assert!(!exited.has_agent_descendant(Some(100)));
        assert!(!exited.has_agent_descendant(None));

        // The versioned native binary is recognized by its comm path too.
        let versioned =
            ProcessTree::parse("10 1 -zsh\n11 10 /Users/x/.local/share/claude/versions/2.1.197\n");
        assert!(versioned.has_agent_descendant(Some(10)));
    }

    #[test]
    fn empty_process_tree_signals_ps_unavailable() {
        // A failed/empty `ps` yields an empty tree, which the poll treats as
        // "unknown" (keep the previous agent) rather than "no agent".
        assert!(ProcessTree::parse("").is_empty());
        assert!(!ProcessTree::parse("1 0 launchd\n").is_empty());
    }

    #[test]
    fn preview_lines_are_trimmed_before_rendering() {
        assert_eq!(
            normalize_preview_line("   GET /api/search 200    "),
            "GET /api/search 200"
        );
        assert_eq!(
            normalize_preview_line("\tworker finished\t"),
            "worker finished"
        );
    }

    #[test]
    fn preview_lines_strip_ansi_and_control_sequences() {
        assert_eq!(
            normalize_preview_line("\u{1b}[32mINFO\u{1b}[0m service ready\u{7}"),
            "INFO service ready"
        );
        assert_eq!(
            normalize_preview_line("before\u{1b}]0;title\u{7}after"),
            "beforeafter"
        );
    }

    #[test]
    fn preview_tabs_become_spaces_so_cells_stay_aligned() {
        // A raw tab would move the real cursor to the next tab stop while
        // ratatui budgets one cell, desyncing everything after it.
        assert_eq!(normalize_preview_line("col1\tcol2"), "col1 col2");

        let text = ansi_preview_text("a\tb").unwrap();
        assert_eq!(preview_text_lines(&text)[0], "a b");
    }

    #[test]
    fn preview_capture_args_preserve_visible_ansi_and_spacing() {
        assert_eq!(
            preview_capture_args("%42"),
            vec!["capture-pane", "-epN", "-t", "%42"]
        );
    }

    #[test]
    fn parses_window_preview_panes_with_geometry() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t40\t10\teditor\tnvim\n%2\t2\t0\t40\t0\t40\t10\tagent\tcodex\n",
        )
        .unwrap();

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].left, 0);
        assert_eq!(panes[1].left, 40);
        assert_eq!(panes[1].width, 40);
        assert_eq!(panes[1].height, 10);
    }

    #[test]
    fn composes_side_by_side_panes_into_window_preview() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t10\t4\teditor\tnvim\n%2\t2\t0\t10\t0\t10\t4\tagent\tcodex\n",
        )
        .unwrap();
        let captures = HashMap::from([
            (
                "%1".to_owned(),
                ansi_preview_text("left one\nleft two").unwrap(),
            ),
            (
                "%2".to_owned(),
                ansi_preview_text("right one\nright two").unwrap(),
            ),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 20, 4));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 nvim─┐┌ 2 codex┐");
        assert_eq!(rendered[1], "│left one││right on│");
        assert_eq!(rendered[2], "│left two││right tw│");
        assert_eq!(rendered[3], "└────────┘└────────┘");
    }

    #[test]
    fn composes_stacked_panes_into_window_preview() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t12\t3\ttop\tzsh\n%2\t2\t0\t0\t3\t12\t3\tbottom\tcodex\n",
        )
        .unwrap();
        let captures = HashMap::from([
            ("%1".to_owned(), ansi_preview_text("top line").unwrap()),
            ("%2".to_owned(), ansi_preview_text("bottom").unwrap()),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 12, 6));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 zsh────┐");
        assert_eq!(rendered[1], "│top line  │");
        assert_eq!(rendered[2], "└──────────┘");
        assert_eq!(rendered[3], "┌ 2 codex──┐");
        assert_eq!(rendered[4], "│bottom    │");
        assert_eq!(rendered[5], "└──────────┘");
    }

    #[test]
    fn composes_three_panes_with_left_half_and_right_quarters() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t20\t10\tone\tzsh\n%2\t2\t0\t20\t0\t20\t5\ttwo\tzsh\n%3\t3\t0\t20\t5\t20\t5\tthree\tzsh\n",
        )
        .unwrap();
        let captures = HashMap::from([
            ("%1".to_owned(), ansi_preview_text("one").unwrap()),
            ("%2".to_owned(), ansi_preview_text("two").unwrap()),
            ("%3".to_owned(), ansi_preview_text("three").unwrap()),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 30, 6));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 zsh───────┐┌ 2 zsh───────┐");
        assert_eq!(rendered[1], "│one          ││two          │");
        assert_eq!(rendered[2], "│             │└─────────────┘");
        assert_eq!(rendered[3], "│             │┌ 3 zsh───────┐");
        assert_eq!(rendered[4], "│             ││three        │");
        assert_eq!(rendered[5], "└─────────────┘└─────────────┘");
    }

    #[test]
    fn ansi_preview_text_preserves_color_and_spacing() {
        let text = ansi_preview_text("\u{1b}[31m  red  \u{1b}[0m\nplain").unwrap();

        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans[0].content.as_ref(), "  red  ");
        assert_eq!(text.lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(text.lines[1].spans[0].content.as_ref(), "plain");
    }

    #[test]
    fn preview_mirror_refreshes_on_new_window_area_or_after_interval() {
        let now = Instant::now();
        let mut mirror = PreviewMirror::default();
        let area = Rect::new(28, 0, 72, 40);

        assert!(mirror.should_refresh("@1", area, now));
        mirror.record_success("@1", area, ansi_preview_text("one").unwrap(), now);
        assert!(!mirror.should_refresh("@1", area, now + Duration::from_millis(99)));
        assert!(mirror.should_refresh("@1", area, now + PREVIEW_REFRESH_INTERVAL));
        assert!(mirror.should_refresh("@2", area, now + Duration::from_millis(10)));
        assert!(mirror.should_refresh(
            "@1",
            Rect::new(28, 0, 80, 40),
            now + Duration::from_millis(10)
        ));
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
                    &test_preview(),
                    0,
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
                    &test_preview(),
                    0,
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
        assert!(rendered.contains("tab: search / keys"));
        assert!(rendered.contains("enter"));
    }

    #[test]
    fn help_extends_modal_without_reducing_session_rows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };

        let closed = switcher_layout(area, false, ViewMode::Sidebar, 2);
        let open = switcher_layout(area, true, ViewMode::Sidebar, 2);

        assert_eq!(closed.list_overlay.x, 0);
        assert_eq!(closed.list_overlay.y, 0);
        assert_eq!(closed.list_overlay.width, 28);
        assert_eq!(closed.list_overlay.height, 40);
        assert_eq!(
            open.sessions.height,
            closed.sessions.height.saturating_sub(HELP_LINE_COUNT)
        );
        assert_eq!(open.list_overlay, closed.list_overlay);
        assert_eq!(open.help.unwrap().height, HELP_LINE_COUNT);
    }

    #[test]
    fn sidebar_right_docks_list_to_right_edge_with_preview_on_left() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };

        let left = switcher_layout(area, false, ViewMode::Sidebar, 2);
        let right = switcher_layout(area, false, ViewMode::SidebarRight, 2);

        assert_eq!(right.list_overlay.width, left.list_overlay.width);
        assert_eq!(right.list_overlay.height, left.list_overlay.height);
        assert_eq!(right.list_overlay.x, area.width - right.list_overlay.width);
        assert_eq!(right.preview.x, 0);
        assert_eq!(right.preview.width, area.width - right.list_overlay.width);
        assert_eq!(right.preview.height, area.height);
    }

    #[test]
    fn view_mode_toggle_cycles_through_all_three_positions() {
        assert_eq!(ViewMode::Sidebar.toggled(), ViewMode::SidebarRight);
        assert_eq!(ViewMode::SidebarRight.toggled(), ViewMode::Palette);
        assert_eq!(ViewMode::Palette.toggled(), ViewMode::Sidebar);
    }

    #[test]
    fn view_mode_round_trips_through_parse_and_format() {
        for view in [ViewMode::Sidebar, ViewMode::SidebarRight, ViewMode::Palette] {
            assert_eq!(parse_view_mode(format_view_mode(view)), Some(view));
        }
        assert_eq!(parse_view_mode("left"), Some(ViewMode::Sidebar));
        assert_eq!(parse_view_mode("right"), Some(ViewMode::SidebarRight));
        assert_eq!(parse_view_mode("center"), Some(ViewMode::Palette));
    }

    #[test]
    fn sidebar_sessions_are_pushed_to_bottom_when_content_is_short() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
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
                    &test_preview(),
                    0,
                )
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.get(0, 0).symbol(), "┌");
        assert_eq!(buffer.get(27, 0).symbol(), "┐");
        assert_eq!(buffer.get(0, 39).symbol(), "└");
        assert_eq!(buffer.get(27, 39).symbol(), "┘");
        // List sits just above the bottom search bar (separator + prompt).
        assert_eq!(buffer.get(2, 34).symbol(), "w");
        assert_eq!(buffer.get(3, 35).symbol(), "0");
        assert_eq!(buffer.get(5, 35).symbol(), "○");
        assert_eq!(buffer.get(2, 36).symbol(), "─");
        assert_eq!(buffer.get(2, 37).symbol(), "❯");
    }

    #[test]
    fn palette_layout_anchors_its_bottom_prompt_above_screen_middle() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };

        // Two compact lines: one session header + one window row. The box's
        // bottom edge sits at 55% of the height (row 22) and grows upward.
        let layout = switcher_layout(area, false, ViewMode::Palette, 2);

        assert_eq!(layout.preview, area);
        assert_eq!(
            layout.list_overlay,
            Rect {
                x: 22,
                y: 14,
                width: 55,
                height: 8,
            }
        );
        assert_eq!(
            layout.sessions,
            Rect {
                x: 24,
                y: 16,
                width: 51,
                height: 2,
            }
        );
        // Search bar at the bottom of the box: separator row + prompt row.
        assert_eq!(
            layout.search,
            Rect {
                x: 24,
                y: 18,
                width: 51,
                height: 2,
            }
        );
    }

    #[test]
    fn palette_layout_caps_height_so_long_lists_scroll() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };

        let layout = switcher_layout(area, false, ViewMode::Palette, 100);

        // Capped one row below the bottom anchor (55% of 40 = 22).
        assert_eq!(layout.list_overlay.height, 21);
        assert_eq!(layout.list_overlay.y, 1);
        assert_eq!(layout.sessions.height, 15);
    }

    #[test]
    fn palette_help_extends_the_floating_box() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };

        let closed = switcher_layout(area, false, ViewMode::Palette, 2);
        let open = switcher_layout(area, true, ViewMode::Palette, 2);

        assert_eq!(
            open.list_overlay.height,
            closed.list_overlay.height + HELP_LINE_COUNT
        );
        assert_eq!(open.sessions.height, closed.sessions.height);
        assert_eq!(open.help.unwrap().height, HELP_LINE_COUNT);
    }

    #[test]
    fn draw_renders_palette_box_over_fullscreen_preview() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
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
                    &test_preview(),
                    0,
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
                    &test_preview(),
                    0,
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
                    &test_preview(),
                    0,
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
                    || row.contains("search:")
                    || row.contains("keys:")
                    || row.contains("count+j/k:")
                    || row.contains("H/L:")
                    || row.contains("C-j/C-k")
                    || row.contains("←/→")
                    || row.contains("enter:")
                    || row.contains("C-t/C-s:")
                    || row.contains("C-u:")
                    || row.contains("esc:")
            })
            .collect::<Vec<_>>();

        assert_eq!(help_rows.len(), 12);
        assert!(help_rows
            .iter()
            .any(|row| row.contains("tab: search / keys")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("S-tab: palette/sidebar")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("search: type filters")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("keys: j/k move, n/N, q")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("count+j/k: move & open")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("H/L: previous/next edge")));
        assert!(help_rows.iter().any(|row| row.contains("C-j/C-k: open")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("←/→: switch session")));
        assert!(help_rows.iter().any(|row| row.contains("enter: open")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("C-t/C-s: new win/sess")));
        assert!(help_rows
            .iter()
            .any(|row| row.contains("C-u: clear filter")));
        assert!(help_rows.iter().any(|row| row.contains("esc: clear")));
    }

    #[test]
    fn prompt_submits_new_window_action_for_selected_session() {
        let mut prompt = PromptState::new(PromptKind::NewWindow {
            session_name: "work".to_owned(),
        });

        for ch in "server".chars() {
            assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Char(ch))), None);
        }

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Enter)),
            Some(Some(SwitcherAction::NewWindow {
                session_name: "work".to_owned(),
                window_name: "server".to_owned(),
            }))
        );
    }

    #[test]
    fn prompt_submits_new_session_action_with_same_input_flow() {
        let mut prompt = PromptState::new(PromptKind::NewSession);

        for ch in "ops".chars() {
            assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Char(ch))), None);
        }

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Enter)),
            Some(Some(SwitcherAction::NewSession {
                session_name: "ops".to_owned(),
            }))
        );
    }

    #[test]
    fn prompt_backspace_and_blank_submission_do_not_create_actions() {
        let mut prompt = PromptState::new(PromptKind::NewSession);

        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Char('x'))),
            None
        );
        assert_eq!(
            handle_prompt_key(&mut prompt, key(KeyCode::Backspace)),
            None
        );
        assert_eq!(handle_prompt_key(&mut prompt, key(KeyCode::Enter)), None);
        assert_eq!(prompt.input, "");
    }

    #[test]
    fn only_enter_selects_now_that_typing_filters() {
        let cards = vec![test_card("work", "1"), test_card("work", "2")];
        let sessions = group_cards_by_session(cards);
        let mut state = GridState::new();
        state.selected_column = 1;

        assert_eq!(
            select_key_action(key(KeyCode::Enter), &state, &sessions),
            Some(SwitcherAction::Select(test_card("work", "2")))
        );
        // Space is query input, not a select key.
        assert_eq!(
            select_key_action(key(KeyCode::Char(' ')), &state, &sessions),
            None
        );
    }

    #[test]
    fn draw_renders_new_window_prompt_inside_modal() {
        let groups = group_cards_by_session(vec![test_card("work", "1")]);
        let state = GridState::new();
        let mut prompt = PromptState::new(PromptKind::NewWindow {
            session_name: "work".to_owned(),
        });
        prompt.input = "server".to_owned();

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
                    Some(&prompt),
                    &test_preview(),
                    0,
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

        assert!(rendered.contains("New window name: server"));
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
            run_started_at: Some(crate::unix_timestamp()),
        };

        let line = compact_tab_line(&card, false, 0, 1, 32, 0);
        let runtime = &line.spans[5];
        let process = &line.spans[7];

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
                    &test_preview(),
                    0,
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
        assert_eq!(buffer.get(2, 32).symbol(), "w");
        assert_eq!(buffer.get(3, 33).symbol(), "0");
        assert_eq!(buffer.get(5, 33).symbol(), "○");
        assert_eq!(buffer.get(2, 34).symbol(), "o");

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
                    &test_preview(),
                    0,
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
                    &test_preview(),
                    0,
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
    fn groups_cards_by_session_in_window_order() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("work", "3"),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].session_name, "work");
        assert_eq!(groups[0].cards.len(), 3);
        assert_eq!(groups[1].session_name, "ops");
        assert_eq!(groups[1].cards.len(), 1);
    }

    #[test]
    fn compact_navigation_uses_terminal_height_for_vertical_viewport() {
        let groups = group_cards_by_session(vec![
            test_card("one", "1"),
            test_card("two", "1"),
            test_card("three", "1"),
            test_card("four", "1"),
            test_card("five", "1"),
        ]);
        let mut state = GridState::new();

        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        assert_eq!(state.selected_row, 3);
        assert_eq!(state.row_offset, 4);

        move_compact_selection(&mut state, &groups, Direction::Down, 5);
        assert_eq!(state.selected_row, 4);
        assert_eq!(state.row_offset, 6);
    }

    #[test]
    fn compact_navigation_moves_up_and_down_between_window_rows() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
        ]);
        let mut state = GridState::new();

        move_compact_selection(&mut state, &groups, Direction::Down, 6);
        assert_eq!((state.selected_row, state.selected_column), (0, 1));

        move_compact_selection(&mut state, &groups, Direction::Down, 6);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_selection(&mut state, &groups, Direction::Up, 6);
        assert_eq!((state.selected_row, state.selected_column), (0, 1));
    }

    #[test]
    fn compact_navigation_jumps_left_and_right_between_sessions() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("db", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;
        state.preferred_column = 1;

        move_compact_selection(&mut state, &groups, Direction::Right, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));

        move_compact_selection(&mut state, &groups, Direction::Right, 8);
        assert_eq!((state.selected_row, state.selected_column), (2, 0));
        assert_eq!(state.preferred_column, 1);

        move_compact_selection(&mut state, &groups, Direction::Left, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));
    }

    #[test]
    fn compact_session_edge_navigation_walks_first_and_last_items() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("db", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 2));

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 1));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 2));

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 0));
    }

    #[test]
    fn compact_session_edge_navigation_clamps_at_list_ends() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();

        move_compact_session_edge(&mut state, &groups, Direction::Up, 8);
        assert_eq!((state.selected_row, state.selected_column), (0, 0));

        state.selected_row = 1;
        move_compact_session_edge(&mut state, &groups, Direction::Down, 8);
        assert_eq!((state.selected_row, state.selected_column), (1, 0));
    }

    #[test]
    fn fuzzy_score_matches_subsequences_and_prefers_word_starts() {
        assert!(fuzzy_score("work window-1 zsh tmp", "wrk").is_some());
        assert!(fuzzy_score("work window-1 zsh tmp", "xyz").is_none());
        assert!(fuzzy_score("work editor", "EDIT").is_some());

        // A hit at a word start beats one buried mid-word.
        let word_start = fuzzy_score("ops server zsh tmp", "ser").unwrap();
        let buried = fuzzy_score("work browser zsh tmp", "ser").unwrap();
        assert!(word_start > buried);
    }

    #[test]
    fn filter_sessions_prunes_windows_and_empty_sessions() {
        let mut editor = test_card("work", "2");
        editor.window_name = "editor".to_owned();
        let sessions =
            group_cards_by_session(vec![test_card("work", "1"), editor, test_card("ops", "1")]);

        let filtered = filter_sessions(&sessions, "edit");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_name, "work");
        assert_eq!(filtered[0].cards.len(), 1);
        assert_eq!(filtered[0].cards[0].window_name, "editor");

        // Session names match too, keeping every window of that session.
        let filtered = filter_sessions(&sessions, "ops");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].cards.len(), 1);

        // A blank query keeps everything.
        assert_eq!(filter_sessions(&sessions, " "), sessions);
    }

    #[test]
    fn keys_mode_count_moves_across_sessions_like_vim() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
            test_card("ops", "2"),
            test_card("ops", "3"),
        ]);
        let mut state = GridState::new();
        let mut count = None;

        assert!(push_movement_count(&mut count, '3'));
        move_compact_selection_by(
            &mut state,
            &groups,
            Direction::Down,
            count.take().unwrap(),
            8,
        );

        assert_eq!((state.selected_row, state.selected_column), (1, 1));
        assert_eq!(count, None);
    }

    #[test]
    fn keys_mode_count_accepts_multiple_digits_and_clamps_at_the_edge() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        let mut count = None;

        assert!(push_movement_count(&mut count, '1'));
        assert!(push_movement_count(&mut count, '0'));
        assert_eq!(count, Some(10));
        move_compact_selection_by(&mut state, &groups, Direction::Down, count.unwrap(), 8);

        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        let mut leading_zero = None;
        assert!(!push_movement_count(&mut leading_zero, '0'));
        assert_eq!(leading_zero, None);
    }

    #[test]
    fn counted_lowercase_motion_selects_the_relative_target_for_opening() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
            test_card("ops", "1"),
            test_card("ops", "2"),
        ]);
        let mut state = GridState::new();
        state.selected_column = 1;

        let mut down_count = Some(2);
        assert_eq!(
            take_counted_open_motion(&mut down_count, 'j'),
            Some((Direction::Down, 2))
        );
        assert_eq!(down_count, None);

        let down = select_compact_relative(&mut state, &groups, Direction::Down, 2, 8).unwrap();
        assert_eq!(down.window_id, "@ops-1");
        assert_eq!((state.selected_row, state.selected_column), (1, 0));

        let mut up_count = Some(2);
        let (up_direction, up_count) = take_counted_open_motion(&mut up_count, 'k').unwrap();
        let up = select_compact_relative(&mut state, &groups, up_direction, up_count, 8).unwrap();
        assert_eq!(up.window_id, "@work-2");
        assert_eq!((state.selected_row, state.selected_column), (0, 1));

        let mut no_count = None;
        assert_eq!(take_counted_open_motion(&mut no_count, 'k'), None);
    }

    #[test]
    fn best_match_position_points_at_the_top_scoring_card() {
        let mut editor = test_card("ops", "2");
        editor.window_name = "editor".to_owned();
        let sessions =
            group_cards_by_session(vec![test_card("work", "1"), test_card("ops", "1"), editor]);

        assert_eq!(best_match_position(&sessions, "editor"), Some((1, 1)));
        assert_eq!(best_match_position(&sessions, ""), None);
        assert_eq!(best_match_position(&sessions, "zzz"), None);
    }

    #[test]
    fn compact_selected_line_index_counts_session_headers() {
        let groups = group_cards_by_session(vec![
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("ops", "1"),
        ]);
        let mut state = GridState::new();
        state.selected_row = 1;

        assert_eq!(compact_selected_line_index(&groups, &state), Some(4));
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
    fn compact_scroll_preserves_context_when_selection_is_visible() {
        let groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("alpha", "2"),
            test_card("work", "1"),
            test_card("work", "2"),
            test_card("work", "3"),
        ]);
        let mut state = GridState::new();
        state.selected_row = 1;
        state.row_offset = 1;

        keep_compact_selection_visible(&mut state, &groups, 6);

        assert_eq!(state.row_offset, 1);
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
        keep_compact_selection_visible(&mut state, &groups, 4);

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
    fn launch_state_uses_restored_compact_viewport() {
        let groups = group_cards_by_session(vec![
            test_card("one", "1"),
            test_card("two", "1"),
            test_card("three", "1"),
            test_card("four", "1"),
            test_card("five", "1"),
        ]);

        let state = initial_grid_state(&groups, Some("@five-1"), 5);

        assert_eq!(state.selected_row, 4);
        assert_eq!(state.row_offset, 6);
    }

    #[test]
    fn env_tmux_value_ignores_unexpanded_tmux_formats() {
        std::env::set_var("TMUX_AGENT_SWITCHER_TEST_LITERAL", "#{window_id}");
        assert_eq!(env_tmux_value("TMUX_AGENT_SWITCHER_TEST_LITERAL"), None);

        std::env::set_var("TMUX_AGENT_SWITCHER_TEST_LITERAL", "@42");
        assert_eq!(
            env_tmux_value("TMUX_AGENT_SWITCHER_TEST_LITERAL"),
            Some("@42".to_owned())
        );

        std::env::remove_var("TMUX_AGENT_SWITCHER_TEST_LITERAL");
    }

    fn test_preview() -> PreviewMirror {
        let mut preview = PreviewMirror::default();
        preview.text = Text::from("preview");
        preview
    }

    fn preview_text_lines(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn test_card(session_name: &str, window_index: &str) -> WindowCard {
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
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}

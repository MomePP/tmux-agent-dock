//! The background status daemon: polls every pane, infers agent status, and
//! caches it in tmux pane/window options so the switcher and status line can
//! read it cheaply.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{
    cards::rollup_agent_status,
    detect::{detect_agent_from_process_name, detect_agent_state, detect_agent_state_from_title},
    embed::{embedded_session_hosts, folded_panes},
    model::{
        format_agent_kind, format_agent_state, AgentEvidence, AgentKind, AgentState, AgentStatus,
        TmuxPane,
    },
    tmux::{
        parse_panes, parse_windows, set_pane_option, shell_quote, tmux_output, tmux_status,
        unix_timestamp,
    },
};

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
pub(crate) const BUSY_DEBOUNCE_POLLS: u32 = 2;
const STATUS_DAEMON_PID_OPTION: &str = "@tmux_agent_switcher_status_daemon_pid";
const STATUS_AGENT_OPTION: &str = "@tmux_agent_switcher_agent";
const STATUS_STATE_OPTION: &str = "@tmux_agent_switcher_state";
pub(crate) const STATUS_SEEN_OPTION: &str = "@tmux_agent_switcher_seen";
const STATUS_RUN_STARTED_OPTION: &str = "@tmux_agent_switcher_run_started_at";
const STATUS_UPDATED_OPTION: &str = "@tmux_agent_switcher_updated";
const STATUS_WINDOW_ICON_OPTION: &str = "@tmux_agent_switcher_window_icon";

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

    // Relinquish the option only while it is still ours. A daemon that lost the
    // handoff — the ordinary way one retires, by another taking the option —
    // used to clear the winner's claim on its way out. `ensure_status_daemon`
    // then read an empty option and started yet another daemon, which took the
    // option, which retired that one, which cleared it again. Once two ever
    // coexisted the pair never settled: a new process every few seconds for as
    // long as the tmux server lived.
    if current_status_daemon_pid() == pid {
        let _ = tmux_status(Command::new("tmux").args([
            "set-option",
            "-g",
            "-u",
            "-q",
            STATUS_DAEMON_PID_OPTION,
        ]));
    }
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
    let visible = visible_panes();
    // Built lazily, at most once per poll: needed whenever a pane's foreground
    // command is not itself an agent, to look for one deeper in the pane.
    let mut processes: Option<ProcessTree> = None;

    for pane in &mut panes {
        let previous = pane.agent_status;
        let agent = match detect_agent_from_process_name(&pane.pane_current_command) {
            Some(agent) => Some(agent),
            // The foreground command is not an agent, which says little on its own:
            // the agent may be running under a wrapper (a login shell, or the
            // `$SHELL -c "claude …"` tmux uses when a session is created with a
            // command — how sidekick.nvim's mux backend spawns one), or it may have
            // just exited. Look for one anywhere under the pane to tell those apart,
            // so a wrapped agent is detected and an exited one stops latching a
            // stale "claude idle". If the process table can't be read this poll,
            // keep the previous agent — a transient `ps` failure shouldn't drop a
            // live agent to unknown.
            None => {
                let tree = processes.get_or_insert_with(ProcessTree::snapshot);
                if tree.is_empty() {
                    previous.agent
                } else {
                    tree.agent_descendant(pane.pane_pid)
                }
            }
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
            let status = debounce_state(previous, agent, raw, pane_debounce, now);
            watched(status, visible.contains(pane.pane_id.as_str()))
        } else {
            debounce.remove(&pane.pane_id);
            AgentStatus::unknown()
        };
        write_agent_status(&pane.pane_id, previous, next)?;
        pane.agent_status = next;
    }

    // The status line reads these per window, so an embedded session's agent has
    // to be attributed to the window hosting it — nothing else shows the window
    // it actually runs in.
    let windows = parse_windows(&tmux_output(&[
        "list-windows",
        "-a",
        "-F",
        "#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_flags}",
    ])?)?;
    let processes = processes.get_or_insert_with(ProcessTree::snapshot);
    let embedded = embedded_session_hosts(&windows, &panes, processes.parents());
    write_window_status_icons(&panes, &folded_panes(&windows, &panes, &embedded))?;

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

/// A snapshot of the process table, used to find the agent running under a pane
/// when the pane's own foreground command isn't one, and to trace a tmux client
/// back to the pane it was launched from (see [`crate::embed`]).
pub(crate) struct ProcessTree {
    children: HashMap<u32, Vec<u32>>,
    parents: HashMap<u32, u32>,
    agents: HashMap<u32, AgentKind>,
}

impl ProcessTree {
    pub(crate) fn snapshot() -> Self {
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
        let mut parents = HashMap::new();
        let mut agents = HashMap::new();
        for line in output.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
                continue;
            };
            children.entry(ppid).or_default().push(pid);
            parents.insert(pid, ppid);
            if let Some(agent) = detect_agent_from_process_name(fields.next().unwrap_or_default()) {
                agents.insert(pid, agent);
            }
        }
        Self {
            children,
            parents,
            agents,
        }
    }

    /// True when the snapshot captured no processes at all, i.e. `ps` failed or
    /// produced nothing — a signal to treat its answers as unavailable.
    pub(crate) fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// pid -> parent pid, for walking a process back up to its ancestors.
    pub(crate) fn parents(&self) -> &HashMap<u32, u32> {
        &self.parents
    }

    /// Every pid under `root`, including `root` itself.
    pub(crate) fn descendants(&self, root: u32) -> HashSet<u32> {
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        seen
    }

    /// The agent `root` or any of its descendants is running, if any.
    fn agent_descendant(&self, root: Option<u32>) -> Option<AgentKind> {
        let root = root?;
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(agent) = self.agents.get(&pid) {
                return Some(*agent);
            }
            if let Some(children) = self.children.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        None
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

fn window_status_icons(
    panes: &[TmuxPane],
    folded: &HashMap<&str, Vec<&TmuxPane>>,
) -> HashMap<String, &'static str> {
    let mut statuses: HashMap<&str, Vec<AgentStatus>> = HashMap::new();
    for pane in panes {
        let window = statuses.entry(&pane.window_id).or_default();
        window.push(pane.agent_status);
        for embedded in folded.get(pane.pane_id.as_str()).into_iter().flatten() {
            window.push(embedded.agent_status);
        }
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

fn write_window_status_icons(
    panes: &[TmuxPane],
    folded: &HashMap<&str, Vec<&TmuxPane>>,
) -> Result<()> {
    let desired = window_status_icons(panes, folded);
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

/// An agent that finishes while you are watching it is not something you have
/// yet to read.
///
/// The done dot exists for turns that ended out of sight. Without this every
/// agent you sit with raised one the moment it stopped, and the only way to
/// clear it was to select the row you were already looking at.
fn watched(status: AgentStatus, visible: bool) -> AgentStatus {
    if visible {
        AgentStatus { seen: true, ..status }
    } else {
        status
    }
}

/// The panes on screen right now: their window is its session's current one,
/// and something is attached to that session.
///
/// tmux does the filtering, so this is one call whatever the pane count. The
/// `session_attached` half is what keeps a sidekick agent unread while its float
/// is shut — the session is alive but nobody is looking at it.
fn visible_panes() -> HashSet<String> {
    tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}",
        "-f",
        "#{&&:#{window_active},#{session_attached}}",
    ])
    .unwrap_or_default()
    .lines()
    .map(|line| line.trim().to_owned())
    .filter(|pane_id| !pane_id.is_empty())
    .collect()
}

pub(crate) fn mark_window_seen(window_id: &str) {
    let output =
        tmux_output(&["list-panes", "-t", window_id, "-F", "#{pane_id}"]).unwrap_or_default();
    for pane_id in output.lines().filter(|line| !line.trim().is_empty()) {
        mark_pane_seen(pane_id);
    }
}

pub(crate) fn mark_pane_seen(pane_id: &str) {
    let _ = set_pane_option(pane_id, STATUS_SEEN_OPTION, "1");
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let icons = window_status_icons(&panes, &HashMap::new());
        assert_eq!(icons.get("@1"), Some(&" #[fg=red,bold]◉#[default]"));
        assert_eq!(icons.get("@2"), Some(&""));
    }

    #[test]
    fn tmux_tab_icon_adopts_an_embedded_session_pane() {
        let panes = parse_panes(
            "%6\t@1\t1\tnvim\t/tmp\tnvim\t100\t\tunknown\t1\t\n\
             %20\t@9\t1\tnu\t/tmp\tagent\t200\tclaude\tworking\t1\t1000\n",
        )
        .unwrap();
        // %20 belongs to a session embedded in %6, so its status lands on @1 —
        // and @9, which nobody can see, keeps reporting for itself.
        let folded = HashMap::from([("%6", vec![&panes[1]])]);

        let icons = window_status_icons(&panes, &folded);
        assert_eq!(icons.get("@1"), Some(&" #[fg=yellow,bold]⠋#[default]"));
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

        // Unless you were watching it finish, in which case there is nothing
        // left to read and the done dot never appears.
        assert!(watched(done, true).seen);
        assert!(!watched(done, false).seen);
    }

    /// Visibility only ever grants "seen"; it never takes it back, and it never
    /// touches the rest of the status.
    #[test]
    fn watching_a_pane_marks_it_read_and_changes_nothing_else() {
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            run_started_at: None,
        };

        assert_eq!(
            watched(done, true),
            AgentStatus { seen: true, ..done },
            "only the read flag moves"
        );

        let working = AgentStatus {
            state: AgentState::Working,
            seen: true,
            run_started_at: Some(1000),
            ..done
        };
        assert_eq!(watched(working, false), working);
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
        assert_eq!(running.agent_descendant(Some(100)), Some(AgentKind::Claude));
        assert_eq!(running.agent_descendant(Some(200)), Some(AgentKind::Claude));

        // Same pane shell once Claude has exited: no agent left underneath.
        let exited = ProcessTree::parse("100 1 zsh\n400 100 nvim\n");
        assert_eq!(exited.agent_descendant(Some(100)), None);
        assert_eq!(exited.agent_descendant(None), None);

        // The versioned native binary is recognized by its comm path too.
        let versioned =
            ProcessTree::parse("10 1 -zsh\n11 10 /Users/x/.local/share/claude/versions/2.1.197\n");
        assert_eq!(versioned.agent_descendant(Some(10)), Some(AgentKind::Claude));
    }

    #[test]
    fn process_tree_finds_the_agent_a_wrapper_shell_runs() {
        // How sidekick.nvim's tmux backend spawns one: the pane runs the login
        // shell tmux started it with, and the agent is a child of that shell.
        let wrapped = ProcessTree::parse("100 1 nu\n200 100 /Users/x/.local/bin/claude_1\n");
        assert_eq!(wrapped.agent_descendant(Some(100)), Some(AgentKind::Claude));
    }

    #[test]
    fn empty_process_tree_signals_ps_unavailable() {
        // A failed/empty `ps` yields an empty tree, which the poll treats as
        // "unknown" (keep the previous agent) rather than "no agent".
        assert!(ProcessTree::parse("").is_empty());
        assert!(!ProcessTree::parse("1 0 launchd\n").is_empty());
    }
}

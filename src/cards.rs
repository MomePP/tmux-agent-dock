//! Building the window cards the switcher displays: joining tmux windows with
//! their panes, rolling pane statuses up per window, grouping cards by session,
//! and persisting the user's session order.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;

use crate::{
    daemon::{ensure_status_daemon, ProcessTree},
    detect::detect_agent_from_process_name,
    model::format_agent_kind,
    embed::{embedded_session_hosts, folded_panes},
    model::{
        AgentKind, AgentState, AgentStatus, FoldedAgent, SessionGroup, TmuxPane, TmuxWindow,
        WindowCard,
    },
    tmux::{parse_panes, parse_windows, tmux_output, tmux_status},
};

const SESSION_ORDER_OPTION: &str = "@tmux_agent_dock_order";

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
    build_cards_with_previews(windows, panes, &HashMap::new(), &HashMap::new(), unread_dir)
}

/// `embedded` maps sessions that live inside another pane to that pane (see
/// [`crate::embed`]): they get no cards of their own, and their panes count
/// toward the card of the pane hosting them.
pub fn build_cards_with_previews(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    embedded: &HashMap<String, String>,
    previews: &HashMap<String, Vec<String>>,
    unread_dir: &Path,
) -> Vec<WindowCard> {
    let folded = folded_panes(windows, panes, embedded);
    let window_sessions: HashMap<&str, &str> = windows
        .iter()
        .map(|window| (window.window_id.as_str(), window.session_name.as_str()))
        .collect();

    windows
        .iter()
        .filter(|window| !embedded.contains_key(&window.session_name))
        .filter_map(|window| {
            let active_pane = panes
                .iter()
                .find(|pane| pane.window_id == window.window_id && pane.pane_active)
                .or_else(|| panes.iter().find(|pane| pane.window_id == window.window_id))?;

            let window_panes: Vec<&TmuxPane> = panes
                .iter()
                .filter(|pane| pane.window_id == window.window_id)
                .collect();
            let adopted: Vec<&TmuxPane> = window_panes
                .iter()
                .filter_map(|pane| folded.get(pane.pane_id.as_str()))
                .flatten()
                .copied()
                .collect();
            let codex_unread = window_panes
                .iter()
                .chain(adopted.iter())
                .any(|pane| codex_unread_file(unread_dir, &pane.pane_id).exists());
            let agent_status = rollup_agent_status(
                window_panes
                    .iter()
                    .chain(adopted.iter())
                    .map(|pane| pane.agent_status),
            );

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
                folded_agents: adopted
                    .iter()
                    .filter(|pane| pane.agent_status.agent.is_some())
                    .map(|pane| FoldedAgent {
                        pane_id: pane.pane_id.clone(),
                        status: pane.agent_status,
                        label: folded_agent_label(
                            window_sessions.get(pane.window_id.as_str()).copied(),
                            pane.agent_status,
                        ),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// What to call a folded agent in the Agents list.
///
/// Two agents spawned from the same Neovim share a host window, so the window's
/// name cannot tell them apart. Their embedded sessions are named
/// `<clone> <cwd-hash>` — `claude_1 b9f9f91c` — and the clone is the half that
/// distinguishes them, and the half the user chose when they pressed
/// `<leader>s` or `2<leader>s`. Fall back to the plain tool name when the
/// session is not shaped that way.
fn folded_agent_label(session_name: Option<&str>, status: AgentStatus) -> String {
    session_name
        .and_then(|name| name.split_whitespace().next())
        .filter(|clone| !clone.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format_agent_kind(status.agent).to_owned())
}

pub(crate) fn rollup_agent_status(statuses: impl Iterator<Item = AgentStatus>) -> AgentStatus {
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

pub(crate) fn apply_session_order(sessions: &mut [SessionGroup], order: &[String]) {
    let positions: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    sessions.sort_by_key(|session| {
        positions
            .get(session.session_name.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
}

pub(crate) fn load_session_order() -> Result<Vec<String>> {
    let output = tmux_output(&[
        "list-sessions",
        "-F",
        "#{session_name}\t#{@tmux_agent_dock_order}",
    ])?;
    let mut rows: Vec<(usize, Option<usize>, String)> = output
        .lines()
        .enumerate()
        .filter_map(|(default_index, line)| {
            let (session_name, rank) = line.split_once('\t')?;
            Some((
                default_index,
                rank.parse::<usize>().ok(),
                session_name.to_owned(),
            ))
        })
        .collect();
    rows.sort_by_key(|(default_index, rank, _)| {
        (rank.is_none(), rank.unwrap_or(usize::MAX), *default_index)
    });
    Ok(rows
        .into_iter()
        .map(|(_, _, session_name)| session_name)
        .collect())
}

pub(crate) fn persist_session_order(sessions: &[SessionGroup]) {
    for (rank, session) in sessions.iter().enumerate() {
        let _ = tmux_status(Command::new("tmux").args([
            "set-option",
            "-q",
            "-t",
            &session.session_name,
            SESSION_ORDER_OPTION,
            &rank.to_string(),
        ]));
    }
}

/// The process label shown for a card (and matched when filtering): agent
/// binaries are normalized to their plain names, anything else shows as-is.
pub(crate) fn compact_tab_process_text(card: &WindowCard) -> String {
    match detect_agent_from_process_name(&card.command) {
        Some(AgentKind::Codex) => "codex".to_owned(),
        Some(AgentKind::Claude) => "claude".to_owned(),
        Some(AgentKind::OpenCode) => "opencode".to_owned(),
        None => card.command.clone(),
    }
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
        "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}\t#{@tmux_agent_dock_agent}\t#{@tmux_agent_dock_state}\t#{@tmux_agent_dock_seen}\t#{@tmux_agent_dock_run_started_at}\t#{@codex_status_state}\t#{@codex_status_unread}",
    ])?)?;
    let processes = ProcessTree::snapshot();
    let embedded = embedded_session_hosts(&windows, &panes, processes.parents());
    Ok(build_cards_with_previews(
        &windows,
        &panes,
        &embedded,
        &HashMap::new(),
        &codex_unread_dir(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::test_card;

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
    fn embedded_session_is_hidden_and_its_status_folded_into_the_host_card() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20.json"), "{}").unwrap();

        let windows =
            parse_windows("@1\tdotfiles\t0\teditor\t*\n@9\tclaude_1 abc\t0\tagent\t*\n").unwrap();
        // %6 runs Neovim; %20 is the agent in the session embedded inside it.
        let panes = parse_panes(
            "%6\t@1\t1\tnvim\t/Users/example/.config\teditor\t100\t\tunknown\t1\t\n\
             %20\t@9\t1\tnu\t/Users/example/.config\tagent\t200\tclaude\tworking\t1\t1000\n",
        )
        .unwrap();
        let embedded = HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())]);

        let cards =
            build_cards_with_previews(&windows, &panes, &embedded, &HashMap::new(), dir.path());

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].session_name, "dotfiles");
        assert_eq!(cards[0].agent_status.state, AgentState::Working);
        assert_eq!(cards[0].agent_status.agent, Some(AgentKind::Claude));
        // The card still shows what the pane itself runs.
        assert_eq!(cards[0].command, "nvim");
        // Unread and seen state live on the folded pane, which has no card.
        assert!(cards[0].codex_unread);
        assert_eq!(
            cards[0]
                .folded_agents
                .iter()
                .map(|agent| agent.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["%20"]
        );
        // The clone half of `claude_1 abc`, which is what tells two agents in
        // one window apart.
        assert_eq!(cards[0].folded_agents[0].label, "claude_1");
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
    fn persisted_session_order_overrides_tmux_order_and_appends_new_sessions() {
        let mut groups = group_cards_by_session(vec![
            test_card("alpha", "1"),
            test_card("beta", "1"),
            test_card("new", "1"),
        ]);

        apply_session_order(&mut groups, &["beta".to_owned(), "alpha".to_owned()]);

        let names: Vec<&str> = groups
            .iter()
            .map(|session| session.session_name.as_str())
            .collect();
        assert_eq!(names, ["beta", "alpha", "new"]);
    }
}

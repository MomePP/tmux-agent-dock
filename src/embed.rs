//! Sessions that live *inside* another pane.
//!
//! A tmux session is not always a peer of the others. sidekick.nvim's default
//! `cli.mux.create = "terminal"` runs `tmux new -A -s "<tool> <hash>"` inside a
//! Neovim terminal buffer, so the agent gets a real, persistent tmux session
//! whose only door is that Neovim float. Neovim gives the buffer its own pty,
//! so the nested client shares neither tty nor window with the pane hosting it
//! and tmux reports the session as top-level like any other.
//!
//! Listing it that way is doubly wrong: the session is not somewhere you can
//! usefully switch to, and the pane that *is* — the one running Neovim — shows
//! no agent at all. So we trace each client's process ancestry back to the pane
//! it was launched from, and callers hide those sessions while rolling their
//! agent status up into the host pane.
//!
//! A session is only folded away when *every* client attached to it is embedded
//! that way. One real terminal attachment and it stays listed on its own, which
//! is what should happen once its Neovim exits and it outlives its host.

use std::collections::{HashMap, HashSet};

use crate::model::{TmuxPane, TmuxWindow};
use crate::tmux::tmux_output;

/// Depth cap for the ancestry walk. A client sits a handful of processes below
/// its pane; anything deeper is a cycle in a malformed `ps` snapshot.
const MAX_ANCESTRY_HOPS: usize = 64;

/// Maps each embedded session's name to the `pane_id` it is running inside.
pub fn embedded_session_hosts(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    parents: &HashMap<u32, u32>,
) -> HashMap<String, String> {
    if parents.is_empty() {
        return HashMap::new();
    }
    let clients = parse_clients(
        &tmux_output(&["list-clients", "-F", "#{session_name}\t#{client_pid}"]).unwrap_or_default(),
    );
    resolve_embedded(&clients, parents, windows, panes)
}

/// The panes of embedded sessions, keyed by the pane hosting them.
pub(crate) fn folded_panes<'a>(
    windows: &'a [TmuxWindow],
    panes: &'a [TmuxPane],
    embedded: &'a HashMap<String, String>,
) -> HashMap<&'a str, Vec<&'a TmuxPane>> {
    let mut folded: HashMap<&str, Vec<&TmuxPane>> = HashMap::new();
    if embedded.is_empty() {
        return folded;
    }

    let sessions = window_sessions(windows);
    for pane in panes {
        let Some(session_name) = sessions.get(pane.window_id.as_str()) else {
            continue;
        };
        let Some(host_pane_id) = embedded.get(*session_name) else {
            continue;
        };
        folded.entry(host_pane_id.as_str()).or_default().push(pane);
    }
    folded
}

pub(crate) fn parse_clients(output: &str) -> Vec<(String, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let (session_name, client_pid) = line.rsplit_once('\t')?;
            Some((session_name.to_owned(), client_pid.trim().parse().ok()?))
        })
        .collect()
}

pub(crate) fn resolve_embedded(
    clients: &[(String, u32)],
    parents: &HashMap<u32, u32>,
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
) -> HashMap<String, String> {
    let sessions = window_sessions(windows);
    let by_pid: HashMap<u32, &TmuxPane> = panes
        .iter()
        .filter_map(|pane| Some((pane.pane_pid?, pane)))
        .collect();

    let mut hosts: HashMap<String, String> = HashMap::new();
    let mut standalone: HashSet<&str> = HashSet::new();

    for (session_name, client_pid) in clients {
        // A client running in a pane of the session it is attached to is a
        // recursive attach, not an embed — folding it would hide the pane into
        // itself, so leave that session alone.
        let host = host_pane(*client_pid, parents, &by_pid).filter(|pane| {
            sessions.get(pane.window_id.as_str()).copied() != Some(session_name.as_str())
        });
        match host {
            Some(pane) => {
                hosts
                    .entry(session_name.clone())
                    .or_insert_with(|| pane.pane_id.clone());
            }
            // A client we can't trace back into a pane is a real terminal
            // attachment: the session stands on its own and must stay listed.
            None => {
                standalone.insert(session_name.as_str());
            }
        }
    }

    hosts.retain(|session_name, _| !standalone.contains(session_name.as_str()));
    hosts
}

/// Walks up from a tmux client to the first pane whose process it descends
/// from, i.e. the pane the client was launched in.
fn host_pane<'a>(
    client_pid: u32,
    parents: &HashMap<u32, u32>,
    by_pid: &HashMap<u32, &'a TmuxPane>,
) -> Option<&'a TmuxPane> {
    let mut pid = client_pid;
    for _ in 0..MAX_ANCESTRY_HOPS {
        if let Some(pane) = by_pid.get(&pid) {
            return Some(pane);
        }
        pid = *parents.get(&pid)?;
        if pid <= 1 {
            return None;
        }
    }
    None
}

fn window_sessions(windows: &[TmuxWindow]) -> HashMap<&str, &str> {
    windows
        .iter()
        .map(|window| (window.window_id.as_str(), window.session_name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentStatus;

    fn window(window_id: &str, session_name: &str) -> TmuxWindow {
        TmuxWindow {
            window_id: window_id.to_owned(),
            session_name: session_name.to_owned(),
            window_index: "0".to_owned(),
            window_name: "window".to_owned(),
            window_flags: String::new(),
        }
    }

    fn pane(pane_id: &str, window_id: &str, pane_pid: u32) -> TmuxPane {
        TmuxPane {
            pane_id: pane_id.to_owned(),
            window_id: window_id.to_owned(),
            pane_active: true,
            pane_current_command: "nvim".to_owned(),
            pane_current_path: "/tmp".to_owned(),
            pane_title: String::new(),
            pane_pid: Some(pane_pid),
            agent_status: AgentStatus::unknown(),
        }
    }

    /// `dotfiles` pane %6 runs nu(100) -> nvim(110) -> tmux client(120), which
    /// is attached to the nested `claude_1 abc` session holding pane %20.
    fn fixture() -> (Vec<TmuxWindow>, Vec<TmuxPane>, HashMap<u32, u32>) {
        let windows = vec![window("@1", "dotfiles"), window("@2", "claude_1 abc")];
        let panes = vec![pane("%6", "@1", 100), pane("%20", "@2", 200)];
        let parents = HashMap::from([(120, 110), (110, 100), (100, 1), (200, 1), (300, 1)]);
        (windows, panes, parents)
    }

    #[test]
    fn folds_a_session_whose_client_runs_inside_another_pane() {
        let (windows, panes, parents) = fixture();
        let clients = vec![
            ("dotfiles".to_owned(), 300),
            ("claude_1 abc".to_owned(), 120),
        ];

        let embedded = resolve_embedded(&clients, &parents, &windows, &panes);
        assert_eq!(
            embedded,
            HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())])
        );

        let folded = folded_panes(&windows, &panes, &embedded);
        assert_eq!(folded["%6"].len(), 1);
        assert_eq!(folded["%6"][0].pane_id, "%20");
    }

    #[test]
    fn keeps_a_session_that_also_has_a_terminal_client() {
        let (windows, panes, parents) = fixture();
        // Same nested client, plus a real terminal attached to that session.
        let clients = vec![
            ("claude_1 abc".to_owned(), 120),
            ("claude_1 abc".to_owned(), 300),
        ];

        assert!(resolve_embedded(&clients, &parents, &windows, &panes).is_empty());
    }

    #[test]
    fn keeps_a_detached_session_and_ignores_recursive_attaches() {
        let (windows, panes, parents) = fixture();
        // No clients at all: nothing to trace, nothing folded.
        assert!(resolve_embedded(&[], &parents, &windows, &panes).is_empty());

        // A client inside %6 attached back to %6's own session.
        let clients = vec![("dotfiles".to_owned(), 120)];
        assert!(resolve_embedded(&clients, &parents, &windows, &panes).is_empty());
    }

    #[test]
    fn parses_client_rows_with_spaces_in_the_session_name() {
        assert_eq!(
            parse_clients("claude_1 b9f9f91c\t65611\ndotfiles-config\t60584\nbroken\n"),
            vec![
                ("claude_1 b9f9f91c".to_owned(), 65611),
                ("dotfiles-config".to_owned(), 60584),
            ]
        );
    }
}

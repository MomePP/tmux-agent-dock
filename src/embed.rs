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
//!
//! Ancestry needs a client to walk up from, and closing the float detaches one
//! without ending the session — `tmux new -A -s` keeps it alive for the next
//! open. So the answer is also remembered in [`EMBEDDED_HOSTS_OPTION`] and kept
//! for as long as it can still be true: the session still exists, and the pane
//! that hosted it still exists and still belongs to some other session. A
//! sidekick session therefore stays folded into its Neovim while its float is
//! shut, and comes back on its own the moment that pane is gone.

use std::collections::{HashMap, HashSet};

use crate::daemon::ProcessTree;
use crate::model::{TmuxPane, TmuxWindow};
use crate::tmux::{parse_panes, parse_windows, tmux_output};

/// Depth cap for the ancestry walk. A client sits a handful of processes below
/// its pane; anything deeper is a cycle in a malformed `ps` snapshot.
const MAX_ANCESTRY_HOPS: usize = 64;

/// Where the resolved mapping is remembered, so a session whose float is closed
/// — and which therefore has no client left to trace — stays folded. One
/// `session\tpane` per line; session names are already assumed tab-free by
/// [`parse_clients`].
///
/// PUBLISHED INTERFACE — outside tooling reads this (README, "Reading the
/// embedded-session map"); tmux-resurrect filters embedded sessions out of its
/// save file on it. Renaming the option or changing the line format breaks such
/// readers *silently*, because their natural fallback is the session-name
/// guessing this exists to replace. Change it here and in the README together.
const EMBEDDED_HOSTS_OPTION: &str = "@tmux_agent_dock_embedded";

/// Maps each embedded session's name to the `pane_id` it is running inside.
pub fn embedded_session_hosts(
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
    parents: &HashMap<u32, u32>,
) -> HashMap<String, String> {
    let stored = tmux_output(&["show-option", "-gqv", EMBEDDED_HOSTS_OPTION]).unwrap_or_default();

    // No process snapshot means no ancestry to walk — but what was already
    // remembered is still as true as it was, so it is kept rather than dropped.
    let live = if parents.is_empty() {
        LiveEmbedding::default()
    } else {
        let clients = parse_clients(
            &tmux_output(&["list-clients", "-F", "#{session_name}\t#{client_pid}"])
                .unwrap_or_default(),
        );
        resolve_embedded(&clients, parents, windows, panes)
    };

    let hosts = merge_remembered(live, &parse_remembered(&stored), windows, panes);

    // Written back only when it actually changed: this runs on every poll of
    // both the daemon and an open dock, and an unconditional `set-option` would
    // be a fork, an exec and a socket round trip several times a second for a
    // value that changes when a float opens or closes.
    //
    // Never under `cfg(test)`, for the reason `ui::mod::set_global_options`
    // gives: a plain `cargo test` that reaches a live tmux write rewrites the
    // developer's own server state with fixture data.
    let formatted = format_remembered(&hosts);
    if !cfg!(test) && formatted != stored.trim_end_matches('\n') {
        let _ = tmux_output(&["set-option", "-g", EMBEDDED_HOSTS_OPTION, &formatted]);
    }
    hosts
}

/// What the currently-attached clients say about which sessions are embedded.
#[derive(Debug, Default)]
pub(crate) struct LiveEmbedding {
    /// Session name to the pane it is running inside.
    pub(crate) hosts: HashMap<String, String>,
    /// Sessions holding at least one client that is *not* inside a pane — a real
    /// terminal attachment. These stay listed however they started, and their
    /// remembered host is discarded rather than merged back in.
    pub(crate) standalone: HashSet<String>,
}

pub(crate) fn parse_remembered(value: &str) -> Vec<(String, String)> {
    value
        .lines()
        .filter_map(|line| line.rsplit_once('\t'))
        .map(|(session, pane)| (session.to_owned(), pane.trim().to_owned()))
        .collect()
}

/// Sorted, so an unchanged mapping formats to an unchanged string and the
/// write-back can be skipped.
pub(crate) fn format_remembered(hosts: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = hosts
        .iter()
        .map(|(session, pane)| format!("{session}\t{pane}"))
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Adds back the sessions we have seen embedded before and that nothing since
/// has contradicted.
///
/// A remembered entry survives while all three still hold: the session exists,
/// the host pane exists, and that pane belongs to some *other* session — the
/// last one both rules out a session folded into itself and expires the memory
/// when the Neovim's pane is gone. Anything a live client says wins outright.
pub(crate) fn merge_remembered(
    live: LiveEmbedding,
    remembered: &[(String, String)],
    windows: &[TmuxWindow],
    panes: &[TmuxPane],
) -> HashMap<String, String> {
    let LiveEmbedding {
        mut hosts,
        standalone,
    } = live;

    let sessions = window_sessions(windows);
    let alive: HashSet<&str> = windows
        .iter()
        .map(|window| window.session_name.as_str())
        .collect();
    let pane_owners: HashMap<&str, &str> = panes
        .iter()
        .filter_map(|pane| {
            Some((
                pane.pane_id.as_str(),
                *sessions.get(pane.window_id.as_str())?,
            ))
        })
        .collect();

    for (session, host) in remembered {
        if standalone.contains(session.as_str()) || !alive.contains(session.as_str()) {
            continue;
        }
        if let Some(owner) = pane_owners.get(host.as_str()) {
            if *owner != session.as_str() {
                hosts.entry(session.clone()).or_insert_with(|| host.clone());
            }
        }
    }
    hosts
}

/// The tty of a client that is not itself running inside a tmux pane, or `None`
/// when nothing is embedded — in which case the acting client is already right.
///
/// Driving the switcher from a sidekick float means the acting client is the
/// *nested* one. Switching that client to an outer session attaches a session
/// inside one of its own panes, and tmux renders that recursively: panes jump,
/// the layout looks broken, and it is hard to get out of. Every client-scoped
/// command must therefore name the outer client rather than inheriting
/// whichever one happened to run it.
/// The embedded sessions as last worked out, without working them out again.
///
/// The daemon and every card refresh keep this current, so anything that only
/// needs to ask "is this session inside a pane?" can have the answer for one
/// small read instead of listing the server and walking the process table.
pub(crate) fn remembered_embedded() -> HashMap<String, String> {
    parse_remembered(
        &tmux_output(&["show-option", "-gqv", EMBEDDED_HOSTS_OPTION]).unwrap_or_default(),
    )
    .into_iter()
    .collect()
}

/// This is on the path of every switch the user makes, so it takes the answer
/// the daemon and every card refresh already keep in [`EMBEDDED_HOSTS_OPTION`]:
/// two small reads instead of listing every window and pane on the server and
/// walking the whole process table. An *empty* value is the one case that has to
/// be worked out from scratch, because it cannot tell "nothing is embedded" from
/// "nobody has computed this yet".
pub fn outer_client_tty() -> Option<String> {
    let clients = tmux_output(&["list-clients", "-F", "#{session_name}\t#{client_tty}"]).ok()?;

    let remembered: HashMap<String, String> =
        parse_remembered(&tmux_output(&["show-option", "-gqv", EMBEDDED_HOSTS_OPTION]).ok()?)
            .into_iter()
            .collect();
    if !remembered.is_empty() {
        return outer_tty(&clients, &remembered);
    }

    outer_tty(&clients, &resolve_embedded_now()?)
}

/// The first client that is not inside a pane. A client attached to an embedded
/// session *is* inside one — that is what makes the session embedded — so the
/// map of those is enough to tell them apart without walking any processes here.
pub(crate) fn outer_tty(clients: &str, embedded: &HashMap<String, String>) -> Option<String> {
    if embedded.is_empty() {
        return None;
    }
    clients
        .lines()
        .filter_map(|line| line.rsplit_once('\t'))
        .find(|(session, _)| !embedded.contains_key(*session))
        .map(|(_, tty)| tty.trim().to_owned())
        .filter(|tty| !tty.is_empty())
}

/// The full sweep: every window, every pane, the whole process table. Only worth
/// it when nothing has been remembered.
fn resolve_embedded_now() -> Option<HashMap<String, String>> {
    let windows = parse_windows(
        &tmux_output(&[
            "list-windows",
            "-a",
            "-F",
            "#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_flags}",
        ])
        .ok()?,
    )
    .ok()?;
    let panes = parse_panes(
        &tmux_output(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{window_id}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}\t#{pane_pid}",
        ])
        .ok()?,
    )
    .ok()?;

    let processes = ProcessTree::snapshot();
    Some(embedded_session_hosts(&windows, &panes, processes.parents()))
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
) -> LiveEmbedding {
    let sessions = window_sessions(windows);
    let by_pid: HashMap<u32, &TmuxPane> = panes
        .iter()
        .filter_map(|pane| Some((pane.pane_pid?, pane)))
        .collect();

    let mut hosts: HashMap<String, String> = HashMap::new();
    let mut standalone: HashSet<String> = HashSet::new();

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
                standalone.insert(session_name.clone());
            }
        }
    }

    hosts.retain(|session_name, _| !standalone.contains(session_name.as_str()));
    LiveEmbedding { hosts, standalone }
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

        let embedded = resolve_embedded(&clients, &parents, &windows, &panes).hosts;
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

        let live = resolve_embedded(&clients, &parents, &windows, &panes);
        assert!(live.hosts.is_empty());
        assert!(live.standalone.contains("claude_1 abc"));
    }

    #[test]
    fn a_detached_session_traces_to_nothing_and_ignores_recursive_attaches() {
        let (windows, panes, parents) = fixture();
        // No clients at all: nothing to trace, nothing to say.
        let live = resolve_embedded(&[], &parents, &windows, &panes);
        assert!(live.hosts.is_empty());
        assert!(live.standalone.is_empty());

        // A client inside %6 attached back to %6's own session.
        let clients = vec![("dotfiles".to_owned(), 120)];
        assert!(resolve_embedded(&clients, &parents, &windows, &panes)
            .hosts
            .is_empty());
    }

    /// Closing the sidekick float detaches the client but leaves the session
    /// running, so ancestry has nothing to walk. The remembered answer carries
    /// it: the session is still there and so is the Neovim's pane.
    #[test]
    fn a_session_whose_float_closed_stays_folded() {
        let (windows, panes, _) = fixture();
        let remembered = vec![("claude_1 abc".to_owned(), "%6".to_owned())];

        let hosts = merge_remembered(LiveEmbedding::default(), &remembered, &windows, &panes);

        assert_eq!(
            hosts,
            HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())])
        );
    }

    /// The three ways the memory expires. Each one means the fold can no longer
    /// be true, and the session has to come back as a peer.
    #[test]
    fn the_remembered_host_expires_when_it_can_no_longer_be_true() {
        let (windows, panes, _) = fixture();
        let remembered = vec![("claude_1 abc".to_owned(), "%6".to_owned())];
        let merge = |live, windows: &[TmuxWindow], panes: &[TmuxPane]| {
            merge_remembered(live, &remembered, windows, panes)
        };

        // The Neovim's pane is gone — its window closed, or the editor quit.
        let orphaned: Vec<TmuxPane> = panes
            .iter()
            .filter(|pane| pane.pane_id != "%6")
            .cloned()
            .collect();
        assert!(merge(LiveEmbedding::default(), &windows, &orphaned).is_empty());

        // The session itself ended.
        let without: Vec<TmuxWindow> = windows
            .iter()
            .filter(|window| window.session_name != "claude_1 abc")
            .cloned()
            .collect();
        assert!(merge(LiveEmbedding::default(), &without, &panes).is_empty());

        // Someone attached a real terminal to it: it is a session in its own
        // right now, whatever it used to be.
        let attached = LiveEmbedding {
            standalone: HashSet::from(["claude_1 abc".to_owned()]),
            ..LiveEmbedding::default()
        };
        assert!(merge(attached, &windows, &panes).is_empty());
    }

    /// A live client outranks the memory — the float can be reopened from a
    /// different Neovim than the one that first spawned it.
    #[test]
    fn a_live_client_wins_over_a_stale_remembered_host() {
        let (mut windows, mut panes, parents) = fixture();
        windows.push(window("@3", "other"));
        panes.push(pane("%30", "@3", 400));

        let live = resolve_embedded(
            &[("claude_1 abc".to_owned(), 120)],
            &parents,
            &windows,
            &panes,
        );
        let stale = vec![("claude_1 abc".to_owned(), "%30".to_owned())];

        assert_eq!(
            merge_remembered(live, &stale, &windows, &panes),
            HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())])
        );
    }

    #[test]
    fn remembered_hosts_round_trip_through_the_option() {
        let hosts = HashMap::from([
            ("claude_1 abc".to_owned(), "%6".to_owned()),
            ("codex_2 def".to_owned(), "%9".to_owned()),
        ]);

        let formatted = format_remembered(&hosts);

        assert_eq!(formatted, "claude_1 abc\t%6\ncodex_2 def\t%9");
        assert_eq!(
            parse_remembered(&formatted)
                .into_iter()
                .collect::<HashMap<_, _>>(),
            hosts
        );
        assert!(parse_remembered("").is_empty());
        assert!(parse_remembered("no tab here\n").is_empty());
        assert!(format_remembered(&HashMap::new()).is_empty());
    }

    /// The client to act on is the one that is not inside a pane. Getting this
    /// wrong is not cosmetic: switching a *nested* client to an outer session
    /// attaches that session inside one of its own panes, which tmux renders
    /// recursively and is hard to get out of.
    #[test]
    fn the_outer_client_is_the_one_not_attached_to_an_embedded_session() {
        let clients = "claude_1 abc\t/dev/ttys002\ndotfiles\t/dev/ttys022\n";
        let embedded = HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())]);

        assert_eq!(
            outer_tty(clients, &embedded),
            Some("/dev/ttys022".to_owned()),
            "the float's own client must never be the one acted on"
        );

        // Nothing embedded: whichever client is acting is already the right one,
        // and naming it explicitly would only be a chance to name it wrongly.
        assert_eq!(outer_tty(clients, &HashMap::new()), None);
        // Every client is inside a pane, so there is no outer one to name.
        assert_eq!(
            outer_tty(
                "claude_1 abc\t/dev/ttys002\n",
                &HashMap::from([("claude_1 abc".to_owned(), "%6".to_owned())])
            ),
            None
        );
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

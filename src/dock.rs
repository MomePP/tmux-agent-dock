//! The docked sidebar's tmux orchestration: opening it, carrying it into
//! whatever window becomes active, and closing it again.
//!
//! The argument vectors are built by pure functions so the flags that matter —
//! the `-d` that stops a move from stealing focus, the `-b -h` that puts the
//! dock on the left — are pinned by tests rather than by a shell script nobody
//! re-reads.

use std::process::Command;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::OnceLock;

use anyhow::Result;

use crate::tmux::{tmux_output, tmux_status};

pub(crate) const DOCK_PANE_OPTION: &str = "@tmux_agent_switcher_dock_pane";
pub(crate) const DOCK_MOVING_OPTION: &str = "@tmux_agent_switcher_dock_moving";
pub(crate) const DOCK_LAYOUT_OPTION: &str = "@tmux_agent_switcher_dock_layout";
pub(crate) const DOCK_WIDTH_OPTION: &str = "@agent_switcher_dock_width";
/// The outer client the dock belongs to, resolved once when it opens.
pub(crate) const DOCK_CLIENT_OPTION: &str = "@tmux_agent_switcher_dock_client";
pub(crate) const DEFAULT_DOCK_WIDTH: u16 = 30;

/// Creates the dock pane on the left of `target_window`. `-d` leaves the user
/// in the pane they were working in; `-P -F` prints the new pane's id so the
/// caller can record it.
///
/// The window is named explicitly rather than left to the acting client:
/// pressed inside a sidekick float that client is the nested one, and the dock
/// would be built inside the embedded session instead of beside the Neovim
/// hosting it.
pub(crate) fn split_args(width: u16, exe: &str, target_window: &str) -> Vec<String> {
    vec![
        "split-window".to_owned(),
        "-b".to_owned(),
        "-h".to_owned(),
        "-d".to_owned(),
        "-l".to_owned(),
        width.to_string(),
        "-t".to_owned(),
        target_window.to_owned(),
        "-P".to_owned(),
        "-F".to_owned(),
        "#{pane_id}".to_owned(),
        format!("{exe} dock"),
    ]
}

/// Carries the dock into the window holding `target_pane`. `-d` keeps the moved
/// pane inactive, so the follow hook never steals focus.
pub(crate) fn move_args(width: u16, dock_pane: &str, target_pane: &str) -> Vec<String> {
    vec![
        "move-pane".to_owned(),
        "-b".to_owned(),
        "-h".to_owned(),
        "-d".to_owned(),
        "-l".to_owned(),
        width.to_string(),
        "-s".to_owned(),
        dock_pane.to_owned(),
        "-t".to_owned(),
        target_pane.to_owned(),
    ]
}

/// Puts a window back the way it was before the dock arrived.
pub(crate) fn restore_layout_args(window_id: &str, layout: &str) -> Vec<String> {
    vec![
        "select-layout".to_owned(),
        "-t".to_owned(),
        window_id.to_owned(),
        layout.to_owned(),
    ]
}

/// A dock needs at least as much room again beside it, or it is a sidebar with
/// nothing to sit next to.
fn has_room(window_width: u16, dock_width: u16) -> bool {
    window_width >= dock_width.saturating_mul(2)
}

fn parse_width(value: &str) -> u16 {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|width| *width > 0)
        .unwrap_or(DEFAULT_DOCK_WIDTH)
}

fn width() -> u16 {
    parse_width(&option(&["show-option", "-gqv", DOCK_WIDTH_OPTION]))
}

fn run(args: &[String]) -> Result<()> {
    tmux_status(Command::new("tmux").args(args))
}

fn option(args: &[&str]) -> String {
    tmux_output(args).unwrap_or_default().trim().to_owned()
}

fn set_global(name: &str, value: &str) -> Result<()> {
    run(&[
        "set-option".to_owned(),
        "-g".to_owned(),
        name.to_owned(),
        value.to_owned(),
    ])
}

fn unset_global(name: &str) {
    let _ = run(&[
        "set-option".to_owned(),
        "-g".to_owned(),
        "-u".to_owned(),
        name.to_owned(),
    ]);
}

/// The dock's pane id, or `None` when it is closed — which includes a recorded
/// pane that no longer exists, so a pane killed by hand cannot wedge the toggle.
fn dock_pane() -> Option<String> {
    let pane = option(&["show-option", "-gqv", DOCK_PANE_OPTION]);
    if pane.is_empty() {
        return None;
    }
    let alive = tmux_output(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap_or_default()
        .lines()
        .any(|line| line.trim() == pane);
    alive.then_some(pane)
}

fn window_of(pane: &str) -> String {
    option(&["display-message", "-p", "-t", pane, "#{window_id}"])
}

fn save_layout(window_id: &str) {
    let layout = option(&["display-message", "-p", "-t", window_id, "#{window_layout}"]);
    let _ = run(&[
        "set-option".to_owned(),
        "-w".to_owned(),
        "-t".to_owned(),
        window_id.to_owned(),
        DOCK_LAYOUT_OPTION.to_owned(),
        layout,
    ]);
}

/// Puts `window_id` back the way it was and forgets the saved layout.
///
/// Failure is ignored on purpose: if panes were created or destroyed while the
/// dock was there, the saved string no longer matches the pane set and tmux's
/// own arrangement is a perfectly good outcome. Restoring geometry is a
/// courtesy, not a guarantee worth erroring over.
fn restore_layout(window_id: &str) {
    let layout = option(&["show-option", "-wqv", "-t", window_id, DOCK_LAYOUT_OPTION]);
    if !layout.is_empty() {
        let _ = run(&restore_layout_args(window_id, &layout));
    }
    let _ = run(&[
        "set-option".to_owned(),
        "-w".to_owned(),
        "-u".to_owned(),
        "-t".to_owned(),
        window_id.to_owned(),
        DOCK_LAYOUT_OPTION.to_owned(),
    ]);
}

/// `prefix + b`: opens the dock beside the current window, or closes it.
pub fn toggle() -> Result<()> {
    if let Some(pane) = dock_pane() {
        let host = window_of(&pane);
        run(&["kill-pane".to_owned(), "-t".to_owned(), pane])?;
        restore_layout(&host);
        unset_global(DOCK_PANE_OPTION);
        unset_global(DOCK_CLIENT_OPTION);
        return Ok(());
    }

    let dock_width = width();
    let outer = crate::embed::outer_client_tty();
    let window_width = match &outer {
        Some(tty) => option(&["display-message", "-p", "-c", tty, "#{window_width}"]),
        None => option(&["display-message", "-p", "#{window_width}"]),
    }
    .parse::<u16>()
    .unwrap_or(0);
    if !has_room(window_width, dock_width) {
        let _ = run(&[
            "display-message".to_owned(),
            format!(
                "agent-switcher: need {} columns for the dock",
                dock_width.saturating_mul(2)
            ),
        ]);
        return Ok(());
    }

    // Resolve through the outer client. Pressed inside a sidekick float the
    // acting client is the nested one, and the dock would be built inside that
    // embedded session — invisible to the real Neovim, and gone when the float
    // closes. Remember it so `follow` asks the same client every time.
    let client = outer;
    let host = match &client {
        Some(tty) => option(&["display-message", "-p", "-c", tty, "#{window_id}"]),
        None => option(&["display-message", "-p", "#{window_id}"]),
    };
    if host.is_empty() {
        return Ok(());
    }
    let _ = set_global(DOCK_CLIENT_OPTION, client.as_deref().unwrap_or(""));
    save_layout(&host);

    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let args = split_args(dock_width, &exe, &host);
    let pane = tmux_output(&args.iter().map(String::as_str).collect::<Vec<_>>())?
        .trim()
        .to_owned();

    set_global(DOCK_PANE_OPTION, &pane)
}

/// The follow hook: carries the dock into whatever window just became active.
///
/// This runs on every window and session change, so its cost is the cost of
/// switching. Each `tmux` call is a fork, an exec and a socket round trip, and
/// doing them one at a time made the dock visibly arrive after the window had
/// already been drawn. Formats can read options (`#{@name}`) and one invocation
/// can carry several commands, so the whole hook is three round trips: read
/// everything about the client, read everything about the dock, then mutate.
/// Chaining the mutations also means tmux redraws once rather than after each.
pub fn follow() -> Result<()> {
    const CLIENT_PROBE: &str = concat!(
        "#{window_id}\t#{pane_id}\t#{window_layout}\t",
        "#{@tmux_agent_switcher_dock_pane}\t",
        "#{@tmux_agent_switcher_dock_moving}\t",
        "#{@agent_switcher_dock_width}\t",
        "#{@tmux_agent_switcher_dock_client}"
    );

    // Globals only, so this is client-independent and safe to ask of whichever
    // client triggered the hook.
    let probe = option(&["display-message", "-p", CLIENT_PROBE]);
    let mut fields = probe.split('\t');
    let _ = fields.next();
    let _ = fields.next();
    let _ = fields.next();
    let pane = fields.next().unwrap_or_default().to_owned();
    let moving = fields.next().unwrap_or_default().to_owned();
    let dock_width = parse_width(fields.next().unwrap_or_default());
    let client = fields.next().unwrap_or_default().trim().to_owned();

    if pane.is_empty() {
        return Ok(());
    }
    // `move-pane` changes the active window and re-fires the hooks that called
    // us. Without this guard the second call moves the dock straight back.
    if !moving.is_empty() {
        return Ok(());
    }

    // Targeting the recorded pane doubles as the aliveness check: tmux fails
    // when it no longer exists, and a stale recording is cleared so the next
    // toggle opens cleanly instead of trying to kill a pane that is gone.
    // Where the dock should be is wherever the OUTER client is looking. Asking
    // the triggering client instead would carry the dock into a sidekick float's
    // embedded session the moment its window changed.
    let mut destination = vec!["display-message".to_owned(), "-p".to_owned()];
    if !client.is_empty() {
        destination.push("-c".to_owned());
        destination.push(client.clone());
    }
    destination.push("#{window_id}\t#{pane_id}\t#{window_layout}".to_owned());
    let Ok(dest) = tmux_output(&destination.iter().map(String::as_str).collect::<Vec<_>>()) else {
        // The remembered client is gone (detached). Leave the dock where it is
        // rather than guessing and moving it somewhere the user is not.
        return Ok(());
    };
    let mut dest_fields = dest.trim_end_matches('\n').split('\t');
    let current = dest_fields.next().unwrap_or_default().to_owned();
    let target = dest_fields.next().unwrap_or_default().to_owned();
    let current_layout = dest_fields.next().unwrap_or_default().to_owned();

    let Ok(host_probe) = tmux_output(&[
        "display-message",
        "-p",
        "-t",
        &pane,
        "#{window_id}\t#{@tmux_agent_switcher_dock_layout}",
    ]) else {
        unset_global(DOCK_PANE_OPTION);
        return Ok(());
    };
    let mut host_fields = host_probe.trim_end_matches('\n').split('\t');
    let host = host_fields.next().unwrap_or_default().to_owned();
    let host_layout = host_fields.next().unwrap_or_default().to_owned();

    if current.is_empty() || current == host {
        return Ok(());
    }

    // One invocation: guard, remember the destination's geometry, move, put the
    // window we left back, forget its layout, release the guard.
    let mut batch = vec![
        "set-option".to_owned(),
        "-g".to_owned(),
        DOCK_MOVING_OPTION.to_owned(),
        "1".to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-w".to_owned(),
        "-t".to_owned(),
        current.clone(),
        DOCK_LAYOUT_OPTION.to_owned(),
        current_layout,
        ";".to_owned(),
    ];
    batch.extend(move_args(dock_width, &pane, &target));
    if !host_layout.is_empty() {
        batch.push(";".to_owned());
        batch.extend(restore_layout_args(&host, &host_layout));
    }
    batch.extend([
        ";".to_owned(),
        "set-option".to_owned(),
        "-w".to_owned(),
        "-u".to_owned(),
        "-t".to_owned(),
        host,
        DOCK_LAYOUT_OPTION.to_owned(),
        ";".to_owned(),
        "set-option".to_owned(),
        "-g".to_owned(),
        "-u".to_owned(),
        DOCK_MOVING_OPTION.to_owned(),
    ]);

    let moved = run(&batch);
    if moved.is_err() {
        // The batch stops at the failure, so the guard would stay set and the
        // dock would never follow again.
        unset_global(DOCK_MOVING_OPTION);
    }
    moved
}

/// Holds the dock at its configured width.
///
/// `move-pane -l` sets the width once, when the dock arrives, and nothing
/// re-asserts it afterwards. tmux rescales a window's panes whenever the window
/// itself changes size — and windows change size constantly, because each one is
/// sized to the last client that looked at it. A 30-column dock in a window
/// squeezed to 120 columns and back came out 41 columns wide in a direct test;
/// on the way down it was crushed to 1. That is the sidebar the user sees
/// "resizing by itself", wider in some windows than others.
///
/// The dock is the one process that always knows how wide it is — the terminal
/// it just drew into *is* the pane — so it corrects itself rather than adding a
/// resize hook that fires on every client event.
///
/// `current` is that drawn width. Best-effort: a window with no room to spare
/// keeps whatever tmux gave it.
pub(crate) fn keep_width(current: u16) {
    static CONFIGURED: OnceLock<u16> = OnceLock::new();
    static LAST_SEEN: AtomicU16 = AtomicU16::new(0);

    // Only act when the width has just changed. A window too narrow to give the
    // dock its columns would otherwise be asked to resize on every tick for
    // ever, because the answer never becomes the one being asked for.
    let changed = LAST_SEEN.swap(current, Ordering::Relaxed) != current;
    let configured = *CONFIGURED.get_or_init(width);
    if !changed || current == configured {
        return;
    }

    let Some(pane) = crate::tmux::env_tmux_value("TMUX_PANE") else {
        return;
    };
    let _ = run(&[
        "resize-pane".to_owned(),
        "-t".to_owned(),
        pane,
        "-x".to_owned(),
        configured.to_string(),
    ]);
}

/// Keeps the dock's own working directory matching the pane it sits beside.
///
/// tmux names a window after its *active* pane. With
/// `automatic-rename-format '#{b:pane_current_path}'` — the user's setup — that
/// means clicking into the sidebar renames the host window to the dock's
/// directory: `momeppkt`, the basename of the home directory the dock inherited
/// when it was spawned. It reverts the moment focus leaves, so the window name
/// flickers on every visit to the sidebar.
///
/// Matching the work pane's directory makes that rename a no-op — tmux computes
/// the same name from either pane. The alternative, turning `automatic-rename`
/// off on the host window, would freeze the name of whichever window you are
/// actually working in, which is worse than the flicker and overrides a setting
/// the user chose.
///
/// Best-effort throughout: a dock that cannot find its own pane, or whose
/// neighbour's directory has been deleted, simply keeps the cwd it has.
pub(crate) fn match_host_cwd() {
    let Some(dock) = crate::tmux::env_tmux_value("TMUX_PANE") else {
        return;
    };
    // `-t <pane>` targets the window *containing* that pane, which is what the
    // dock needs — it has no other handle on where it currently lives.
    let Ok(panes) = tmux_output(&[
        "list-panes",
        "-t",
        &dock,
        "-F",
        "#{pane_id}\t#{pane_current_path}",
    ]) else {
        return;
    };
    let Some(path) = panes
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(pane_id, path)| *pane_id != dock && !path.is_empty())
        .map(|(_, path)| path)
    else {
        return;
    };
    let _ = std::env::set_current_dir(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_puts_the_dock_left_without_taking_focus() {
        let args = split_args(30, "/opt/bin/tmux-agent-switcher", "@7");

        assert_eq!(
            args,
            vec![
                "split-window",
                "-b",
                "-h",
                "-d",
                "-l",
                "30",
                "-t",
                "@7",
                "-P",
                "-F",
                "#{pane_id}",
                "/opt/bin/tmux-agent-switcher dock",
            ]
        );
    }

    /// `-d` is the whole reason the follow hook does not have to re-select the
    /// pane the user was in: a move without it makes the dock active.
    #[test]
    fn move_keeps_the_dock_left_and_unfocused() {
        let args = move_args(30, "%7", "%12");

        assert_eq!(
            args,
            vec!["move-pane", "-b", "-h", "-d", "-l", "30", "-s", "%7", "-t", "%12"]
        );
    }

    #[test]
    fn restore_layout_targets_the_window_it_belongs_to() {
        let args = restore_layout_args("@3", "b5e2,80x24,0,0,1");

        assert_eq!(args, vec!["select-layout", "-t", "@3", "b5e2,80x24,0,0,1"]);
    }

    #[test]
    fn a_window_narrower_than_two_docks_has_no_room() {
        // 30 columns of dock needs 30 more to work in.
        assert!(!has_room(59, 30));
        assert!(has_room(60, 30));
        assert!(has_room(200, 30));
    }

    #[test]
    fn the_configured_width_falls_back_to_the_default() {
        assert_eq!(parse_width(""), DEFAULT_DOCK_WIDTH);
        assert_eq!(parse_width("  "), DEFAULT_DOCK_WIDTH);
        assert_eq!(parse_width("not a number"), DEFAULT_DOCK_WIDTH);
        assert_eq!(parse_width("0"), DEFAULT_DOCK_WIDTH);
        assert_eq!(parse_width("42"), 42);
        assert_eq!(parse_width(" 42 "), 42);
    }
}

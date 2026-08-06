//! Asking a Neovim to bring one of its sidekick agents forward.
//!
//! Agents spawned by sidekick live in embedded tmux sessions whose only door is
//! a Neovim float, so several share one host pane and focusing that pane lands
//! you on whichever slot Neovim last had open. Selecting `claude_2` has to say
//! so, and the only component that can act on it is the Neovim itself.
//!
//! Keystrokes cannot express it: the plugin's own binding is a *toggle*, so
//! sending it would hide an already-open float as often as reveal one. Neovim's
//! RPC socket can — `sidekick.cli.show` is idempotent, reveals and focuses, and
//! takes the clone by name.
//!
//! Everything here is best effort. No socket, no Neovim, a Neovim without
//! sidekick loaded, a renamed API — each ends the same way: the host pane is
//! focused and nothing is disturbed, which is exactly the old behaviour.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::daemon::ProcessTree;

/// Reveals `clone` in whichever Neovim is running under `pane_id`.
pub(crate) fn show_agent(pane_pid: Option<u32>, clone: &str) {
    let Some(socket) = socket_under(pane_pid) else {
        return;
    };
    // `pcall` so a Neovim without sidekick loaded fails inside Lua rather than
    // returning an error we would only discard anyway.
    let expr = format!(
        "luaeval(\"pcall(function() require('sidekick.cli').show({{ name = _A, filter = {{ cwd = true }}, focus = true }}) end)\", '{}')",
        clone.replace('\'', "''")
    );
    let _ = Command::new("nvim")
        .args(["--server", &socket.to_string_lossy(), "--remote-expr", &expr])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The RPC socket of a Neovim running under `pane_pid`.
///
/// Neovim names its socket `nvim.<pid>.0`, so the pid in the filename is what
/// ties a socket to a pane — several Neovims run at once and only the one under
/// this pane hosts these agents.
fn socket_under(pane_pid: Option<u32>) -> Option<PathBuf> {
    let pids = descendants(pane_pid?);
    if pids.is_empty() {
        return None;
    }

    let base = std::env::temp_dir().join(format!("nvim.{}", std::env::var("USER").ok()?));
    for session in std::fs::read_dir(base).ok()? {
        let Ok(session) = session else { continue };
        let Ok(entries) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if socket_pid(&entry.file_name().to_string_lossy()).is_some_and(|pid| pids.contains(&pid))
            {
                return Some(entry.path());
            }
        }
    }
    None
}

/// The pid embedded in a `nvim.<pid>.0` socket name.
fn socket_pid(file_name: &str) -> Option<u32> {
    file_name
        .strip_prefix("nvim.")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn descendants(root: u32) -> HashSet<u32> {
    let tree = ProcessTree::snapshot();
    tree.descendants(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_name_carries_the_pid_that_owns_it() {
        assert_eq!(socket_pid("nvim.99191.0"), Some(99191));
        assert_eq!(socket_pid("nvim.1.0"), Some(1));
        assert_eq!(socket_pid("nvim.abc.0"), None);
        assert_eq!(socket_pid("something-else"), None);
        assert_eq!(socket_pid("nvim."), None);
    }

    /// Without a pane there is nothing to search under, and the caller falls
    /// back to plain focus.
    #[test]
    fn no_pane_means_no_socket() {
        assert!(socket_under(None).is_none());
    }
}

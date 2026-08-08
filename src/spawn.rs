//! Starting a child process and not waiting for it.
//!
//! Two things here want the same awkward shape: ask something slow to happen,
//! and carry on drawing. Getting that right depends on which of this crate's
//! processes is asking, and both cases have to work:
//!
//! - The **popup** exits the moment it has asked. Only a separate *process*
//!   survives that, so a detached thread would take the request to the grave.
//! - The **daemon** and the **dock** live for as long as the tmux server. A
//!   bare `spawn()` there leaves a zombie behind every single time, so
//!   something must `wait()` on the child — which a thread can do.
//!
//! A spawned process plus a thread waiting on it satisfies both at once.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Runs `command` in the background, discarding whatever it has to say.
///
/// The child gets its own process group. The popup surface runs inside
/// `display-popup -E`, which tmux tears down as soon as its command exits, and
/// a child left in that group would be signalled along with it — possibly
/// before it had done anything at all.
pub(crate) fn detached(command: &mut Command) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();

    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Runs a user-written command line in the background, through a shell.
///
/// Through a shell because the string comes from a tmux option: whatever a
/// person would reasonably write in their `tmux.conf` — arguments, a `~`, a
/// pipeline — should work. Empty means "nothing configured", not "run a shell".
pub(crate) fn shell_detached(command_line: &str) {
    if command_line.trim().is_empty() {
        return;
    }
    detached(Command::new("sh").args(["-c", command_line]));
}

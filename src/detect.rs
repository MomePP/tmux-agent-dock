//! Passive agent detection: recognizing agent processes by name and inferring
//! their state (working / blocked / idle) from the pane title and screen tail.

use crate::model::{AgentEvidence, AgentKind, AgentState};

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

pub(crate) fn detect_agent_state_from_title(agent: AgentKind, title: &str) -> Option<AgentState> {
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
/// (`BUSY_DEBOUNCE_POLLS` in the daemon module) already absorbs the common
/// fast-typed case.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

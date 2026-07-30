//! Fuzzy filtering of the session/window list, telescope-style.

use crate::{
    cards::compact_tab_process_text,
    model::{SessionGroup, WindowCard},
    ui::state::{fallback_grid_state, keep_compact_selection_visible, GridState},
};

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
pub(crate) fn apply_query(
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
pub(crate) fn delete_query_word(query: &mut String) {
    while query.ends_with(' ') {
        query.pop();
    }
    while query.chars().last().is_some_and(|ch| ch != ' ') {
        query.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cards::group_cards_by_session, test_support::test_card};

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
    fn best_match_position_points_at_the_top_scoring_card() {
        let mut editor = test_card("ops", "2");
        editor.window_name = "editor".to_owned();
        let sessions =
            group_cards_by_session(vec![test_card("work", "1"), test_card("ops", "1"), editor]);

        assert_eq!(best_match_position(&sessions, "editor"), Some((1, 1)));
        assert_eq!(best_match_position(&sessions, ""), None);
        assert_eq!(best_match_position(&sessions, "zzz"), None);
    }
}

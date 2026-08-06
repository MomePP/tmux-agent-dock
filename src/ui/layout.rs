//! Screen geometry: where the list box, search bar, help, and preview sit for
//! each view mode.

use ratatui::layout::Rect;

use super::state::{InputMode, ViewMode};

const SIDEBAR_WIDTH_PERCENT: u16 = 25;
const SIDEBAR_MIN_WIDTH: u16 = 28;
const SIDEBAR_MAX_WIDTH: u16 = 64;
pub(crate) const FLOATING_LIST_INSET: u16 = 2;
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
pub(crate) const SEARCH_BAR_ROWS: u16 = 2;
pub(crate) const HELP_LINE_COUNT: u16 = 14;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SwitcherLayout {
    pub(crate) list_overlay: Rect,
    pub(crate) search: Rect,
    pub(crate) sessions: Rect,
    pub(crate) help: Option<Rect>,
    pub(crate) preview: Rect,
}

pub(crate) fn switcher_layout(
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

pub(crate) fn switcher_layout_for_input(
    area: Rect,
    show_help: bool,
    view: ViewMode,
    line_count: usize,
    input: InputMode,
) -> SwitcherLayout {
    let mut layout = switcher_layout(area, show_help, view, line_count);
    if matches!(input, InputMode::Keys | InputMode::Numbers) {
        let reclaimed_rows = layout.search.height;
        layout.sessions.height = layout.sessions.height.saturating_add(reclaimed_rows);
        if let Some(help) = layout.help.as_mut() {
            help.y = help.y.saturating_add(reclaimed_rows);
        }
        layout.search.y = layout.search.y.saturating_add(layout.search.height);
        layout.search.height = 0;
    }
    layout
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

pub(crate) fn compact_navigation_height(
    terminal_size: Rect,
    show_help: bool,
    view: ViewMode,
    line_count: usize,
    input: InputMode,
) -> u16 {
    switcher_layout_for_input(terminal_size, show_help, view, line_count, input)
        .sessions
        .height
        .saturating_add(1)
}

/// One column of breathing room down the dock's left edge. The popup gets this
/// for free from the border it sits inside; the dock has no border, so without
/// it every row starts hard against the pane divider — and a column off from
/// the status line above, which is where the eye picks up the alignment.
const DOCK_LEFT_PAD: u16 = 1;

/// Geometry for the docked sidebar: the pane is the list. No modal inset,
/// because there is no border to sit inside, and no preview, because the work
/// pane beside the dock is the thing a preview would have been showing.
///
/// Row order matches the popup so the two surfaces feel the same: list on top,
/// help below it, search prompt on the bottom row.
#[allow(dead_code)] // Consumed by Task 4.
pub(crate) fn dock_layout(area: Rect, show_help: bool, input: InputMode) -> SwitcherLayout {
    let search_height = if matches!(input, InputMode::Search) {
        area.height.min(SEARCH_BAR_ROWS)
    } else {
        0
    };
    let body_height = area.height.saturating_sub(search_height);
    let help_height = if show_help {
        HELP_LINE_COUNT.min(body_height)
    } else {
        0
    };
    // Every content rect starts one column in; `list_overlay` keeps the pane's
    // own bounds, since it is what the surface clears and measures against.
    let content = Rect {
        x: area.x.saturating_add(DOCK_LEFT_PAD),
        width: area.width.saturating_sub(DOCK_LEFT_PAD),
        ..area
    };

    let sessions = Rect {
        height: body_height.saturating_sub(help_height),
        ..content
    };
    let help = (help_height > 0).then_some(Rect {
        y: area.y.saturating_add(sessions.height),
        height: help_height,
        ..content
    });
    let search = Rect {
        y: area.y.saturating_add(body_height),
        height: search_height,
        ..content
    };

    SwitcherLayout {
        list_overlay: area,
        search,
        sessions,
        help,
        preview: Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        },
    }
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

pub(crate) fn inset_rect(area: Rect, inset: u16) -> Rect {
    let doubled = inset.saturating_mul(2);
    Rect {
        x: area.x.saturating_add(inset),
        y: area.y.saturating_add(inset),
        width: area.width.saturating_sub(doubled),
        height: area.height.saturating_sub(doubled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            height: 44,
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
    fn navigation_modes_hide_the_input_bar_and_reclaim_its_rows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let search =
            switcher_layout_for_input(area, false, ViewMode::Sidebar, 4, InputMode::Search);
        let keys = switcher_layout_for_input(area, false, ViewMode::Sidebar, 4, InputMode::Keys);
        let numbers =
            switcher_layout_for_input(area, false, ViewMode::Sidebar, 4, InputMode::Numbers);

        assert_eq!(search.search.height, SEARCH_BAR_ROWS);
        assert_eq!(keys.search.height, 0);
        assert_eq!(numbers.search.height, 0);
        assert_eq!(
            keys.sessions.height,
            search.sessions.height + SEARCH_BAR_ROWS
        );
        assert_eq!(
            numbers.sessions.height,
            search.sessions.height + SEARCH_BAR_ROWS
        );
    }

    #[test]
    fn dock_layout_uses_the_whole_pane_with_no_modal_inset() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };

        let layout = dock_layout(area, false, InputMode::Keys);

        // No border to inset past, but one column of left padding so rows line
        // up with the status line above rather than hugging the pane divider.
        assert_eq!(layout.list_overlay, area);
        assert_eq!(layout.sessions.x, DOCK_LEFT_PAD);
        assert_eq!(layout.sessions.width, 30 - DOCK_LEFT_PAD);
        assert_eq!(layout.search.x, DOCK_LEFT_PAD);
        // Nothing to preview beside a pane that fills its own width.
        assert_eq!(layout.preview.width, 0);
        assert_eq!(layout.preview.height, 0);
        // Keys mode hides the search bar, so the list takes the full height.
        assert_eq!(layout.sessions.height, 40);
        assert_eq!(layout.help, None);
    }

    #[test]
    fn dock_layout_puts_the_search_bar_at_the_bottom_in_search_mode() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };

        let layout = dock_layout(area, false, InputMode::Search);

        assert_eq!(layout.search.height, SEARCH_BAR_ROWS);
        assert_eq!(layout.search.y, 40 - SEARCH_BAR_ROWS);
        assert_eq!(layout.sessions.height, 40 - SEARCH_BAR_ROWS);
        assert_eq!(layout.sessions.y, 0);
    }

    #[test]
    fn dock_layout_gives_help_the_rows_below_the_list() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 40,
        };

        let layout = dock_layout(area, true, InputMode::Keys);
        let help = layout.help.expect("help area");

        assert_eq!(help.height, HELP_LINE_COUNT);
        assert_eq!(layout.sessions.height, 40 - HELP_LINE_COUNT);
        assert_eq!(help.y, layout.sessions.height);
    }
}

//! Live pane previews: capturing pane content, parsing its ANSI colors into
//! ratatui text, and compositing multi-pane windows into one scaled mock-up.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

use crate::{
    model::WindowCard,
    tmux::{split_tmux_fields, tmux_output},
    ACCENT,
};

pub(crate) const PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// The cached preview of the selected window, refreshed at most every
/// [`PREVIEW_REFRESH_INTERVAL`] (or immediately when the window or area
/// changes).
#[derive(Clone, Debug, Default)]
pub(crate) struct PreviewMirror {
    pub(crate) window_id: Option<String>,
    pub(crate) area: Option<Rect>,
    pub(crate) text: Text<'static>,
    pub(crate) refreshed_at: Option<Instant>,
}

impl PreviewMirror {
    fn should_refresh(&self, window_id: &str, area: Rect, now: Instant) -> bool {
        if self.window_id.as_deref() != Some(window_id) || self.area != Some(area) {
            return true;
        }

        self.refreshed_at
            .map(|refreshed_at| now.duration_since(refreshed_at) >= PREVIEW_REFRESH_INTERVAL)
            .unwrap_or(true)
    }

    pub(crate) fn refresh_for(&mut self, card: Option<&WindowCard>, area: Rect, now: Instant) {
        let Some(card) = card else {
            return;
        };
        if !self.should_refresh(&card.window_id, area, now) {
            return;
        }

        if let Ok(text) = capture_window_preview_text(&card.window_id, area)
            .or_else(|_| capture_preview_text(&card.target_pane_id))
        {
            self.record_success(&card.window_id, area, text, now);
        }
    }

    fn record_success(&mut self, window_id: &str, area: Rect, text: Text<'static>, now: Instant) {
        self.window_id = Some(window_id.to_owned());
        self.area = Some(area);
        self.text = text;
        self.refreshed_at = Some(now);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowPreviewPane {
    pane_id: String,
    pane_index: String,
    pane_active: bool,
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    pane_title: String,
    pane_current_command: String,
}

fn parse_window_preview_panes(output: &str) -> Result<Vec<WindowPreviewPane>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = split_tmux_fields(line, 9)?;
            Ok(WindowPreviewPane {
                pane_id: fields[0].to_owned(),
                pane_index: fields[1].to_owned(),
                pane_active: fields[2] == "1",
                left: fields[3].parse().unwrap_or(0),
                top: fields[4].parse().unwrap_or(0),
                width: fields[5].parse().unwrap_or(0),
                height: fields[6].parse().unwrap_or(0),
                pane_title: fields[7].to_owned(),
                pane_current_command: fields[8].to_owned(),
            })
        })
        .collect()
}

fn preview_capture_args(pane_id: &str) -> Vec<&str> {
    vec!["capture-pane", "-epN", "-t", pane_id]
}

fn capture_preview_text(pane_id: &str) -> Result<Text<'static>> {
    ansi_preview_text(&tmux_output(&preview_capture_args(pane_id))?)
}

fn capture_window_preview_text(window_id: &str, area: Rect) -> Result<Text<'static>> {
    let panes = parse_window_preview_panes(&tmux_output(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_title}\t#{pane_current_command}",
    ])?)?;

    if panes.is_empty() {
        return Err(anyhow!("window {window_id} has no panes"));
    }

    if panes.len() == 1 {
        return capture_preview_text(&panes[0].pane_id);
    }

    let mut captures = HashMap::new();
    for pane in &panes {
        if let Ok(text) = capture_preview_text(&pane.pane_id) {
            captures.insert(pane.pane_id.clone(), text);
        }
    }

    Ok(compose_window_preview_text(&panes, &captures, area))
}

#[derive(Clone)]
struct PreviewCell {
    symbol: char,
    style: Style,
}

impl Default for PreviewCell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: Style::default(),
        }
    }
}

fn compose_window_preview_text(
    panes: &[WindowPreviewPane],
    captures: &HashMap<String, Text<'static>>,
    area: Rect,
) -> Text<'static> {
    if area.width == 0 || area.height == 0 {
        return Text::default();
    }

    let width = area.width as usize;
    let height = area.height as usize;
    let mut cells = vec![vec![PreviewCell::default(); width]; height];
    let pane_rects = geometry_preview_rects(panes, area);
    for (pane, rect) in panes.iter().zip(pane_rects) {
        draw_preview_pane(&mut cells, rect, pane, captures.get(&pane.pane_id));
    }

    preview_cells_to_text(cells)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn geometry_preview_rects(panes: &[WindowPreviewPane], area: Rect) -> Vec<PaneRect> {
    if panes.is_empty() || area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let source_left = panes.iter().map(|pane| pane.left).min().unwrap_or(0);
    let source_top = panes.iter().map(|pane| pane.top).min().unwrap_or(0);
    let source_right = panes
        .iter()
        .map(|pane| pane.left.saturating_add(pane.width))
        .max()
        .unwrap_or(source_left)
        .max(source_left.saturating_add(1));
    let source_bottom = panes
        .iter()
        .map(|pane| pane.top.saturating_add(pane.height))
        .max()
        .unwrap_or(source_top)
        .max(source_top.saturating_add(1));
    let source_width = source_right.saturating_sub(source_left).max(1);
    let source_height = source_bottom.saturating_sub(source_top).max(1);

    panes
        .iter()
        .map(|pane| {
            let left = scale_position(
                pane.left.saturating_sub(source_left),
                source_width,
                area.width,
            );
            let top = scale_position(
                pane.top.saturating_sub(source_top),
                source_height,
                area.height,
            );
            let right = scale_position(
                pane.left
                    .saturating_add(pane.width)
                    .saturating_sub(source_left),
                source_width,
                area.width,
            )
            .max(left.saturating_add(3))
            .min(area.width);
            let bottom = scale_position(
                pane.top
                    .saturating_add(pane.height)
                    .saturating_sub(source_top),
                source_height,
                area.height,
            )
            .max(top.saturating_add(3))
            .min(area.height);

            PaneRect {
                x: left as usize,
                y: top as usize,
                width: right.saturating_sub(left) as usize,
                height: bottom.saturating_sub(top) as usize,
            }
        })
        .collect()
}

fn scale_position(value: u16, source: u16, target: u16) -> u16 {
    if source == 0 || target == 0 {
        return 0;
    }

    ((value as u32 * target as u32) / source as u32) as u16
}

fn draw_preview_pane(
    cells: &mut [Vec<PreviewCell>],
    rect: PaneRect,
    pane: &WindowPreviewPane,
    capture: Option<&Text<'static>>,
) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }

    let border_style = if pane.pane_active {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let x2 = rect.x + rect.width - 1;
    let y2 = rect.y + rect.height - 1;

    put_preview_cell(cells, rect.x, rect.y, '┌', border_style);
    put_preview_cell(cells, x2, rect.y, '┐', border_style);
    put_preview_cell(cells, rect.x, y2, '└', border_style);
    put_preview_cell(cells, x2, y2, '┘', border_style);
    for x in rect.x + 1..x2 {
        put_preview_cell(cells, x, rect.y, '─', border_style);
        put_preview_cell(cells, x, y2, '─', border_style);
    }
    for y in rect.y + 1..y2 {
        put_preview_cell(cells, rect.x, y, '│', border_style);
        put_preview_cell(cells, x2, y, '│', border_style);
    }

    let marker = if pane.pane_active { "*" } else { " " };
    let command = if pane.pane_current_command.is_empty() {
        pane.pane_title.as_str()
    } else {
        pane.pane_current_command.as_str()
    };
    let title = format!("{marker}{} {command}", pane.pane_index);
    for (offset, ch) in title.chars().take(rect.width.saturating_sub(2)).enumerate() {
        put_preview_cell(cells, rect.x + 1 + offset, rect.y, ch, border_style);
    }

    if let Some(capture) = capture {
        let content = preview_text_to_cells(capture);
        let inner_width = rect.width.saturating_sub(2);
        let inner_height = rect.height.saturating_sub(2);
        for (line_index, line) in content.iter().take(inner_height).enumerate() {
            for (column, cell) in line.iter().take(inner_width).enumerate() {
                put_preview_cell(
                    cells,
                    rect.x + 1 + column,
                    rect.y + 1 + line_index,
                    cell.symbol,
                    cell.style,
                );
            }
        }
    }
}

fn put_preview_cell(
    cells: &mut [Vec<PreviewCell>],
    x: usize,
    y: usize,
    symbol: char,
    style: Style,
) {
    if let Some(row) = cells.get_mut(y) {
        if let Some(cell) = row.get_mut(x) {
            cell.symbol = symbol;
            cell.style = style;
        }
    }
}

fn preview_text_to_cells(text: &Text<'static>) -> Vec<Vec<PreviewCell>> {
    text.lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .flat_map(|span| {
                    span.content.chars().map(|symbol| PreviewCell {
                        symbol,
                        style: span.style,
                    })
                })
                .collect()
        })
        .collect()
}

fn preview_cells_to_text(cells: Vec<Vec<PreviewCell>>) -> Text<'static> {
    let lines = cells
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for cell in row {
                if let Some(last) = spans.last_mut() {
                    if last.style == cell.style {
                        last.content.to_mut().push(cell.symbol);
                        continue;
                    }
                }
                spans.push(Span::styled(cell.symbol.to_string(), cell.style));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    Text::from(lines)
}

fn ansi_preview_text(output: &str) -> Result<Text<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = output.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                flush_preview_span(&mut spans, &mut text, style);
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            '\r' => {}
            '\u{1b}' => {
                flush_preview_span(&mut spans, &mut text, style);
                match chars.peek().copied() {
                    Some('[') => {
                        chars.next();
                        let mut sequence = String::new();
                        for next in chars.by_ref() {
                            let done = ('@'..='~').contains(&next);
                            sequence.push(next);
                            if done {
                                break;
                            }
                        }
                        if sequence.ends_with('m') {
                            style = apply_sgr_sequence(style, &sequence[..sequence.len() - 1]);
                        }
                    }
                    Some(']') => {
                        chars.next();
                        let mut previous_was_escape = false;
                        for next in chars.by_ref() {
                            if next == '\u{7}' || (previous_was_escape && next == '\\') {
                                break;
                            }
                            previous_was_escape = next == '\u{1b}';
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            // A literal tab would make the terminal jump to the next tab stop
            // while ratatui counts it as one cell, desyncing every cell after
            // it on the line — smearing content across the modal.
            '\t' => text.push(' '),
            _ if ch.is_control() => {}
            _ => text.push(ch),
        }
    }

    flush_preview_span(&mut spans, &mut text, style);
    lines.push(Line::from(spans));
    Ok(Text::from(lines))
}

fn flush_preview_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn apply_sgr_sequence(mut style: Style, sequence: &str) -> Style {
    let mut codes = sequence
        .split(';')
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .peekable();

    if sequence.is_empty() {
        return Style::default();
    }

    while let Some(code) = codes.next() {
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(basic_ansi_color(code - 30)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(basic_ansi_color(code - 40)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(bright_ansi_color(code - 90)),
            100..=107 => style.bg = Some(bright_ansi_color(code - 100)),
            38 => {
                if let Some(color) = parse_extended_ansi_color(&mut codes) {
                    style.fg = Some(color);
                }
            }
            48 => {
                if let Some(color) = parse_extended_ansi_color(&mut codes) {
                    style.bg = Some(color);
                }
            }
            _ => {}
        }
    }

    style
}

fn parse_extended_ansi_color(
    codes: &mut std::iter::Peekable<impl Iterator<Item = u16>>,
) -> Option<Color> {
    match codes.next()? {
        5 => codes.next().map(|value| Color::Indexed(value as u8)),
        2 => {
            let red = codes.next()? as u8;
            let green = codes.next()? as u8;
            let blue = codes.next()? as u8;
            Some(Color::Rgb(red, green, blue))
        }
        _ => None,
    }
}

fn basic_ansi_color(index: u16) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn bright_ansi_color(index: u16) -> Color {
    match index {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

pub fn normalize_preview_line(line: &str) -> String {
    strip_ansi_and_controls(line).trim().to_owned()
}

fn strip_ansi_and_controls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut previous_was_escape = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_was_escape && next == '\\') {
                            break;
                        }
                        previous_was_escape = next == '\u{1b}';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }

        if ch == '\t' {
            out.push(' ');
            continue;
        }
        if ch.is_control() {
            continue;
        }

        out.push(ch);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_text_lines(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn preview_lines_are_trimmed_before_rendering() {
        assert_eq!(
            normalize_preview_line("   GET /api/search 200    "),
            "GET /api/search 200"
        );
        assert_eq!(
            normalize_preview_line("\tworker finished\t"),
            "worker finished"
        );
    }

    #[test]
    fn preview_lines_strip_ansi_and_control_sequences() {
        assert_eq!(
            normalize_preview_line("\u{1b}[32mINFO\u{1b}[0m service ready\u{7}"),
            "INFO service ready"
        );
        assert_eq!(
            normalize_preview_line("before\u{1b}]0;title\u{7}after"),
            "beforeafter"
        );
    }

    #[test]
    fn preview_tabs_become_spaces_so_cells_stay_aligned() {
        // A raw tab would move the real cursor to the next tab stop while
        // ratatui budgets one cell, desyncing everything after it.
        assert_eq!(normalize_preview_line("col1\tcol2"), "col1 col2");

        let text = ansi_preview_text("a\tb").unwrap();
        assert_eq!(preview_text_lines(&text)[0], "a b");
    }

    #[test]
    fn preview_capture_args_preserve_visible_ansi_and_spacing() {
        assert_eq!(
            preview_capture_args("%42"),
            vec!["capture-pane", "-epN", "-t", "%42"]
        );
    }

    #[test]
    fn parses_window_preview_panes_with_geometry() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t40\t10\teditor\tnvim\n%2\t2\t0\t40\t0\t40\t10\tagent\tcodex\n",
        )
        .unwrap();

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert!(panes[0].pane_active);
        assert_eq!(panes[0].left, 0);
        assert_eq!(panes[1].left, 40);
        assert_eq!(panes[1].width, 40);
        assert_eq!(panes[1].height, 10);
    }

    #[test]
    fn composes_side_by_side_panes_into_window_preview() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t10\t4\teditor\tnvim\n%2\t2\t0\t10\t0\t10\t4\tagent\tcodex\n",
        )
        .unwrap();
        let captures = HashMap::from([
            (
                "%1".to_owned(),
                ansi_preview_text("left one\nleft two").unwrap(),
            ),
            (
                "%2".to_owned(),
                ansi_preview_text("right one\nright two").unwrap(),
            ),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 20, 4));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 nvim─┐┌ 2 codex┐");
        assert_eq!(rendered[1], "│left one││right on│");
        assert_eq!(rendered[2], "│left two││right tw│");
        assert_eq!(rendered[3], "└────────┘└────────┘");
    }

    #[test]
    fn composes_stacked_panes_into_window_preview() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t12\t3\ttop\tzsh\n%2\t2\t0\t0\t3\t12\t3\tbottom\tcodex\n",
        )
        .unwrap();
        let captures = HashMap::from([
            ("%1".to_owned(), ansi_preview_text("top line").unwrap()),
            ("%2".to_owned(), ansi_preview_text("bottom").unwrap()),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 12, 6));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 zsh────┐");
        assert_eq!(rendered[1], "│top line  │");
        assert_eq!(rendered[2], "└──────────┘");
        assert_eq!(rendered[3], "┌ 2 codex──┐");
        assert_eq!(rendered[4], "│bottom    │");
        assert_eq!(rendered[5], "└──────────┘");
    }

    #[test]
    fn composes_three_panes_with_left_half_and_right_quarters() {
        let panes = parse_window_preview_panes(
            "%1\t1\t1\t0\t0\t20\t10\tone\tzsh\n%2\t2\t0\t20\t0\t20\t5\ttwo\tzsh\n%3\t3\t0\t20\t5\t20\t5\tthree\tzsh\n",
        )
        .unwrap();
        let captures = HashMap::from([
            ("%1".to_owned(), ansi_preview_text("one").unwrap()),
            ("%2".to_owned(), ansi_preview_text("two").unwrap()),
            ("%3".to_owned(), ansi_preview_text("three").unwrap()),
        ]);

        let text = compose_window_preview_text(&panes, &captures, Rect::new(0, 0, 30, 6));
        let rendered = preview_text_lines(&text);

        assert_eq!(rendered[0], "┌*1 zsh───────┐┌ 2 zsh───────┐");
        assert_eq!(rendered[1], "│one          ││two          │");
        assert_eq!(rendered[2], "│             │└─────────────┘");
        assert_eq!(rendered[3], "│             │┌ 3 zsh───────┐");
        assert_eq!(rendered[4], "│             ││three        │");
        assert_eq!(rendered[5], "└─────────────┘└─────────────┘");
    }

    #[test]
    fn ansi_preview_text_preserves_color_and_spacing() {
        let text = ansi_preview_text("\u{1b}[31m  red  \u{1b}[0m\nplain").unwrap();

        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans[0].content.as_ref(), "  red  ");
        assert_eq!(text.lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(text.lines[1].spans[0].content.as_ref(), "plain");
    }

    #[test]
    fn preview_mirror_refreshes_on_new_window_area_or_after_interval() {
        let now = Instant::now();
        let mut mirror = PreviewMirror::default();
        let area = Rect::new(28, 0, 72, 40);

        assert!(mirror.should_refresh("@1", area, now));
        mirror.record_success("@1", area, ansi_preview_text("one").unwrap(), now);
        assert!(!mirror.should_refresh("@1", area, now + Duration::from_millis(99)));
        assert!(mirror.should_refresh("@1", area, now + PREVIEW_REFRESH_INTERVAL));
        assert!(mirror.should_refresh("@2", area, now + Duration::from_millis(10)));
        assert!(mirror.should_refresh(
            "@1",
            Rect::new(28, 0, 80, 40),
            now + Duration::from_millis(10)
        ));
    }
}

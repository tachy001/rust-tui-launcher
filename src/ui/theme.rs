//! Color palette, typography styles, and reusable Ratatui layout/widget primitives.
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
    Frame,
};

// ── Launcher Color Palette (Deep Slate + Minecraft Emerald) ────────────────
pub const CANVAS_BG: Color = Color::Rgb(14, 17, 24);
pub const PANEL_BG: Color = Color::Rgb(20, 24, 35);
pub const CARD_BG: Color = Color::Rgb(27, 33, 48);
pub const BAR_BG: Color = Color::Rgb(23, 28, 41);
pub const SEL_BG: Color = Color::Rgb(35, 53, 68);
pub const SEL_INACTIVE_BG: Color = Color::Rgb(29, 36, 52);

pub const BORDER: Color = Color::Rgb(50, 60, 84);
pub const BORDER_SUBTLE: Color = Color::Rgb(38, 46, 66);
pub const BORDER_FOCUS: Color = Color::Rgb(98, 218, 150);

pub const TEXT: Color = Color::Rgb(226, 233, 248);
pub const SUBTEXT: Color = Color::Rgb(168, 179, 207);
pub const MUTED: Color = Color::Rgb(118, 130, 162);
pub const DIM: Color = Color::Rgb(76, 86, 114);

pub const ACCENT: Color = Color::Rgb(98, 218, 150);
pub const INFO: Color = Color::Rgb(122, 176, 252);
pub const WARN: Color = Color::Rgb(242, 192, 106);
pub const ORANGE: Color = Color::Rgb(240, 146, 96);
pub const DANGER: Color = Color::Rgb(244, 116, 138);
pub const PURPLE: Color = Color::Rgb(190, 152, 250);

pub fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub fn pill(fg_color: Color, bg_color: Color) -> Style {
    Style::default()
        .fg(fg_color)
        .bg(bg_color)
        .add_modifier(Modifier::BOLD)
}

pub fn row_highlight(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(SEL_BG)
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(SEL_INACTIVE_BG).fg(TEXT)
    }
}

pub fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

/// Truncate `text` to fit within `max_cols` terminal display columns, adding `…` when clipped.
pub fn clip_str(text: &str, max_cols: usize) -> String {
    if display_width(text) <= max_cols {
        return text.to_owned();
    }
    if max_cols <= 1 {
        return "…".repeat(max_cols);
    }
    let budget = max_cols - 1;
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let w = display_width(&ch.to_string());
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Keep the input tail visible using terminal display columns, including CJK.
pub fn input_tail(text: &str, width: u16) -> String {
    let mut used = 0;
    let mut tail = Vec::new();
    for c in text.chars().rev() {
        let cell_width = display_width(&c.to_string());
        if used + cell_width > usize::from(width) {
            break;
        }
        tail.push(c);
        used += cell_width;
    }
    tail.into_iter().rev().collect()
}

/// Pad `text` on the right to `cols` display columns (clipping if wider).
pub fn pad_right(text: &str, cols: usize) -> String {
    let clipped = clip_str(text, cols);
    let w = display_width(&clipped);
    format!("{clipped}{}", " ".repeat(cols.saturating_sub(w)))
}

/// Elevated rounded panel card with focus-aware border & title styling.
pub fn panel_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let border_color = if focused { BORDER_FOCUS } else { BORDER };
    let title_color = if focused { ACCENT } else { TEXT };
    let marker = if focused { "◆ " } else { "◇ " };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(border_color))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(marker, fg(border_color)),
            Span::styled(title.into(), bold(title_color)),
            Span::raw(" "),
        ]))
        .style(Style::default().bg(PANEL_BG).fg(TEXT))
}

/// Subtle inner card block for grouping hero stats or sub-sections.
pub fn card_block(title: impl Into<String>, accent: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(BORDER_SUBTLE))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.into(), bold(accent)),
            Span::raw(" "),
        ]))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(CARD_BG).fg(TEXT))
}

/// Horizontal divider line spanning `width` columns.
pub fn divider(width: u16) -> Line<'static> {
    Line::styled("─".repeat(width as usize), fg(BORDER_SUBTLE))
}

/// Render a centered modal frame with backdrop clear; returns the inner content rect.
pub fn render_modal(
    f: &mut Frame,
    area: Rect,
    size: (u16, u16),
    title: impl Into<String>,
    accent: Color,
    footer: Option<Line<'static>>,
) -> Rect {
    let rect = centered_rect(area, size.0, size.1);
    f.render_widget(Clear, rect);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(accent))
        .title(Line::from(vec![
            Span::styled(" ◆ ", bold(accent)),
            Span::styled(title.into(), bold(TEXT)),
            Span::raw(" "),
        ]))
        .padding(Padding::new(2, 2, 1, 0))
        .style(Style::default().bg(CARD_BG).fg(TEXT));
    if let Some(footer) = footer {
        block = block.title_bottom(footer.right_aligned());
    }
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    inner
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

/// Split `area` horizontally with a 1-column visual gap when wide enough.
pub fn split_columns(area: Rect, left_pct: u16, min_dual_width: u16) -> (Rect, Option<Rect>) {
    if area.width >= min_dual_width {
        let [left, _gap, right] = Layout::horizontal([
            Constraint::Percentage(left_pct),
            Constraint::Length(1),
            Constraint::Min(32),
        ])
        .areas(area);
        (left, Some(right))
    } else {
        (area, None)
    }
}

/// Keycap badge sequence: ` Key ` Label  ` Key ` Label.
pub fn key_chips(items: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (idx, (key, label)) in items.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .bg(CARD_BG)
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), fg(SUBTEXT)));
    }
    spans
}

/// Smooth unicode progress bar spans of `bar_width` cells.
pub fn progress_bar(ratio: f64, bar_width: usize, fill_color: Color) -> Vec<Span<'static>> {
    let clamped = ratio.clamp(0.0, 1.0);
    let eighths = (clamped * (bar_width as f64) * 8.0).round() as usize;
    let full = (eighths / 8).min(bar_width);
    let rem = eighths % 8;
    let partial = match rem {
        1 => "▏",
        2 => "▎",
        3 => "▍",
        4 => "▌",
        5 => "▋",
        6 => "▊",
        7 => "▉",
        _ => "",
    };
    let used = full + usize::from(rem > 0 && full < bar_width);
    let empty = bar_width.saturating_sub(used);
    let mut spans = vec![Span::styled("█".repeat(full), fg(fill_color))];
    if rem > 0 && full < bar_width {
        spans.push(Span::styled(partial, fg(fill_color)));
    }
    if empty > 0 {
        spans.push(Span::styled("─".repeat(empty), fg(DIM)));
    }
    spans
}

/// Render a single-line toolbar header + bottom divider inside a panel's inner rect,
/// returning the remaining body rect below the divider.
pub fn render_panel_toolbar(
    f: &mut Frame,
    inner: Rect,
    left: Line<'static>,
    right: Option<Line<'static>>,
) -> Rect {
    if inner.height < 3 {
        return inner;
    }
    let [bar, rule, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);
    if let Some(right) = right {
        if left.width() + right.width() + 2 <= bar.width as usize {
            f.render_widget(Paragraph::new(right.right_aligned()), bar);
        }
    }
    f.render_widget(Paragraph::new(left), bar);
    f.render_widget(Paragraph::new(divider(rule.width)), rule);
    rest
}

pub fn loader_badge(id: &str) -> (&'static str, Color) {
    let lower = id.to_ascii_lowercase();
    if lower.contains("neoforge") {
        ("NeoForge", ORANGE)
    } else if lower.contains("forge") {
        ("Forge", ORANGE)
    } else if lower.contains("fabric") {
        ("Fabric", WARN)
    } else if lower.contains("quilt") {
        ("Quilt", PURPLE)
    } else if lower.contains("optifine") {
        ("OptiFine", INFO)
    } else {
        (crate::localization::tr("原版"), ACCENT)
    }
}

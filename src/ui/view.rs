//! Pure view layer for the redesigned TUI Launcher.
use super::{
    theme::{
        bold, card_block, clip_str, display_width, divider, fg, input_tail, key_chips,
        loader_badge, pad_right, panel_block, pill, progress_bar, render_modal,
        render_panel_toolbar, row_highlight, split_columns, ACCENT, BAR_BG, BORDER, BORDER_SUBTLE,
        CANVAS_BG, CARD_BG, DANGER, DIM, INFO, MUTED, ORANGE, PANEL_BG, SUBTEXT, TEXT, WARN,
    },
    App, EditKind, HitTarget, PaneFocus, PrimaryPage, WorkbenchTab, ACCOUNT_ACTIONS,
    DOWNLOAD_ACTIONS, INSTANCE_SETTING_INDICES, LOADER_ITEMS, OVERVIEW_ACTIONS, SETTINGS,
    SETTINGS_CATEGORIES,
};
use crate::{accounts::AccountKind, localization::tr};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Gauge, List, ListItem, ListState, Padding, Paragraph, Wrap,
    },
    Frame,
};
use std::time::Duration;

// Re-export helper trait/functions needed internally
trait AccountExt {
    fn active_summary(&self, fallback_user: &str) -> (String, &'static str, bool);
}
impl AccountExt for crate::accounts::Accounts {
    fn active_summary(&self, fallback_user: &str) -> (String, &'static str, bool) {
        if let Some(acc) = self.active() {
            let is_ms = acc.kind == AccountKind::Microsoft;
            (
                acc.name.clone(),
                if is_ms { tr("正版") } else { tr("离线") },
                is_ms,
            )
        } else {
            (fallback_user.to_owned(), tr("离线"), false)
        }
    }
}

fn instance_display(app: &App, id: &str) -> String {
    app.names.get(id).cloned().unwrap_or_else(|| id.to_owned())
}

fn short_setting_label(raw: &str) -> String {
    let translated = tr(raw);
    translated
        .split(['（', '('])
        .next()
        .unwrap_or(translated)
        .trim()
        .to_owned()
}

fn setting_hint(raw: &str) -> Option<String> {
    let translated = tr(raw);
    let (_, rest) = translated.split_once(['（', '('])?;
    Some(rest.trim_end_matches(['）', ')']).trim().to_owned())
}

fn is_bool_setting(index: usize) -> bool {
    matches!(index, 4 | 12 | 13 | 18 | 19)
}

fn log_line_color(line: &str) -> Color {
    let lower = line.to_ascii_lowercase();
    if line.contains("失败")
        || line.contains("错误")
        || lower.contains("error")
        || lower.contains("fail")
    {
        DANGER
    } else if line.contains("警告") || lower.contains("warn") {
        WARN
    } else if line.contains("完成") || line.contains("已就绪") || line.contains("已保存") {
        ACCENT
    } else {
        TEXT
    }
}

fn progress_ratio(app: &App) -> (f64, bool) {
    let (_, done, total) = &app.progress;
    if let Some((transfer, _)) = &app.transfer {
        match transfer.total.filter(|t| *t > 0) {
            Some(t) => ((transfer.done as f64 / t as f64).clamp(0.0, 1.0), true),
            None => (0.0, false),
        }
    } else if *total == 0 {
        (0.0, false)
    } else {
        ((*done as f64 / *total as f64).clamp(0.0, 1.0), true)
    }
}

fn progress_detail(app: &App) -> String {
    let (label, done, total) = &app.progress;
    if let Some((transfer, updated)) = &app.transfer {
        if !app.paused && updated.elapsed() > Duration::from_secs(3) {
            tformat!("{} · 等待数据或校验中", transfer.label)
        } else {
            transfer.display(app.paused)
        }
    } else if *total > 0 {
        format!("{label} ({done}/{total})")
    } else {
        label.clone()
    }
}

// ── Main Frame Entry ───────────────────────────────────────────────────────

pub(super) fn draw(f: &mut Frame, app: &App) {
    crate::localization::set(&app.config.language);
    app.hits.borrow_mut().clear();

    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(CANVAS_BG).fg(TEXT)),
        area,
    );

    if area.width < 48 || area.height < 16 {
        f.render_widget(
            Paragraph::new(tr("请将终端调整至至少 48 × 16\nQ 空闲时退出"))
                .style(fg(ACCENT))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let outer = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };

    let top_h = if outer.height >= 22 { 3 } else { 1 };
    let dock_h = if outer.height >= 22 { 4 } else { 2 };
    let [top_area, body_area, dock_area] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Min(8),
        Constraint::Length(dock_h),
    ])
    .areas(outer);

    draw_top_bar(f, app, top_area);
    match app.page {
        PrimaryPage::Play => draw_play(f, app, body_area),
        PrimaryPage::Downloads => draw_downloads(f, app, body_area),
        PrimaryPage::Accounts => draw_accounts(f, app, body_area),
        PrimaryPage::Settings => draw_settings(f, app, body_area),
        PrimaryPage::Logs => draw_logs(f, app, body_area),
    }
    draw_bottom_dock(f, app, dock_area);
    draw_modals(f, app, area);
}

// ── Top Navigation Bar ─────────────────────────────────────────────────────

fn draw_top_bar(f: &mut Frame, app: &App, area: Rect) {
    let inner = if area.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fg(BORDER))
            .style(Style::default().bg(BAR_BG).fg(TEXT));
        let inner = block.inner(area);
        f.render_widget(block, area);
        inner
    } else {
        f.render_widget(Block::default().style(Style::default().bg(BAR_BG)), area);
        area
    };

    // Build brand + tabs and track exact hit rects
    let compact = inner.width < 104;
    let brand = if compact {
        " ◆ TUI-MC ".to_owned()
    } else {
        format!(" ◆ TUI-MC v{}  ", env!("CARGO_PKG_VERSION"))
    };
    let mut x_cursor = inner.x + display_width(&brand) as u16;
    let mut left_spans = vec![Span::styled(" ◆ TUI-MC ", bold(ACCENT))];
    if !compact {
        left_spans.push(Span::styled(
            format!("v{}  ", env!("CARGO_PKG_VERSION")),
            fg(DIM),
        ));
    }

    for page in PrimaryPage::ALL {
        let num = page.index() + 1;
        let name = tr(if compact {
            page.short_label()
        } else {
            page.label()
        });
        let tab_text = format!(" {num} {name} ");
        let w = display_width(&tab_text) as u16;
        if x_cursor + w <= inner.x + inner.width {
            app.hits
                .borrow_mut()
                .push((Rect::new(x_cursor, inner.y, w, 1), HitTarget::Page(page)));
        }
        if app.page == page {
            left_spans.push(Span::styled(tab_text, pill(CANVAS_BG, ACCENT)));
        } else {
            left_spans.push(Span::styled(format!(" {num} "), bold(ACCENT).bg(CARD_BG)));
            left_spans.push(Span::styled(format!("{name} "), fg(SUBTEXT).bg(CARD_BG)));
        }
        left_spans.push(Span::raw(" "));
        x_cursor = x_cursor.saturating_add(w + 1);
    }

    let left_line = Line::from(left_spans);
    let avail_right = inner.width.saturating_sub(left_line.width() as u16 + 1) as usize;

    // Right status pills: active account + optional active instance
    let (acc_name, acc_kind, is_ms) = app.accounts.active_summary(&app.config.username);
    let mut acc_spans = vec![
        Span::styled(" ● ", bold(if is_ms { INFO } else { ACCENT })),
        Span::styled(clip_str(&acc_name, 14), bold(TEXT)),
        Span::styled(format!(" ({acc_kind}) "), fg(MUTED)),
    ];
    if let Some(id) = app.active_instance() {
        let inst_name = clip_str(&instance_display(app, &id), 16);
        let mut full_spans = acc_spans.clone();
        full_spans.pop();
        full_spans.push(Span::styled(format!(" ({acc_kind})"), fg(MUTED)));
        full_spans.push(Span::styled("  ▪ ", fg(INFO)));
        full_spans.push(Span::styled(format!("{inst_name} "), fg(SUBTEXT)));
        let full_line = Line::from(full_spans);
        if full_line.width() <= avail_right {
            f.render_widget(Paragraph::new(full_line.right_aligned()), inner);
            f.render_widget(Paragraph::new(left_line), inner);
            return;
        }
    }
    let acc_line = Line::from(std::mem::take(&mut acc_spans));
    if acc_line.width() <= avail_right {
        f.render_widget(Paragraph::new(acc_line.right_aligned()), inner);
    }
    f.render_widget(Paragraph::new(left_line), inner);
}

// ── Stateful List Helper with Mouse Hit Registration ───────────────────────

struct SelectList<'a> {
    area: Rect,
    pane: PaneFocus,
    selected: usize,
    row_height: u16,
    focused: bool,
    empty: &'a [Line<'static>],
}

fn render_selectable_list(
    f: &mut Frame,
    app: &App,
    spec: SelectList<'_>,
    items: Vec<ListItem<'static>>,
) {
    if items.is_empty() {
        app.hits
            .borrow_mut()
            .push((spec.area, HitTarget::FocusPane(spec.pane)));
        let mut padded = vec![Line::raw("")];
        padded.extend_from_slice(spec.empty);
        f.render_widget(
            Paragraph::new(Text::from(padded)).wrap(Wrap { trim: false }),
            spec.area,
        );
        return;
    }
    let count = items.len();
    let sel = spec.selected.min(count - 1);
    let mut state = ListState::default().with_selected(Some(sel));
    f.render_stateful_widget(
        List::new(items)
            .highlight_style(row_highlight(spec.focused))
            .highlight_symbol(if spec.focused { "▸ " } else { "  " }),
        spec.area,
        &mut state,
    );
    app.hits.borrow_mut().push((
        spec.area,
        HitTarget::ListArea {
            pane: spec.pane,
            offset: state.offset(),
            row_height: spec.row_height,
            count,
        },
    ));
}

// ── Page 1: Play & Instance Workbench ──────────────────────────────────────

fn draw_play(f: &mut Frame, app: &App, area: Rect) {
    let (left_col, right_col) = split_columns(area, 42, 90);
    if let Some(right_area) = right_col {
        draw_instance_list_pane(f, app, left_col, app.focus == PaneFocus::Left);
        draw_workbench_pane(f, app, right_area, app.focus == PaneFocus::Right);
    } else if app.focus == PaneFocus::Left {
        draw_instance_list_pane(f, app, left_col, true);
    } else {
        draw_workbench_pane(f, app, left_col, true);
    }
}

fn draw_instance_list_pane(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let visible = app.local_visible();
    let count = visible.len();
    let pos_tag = if count > 0 {
        format!(" {}/{} ", app.instance_sel.min(count - 1) + 1, count)
    } else {
        " 0/0 ".into()
    };
    let block = panel_block(format!("{} ({count})", tr("实例列表")), focused)
        .title_bottom(Line::styled(pos_tag, fg(MUTED)).right_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let filter_left = if app.instance_query.is_empty() {
        Line::from(vec![
            Span::styled(" / ", bold(ACCENT).bg(CARD_BG)),
            Span::styled(format!(" {} ", tr("搜索筛选")), fg(MUTED)),
            Span::styled("  I ", bold(ACCENT).bg(CARD_BG)),
            Span::styled(format!(" {}", tr("导入整合包")), fg(MUTED)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" 筛选: ", fg(WARN)),
            Span::styled(app.instance_query.clone(), bold(WARN)),
            Span::styled(format!("  (/ {})", tr("修改")), fg(MUTED)),
        ])
    };
    let list_area = render_panel_toolbar(
        f,
        inner,
        filter_left,
        Some(Line::styled(tr("Space 启动"), fg(ACCENT))),
    );

    let row_w = list_area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = visible
        .iter()
        .map(|id| {
            let title = instance_display(app, id);
            let (loader, l_color) = loader_badge(id);
            let badge = format!("[{loader}]");
            let is_last = app.config.last_version.as_deref() == Some(id.as_str());
            let star = if is_last { "★ " } else { "" };
            let title_budget =
                row_w.saturating_sub(display_width(&badge) + display_width(star) + 2);
            let clipped_title = clip_str(&title, title_budget);
            let gap = row_w.saturating_sub(
                display_width(star) + display_width(&clipped_title) + display_width(&badge),
            );

            let line1 = Line::from(vec![
                Span::styled(star, bold(WARN)),
                Span::styled(clipped_title, bold(TEXT)),
                Span::raw(" ".repeat(gap)),
                Span::styled(badge, bold(l_color)),
            ]);

            let meta = match app.config.effective(id) {
                Ok(cfg) => {
                    let mem = if cfg.auto_memory {
                        format!("Auto≤{}M", cfg.memory_mb)
                    } else {
                        format!("{} MB", cfg.memory_mb)
                    };
                    let iso = if cfg.isolated {
                        tr("独立目录")
                    } else {
                        tr("共享目录")
                    };
                    if app.names.contains_key(*id) {
                        format!("  {id} · {mem} · {iso}")
                    } else {
                        format!("  {mem} · {iso}")
                    }
                }
                Err(_) => format!("  {id}"),
            };
            let line2 = Line::styled(clip_str(&meta, row_w), fg(MUTED));
            ListItem::new(vec![line1, line2])
        })
        .collect();

    let empty = if !app.instance_query.is_empty() {
        vec![
            Line::styled(format!(" {}", tr("没有匹配的本地实例。")), bold(WARN)),
            Line::styled(
                format!(" {}", tr("按 / 打开搜索框，Ctrl+U 清空过滤条件。")),
                fg(MUTED),
            ),
        ]
    } else {
        vec![
            Line::styled(format!(" {}", tr("还没有安装任何游戏实例。")), bold(TEXT)),
            Line::raw(""),
            Line::styled(
                format!(" • {}", tr("按 2 前往「下载中心」获取并安装官方版本")),
                fg(SUBTEXT),
            ),
            Line::styled(
                format!(" • {}", tr("按 I 直接导入 .mrpack 或 CurseForge 整合包")),
                fg(SUBTEXT),
            ),
            Line::styled(
                format!(
                    " • {}",
                    tr("按 4 在「全局设置」中修改已有的 .minecraft 目录")
                ),
                fg(SUBTEXT),
            ),
        ]
    };

    render_selectable_list(
        f,
        app,
        SelectList {
            area: list_area,
            pane: PaneFocus::Left,
            selected: app.instance_sel,
            row_height: 2,
            focused,
            empty: &empty,
        },
        items,
    );
}

fn draw_workbench_pane(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let active_id = app.active_instance();
    let header_title = match &active_id {
        Some(id) => tformat!("实例工作台 · {}", instance_display(app, id)),
        None => tr("实例工作台").into(),
    };
    let bottom_hint = if focused {
        tr(" ←→ 切换子标签 · Esc 返回实例列表 ")
    } else {
        tr(" Enter / → 进入工作台 ")
    };
    let block = panel_block(header_title, focused)
        .title_bottom(Line::styled(bottom_hint, fg(MUTED)).right_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hits
        .borrow_mut()
        .push((inner, HitTarget::FocusPane(PaneFocus::Right)));

    let Some(id) = active_id else {
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    format!(" {}", tr("请先在左侧实例列表选择或安装一个实例。")),
                    fg(MUTED),
                ),
            ]),
            inner,
        );
        return;
    };

    // Split workbench vertically: Hero Summary Card (5 rows if tall) + Sub-tab bar (2 rows) + Content
    let show_hero = inner.height >= 16;
    let content_area = if show_hero {
        let [hero_area, tabs_area, body] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(2),
            Constraint::Min(4),
        ])
        .areas(inner);
        draw_instance_hero_card(f, app, &id, hero_area);
        draw_workbench_subtabs(f, app, tabs_area);
        body
    } else {
        let [tabs_area, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(inner);
        draw_workbench_subtabs(f, app, tabs_area);
        body
    };

    match app.wb_tab {
        WorkbenchTab::Overview => draw_wb_overview(f, app, content_area, focused),
        WorkbenchTab::Settings => draw_wb_settings(f, app, &id, content_area, focused),
        WorkbenchTab::Mods => draw_wb_mods(f, app, content_area, focused),
        WorkbenchTab::Worlds => draw_wb_worlds(f, app, content_area, focused),
        WorkbenchTab::Loaders => draw_wb_loaders(f, app, &id, content_area, focused),
    }
}

fn draw_instance_hero_card(f: &mut Frame, app: &App, id: &str, area: Rect) {
    let card = card_block(tr("当前实例概览"), INFO);
    let inner = card.inner(area);
    f.render_widget(card, area);
    if inner.height == 0 {
        return;
    }

    let name = instance_display(app, id);
    let (loader, l_color) = loader_badge(id);
    let (acc_name, acc_kind, _) = app.accounts.active_summary(&app.config.username);
    let enabled_mods = app.mod_list.iter().filter(|m| m.enabled).count();
    let world_count = app.world_list.len();

    let (mem_str, iso_str) = match app.config.effective(id) {
        Ok(cfg) => (
            if cfg.auto_memory {
                tformat!("自动 (≤{} MB)", cfg.memory_mb)
            } else {
                format!("{} MB", cfg.memory_mb)
            },
            if cfg.isolated {
                tr("独立实例目录")
            } else {
                tr("共享游戏目录")
            },
        ),
        Err(_) => ("--".into(), tr("配置异常")),
    };

    // Row 0: Instance Title + Launch Button
    let btn_label = format!(" ▶ Space {} ", tr("启动游戏"));
    let btn_w = display_width(&btn_label) as u16;
    let row0 = Rect::new(inner.x, inner.y, inner.width, 1);
    if inner.width > btn_w + 10 {
        let btn_rect = Rect::new(inner.x + inner.width - btn_w, inner.y, btn_w, 1);
        app.hits.borrow_mut().push((btn_rect, HitTarget::LaunchNow));
        f.render_widget(
            Paragraph::new(Line::styled(btn_label, pill(CANVAS_BG, ACCENT)).right_aligned()),
            row0,
        );
    }
    let mut title_spans = vec![Span::styled(
        clip_str(&name, inner.width.saturating_sub(btn_w + 4) as usize),
        bold(TEXT),
    )];
    if name != id {
        title_spans.push(Span::styled(format!("  ({id})"), fg(MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(title_spans)), row0);

    if inner.height >= 3 {
        let col_w = (inner.width as usize / 2).saturating_sub(2);
        let left1 = format!("{}: {loader}", tr("加载器"));
        let right1 = format!("{}: {acc_name} · {acc_kind}", tr("账户"));
        let left2 = format!("{}: {mem_str}", tr("内存"));
        let right2 = format!(
            "{iso_str} · {enabled_mods} Mod · {world_count} {}",
            tr("存档")
        );

        let r1 = Line::from(vec![
            Span::styled(pad_right(&left1, col_w), fg(l_color)),
            Span::raw("  "),
            Span::styled(clip_str(&right1, col_w), fg(SUBTEXT)),
        ]);
        let r2 = Line::from(vec![
            Span::styled(pad_right(&left2, col_w), fg(SUBTEXT)),
            Span::raw("  "),
            Span::styled(clip_str(&right2, col_w), fg(MUTED)),
        ]);
        f.render_widget(
            Paragraph::new(vec![r1, r2]),
            Rect::new(inner.x, inner.y + 1, inner.width, 2),
        );
    }
}

fn draw_workbench_subtabs(f: &mut Frame, app: &App, area: Rect) {
    let [tab_row, rule_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let mut spans = Vec::new();
    let mut x = tab_row.x;

    for tab in WorkbenchTab::ALL {
        let label = match tab {
            WorkbenchTab::Mods if !app.mod_list.is_empty() => {
                format!(" {} ({}) ", tr(tab.label()), app.mod_list.len())
            }
            WorkbenchTab::Worlds if !app.world_list.is_empty() => {
                format!(" {} ({}) ", tr(tab.label()), app.world_list.len())
            }
            _ => format!(" {} ", tr(tab.label())),
        };
        let w = display_width(&label) as u16;
        if x + w <= tab_row.x + tab_row.width {
            app.hits
                .borrow_mut()
                .push((Rect::new(x, tab_row.y, w, 1), HitTarget::WorkbenchTab(tab)));
        }
        let active = app.wb_tab == tab;
        let style = if active && app.focus == PaneFocus::Right {
            pill(CANVAS_BG, ACCENT)
        } else if active {
            bold(ACCENT).bg(CARD_BG)
        } else {
            fg(MUTED)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        x = x.saturating_add(w + 1);
    }

    f.render_widget(Paragraph::new(Line::from(spans)), tab_row);
    f.render_widget(Paragraph::new(divider(rule_row.width)), rule_row);
}

fn draw_wb_overview(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let w = area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = OVERVIEW_ACTIONS
        .iter()
        .enumerate()
        .map(|(idx, (icon, title, desc))| {
            let title_color = match idx {
                0 => ACCENT,
                8 => DANGER,
                _ => TEXT,
            };
            let left = format!("{icon} {}", tr(title));
            let left_col = 20.min(w / 2);
            let desc_budget = w.saturating_sub(left_col + 2);
            Line::from(vec![
                Span::styled(pad_right(&left, left_col), bold(title_color)),
                Span::raw("  "),
                Span::styled(clip_str(tr(desc), desc_budget), fg(MUTED)),
            ])
            .into()
        })
        .collect();

    render_selectable_list(
        f,
        app,
        SelectList {
            area,
            pane: PaneFocus::Right,
            selected: app.wb_overview_sel,
            row_height: 1,
            focused,
            empty: &[],
        },
        items,
    );
}

fn draw_wb_settings(f: &mut Frame, app: &App, id: &str, area: Rect, focused: bool) {
    let toolbar_area = render_panel_toolbar(
        f,
        area,
        Line::styled(
            tr("实例独立覆盖参数 · 未修改项自动继承全局设置"),
            fg(SUBTEXT),
        ),
        Some(Line::styled(tr("D 重置为全局"), fg(WARN))),
    );
    let w = toolbar_area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = INSTANCE_SETTING_INDICES
        .iter()
        .map(|&idx| {
            let overridden = app.is_instance_overridden(id, idx);
            let value = app.setting_value(Some(id), idx);
            format_setting_row(idx, &value, Some(overridden), w)
        })
        .collect();

    render_selectable_list(
        f,
        app,
        SelectList {
            area: toolbar_area,
            pane: PaneFocus::Right,
            selected: app.wb_setting_sel,
            row_height: 1,
            focused,
            empty: &[],
        },
        items,
    );
}

fn format_setting_row(
    idx: usize,
    value: &str,
    overridden: Option<bool>,
    row_width: usize,
) -> ListItem<'static> {
    let label = short_setting_label(SETTINGS[idx]);
    let (val_str, val_color) = if is_bool_setting(idx) {
        if value == "true" {
            (format!("◉ {}", tr("已开启")), ACCENT)
        } else {
            (format!("○ {}", tr("已关闭")), MUTED)
        }
    } else if value.is_empty() {
        (tr("（未设置）").to_owned(), DIM)
    } else {
        (value.to_owned(), TEXT)
    };

    let scope_tag = match overridden {
        Some(true) => Some((format!("[{}] ", tr("已覆盖")), WARN)),
        Some(false) => Some((format!("[{}] ", tr("继承")), DIM)),
        None => None,
    };
    let tag_w = scope_tag
        .as_ref()
        .map(|(t, _)| display_width(t))
        .unwrap_or(0);
    let right_max = (row_width / 2).max(14);
    let clipped_val = clip_str(&val_str, right_max.saturating_sub(tag_w));
    let label_budget = row_width
        .saturating_sub(tag_w + display_width(&clipped_val) + 2)
        .max(10);
    let clipped_label = clip_str(&label, label_budget);
    let gap = row_width
        .saturating_sub(display_width(&clipped_label) + tag_w + display_width(&clipped_val));

    let mut spans = vec![
        Span::styled(clipped_label, fg(TEXT)),
        Span::raw(" ".repeat(gap)),
    ];
    if let Some((tag, color)) = scope_tag {
        spans.push(Span::styled(tag, fg(color)));
    }
    spans.push(Span::styled(clipped_val, bold(val_color)));
    ListItem::new(Line::from(spans))
}

fn draw_wb_mods(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let w = area.width.saturating_sub(3) as usize;
    if let Some((project, versions)) = &app.mod_versions {
        let body = render_panel_toolbar(
            f,
            area,
            Line::from(vec![
                Span::styled(format!("{} · ", tr("选择兼容版本")), bold(INFO)),
                Span::styled(project.clone(), fg(SUBTEXT)),
            ]),
            Some(Line::styled(tr("Backspace 返回 · L 本地列表"), fg(MUTED))),
        );
        let items = versions
            .iter()
            .map(|ver| {
                let c = match ver.version_type.as_str() {
                    "release" => ACCENT,
                    "beta" => WARN,
                    _ => ORANGE,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(pad_right(&ver.version_number, 18), bold(TEXT)),
                    Span::styled(pad_right(&ver.version_type, 10), fg(c)),
                    Span::styled(
                        ver.date_published.get(..10).unwrap_or("").to_owned(),
                        fg(MUTED),
                    ),
                ]))
            })
            .collect();
        let empty = [Line::styled(format!(" {}", tr("没有兼容版本")), fg(MUTED))];
        render_selectable_list(
            f,
            app,
            SelectList {
                area: body,
                pane: PaneFocus::Right,
                selected: app.mod_sel,
                row_height: 1,
                focused,
                empty: &empty,
            },
            items,
        );
    } else if let Some(search) = &app.mod_search {
        let body = render_panel_toolbar(
            f,
            area,
            Line::from(vec![
                Span::styled("Modrinth · ", bold(ACCENT)),
                Span::styled(
                    format!(
                        "{} ({}/{})",
                        app.mod_query,
                        search.offset.saturating_add(1),
                        search.total_hits
                    ),
                    fg(TEXT),
                ),
            ]),
            Some(Line::styled(
                tr("Enter 安装 · V 选版本 · N/P 翻页 · L 本地"),
                fg(MUTED),
            )),
        );
        let items = search
            .hits
            .iter()
            .map(|hit| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(clip_str(&hit.title, w.saturating_sub(18)), bold(TEXT)),
                        Span::styled(format!("  ({})", clip_str(&hit.slug, 14)), fg(DIM)),
                    ]),
                    Line::styled(
                        format!("  {}", clip_str(&hit.description, w.saturating_sub(2))),
                        fg(MUTED),
                    ),
                ])
            })
            .collect();
        let empty = [Line::styled(
            format!(" {}", tr("没有匹配项目，按 B 修改关键词")),
            fg(MUTED),
        )];
        render_selectable_list(
            f,
            app,
            SelectList {
                area: body,
                pane: PaneFocus::Right,
                selected: app.mod_sel,
                row_height: 2,
                focused,
                empty: &empty,
            },
            items,
        );
    } else {
        let enabled = app.mod_list.iter().filter(|m| m.enabled).count();
        let body = render_panel_toolbar(
            f,
            area,
            Line::styled(
                tformat!(
                    "{enabled} 个启用 · 共 {total} 个",
                    enabled = enabled,
                    total = app.mod_list.len()
                ),
                fg(SUBTEXT),
            ),
            Some(Line::styled(
                tr("A 导入 · B 在线搜索 · U 检查更新"),
                fg(ACCENT),
            )),
        );
        let items = app
            .mod_list
            .iter()
            .map(|entry| {
                let (dot, status_text, color) = if entry.enabled {
                    ("● ", tr("已启用"), ACCENT)
                } else {
                    ("○ ", tr("已禁用"), WARN)
                };
                let right = format!("{:>5} KB  [{status_text}]", entry.size / 1024);
                let name_budget = w.saturating_sub(display_width(&right) + 4);
                let clipped = clip_str(&entry.filename, name_budget);
                let gap = w.saturating_sub(2 + display_width(&clipped) + display_width(&right));
                ListItem::new(Line::from(vec![
                    Span::styled(dot, bold(color)),
                    Span::styled(clipped, fg(if entry.enabled { TEXT } else { MUTED })),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(right, fg(color)),
                ]))
            })
            .collect();
        let empty = [
            Line::styled(format!(" {}", tr("当前实例还没有安装 Mod。")), bold(TEXT)),
            Line::styled(
                format!(" • {}", tr("按 B 搜索并自动下载兼容的 Modrinth 模组")),
                fg(SUBTEXT),
            ),
            Line::styled(
                format!(" • {}", tr("按 A 从本机导入 .jar 模组文件")),
                fg(SUBTEXT),
            ),
        ];
        render_selectable_list(
            f,
            app,
            SelectList {
                area: body,
                pane: PaneFocus::Right,
                selected: app.mod_sel,
                row_height: 1,
                focused,
                empty: &empty,
            },
            items,
        );
    }
}

fn draw_wb_worlds(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let body = render_panel_toolbar(
        f,
        area,
        Line::styled(
            tformat!("共 {} 个本地存档世界", app.world_list.len()),
            fg(SUBTEXT),
        ),
        Some(Line::styled(
            tr("Enter 备份 ZIP · I 导入世界 ZIP"),
            fg(ACCENT),
        )),
    );
    let w = body.width.saturating_sub(3) as usize;
    let items = app
        .world_list
        .iter()
        .map(|world| {
            let action = format!("[{}]", tr("Enter 备份"));
            let name_budget = w.saturating_sub(display_width(&action) + 5);
            let clipped = clip_str(world, name_budget);
            let gap = w.saturating_sub(2 + display_width(&clipped) + display_width(&action));
            ListItem::new(Line::from(vec![
                Span::styled("▪ ", fg(INFO)),
                Span::styled(clipped, bold(TEXT)),
                Span::raw(" ".repeat(gap)),
                Span::styled(action, fg(MUTED)),
            ]))
        })
        .collect();

    let empty = [
        Line::styled(format!(" {}", tr("暂无本地存档世界。")), bold(TEXT)),
        Line::styled(
            format!(
                " {}",
                tr("按 I 可从 ZIP 压缩包导入存档（不会覆盖已有世界）。")
            ),
            fg(MUTED),
        ),
    ];
    render_selectable_list(
        f,
        app,
        SelectList {
            area: body,
            pane: PaneFocus::Right,
            selected: app.world_sel,
            row_height: 1,
            focused,
            empty: &empty,
        },
        items,
    );
}

fn draw_wb_loaders(f: &mut Frame, app: &App, id: &str, area: Rect, focused: bool) {
    let body = render_panel_toolbar(
        f,
        area,
        Line::styled(
            tformat!("为 {} 安装加载器或优化组件", instance_display(app, id)),
            fg(SUBTEXT),
        ),
        Some(Line::styled(tr("Enter 选择组件版本"), fg(ACCENT))),
    );
    let w = body.width.saturating_sub(3) as usize;
    let items = LOADER_ITEMS
        .iter()
        .map(|(name, desc)| {
            let (_, color) = loader_badge(name);
            ListItem::new(vec![
                Line::styled(format!("◈ {name}"), bold(color)),
                Line::styled(
                    format!("  {}", clip_str(tr(desc), w.saturating_sub(2))),
                    fg(MUTED),
                ),
            ])
        })
        .collect();

    render_selectable_list(
        f,
        app,
        SelectList {
            area: body,
            pane: PaneFocus::Right,
            selected: app.wb_loader_sel,
            row_height: 2,
            focused,
            empty: &[],
        },
        items,
    );
}

// ── Page 2: Downloads Center ───────────────────────────────────────────────

fn draw_downloads(f: &mut Frame, app: &App, area: Rect) {
    let (left_col, right_col) = split_columns(area, 54, 88);
    let visible = app.remote_visible();
    let count = visible.len();
    let pos_tag = if count > 0 {
        format!(" {}/{} ", app.remote_sel.min(count - 1) + 1, count)
    } else {
        " 0/0 ".into()
    };

    let left_focused = app.focus == PaneFocus::Left;
    let left_block = panel_block(format!("{} ({count})", tr("官方版本目录")), left_focused)
        .title_bottom(Line::styled(pos_tag, fg(MUTED)).right_aligned())
        .padding(Padding::horizontal(1));
    let left_inner = left_block.inner(left_col);
    f.render_widget(left_block, left_col);

    let snap_label = if app.snapshots {
        tr("含快照 [S]")
    } else {
        tr("仅正式版 [S]")
    };
    let toolbar_left = Line::from(vec![
        Span::styled(
            format!(" {snap_label} "),
            bold(if app.snapshots { WARN } else { ACCENT }).bg(CARD_BG),
        ),
        Span::raw("  "),
        if app.remote_query.is_empty() {
            Span::styled(format!("/ {}", tr("搜索版本")), fg(MUTED))
        } else {
            Span::styled(format!("/ {}", app.remote_query), bold(WARN))
        },
    ]);
    let list_area = render_panel_toolbar(
        f,
        left_inner,
        toolbar_left,
        Some(Line::styled(tr("R 同步官方列表"), fg(INFO))),
    );

    let w = list_area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = visible
        .iter()
        .map(|ver| {
            let (kind_text, kind_color) = match ver.kind.as_str() {
                "release" => (tr("正式版"), ACCENT),
                "snapshot" => (tr("快照版"), WARN),
                _ => (tr("远古版"), MUTED),
            };
            let is_installed = app.local.iter().any(|x| x == &ver.id);
            let inst_badge = if is_installed {
                format!("  ✓ {}", tr("已安装"))
            } else {
                String::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(pad_right(&ver.id, 20.min(w / 2)), bold(TEXT)),
                Span::styled(format!("[{kind_text}]"), fg(kind_color)),
                Span::styled(inst_badge, fg(ACCENT)),
            ]))
        })
        .collect();

    let empty = vec![
        Line::styled(
            format!(" {}", tr("尚未加载官方 Minecraft 版本列表。")),
            bold(TEXT),
        ),
        Line::raw(""),
        Line::styled(
            format!(" • {}", tr("按 Enter 或 R 立即从官方/镜像源获取版本目录")),
            fg(ACCENT),
        ),
        Line::styled(
            format!(
                " • {}",
                tr("按 I 可直接从本地导入 .mrpack / CurseForge 整合包")
            ),
            fg(SUBTEXT),
        ),
    ];

    render_selectable_list(
        f,
        app,
        SelectList {
            area: list_area,
            pane: PaneFocus::Left,
            selected: app.remote_sel,
            row_height: 1,
            focused: left_focused,
            empty: &empty,
        },
        items,
    );

    if let Some(right_area) = right_col {
        let right_focused = app.focus == PaneFocus::Right;
        let right_block = panel_block(tr("安装工作台"), right_focused)
            .title_bottom(
                Line::styled(
                    if right_focused {
                        tr(" Enter 执行 · ← 返回版本列表 ")
                    } else {
                        tr(" → 选择加载器安装方式 ")
                    },
                    fg(MUTED),
                )
                .right_aligned(),
            )
            .padding(Padding::horizontal(1));
        let right_inner = right_block.inner(right_area);
        f.render_widget(right_block, right_area);

        let [card_area, actions_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(4)]).areas(right_inner);

        let card = card_block(tr("目标版本信息"), INFO);
        let card_in = card.inner(card_area);
        f.render_widget(card, card_area);
        if let Some(ver) = app.selected_remote() {
            let installed = app.local.iter().any(|x| x == &ver.id);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled(format!("Minecraft {}", ver.id), bold(TEXT)),
                        Span::raw("   "),
                        Span::styled(
                            if installed {
                                tr("✓ 本地已安装")
                            } else {
                                tr("○ 尚未安装")
                            },
                            fg(if installed { ACCENT } else { MUTED }),
                        ),
                    ]),
                    Line::styled(
                        format!(
                            "{}: {}   ·   {}: {} 线程",
                            tr("下载源"),
                            app.config.download_source,
                            tr("并发"),
                            app.config.download_threads
                        ),
                        fg(SUBTEXT),
                    ),
                ]),
                card_in,
            );
        } else {
            f.render_widget(
                Paragraph::new(vec![
                    Line::styled(tr("等待选择 Minecraft 版本"), fg(MUTED)),
                    Line::styled(
                        format!("{}: {}", tr("当前下载源"), app.config.download_source),
                        fg(DIM),
                    ),
                ]),
                card_in,
            );
        }

        let rw = actions_area.width.saturating_sub(3) as usize;
        let action_items: Vec<ListItem> = DOWNLOAD_ACTIONS
            .iter()
            .enumerate()
            .map(|(i, (icon, title, desc))| {
                let c = if i == 0 { ACCENT } else { TEXT };
                ListItem::new(vec![
                    Line::styled(format!("{icon} {}", tr(title)), bold(c)),
                    Line::styled(
                        format!("  {}", clip_str(tr(desc), rw.saturating_sub(2))),
                        fg(MUTED),
                    ),
                ])
            })
            .collect();

        render_selectable_list(
            f,
            app,
            SelectList {
                area: actions_area,
                pane: PaneFocus::Right,
                selected: app.download_action_sel,
                row_height: 2,
                focused: right_focused,
                empty: &[],
            },
            action_items,
        );
    }
}

// ── Page 3: Accounts Management ────────────────────────────────────────────

fn draw_accounts(f: &mut Frame, app: &App, area: Rect) {
    let (left_col, right_col) = split_columns(area, 48, 86);
    let left_focused = app.focus == PaneFocus::Left;
    let count = app.accounts.entries.len();
    let left_block = panel_block(format!("{} ({count})", tr("账户列表")), left_focused)
        .padding(Padding::horizontal(1));
    let left_inner = left_block.inner(left_col);
    f.render_widget(left_block, left_col);

    let list_area = render_panel_toolbar(
        f,
        left_inner,
        Line::styled(tr("支持正版微软授权与多离线角色切换"), fg(SUBTEXT)),
        Some(Line::styled(tr("Enter 切换"), fg(ACCENT))),
    );

    let lw = list_area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = app
        .accounts
        .entries
        .iter()
        .map(|acc| {
            let active = app.accounts.active.as_ref() == Some(&acc.id);
            let is_ms = acc.kind == AccountKind::Microsoft;
            let badge = if is_ms {
                format!("[{}]", tr("微软正版"))
            } else {
                format!("[{}]", tr("离线账户"))
            };
            let status = if active {
                format!("● {} ", tr("当前使用"))
            } else {
                "○ ".into()
            };
            let title_budget =
                lw.saturating_sub(display_width(&status) + display_width(&badge) + 2);
            let clipped = clip_str(&acc.name, title_budget);
            let gap = lw.saturating_sub(
                display_width(&status) + display_width(&clipped) + display_width(&badge),
            );
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(status, bold(if active { ACCENT } else { DIM })),
                    Span::styled(clipped, bold(TEXT)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(badge, fg(if is_ms { INFO } else { MUTED })),
                ]),
                Line::styled(
                    format!("  UUID: {}", clip_str(&acc.id, lw.saturating_sub(8))),
                    fg(DIM),
                ),
            ])
        })
        .collect();

    let empty = [
        Line::styled(format!(" {}", tr("尚未添加独立账户记录。")), bold(TEXT)),
        Line::styled(
            format!(" • {}", tr("按 A 使用浏览器登录正版微软 Minecraft 账户")),
            fg(INFO),
        ),
        Line::styled(
            format!(" • {}", tr("按 N 快速新建本地离线角色")),
            fg(SUBTEXT),
        ),
    ];
    render_selectable_list(
        f,
        app,
        SelectList {
            area: list_area,
            pane: PaneFocus::Left,
            selected: app.account_sel,
            row_height: 2,
            focused: left_focused,
            empty: &empty,
        },
        items,
    );

    if let Some(right_area) = right_col {
        let right_focused = app.focus == PaneFocus::Right;
        let right_block =
            panel_block(tr("身份与凭证操作"), right_focused).padding(Padding::horizontal(1));
        let right_inner = right_block.inner(right_area);
        f.render_widget(right_block, right_area);

        let [card_area, actions_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(4)]).areas(right_inner);

        let (active_name, active_kind, is_ms) = app.accounts.active_summary(&app.config.username);
        let card = card_block(tr("当前生效会话"), if is_ms { INFO } else { ACCENT });
        let card_in = card.inner(card_area);
        f.render_widget(card, card_area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(format!("● {active_name}"), bold(TEXT)),
                    Span::styled(
                        format!("  [{active_kind}]"),
                        fg(if is_ms { INFO } else { WARN }),
                    ),
                ]),
                Line::styled(
                    tr("微软正版刷新令牌与访问凭证均加密存储于操作系统凭据库。"),
                    fg(MUTED),
                ),
            ]),
            card_in,
        );

        let rw = actions_area.width.saturating_sub(3) as usize;
        let action_items: Vec<ListItem> = ACCOUNT_ACTIONS
            .iter()
            .enumerate()
            .map(|(idx, (icon, title, desc))| {
                let c = match idx {
                    1 => INFO,
                    4 => DANGER,
                    _ => TEXT,
                };
                ListItem::new(vec![
                    Line::styled(format!("{icon} {}", tr(title)), bold(c)),
                    Line::styled(
                        format!("  {}", clip_str(tr(desc), rw.saturating_sub(2))),
                        fg(MUTED),
                    ),
                ])
            })
            .collect();

        render_selectable_list(
            f,
            app,
            SelectList {
                area: actions_area,
                pane: PaneFocus::Right,
                selected: app.account_action_sel,
                row_height: 2,
                focused: right_focused,
                empty: &[],
            },
            action_items,
        );
    }
}

// ── Page 4: Global Settings (Master-Detail Split) ──────────────────────────

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    let (left_col, right_col) = split_columns(area, 30, 82);
    if let Some(right_area) = right_col {
        draw_settings_categories(f, app, left_col, app.focus == PaneFocus::Left);
        draw_settings_detail(f, app, right_area, app.focus == PaneFocus::Right);
    } else if app.focus == PaneFocus::Left {
        draw_settings_categories(f, app, left_col, true);
    } else {
        draw_settings_detail(f, app, left_col, true);
    }
}

fn draw_settings_categories(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let block = panel_block(tr("设置分组"), focused).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = inner.width.saturating_sub(3) as usize;
    let icons = ["▸", "◆", "↓", "◈", "≡", "×"];
    let counts = [
        super::SETTING_GROUPS[0].len(),
        super::SETTING_GROUPS[1].len(),
        super::SETTING_GROUPS[2].len(),
        super::SETTING_GROUPS[3].len(),
        SETTINGS.len(),
        app.trash.len(),
    ];

    let items: Vec<ListItem> = SETTINGS_CATEGORIES
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let count_str = format!("({})", counts[i]);
            let left = format!("{} {}", icons[i], tr(name));
            let gap = w.saturating_sub(display_width(&left) + display_width(&count_str));
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(left, bold(if i == 5 { WARN } else { TEXT })),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(count_str, fg(DIM)),
                ]),
                Line::styled(
                    format!("  {}", clip_str(tr(desc), w.saturating_sub(2))),
                    fg(MUTED),
                ),
            ])
        })
        .collect();

    render_selectable_list(
        f,
        app,
        SelectList {
            area: inner,
            pane: PaneFocus::Left,
            selected: app.settings_cat,
            row_height: 2,
            focused,
            empty: &[],
        },
        items,
    );
}

fn draw_settings_detail(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let (cat_title, cat_desc) = SETTINGS_CATEGORIES[app.settings_cat.min(5)];
    let bottom = if focused {
        tr(" Enter 编辑 · Space 快捷切换 · ←/Esc 返回分组 ")
    } else {
        tr(" Enter / → 进入右侧修改 ")
    };
    let block = panel_block(format!("{} · {}", tr("全局设置"), tr(cat_title)), focused)
        .title_bottom(Line::styled(bottom, fg(MUTED)).right_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.settings_cat == 5 {
        let body = render_panel_toolbar(
            f,
            inner,
            Line::styled(tr(cat_desc), fg(SUBTEXT)),
            Some(Line::styled(tr("Enter 恢复 · R 刷新"), fg(ACCENT))),
        );
        let items: Vec<ListItem> = app
            .trash
            .iter()
            .map(|entry| {
                ListItem::new(vec![
                    Line::styled(format!("↺ {}", entry.label), bold(TEXT)),
                    Line::styled(format!("  ID: {}", entry.id), fg(MUTED)),
                ])
            })
            .collect();
        let empty = [Line::styled(format!(" {}", tr("回收站为空。")), fg(MUTED))];
        render_selectable_list(
            f,
            app,
            SelectList {
                area: body,
                pane: PaneFocus::Right,
                selected: app.trash_sel,
                row_height: 2,
                focused,
                empty: &empty,
            },
            items,
        );
        return;
    }

    let indices = app.global_setting_indices();
    let hint_text = app
        .active_global_setting_index()
        .and_then(|idx| setting_hint(SETTINGS[idx]))
        .unwrap_or_else(|| tr(cat_desc).to_owned());

    let body = render_panel_toolbar(
        f,
        inner,
        Line::styled(hint_text, fg(SUBTEXT)),
        Some(Line::styled(tr("V 检测 Java"), fg(INFO))),
    );

    let w = body.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = indices
        .iter()
        .map(|&idx| {
            let val = app.setting_value(None, idx);
            format_setting_row(idx, &val, None, w)
        })
        .collect();

    render_selectable_list(
        f,
        app,
        SelectList {
            area: body,
            pane: PaneFocus::Right,
            selected: app.settings_field_sel,
            row_height: 1,
            focused,
            empty: &[],
        },
        items,
    );
}

// ── Page 5: Tasks & Logs ───────────────────────────────────────────────────

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let [task_area, log_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(4)]).areas(area);

    // Top live task monitor card
    let task_card = card_block(
        tr("后台任务监控"),
        if app.task.is_some() { INFO } else { ACCENT },
    );
    let task_inner = task_card.inner(task_area);
    f.render_widget(task_card, task_area);
    if app.task.is_some() {
        let (ratio, known) = progress_ratio(app);
        let detail = progress_detail(app);
        let bar_w = (task_inner.width as usize / 3).clamp(12, 36);
        let mut line1 = vec![Span::styled(
            if app.paused {
                format!("◌ {}  ", tr("已暂停"))
            } else {
                format!("◐ {}  ", tr("运行中"))
            },
            bold(if app.paused { WARN } else { INFO }),
        )];
        line1.extend(progress_bar(ratio, bar_w, ACCENT));
        if known {
            line1.push(Span::styled(
                format!(" {:>3.0}%  ", ratio * 100.0),
                bold(TEXT),
            ));
        } else {
            line1.push(Span::raw("  "));
        }
        line1.push(Span::styled(detail, fg(SUBTEXT)));
        f.render_widget(
            Paragraph::new(vec![
                Line::from(line1),
                Line::styled(
                    tr("P 暂停/继续下载 · C 取消当前任务 · F6 打开独立进度浮窗"),
                    fg(MUTED),
                ),
            ]),
            task_inner,
        );
    } else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(format!("● {}  ", tr("当前无运行中后台任务")), bold(ACCENT)),
                    Span::styled(
                        tformat!("共记录 {} 行事件与输出", app.logs.len()),
                        fg(MUTED),
                    ),
                ]),
                Line::styled(
                    tr("↑↓ 逐行滚动 · PgUp/PgDn 翻页 · ←→ 横向平移 · Home 最早 · End 跟随最新"),
                    fg(DIM),
                ),
            ]),
            task_inner,
        );
    }

    let height = log_area.height.saturating_sub(2) as usize;
    app.log_view_height.set(height.max(1));
    let scroll = app
        .log_scroll
        .min(app.logs.len().saturating_sub(height.max(1)));
    let end = app.logs.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);

    let lines: Vec<Line> = app
        .logs
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(idx, line)| {
            Line::from(vec![
                Span::styled(format!("{:>4} │ ", idx + 1), fg(DIM)),
                Span::styled(line.as_str(), fg(log_line_color(line))),
            ])
        })
        .collect();

    let follow = if app.log_scroll == 0 {
        Span::styled(format!(" ● {} ", tr("跟随最新")), bold(ACCENT))
    } else {
        Span::styled(format!(" ◌ {} ", tr("历史回看")), bold(WARN))
    };
    let pos = Line::from(vec![
        Span::styled(
            format!(
                " {}–{} / {} ",
                if end == 0 { 0 } else { start + 1 },
                end,
                app.logs.len()
            ),
            fg(MUTED),
        ),
        follow,
    ])
    .right_aligned();

    f.render_widget(
        Paragraph::new(lines).scroll((0, app.log_horizontal)).block(
            panel_block(tr("任务日志"), true)
                .title_bottom(pos)
                .padding(Padding::horizontal(1)),
        ),
        log_area,
    );
}

// ── Bottom Status & Keybind Dock ───────────────────────────────────────────

fn draw_bottom_dock(f: &mut Frame, app: &App, area: Rect) {
    let inner = if area.height >= 4 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fg(BORDER_SUBTLE))
            .style(Style::default().bg(BAR_BG).fg(TEXT))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        inner
    } else {
        f.render_widget(Block::default().style(Style::default().bg(BAR_BG)), area);
        area
    };

    let [status_row, keys_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

    // Row 1: Live task progress or latest status message
    if app.task.is_some() {
        app.hits
            .borrow_mut()
            .push((status_row, HitTarget::TaskTogglePopup));
        let (ratio, known) = progress_ratio(app);
        let (state_txt, state_col) = if app.control.cancelled() {
            (tr("正在取消"), WARN)
        } else if app.paused {
            (tr("已暂停"), WARN)
        } else {
            (tr("进行中"), INFO)
        };
        let bar_w = (status_row.width as usize / 5).clamp(10, 26);
        let mut spans = vec![
            Span::styled("◐ ", bold(state_col)),
            Span::styled(state_txt, bold(state_col)),
            Span::raw("  "),
        ];
        spans.extend(progress_bar(ratio, bar_w, ACCENT));
        if known {
            spans.push(Span::styled(
                format!(" {:>3.0}% ", ratio * 100.0),
                bold(TEXT),
            ));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            clip_str(&progress_detail(app), status_row.width as usize / 2),
            fg(SUBTEXT),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), status_row);
    } else {
        let latest = app
            .logs
            .back()
            .map(String::as_str)
            .unwrap_or(tr("选择实例按 Space 立即启动，Enter 打开右侧工作台"));
        let dir_hint = Line::styled(
            format!(
                "{} ",
                clip_str(&app.config.game_dir.display().to_string(), 28)
            ),
            fg(DIM),
        );
        let left = Line::from(vec![
            Span::styled("● ", bold(ACCENT)),
            Span::styled(tr("就绪"), bold(ACCENT)),
            Span::styled("  │  ", fg(DIM)),
            Span::styled(
                clip_str(latest, status_row.width.saturating_sub(36) as usize),
                fg(log_line_color(latest)),
            ),
        ]);
        if left.width() + dir_hint.width() + 2 <= status_row.width as usize {
            f.render_widget(Paragraph::new(dir_hint.right_aligned()), status_row);
        }
        f.render_widget(Paragraph::new(left), status_row);
    }

    // Row 2: Context-sensitive keycap hints
    let context = context_hints(app);
    let left_keys = Line::from(key_chips(&context));

    let mut global = Vec::new();
    if app.task.is_some() {
        global.push(("F6", tr("任务")));
        global.push(("C", tr("取消")));
    }
    global.push(("Tab", tr("切页")));
    global.push(("F1", tr("帮助")));
    global.push(("Q", tr("退出")));
    let right_keys = Line::from(key_chips(&global));

    if left_keys.width() + right_keys.width() + 3 <= keys_row.width as usize {
        f.render_widget(Paragraph::new(right_keys.right_aligned()), keys_row);
    }
    f.render_widget(Paragraph::new(left_keys), keys_row);
}

fn context_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    match (app.page, app.focus) {
        (PrimaryPage::Play, PaneFocus::Left) => vec![
            ("Space", tr("启动")),
            ("Enter/→", tr("工作台")),
            ("/", tr("搜索")),
            ("O", tr("设置")),
            ("M", tr("Mod")),
            ("R", tr("刷新")),
        ],
        (PrimaryPage::Play, PaneFocus::Right) => match app.wb_tab {
            WorkbenchTab::Overview => vec![
                ("Enter", tr("执行")),
                ("Space", tr("启动")),
                ("←→", tr("子页")),
                ("Esc", tr("返回列表")),
            ],
            WorkbenchTab::Settings => vec![
                ("Enter", tr("编辑")),
                ("Space", tr("切换")),
                ("D", tr("重置覆盖")),
                ("←→", tr("子页")),
                ("Esc", tr("返回")),
            ],
            WorkbenchTab::Mods => vec![
                ("Enter", tr("启停/安装")),
                ("A", tr("导入")),
                ("B", tr("在线搜索")),
                ("U", tr("更新")),
                ("Del", tr("移出")),
                ("Esc", tr("返回")),
            ],
            WorkbenchTab::Worlds => vec![
                ("Enter", tr("备份")),
                ("I", tr("导入世界")),
                ("←→", tr("子页")),
                ("Esc", tr("返回")),
            ],
            WorkbenchTab::Loaders => vec![
                ("Enter", tr("选择版本")),
                ("←→", tr("子页")),
                ("Esc", tr("返回")),
            ],
        },
        (PrimaryPage::Downloads, PaneFocus::Left) => vec![
            ("Enter", tr("安装原版")),
            ("→", tr("加载器安装")),
            ("R", tr("获取列表")),
            ("S", tr("快照")),
            ("/", tr("搜索")),
            ("I", tr("导入整合包")),
        ],
        (PrimaryPage::Downloads, PaneFocus::Right) => vec![
            ("Enter", tr("执行安装")),
            ("←/Esc", tr("返回版本列表")),
            ("R", tr("刷新目录")),
        ],
        (PrimaryPage::Accounts, _) => vec![
            ("Enter", tr("切换")),
            ("A", tr("微软登录")),
            ("N", tr("添加离线")),
            ("D", tr("移除")),
            ("Shift+K", tr("CF密钥")),
        ],
        (PrimaryPage::Settings, PaneFocus::Left) => vec![
            ("↑↓", tr("选择分类")),
            ("Enter/→", tr("编辑右侧")),
            ("V", tr("检测 Java")),
        ],
        (PrimaryPage::Settings, PaneFocus::Right) => vec![
            ("Enter", tr("编辑/恢复")),
            ("Space", tr("快捷切换")),
            ("←/Esc", tr("返回分类")),
        ],
        (PrimaryPage::Logs, _) => vec![
            ("↑↓", tr("滚动")),
            ("PgUp/PgDn", tr("翻页")),
            ("←→", tr("横向平移")),
            ("End", tr("最新")),
        ],
    }
}

// ── Modal Overlays ─────────────────────────────────────────────────────────

fn modal_footer(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = key_chips(items);
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn draw_modals(f: &mut Frame, app: &App, area: Rect) {
    if let Some(picker) = &app.loader_picker {
        let inner = render_modal(
            f,
            area,
            (76, 22),
            format!("{} · Minecraft {}", picker.kind.name(), picker.game),
            INFO,
            Some(modal_footer(&[
                ("↑↓", tr("选择")),
                ("Enter", tr("安装")),
                ("Esc", tr("取消")),
            ])),
        );
        let mut items = vec![ListItem::new(Line::styled(
            format!("★ {}", tr("自动选择推荐 / 稳定版本")),
            bold(ACCENT),
        ))];
        items.extend(picker.versions.iter().map(|entry| {
            let mut spans = vec![Span::styled(entry.version.clone(), fg(TEXT))];
            if !entry.stable {
                spans.push(Span::styled(tr("  [预览]"), fg(WARN)));
            }
            ListItem::new(Line::from(spans))
        }));
        let mut state = ListState::default().with_selected(Some(picker.selected));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(row_highlight(true))
                .highlight_symbol("▸ "),
            inner,
            &mut state,
        );
    }

    if app.task_popup && app.task.is_some() {
        let (state_title, accent) = if app.control.cancelled() {
            (tr("正在取消 · 等待当前操作结束"), WARN)
        } else if app.paused {
            (tr("任务已暂停"), WARN)
        } else {
            (tr("后台任务进行中"), INFO)
        };
        let controls: Vec<(&str, &str)> = if app.control.cancelled() {
            vec![("Esc", tr("后台")), ("L", tr("日志"))]
        } else if app.pause_allowed {
            vec![
                ("P", tr("暂停/继续")),
                ("C", tr("取消")),
                ("Esc", tr("后台运行")),
                ("L", tr("日志")),
            ]
        } else {
            vec![
                ("C", tr("取消")),
                ("Esc", tr("后台运行")),
                ("L", tr("日志")),
            ]
        };
        let inner = render_modal(
            f,
            area,
            (74, 13),
            state_title,
            accent,
            Some(modal_footer(&controls)),
        );
        let [stage_r, gauge_r, detail_r, log_r] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .areas(inner);

        f.render_widget(
            Paragraph::new(Line::styled(app.progress.0.clone(), bold(TEXT))),
            stage_r,
        );
        let (ratio, known) = progress_ratio(app);
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::default().fg(ACCENT).bg(PANEL_BG))
                .label(Span::styled(
                    if known {
                        format!("{:.0}%", ratio * 100.0)
                    } else {
                        tr("处理中 · 等待进度").into()
                    },
                    bold(TEXT),
                )),
            gauge_r,
        );
        f.render_widget(
            Paragraph::new(Line::styled(
                format!("\n{}", progress_detail(app)),
                fg(SUBTEXT),
            )),
            detail_r,
        );
        let latest = app.logs.back().map(String::as_str).unwrap_or("");
        f.render_widget(
            Paragraph::new(Line::styled(latest.to_owned(), fg(MUTED))).wrap(Wrap { trim: false }),
            log_r,
        );
    }

    if let Some((title, message)) = &app.task_result {
        let color = if title == tr("任务失败") {
            DANGER
        } else if title == tr("任务已取消") {
            WARN
        } else {
            ACCENT
        };
        let inner = render_modal(
            f,
            area,
            (78, 16),
            title.as_str(),
            color,
            Some(modal_footer(&[
                ("↑↓", tr("滚动")),
                ("Enter/Esc", tr("关闭")),
                ("L", tr("打开日志")),
            ])),
        );
        let paragraph = Paragraph::new(message.as_str())
            .style(fg(TEXT))
            .wrap(Wrap { trim: false });
        let max_scroll = paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
        app.result_max_scroll.set(max_scroll);
        f.render_widget(
            paragraph.scroll((app.result_scroll.min(max_scroll), 0)),
            inner,
        );
    }

    if let Some(editor) = &app.editor {
        draw_editor_modal(f, area, editor);
    }

    if app.confirm_stop {
        let inner = render_modal(
            f,
            area,
            (64, 8),
            tr("终止游戏确认"),
            WARN,
            Some(modal_footer(&[
                ("Y", tr("确认终止")),
                ("N/Esc", tr("返回")),
            ])),
        );
        f.render_widget(
            Paragraph::new(tr(
                "强制结束运行中的游戏进程可能导致未保存的世界进度丢失。\n按 Y 确认立即终止，按 N 或 Esc 返回。",
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    if let Some(id) = &app.confirm_remove {
        let inner = render_modal(
            f,
            area,
            (66, 8),
            tr("移入实例回收站"),
            DANGER,
            Some(modal_footer(&[
                ("Y", tr("确认移入")),
                ("N/Esc", tr("取消")),
            ])),
        );
        f.render_widget(
            Paragraph::new(tformat!(
                "将实例「{id}」移入回收站？实例配置、Mod 与存档均会保留，可在「全局设置 › 实例回收站」随时恢复。",
                id = instance_display(app, id)
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    if let Some(selected) = app.language_picker {
        let inner = render_modal(
            f,
            area,
            (68, 17),
            "Language / 界面语言",
            INFO,
            Some(Line::styled(" ↑↓ · Enter Select · Esc Back ", fg(MUTED))),
        );
        let items: Vec<ListItem> = std::iter::once(ListItem::new(Line::styled(
            "System / 跟随系统  [auto]",
            bold(ACCENT),
        )))
        .chain(crate::localization::UiLanguage::ALL.iter().map(|lang| {
            let (done, total) = lang.coverage();
            ListItem::new(Line::from(vec![
                Span::styled(pad_right(lang.name(), 28), fg(TEXT)),
                Span::styled(format!("[{}]  ", lang.game_code()), fg(INFO)),
                Span::styled(format!("{done}/{total}"), fg(MUTED)),
            ]))
        }))
        .collect();
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(row_highlight(true))
                .highlight_symbol("▸ "),
            inner,
            &mut state,
        );
    }

    if app.help {
        let inner = render_modal(
            f,
            area,
            (82, 22),
            tr("键盘与鼠标操作指南"),
            INFO,
            Some(modal_footer(&[("↑↓", tr("滚动")), ("F1/Esc", tr("关闭"))])),
        );
        let help_lines = vec![
            Line::styled(tr("── 全局导航与鼠标支持 ──"), bold(ACCENT)),
            Line::styled(tr("  1 – 5 / Tab / [ ]   直接切换五个顶部主工作区"), fg(TEXT)),
            Line::styled(tr("  鼠标左键 / 滚轮     点击顶部标签、子标签、列表项与启动按钮，滚轮上下翻动"), fg(TEXT)),
            Line::styled(tr("  R / V               刷新当前页面数据 / 自动扫描检测本机已安装 Java"), fg(TEXT)),
            Line::styled(tr("  F6 / P / C          打开任务进度浮窗 / 暂停或继续下载 / 取消任务"), fg(TEXT)),
            Line::raw(""),
            Line::styled(tr("── [1] 启动与实例工作台 ──"), bold(ACCENT)),
            Line::styled(tr("  Space               使用当前账户立即启动所选游戏实例"), fg(TEXT)),
            Line::styled(tr("  Enter / →           进入右侧实例工作台（概览 / 独立设置 / Mod / 存档 / 组件）"), fg(TEXT)),
            Line::styled(tr("  ← →（工作台内）     在五个实例工作台子页面之间平滑切换，Esc 返回实例列表"), fg(TEXT)),
            Line::styled(tr("  O / M / W           从实例列表直达独立设置 / Mod 管理 / 存档管理"), fg(TEXT)),
            Line::styled(tr("  F2 / T / E / Del    重命名 / 复制实例 / 导出 .mrpack / 移入回收站"), fg(TEXT)),
            Line::raw(""),
            Line::styled(tr("── [2] 下载中心 · [3] 账户 · [4] 全局设置 ──"), bold(ACCENT)),
            Line::styled(tr("  下载中心            R 获取版本目录 · S 切换快照 · → 选择原版/Fabric/Forge/NeoForge"), fg(TEXT)),
            Line::styled(tr("  账户管理            Enter 切换账户 · A 微软正版浏览器登录 · N 添加离线账户"), fg(TEXT)),
            Line::styled(tr("  全局设置            左侧选择分类，Enter/→ 编辑右侧字段，Space 快捷切换开关与镜像源"), fg(TEXT)),
        ];
        f.render_widget(
            Paragraph::new(help_lines)
                .scroll((app.help_scroll, 0))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn draw_editor_modal(f: &mut Frame, area: Rect, editor: &super::Editor) {
    let (heading, subtitle) = match editor.kind {
        EditKind::InstanceSearch => (
            tr("搜索本地实例"),
            tr("按名称或版本 ID 实时过滤 · Ctrl+U 清空"),
        ),
        EditKind::RemoteSearch => (
            tr("搜索官方版本"),
            tr("输入版本号（如 1.21 或 1.20.1）· Ctrl+U 清空"),
        ),
        EditKind::GlobalSetting(idx) => (tr("编辑全局设置"), tr(SETTINGS[idx])),
        EditKind::InstanceSetting(idx) => (tr("编辑实例独立设置"), tr(SETTINGS[idx])),
        EditKind::ImportMod => (tr("导入本地 Mod"), tr("输入本地 .jar 文件完整路径")),
        EditKind::OfflineAccount => (
            tr("新建离线账户"),
            tr("输入 1–16 位英文字母、数字或下划线角色名"),
        ),
        EditKind::ImportPack => (
            tr("导入整合包"),
            tr("输入 Modrinth .mrpack 或 CurseForge .zip 路径"),
        ),
        EditKind::NameInstance => (
            tr("修改实例显示名称"),
            tr("仅改变界面显示名，底层版本 ID 保持不变"),
        ),
        EditKind::CloneInstance => (
            tr("复制为新实例"),
            tr("输入新实例目录 ID（完整复制配置、Mod 与存档）"),
        ),
        EditKind::ExportPack => (
            tr("导出 Modrinth 整合包"),
            tr("输入输出 .mrpack 文件路径（不含存档与凭证）"),
        ),
        EditKind::CurseKey => (
            tr("配置 CurseForge API 密钥"),
            tr("密钥将加密存入系统安全凭据存储"),
        ),
        EditKind::ModQuery => (
            tr("搜索 Modrinth 在线模组"),
            tr("自动按当前实例游戏版本与加载器过滤兼容模组"),
        ),
        EditKind::BackupWorld => (
            tr("备份存档世界"),
            tr("输入输出 .zip 备份文件路径（不覆盖已有文件）"),
        ),
        EditKind::ImportWorldZip => (
            tr("导入存档世界 (1/2)"),
            tr("输入世界备份 .zip 路径，下一步命名新目录"),
        ),
        EditKind::ImportWorldName => (
            tr("导入存档世界 (2/2)"),
            tr("输入新存档目录名称（不会覆盖已有世界）"),
        ),
    };

    let inner = render_modal(
        f,
        area,
        (82, 9),
        heading,
        ACCENT,
        Some(modal_footer(&[
            ("Enter", tr("确认")),
            ("Esc", tr("取消")),
            ("Ctrl+U", tr("清空")),
        ])),
    );
    let [desc_r, box_r] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(3)]).areas(inner);
    f.render_widget(Paragraph::new(Line::styled(subtitle, fg(SUBTEXT))), desc_r);

    let masked = if matches!(editor.kind, EditKind::CurseKey) {
        "•".repeat(editor.text.chars().count())
    } else {
        editor.text.replace(['\n', '\r', '\t'], " ")
    };

    let field_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(ACCENT))
        .style(Style::default().bg(CANVAS_BG));
    let field_in = field_block.inner(box_r);
    let visible = input_tail(&masked, field_in.width.saturating_sub(3));

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ▸ ", bold(ACCENT)),
            Span::styled(visible.clone(), fg(TEXT)),
        ]))
        .block(field_block),
        box_r,
    );
    if field_in.height > 0 {
        f.set_cursor_position((field_in.x + 3 + display_width(&visible) as u16, field_in.y));
    }
}

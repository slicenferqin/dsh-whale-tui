//! ratatui rendering: status bar / scrollback / composer / shortcuts bar.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use crate::app::{App, Dialog, Focus, RunState, ShortcutSection};
use crate::resume::age_label;
use crate::transcript::{CellKind, Transcript};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(
            Style::default()
                .fg(app.theme.text_primary)
                .bg(app.theme.bg_base),
        ),
        area,
    );

    let composer_height = composer_height(&app.input, area.width.saturating_sub(5));
    let stats_lines = stats_lines(app, area.width);
    // GoalBar docks directly above the composer, matching where DSH's own web
    // client puts it (dsh-client-ui-goal). Zero rows when there is no goal, so
    // a session that never sets one loses no space.
    let goal_height = u16::from(app.projections.goal.is_some());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(stats_lines.len() as u16),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(goal_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);
    app.composer_top = chunks[5].y;
    draw_status(f, app, chunks[0]);
    draw_stats(f, app, chunks[1], &stats_lines);
    draw_scrollback(f, app, chunks[2]);
    draw_activity(f, app, chunks[3]);
    if goal_height > 0 {
        draw_goal_bar(f, app, chunks[4]);
    }
    draw_composer(f, app, chunks[5]);
    draw_shortcuts(f, app, chunks[6]);
    if app.has_dialog() {
        draw_dialog(f, app, area);
    }
}

/// GoalBar: objective, phase badge, and round progress. A blocked goal shows
/// its reason instead of the round counter — that is the actionable fact.
fn draw_goal_bar(f: &mut Frame, app: &App, area: Rect) {
    use crate::projection::GoalPhase;
    let Some(goal) = app.projections.goal.as_ref() else {
        return;
    };
    let (badge_color, badge) = match goal.phase {
        GoalPhase::Active => (app.theme.accent_running, "◆ goal"),
        GoalPhase::Paused => (app.theme.gray, "‖ goal"),
        GoalPhase::Blocked => (app.theme.warning, "! goal"),
        GoalPhase::Complete => (app.theme.accent_success, "✓ goal"),
    };
    let mut spans = vec![
        Span::styled(
            format!("  {badge} "),
            Style::default()
                .fg(badge_color)
                .bg(app.theme.bg_base)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // Rounds first: it is fixed-width, so the objective takes whatever is left
    // rather than pushing the counter off a narrow terminal.
    let tail = match (goal.phase, goal.blocked_reason.as_deref()) {
        (GoalPhase::Blocked, Some(reason)) => format!(" · {reason}"),
        _ if goal.max_rounds > 0 => {
            format!(" · round {}/{}", goal.rounds_started, goal.max_rounds)
        }
        _ => String::new(),
    };
    let used = unicode_width::UnicodeWidthStr::width(badge) + 3
        + unicode_width::UnicodeWidthStr::width(tail.as_str());
    let budget = (area.width as usize).saturating_sub(used).max(8);
    spans.push(Span::styled(
        truncated(&goal.objective, budget),
        Style::default()
            .fg(app.theme.text_primary)
            .bg(app.theme.bg_base),
    ));
    if !tail.is_empty() {
        spans.push(Span::styled(
            tail,
            Style::default()
                .fg(if goal.phase == GoalPhase::Blocked {
                    app.theme.warning
                } else {
                    app.theme.gray
                })
                .bg(app.theme.bg_base),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(app.theme.bg_base)),
        area,
    );
}

fn composer_height(input: &str, content_width: u16) -> u16 {
    composer_rows(input, content_width).len().clamp(1, 5) as u16 + 2
}

/// How many wrapped rows the composer shows at once.
const COMPOSER_VIEW_ROWS: usize = 5;

/// Wrapped composer rows plus where the caret lands in them. `cursor` is a byte
/// offset into `input`; the caret is placed *before* the character at that
/// offset, so a caret sitting on a wrap point renders at the start of the next
/// row rather than off the right edge.
struct ComposerLayout {
    rows: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

fn composer_layout(input: &str, cursor: usize, content_width: u16) -> ComposerLayout {
    let width = content_width.max(1) as usize;
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut row_width = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut placed = false;
    for (offset, ch) in input.char_indices() {
        if ch == '\n' {
            if offset == cursor {
                cursor_row = rows.len();
                cursor_col = row_width;
                placed = true;
            }
            rows.push(std::mem::take(&mut row));
            row_width = 0;
            continue;
        }
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width > 0 && row_width > 0 && row_width + char_width > width {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        if offset == cursor {
            cursor_row = rows.len();
            cursor_col = row_width;
            placed = true;
        }
        row.push(ch);
        row_width = row_width.saturating_add(char_width);
    }
    if !placed {
        cursor_row = rows.len();
        cursor_col = row_width;
    }
    rows.push(row);
    ComposerLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn composer_rows(input: &str, content_width: u16) -> Vec<String> {
    composer_layout(input, 0, content_width).rows
}

fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

/// Grok 风格的圆角弹窗外框：品牌色 border、纯色底，视觉上从 scrollback 浮起。
fn popup_block<'a>(title: impl Into<Line<'a>>, color: ratatui::style::Color) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(title)
}

/// Truncate to a display-column budget. The ellipsis is counted *against* the
/// budget, so the result is never wider than asked — callers lay out around
/// this, and one extra column silently clips whatever follows.
fn truncated(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if unicode_width::UnicodeWidthStr::width(text) <= budget {
        return text.to_string();
    }
    let limit = budget - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn pretty_outcome(s: &str) -> &str {
    match s {
        "allowed-once" => "允许一次 (Allow once)",
        "rejected" => "拒绝 (Reject)",
        "cancelled" => "取消 (Cancel)",
        "unavailable" => "不可用 (Unavailable)",
        "always" | "always-allow" => "始终允许 (Always allow)",
        other => other,
    }
}

fn draw_dialog(f: &mut Frame, app: &App, area: Rect) {
    match &app.dialog {
        Dialog::None => {}
        Dialog::Approval(d) => {
            let w = area.width.min(72).saturating_sub(4).max(30);
            let h = (d.options.len() as u16 + 6)
                .min(area.height.saturating_sub(4))
                .max(8);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    format!(" 工具请求 · {}", d.tool_name),
                    Style::default()
                        .fg(app.theme.accent_tool)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            if !d.reason.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", d.reason),
                    Style::default()
                        .fg(app.theme.text_secondary)
                        .bg(app.theme.bg_light),
                )));
            }
            if !d.input.is_empty() {
                let one = truncated(&d.input, 72);
                lines.push(Line::from(Span::styled(
                    format!("  {} ", one),
                    Style::default()
                        .fg(app.theme.gray_bright)
                        .bg(app.theme.code_bg),
                )));
            } else if let Some(call_id) = &d.call_id {
                lines.push(Line::from(Span::styled(
                    format!("  call {}", truncated(call_id, 64)),
                    Style::default()
                        .fg(app.theme.gray_dim)
                        .bg(app.theme.bg_light),
                )));
            }
            lines.push(Line::from(""));
            for (i, opt) in d.options.iter().enumerate() {
                let selected = i == d.selected;
                let mark = if selected { "▌" } else { " " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(app.theme.text_secondary)
                        .bg(app.theme.bg_light)
                };
                lines.push(Line::from(Span::styled(
                    format!(" {mark} {}. {}", i + 1, pretty_outcome(opt)),
                    style,
                )));
            }
            let block = popup_block(
                Line::from(Span::styled(
                    " ⚑ permission ",
                    Style::default()
                        .fg(app.theme.accent_tool)
                        .add_modifier(Modifier::BOLD),
                )),
                app.theme.accent_brand,
            )
            .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Queue(view) => {
            let queued = app
                .queue()
                .iter()
                .filter(|item| item.placement == "queued")
                .collect::<Vec<_>>();
            let w = area.width.min(88).saturating_sub(4).max(40);
            let h = (queued.len() as u16 + 5)
                .min(area.height.saturating_sub(4))
                .max(7);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines = vec![Line::from(Span::styled(
                format!(" {} queued follow-up{}", queued.len(), if queued.len() == 1 { "" } else { "s" }),
                Style::default()
                    .fg(app.theme.accent_plan)
                    .add_modifier(Modifier::BOLD),
            )), Line::from("")];
            for (index, item) in queued.iter().enumerate() {
                let selected = index == view.selected;
                let mark = if selected { "›" } else { " " };
                let content = if selected && view.editing {
                    format!(" {mark} edit: {}", view.draft)
                } else {
                    format!(" {mark} {}", item.preview)
                };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(content, style)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " Enter send now · s steer · e edit · d remove",
                Style::default().fg(app.theme.gray),
            )));
            let block = popup_block(" ⇥ prompt queue ", app.theme.accent_plan)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Tasks(t) => {
            let w = area.width.min(90).saturating_sub(4).max(40);
            let h = (t.rows.len() as u16 + 3)
                .min(area.height.saturating_sub(4))
                .max(6);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                " tasks — r 刷新 · q/Esc 关闭",
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            if t.rows.is_empty() {
                lines.push(Line::from(Span::styled(
                    " (无后台任务)",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            for (i, row) in t.rows.iter().enumerate() {
                let selected = i == t.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{}{} · {} · {} · {} · {}",
                        mark, row.kind, row.id, row.status, row.label, row.detail
                    ),
                    style,
                )));
            }
            let block = popup_block(" ◈ tasks ", app.theme.accent_plan)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Todos(v) => {
            let todos = &app.transcript.todos;
            let done = todos
                .iter()
                .filter(|t| t.status == crate::transcript::TodoStatus::Completed)
                .count();
            let w = area.width.min(90).saturating_sub(4).max(40);
            let h = (todos.len() as u16 + 4)
                .min(area.height.saturating_sub(4))
                .max(6);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    format!(" {done}/{} 完成 — y 复制 · q/Esc 关闭", todos.len()),
                    Style::default().fg(app.theme.gray),
                )),
                Line::from(""),
            ];
            if todos.is_empty() {
                lines.push(Line::from(Span::styled(
                    " (暂无任务列表)",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            for (i, item) in todos.iter().enumerate() {
                use crate::transcript::TodoStatus;
                let selected = i == v.selected;
                let marker_color = match item.status {
                    TodoStatus::Completed => app.theme.accent_success,
                    TodoStatus::InProgress => app.theme.accent_running,
                    TodoStatus::Cancelled => app.theme.gray_dim,
                    TodoStatus::Pending => app.theme.gray,
                };
                let mut text_style = match item.status {
                    TodoStatus::Completed => Style::default().fg(app.theme.gray),
                    TodoStatus::InProgress => Style::default()
                        .fg(app.theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                    TodoStatus::Cancelled => Style::default()
                        .fg(app.theme.gray_dim)
                        .add_modifier(Modifier::CROSSED_OUT),
                    TodoStatus::Pending => Style::default().fg(app.theme.text_secondary),
                };
                let mut marker_style = Style::default().fg(marker_color);
                if selected {
                    marker_style = marker_style.bg(app.theme.bg_highlight);
                    text_style = text_style.bg(app.theme.bg_highlight);
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{} ", if selected { "› " } else { "  " }, item.status.marker()),
                        marker_style,
                    ),
                    Span::styled(truncated(&item.text, w.saturating_sub(9) as usize), text_style),
                ]));
            }
            let block = popup_block(" ◈ todos ", app.theme.accent_plan)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Subagent(v) => {
            let w = area.width.saturating_sub(6).max(40);
            let h = area.height.saturating_sub(4).max(10);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let lines_all: Vec<Line> = app
                .child_transcripts
                .get(&v.child_id)
                .map(|t| transcript_lines(app, t))
                .unwrap_or_default();
            let rows = wrap_lines(&lines_all, w.saturating_sub(2));
            let view = tail_window(&rows, h.saturating_sub(2) as usize, v.scroll);
            let block = popup_block(
                Line::from(Span::styled(
                    format!(" ⬢ subagent {} ", v.child_id),
                    Style::default()
                        .fg(app.theme.accent_plan)
                        .add_modifier(Modifier::BOLD),
                )),
                app.theme.accent_plan,
            )
            .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(view)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Block(view) => {
            let w = area.width.saturating_sub(4).clamp(40, 120);
            let h = area.height.saturating_sub(4).max(12);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let raw = view.cell.raw && !view.cell.raw_text.is_empty();
            let text = if raw {
                &view.cell.raw_text
            } else {
                &view.cell.text
            };
            let body = if raw {
                text.lines()
                    .map(|line| Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(app.theme.text_secondary),
                    )))
                    .collect::<Vec<_>>()
            } else {
                tool_body_lines(app, text)
            };
            let rows = wrap_lines(&body, w.saturating_sub(2));
            let shown = tail_window(&rows, h.saturating_sub(2) as usize, view.scroll);
            let label = if raw { "raw" } else { "formatted" };
            let title = if view.cell.title.is_empty() {
                format!(" ◆ tool result · {label} ")
            } else {
                format!(" ◆ {} · {label} ", view.cell.title)
            };
            let block = popup_block(title, app.theme.accent_tool)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(shown)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::History(view) => {
            let w = area.width.min(88).saturating_sub(4).max(40);
            let max_rows = area.height.saturating_sub(8).max(4) as usize;
            let shown = view.visible.len().min(max_rows);
            let h = (shown as u16 + 5)
                .min(area.height.saturating_sub(4))
                .max(7);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines = vec![Line::from(vec![
                Span::styled(" search: ", Style::default().fg(app.theme.gray)),
                Span::styled(&view.query, Style::default().fg(app.theme.text_primary)),
            ]), Line::from("")];
            if view.visible.is_empty() {
                lines.push(Line::from(Span::styled(
                    " no matching prompts",
                    Style::default().fg(app.theme.gray_dim),
                )));
            } else {
                let start = view.selected.saturating_sub(max_rows.saturating_sub(1));
                for (position, index) in view.visible.iter().enumerate().skip(start).take(max_rows) {
                    let selected = position == view.selected;
                    let mark = if selected { "› " } else { "  " };
                    let preview = app.history[*index].replace('\n', " ");
                    let style = if selected {
                        Style::default()
                            .fg(app.theme.text_primary)
                            .bg(app.theme.bg_highlight)
                    } else {
                        Style::default().fg(app.theme.text_secondary)
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{mark}{}", truncated(&preview, w.saturating_sub(6) as usize)),
                        style,
                    )));
                }
            }
            let block = popup_block(" ↟ prompt history ", app.theme.accent_user)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Info(d) => {
            let w = area.width.min(80).saturating_sub(4).max(40);
            let h = (d.rows.len() as u16 + 2)
                .min(area.height.saturating_sub(4))
                .max(6);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                " session info — q/Enter/Esc 关闭",
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for (k, v) in &d.rows {
                let one: String = v.chars().take(60).collect();
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {}: ", k),
                        Style::default().fg(app.theme.accent_plan),
                    ),
                    Span::styled(one, Style::default().fg(app.theme.text_primary)),
                ]));
            }
            let block = popup_block(" ◉ info ", app.theme.accent_user)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Theme(t) => {
            let w = area.width.min(40).saturating_sub(4).max(20);
            let h = 6u16;
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                " /theme — 实时预览 · Enter 保持 · Esc 还原",
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for (i, name) in ["dark", "light"].iter().enumerate() {
                let selected = i == t.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(format!("{}{}", mark, name), style)));
            }
            let block = popup_block(" ◐ theme ", app.theme.accent_thinking)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Palette(p) => {
            let w = area.width.min(66).saturating_sub(2).max(44);
            let max_rows = area.height.saturating_sub(8).max(6) as usize;
            let mut lines: Vec<Line> = vec![Line::from(vec![
                Span::styled(" search: ", Style::default().fg(app.theme.gray)),
                Span::styled(&p.filter, Style::default().fg(app.theme.text_primary)),
            ])];
            lines.push(Line::from(Span::styled(
                "─".repeat(w.saturating_sub(2) as usize),
                Style::default().fg(app.theme.border),
            )));
            let mut previous_section = None;
            for (row_idx, item_idx) in p.visible.iter().take(max_rows).enumerate() {
                let row = &p.rows[*item_idx];
                if previous_section != Some(row.section) {
                    lines.push(Line::from(Span::styled(
                        format!(" {} ", row.section),
                        Style::default()
                            .fg(app.theme.gray_bright)
                            .add_modifier(Modifier::BOLD),
                    )));
                    previous_section = Some(row.section);
                }
                let selected = row_idx == p.selected;
                let mark = if selected { "◆" } else { " " };
                let shortcut = row.shortcut.as_deref().unwrap_or(&row.label);
                let label_budget =
                    w.saturating_sub(8).saturating_sub(shortcut.len() as u16) as usize;
                let label = truncated(&row.action, label_budget.max(8));
                let gap = w
                    .saturating_sub(6)
                    .saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()) as u16)
                    .saturating_sub(unicode_width::UnicodeWidthStr::width(shortcut) as u16)
                    as usize;
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("  {mark} {label}{}{shortcut}", " ".repeat(gap)),
                    style,
                )));
            }
            if p.visible.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  No matching commands",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            let h = (lines.len() as u16 + 2)
                .min(area.height.saturating_sub(2))
                .max(7);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let block = popup_block(" Commands ", app.theme.border)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Shortcuts(s) => {
            let w = area.width.min(82).saturating_sub(2).max(48);
            let h = area.height.min(30).saturating_sub(2).max(14);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" / ", Style::default().fg(app.theme.accent_brand)),
                    Span::styled("to search  ", Style::default().fg(app.theme.gray)),
                    Span::styled(&s.filter, Style::default().fg(app.theme.text_primary)),
                ]),
                Line::from(Span::styled(
                    "─".repeat(w.saturating_sub(2) as usize),
                    Style::default().fg(app.theme.border),
                )),
            ];
            let query = s.filter.to_lowercase();
            for (index, section) in ShortcutSection::ALL.iter().enumerate() {
                let rows = s
                    .rows
                    .iter()
                    .filter(|row| row.section == *section)
                    .filter(|row| {
                        query.is_empty()
                            || row.label.to_lowercase().contains(&query)
                            || row.keys.to_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>();
                if !query.is_empty() && rows.is_empty() {
                    continue;
                }
                let selected = s.selected_section == index;
                let expanded = s.expanded[index] || !query.is_empty();
                let marker = if expanded { "⌄" } else { "›" };
                let heading_style = Style::default()
                    .fg(if selected {
                        app.theme.text_primary
                    } else {
                        app.theme.text_secondary
                    })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    });
                lines.push(Line::from(Span::styled(
                    format!(" {} {} ({})", marker, section.title(), rows.len()),
                    heading_style,
                )));
                if expanded {
                    for row in rows {
                        let label_width = unicode_width::UnicodeWidthStr::width(row.label);
                        let key_width = unicode_width::UnicodeWidthStr::width(row.keys);
                        let gap = (w as usize).saturating_sub(label_width + key_width + 8);
                        lines.push(Line::from(vec![
                            Span::styled("    ◆ ", Style::default().fg(app.theme.accent_brand)),
                            Span::styled(row.label, Style::default().fg(app.theme.text_secondary)),
                            Span::raw(" ".repeat(gap)),
                            Span::styled(row.keys, Style::default().fg(app.theme.gray_bright)),
                        ]));
                    }
                }
            }
            let block = popup_block(" Keyboard Shortcuts ", app.theme.border)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Rewind(r) => {
            let w = area.width.min(80).saturating_sub(4).max(40);
            let h = (r.items.len() as u16 + 3)
                .min(area.height.saturating_sub(4))
                .max(6);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                " rewind — Enter 回滚到该消息之前 · Esc 关闭",
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for (i, item) in r.items.iter().enumerate() {
                let selected = i == r.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{}{} (#{})", mark, item.preview, item.seq),
                    style,
                )));
            }
            let block = popup_block(" ⟲ rewind ", app.theme.accent_plan)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::FilePicker(fp) => {
            let w = area.width.min(80).saturating_sub(4).max(40);
            let h = (fp.visible.len().min(12) as u16 + 3).max(5);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                format!(" @{} — Tab/Enter 插入 · Esc 关闭", fp.query),
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for (row_idx, item_idx) in fp.visible.iter().take(12).enumerate() {
                let row = &fp.files[*item_idx];
                let selected = row_idx == fp.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(format!("{}{}", mark, row), style)));
            }
            if fp.visible.is_empty() {
                lines.push(Line::from(Span::styled(
                    " (无匹配)",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            let block = popup_block(" @ file ", app.theme.accent_user)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Model(m) => {
            let vis = m.visible();
            let w = area.width.min(84).saturating_sub(4).max(40);
            let h = (vis.len() as u16 + 4)
                .min(area.height.saturating_sub(4))
                .max(7);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                format!(" /model — filter: {}", m.filter),
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for item_idx in &vis {
                let row = &m.rows[*item_idx];
                let selected = *item_idx == m.selected;
                let mark = if selected { "› " } else { "  " };
                let context = row
                    .context_window
                    .map(|window| format!(" · {} ctx", compact_number(window)))
                    .unwrap_or_default();
                let description = row
                    .description
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .map(|text| format!(" — {text}"))
                    .unwrap_or_default();
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{mark}{}/{} — {}{context}{description}",
                        row.provider, row.id, row.name
                    ),
                    style,
                )));
            }
            if vis.is_empty() {
                lines.push(Line::from(Span::styled(
                    " (无匹配)",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            let block = popup_block(" ◈ model ", app.theme.accent_assistant)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Effort(effort) => {
            let w = area.width.min(72).saturating_sub(4).max(40);
            let h = (effort.rows.len() as u16 + 6)
                .min(area.height.saturating_sub(4))
                .max(8);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" model  ", Style::default().fg(app.theme.gray_dim)),
                    Span::styled(
                        format!("{}/{}", effort.model.provider, effort.model.id),
                        Style::default()
                            .fg(app.theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    " reasoning effort",
                    Style::default().fg(app.theme.gray),
                )),
                Line::from(""),
            ];
            for (index, row) in effort.rows.iter().enumerate() {
                let selected = index == effort.selected;
                let mark = if selected { "› " } else { "  " };
                let description = row
                    .description
                    .as_deref()
                    .map(|text| format!(" — {text}"))
                    .unwrap_or_default();
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{mark}{}{}", row.name, description),
                    style,
                )));
            }
            let block = popup_block(" ◇ effort ", app.theme.accent_assistant)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Resume(p) => {
            let w = area.width.min(80).saturating_sub(4).max(40);
            let h = (p.items.len() as u16 + 2)
                .min(area.height.saturating_sub(4))
                .max(6);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                " /resume — Enter 恢复 · Esc 关闭",
                Style::default().fg(app.theme.gray),
            ))];
            lines.push(Line::from(""));
            for (i, item) in p.items.iter().enumerate() {
                let selected = i == p.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                let mut label = item.preview.clone();
                if label.is_empty() {
                    label = item.id.clone();
                }
                lines.push(Line::from(Span::styled(
                    format!(
                        "{}{} · {} turns · {} · {}",
                        mark,
                        label,
                        item.turns,
                        age_label(item.modified),
                        item.id
                    ),
                    style,
                )));
            }
            let block = popup_block(" ↺ resume ", app.theme.accent_success)
                .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
        Dialog::Ask(d) => {
            let n = d.questions.len().max(1);
            let cur = d.current.min(n - 1);
            let q = &d.questions[cur];
            let opts = q.options.len();
            let is_plan_review = q.plan_approve.is_some();
            let w = if is_plan_review {
                area.width.saturating_sub(4).clamp(40, 104)
            } else {
                area.width.min(72).saturating_sub(4).max(30)
            };
            let h = if is_plan_review {
                area.height.saturating_sub(4).max(12)
            } else {
                (opts as u16 + 7).min(area.height.saturating_sub(4)).max(9)
            };
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let title = if q.header.is_empty() {
                format!(" 问题 {}/{}{} ", cur + 1, n, if d.parked { " · parked" } else { "" })
            } else {
                format!(" {}{} ", q.header, if d.parked { " · parked" } else { "" })
            };
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                format!(" {}", q.question),
                Style::default().fg(app.theme.text_primary),
            ))];
            lines.push(Line::from(""));
            if is_plan_review && !q.detail.is_empty() {
                let detail_lines: Vec<&str> = q.detail.split('\n').collect();
                let reserved = 7usize + usize::from(d.taking_feedback) * 2;
                let win = rect.height.saturating_sub(reserved as u16).max(3) as usize;
                let max_start = detail_lines.len().saturating_sub(win);
                let start = d.detail_scroll.min(max_start);
                let end = (start + win).min(detail_lines.len());
                for (offset, raw) in detail_lines[start..end].iter().enumerate() {
                    let line_no = start + offset + 1;
                    let content_width = rect.width.saturating_sub(9) as usize;
                    let one = truncated(raw, content_width.max(1));
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {line_no:>4} │ "),
                            Style::default().fg(app.theme.gray_dim),
                        ),
                        Span::styled(
                            one,
                            Style::default().fg(app.theme.gray).bg(app.theme.bg_light),
                        ),
                    ]));
                }
                let position = if detail_lines.is_empty() {
                    "0/0".to_string()
                } else {
                    format!("{}-{}/{}", start + 1, end, detail_lines.len())
                };
                lines.push(Line::from(Span::styled(
                    format!("  {position} · ↑/↓ scroll · c comment line · y copy plan"),
                    Style::default().fg(app.theme.gray_dim),
                )));
                lines.push(Line::from(""));
            }
            if d.taking_feedback {
                lines.push(Line::from(Span::styled(
                    format!(" s: 意见: {}", d.feedback),
                    Style::default().fg(app.theme.accent_plan),
                )));
                lines.push(Line::from(""));
            }
            if d.taking_text {
                lines.push(Line::from(Span::styled(
                    format!(" z: 文本: {}", d.custom_text),
                    Style::default().fg(app.theme.accent_plan),
                )));
                lines.push(Line::from(""));
            }
            for (i, opt) in q.options.iter().enumerate() {
                let chosen = d.answers[cur].contains(&i);
                let focused = d.cursors[cur] == i;
                let cursor = if focused { "› " } else { "  " };
                let box_mark = if q.multi_select {
                    if chosen {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                } else if chosen {
                    "● "
                } else {
                    "○ "
                };
                let style = if focused && !d.parked {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else if chosen {
                    Style::default().fg(app.theme.accent_brand)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{}{}{}. {}", cursor, box_mark, i + 1, opt),
                    style,
                )));
            }
            if q.options.is_empty() {
                lines.push(Line::from(Span::styled(
                    " 按 z 输入自由文本",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            let block = popup_block(
                Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(app.theme.accent_brand)
                        .add_modifier(Modifier::BOLD),
                )),
                app.theme.accent_brand,
            )
            .style(Style::default().bg(app.theme.bg_light));
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_light)),
                rect,
            );
        }
    }
}

fn running_spinner() -> &'static str {
    const GLYPHS: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 140;
    GLYPHS[(tick % 4) as usize]
}

fn compact_number(value: u64) -> String {
    let scaled = |value: f64| {
        if value >= 100.0 {
            format!("{:.0}", value.round())
        } else {
            format!("{:.1}", (value * 10.0).round() / 10.0)
        }
    };
    if value >= 1_000_000 {
        format!("{}M", scaled(value as f64 / 1_000_000.0))
    } else if value >= 1_000 {
        format!("{}K", scaled(value as f64 / 1_000.0))
    } else {
        value.to_string()
    }
}

fn compact_duration(ms: u64) -> String {
    if ms > 0 && ms < 100 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1_000.0;
    if seconds < 60.0 {
        format!("{:.1}s", (seconds * 10.0).round() / 10.0)
    } else {
        let whole = seconds.round() as u64;
        format!("{}m{}s", whole / 60, whole % 60)
    }
}

fn compact_rate(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn stats_groups(app: &App) -> Vec<String> {
    let stats = app.transcript.stats;
    let usage = app.transcript.usage;
    let billed_input = usage
        .input
        .saturating_add(usage.cache)
        .saturating_add(usage.cache_write);
    let has_activity = stats.steps > 0
        || stats.llm_ms > 0
        || stats.tool_ms > 0
        || stats.ttft_steps > 0
        || billed_input > 0
        || usage.output > 0;
    if !has_activity {
        return Vec::new();
    }

    let ttft = stats
        .ttft_ms
        .checked_div(stats.ttft_steps)
        .map(compact_duration)
        .unwrap_or_else(|| "—".to_string());
    let tps = if stats.decode_ms == 0 {
        "—".to_string()
    } else {
        compact_rate(stats.decode_tokens as f64 / (stats.decode_ms as f64 / 1_000.0))
    };
    let cache_hit = if billed_input == 0 {
        "—".to_string()
    } else {
        format!(
            "{}%",
            ((usage.cache as f64 / billed_input as f64) * 100.0).round() as u64
        )
    };

    vec![
        format!("Turns {} · Steps {}", stats.turns, stats.steps),
        format!(
            "LLM {} · Tools {}",
            compact_duration(stats.llm_ms),
            compact_duration(stats.tool_ms)
        ),
        format!("TTFB {ttft} · TPS {tps}"),
        format!("Cache hit {cache_hit}"),
        format!(
            "Input {} · Output {}",
            compact_number(billed_input),
            compact_number(usage.output)
        ),
    ]
}

fn stats_lines(app: &App, width: u16) -> Vec<String> {
    let budget = width.saturating_sub(2).max(1) as usize;
    let mut lines = Vec::new();
    let mut current = String::new();
    for group in stats_groups(app) {
        let candidate = if current.is_empty() {
            group.clone()
        } else {
            format!("{current} | {group}")
        };
        if current.is_empty() || unicode_width::UnicodeWidthStr::width(candidate.as_str()) <= budget
        {
            current = candidate;
        } else {
            lines.push(current);
            current = group;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect, lines: &[String]) {
    let lines = lines
        .iter()
        .map(|line| Line::from(format!("  {line}")))
        .collect::<Vec<_>>();
    f.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(app.theme.gray_dim)
                .bg(app.theme.bg_base),
        ),
        area,
    );
}

fn workspace_label(workspace: &str) -> String {
    let home = std::env::var("HOME").ok();
    if let Some(home) = home.as_deref() {
        if workspace == home {
            return "~".to_string();
        }
        if let Some(rest) = workspace
            .strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return format!("~/{rest}");
        }
    }
    workspace.to_string()
}

/// Top-right context chip and its pressure colour.
///
/// Prefers DSH's `contextPressure` projection: `projectedTokens` is what the
/// NEXT request's prompt will cost, anchored on the provider's own figure and
/// repriced only for the delta — so unlike a running token sum it *drops* the
/// moment a compaction shadows a span. Falls back to the scraped usage totals
/// when no projection has arrived (non-DSH harness, or before the first
/// provider-reported usage).
///
/// The colour bands are visual pressure only. They are deliberately not
/// labelled as the auto-compaction threshold — DSH's actual threshold lives in
/// `dsh-compaction-basic` config and is not read here.
fn context_chip(app: &App) -> (String, ratatui::style::Color) {
    let projection = app.projections.context_pressure;
    let used = projection
        .and_then(|p| p.projected_tokens.or(p.pressure_tokens))
        .unwrap_or_else(|| {
            let usage = app.transcript.usage;
            usage
                .input
                .saturating_add(usage.cache)
                .saturating_add(usage.cache_write)
        });
    let window = projection
        .and_then(|p| p.context_window)
        .or(app.context_window);
    let fraction = app.projections.context_fraction().or_else(|| {
        let window = window?;
        (window > 0).then(|| (used as f64 / window as f64).clamp(0.0, 1.0))
    });
    let color = match fraction {
        Some(f) if f >= 0.90 => app.theme.accent_error,
        Some(f) if f >= 0.70 => app.theme.warning,
        _ => app.theme.gray_bright,
    };
    let text = match (window, fraction) {
        (Some(window), Some(f)) => format!(
            "{} / {} · {}%",
            compact_number(used),
            compact_number(window),
            (f * 100.0).round() as u64
        ),
        (Some(window), None) => format!("{} / {}", compact_number(used), compact_number(window)),
        _ => compact_number(used),
    };
    (text, color)
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (context, context_color) = context_chip(app);
    let context_width = unicode_width::UnicodeWidthStr::width(context.as_str()) as u16;
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(context_width.saturating_add(2)),
        ])
        .split(area);
    let route = workspace_label(&app.workspace);
    let mut model = app.model.clone();
    if let Some(effort) = app.reasoning_effort_name.as_deref() {
        model.push_str(" · ");
        model.push_str(effort);
    }
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ╱ ", Style::default().fg(app.theme.accent_brand)),
            Span::styled(route, Style::default().fg(app.theme.text_secondary)),
            Span::styled("  ", Style::default()),
            Span::styled(
                model,
                Style::default()
                    .fg(app.theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(app.theme.bg_base)),
        split[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{context} "),
            Style::default()
                .fg(context_color)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Right)
        .style(Style::default().bg(app.theme.bg_base)),
        split[1],
    );
}

fn activity_text(app: &App) -> Option<(String, ratatui::style::Color)> {
    if let Some(notice) = app.notice.as_deref().filter(|notice| !notice.is_empty()) {
        return Some((notice.to_string(), app.theme.warning));
    }
    if app.queued_count() > 0 {
        return Some((
            format!(
                "{} queued follow-up{} · Ctrl+; to inspect",
                app.queued_count(),
                if app.queued_count() == 1 { "" } else { "s" }
            ),
            app.theme.accent_plan,
        ));
    }
    match app.state {
        RunState::Running => Some((
            format!("{} Working…  Esc to interrupt", running_spinner()),
            app.theme.accent_running,
        )),
        RunState::Starting => Some(("◌ Starting…".to_string(), app.theme.warning)),
        RunState::Idle => None,
    }
}

fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::raw("  ")];
    if let Some((text, color)) = activity_text(app) {
        spans.push(Span::styled(text, Style::default().fg(color)));
    }
    let occupied = spans.iter().map(|span| span.width()).sum::<usize>();
    let (mode_text, mode_color) = match app.permission_mode {
        crate::app::PermissionMode::Normal => ("Normal", app.theme.gray_bright),
        crate::app::PermissionMode::Plan => ("Plan", app.theme.accent_plan),
        crate::app::PermissionMode::AlwaysApprove => ("Always-approve", app.theme.accent_success),
    };
    let mode_text = format!("{mode_text} mode");
    let mode_width = unicode_width::UnicodeWidthStr::width(mode_text.as_str());
    let gap = (area.width as usize).saturating_sub(occupied + mode_width + 2);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(
        mode_text,
        Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(app.theme.bg_base)),
        area,
    );
}

fn owned_line(source: &Line<'_>, spans: Vec<Span<'static>>) -> Line<'static> {
    Line {
        style: source.style,
        alignment: source.alignment,
        spans,
    }
}

fn flush_span(row: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        row.push(Span::styled(std::mem::take(text), style));
    }
}

/// Wrap styled lines into terminal rows before scroll slicing. Ratatui's
/// Paragraph truncates by default, and slicing first loses the tail whenever a
/// source line expands into multiple visual rows.
fn wrap_lines(lines: &[Line<'_>], width: u16) -> Vec<Line<'static>> {
    let max_width = width.max(1) as usize;
    let mut rows = Vec::new();

    for source in lines {
        let (continuation, continuation_style) = source
            .spans
            .first()
            .map(|span| {
                let indent: String = span
                    .content
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .collect();
                (indent, span.style)
            })
            .unwrap_or_default();
        let continuation_width = unicode_width::UnicodeWidthStr::width(continuation.as_str());
        let continuation = (continuation_width < max_width).then_some(continuation);

        let mut row = Vec::new();
        let mut row_width = 0usize;
        let mut saw_content = false;

        for span in &source.spans {
            let mut segment = String::new();
            for ch in span.content.chars() {
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if ch_width > 0 && row_width > 0 && row_width + ch_width > max_width {
                    flush_span(&mut row, &mut segment, span.style);
                    rows.push(owned_line(source, std::mem::take(&mut row)));
                    if let Some(indent) = continuation.as_ref().filter(|s| !s.is_empty()) {
                        row.push(Span::styled(indent.clone(), continuation_style));
                        row_width = continuation_width;
                    } else {
                        row_width = 0;
                    }
                }
                segment.push(ch);
                row_width = row_width.saturating_add(ch_width);
                saw_content = true;
            }
            flush_span(&mut row, &mut segment, span.style);
        }

        if !row.is_empty() || !saw_content {
            rows.push(owned_line(source, row));
        }
    }

    rows
}

#[derive(Debug, Clone, Copy)]
struct CellRowRange {
    start: usize,
    end: usize,
}

fn transcript_rows(
    app: &App,
    t: &Transcript,
    width: u16,
) -> (Vec<Line<'static>>, Vec<CellRowRange>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::with_capacity(t.cells.len());
    for (index, cell) in t.cells.iter().enumerate() {
        let one = Transcript::from_cell(cell.clone(), t.selected == Some(index));
        let start = rows.len();
        rows.extend(wrap_lines(&transcript_lines(app, &one), width));
        let end = rows.len();
        ranges.push(CellRowRange { start, end });
        if index + 1 < t.cells.len() {
            rows.push(Line::styled("", Style::default().bg(app.theme.bg_base)));
        }
    }
    (rows, ranges)
}

/// Body of an expanded tool card. Diff-shaped output gets the dedicated
/// `diff_*` slots; everything else stays neutral.
fn tool_body_lines(app: &App, text: &str) -> Vec<Line<'static>> {
    let is_diff = crate::toolcard::diff_stat(text).is_some();
    text.split('\n')
        .map(|raw| crate::toolcard::body_line(raw, is_diff, &app.theme, "      "))
        .collect()
}

fn scroll_for_selection(
    rows_len: usize,
    height: usize,
    current_scroll: usize,
    selected: Option<usize>,
    ranges: &[CellRowRange],
) -> usize {
    let Some(range) = selected.and_then(|index| ranges.get(index)).copied() else {
        return 0;
    };
    let max_scroll = rows_len.saturating_sub(height);
    let current_scroll = current_scroll.min(max_scroll);
    let viewport_end = rows_len.saturating_sub(current_scroll);
    let viewport_start = viewport_end.saturating_sub(height);
    if range.start < viewport_start {
        rows_len
            .saturating_sub(range.start.saturating_add(height))
            .min(max_scroll)
    } else if range.end > viewport_end {
        rows_len.saturating_sub(range.end).min(max_scroll)
    } else {
        current_scroll
    }
}
fn tail_window<'a>(rows: &[Line<'a>], height: usize, scroll: usize) -> Vec<Line<'a>> {
    if height == 0 {
        return Vec::new();
    }
    let end = rows.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    rows[start..end].to_vec()
}

/// 将会话单元转换成低噪声的 Grok 风格行：用户消息是整行带状卡片，
/// assistant 正文不再套高亮背景，工具与思考块保持紧凑的事件行。
fn transcript_lines(app: &App, t: &Transcript) -> Vec<Line<'static>> {
    let selected = t.selected;
    let mut lines = Vec::new();
    for (index, cell) in t.cells.iter().enumerate() {
        let active = app.focus == Focus::Scrollback && selected == Some(index);
        match cell.kind {
            CellKind::User => {
                let bg = if active {
                    app.theme.bg_highlight
                } else {
                    app.theme.bg_light
                };
                let mut body = cell.text.split('\n');
                let first = body.next().unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled("  › ", Style::default().fg(app.theme.accent_user).bg(bg)),
                    Span::styled(
                        first.to_string(),
                        Style::default().fg(app.theme.text_primary).bg(bg),
                    ),
                ]));
                for raw in body {
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default().bg(bg)),
                        Span::styled(
                            raw.to_string(),
                            Style::default().fg(app.theme.text_primary).bg(bg),
                        ),
                    ]));
                }
            }
            CellKind::Assistant => {
                let body_style = Style::default()
                    .fg(app.theme.text_primary)
                    .bg(app.theme.bg_base);
                let body = crate::markdown::render_cached(&cell.text, &app.theme, body_style);
                for mut line in body {
                    let mut spans = vec![Span::styled("    ", body_style)];
                    spans.append(&mut line.spans);
                    line.spans = spans;
                    line.style = line.style.patch(Style::default().bg(app.theme.bg_base));
                    lines.push(line);
                }
            }
            kind => {
                let accent = match kind {
                    CellKind::Thinking => app.theme.accent_thinking,
                    CellKind::Tool | CellKind::ToolResult => {
                        if cell.failed {
                            app.theme.accent_error
                        } else {
                            app.theme.accent_tool
                        }
                    }
                    CellKind::Notice => app.theme.warning,
                    CellKind::Subagent => app.theme.accent_plan,
                    CellKind::User | CellKind::Assistant => app.theme.text_primary,
                };
                let fold = if cell.folded { "▸" } else { "▾" };
                let mut title = match kind {
                    CellKind::Thinking => "Thought".to_string(),
                    CellKind::Tool => cell.title.clone(),
                    CellKind::ToolResult if cell.failed => "Failed".to_string(),
                    CellKind::ToolResult => "Result".to_string(),
                    CellKind::Subagent => "Subagent".to_string(),
                    CellKind::Notice if cell.title.is_empty() => "Notice".to_string(),
                    CellKind::Notice => cell.title.clone(),
                    CellKind::User | CellKind::Assistant => String::new(),
                };
                if cell.raw {
                    title.push_str(" · raw");
                }
                let marker = if active { "  ›" } else { "  ◆" };
                // Grok stamps the collapsed diff count into the header itself,
                // so a folded edit says what it changed without expanding.
                let stat = (!cell.raw && matches!(kind, CellKind::Tool | CellKind::ToolResult))
                    .then(|| crate::toolcard::diff_stat(&cell.text))
                    .flatten();
                let mut header = vec![
                    Span::styled(marker, Style::default().fg(accent)),
                    Span::styled(
                        format!(" {fold} {title}"),
                        Style::default()
                            .fg(if active {
                                app.theme.text_primary
                            } else {
                                app.theme.text_secondary
                            })
                            .add_modifier(if active {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ];
                if let Some((added, removed)) = stat {
                    header.push(Span::styled(
                        format!(" +{added}"),
                        Style::default().fg(app.theme.diff_insert_fg),
                    ));
                    header.push(Span::styled(
                        format!(" -{removed}"),
                        Style::default().fg(app.theme.diff_delete_fg),
                    ));
                }
                lines.push(Line::from(header));
                let content = if cell.raw && !cell.raw_text.is_empty() {
                    &cell.raw_text
                } else {
                    &cell.text
                };
                if cell.folded {
                    // Per-tool collapse: execute keeps head/tail of its output,
                    // a diff shrinks to its stat, everything else to one line.
                    let kind_of = cell
                        .tool
                        .as_deref()
                        .map(crate::toolcard::ToolKind::classify)
                        .unwrap_or(crate::toolcard::ToolKind::Other);
                    for row in crate::toolcard::fold_preview(kind_of, content) {
                        // Skip a preview the header already states — a folded
                        // `Edit src/lib.rs` should not then say `src/lib.rs`,
                        // and a `+4 -1` stat should not print twice.
                        if stat.is_some() && row.starts_with('+') && row.contains(" -") {
                            continue;
                        }
                        if title.ends_with(row.as_str()) {
                            continue;
                        }
                        lines.push(Line::from(Span::styled(
                            format!("      {}", truncated(&row, 76)),
                            Style::default().fg(app.theme.gray_dim),
                        )));
                    }
                } else if matches!(kind, CellKind::Tool | CellKind::ToolResult) && !cell.raw {
                    lines.extend(tool_body_lines(app, content));
                } else {
                    for raw in content.split('\n') {
                        lines.push(Line::from(Span::styled(
                            format!("      {raw}"),
                            Style::default().fg(app.theme.text_secondary),
                        )));
                    }
                }
            }
        }
        if index + 1 < t.cells.len() {
            lines.push(Line::styled("", Style::default().bg(app.theme.bg_base)));
        }
    }
    lines
}

fn draw_welcome(f: &mut Frame, app: &App, area: Rect) {
    let project = std::path::Path::new(&app.workspace)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&app.workspace);
    let brand = Style::default()
        .fg(app.theme.accent_brand)
        .add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::from(vec![
            Span::styled("   ╭──╮ ", brand),
            Span::styled(
                "Whale TUI",
                Style::default()
                    .fg(app.theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ◁    ▷", brand),
            Span::styled(
                "  DeepSeek Harness",
                Style::default().fg(app.theme.text_secondary),
            ),
        ]),
        Line::from(Span::styled("   ╰──╯", brand)),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Ready in ", Style::default().fg(app.theme.gray)),
            Span::styled(
                project.to_string(),
                Style::default().fg(app.theme.text_primary),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  New session",
                Style::default().fg(app.theme.text_secondary),
            ),
            Span::styled("   Ctrl+N", Style::default().fg(app.theme.gray_bright)),
        ]),
        Line::from(vec![
            Span::styled(
                "  Resume session",
                Style::default().fg(app.theme.text_secondary),
            ),
            Span::styled("Ctrl+S", Style::default().fg(app.theme.gray_bright)),
        ]),
        Line::from(vec![
            Span::styled("  Commands", Style::default().fg(app.theme.text_secondary)),
            Span::styled("      Ctrl+P", Style::default().fg(app.theme.gray_bright)),
        ]),
    ];
    let content_width = lines.iter().map(Line::width).max().unwrap_or(1) as u16;
    let width = content_width.saturating_add(4).min(area.width).max(24);
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = centered_rect(width, height, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(app.theme.border)),
            )
            .style(Style::default().bg(app.theme.bg_base)),
        rect,
    );
}

fn draw_scrollback(f: &mut Frame, app: &mut App, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let text_area = split[0];
    let bar_area = split[1];

    if app.transcript.is_empty() {
        draw_welcome(f, app, text_area);
        return;
    }

    let (rows, ranges) = transcript_rows(app, &app.transcript, text_area.width.saturating_sub(1));
    let height = text_area.height as usize;
    if app.follow_selection && app.focus == Focus::Scrollback {
        app.scroll = scroll_for_selection(
            rows.len(),
            height,
            app.scroll,
            app.transcript.selected,
            &ranges,
        );
    } else {
        app.scroll = app.scroll.min(rows.len().saturating_sub(height));
    }
    let view = tail_window(&rows, height, app.scroll);
    f.render_widget(
        Paragraph::new(view).style(Style::default().bg(app.theme.bg_base)),
        text_area,
    );

    if rows.len() > height {
        let start = rows.len().saturating_sub(app.scroll).saturating_sub(height);
        let mut state = ScrollbarState::new(rows.len())
            .position(start.min(rows.len().saturating_sub(height)))
            .viewport_content_length(height);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("│"))
                .thumb_symbol("▐")
                .style(Style::default().fg(app.theme.gray_dim)),
            bar_area,
            &mut state,
        );
    }
}

fn draw_composer(f: &mut Frame, app: &App, area: Rect) {
    let border_color = match app.focus {
        Focus::Prompt => app.theme.prompt_border_active,
        Focus::Scrollback => app.theme.prompt_border,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(app.theme.bg_light));
    let inner_width = area.width.saturating_sub(5).max(1);
    let layout = composer_layout(&app.input, app.cursor, inner_width);
    let all_rows = &layout.rows;
    // Anchor on the tail, then pull the window up if the caret sits above it —
    // a long draft has to stay editable at the top, not just at the end.
    let view_rows = all_rows.len().min(COMPOSER_VIEW_ROWS);
    let mut visible_start = all_rows.len().saturating_sub(view_rows);
    if layout.cursor_row < visible_start {
        visible_start = layout.cursor_row;
    }
    let visible_rows = &all_rows[visible_start..(visible_start + view_rows).min(all_rows.len())];
    let mut lines = Vec::with_capacity(visible_rows.len());
    for (index, raw) in visible_rows.iter().enumerate() {
        let first_visual_row = visible_start + index == 0;
        let prefix = if first_visual_row { "› " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(if app.focus == Focus::Prompt {
                        app.theme.accent_brand
                    } else {
                        app.theme.gray
                    })
                    .bg(app.theme.bg_light)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                raw.clone(),
                Style::default()
                    .fg(app.theme.text_primary)
                    .bg(app.theme.bg_light),
            ),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(app.theme.bg_light)),
        area,
    );
    if app.focus == Focus::Prompt && !app.has_dialog() {
        let gutter = prefix_width("› ") as u16;
        let x = area
            .x
            .saturating_add(1)
            .saturating_add(gutter)
            .saturating_add(layout.cursor_col as u16);
        let y = area
            .y
            .saturating_add(1)
            .saturating_add(layout.cursor_row.saturating_sub(visible_start) as u16);
        f.set_cursor_position((
            x.min(area.right().saturating_sub(2)),
            y.min(area.bottom().saturating_sub(2)),
        ));
    }
}

fn prefix_width(prefix: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(prefix)
}

fn draw_shortcuts(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.dialog {
        Dialog::Approval(d) => {
            if d.parked {
                "↑/↓ select · h/l fold · Tab permission card · Ctrl+Q quit"
            } else {
                "↑/↓ select · 1-9 pick · Enter confirm · Esc park · Ctrl+C cancel · Ctrl+Q quit"
            }
        }
        Dialog::Resume(_) => "↑/↓ select · Enter resume · Esc close · Ctrl+Q quit",
        Dialog::Model(_) => {
            "type filter · ↑/↓ select · Enter choose effort · Esc close · Ctrl+Q quit"
        }
        Dialog::Block(_) => "↑/↓ scroll · r formatted/raw · y copy · Enter/Esc close",
        Dialog::Effort(_) => "↑/↓ select · Enter switch · Esc back · Ctrl+Q quit",
        Dialog::FilePicker(_) => {
            "type filter · ↑/↓ select · Tab/Enter insert · Esc close · Ctrl+Q quit"
        }
        Dialog::Rewind(_) => "↑/↓ select · Enter rewind · Esc close · Ctrl+Q quit",
        Dialog::Palette(_) => "type filter · ↑/↓ nav · Enter select · Esc close",
        Dialog::History(_) => "type search · ↑/↓ select · Enter/Tab edit · Delete remove · Esc close",
        Dialog::Shortcuts(_) => "↑/↓ nav · e/Space/→ expand · ← collapse · / search · Esc close",
        Dialog::Info(_) => "q/Enter/Esc close · Ctrl+Q quit",
        Dialog::Subagent(_) => "↑/↓ scroll · q/Esc back · Ctrl+Q quit",
        Dialog::Tasks(_) => "↑/↓ select · r refresh · q/Esc close · Ctrl+Q quit",
        Dialog::Todos(_) => "↑/↓ select · g/G ends · y copy list · q/Esc close",
        Dialog::Queue(view) => {
            if view.editing {
                "type edit · Enter save · Esc cancel edit · Ctrl+Q quit"
            } else {
                "↑/↓ select · Enter send now · s steer · e edit · d remove · Esc close"
            }
        }
        Dialog::Theme(_) => "↑/↓ preview · Enter keep · Esc revert · Ctrl+Q quit",
        Dialog::Ask(d) => {
            if d.parked {
                "↑/↓ select · h/l fold · Tab question card · Ctrl+Q quit"
            } else {
                let cur = d.current.min(d.questions.len().saturating_sub(1));
                if d.questions[cur].plan_approve.is_some() {
                    "a approve · s request changes · c comment · y copy · Esc park · q reject"
                } else {
                    "↑/↓ select · ←/→ question · Space toggle · z text · Enter submit · Esc park"
                }
            }
        }
        Dialog::None => match app.focus {
            Focus::Prompt if app.multiline => {
                "Enter:newline  │  Alt+Enter:send  │  Ctrl+Enter:send-now  │  Ctrl+;:queue"
            }
            Focus::Prompt => {
                "Enter:send/queue  │  Ctrl+Enter:send-now  │  Ctrl+;:queue  │  Ctrl+P:commands"
            }
            Focus::Scrollback => "↑/↓:select  │  h/l/e:fold  │  r:raw  │  Enter:view  │  y:copy",
        },
    };
    let hint = if unicode_width::UnicodeWidthStr::width(hint) > area.width as usize {
        match &app.dialog {
            Dialog::None if area.width < 52 => match app.focus {
                Focus::Prompt if app.multiline => "Enter newline · Alt+Enter send · Tab scrollback",
                Focus::Prompt => "Enter send · Ctrl+M multiline · Tab scrollback",
                Focus::Scrollback => "↑/↓ select · e fold · Tab prompt",
            },
            Dialog::None => match app.focus {
                Focus::Prompt if app.multiline => {
                    "Enter:newline  │  Alt+Enter:send  │  Ctrl+M:single-line"
                }
                Focus::Prompt => "Ctrl+M:multiline  │  Ctrl+P:commands  │  Ctrl+X:shortcuts",
                Focus::Scrollback => "↑/↓:select  │  h/l/e:fold  │  g/G:ends  │  y:copy",
            },
            Dialog::Ask(d)
                if d.questions
                    .get(d.current)
                    .is_some_and(|question| question.plan_approve.is_some()) =>
            {
                "a approve · s changes · q close · Ctrl+Q quit"
            }
            Dialog::Ask(_) => "↑/↓ select · Enter submit · Esc skip · Ctrl+Q quit",
            Dialog::Approval(_) => "↑/↓ select · Enter confirm · Esc park · Ctrl+Q quit",
            _ if area.width < 52 => "Enter confirm · Esc close · Ctrl+Q quit",
            _ => "↑/↓ select · Enter confirm · Esc close · Ctrl+Q quit",
        }
    } else {
        hint
    };
    let hint = if app.term_kind.is_vscode_family() {
        hint.replace("Ctrl+Q", "Ctrl+D")
    } else {
        hint.to_string()
    };
    f.render_widget(
        Paragraph::new(format!("  {hint}"))
            .style(Style::default().fg(app.theme.gray).bg(app.theme.bg_base)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    use super::*;
    use crate::theme::DARK;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::channel();
        App::new(
            DARK,
            "test-session".into(),
            "test-provider".into(),
            "test-model".into(),
            true,
            tx,
            ".".into(),
        )
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn draw_uses_base_canvas_and_framed_composer() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer.cell((30, 10)).unwrap().bg, DARK.bg_base);
        assert_eq!(buffer.cell((0, 16)).unwrap().symbol(), "╭");
        assert_eq!(buffer.cell((30, 17)).unwrap().bg, DARK.bg_light);
        assert_eq!(buffer.cell((59, 19)).unwrap().bg, DARK.bg_base);
        let footer = (0..60)
            .filter_map(|x| buffer.cell((x, 19)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(footer.contains("Ctrl+X:shortcuts"));
    }

    #[test]
    fn tool_cards_get_per_tool_headers_stats_and_head_tail_folds() {
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        let long_output = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        for event in [
            serde_json::json!({"type":"tool/call","seq":1,"time":10,
                "data":{"callId":"e1","name":"edit","arguments":"{\"file_path\":\"src/lib.rs\"}"}}),
            serde_json::json!({"type":"tool/result","seq":2,"time":20,
                "data":{"message":{"source":{"kind":"tool","callId":"e1"},"content":[
                    {"type":"tool-result","toolCallId":"e1","content":[
                        {"type":"text","text":"@@ -1 +1,3 @@\n-old\n+new\n+more\n"}]}]}}}),
            serde_json::json!({"type":"tool/call","seq":3,"time":30,
                "data":{"callId":"b1","name":"bash","arguments":"{\"command\":\"cargo test --all\"}"}}),
            serde_json::json!({"type":"tool/result","seq":4,"time":40,
                "data":{"message":{"source":{"kind":"tool","callId":"b1"},"content":[
                    {"type":"tool-result","toolCallId":"b1","content":[
                        {"type":"text","text":long_output}]}]}}}),
        ] {
            app.transcript.apply(&event);
        }

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let dump = terminal.backend().to_string();

        // per-tool headers, not bare tool names
        assert!(dump.contains("Edit src/lib.rs"), "edit header:\n{dump}");
        assert!(dump.contains("Run cargo test --all"), "run header:\n{dump}");
        // diff stat lands in the header
        assert!(dump.contains("+2 -1"), "diff stat in header:\n{dump}");
        // …and is not repeated in the folded body
        assert_eq!(
            dump.matches("+2 -1").count(),
            1,
            "diff stat printed twice:\n{dump}"
        );
        // the header already names the file/command, so the fold adds nothing
        assert_eq!(
            dump.matches("src/lib.rs").count(),
            1,
            "path echoed under its own header:\n{dump}"
        );
        // execute output folds to head + ellipsis + tail
        assert!(dump.contains("line 1"), "head kept:\n{dump}");
        assert!(dump.contains("… 7 more lines"), "ellipsis row:\n{dump}");
        assert!(dump.contains("line 12"), "tail kept:\n{dump}");
        assert!(!dump.contains("line 6"), "middle should be hidden:\n{dump}");
    }

    #[test]
    fn context_chip_prefers_the_projection_and_reacts_to_compaction() {
        let mut app = test_app();
        app.context_window = Some(100_000);
        app.transcript.usage.input = 40_000;

        // no projection yet: fall back to the scraped usage sum
        let (text, color) = context_chip(&app);
        assert!(text.contains("40.0K / 100K"), "fallback text: {text}");
        assert!(text.contains("40%"), "fallback percent: {text}");
        assert_eq!(color, DARK.gray_bright);

        // the projection wins, including its own window
        app.projections.apply(
            "contextPressure",
            &serde_json::json!({
                "pressureTokens": 150_000, "projectedTokens": 160_000, "contextWindow": 200_000
            }),
            1,
        );
        let (text, color) = context_chip(&app);
        assert!(text.contains("160K / 200K"), "projection text: {text}");
        assert!(text.contains("80%"), "projection percent: {text}");
        assert_eq!(color, DARK.warning, "80% is the warning band");

        // a compaction shadows a span: projectedTokens drops even though the
        // provider-reported pressure has not been re-sampled
        app.projections.apply(
            "contextPressure",
            &serde_json::json!({
                "pressureTokens": 150_000, "projectedTokens": 20_000, "contextWindow": 200_000
            }),
            2,
        );
        let (text, color) = context_chip(&app);
        assert!(text.contains("20.0K / 200K"), "after compaction: {text}");
        assert!(text.contains("10%"), "after compaction: {text}");
        assert_eq!(color, DARK.gray_bright, "pressure relieved");

        // over the top band
        app.projections.apply(
            "contextPressure",
            &serde_json::json!({"projectedTokens": 195_000, "contextWindow": 200_000}),
            3,
        );
        assert_eq!(context_chip(&app).1, DARK.accent_error);
    }

    #[test]
    fn context_chip_shows_a_bare_count_when_no_window_is_known() {
        let mut app = test_app();
        app.context_window = None;
        app.transcript.usage.input = 1_234;
        let (text, _) = context_chip(&app);
        assert!(!text.contains('/'), "no window means no ratio: {text}");
        assert!(!text.contains('%'), "and no percent: {text}");

        // a projection with tokens but no advertised window behaves the same
        app.projections.apply(
            "contextPressure",
            &serde_json::json!({"projectedTokens": 5_000}),
            1,
        );
        let (text, color) = context_chip(&app);
        assert!(text.contains("5.0K"), "{text}");
        assert!(!text.contains('%'), "{text}");
        assert_eq!(color, DARK.gray_bright);
    }

    #[test]
    fn truncated_never_exceeds_its_budget_including_the_ellipsis() {
        use unicode_width::UnicodeWidthStr;
        for budget in 0..12usize {
            for text in ["", "abc", "abcdefghijklmnop", "你好世界你好世界", "a你b好c"] {
                let out = truncated(text, budget);
                assert!(
                    UnicodeWidthStr::width(out.as_str()) <= budget,
                    "budget {budget} exceeded by {out:?} from {text:?}"
                );
            }
        }
        // exact fits are returned whole, with no ellipsis
        assert_eq!(truncated("abcde", 5), "abcde");
        assert_eq!(truncated("abcdef", 5), "abcd…");
        // wide glyphs are not split
        assert_eq!(truncated("你好世界", 5), "你好…");
    }

    #[test]
    fn goal_bar_appears_only_with_a_goal_and_shows_rounds_or_block_reason() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();

        // no goal: the bar must not steal a row
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let without = terminal.backend().to_string();
        assert!(!without.contains("goal"), "no goal, no bar:\n{without}");

        app.projections.apply(
            "goal",
            &serde_json::json!({
                "goal": {"id":"g1","revision":1,"objective":"ship the TUI",
                         "phase":"active","maxGoalRounds":10},
                "roundsStarted": 3, "createdAt": 1, "updatedAt": 2
            }),
            1,
        );
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let active = terminal.backend().to_string();
        assert!(active.contains("ship the TUI"), "objective:\n{active}");
        assert!(active.contains("round 3/10"), "round counter:\n{active}");

        // blocked: the reason replaces the counter, since it is the actionable bit
        app.projections.apply(
            "goal",
            &serde_json::json!({
                "goal": {"id":"g1","revision":2,"objective":"ship the TUI",
                         "phase":"blocked","maxGoalRounds":10,
                         "blockedReason":{"code":"needs-input","message":"waiting on approval"}},
                "roundsStarted": 3, "createdAt": 1, "updatedAt": 3
            }),
            2,
        );
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let blocked = terminal.backend().to_string();
        assert!(blocked.contains("waiting on approval"), "reason:\n{blocked}");
        assert!(!blocked.contains("round 3/10"), "counter yields to reason:\n{blocked}");
    }

    #[test]
    fn goal_bar_keeps_the_round_counter_on_a_narrow_terminal() {
        let backend = TestBackend::new(40, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.projections.apply(
            "goal",
            &serde_json::json!({
                "goal": {"id":"g1","revision":1,
                         "objective":"a very long objective that cannot possibly fit in forty columns",
                         "phase":"active","maxGoalRounds":9},
                "roundsStarted": 7, "createdAt": 1, "updatedAt": 2
            }),
            1,
        );
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let dump = terminal.backend().to_string();
        assert!(
            dump.contains("round 7/9"),
            "the objective must yield, not the counter:\n{dump}"
        );
    }

    #[test]
    fn todos_pane_marks_status_with_distinct_colours() {
        let backend = TestBackend::new(70, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.transcript.apply(&serde_json::json!({
            "type": "tool/call", "seq": 1, "time": 10,
            "data": {"callId": "c1", "name": "todo_write",
                     "arguments": "{\"todos\":[{\"content\":\"done one\",\"status\":\"completed\"},{\"content\":\"doing two\",\"status\":\"in_progress\"},{\"content\":\"later\",\"status\":\"pending\"}]}"}
        }));
        app.dialog = crate::app::Dialog::Todos(crate::app::TodosView { selected: 1 });

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let dump = terminal.backend().to_string();

        assert!(dump.contains("doing two"), "pane body missing:\n{dump}");
        assert!(dump.contains("1/3"), "progress count missing:\n{dump}");

        let mut fgs = std::collections::HashSet::new();
        let mut bgs = std::collections::HashSet::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    fgs.insert(cell.fg);
                    bgs.insert(cell.bg);
                }
            }
        }
        assert!(fgs.contains(&DARK.accent_success), "completed marker colour");
        assert!(fgs.contains(&DARK.accent_running), "in-progress marker colour");
        assert!(bgs.contains(&DARK.bg_highlight), "selected row highlight");
    }

    #[test]
    fn draw_paints_code_blocks_with_syntax_colours_from_the_theme() {
        let backend = TestBackend::new(100, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.transcript.push(
            CellKind::Assistant,
            String::new(),
            "fix:\n\n```rust\nfn go() {\n    let s = \"hi\"; // note\n}\n```\n".to_string(),
        );

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut seen = std::collections::HashSet::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    seen.insert(cell.fg);
                }
            }
        }

        for (label, color) in [
            ("keyword", DARK.syn_keyword),
            ("string", DARK.syn_string),
            ("comment", DARK.syn_comment),
            ("function", DARK.syn_function),
        ] {
            assert!(seen.contains(&color), "{label} colour missing from the frame");
        }
        // and the slab background is still there behind the code
        let backgrounds: std::collections::HashSet<_> = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| buffer.cell((x, y)).map(|c| c.bg))
            .collect();
        assert!(backgrounds.contains(&DARK.code_bg), "code_bg slab missing");
    }

    #[test]
    fn draw_renders_deepseek_blue_on_active_controls() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer.cell((0, 16)).unwrap().fg, DARK.accent_brand);
        assert_eq!(buffer.cell((1, 17)).unwrap().fg, DARK.accent_brand);
        assert_ne!(buffer.cell((0, 16)).unwrap().fg, Color::Magenta);
        assert_ne!(buffer.cell((0, 16)).unwrap().fg, Color::LightMagenta);
    }

    #[test]
    fn draw_preserves_light_theme_hierarchy() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.theme = crate::theme::LIGHT;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(
            buffer.cell((30, 10)).unwrap().bg,
            crate::theme::LIGHT.bg_base
        );
        assert_eq!(
            buffer.cell((30, 17)).unwrap().bg,
            crate::theme::LIGHT.bg_light
        );
        assert_eq!(
            buffer.cell((0, 16)).unwrap().fg,
            crate::theme::LIGHT.prompt_border_active
        );
    }

    #[test]
    fn wrap_lines_respects_cjk_width_and_span_style() {
        let style = Style::default().fg(Color::Cyan);
        let input = vec![Line::from(Span::styled("你好世界你好", style))];
        let rows = wrap_lines(&input, 6);

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.width() <= 6));
        assert_eq!(
            rows.iter().map(line_text).collect::<String>(),
            "你好世界你好"
        );
        assert!(rows
            .iter()
            .flat_map(|row| row.spans.iter())
            .all(|span| span.style == style));
    }

    #[test]
    fn tail_window_slices_wrapped_visual_rows() {
        let input = vec![Line::from("abcdefghijklmno"), Line::from("TAIL")];
        let rows = wrap_lines(&input, 5);
        let visible = tail_window(&rows, 2, 0);

        assert_eq!(
            visible.iter().map(line_text).collect::<Vec<_>>(),
            ["klmno", "TAIL"]
        );
    }

    #[test]
    fn composer_height_tracks_wrapped_and_multiline_input() {
        assert_eq!(composer_height("", 20), 3);
        assert_eq!(composer_height("first\nsecond", 20), 4);
        assert_eq!(composer_height("你好世界你好", 6), 4);
        assert_eq!(composer_height(&"x".repeat(200), 10), 7);
    }

    #[test]
    fn composer_rows_preserve_wrapped_input_instead_of_truncating_it() {
        assert_eq!(composer_rows("abcdefghijk", 5), vec!["abcde", "fghij", "k"]);
        assert_eq!(composer_rows("你好世界", 4), vec!["你好", "世界"]);
    }

    #[test]
    fn composer_layout_places_the_caret_on_the_right_wrapped_row() {
        // caret at the end
        let end = composer_layout("abcdefghijk", 11, 5);
        assert_eq!((end.cursor_row, end.cursor_col), (2, 1));

        // caret mid-draft, inside the first wrapped row
        let mid = composer_layout("abcdefghijk", 2, 5);
        assert_eq!((mid.cursor_row, mid.cursor_col), (0, 2));

        // a caret sitting exactly on a wrap point belongs to the next row,
        // not off the right edge of the previous one
        let wrap = composer_layout("abcdefghijk", 5, 5);
        assert_eq!((wrap.cursor_row, wrap.cursor_col), (1, 0));

        // wide glyphs advance the column by their display width
        let cjk = composer_layout("你好世界", 6, 8);
        assert_eq!((cjk.cursor_row, cjk.cursor_col), (0, 4));

        // caret before an explicit break stays at the end of that line
        let nl = composer_layout("ab\ncd", 2, 10);
        assert_eq!((nl.cursor_row, nl.cursor_col), (0, 2));
        let after_nl = composer_layout("ab\ncd", 3, 10);
        assert_eq!((after_nl.cursor_row, after_nl.cursor_col), (1, 0));
    }

    #[test]
    fn composer_scrolls_up_to_keep_an_offscreen_caret_visible() {
        let backend = TestBackend::new(40, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        // eight logical lines against a five-row composer window
        app.set_input("L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8");
        app.cursor = 0;

        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let dump = terminal.backend().to_string();
        assert!(
            dump.contains("L1"),
            "caret at the top must pull the composer window up:\n{dump}"
        );
        assert!(
            !dump.contains("L8"),
            "the tail should have scrolled out of view:\n{dump}"
        );

        app.cursor = app.input.len();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let dump = terminal.backend().to_string();
        assert!(
            dump.contains("L8"),
            "caret at the end shows the tail again:\n{dump}"
        );
    }

    #[test]
    fn selection_scroll_only_moves_when_selected_block_leaves_viewport() {
        let ranges = vec![
            CellRowRange { start: 0, end: 2 },
            CellRowRange { start: 5, end: 7 },
            CellRowRange { start: 10, end: 12 },
        ];

        assert_eq!(scroll_for_selection(12, 5, 0, Some(2), &ranges), 0);
        assert_eq!(scroll_for_selection(12, 5, 0, Some(0), &ranges), 7);
        assert_eq!(scroll_for_selection(12, 5, 7, Some(1), &ranges), 5);
    }

    #[test]
    fn status_bar_shows_effort_context_and_all_metrics() {
        let backend = TestBackend::new(180, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.model = "gpt-5.6-sol".into();
        app.reasoning_effort_name = Some("High".into());
        app.context_window = Some(400_000);
        app.transcript.stats.turns = 3;
        app.transcript.stats.steps = 8;
        app.transcript.stats.llm_ms = 45_230;
        app.transcript.stats.tool_ms = 3_000;
        app.transcript.stats.ttft_ms = 2_400;
        app.transcript.stats.ttft_steps = 3;
        app.transcript.stats.decode_ms = 5_000;
        app.transcript.stats.decode_tokens = 100;
        app.transcript.usage.input = 100;
        app.transcript.usage.cache = 900;
        app.transcript.usage.output = 50;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let top = (0..180)
            .filter_map(|x| buffer.cell((x, 0)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        let stats = (0..180)
            .filter_map(|x| buffer.cell((x, 1)))
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(top.contains("gpt-5.6-sol · High"));
        assert!(top.contains("1.0K / 400K"));
        assert!(stats.contains("Turns 3 · Steps 8"));
        assert!(stats.contains("LLM 45.2s · Tools 3.0s"));
        assert!(stats.contains("TTFB 0.8s · TPS 20"));
        assert!(stats.contains("Cache hit 90%"));
        assert!(stats.contains("Input 1.0K · Output 50"));
    }
    #[test]
    fn stats_line_shows_all_requested_session_metrics() {
        let mut app = test_app();
        app.transcript.stats.turns = 3;
        app.transcript.stats.steps = 8;
        app.transcript.stats.llm_ms = 45_230;
        app.transcript.stats.tool_ms = 3_000;
        app.transcript.stats.ttft_ms = 2_400;
        app.transcript.stats.ttft_steps = 3;
        app.transcript.stats.decode_ms = 5_000;
        app.transcript.stats.decode_tokens = 100;
        app.transcript.usage.input = 100;
        app.transcript.usage.cache = 900;
        app.transcript.usage.output = 50;

        assert_eq!(
            stats_groups(&app).join(" | "),
            "Turns 3 · Steps 8 | LLM 45.2s · Tools 3.0s | TTFB 0.8s · TPS 20 | Cache hit 90% | Input 1.0K · Output 50"
        );
    }

    #[test]
    fn narrow_status_wraps_without_losing_metrics() {
        let mut app = test_app();
        app.transcript.stats.turns = 1;
        app.transcript.stats.steps = 2;
        app.transcript.stats.llm_ms = 5_000;
        app.transcript.stats.tool_ms = 20;
        app.transcript.stats.ttft_ms = 2_000;
        app.transcript.stats.ttft_steps = 1;
        app.transcript.stats.decode_ms = 1_000;
        app.transcript.stats.decode_tokens = 20;
        app.transcript.usage.input = 2_000;
        app.transcript.usage.cache = 10_000;
        app.transcript.usage.output = 100;

        let lines = stats_lines(&app, 32);
        let text = lines.join(" | ");
        assert!(lines.len() > 1);
        assert!(text.contains("Turns 1 · Steps 2"));
        assert!(text.contains("Input 12.0K · Output 100"));
        assert!(text.contains("LLM 5.0s · Tools 20ms"));
        assert!(text.contains("TTFB 2.0s · TPS 20"));
        assert!(text.contains("Cache hit 83%"));
    }
}

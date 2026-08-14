//! ratatui rendering: status bar / scrollback / composer / shortcuts bar.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Dialog, Focus, RunState};
use crate::resume::age_label;
use crate::transcript::CellKind;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_status(f, app, chunks[0]);
    draw_scrollback(f, app, chunks[1]);
    draw_composer(f, app, chunks[2]);
    draw_shortcuts(f, app, chunks[3]);
    if app.has_dialog() {
        draw_dialog(f, app, area);
    }
}

fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
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
            let h = (d.options.len() as u16 + 6).min(area.height.saturating_sub(4)).max(8);
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
                    format!(" {}", d.reason),
                    Style::default().fg(app.theme.text_secondary),
                )));
            }
            if !d.input.is_empty() {
                let one: String = d.input.chars().take(80).collect();
                lines.push(Line::from(Span::styled(
                    format!(" {}", one),
                    Style::default().fg(app.theme.gray),
                )));
            }
            lines.push(Line::from(""));
            for (i, opt) in d.options.iter().enumerate() {
                let selected = i == d.selected;
                let mark = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
                } else {
                    Style::default().fg(app.theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{}{}. {}", mark, i + 1, pretty_outcome(opt)),
                    style,
                )));
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.prompt_border_active))
                .title(" permission ");
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_base)),
                rect,
            );
        }
        Dialog::Resume(p) => {
            let w = area.width.min(80).saturating_sub(4).max(40);
            let h = (p.items.len() as u16 + 2).min(area.height.saturating_sub(4)).max(6);
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
                    format!("{}{} · {} turns · {} · {}", mark, label, item.turns, age_label(item.modified), item.id),
                    style,
                )));
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.prompt_border_active))
                .title(" resume ");
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_base)),
                rect,
            );
        }
        Dialog::Ask(d) => {
            let n = d.questions.len().max(1);
            let cur = d.current.min(n - 1);
            let q = &d.questions[cur];
            let opts = q.options.len();
            let w = area.width.min(72).saturating_sub(4).max(30);
            let h = (opts as u16 + 7).min(area.height.saturating_sub(4)).max(9);
            let rect = centered_rect(w, h, area);
            f.render_widget(Clear, rect);
            let title = if q.header.is_empty() {
                format!(" 问题 {}/{} ", cur + 1, n)
            } else {
                format!(" {} ", q.header)
            };
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                format!(" {}", q.question),
                Style::default().fg(app.theme.text_primary),
            ))];
            lines.push(Line::from(""));
            for (i, opt) in q.options.iter().enumerate() {
                let chosen = d.answers[cur].contains(&i);
                let cursor = if chosen { "› " } else { "  " };
                let box_mark = if q.multi_select {
                    if chosen { "[x]" } else { "[ ]" }
                } else {
                    ""
                };
                let style = if chosen {
                    Style::default()
                        .fg(app.theme.text_primary)
                        .bg(app.theme.bg_highlight)
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
                    " (自由文本回答暂未实现，Esc 跳过)",
                    Style::default().fg(app.theme.gray_dim),
                )));
            }
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.prompt_border_active))
                .title(title);
            f.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .style(Style::default().bg(app.theme.bg_base)),
                rect,
            );
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    let mut spans = vec![
        Span::styled(
            " dsh-whale",
            Style::default()
                .fg(app.theme.accent_assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", app.model),
            Style::default().fg(app.theme.text_secondary),
        ),
    ];
    match app.state {
        RunState::Running => spans.push(Span::styled(
            " ● running",
            Style::default().fg(app.theme.accent_running),
        )),
        RunState::Starting => spans.push(Span::styled(
            " ○ starting",
            Style::default().fg(app.theme.warning),
        )),
        RunState::Idle => spans.push(Span::styled(
            " · idle",
            Style::default().fg(app.theme.gray_dim),
        )),
    }
    if !app.queue().is_empty() {
        spans.push(Span::styled(
            format!(" · queue {}", app.queue().len()),
            Style::default().fg(app.theme.accent_plan),
        ));
    }
    if !app.status.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", app.status),
            Style::default().fg(app.theme.text_secondary),
        ));
    }
    if let Some(n) = &app.notice {
        spans.push(Span::styled(
            format!(" · {}", n),
            Style::default().fg(app.theme.warning),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(app.theme.bg_base)),
        split[0],
    );
    // Token usage from the last assistant/chunk usage event.
    let u = app.transcript.usage;
    let ctx_text = if u.input > 0 || u.output > 0 {
        format!("in {} · out {} · cache {}", u.input, u.output, u.cache)
    } else {
        "ctx 0/1.0M".to_string()
    };
    let ctx = Span::styled(ctx_text, Style::default().fg(app.theme.gray));
    f.render_widget(
        Paragraph::new(Line::from(vec![ctx]))
            .alignment(Alignment::Right)
            .style(Style::default().bg(app.theme.bg_base)),
        split[1],
    );
}

fn draw_scrollback(f: &mut Frame, app: &mut App, area: Rect) {
    let t = &app.transcript;
    let sel = t.selected;
    let mut lines: Vec<Line> = Vec::new();
    for (i, cell) in t.cells.iter().enumerate() {
        let accent = match cell.kind {
            CellKind::User => app.theme.accent_user,
            CellKind::Assistant => app.theme.accent_assistant,
            CellKind::Thinking => app.theme.accent_thinking,
            CellKind::Tool | CellKind::ToolResult => app.theme.accent_tool,
            CellKind::Notice => app.theme.warning,
            CellKind::Subagent => app.theme.accent_plan,
        };
        let selected = sel == Some(i);
        let bg = if selected {
            app.theme.bg_highlight
        } else {
            app.theme.bg_base
        };
        let bar = if selected { "▌" } else { "│" };
        let head = match cell.kind {
            CellKind::User => "you".to_string(),
            CellKind::Assistant => "assistant".to_string(),
            CellKind::Tool if cell.failed => format!("{} ✗", cell.title),
            CellKind::Tool => format!("{} ◆", cell.title),
            CellKind::ToolResult if cell.failed => "output ✗".to_string(),
            CellKind::ToolResult => "output".to_string(),
            _ => cell.title.clone(),
        };
        lines.push(Line::from(Span::styled(
            format!("{bar} {head}"),
            Style::default().fg(accent).bg(bg),
        )));
        if !cell.folded {
            for raw in cell.text.split('\n') {
                lines.push(Line::from(Span::styled(
                    format!("  {raw}"),
                    Style::default().fg(app.theme.text_primary).bg(bg),
                )));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "welcome — 输入消息开始（demo 模式已预置一轮脚本化对话）",
            Style::default().fg(app.theme.gray_dim),
        )));
    }
    // Tail window + manual scroll offset.
    let height = area.height.saturating_sub(1) as usize;
    let start = lines.len().saturating_sub(height).saturating_sub(app.scroll);
    let end = lines.len().saturating_sub(app.scroll);
    let view: Vec<Line> = lines[start.max(0)..end.max(start)].to_vec();
    f.render_widget(Paragraph::new(view), area);
}

fn draw_composer(f: &mut Frame, app: &App, area: Rect) {
    let border_color = match app.focus {
        Focus::Prompt => app.theme.prompt_border_active,
        Focus::Scrollback => app.theme.prompt_border,
    };
    let title = format!(" {} ", app.session_id);
    let text = if app.input.is_empty() {
        "› ".to_string()
    } else {
        format!("› {}", app.input)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, Style::default().fg(app.theme.gray_dim)));
    f.render_widget(
        Paragraph::new(text)
            .block(block)
            .style(Style::default().fg(app.theme.text_primary)),
        area,
    );
}

fn draw_shortcuts(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.dialog {
        Dialog::Approval(_) => "↑/↓ select · 1-9 pick · Enter confirm · Esc cancel · Ctrl+Q quit",
        Dialog::Resume(_) => "↑/↓ select · Enter resume · Esc close · Ctrl+Q quit",
        Dialog::Ask(_) => "↑/↓ select · ←/→ question · 1-9 pick · Space toggle · Enter next/submit · Esc skip · Ctrl+Q quit",
        Dialog::None => match app.focus {
            Focus::Prompt => "Enter send · Shift+Enter newline · Alt+Enter send-now · Esc cancel/2×clear · Ctrl+Q quit",
            Focus::Scrollback => "↑/↓ select · h/l fold · Tab prompt · Ctrl+E thinking · Ctrl+T theme · Ctrl+Q quit",
        },
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            hint,
            Style::default().fg(app.theme.gray).bg(app.theme.bg_light),
        )),
        area,
    );
}

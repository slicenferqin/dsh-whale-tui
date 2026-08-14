//! ratatui rendering: status bar / scrollback / composer / shortcuts bar.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Focus, RunState};
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
    let hint = match app.focus {
        Focus::Prompt => "Enter send · Shift+Enter newline · Alt+Enter send-now · Esc cancel/2×clear · Ctrl+Q quit",
        Focus::Scrollback => "↑/↓ select · h/l fold · Tab prompt · Ctrl+E thinking · Ctrl+T theme · Ctrl+Q quit",
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            hint,
            Style::default().fg(app.theme.gray).bg(app.theme.bg_light),
        )),
        area,
    );
}

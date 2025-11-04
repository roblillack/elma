//! Ratatui rendering helpers for the Elma mail client.
//!
//! The module contains a thin layer that maps the abstract application state to
//! widgets.  All layout decisions are centralised here so the controller logic in
//! [`crate::app`] remains agnostic of the terminal representation.

use crate::app::{ActiveView, App, MessageViewState};
use crate::model::MessageStatus;
use crate::viewer;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use time::OffsetDateTime;

const ACTION_BAR_BG: Color = Color::Rgb(211, 211, 211);
const ACTION_BAR_FG: Color = Color::Rgb(105, 105, 105);
const ARCHIVED_FG: Color = Color::Rgb(0, 139, 139);

/// Render the entire UI based on the currently active view.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    match app.active_view() {
        ActiveView::Inbox => render_inbox(frame, app),
        ActiveView::Message => render_message(frame, app),
    }
}

/// Draw the action bar, optionally reserving space for the commit indicator.
fn render_action_bar(frame: &mut Frame<'_>, area: Rect, text: String, indicator: Option<String>) {
    if area.width == 0 {
        return;
    }

    if let Some(indicator) = indicator {
        let indicator_width = indicator.chars().count() as u16;

        if indicator_width >= area.width {
            let indicator_widget = Paragraph::new(indicator)
                .style(Style::default().fg(Color::White).bg(Color::Red))
                .block(Block::default().style(Style::default().bg(Color::Red)));
            frame.render_widget(indicator_widget, area);
            return;
        }

        let segments = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(indicator_width)])
            .split(area);

        let action_bar = Paragraph::new(text)
            .style(action_bar_style())
            .block(Block::default());
        frame.render_widget(action_bar, segments[0]);

        let indicator_widget = Paragraph::new(indicator)
            .style(Style::default().fg(Color::White).bg(Color::Red))
            .block(Block::default().style(Style::default().bg(Color::Red)));
        frame.render_widget(indicator_widget, segments[1]);
    } else {
        let action_bar = Paragraph::new(text)
            .style(action_bar_style())
            .block(Block::default());
        frame.render_widget(action_bar, area);
    }
}

/// Render the inbox list together with action and status bars.
fn render_inbox(frame: &mut Frame<'_>, app: &mut App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.size());

    render_action_bar(
        frame,
        layout[0],
        app.inbox_action_bar(),
        app.commit_indicator(),
    );

    render_message_table(frame, app, layout[1]);

    let mut info_text = app.inbox_info_bar();
    if let Some(status) = app.inbox_status_line() {
        if !status.is_empty() {
            info_text.push_str(" — ");
            info_text.push_str(status);
        }
    }
    let info_bar = Paragraph::new(info_text)
        .style(action_bar_style())
        .block(Block::default());
    frame.render_widget(info_bar, layout[2]);
}

/// Render the message view, falling back to the inbox if no message is open.
fn render_message(frame: &mut Frame<'_>, app: &mut App) {
    let Some(view) = app.message_view() else {
        render_inbox(frame, app);
        return;
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.size());

    let action_bar_text = message_action_bar(app, view);
    render_action_bar(frame, layout[0], action_bar_text, app.commit_indicator());

    render_message_body(frame, view, layout[1]);

    let info_text = view.info_line.clone().unwrap_or_else(|| String::new());
    let info_bar = Paragraph::new(info_text)
        .style(action_bar_style())
        .block(Block::default());
    frame.render_widget(info_bar, layout[2]);
}

/// Build the inbox table, handling scrolling and selection.
fn render_message_table(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let messages = app.inbox_messages().to_vec();
    let total = messages.len();
    let height = area.height as usize;

    let mut top = app.inbox_scroll_top();
    if height > 0 {
        if top > total.saturating_sub(1) {
            top = total.saturating_sub(1);
        }

        if let Some(selected) = app.inbox_selected() {
            if selected < top {
                top = selected;
            } else if selected >= top + height {
                top = selected + 1 - height;
            }
        }

        if total > height && top + height > total {
            top = total - height;
        }
    } else {
        top = 0;
    }

    app.set_inbox_scroll_top(top);

    let now = OffsetDateTime::now_utc();
    let widths = [
        Constraint::Length(3),
        Constraint::Length(14),
        Constraint::Length(21),
        Constraint::Length(5),
        Constraint::Min(10),
    ];

    let visible_rows = messages
        .iter()
        .enumerate()
        .skip(top)
        .take(if height == 0 { total } else { height })
        .map(|(_, message)| app.formatted_message_row(message, now))
        .map(|row| {
            let style = style_for_row(&row);
            Row::new(vec![
                Cell::from(row.flags),
                Cell::from(row.date),
                Cell::from(row.sender),
                Cell::from(row.size),
                Cell::from(row.subject),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();

    let table = Table::new(visible_rows, widths)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");

    let mut state = TableState::default();
    if let Some(selected) = app.inbox_selected() {
        if selected >= top {
            let relative = selected - top;
            if height == 0 || relative < height {
                state.select(Some(relative));
            }
        }
    }

    frame.render_stateful_widget(table, area, &mut state);
}

/// Render the message body pane, including metadata and FTML/HTML content.
fn render_message_body(frame: &mut Frame<'_>, view: &MessageViewState, area: Rect) {
    let width = area.width.max(2) - 2;
    let content_width = width.min(80);
    let mut lines = Vec::new();

    let meta_lines = message_metadata_lines(view, content_width);
    lines.extend(meta_lines);
    lines.push(Line::raw(""));

    if view.unformatted {
        let rendered = view
            .raw_html
            .as_ref()
            .cloned()
            .or_else(|| plain_text(&view.content));
        if let Some(raw) = rendered {
            lines.extend(raw.lines().map(|line| Line::raw(line.to_string())));
        } else {
            lines.push(Line::raw("No raw content available."));
        }
    } else if let Some(document) = &view.document {
        match viewer::render_document(document, content_width as u16) {
            Ok(rendered) => {
                for line in rendered {
                    lines.push(Line::raw(line));
                }
            }
            Err(err) => {
                lines.push(Line::raw(format!("Failed to render FTML: {err}")));
            }
        }
    } else if let Some(text) = plain_text(&view.content) {
        lines.extend(text.lines().map(|line| Line::raw(line.to_string())));
    } else {
        lines.push(Line::raw("No viewable content for this message."));
    }

    lines.push(Line::raw(""));
    lines.push(Line::raw("---"));
    lines.push(Line::raw("Message parts:"));
    for part in &view.content.parts {
        lines.push(Line::raw(format!(
            "- {}: {} bytes",
            part.content_type,
            part.content.len()
        )));
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((view.scroll, 0))
        .block(Block::default().borders(Borders::NONE));

    frame.render_widget(paragraph, area);
}

fn message_action_bar(app: &App, view: &MessageViewState) -> String {
    let mut text = String::from("q:Close s:Star r:Reply f:Forward y:Archive d:Delete");

    let total = app.inbox_messages().len();
    let mut entries = Vec::new();
    if view.message_index + 1 < total {
        entries.push("j:Next");
    }
    if view.message_index > 0 {
        entries.push("k:Prev");
    }

    if !entries.is_empty() {
        text.push_str(" — ");
        text.push_str(&entries.join(" "));
    }

    text
}

fn message_metadata_lines(view: &MessageViewState, width: u16) -> Vec<Line<'static>> {
    let message = &view.message;
    let date_line = {
        let year = message.sent.year();
        let month = message.sent.month() as u8;
        let day = message.sent.day();
        let hour = message.sent.hour();
        let minute = message.sent.minute();
        let second = message.sent.second();
        let total_minutes = message.sent.offset().whole_minutes() as i32;
        let sign = if total_minutes < 0 { '-' } else { '+' };
        let abs_minutes = total_minutes.abs();
        let offset_hours = abs_minutes / 60;
        let offset_minutes = abs_minutes % 60;
        format!(
            "Date:    {year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {sign}{offset_hours:02}{offset_minutes:02}"
        )
    };

    let mut meta = vec![
        date_line,
        format!("From:    {}", message.sender),
        format!("Subject: {}", message.subject),
    ];

    if !view.content.mailer.is_empty() {
        meta.push(format!("Mailer:  {}", view.content.mailer));
    }

    meta.into_iter()
        .map(|line| {
            let padded = pad_to_width(&line, width);
            Line::from(Span::styled(
                padded,
                Style::default().add_modifier(Modifier::ITALIC),
            ))
        })
        .collect()
}

fn pad_to_width(text: &str, width: u16) -> String {
    let mut result = text.to_string();
    let mut len = text.chars().count();
    if len > width as usize && width > 0 {
        result = text.chars().take(width as usize - 1).collect::<String>();
        result.push('…');
        len = width as usize;
    }

    while len < width as usize {
        result.push(' ');
        len += 1;
    }

    result
}

fn action_bar_style() -> Style {
    Style::default().bg(ACTION_BAR_BG).fg(ACTION_BAR_FG)
}

fn style_for_row(row: &crate::app::MessageRow) -> Style {
    let mut style = Style::default();

    style = match row.status {
        MessageStatus::New => style.fg(Color::Red),
        MessageStatus::Archived => style.fg(ARCHIVED_FG).add_modifier(Modifier::ITALIC),
        MessageStatus::Deleted => style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM),
        MessageStatus::Read => style,
    };

    if row.starred {
        style = style.add_modifier(Modifier::BOLD);
    }

    style
}

fn plain_text(content: &crate::model::MessageContent) -> Option<String> {
    content
        .part("text/plain")
        .and_then(|part| String::from_utf8(part.content.clone()).ok())
}

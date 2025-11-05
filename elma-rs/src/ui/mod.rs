//! Ratatui rendering helpers for the Elma mail client.
//!
//! The module contains a thin layer that maps the abstract application state to
//! widgets.  All layout decisions are centralised here so the controller logic in
//! [`crate::app`] remains agnostic of the terminal representation.

use crate::app::{
    ActiveView, App, ComposeButton, ComposeField, ComposeFocus, ComposeState, MessageViewState,
    ShortcutMenu,
};
use crate::model::MessageStatus;
use crate::viewer;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use time::OffsetDateTime;

const ACTION_BAR_BG: Color = Color::Rgb(211, 211, 211);
const ACTION_BAR_FG: Color = Color::Rgb(105, 105, 105);
const ARCHIVED_FG: Color = Color::Rgb(0, 139, 139);

/// Render the entire UI based on the currently active view.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    if app.compose_state().is_some() {
        render_inbox(frame, app);
        render_compose(frame, app);
        if let Some(menu) = app.shortcut_menu() {
            render_shortcut_menu(frame, menu);
        }
        return;
    }

    match app.active_view() {
        ActiveView::Mailbox => render_inbox(frame, app),
        ActiveView::Message => render_message(frame, app),
        ActiveView::Compose => render_compose(frame, app),
    }

    if let Some(menu) = app.shortcut_menu() {
        render_shortcut_menu(frame, menu);
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

fn render_compose(frame: &mut Frame<'_>, app: &mut App) {
    let Some(state) = app.compose_state() else {
        render_inbox(frame, app);
        return;
    };

    let frame_area = frame.size();
    let dialog_width = if frame_area.width >= 90 {
        80u16.min(frame_area.width)
    } else {
        frame_area.width
    };
    let dialog_height = if frame_area.height > 30 {
        ((frame_area.height as u32 * 80) / 100).max(1) as u16
    } else {
        frame_area.height
    };

    let offset_x = frame_area
        .width
        .saturating_sub(dialog_width)
        .saturating_div(2);
    let offset_y = frame_area
        .height
        .saturating_sub(dialog_height)
        .saturating_div(2);

    let modal_area = Rect::new(
        frame_area.x + offset_x,
        frame_area.y + offset_y,
        dialog_width,
        dialog_height,
    );

    let popup_style = Style::default().bg(Color::Black).fg(Color::White);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(popup_style)
        .border_style(Style::default().fg(Color::Gray));
    let inner = block.inner(modal_area);

    frame.render_widget(Clear, modal_area);
    frame.render_widget(block, modal_area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(app.compose_action_bar())
        .style(popup_style)
        .alignment(Alignment::Center);
    frame.render_widget(header, layout[0]);

    let field_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(layout[1]);

    let mut cursor_pos = None;

    for (area, label, field) in [
        (field_rows[0], "To", ComposeField::To),
        (field_rows[1], "Cc", ComposeField::Cc),
        (field_rows[2], "Bcc", ComposeField::Bcc),
        (field_rows[3], "Subject", ComposeField::Subject),
    ] {
        if cursor_pos.is_none() {
            cursor_pos = render_compose_field(frame, area, state, label, field);
        } else {
            render_compose_field(frame, area, state, label, field);
        }
    }

    if cursor_pos.is_none() {
        cursor_pos = render_compose_content(frame, layout[2], state);
    } else {
        render_compose_content(frame, layout[2], state);
    }
    render_compose_buttons(frame, layout[3], state);

    let status_text = app
        .compose_status_line()
        .map(|text| text.to_string())
        .unwrap_or_else(|| "Tab to move between fields; Enter activates a button.".to_string());
    let status = Paragraph::new(status_text)
        .style(popup_style)
        .alignment(Alignment::Center);
    frame.render_widget(status, layout[4]);

    if let Some((x, y)) = cursor_pos {
        frame.set_cursor(x, y);
    } else {
        frame.set_cursor(modal_area.x + 1, modal_area.y + 1);
    }
}

fn render_compose_field(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ComposeState,
    label: &str,
    field: ComposeField,
) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }

    let focused = state.is_field_focused(field);
    let (value, _) = state.field_data(field);
    let (before, _) = state.field_parts(field);

    let label_text = format!("{label}: ");
    let label_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let placeholder_style = Style::default().fg(Color::DarkGray);
    let value_style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let mut spans = vec![Span::styled(label_text.clone(), label_style)];
    if value.is_empty() {
        spans.push(Span::styled("<empty>".to_string(), placeholder_style));
    } else {
        spans.push(Span::styled(value.to_string(), value_style));
    }

    let base_style = if focused {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default().bg(Color::Black)
    };

    let paragraph = Paragraph::new(Line::from(spans))
        .style(base_style)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);

    if !focused {
        return None;
    }

    let label_width = label_text.chars().count() as u16;
    let before_width = before.chars().count() as u16;
    let max_x = area.x + area.width.saturating_sub(1);
    let mut cursor_x = area.x + label_width + before_width;
    if cursor_x > max_x {
        cursor_x = max_x;
    }

    Some((cursor_x, area.y))
}

fn render_compose_content(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ComposeState,
) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }

    let focused = state.is_field_focused(ComposeField::Content);
    let (value, _) = state.field_data(ComposeField::Content);
    let (before, _) = state.field_parts(ComposeField::Content);
    let placeholder_style = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    if value.is_empty() {
        lines.push(Line::styled("<type your message>", placeholder_style));
    } else {
        for line in value.lines() {
            lines.push(Line::raw(line.to_string()));
        }
        if value.ends_with('\n') {
            lines.push(Line::raw(String::new()));
        }
    }

    if lines.is_empty() {
        lines.push(Line::raw(String::new()));
    }

    let base_style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White).bg(Color::Black)
    };

    let paragraph = Paragraph::new(lines)
        .style(base_style)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);

    if !focused {
        return None;
    }

    let mut row: u16 = 0;
    let mut col: u16 = 0;
    for ch in before.chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
        } else {
            col = col.saturating_add(1);
        }
    }

    let max_y = area.y + area.height.saturating_sub(1);
    let max_x = area.x + area.width.saturating_sub(1);
    let cursor_y = (area.y + row).min(max_y);
    let cursor_x = (area.x + col).min(max_x);

    Some((cursor_x, cursor_y))
}

fn render_compose_buttons(frame: &mut Frame<'_>, area: Rect, state: &ComposeState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let buttons = [
        (ComposeButton::Cancel, "Cancel"),
        (ComposeButton::Draft, "Draft"),
        (ComposeButton::Send, "Send"),
    ];

    let mut spans = Vec::new();
    for (idx, (button, label)) in buttons.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("   "));
        }
        let focused = matches!(state.focus(), ComposeFocus::Button(active) if active == *button);
        if focused {
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default().fg(Color::White),
            ));
        }
    }

    let paragraph = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Black))
        .alignment(Alignment::Center);

    frame.render_widget(paragraph, area);
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
        Constraint::Length(6),
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
                Cell::from(row.sequence),
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

fn render_shortcut_menu(frame: &mut Frame<'_>, menu: &ShortcutMenu) {
    let mut lines = Vec::new();
    let header_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![Span::styled(menu.title(), header_style)]));
    lines.push(Line::raw(""));

    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    for entry in menu.entries() {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(entry.key.to_string(), key_style),
            Span::raw(format!("  {}", entry.description)),
        ]);
        lines.push(line);
    }

    let content_width = lines
        .iter()
        .map(|line| line.width() as u16)
        .max()
        .unwrap_or(0);
    let inner_width = content_width.max(menu.title().len() as u16);
    let inner_height = lines.len() as u16;

    if inner_width == 0 || inner_height == 0 {
        return;
    }

    let width = inner_width + 2;
    let height = inner_height + 2;

    let frame_area = frame.size();
    if frame_area.width < width || frame_area.height < height {
        return;
    }

    let x = frame_area.x + frame_area.width - width;
    let y = frame_area.y + frame_area.height - height;
    let area = Rect::new(x, y, width, height);

    let popup_style = Style::default().bg(Color::Black).fg(Color::White);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(popup_style)
        .border_style(Style::default().fg(Color::Gray));

    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(lines)
        .style(popup_style)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
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
    text.push_str(" Up/Down/Space:Scroll");

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

    let labels_line = if message.labels.is_empty() {
        "Labels:  (none)".to_string()
    } else {
        format!("Labels:  {}", message.labels.join(", "))
    };
    meta.push(labels_line);

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
        MessageStatus::PendingInbox => style.add_modifier(Modifier::DIM),
        MessageStatus::Spam => style.fg(Color::Magenta).add_modifier(Modifier::DIM),
        MessageStatus::Read => style,
    };

    if row.starred {
        style = style.add_modifier(Modifier::BOLD);
    }

    style
}

fn plain_text(content: &crate::model::MessageContent) -> Option<String> {
    content.part("text/plain").map(|part| {
        String::from_utf8(part.content.clone())
            .unwrap_or_else(|_| String::from_utf8_lossy(&part.content).into_owned())
    })
}

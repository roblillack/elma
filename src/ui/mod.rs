//! Ratatui rendering helpers for the Elma mail client.
//!
//! The module contains a thin layer that maps the abstract application state to
//! widgets.  All layout decisions are centralised here so the controller logic in
//! [`crate::app`] remains agnostic of the terminal representation.

use crate::app::{
    ActiveView, App, ComposeButton, ComposeField, ComposeFocus, ComposeState, MessageViewState,
    ProgressMode, SaveAttachmentDialog, SaveAttachmentFocus, ShortcutMenu,
};
use crate::model::{MessageAttachment, MessageStatus, format_size};
use crate::viewer;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap},
};
use time::OffsetDateTime;

const ACTION_BAR_BG: Color = Color::Rgb(211, 211, 211);
const ACTION_BAR_FG: Color = Color::Rgb(105, 105, 105);
const ARCHIVED_FG: Color = Color::Rgb(0, 139, 139);
const LABEL_SPECIAL_BG: Color = Color::Rgb(64, 64, 64);
const LABEL_SPECIAL_FG: Color = Color::White;
const LABEL_DEFAULT_BG: Color = Color::Rgb(224, 224, 224);
const LABEL_DEFAULT_FG: Color = Color::Black;

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

    if let Some(dialog) = app.save_attachment_dialog() {
        let attachments = app.save_attachment_attachments();
        let cursor = render_save_attachment_dialog(frame, dialog, attachments);
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Draw the action bar, optionally reserving space for the commit indicator.
fn render_action_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    text: String,
    indicator: Option<(String, ProgressMode)>,
) {
    if area.width == 0 {
        return;
    }

    if let Some((indicator, mode)) = indicator {
        let indicator_style = match mode {
            ProgressMode::Write => Style::default().fg(Color::White).bg(Color::Red),
            ProgressMode::Read => Style::default().fg(Color::Red).bg(ACTION_BAR_BG),
        };
        let indicator_block_style = match mode {
            ProgressMode::Write => Style::default().bg(Color::Red),
            ProgressMode::Read => Style::default().bg(ACTION_BAR_BG),
        };

        let indicator_width = indicator.chars().count() as u16;

        if indicator_width >= area.width {
            let indicator_widget = Paragraph::new(indicator)
                .style(indicator_style)
                .block(Block::default().style(indicator_block_style));
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
            .style(indicator_style)
            .block(Block::default().style(indicator_block_style));
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
    let search_focused = app.search_state().is_some_and(|s| s.2);
    let layout = if search_focused {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(frame.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(frame.area())
    };

    render_action_bar(
        frame,
        layout[0],
        app.inbox_action_bar(),
        app.commit_indicator(),
    );

    let message_area = layout[1];
    let info_area = layout[2];

    render_message_table(frame, app, message_area);

    // Render unfocused search as a popup overlay in the top-right of the message area.
    if !search_focused {
        render_search_popup(frame, message_area, app);
    }

    let mut info_text = app.inbox_info_bar();
    if let Some(status) = app.inbox_status_line()
        && !status.is_empty()
    {
        info_text.push_str(" — ");
        info_text.push_str(status);
    }
    let info_bar = Paragraph::new(info_text)
        .style(action_bar_style())
        .block(Block::default());
    frame.render_widget(info_bar, info_area);

    if search_focused {
        let cursor_pos = render_search_panel(frame, layout[3], app);
        if let Some((x, y)) = cursor_pos {
            frame.set_cursor_position((x, y));
        }
    }
}

fn render_search_panel(frame: &mut Frame<'_>, area: Rect, app: &App) -> Option<(u16, u16)> {
    let (value, cursor, _focused) = app.search_state()?;

    if area.height == 0 || area.width == 0 {
        return None;
    }

    let label = "Find: ";
    let (before_cursor, after_cursor) = value.split_at(cursor.min(value.len()));
    let help = " (input search terms; press <Enter> to activate, <Esc> to cancel)";

    let spans = vec![
        Span::raw(label),
        Span::raw(before_cursor),
        Span::raw(after_cursor),
        Span::styled(help, Style::default().fg(Color::DarkGray)),
    ];
    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, area);

    let label_width = label.chars().count() as u16;
    let max_x = area.x + area.width.saturating_sub(1);
    let cursor_x = (area.x + label_width + cursor as u16).min(max_x);
    Some((cursor_x, area.y))
}

/// Render the search panel as a popup overlay in the top-right of `area`.
fn render_search_popup(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some((value, _cursor, _focused)) = app.search_state() else {
        return;
    };

    let margin_top: u16 = 1;
    let margin_right: u16 = 3;

    let title = " Search ";
    let line1 = "Showing results for:";
    // 1 char padding on each side inside the border
    let inner_width = (line1.len() as u16).max(value.len() as u16) + 2;

    let bottom_label = "/:Change  Esc:Clear";
    let inner_width = inner_width.max(bottom_label.len() as u16);

    let width = inner_width + 2; // borders only; padding handled by Block
    let height = 2 + 2; // 2 content lines + 2 border

    if area.width < width + margin_right || area.height < height + margin_top {
        return;
    }

    let x = area.x + area.width - width - margin_right;
    let y = area.y + margin_top;
    let popup_area = Rect::new(x, y, width, height);

    let popup_style = Style::default().bg(ACTION_BAR_BG).fg(ACTION_BAR_FG);
    let dark_fg = Style::default().fg(Color::Black);

    let key_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(ACTION_BAR_FG);

    let bottom_title = Line::from(vec![
        Span::styled("/", key_style),
        Span::styled(":", sep_style),
        Span::styled("Change", dark_fg),
        Span::raw("  "),
        Span::styled("Esc", key_style),
        Span::styled(":", sep_style),
        Span::styled("Clear", dark_fg),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(popup_style)
        .border_style(Style::default().fg(ACTION_BAR_FG))
        .title_top(Line::styled(
            title,
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(bottom_title)
        .padding(Padding::horizontal(1));

    let value_style = Style::default()
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(Span::styled(line1, dark_fg)),
        Line::from(Span::styled(value, value_style)),
    ];

    frame.render_widget(Clear, popup_area);
    let paragraph = Paragraph::new(lines).style(popup_style).block(block);
    frame.render_widget(paragraph, popup_area);
}

fn render_compose(frame: &mut Frame<'_>, app: &mut App) {
    if app.compose_state().is_none() {
        render_inbox(frame, app);
        return;
    }

    let frame_area = frame.area();
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

    let attachment_count = app
        .compose_state()
        .map(|state| state.attachments().len())
        .unwrap_or(0);
    let attachments_height = if attachment_count == 0 {
        0
    } else {
        (attachment_count as u16).saturating_add(1).min(6)
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(attachments_height),
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

    {
        let state = app.compose_state().expect("compose state should exist");
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
    }

    if attachments_height > 0 {
        let state = app.compose_state().expect("compose state should exist");
        render_compose_attachments(frame, layout[2], state);
    }

    render_compose_body(frame, layout[3], app);

    {
        let state = app.compose_state().expect("compose state should exist");
        render_compose_buttons(frame, layout[4], state);
    }

    let status_text = app
        .compose_status_line()
        .map(|text| text.to_string())
        .unwrap_or_else(|| {
            "Tab to move between fields; Enter activates a button. Drop a file on the terminal to attach it."
                .to_string()
        });
    let status = Paragraph::new(status_text)
        .style(popup_style)
        .alignment(Alignment::Center);
    frame.render_widget(status, layout[5]);

    if let Some(prompt) = app
        .compose_state()
        .and_then(|state| state.attachment_prompt().map(|(v, c)| (v.to_string(), c)))
    {
        let prompt_cursor = render_attachment_prompt(frame, modal_area, &prompt.0, prompt.1);
        if let Some(pos) = prompt_cursor {
            cursor_pos = Some(pos);
        }
    }

    if let Some((x, y)) = cursor_pos {
        frame.set_cursor_position((x, y));
    }
}

fn render_compose_attachments(frame: &mut Frame<'_>, area: Rect, state: &ComposeState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let focused = state.is_attachments_focused();
    let attachments = state.attachments();
    let selected = state.attachment_selected();

    let label_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(Color::DarkGray);

    let header_text = format!(
        "Attachments ({}):  [Del/Backspace to remove]",
        attachments.len()
    );
    let header = Paragraph::new(Line::from(vec![Span::styled(header_text, label_style)]))
        .style(popup_compose_style());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    frame.render_widget(header, rows[0]);

    let list_area = rows[1];
    if list_area.height == 0 {
        return;
    }

    let visible = list_area.height as usize;
    let total = attachments.len();
    let sel = selected.unwrap_or(0);
    let start = if total > visible {
        sel.saturating_sub(visible - 1)
    } else {
        0
    };

    let entry_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); visible.min(total.max(1))])
        .split(list_area);

    for (row_idx, attachment_idx) in (start..total.min(start + visible)).enumerate() {
        let Some(area) = entry_rows.get(row_idx) else {
            break;
        };
        let attachment = &attachments[attachment_idx];
        let is_selected = Some(attachment_idx) == selected;
        let row_style = if focused && is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if is_selected {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        let marker = if is_selected { "▶ " } else { "  " };
        let text = format!(
            "{marker}{name}  ({mime}, {size})",
            name = attachment.filename,
            mime = attachment.mime_type,
            size = format_size(attachment.size()).trim()
        );
        let line = Line::from(Span::styled(text, row_style));
        let paragraph = Paragraph::new(line).style(popup_compose_style());
        frame.render_widget(paragraph, *area);
    }

    if total == 0 {
        let placeholder =
            Paragraph::new(Line::from(Span::styled("(none)", muted))).style(popup_compose_style());
        if let Some(area) = entry_rows.first() {
            frame.render_widget(placeholder, *area);
        }
    }
}

fn popup_compose_style() -> Style {
    Style::default().bg(Color::Black).fg(Color::White)
}

fn render_attachment_prompt(
    frame: &mut Frame<'_>,
    modal_area: Rect,
    value: &str,
    cursor: usize,
) -> Option<(u16, u16)> {
    let min_width: u16 = 50;
    let width = modal_area
        .width
        .saturating_sub(4)
        .min(80)
        .max(min_width.min(modal_area.width));
    if width < 10 {
        return None;
    }
    let height: u16 = 4;
    if modal_area.height < height + 2 {
        return None;
    }

    let x = modal_area.x + modal_area.width.saturating_sub(width) / 2;
    let y = modal_area.y + modal_area.height.saturating_sub(height) / 2;
    let area = Rect::new(x, y, width, height);

    let block_style = Style::default().bg(Color::Black).fg(Color::White);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title_top(Line::styled(
            " Attach file ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(": attach  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(": cancel"),
        ]));
    let inner = block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let label = "Path: ";
    let split = cursor.min(value.chars().count());
    let (before, _after) = split_at_char_boundary(value, split);

    let line = Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let paragraph = Paragraph::new(line).style(block_style);
    frame.render_widget(paragraph, inner);

    let label_width = label.chars().count() as u16;
    let before_width = before.chars().count() as u16;
    let max_x = inner.x + inner.width.saturating_sub(1);
    let cursor_x = (inner.x + label_width + before_width).min(max_x);
    Some((cursor_x, inner.y))
}

fn split_at_char_boundary(value: &str, chars_before: usize) -> (&str, &str) {
    let mut idx = 0;
    for (count, (byte_idx, _)) in value.char_indices().enumerate() {
        if count == chars_before {
            idx = byte_idx;
            return value.split_at(idx);
        }
        idx = byte_idx + value[byte_idx..].chars().next().map_or(0, |c| c.len_utf8());
    }
    value.split_at(idx)
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

fn render_compose_body(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        if let Some(state) = app.compose_state_mut() {
            state.set_body_view_height(0);
            state.set_body_scroll(0);
        }
        return;
    }

    let (focused, all_lines) = {
        let Some(state) = app.compose_state() else {
            return;
        };

        let focused = state.is_body_focused();
        if state.body().is_empty() {
            let placeholder = Line::styled(
                "Press [Edit message] to compose.",
                Style::default().fg(Color::DarkGray),
            );
            (focused, vec![placeholder])
        } else {
            match viewer::render_document(state.body(), area.width) {
                Ok(lines) if lines.is_empty() => (focused, vec![Line::raw(String::new())]),
                Ok(lines) => (
                    focused,
                    lines.into_iter().map(Line::raw).collect::<Vec<_>>(),
                ),
                Err(err) => (
                    focused,
                    vec![Line::styled(
                        format!("Failed to render message body: {err}"),
                        Style::default().fg(Color::Red),
                    )],
                ),
            }
        }
    };

    let viewport = area.height as usize;
    if viewport == 0 {
        if let Some(state) = app.compose_state_mut() {
            state.set_body_view_height(0);
            state.set_body_scroll(0);
        }
        return;
    }

    let visible_count = viewport.max(1);

    let scroll = {
        let Some(state) = app.compose_state_mut() else {
            return;
        };
        state.set_body_view_height(visible_count);
        let max_scroll = all_lines.len().saturating_sub(visible_count);
        let clamped = state.body_scroll().min(max_scroll);
        if clamped != state.body_scroll() {
            state.set_body_scroll(clamped);
        }
        clamped
    };

    let mut lines: Vec<Line> = all_lines
        .iter()
        .skip(scroll)
        .take(visible_count)
        .cloned()
        .collect();

    while lines.len() < visible_count {
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
}

fn render_compose_buttons(frame: &mut Frame<'_>, area: Rect, state: &ComposeState) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let buttons = [
        (ComposeButton::Attach, "Attach file..."),
        (ComposeButton::Cancel, "Cancel"),
        (ComposeButton::Edit, "Edit message"),
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
        .split(frame.area());

    let action_bar_text = message_action_bar(app, view);
    render_action_bar(frame, layout[0], action_bar_text, app.commit_indicator());

    render_message_body(frame, view, layout[1]);

    let info_text = view.info_line.clone().unwrap_or_else(String::new);
    let info_bar = Paragraph::new(info_text)
        .style(action_bar_style())
        .block(Block::default());
    frame.render_widget(info_bar, layout[2]);
}

/// Build the inbox table, handling scrolling and selection.
fn render_message_table(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let messages: Vec<_> = app.visible_messages().into_iter().cloned().collect();
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

        if top + height > total {
            top = total.saturating_sub(height);
        }
    } else {
        top = 0;
    }

    app.set_inbox_scroll_top(top);

    let now = OffsetDateTime::now_utc();
    let widths = [
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(14),
        Constraint::Length(21),
        Constraint::Length(5),
        Constraint::Min(10),
    ];
    let column_spacing = 1u16;
    let subject_column_width = area
        .width
        .saturating_sub(6 + 4 + 14 + 21 + 5 + column_spacing * (widths.len() as u16 - 1));

    let selected_index = app.inbox_selected();
    let visible_rows = messages
        .iter()
        .enumerate()
        .skip(top)
        .take(if height == 0 { total } else { height })
        .map(|(absolute_idx, message)| {
            (
                app.formatted_message_row(message, now),
                matches!(selected_index, Some(sel) if sel == absolute_idx),
            )
        })
        .map(|(row, highlighted)| {
            let style = style_for_row(&row);
            let subject_cell = build_subject_cell(&row, subject_column_width, highlighted);
            Row::new(vec![
                Cell::from(row.sequence),
                Cell::from(row.flags),
                Cell::from(row.date),
                Cell::from(row.sender),
                Cell::from(row.size),
                subject_cell,
            ])
            .style(style)
        })
        .collect::<Vec<_>>();

    let table = Table::new(visible_rows, widths)
        .block(Block::default().borders(Borders::NONE))
        .column_spacing(column_spacing)
        .row_highlight_style(
            if app.search_state().is_some_and(|s| s.2) || app.shortcut_menu().is_some() {
                Style::default().fg(ACTION_BAR_FG).bg(ACTION_BAR_BG)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            },
        )
        .highlight_symbol("");

    let mut state = TableState::default();
    if let Some(selected) = app.inbox_selected()
        && selected >= top
    {
        let relative = selected - top;
        if height == 0 || relative < height {
            state.select(Some(relative));
        }
    }

    frame.render_stateful_widget(table, area, &mut state);
}

#[derive(Clone)]
struct DisplayLabel {
    text: String,
    kind: DisplayLabelKind,
}

#[derive(Clone, Copy)]
enum DisplayLabelKind {
    Special,
    Normal,
}

struct LabelRender {
    spans: Vec<Span<'static>>,
    width: usize,
}

enum SpecialLabelMapping {
    Display(&'static str),
    Hidden,
}

fn build_subject_cell(
    row: &crate::app::MessageRow,
    subject_width: u16,
    highlighted: bool,
) -> Cell<'static> {
    if subject_width == 0 {
        return Cell::from("");
    }

    let total_width = subject_width as usize;
    let base_subject = if row.subject.trim().is_empty() {
        row.uid.clone()
    } else {
        format!("{} {}", row.uid, row.subject)
    };

    let label_render = format_labels(&row.labels, total_width, highlighted);
    let mut spans = Vec::new();
    let mut remaining = total_width;

    if label_render.width > 0 {
        remaining = remaining.saturating_sub(label_render.width);
        spans.extend(label_render.spans);
    }

    if !base_subject.is_empty() && remaining > 0 {
        if label_render.width > 0 {
            if remaining == 0 {
                return Cell::from(Line::from(spans));
            }
            spans.push(Span::raw(" "));
            remaining = remaining.saturating_sub(1);
        }

        if remaining > 0 {
            let subject_text = fit_text_with_padding(&base_subject, remaining, false);
            if !subject_text.is_empty() {
                spans.push(Span::raw(subject_text));
            }
        }
    }

    Cell::from(Line::from(spans))
}

fn format_labels(labels: &[String], subject_width: usize, highlighted: bool) -> LabelRender {
    if subject_width == 0 {
        return LabelRender {
            spans: Vec::new(),
            width: 0,
        };
    }

    let mut display_labels = prepare_display_labels(labels);
    if display_labels.is_empty() {
        return LabelRender {
            spans: Vec::new(),
            width: 0,
        };
    }

    if display_labels.len() > 2 {
        let count = display_labels.len();
        display_labels = vec![DisplayLabel {
            text: format!("{count} labels"),
            kind: DisplayLabelKind::Normal,
        }];
    }

    let mut spans = Vec::new();
    let mut width = 0usize;

    for (index, label) in display_labels.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
            width += 1;
        }
        let text = format!("[{}]", label.text);
        width += text_width(&text);
        spans.push(Span::styled(text, label_style(label.kind, highlighted)));
    }

    if width > subject_width {
        return LabelRender {
            spans: Vec::new(),
            width: 0,
        };
    }

    LabelRender { spans, width }
}

fn prepare_display_labels(labels: &[String]) -> Vec<DisplayLabel> {
    let mut prepared = Vec::new();

    for raw_label in labels {
        let cleaned = raw_label.trim().trim_matches('"');
        if cleaned.is_empty() {
            continue;
        }

        if let Some(mapping) = map_special_use_label(cleaned) {
            match mapping {
                SpecialLabelMapping::Hidden => continue,
                SpecialLabelMapping::Display(name) => prepared.push(DisplayLabel {
                    text: name.to_string(),
                    kind: DisplayLabelKind::Special,
                }),
            }
        } else {
            prepared.push(DisplayLabel {
                text: cleaned.to_string(),
                kind: DisplayLabelKind::Normal,
            });
        }
    }

    prepared.sort_by(|a, b| {
        let a_key = a.text.to_ascii_lowercase();
        let b_key = b.text.to_ascii_lowercase();
        a_key.cmp(&b_key).then_with(|| a.text.cmp(&b.text))
    });

    prepared
}

fn map_special_use_label(label: &str) -> Option<SpecialLabelMapping> {
    let normalized = label.trim();
    if normalized.is_empty() {
        return None;
    }

    let lower = normalized.to_ascii_lowercase();
    let stripped = lower.trim_start_matches('\\');
    match stripped {
        "starred" | "[gmail]/starred" => Some(SpecialLabelMapping::Hidden),
        "important" | "[gmail]/important" => Some(SpecialLabelMapping::Hidden),
        "inbox" | "[gmail]/inbox" => Some(SpecialLabelMapping::Display("Inbox")),
        "sent" | "sent mail" | "[gmail]/sent mail" | "[gmail]/sent" => {
            Some(SpecialLabelMapping::Display("Sent"))
        }
        "draft" | "drafts" | "[gmail]/drafts" => Some(SpecialLabelMapping::Display("Drafts")),
        "spam" | "[gmail]/spam" => Some(SpecialLabelMapping::Display("Spam")),
        "trash" | "[gmail]/trash" => Some(SpecialLabelMapping::Display("Trash")),
        "all" | "all mail" | "[gmail]/all mail" | "archive" | "[gmail]/archive" => {
            Some(SpecialLabelMapping::Display("Archive"))
        }
        _ => None,
    }
}

fn label_style(kind: DisplayLabelKind, highlighted: bool) -> Style {
    if highlighted {
        return Style::default();
    }

    match kind {
        DisplayLabelKind::Special => Style::default().fg(LABEL_SPECIAL_FG).bg(LABEL_SPECIAL_BG),
        DisplayLabelKind::Normal => Style::default().fg(LABEL_DEFAULT_FG).bg(LABEL_DEFAULT_BG),
    }
}

fn fit_text_with_padding(text: &str, target_width: usize, pad: bool) -> String {
    if target_width == 0 {
        return String::new();
    }

    let current_width = text_width(text);
    if current_width <= target_width {
        if !pad {
            return text.to_string();
        }
        let mut result = text.to_string();
        result.extend(std::iter::repeat_n(
            ' ',
            target_width.saturating_sub(current_width),
        ));
        return result;
    }

    if target_width == 1 {
        return "…".to_string();
    }

    let mut result = String::new();
    for ch in text.chars().take(target_width.saturating_sub(1)) {
        result.push(ch);
    }
    result.push('…');

    if pad {
        let result_width = text_width(&result);
        if result_width < target_width {
            result.extend(std::iter::repeat_n(' ', target_width - result_width));
        }
    }

    result
}

fn text_width(value: &str) -> usize {
    value.chars().count()
}

fn render_shortcut_menu(frame: &mut Frame<'_>, menu: &ShortcutMenu) {
    let title = format!(" {} ", menu.title());

    let mut lines = Vec::new();
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
    let inner_width = content_width.max(title.len() as u16);
    let inner_height = lines.len() as u16;

    if inner_width == 0 || inner_height == 0 {
        return;
    }

    let width = inner_width + 2;
    let height = inner_height + 2;

    let frame_area = frame.area();
    if frame_area.width < width || frame_area.height < height {
        return;
    }

    let x = frame_area.x;
    let y = frame_area.y + frame_area.height - height - 1;
    let area = Rect::new(x, y, width, height);

    let popup_style = Style::default().bg(Color::Black).fg(Color::White);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(popup_style)
        .border_style(Style::default().fg(Color::Gray))
        .title_top(Line::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(lines)
        .style(popup_style)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_save_attachment_dialog(
    frame: &mut Frame<'_>,
    dialog: &SaveAttachmentDialog,
    attachments: &[MessageAttachment],
) -> Option<(u16, u16)> {
    let frame_area = frame.area();
    let width = frame_area.width.min(80).max(30.min(frame_area.width));
    let list_rows = attachments.len().min(10) as u16;
    let height = (1 /* top border */ + 1 /* folder label */ + 1 /* folder value */ + 1 /* spacer */
        + 1 /* list header */
        + list_rows.max(1)
        + 1 /* status */
        + 1/* bottom border */)
        .min(frame_area.height);

    if width < 20 || height < 6 {
        return None;
    }

    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let area = Rect::new(x, y, width, height);

    let popup_style = Style::default().bg(Color::Black).fg(Color::White);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(popup_style)
        .title_top(Line::styled(
            " Save attachment ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(": switch focus  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(": save  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(": cancel"),
        ]));
    let inner = block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let label_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("Folder:", label_style))).style(popup_style),
        layout[0],
    );

    let folder_focused = matches!(dialog.focus(), SaveAttachmentFocus::Folder);
    let (folder_value, folder_cursor) = dialog.folder_parts();
    let folder_style = if folder_focused {
        Style::default()
            .fg(Color::Yellow)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let folder_text = if folder_value.is_empty() {
        Span::styled("<empty>", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(folder_value.to_string(), folder_style)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(" "), folder_text])).style(popup_style),
        layout[1],
    );

    let list_focused = matches!(dialog.focus(), SaveAttachmentFocus::List);
    let list_header = format!("Attachments ({}):", attachments.len());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(list_header, label_style))).style(popup_style),
        layout[3],
    );

    let list_area = layout[4];
    if list_area.height > 0 {
        let visible = list_area.height as usize;
        let total = attachments.len();
        let selected = dialog.selected().min(total.saturating_sub(1));
        let start = if total > visible {
            selected.saturating_sub(visible - 1)
        } else {
            0
        };

        let row_constraints: Vec<_> =
            std::iter::repeat_n(Constraint::Length(1), visible.min(total.max(1))).collect();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(list_area);

        if total == 0 {
            if let Some(area) = rows.first() {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "(none)",
                        Style::default().fg(Color::DarkGray),
                    )))
                    .style(popup_style),
                    *area,
                );
            }
        } else {
            for (row_idx, attachment_idx) in (start..total.min(start + visible)).enumerate() {
                let Some(area) = rows.get(row_idx) else {
                    break;
                };
                let attachment = &attachments[attachment_idx];
                let is_selected = attachment_idx == selected;
                let row_style = if list_focused && is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                let marker = if is_selected { "▶ " } else { "  " };
                let name = attachment
                    .filename
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or("(unnamed attachment)");
                let text = format!(
                    "{marker}{name}  ({mime}, {size})",
                    mime = attachment.mime_type,
                    size = format_size(attachment.size).trim()
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, row_style))).style(popup_style),
                    *area,
                );
            }
        }
    }

    let status_text = dialog.status().map(|s| s.to_string()).unwrap_or_else(|| {
        "Tab to change focus, Up/Down to pick an attachment, Enter to save.".to_string()
    });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status_text,
            Style::default().fg(Color::DarkGray),
        )))
        .style(popup_style),
        layout[5],
    );

    if folder_focused {
        let label_prefix = 1u16; // the leading space
        let before_cursor_chars: u16 = folder_value
            .chars()
            .take(folder_cursor)
            .count()
            .try_into()
            .unwrap_or(u16::MAX);
        let max_x = layout[1].x + layout[1].width.saturating_sub(1);
        let cursor_x = (layout[1].x + label_prefix + before_cursor_chars).min(max_x);
        Some((cursor_x, layout[1].y))
    } else {
        None
    }
}

/// Render the message body pane, including metadata and FTML/HTML content.
fn render_message_body(frame: &mut Frame<'_>, view: &MessageViewState, area: Rect) {
    let width = area.width.max(2) - 2;
    let content_width = width.min(80);
    let mut lines = Vec::new();

    let meta_lines = message_metadata_lines(view, content_width);
    lines.extend(meta_lines);

    if !view.content.attachments.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::raw("Attachments:"));
        for attachment in &view.content.attachments {
            let display_name = attachment
                .filename
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or("(unnamed attachment)");
            let size_display = format_size(attachment.size);
            let size_display = size_display.trim().to_string();
            lines.push(Line::raw(format!(
                "- {} ({}, {})",
                display_name, attachment.mime_type, size_display
            )));
        }
    }

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
        match viewer::render_document(document, content_width) {
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
    let mut text = String::from("q:Close s:Star");
    if !view.content.attachments.is_empty() {
        text.push_str(" S:SaveAttachment");
    }
    text.push_str(" +/=:Important -:NotImportant r:Reply f:Forward y:Archive d:Delete");
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

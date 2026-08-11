//! Ratatui rendering helpers for the Elma mail client.
//!
//! The module contains a thin layer that maps the abstract application state to
//! widgets.  All layout decisions are centralised here so the controller logic in
//! [`crate::app`] remains agnostic of the terminal representation.

use crate::app::{
    ActiveView, App, ComposeButton, ComposeField, ComposeFocus, ComposeState, LoadPhase,
    LoadingState, MessageViewState, ProgressMode, SaveAttachmentDialog, SaveAttachmentFocus,
    ShortcutMenu, byte_index_for,
};
use crate::model::{MailboxKind, MessageAttachment, MessageStatus, format_size};
use crate::viewer;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap},
};
use std::time::Duration;
use time::OffsetDateTime;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const ACTION_BAR_BG: Color = Color::Rgb(211, 211, 211);
const ACTION_BAR_FG: Color = Color::Rgb(105, 105, 105);
const ARCHIVED_FG: Color = Color::Rgb(0, 139, 139);
const LABEL_SPECIAL_BG: Color = Color::Rgb(64, 64, 64);
const LABEL_SPECIAL_FG: Color = Color::White;
const LABEL_DEFAULT_BG: Color = Color::Rgb(224, 224, 224);
const LABEL_DEFAULT_FG: Color = Color::Black;
/// Base style shared by every popup surface (compose, dialogs, prompts).
const POPUP_STYLE: Style = Style::new().bg(Color::Black).fg(Color::White);

/// Visible window of a single-line text field, plus the cursor's column in it.
///
/// A field is one row tall, so a value wider than the row scrolls with the
/// cursor instead of being clipped at the end.  Both the window and the caret
/// are measured in display columns, so double-width characters (CJK, emoji)
/// keep the caret over the text it belongs to.
fn text_field_view(value: &str, cursor: usize, width: u16) -> (&str, u16) {
    if width == 0 {
        return ("", 0);
    }

    let cursor_idx = byte_index_for(value, cursor);
    // Keep one column free for the caret when it sits past the last character.
    let before_budget = width.saturating_sub(1) as usize;

    let mut start = cursor_idx;
    let mut before = 0usize;
    for (idx, ch) in value[..cursor_idx].char_indices().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if before + ch_width > before_budget {
            break;
        }
        before += ch_width;
        start = idx;
    }

    let mut end = cursor_idx;
    let mut used = before;
    for (idx, ch) in value[cursor_idx..].char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width as usize {
            break;
        }
        used += ch_width;
        end = cursor_idx + idx + ch.len_utf8();
    }

    (&value[start..end], before as u16)
}

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

/// Frame of the throbber shown next to a background operation.
///
/// Driven purely by elapsed time, so every spinner in the UI animates off the
/// main loop's redraw tick without any state of its own.
fn spinner_frame(elapsed: Duration) -> char {
    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let idx = (elapsed.as_millis() / 80) as usize % SPINNER.len();
    SPINNER[idx]
}

/// `⠹ Loading 'Invoice' (1.2s)` — one line describing work still running.
fn progress_text(label: &str, elapsed: Duration) -> String {
    format!(
        "{} {label} ({:.1}s)",
        spinner_frame(elapsed),
        elapsed.as_secs_f32()
    )
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

    if let Some((account, mailbox, state)) = app.loading_overlay() {
        render_loading_overlay(frame, message_area, account, mailbox, state);
    }

    let mut info_text = app.inbox_info_bar();
    // A load in flight outranks the last status: it is what the user is waiting for.
    if let Some((label, elapsed)) = app.pending_message_load() {
        info_text.push_str(" — ");
        info_text.push_str(&progress_text(label, elapsed));
    } else if let Some(status) = app.inbox_status_line()
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

/// Explain the wait while a mailbox has nothing to show yet.
///
/// Shown on the cold start, when switching accounts and when switching
/// mailboxes -- anywhere the list is empty because a load is still running.  It
/// takes no keys: the app stays usable underneath, and the overlay disappears by
/// itself as soon as the first messages land.
fn render_loading_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    account: &str,
    mailbox: MailboxKind,
    state: &LoadingState,
) {
    let (headline, detail) = match &state.phase {
        LoadPhase::Connecting => (
            "Connecting to the mail server".to_string(),
            "Signing in and opening the mailbox...".to_string(),
        ),
        LoadPhase::Receiving { loaded, total } => (
            "Receiving messages".to_string(),
            format!("{loaded} of {total} headers"),
        ),
        LoadPhase::Failed(reason) => ("Could not open this mailbox".to_string(), reason.clone()),
    };

    let failed = matches!(state.phase, LoadPhase::Failed(_));
    let accent = if failed { Color::Red } else { Color::Cyan };

    let title = format!("{account} • {mailbox}");
    let elapsed = state.elapsed();
    // A failure is not still running, so it gets neither throbber nor timer.
    let status = if failed {
        headline
    } else {
        format!(
            "{} {} ({:.1}s)",
            spinner_frame(elapsed),
            headline,
            elapsed.as_secs_f32()
        )
    };

    // Wide enough for the longest line, but never wider than the space there is.
    let content_width = [title.as_str(), status.as_str(), detail.as_str()]
        .iter()
        .map(|line| text_width(line))
        .max()
        .unwrap_or(0) as u16;
    let width = (content_width + 4).clamp(24, 72).min(area.width);
    if area.width < 24 {
        return;
    }

    // A backend error is a sentence, not a label, so the box grows to however
    // many rows it wraps to rather than clipping the half that says why.
    let text_width_available = width.saturating_sub(4).max(1) as usize;
    let detail_rows = text_width(&detail).div_ceil(text_width_available).max(1) as u16;
    let height = (4 + detail_rows).min(area.height);
    if height < 5 {
        return;
    }

    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);

    // Same surface as every other popup: the frame carries the black background
    // rather than letting the message list show through it.
    let block = Block::default()
        .borders(Borders::ALL)
        .style(POPUP_STYLE)
        .border_style(POPUP_STYLE)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // With the frame plain, the accent is what marks a failure as one.
    let status_style = if failed {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let lines = vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(status, status_style)),
        Line::from(Span::styled(detail, Style::default().fg(Color::Gray))),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .style(POPUP_STYLE)
            .wrap(Wrap { trim: true }),
        inner,
    );
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

    let menu_style = Style::default().bg(ACTION_BAR_BG).fg(ACTION_BAR_FG);
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
        .style(menu_style)
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
    let paragraph = Paragraph::new(lines).style(menu_style).block(block);
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

    let block = Block::default()
        .borders(Borders::ALL)
        .style(POPUP_STYLE)
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
        .style(POPUP_STYLE)
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

    // While the backend has the message the whole view is read-only, so nothing
    // offers focus: no lit button, no cursor.
    let pending_outgoing = app
        .pending_outgoing()
        .map(|(label, elapsed)| format!("{} — please wait", progress_text(label, elapsed)));
    let busy = pending_outgoing.is_some();

    {
        let state = app.compose_state().expect("compose state should exist");
        render_compose_buttons(frame, layout[4], state, busy);
    }

    let status_text = pending_outgoing
        .or_else(|| app.compose_status_line().map(|text| text.to_string()))
        .unwrap_or_else(|| {
            "Tab to move between fields; Enter activates a button. Drop a file on the terminal to attach it."
                .to_string()
        });
    let status = Paragraph::new(status_text)
        .style(if busy {
            POPUP_STYLE.fg(Color::Yellow)
        } else {
            POPUP_STYLE
        })
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

    if let Some((name, size, projected)) = app
        .compose_state()
        .and_then(|state| state.large_attachment_question())
    {
        render_large_attachment_prompt(frame, modal_area, &name, size, projected);
        // The question owns the view until it is answered; nothing underneath
        // it takes typing, so nothing underneath it shows a caret.
        cursor_pos = None;
    }

    if busy {
        cursor_pos = None;
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

    let header = Paragraph::new(Line::from(vec![
        Span::styled(format!("Attachments ({}):", attachments.len()), label_style),
        // What the message weighs on the wire, which is what a provider's limit
        // is measured against -- see ComposeState::message_size.
        Span::styled(
            format!(
                "  message size {}",
                format_size(state.message_size()).trim()
            ),
            Style::default().fg(Color::White),
        ),
        Span::styled("  [Del/Backspace to remove]", muted),
    ]))
    .style(POPUP_STYLE);
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
        let paragraph = Paragraph::new(line).style(POPUP_STYLE);
        frame.render_widget(paragraph, *area);
    }

    if total == 0 {
        let placeholder =
            Paragraph::new(Line::from(Span::styled("(none)", muted))).style(POPUP_STYLE);
        if let Some(area) = entry_rows.first() {
            frame.render_widget(placeholder, *area);
        }
    }
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
    let label_width = label.width() as u16;
    let (visible, cursor_col) =
        text_field_view(value, cursor, inner.width.saturating_sub(label_width));

    let line = Line::from(vec![
        Span::styled(
            label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            visible.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let paragraph = Paragraph::new(line).style(POPUP_STYLE);
    frame.render_widget(paragraph, inner);

    let max_x = inner.x + inner.width.saturating_sub(1);
    let cursor_x = (inner.x + label_width + cursor_col).min(max_x);
    Some((cursor_x, inner.y))
}

/// Ask whether a file large enough to matter should really be attached.
///
/// No text field, so nothing here claims the cursor: the dialog takes a yes or
/// a no and hands compose straight back.
fn render_large_attachment_prompt(
    frame: &mut Frame<'_>,
    modal_area: Rect,
    name: &str,
    size: usize,
    projected: usize,
) {
    let min_width: u16 = 50;
    let width = modal_area
        .width
        .saturating_sub(4)
        .min(80)
        .max(min_width.min(modal_area.width));
    if width < 10 {
        return;
    }
    let height: u16 = 4;
    if modal_area.height < height + 2 {
        return;
    }

    let x = modal_area.x + modal_area.width.saturating_sub(width) / 2;
    let y = modal_area.y + modal_area.height.saturating_sub(height) / 2;
    let area = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title_top(Line::styled(
            " Large attachment ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw("/"),
            Span::styled("y", Style::default().fg(Color::Yellow)),
            Span::raw(": attach  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw("/"),
            Span::styled("n", Style::default().fg(Color::Yellow)),
            Span::raw(": skip"),
        ]));
    let inner = block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The size leads the second line rather than trailing the name, so a long
    // name being clipped cannot take the number with it.
    let lines = vec![
        Line::from(Span::styled(
            name.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw(format!(
            "{}. Attach anyway? Message would be {}.",
            format_size(size).trim(),
            format_size(projected).trim()
        ))),
    ];
    frame.render_widget(Paragraph::new(lines).style(POPUP_STYLE), inner);
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
    let (value, cursor) = state.field_data(field);

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

    let label_width = label_text.width() as u16;
    // An unfocused field scrolls back to the start: its cursor is wherever the
    // user last left it, and the head of an address is the useful part.
    let view_cursor = if focused { cursor } else { 0 };
    let (visible, cursor_col) =
        text_field_view(value, view_cursor, area.width.saturating_sub(label_width));

    let mut spans = vec![Span::styled(label_text, label_style)];
    if value.is_empty() {
        spans.push(Span::styled("<empty>".to_string(), placeholder_style));
    } else {
        spans.push(Span::styled(visible.to_string(), value_style));
    }

    let base_style = if focused {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default().bg(Color::Black)
    };

    let paragraph = Paragraph::new(Line::from(spans)).style(base_style);
    frame.render_widget(paragraph, area);

    if !focused {
        return None;
    }

    let max_x = area.x + area.width.saturating_sub(1);
    let cursor_x = (area.x + label_width + cursor_col).min(max_x);

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

fn render_compose_buttons(frame: &mut Frame<'_>, area: Rect, state: &ComposeState, disabled: bool) {
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
        if disabled {
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default().fg(Color::DarkGray),
            ));
            continue;
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

    let info_text = match app.pending_message_load() {
        Some((label, elapsed)) => progress_text(label, elapsed),
        None => view.info_line.clone().unwrap_or_default(),
    };
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

    let block = Block::default()
        .borders(Borders::ALL)
        .style(POPUP_STYLE)
        .border_style(Style::default().fg(Color::Gray))
        .title_top(Line::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(lines)
        .style(POPUP_STYLE)
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(POPUP_STYLE)
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
        Paragraph::new(Line::from(Span::styled("Folder:", label_style))).style(POPUP_STYLE),
        layout[0],
    );

    let folder_focused = matches!(dialog.focus(), SaveAttachmentFocus::Folder);
    let (folder_value, folder_cursor) = dialog.folder_data();
    // The leading space in front of the value.
    let folder_indent = 1u16;
    let (folder_visible, folder_cursor_col) = text_field_view(
        folder_value,
        if folder_focused { folder_cursor } else { 0 },
        layout[1].width.saturating_sub(folder_indent),
    );
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
        Span::styled(folder_visible.to_string(), folder_style)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(" "), folder_text])).style(POPUP_STYLE),
        layout[1],
    );

    let list_focused = matches!(dialog.focus(), SaveAttachmentFocus::List);
    let list_header = format!("Attachments ({}):", attachments.len());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(list_header, label_style))).style(POPUP_STYLE),
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
                    .style(POPUP_STYLE),
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
                    "{marker}{name}  ({mime}, {size}{inline})",
                    mime = attachment.mime_type,
                    size = format_size(attachment.size).trim(),
                    // Saying so explains why the message carries no `@`.
                    inline = if attachment.inline { ", inline" } else { "" }
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, row_style))).style(POPUP_STYLE),
                    *area,
                );
            }
        }
    }

    let status_line = if let Some((filename, elapsed)) = dialog.active_operation() {
        Line::from(vec![
            Span::styled(
                spinner_frame(elapsed).to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("Saving '{filename}' ({:.1}s)...", elapsed.as_secs_f32()),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else {
        let status_text = dialog.status().map(|s| s.to_string()).unwrap_or_else(|| {
            "Tab to change focus, Up/Down to pick an attachment, Enter to save.".to_string()
        });
        Line::from(Span::styled(
            status_text,
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(status_line).style(POPUP_STYLE), layout[5]);

    if folder_focused && !dialog.is_busy() {
        let max_x = layout[1].x + layout[1].width.saturating_sub(1);
        let cursor_x = (layout[1].x + folder_indent + folder_cursor_col).min(max_x);
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
            let inline = if attachment.inline { ", inline" } else { "" };
            lines.push(Line::raw(format!(
                "- {} ({}, {}{})",
                display_name, attachment.mime_type, size_display, inline
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

#[cfg(test)]
mod tests {
    use super::{LoadPhase, LoadingState, MailboxKind, text_field_view};

    #[test]
    fn text_field_view_shows_the_whole_value_when_it_fits() {
        assert_eq!(text_field_view("draft.txt", 9, 20), ("draft.txt", 9));
        assert_eq!(text_field_view("draft.txt", 0, 20), ("draft.txt", 0));
        assert_eq!(text_field_view("", 0, 20), ("", 0));
    }

    #[test]
    fn text_field_view_scrolls_with_the_cursor() {
        let path = "/home/rob/documents/invoice.pdf";

        // Cursor at the end: the window ends there and keeps one column for the
        // caret, so the tail of a long path stays readable instead of clipping.
        assert_eq!(text_field_view(path, path.len(), 10), ("voice.pdf", 9));

        // Cursor at the start: the window starts there and fills the field.
        assert_eq!(text_field_view(path, 0, 10), ("/home/rob/", 0));

        // Cursor in the middle: the text before it fills the field, and one
        // more character shows on the far side.
        assert_eq!(text_field_view(path, 12, 10), ("me/rob/doc", 9));
    }

    #[test]
    fn text_field_view_measures_in_display_columns() {
        // Each of these is two columns wide, so only four fit in a field of
        // nine columns once the caret's column is reserved.
        let (visible, cursor) = text_field_view("日本語表示", 5, 9);
        assert_eq!(visible, "本語表示");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn attachment_prompt_renders_a_wide_path_without_panicking() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("terminal");
        let path = "/home/rob/文書/very-long-attachment-name.pdf";
        for cursor in [0usize, 12, path.chars().count()] {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    super::render_attachment_prompt(frame, area, path, cursor);
                })
                .expect("draw");
        }
    }

    #[test]
    fn text_field_view_handles_a_zero_width_field() {
        assert_eq!(text_field_view("anything", 4, 0), ("", 0));
    }

    #[test]
    fn the_large_attachment_question_reaches_the_screen() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(60, 14)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::render_large_attachment_prompt(
                    frame,
                    area,
                    "holiday.mov",
                    24_000_000,
                    33_000_000,
                );
            })
            .expect("draw");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("holiday.mov"), "{screen}");
        assert!(screen.contains("Large attachment"), "{screen}");
        // Both sizes: what the file weighs and what it would do to the message.
        assert!(screen.contains("24M"), "{screen}");
        assert!(screen.contains("33M"), "{screen}");
        assert!(screen.contains("attach"), "{screen}");
    }

    /// A terminal too short for the dialog must skip it, not panic.
    #[test]
    fn the_large_attachment_question_gives_up_on_a_tiny_screen() {
        use ratatui::{Terminal, backend::TestBackend};

        for (width, height) in [(60u16, 4u16), (8, 14), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    super::render_large_attachment_prompt(
                        frame,
                        area,
                        "holiday.mov",
                        24_000_000,
                        33_000_000,
                    );
                })
                .expect("draw");
        }
    }

    /// Draw the overlay in `phase` and return what landed on the screen.
    fn loading_screen(phase: LoadPhase) -> String {
        use ratatui::{Terminal, backend::TestBackend};

        let state = LoadingState::in_phase(phase);
        let mut terminal = Terminal::new(TestBackend::new(70, 16)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::render_loading_overlay(frame, area, "Work", MailboxKind::Inbox, &state);
            })
            .expect("draw");

        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn the_loading_overlay_names_the_account_it_is_waiting_on() {
        let screen = loading_screen(LoadPhase::Connecting);
        // With four accounts configured, which one this is matters.
        assert!(screen.contains("Work"), "{screen}");
        assert!(screen.contains("Inbox"), "{screen}");
        assert!(screen.contains("Connecting to the mail server"), "{screen}");
    }

    /// The overlay has to sit in the middle of the list and read as a popup,
    /// not as a box floating over whatever the terminal's background happens to
    /// be.
    #[test]
    fn the_loading_overlay_is_centred_on_the_usual_popup_surface() {
        use ratatui::{Terminal, backend::TestBackend};

        let state = LoadingState::in_phase(LoadPhase::Connecting);
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                super::render_loading_overlay(frame, area, "Vizzlo", MailboxKind::Inbox, &state);
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();

        let drawn =
            |row: u16| (0..buf.area.width).any(|col| buf.cell((col, row)).unwrap().symbol() != " ");
        let top = (0..buf.area.height).find(|&row| drawn(row)).expect("drawn");
        let bottom = (0..buf.area.height)
            .rev()
            .find(|&row| drawn(row))
            .expect("drawn");
        assert_eq!(
            top,
            buf.area.height - 1 - bottom,
            "the same number of rows has to be clear above and below"
        );

        let border_col = (0..buf.area.width)
            .find(|&col| buf.cell((col, top)).unwrap().symbol() != " ")
            .expect("a border cell");
        let border = buf.cell((border_col, top)).unwrap();
        assert_eq!(border.fg, super::Color::White);
        assert_eq!(border.bg, super::Color::Black);
    }

    #[test]
    fn the_loading_overlay_counts_messages_once_they_arrive() {
        let screen = loading_screen(LoadPhase::Receiving {
            loaded: 24,
            total: 100,
        });
        assert!(screen.contains("Receiving messages"), "{screen}");
        assert!(screen.contains("24 of 100 headers"), "{screen}");
    }

    #[test]
    fn a_failure_reaches_the_screen_in_full() {
        let screen = loading_screen(LoadPhase::Failed(
            "logging in to Gmail: authentication failed".to_string(),
        ));
        assert!(screen.contains("Could not open this mailbox"), "{screen}");
        assert!(screen.contains("authentication failed"), "{screen}");
    }

    /// A terminal too small for the overlay must skip it, not panic.
    #[test]
    fn the_loading_overlay_gives_up_on_a_tiny_screen() {
        use ratatui::{Terminal, backend::TestBackend};

        let state = LoadingState::in_phase(LoadPhase::Connecting);
        for (width, height) in [(70u16, 4u16), (10, 16), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    super::render_loading_overlay(frame, area, "Work", MailboxKind::Inbox, &state);
                })
                .expect("draw");
        }
    }
}

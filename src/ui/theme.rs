//! Every colour the client draws with, resolved against the terminal's own
//! background, against whether a popup is the one taking keys -- and against
//! whether the user wants colour at all.
//!
//! The message list is painted straight onto the terminal's background, but a
//! popup needs a surface of its own -- and which surface reads as "this window
//! has focus" depends on what the terminal puts behind it.  On a light terminal
//! the focused popup is the dark one; on a dark terminal it is the light one.
//! Whatever the popup covers takes the tone closest to the terminal's own, so it
//! recedes without disappearing:
//!
//! ```text
//!            focused              covered
//!   light    INK (black)          PAPER (light grey)
//!   dark     PAPER (light grey)   SLATE (dark grey)
//!   mono     heavy frame          faint, in a light frame
//! ```
//!
//! Every colour a popup uses comes out of its [`Surface`] rather than being
//! named at the call site, because a palette that reads on black -- bright
//! yellow key hints, cyan labels -- washes out on light grey, and the other way
//! round.  The accents are Tango's, the same palette the snapshot terminals are
//! built from (see [`crate::test_harness::TerminalTheme`]): its bright variants
//! on the dark surfaces, its dark ones on the light surface.
//!
//! [`Theme::Mono`] is the same interface with the colour taken out.  It says
//! everything the other two say in weight instead -- bold, faint, italics,
//! reverse video, a heavier frame -- which is what a terminal without colour
//! has, and which reads the same on either background:
//!
//! ```text
//!   role                 coloured                     monochrome
//!   ordinary text        the surface's own            the terminal's own
//!   a dialog's name      blue, bold                   bold
//!   a field's label      cyan or teal, bold           bold
//!   a key, an error      red, bold                    bold
//!   help, a placeholder  grey                         faint
//!   a selected row       the accent, behind the text  reverse video
//!   a focused field      a darker background          underlined
//!   the action bar       grey, along the top          reverse video
//!   a covered popup      a background nearer the      faint, and framed in
//!                        terminal's own               light rather than heavy
//! ```

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::BorderType;

use super::DisplayLabelKind;
use crate::app::ProgressMode;
use crate::model::MessageStatus;

/// Which palette the client draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    /// No colour at all, for `NO_COLOR`, `--no-color`, and terminals that have
    /// none to give.
    Mono,
}

/// Where a popup sits in the stack: the one keys go to, or one covered by it.
///
/// The search popup is [`Focus::Covered`] whenever it is drawn, since it is the
/// resting state of a search the user has already confirmed -- what it shows is
/// a filter still in force, not a window to type into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Focused,
    Covered,
}

/// The colours of one popup surface: a background and everything that has to
/// read on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// What the popup fills its area with.
    pub bg: Color,
    /// Ordinary text: values, prose, whatever carries no meaning of its own.
    pub fg: Color,
    /// The frame around the popup -- a step back from `fg`, so the box reads as
    /// a container rather than as more text.
    pub border: Color,
    /// Field labels inside a dialog: `To:`, `Folder:`, `Attachments (3):`.
    pub accent: Color,
    /// The name of a dialog, in the top of its frame.  See [`Surface::title`].
    pub heading: Color,
    /// The value of whichever field has focus, and a warning that is not yet an
    /// error.
    pub key: Color,
    /// Keys the user can press, and errors.
    pub hot: Color,
    /// Placeholders and help text: present, but not competing with `fg`.
    pub muted: Color,
    /// Behind the text field that has focus, so the caret sits in a lit row.
    pub field_bg: Color,
    /// The selected row of a list, or a button with focus.
    pub select_bg: Color,
    pub select_fg: Color,
}

/// How one popup is drawn: with a palette of its own, or without colour at all.
///
/// The monochrome surface carries only its place in the stack, because without
/// colour that is the whole of what separates the popup taking keys from the
/// one underneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Colour(Palette),
    Mono(Focus),
}

/// Secondary text drawn straight onto the terminal's own background rather than
/// onto a popup: the search prompt's hint, and anything else that has to stay
/// legible without knowing what is behind it.
///
/// Tango Aluminium 4, the one grey that keeps its distance from both of the
/// terminal's backgrounds -- unlike the DarkGray this used to be, which on a
/// dark terminal is very nearly the background itself.
const MUTED_ON_TERMINAL: Color = Color::Rgb(136, 138, 133);

/// The bar along the top of the client and the one along the bottom.
const BAR_BG: Color = Color::Rgb(211, 211, 211);
const BAR_FG: Color = Color::Rgb(105, 105, 105);

/// A message that has been archived: dark enough to read on either terminal.
const ARCHIVED_FG: Color = Color::Rgb(0, 139, 139);

/// The `[Inbox]`, `[Sent]`, `[Trash]` chips the server names itself, and the
/// `[Work]`, `[Invoices]` ones the user does.
const LABEL_SPECIAL_BG: Color = Color::Rgb(64, 64, 64);
const LABEL_SPECIAL_FG: Color = Color::White;
const LABEL_DEFAULT_BG: Color = Color::Rgb(224, 224, 224);
const LABEL_DEFAULT_FG: Color = Color::Black;

/// Focused on a light terminal: white on black, framed in light grey.
const INK: Palette = Palette {
    bg: Color::Black,
    fg: Color::White,
    border: Color::Rgb(211, 211, 211),
    accent: Color::Cyan,
    heading: Color::LightBlue,
    key: Color::Yellow,
    // Tango's bright scarlet.  The normal one is dark enough against black that
    // a lit key reads as a smudge.
    hot: Color::LightRed,
    // Tango Aluminium 4.  Brighter than the DarkGray placeholders used to be,
    // which on black were barely there.
    muted: Color::Rgb(136, 138, 133),
    field_bg: Color::DarkGray,
    select_bg: Color::Cyan,
    select_fg: Color::Black,
};

/// Covered on a light terminal, focused on a dark one: black on light grey.
///
/// The background is the action bar's, so the search popup -- which is drawn on
/// this surface in both themes -- goes on matching the bar it belongs to.
const PAPER: Palette = Palette {
    bg: Color::Rgb(211, 211, 211),
    fg: Color::Black,
    border: Color::Rgb(105, 105, 105),
    accent: Color::Rgb(0, 92, 94),
    // Tango Sky Blue 3 and Chocolate 3: the dark ends of the palette, which are
    // the only ones that hold up against a light grey.
    heading: Color::Rgb(32, 74, 135),
    key: Color::Rgb(143, 89, 2),
    hot: Color::Rgb(164, 0, 0),
    muted: Color::Rgb(85, 87, 83),
    field_bg: Color::Rgb(186, 189, 182),
    select_bg: Color::Rgb(0, 92, 94),
    select_fg: Color::Rgb(238, 238, 236),
};

/// Covered on a dark terminal: light grey on dark grey.
///
/// Lighter than any Tango dark background, so the box still reads as a box
/// against the terminal it is drawn on.
const SLATE: Palette = Palette {
    bg: Color::Rgb(72, 72, 72),
    fg: Color::Rgb(238, 238, 236),
    border: Color::Rgb(136, 138, 133),
    accent: Color::LightCyan,
    heading: Color::LightBlue,
    key: Color::LightYellow,
    hot: Color::LightRed,
    muted: Color::Rgb(186, 189, 182),
    field_bg: Color::Rgb(95, 95, 95),
    select_bg: Color::Cyan,
    select_fg: Color::Black,
};

impl Theme {
    /// The surface a popup in `focus` is drawn on.
    pub const fn surface(self, focus: Focus) -> Surface {
        match (self, focus) {
            (Self::Light, Focus::Focused) => Surface::Colour(INK),
            (Self::Light, Focus::Covered) => Surface::Colour(PAPER),
            (Self::Dark, Focus::Focused) => Surface::Colour(PAPER),
            (Self::Dark, Focus::Covered) => Surface::Colour(SLATE),
            (Self::Mono, focus) => Surface::Mono(focus),
        }
    }

    /// The surface of the popup taking keys.
    pub const fn focused(self) -> Surface {
        self.surface(Focus::Focused)
    }

    /// The surface of a popup something else is covering.
    pub const fn covered(self) -> Surface {
        self.surface(Focus::Covered)
    }

    /// Whether the client is drawing without colour.
    const fn is_mono(self) -> bool {
        matches!(self, Self::Mono)
    }

    /// The bar of keys along the top of the client, and the bar of counts along
    /// the bottom.
    ///
    /// Reverse video is what a terminal without colour has in place of a filled
    /// bar, and it is what these two have always been in mail clients that
    /// predate colour.
    pub fn bar_style(self) -> Style {
        if self.is_mono() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(BAR_BG).fg(BAR_FG)
        }
    }

    /// Help text drawn on the terminal's own background rather than on a popup:
    /// what the search prompt says about the keys it takes.
    pub fn hint_style(self) -> Style {
        if self.is_mono() {
            Style::new().add_modifier(Modifier::DIM)
        } else {
            Style::new().fg(MUTED_ON_TERMINAL)
        }
    }

    /// The selected row of the message list.  `active` is false while a popup
    /// has the keys, so the list shows where the selection is without claiming
    /// to be what the next key moves.
    pub fn selection_style(self, active: bool) -> Style {
        match (self.is_mono(), active) {
            (true, true) => Style::new().add_modifier(Modifier::REVERSED),
            (true, false) => Style::new().add_modifier(Modifier::REVERSED | Modifier::DIM),
            (false, true) => Style::new().add_modifier(Modifier::REVERSED),
            (false, false) => Style::new().fg(BAR_FG).bg(BAR_BG),
        }
    }

    /// One row of the message list, by what has become of the message.
    ///
    /// Without colour the flag column still spells each state out -- `N`, `A`,
    /// `D`, `!` -- so the row only has to carry how much of the user's
    /// attention it deserves.
    pub fn row_style(self, status: MessageStatus, starred: bool) -> Style {
        let mono = self.is_mono();
        let mut style = match status {
            MessageStatus::New if mono => Style::new().add_modifier(Modifier::BOLD),
            MessageStatus::New => Style::new().fg(Color::Red),
            MessageStatus::Archived if mono => Style::new().add_modifier(Modifier::ITALIC),
            MessageStatus::Archived => Style::new().fg(ARCHIVED_FG).add_modifier(Modifier::ITALIC),
            MessageStatus::Deleted => {
                Style::new().add_modifier(Modifier::CROSSED_OUT | Modifier::DIM)
            }
            MessageStatus::PendingInbox => Style::new().add_modifier(Modifier::DIM),
            MessageStatus::Spam if mono => {
                Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC)
            }
            MessageStatus::Spam => Style::new().fg(Color::Magenta).add_modifier(Modifier::DIM),
            MessageStatus::Read => Style::new(),
        };

        if starred {
            style = style.add_modifier(Modifier::BOLD);
        }

        style
    }

    /// A `[Label]` chip in the subject column.
    ///
    /// A highlighted row is already reverse video from end to end, and a chip
    /// that painted itself over that would punch a hole in the selection, so on
    /// that one row the chips are left as plain text.
    pub fn chip_style(self, kind: DisplayLabelKind, highlighted: bool) -> Style {
        if highlighted {
            return Style::new();
        }

        match (self.is_mono(), kind) {
            // The brackets already say "label", so the user's own need only
            // stay out of the subject's way; the ones the server names itself
            // are worth a chip.
            (true, DisplayLabelKind::Special) => Style::new().add_modifier(Modifier::REVERSED),
            (true, DisplayLabelKind::Normal) => Style::new().add_modifier(Modifier::DIM),
            (false, DisplayLabelKind::Special) => {
                Style::new().fg(LABEL_SPECIAL_FG).bg(LABEL_SPECIAL_BG)
            }
            (false, DisplayLabelKind::Normal) => {
                Style::new().fg(LABEL_DEFAULT_FG).bg(LABEL_DEFAULT_BG)
            }
        }
    }

    /// The progress indicator at the right of the action bar.
    ///
    /// Without colour the bar is reverse video, so a write in flight is drawn
    /// the right way round: a hole in the bar, which is harder to miss than
    /// anything that could be added to it.
    pub fn indicator_style(self, mode: ProgressMode) -> Style {
        match (self.is_mono(), mode) {
            (true, ProgressMode::Write) => Style::new().add_modifier(Modifier::BOLD),
            (true, ProgressMode::Read) => Style::new().add_modifier(Modifier::REVERSED),
            (false, ProgressMode::Write) => Style::new().fg(Color::White).bg(Color::Red),
            (false, ProgressMode::Read) => Style::new().fg(Color::Red).bg(BAR_BG),
        }
    }

    /// What the indicator is drawn on, which is the rest of its corner of the
    /// bar.
    pub fn indicator_fill_style(self, mode: ProgressMode) -> Style {
        match (self.is_mono(), mode) {
            (true, ProgressMode::Write) => Style::new(),
            (true, ProgressMode::Read) => Style::new().add_modifier(Modifier::REVERSED),
            (false, ProgressMode::Write) => Style::new().bg(Color::Red),
            (false, ProgressMode::Read) => Style::new().bg(BAR_BG),
        }
    }
}

impl Surface {
    /// Text on the surface: what every widget inside the popup starts from.
    pub fn style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().bg(palette.bg).fg(palette.fg),
            // Without a colour to fill it with, a popup steps back by going
            // faint rather than by changing what is behind it.  Everything
            // drawn inside inherits this, which is the point: a covered dialog
            // recedes whole.
            Self::Mono(Focus::Focused) => Style::new(),
            Self::Mono(Focus::Covered) => Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// The frame.  Carries no background of its own, so it inherits the block's.
    pub fn border_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.border),
            // Including whether the block it belongs to is faint.
            Self::Mono(_) => Style::new(),
        }
    }

    /// How heavy the frame is drawn.
    ///
    /// Without colour this is what says which popup has the keys even on a
    /// terminal that ignores `dim` -- and it is the one difference between two
    /// popups that no amount of squinting can lose.
    pub fn border_type(self) -> BorderType {
        match self {
            Self::Mono(Focus::Focused) => BorderType::Thick,
            Self::Colour(_) | Self::Mono(Focus::Covered) => BorderType::Plain,
        }
    }

    /// A span of ordinary text, where the surrounding widget is drawn in
    /// something else.
    pub fn text_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.fg),
            Self::Mono(_) => Style::new(),
        }
    }

    /// A field's label.
    pub fn label_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
            Self::Mono(_) => Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// Something that wants looking at but is not yet wrong: the value of the
    /// field with focus, a file large enough to ask about, a save in flight.
    pub fn warning_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.key),
            Self::Mono(_) => Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// A key the user can press, or an error.
    pub fn hot_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.hot).add_modifier(Modifier::BOLD),
            Self::Mono(_) => Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// A placeholder, or help text under something that matters more.
    pub fn muted_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().fg(palette.muted),
            Self::Mono(_) => Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// A selected row, or the button focus sits on.
    pub fn select_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new()
                .fg(palette.select_fg)
                .bg(palette.select_bg)
                .add_modifier(Modifier::BOLD),
            Self::Mono(_) => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// The selected row of a list whose popup has the keys elsewhere: where the
    /// selection is, without claiming the next key moves it.
    pub fn marked_style(self) -> Style {
        match self {
            Self::Colour(palette) => Style::new().bg(palette.field_bg),
            Self::Mono(_) => Style::new().add_modifier(Modifier::REVERSED | Modifier::DIM),
        }
    }

    /// The row a text field with focus sits on, so the caret has somewhere to
    /// be.
    pub fn field_style(self) -> Style {
        match self {
            Self::Colour(palette) => self.style().bg(palette.field_bg),
            // A rule under the whole row, which is what an input line looked
            // like before terminals could fill one.
            Self::Mono(_) => self.style().add_modifier(Modifier::UNDERLINED),
        }
    }

    /// The message body while it has focus.  A field's rule would run the depth
    /// of the pane, so this one is said in weight instead.
    pub fn body_style(self) -> Style {
        match self {
            Self::Colour(palette) => self
                .style()
                .fg(palette.key)
                .bg(palette.field_bg)
                .add_modifier(Modifier::BOLD),
            Self::Mono(_) => self.style().add_modifier(Modifier::BOLD),
        }
    }

    /// A dialog's name, for the top of its frame.
    ///
    /// Every popup that has a name wears it the same way -- in the frame rather
    /// than on a line of its own -- so that the first row inside a dialog is
    /// always its content.
    pub fn title(self, name: &str) -> Line<'static> {
        let style = match self {
            Self::Colour(palette) => Style::new().fg(palette.heading),
            Self::Mono(_) => Style::new(),
        };
        Line::styled(format!(" {name} "), style.add_modifier(Modifier::BOLD))
    }

    /// The keys a dialog takes, for the bottom of its frame: `Enter:Save
    /// Esc:Cancel`, the key lit and the action beside it.
    ///
    /// Reads as the action bar does, which is where a user looks for keys
    /// everywhere else in the client.
    pub fn key_hints(self, hints: &[(&str, &str)]) -> Line<'static> {
        let key_style = self.hot_style();
        // A step back from both the key and the action: the frame's colour
        // where there is one, faint where there is not.
        let separator_style = match self {
            Self::Colour(_) => self.border_style(),
            Self::Mono(_) => Style::new().add_modifier(Modifier::DIM),
        };
        let action_style = self.text_style();

        let mut spans = Vec::with_capacity(hints.len() * 4);
        for (key, action) in hints {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled((*key).to_string(), key_style));
            spans.push(Span::styled(":", separator_style));
            spans.push(Span::styled((*action).to_string(), action_style));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every style a popup can be drawn with, for the tests that have to hold
    /// them all to one rule.
    fn every_style(surface: Surface) -> Vec<(&'static str, Style)> {
        vec![
            ("style", surface.style()),
            ("border", surface.border_style()),
            ("text", surface.text_style()),
            ("label", surface.label_style()),
            ("warning", surface.warning_style()),
            ("hot", surface.hot_style()),
            ("muted", surface.muted_style()),
            ("select", surface.select_style()),
            ("marked", surface.marked_style()),
            ("field", surface.field_style()),
            ("body", surface.body_style()),
            ("title", surface.title("Compose").spans[0].style),
            (
                "hint key",
                surface.key_hints(&[("Esc", "Cancel")]).spans[0].style,
            ),
            (
                "hint separator",
                surface.key_hints(&[("Esc", "Cancel")]).spans[1].style,
            ),
            (
                "hint action",
                surface.key_hints(&[("Esc", "Cancel")]).spans[2].style,
            ),
        ]
    }

    /// Every style the client draws outside a popup.
    fn every_chrome_style(theme: Theme) -> Vec<(&'static str, Style)> {
        let mut styles = vec![
            ("bar", theme.bar_style()),
            ("hint", theme.hint_style()),
            ("selection", theme.selection_style(true)),
            ("inactive selection", theme.selection_style(false)),
            (
                "write indicator",
                theme.indicator_style(ProgressMode::Write),
            ),
            ("read indicator", theme.indicator_style(ProgressMode::Read)),
            (
                "write indicator fill",
                theme.indicator_fill_style(ProgressMode::Write),
            ),
            (
                "read indicator fill",
                theme.indicator_fill_style(ProgressMode::Read),
            ),
            (
                "special label",
                theme.chip_style(DisplayLabelKind::Special, false),
            ),
            ("label", theme.chip_style(DisplayLabelKind::Normal, false)),
        ];
        for status in [
            MessageStatus::New,
            MessageStatus::Read,
            MessageStatus::Archived,
            MessageStatus::Deleted,
            MessageStatus::PendingInbox,
            MessageStatus::Spam,
        ] {
            styles.push(("row", theme.row_style(status, false)));
            styles.push(("starred row", theme.row_style(status, true)));
        }
        styles
    }

    /// The point of the whole module: the popup taking keys never shares a
    /// background with the one underneath it, in either terminal.
    #[test]
    fn focus_is_visible_in_both_themes() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_ne!(
                theme.focused().style().bg,
                theme.covered().style().bg,
                "{theme:?}: a covered popup must not look like the focused one"
            );
        }
    }

    /// A focused popup is the surface furthest from the terminal's own
    /// background: dark on a light terminal, light on a dark one.
    #[test]
    fn the_focused_surface_opposes_the_terminal() {
        assert_eq!(Theme::Light.focused(), Surface::Colour(INK));
        assert_eq!(Theme::Dark.focused(), Surface::Colour(PAPER));
    }

    /// Nothing on a surface may be drawn in the surface's own background
    /// colour, which would make it invisible.
    #[test]
    fn no_colour_disappears_into_its_surface() {
        for palette in [INK, PAPER, SLATE] {
            for (role, colour) in [
                ("fg", palette.fg),
                ("border", palette.border),
                ("accent", palette.accent),
                ("heading", palette.heading),
                ("key", palette.key),
                ("hot", palette.hot),
                ("muted", palette.muted),
                ("field_bg", palette.field_bg),
                ("select_bg", palette.select_bg),
            ] {
                assert_ne!(colour, palette.bg, "{role} vanishes into the background");
            }
        }
    }

    /// The monochrome theme has to be exactly that, everywhere: a single colour
    /// asked for anywhere in it would be a colour drawn against a terminal
    /// whose own two are unknown.
    #[test]
    fn the_monochrome_theme_names_no_colour() {
        for focus in [Focus::Focused, Focus::Covered] {
            for (role, style) in every_style(Theme::Mono.surface(focus)) {
                assert_eq!(style.fg, None, "{focus:?} {role} names a foreground");
                assert_eq!(style.bg, None, "{focus:?} {role} names a background");
            }
        }

        for (role, style) in every_chrome_style(Theme::Mono) {
            assert_eq!(style.fg, None, "{role} names a foreground");
            assert_eq!(style.bg, None, "{role} names a background");
        }
    }

    /// Which popup has the keys still has to be visible without colour, and by
    /// more than one sign: `dim` is the first thing a bare terminal drops.
    #[test]
    fn focus_is_visible_without_colour() {
        let focused = Theme::Mono.focused();
        let covered = Theme::Mono.covered();

        assert_ne!(focused.style(), covered.style());
        assert_ne!(focused.border_type(), covered.border_type());
    }

    /// Every state a message row can be in has to stay distinguishable once the
    /// colours are gone, or the list is a wall of identical text.
    #[test]
    fn message_states_stay_apart_without_colour() {
        let states = [
            MessageStatus::New,
            MessageStatus::Read,
            MessageStatus::Archived,
            MessageStatus::Deleted,
            MessageStatus::PendingInbox,
            MessageStatus::Spam,
        ];

        for (index, status) in states.iter().enumerate() {
            for other in &states[index + 1..] {
                assert_ne!(
                    Theme::Mono.row_style(*status, false),
                    Theme::Mono.row_style(*other, false),
                    "{status:?} and {other:?} are drawn the same way"
                );
            }
        }
    }
}

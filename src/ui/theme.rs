//! The colours a popup is drawn with, resolved against the terminal's own
//! background and against whether the popup is the one taking keys.
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
//! ```
//!
//! Every colour a popup uses comes out of its [`Surface`] rather than being
//! named at the call site, because a palette that reads on black -- bright
//! yellow key hints, cyan labels -- washes out on light grey, and the other way
//! round.  The accents are Tango's, the same palette the snapshot terminals are
//! built from (see [`crate::test_harness::TerminalTheme`]): its bright variants
//! on the dark surfaces, its dark ones on the light surface.

use ratatui::style::{Color, Modifier, Style};

/// Which of its two backgrounds the terminal is showing behind the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
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

/// One popup surface: a background and every colour that has to read on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    /// What the popup fills its area with.
    pub bg: Color,
    /// Ordinary text: values, prose, whatever carries no meaning of its own.
    pub fg: Color,
    /// The frame around the popup -- a step back from `fg`, so the box reads as
    /// a container rather than as more text.
    pub border: Color,
    /// Titles and field labels.
    pub accent: Color,
    /// The search popup's title, which is a heading of a different kind: it
    /// names a filter rather than a dialog.
    pub heading: Color,
    /// Keys the user can press, and the value of whichever field has focus.
    pub key: Color,
    /// Errors, and the keys of the search popup's own two shortcuts.
    pub hot: Color,
    /// Placeholders and help text: present, but not competing with `fg`.
    pub muted: Color,
    /// Behind the text field that has focus, so the caret sits in a lit row.
    pub field_bg: Color,
    /// The selected row of a list, or a button with focus.
    pub select_bg: Color,
    pub select_fg: Color,
}

/// Secondary text drawn straight onto the terminal's own background rather than
/// onto a popup: the search prompt's hint, and anything else that has to stay
/// legible without knowing what is behind it.
///
/// Tango Aluminium 4, the one grey that keeps its distance from both of the
/// terminal's backgrounds -- unlike the DarkGray this used to be, which on a
/// dark terminal is very nearly the background itself.
pub const MUTED_ON_TERMINAL: Color = Color::Rgb(136, 138, 133);

/// Focused on a light terminal: white on black, framed in light grey.
const INK: Surface = Surface {
    bg: Color::Black,
    fg: Color::White,
    border: Color::Rgb(211, 211, 211),
    accent: Color::Cyan,
    heading: Color::LightBlue,
    key: Color::Yellow,
    hot: Color::Red,
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
const PAPER: Surface = Surface {
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
const SLATE: Surface = Surface {
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
            (Self::Light, Focus::Focused) => INK,
            (Self::Light, Focus::Covered) => PAPER,
            (Self::Dark, Focus::Focused) => PAPER,
            (Self::Dark, Focus::Covered) => SLATE,
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
}

impl Surface {
    /// Text on the surface: what every widget inside the popup starts from.
    pub fn style(self) -> Style {
        Style::new().bg(self.bg).fg(self.fg)
    }

    /// The frame.  Carries no background of its own, so it inherits the block's.
    pub fn border_style(self) -> Style {
        Style::new().fg(self.border)
    }

    /// A title or a field label.
    pub fn label_style(self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    /// A key the user can press.
    pub fn key_style(self) -> Style {
        Style::new().fg(self.key)
    }

    /// A placeholder, or help text under something that matters more.
    pub fn muted_style(self) -> Style {
        Style::new().fg(self.muted)
    }

    /// A selected row, or the button focus sits on.
    pub fn select_style(self) -> Style {
        Style::new()
            .fg(self.select_fg)
            .bg(self.select_bg)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module: the popup taking keys never shares a
    /// background with the one underneath it, in either terminal.
    #[test]
    fn focus_is_visible_in_both_themes() {
        for theme in [Theme::Dark, Theme::Light] {
            assert_ne!(
                theme.focused().bg,
                theme.covered().bg,
                "{theme:?}: a covered popup must not look like the focused one"
            );
        }
    }

    /// A focused popup is the surface furthest from the terminal's own
    /// background: dark on a light terminal, light on a dark one.
    #[test]
    fn the_focused_surface_opposes_the_terminal() {
        assert_eq!(Theme::Light.focused(), INK);
        assert_eq!(Theme::Dark.focused(), PAPER);
    }

    /// Nothing on a surface may be drawn in the surface's own background
    /// colour, which would make it invisible.
    #[test]
    fn no_colour_disappears_into_its_surface() {
        for surface in [INK, PAPER, SLATE] {
            for (role, colour) in [
                ("fg", surface.fg),
                ("border", surface.border),
                ("accent", surface.accent),
                ("heading", surface.heading),
                ("key", surface.key),
                ("hot", surface.hot),
                ("muted", surface.muted),
                ("field_bg", surface.field_bg),
                ("select_bg", surface.select_bg),
            ] {
                assert_ne!(colour, surface.bg, "{role} vanishes into the background");
            }
        }
    }
}

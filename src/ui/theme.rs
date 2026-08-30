//! Theme-aware surface colors for the TUI.
//!
//! Fixed RGB backgrounds bypass the terminal palette, so they clash the
//! moment the user changes their terminal theme. Surface colors here instead
//! come from the terminal palette (`Color::Indexed`) and, as a fallback for
//! terminals that don't map the palette to the active theme, from a one-time
//! query of the terminal's real background color (OSC 11) at startup.

use std::sync::OnceLock;

use ratatui::style::Color;
use terminal_colorsaurus::{color_palette, QueryOptions, ThemeMode};

/// Which side of the light/dark split the terminal background sits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Background {
    Dark,
    Light,
    /// Unknown (query failed or not a TTY); assume dark, the common case.
    Unknown,
}

/// BG for the composer, submitted prompts, and overlays: a "raised" surface.
/// The palette slot is theme-mapped by the terminal; when the palette isn't
/// theme-aware we derive a color one step away from the real background.
pub(crate) fn surface_bg() -> Color {
    static SURFACE: OnceLock<Color> = OnceLock::new();
    *SURFACE.get_or_init(|| match background() {
        Background::Light => Color::Indexed(254),
        Background::Dark => Color::Indexed(236),
        // No theme information at all: leave the background untouched so the
        // surface always blends with whatever the terminal paints.
        Background::Unknown => Color::Reset,
    })
}

/// Slightly stronger surface for popups so they read as floating above the UI.
pub(crate) fn popup_bg() -> Color {
    static POPUP: OnceLock<Color> = OnceLock::new();
    *POPUP.get_or_init(|| match background() {
        Background::Light => Color::Indexed(253),
        Background::Dark => Color::Indexed(235),
        Background::Unknown => Color::Reset,
    })
}

/// Foreground for prominent text on the composer/surface. For light themes
/// this is black; for dark themes white; when the theme is unknown the
/// terminal's own default foreground is inherited so text can never end up
/// the same color as the surface behind it.
pub(crate) fn surface_fg() -> Color {
    match background() {
        Background::Light => Color::Black,
        Background::Dark => Color::White,
        Background::Unknown => Color::Reset,
    }
}

/// Foreground for secondary text on a raised surface (descriptions, hints):
/// quiet, but still readable in both theme modes.
pub(crate) fn secondary_fg() -> Color {
    match background() {
        // Bright black is a readable mid-gray on light themes, whereas ANSI
        // silver (Gray) all but disappears on them.
        Background::Light => Color::DarkGray,
        _ => Color::Gray,
    }
}

/// Foreground for de-emphasised text on the composer/surface.
pub(crate) fn muted_fg() -> Color {
    Color::DarkGray
}

fn background() -> Background {
    static BACKGROUND: OnceLock<Background> = OnceLock::new();
    *BACKGROUND.get_or_init(detect_background)
}

/// One-time OSC 11 query of the terminal's actual background color. Called
/// eagerly at TUI startup (before raw mode) and memoized for the process.
pub(super) fn detect_background() -> Background {
    match color_palette(QueryOptions::default()) {
        Ok(palette) => match palette.theme_mode() {
            ThemeMode::Dark => Background::Dark,
            ThemeMode::Light => Background::Light,
        },
        Err(_) => Background::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_are_consistent_with_background() {
        // Whatever the detected background, surfaces must resolve without
        // panicking and stay on-theme (Reset/Indexed, never fixed RGB).
        for color in [surface_bg(), popup_bg()] {
            match color {
                Color::Reset | Color::Indexed(_) => {}
                other => panic!("fixed color leaks theme: {other:?}"),
            }
        }
        if background() == Background::Light {
            assert_eq!(surface_fg(), Color::Black);
        }
        // Unknown theme must inherit the terminal fg (Reset), never a fixed
        // color that could match the surface it sits on.
        if background() == Background::Unknown {
            assert_eq!(surface_fg(), Color::Reset);
            assert_eq!(surface_bg(), Color::Reset);
        }
    }
}

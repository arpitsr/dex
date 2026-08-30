//! Theme-aware surface colors for the TUI.
//!
//! Fixed palette slots bypass the terminal's theme (the 256-color gray ramp
//! is never remapped), so neutral grays clash with tinted backgrounds. Surface
//! colors here are instead derived from the terminal's real background and
//! foreground colors, queried once at startup (OSC 11): a surface is the
//! background blended a step toward the foreground, so it keeps the theme's
//! hue and always contrasts with text on it.

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

/// The terminal's real colors, queried once via OSC 11 and memoized.
struct Palette {
    mode: ThemeMode,
    background: (u8, u8, u8),
    foreground: (u8, u8, u8),
}

fn palette() -> Option<&'static Palette> {
    static PALETTE: OnceLock<Option<Palette>> = OnceLock::new();
    PALETTE
        .get_or_init(|| {
            let p = color_palette(QueryOptions::default()).ok()?;
            Some(Palette {
                mode: p.theme_mode(),
                background: p.background.scale_to_8bit(),
                foreground: p.foreground.scale_to_8bit(),
            })
        })
        .as_ref()
}

/// Blend `base` toward `toward` by `amount` (0.0 = base, 1.0 = toward).
fn blend(base: (u8, u8, u8), toward: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let mix = |b: u8, t: u8| (f32::from(b) + (f32::from(t) - f32::from(b)) * amount).round() as u8;
    (
        mix(base.0, toward.0),
        mix(base.1, toward.1),
        mix(base.2, toward.2),
    )
}

/// A raised surface: the terminal's actual background lifted a step toward
/// its foreground, so the hue matches the active theme. `amount` controls how
/// far the surface sits above the background (larger = more prominent).
fn raised(amount: f32) -> Color {
    match palette() {
        Some(p) => {
            let (r, g, b) = blend(p.background, p.foreground, amount);
            Color::Rgb(r, g, b)
        }
        // No theme information at all: leave the background untouched so the
        // surface always blends with whatever the terminal paints.
        None => Color::Reset,
    }
}

/// BG for the composer, submitted prompts, and overlays: a "raised" surface.
pub(crate) fn surface_bg() -> Color {
    match background() {
        Background::Dark => raised(0.10),
        Background::Light => raised(0.06),
        Background::Unknown => Color::Reset,
    }
}

/// Slightly stronger surface for popups so they read as floating above the UI.
pub(crate) fn popup_bg() -> Color {
    match background() {
        Background::Dark => raised(0.18),
        Background::Light => raised(0.12),
        Background::Unknown => Color::Reset,
    }
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
    match palette() {
        Some(p) => match p.mode {
            ThemeMode::Dark => Background::Dark,
            ThemeMode::Light => Background::Light,
        },
        None => Background::Unknown,
    }
}

/// Warm the one-time OSC 11 query. Called eagerly at TUI startup (before raw
/// mode) so later color lookups are pure memo hits.
pub(super) fn detect_background() -> Background {
    background()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_are_consistent_with_background() {
        // Whatever the detected background, surfaces must resolve without
        // panicking and stay on-theme: Reset when the theme is unknown, or
        // RGB derived from the queried palette.
        for color in [surface_bg(), popup_bg()] {
            match color {
                Color::Reset => {}
                Color::Rgb(..) if background() != Background::Unknown => {}
                other => panic!("color leaks theme: {other:?}"),
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

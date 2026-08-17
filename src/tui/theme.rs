/*  This file is part of the Dom smarthome app.
 *
 *  Copyright (C) 2026 Marko Ivankovic
 *
 *  Licensed under the Prosperity Public License 3.0.0: free to use and share
 *  for noncommercial purposes, and free to try for commercial purposes for
 *  thirty days. Continued commercial use requires a license negotiated with
 *  the contributor.
 *
 *  Contributor: Marko Ivankovic <marko@ivankovic.me>
 *  Source Code: https://github.com/ivankovic/dom
 *
 *  See the LICENSE file for the full terms.
 *
 *  As far as the law allows, this software comes as is, without any warranty
 *  or condition, and the contributor won't be liable to anyone for any
 *  damages related to this software or this license, under any kind of legal
 *  claim.
 */

//! The TUI colour palette, and dark/light selection.
//!
//! Every colour the UI draws comes from a `Theme` rather than a literal, so the
//! same render code is readable on a dark or a light terminal. Fields are named
//! for what they *mean* (`consumption`, `focus_border`) rather than for a colour,
//! so a light palette can pick a different hue for the same role.
//!
//! AGENTS.md prefers the Stylize helpers (`.cyan()`, `.dim()`) over building a
//! `Style` by hand, but each of those hardcodes one ANSI colour and so cannot be
//! themed. A palette lookup is a runtime-computed style, which AGENTS.md
//! explicitly permits via `Span::styled` / `.set_style` — that is the form used
//! throughout `render.rs`, and it should not be "simplified" back to the fixed
//! helpers.

use ratatui::style::Color;

use crate::app::NetworkDeviceStatus;

/// `Config` table key under which the user's explicit choice is stored. Absent
/// means "never chosen", which is what makes auto-detection kick in.
pub const CONFIG_KEY: &str = "theme";

/// Which palette to draw with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl ThemeMode {
    pub fn toggled(self) -> Self {
        match self {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        }
    }

    /// Stable string for persisting the user's choice in the `Config` table.
    /// Deliberately not `Display`: this is a storage format, and changing it
    /// would silently orphan the stored value.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
        }
    }

    /// Parses a value previously written by `as_str`. Unrecognised values give
    /// `None` so the caller falls back to auto-detection rather than to an
    /// arbitrary palette.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(ThemeMode::Dark),
            "light" => Some(ThemeMode::Light),
            _ => None,
        }
    }
}

/// Reads the terminal's `COLORFGBG` hint to guess whether the background is
/// dark or light.
///
/// xterm, rxvt, Konsole and others set it to `"<fg>;<bg>"` using ANSI colour
/// indices — `"15;0"` is white-on-black (a dark terminal), `"0;15"` is
/// black-on-white (a light one). rxvt may write three fields; the background is
/// always the last. Some terminals write the literal `"default"`, which tells us
/// nothing.
///
/// Returns `None` whenever the background can't be read as an index, including
/// the very common case of the variable being unset entirely — notably under
/// tmux and most modern terminals, which expose no equivalent hint short of an
/// OSC 11 query (rejected: it needs a raw-mode write-then-read against the tty
/// during startup and hangs on terminals that don't answer). Callers must supply
/// their own default; `ThemeMode::default()` is Dark.
///
/// Takes the value as an argument rather than reading the environment itself, so
/// the mapping is testable.
pub fn detect_mode(colorfgbg: Option<&str>) -> Option<ThemeMode> {
    let bg = colorfgbg?.split(';').next_back()?.trim();
    let index: u8 = bg.parse().ok()?;
    // ANSI 0-6 are the dark base colours and 8 is bright black; 7 (light grey),
    // 15 (white) and the remaining bright shades are light backgrounds.
    Some(match index {
        0..=6 | 8 => ThemeMode::Dark,
        _ => ThemeMode::Light,
    })
}

/// Decides which palette to start with.
///
/// A choice the user made explicitly (`stored`, as written to the `Config`
/// table by pressing 't') always wins: once someone has told us, we never
/// second-guess them from terminal hints that are frequently absent or wrong.
/// Only when there is no stored choice does auto-detection get a say, and if
/// that is also inconclusive the default palette is used.
///
/// Pure, taking both inputs as arguments, so the precedence is testable.
pub fn resolve_mode(stored: Option<&str>, colorfgbg: Option<&str>) -> ThemeMode {
    stored
        .and_then(ThemeMode::parse)
        .or_else(|| detect_mode(colorfgbg))
        .unwrap_or_default()
}

/// `resolve_mode` against the real environment.
pub fn resolve_mode_from_env(stored: Option<&str>) -> ThemeMode {
    resolve_mode(stored, std::env::var("COLORFGBG").ok().as_deref())
}

/// A full set of drawing colours. `Copy`, so `render.rs` can take it by value
/// out of the `App` read guard without cloning per frame.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub mode: ThemeMode,

    // ── Energy semantics ──
    pub consumption: Color,
    pub production: Color,
    pub grid_export: Color,
    pub grid_import: Color,
    pub battery_charge: Color,
    pub battery_discharge: Color,

    // ── Fill-level bands (battery remaining) ──
    pub level_good: Color,
    pub level_warn: Color,
    pub level_critical: Color,

    // ── Chrome ──
    /// Text/marks for something switched off or unknown.
    pub inactive: Color,
    /// Unfilled portion of a gauge bar.
    pub gauge_track: Color,
    pub status_bar_bg: Color,
    pub status_bar_fg: Color,
    /// Border of the currently focused panel.
    pub focus_border: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    /// Active text input, or the focused field of a dialog.
    pub input_active: Color,

    // ── Network infrastructure status ──
    pub status_ok: Color,
    pub status_slow: Color,
    pub status_degraded: Color,
    pub status_lost: Color,
    pub status_unknown: Color,

    // ── Internet traffic chart ──
    pub traffic_rx: Color,
    pub traffic_tx: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::for_mode(ThemeMode::default())
    }
}

impl Theme {
    pub fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
        }
    }

    /// The palette the app has always drawn: the base ANSI colours, which the
    /// terminal renders bright enough to read on a dark background. Kept
    /// value-for-value identical to the pre-theme hardcoded colours so enabling
    /// theming changed nothing for anyone already on a dark terminal.
    fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            consumption: Color::Red,
            production: Color::Green,
            grid_export: Color::Blue,
            grid_import: Color::Yellow,
            battery_charge: Color::Cyan,
            battery_discharge: Color::Yellow,
            level_good: Color::Green,
            level_warn: Color::Yellow,
            level_critical: Color::Red,
            inactive: Color::DarkGray,
            gauge_track: Color::DarkGray,
            status_bar_bg: Color::DarkGray,
            status_bar_fg: Color::White,
            focus_border: Color::Blue,
            selection_bg: Color::Blue,
            selection_fg: Color::White,
            input_active: Color::Yellow,
            status_ok: Color::Green,
            status_slow: Color::Yellow,
            status_degraded: Color::Magenta,
            status_lost: Color::Red,
            status_unknown: Color::DarkGray,
            traffic_rx: Color::Cyan,
            traffic_tx: Color::Magenta,
        }
    }

    /// Darkened equivalents for a light background.
    ///
    /// Uses 256-colour indices rather than the base ANSI names because the
    /// problem colours have no dark variant in the 16-colour set: plain yellow
    /// and cyan are close to invisible on white, and `White`/`DarkGray` as a
    /// foreground disappear entirely. 256-colour support is effectively
    /// universal on terminals that run this app (and is what `TERM` advertises);
    /// on a strictly 16-colour terminal these degrade to the nearest base
    /// colour, which is still readable.
    fn light() -> Self {
        // 256-colour palette indices, named for what they are.
        const DARK_RED: Color = Color::Indexed(124);
        const DARK_GREEN: Color = Color::Indexed(28);
        const DARK_BLUE: Color = Color::Indexed(25);
        const DARK_AMBER: Color = Color::Indexed(130);
        const DARK_TEAL: Color = Color::Indexed(30);
        const DARK_PURPLE: Color = Color::Indexed(90);
        const MID_GREY: Color = Color::Indexed(243);
        const LIGHT_GREY: Color = Color::Indexed(252);
        const PALE_BLUE: Color = Color::Indexed(153);

        Self {
            mode: ThemeMode::Light,
            consumption: DARK_RED,
            production: DARK_GREEN,
            grid_export: DARK_BLUE,
            grid_import: DARK_AMBER,
            battery_charge: DARK_TEAL,
            battery_discharge: DARK_AMBER,
            level_good: DARK_GREEN,
            level_warn: DARK_AMBER,
            level_critical: DARK_RED,
            inactive: MID_GREY,
            gauge_track: LIGHT_GREY,
            status_bar_bg: LIGHT_GREY,
            status_bar_fg: Color::Black,
            focus_border: DARK_BLUE,
            selection_bg: PALE_BLUE,
            selection_fg: Color::Black,
            input_active: DARK_AMBER,
            status_ok: DARK_GREEN,
            status_slow: DARK_AMBER,
            status_degraded: DARK_PURPLE,
            status_lost: DARK_RED,
            status_unknown: MID_GREY,
            traffic_rx: DARK_TEAL,
            traffic_tx: DARK_PURPLE,
        }
    }

    /// Colour for a network-infrastructure device's status. Lives here rather
    /// than on `NetworkDeviceStatus` so `app` stays independent of `tui`.
    pub fn status_color(&self, status: &NetworkDeviceStatus) -> Color {
        match status {
            NetworkDeviceStatus::Ok => self.status_ok,
            NetworkDeviceStatus::Slow => self.status_slow,
            NetworkDeviceStatus::Degraded => self.status_degraded,
            NetworkDeviceStatus::Lost => self.status_lost,
            NetworkDeviceStatus::Unknown => self.status_unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_stored_choice_beats_terminal_detection() {
        // The terminal says dark; the user said light. The user wins — that is
        // the whole point of remembering the choice.
        assert_eq!(resolve_mode(Some("light"), Some("15;0")), ThemeMode::Light);
        assert_eq!(resolve_mode(Some("dark"), Some("0;15")), ThemeMode::Dark);
    }

    #[test]
    fn detection_decides_only_when_nothing_was_stored() {
        assert_eq!(resolve_mode(None, Some("0;15")), ThemeMode::Light);
        assert_eq!(resolve_mode(None, Some("15;0")), ThemeMode::Dark);
    }

    #[test]
    fn an_unreadable_stored_value_does_not_win() {
        // A value we can't parse (hand-edited DB, or a mode removed in a later
        // version) must fall through to detection rather than pinning a
        // palette arbitrarily.
        assert_eq!(
            resolve_mode(Some("solarized"), Some("0;15")),
            ThemeMode::Light
        );
        assert_eq!(resolve_mode(Some(""), Some("15;0")), ThemeMode::Dark);
    }

    #[test]
    fn defaults_when_neither_source_says_anything() {
        // The common case: no stored choice, and a terminal that exposes no
        // hint (tmux, most modern terminals).
        assert_eq!(resolve_mode(None, None), ThemeMode::default());
        assert_eq!(resolve_mode(None, None), ThemeMode::Dark);
        assert_eq!(resolve_mode(Some("nonsense"), None), ThemeMode::Dark);
    }

    #[test]
    fn detect_mode_reads_a_dark_background() {
        // white on black, the usual dark-terminal value
        assert_eq!(detect_mode(Some("15;0")), Some(ThemeMode::Dark));
        assert_eq!(detect_mode(Some("7;0")), Some(ThemeMode::Dark));
        // bright black is still a dark background
        assert_eq!(detect_mode(Some("15;8")), Some(ThemeMode::Dark));
    }

    #[test]
    fn detect_mode_reads_a_light_background() {
        // black on white
        assert_eq!(detect_mode(Some("0;15")), Some(ThemeMode::Light));
        // light grey background
        assert_eq!(detect_mode(Some("0;7")), Some(ThemeMode::Light));
    }

    #[test]
    fn detect_mode_takes_the_background_from_the_last_field() {
        // rxvt writes three fields; the background is last, not second.
        assert_eq!(detect_mode(Some("0;default;15")), Some(ThemeMode::Light));
        assert_eq!(detect_mode(Some("15;default;0")), Some(ThemeMode::Dark));
    }

    #[test]
    fn detect_mode_is_none_when_the_terminal_tells_us_nothing() {
        // Unset entirely: tmux and most modern terminals. The caller must
        // default rather than guess.
        assert_eq!(detect_mode(None), None);
        assert_eq!(detect_mode(Some("default;default")), None);
        assert_eq!(detect_mode(Some("")), None);
        assert_eq!(detect_mode(Some("15;")), None);
        // Out of the ANSI index range.
        assert_eq!(detect_mode(Some("0;999")), None);
    }

    #[test]
    fn unknown_mode_falls_back_rather_than_guessing() {
        assert_eq!(ThemeMode::parse("dark"), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("LIGHT"), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("  light  "), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("solarized"), None);
        assert_eq!(ThemeMode::parse(""), None);
    }

    #[test]
    fn mode_round_trips_through_its_stored_form() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            assert_eq!(ThemeMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn toggling_twice_returns_to_the_start() {
        assert_eq!(ThemeMode::Dark.toggled(), ThemeMode::Light);
        assert_eq!(ThemeMode::Dark.toggled().toggled(), ThemeMode::Dark);
    }

    #[test]
    fn each_mode_builds_its_own_palette() {
        assert_eq!(Theme::for_mode(ThemeMode::Dark).mode, ThemeMode::Dark);
        assert_eq!(Theme::for_mode(ThemeMode::Light).mode, ThemeMode::Light);
        // The dark palette must keep the colours the app shipped with.
        assert_eq!(Theme::for_mode(ThemeMode::Dark).consumption, Color::Red);
        assert_eq!(Theme::default().mode, ThemeMode::Dark);
    }

    #[test]
    fn light_palette_avoids_foregrounds_that_vanish_on_white() {
        let light = Theme::light();
        // Plain white or bright yellow text on a light background is the exact
        // failure this palette exists to avoid.
        for color in [
            light.consumption,
            light.production,
            light.grid_import,
            light.battery_charge,
            light.status_bar_fg,
            light.selection_fg,
            light.input_active,
        ] {
            assert_ne!(color, Color::White);
            assert_ne!(color, Color::Yellow);
        }
    }

    #[test]
    fn status_colors_differ_per_state_in_both_modes() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let t = Theme::for_mode(mode);
            let ok = t.status_color(&NetworkDeviceStatus::Ok);
            let lost = t.status_color(&NetworkDeviceStatus::Lost);
            let unknown = t.status_color(&NetworkDeviceStatus::Unknown);
            assert_ne!(ok, lost, "{mode:?}: OK and LOST must be distinguishable");
            assert_ne!(ok, unknown);
            assert_ne!(lost, unknown);
        }
    }
}

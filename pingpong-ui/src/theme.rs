//! Colours, type and geometry: quiet neutral surfaces, one hairline weight,
//! colour kept for status, and Ping's orange as the one accent.

use gpui::{FontWeight, Rgba, Window, WindowAppearance};

pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Rgba {
    Rgba { r, g, b, a }
}

/// Status inks, the same on every surface in dark mode; light surfaces pull
/// them toward the text colour so they stay readable (see [`Theme::ink`]).
pub struct Ink;

impl Ink {
    pub const FRESH: Rgba = rgba(0.204, 0.780, 0.349, 1.0);
    pub const ATTENTION: Rgba = rgba(0.961, 0.651, 0.137, 1.0);
    pub const DANGER: Rgba = rgba(0.961, 0.271, 0.227, 1.0);
    /// The ball in Ping's icon.
    pub const ACCENT: Rgba = rgba(0.976, 0.475, 0.290, 1.0);
    pub const IDLE: Rgba = rgba(0.557, 0.557, 0.576, 1.0);
}

pub struct Radius;

impl Radius {
    pub const CHIP: f32 = 5.0;
    pub const CONTROL: f32 = 6.0;
    pub const ROW: f32 = 7.0;
    pub const CARD: f32 = 10.0;
    pub const PANEL: f32 = 12.0;
    pub const SHEET: f32 = 14.0;
}

/// Type sizes and weights (points).
pub struct Type;

impl Type {
    pub const META: f32 = 11.0;
    pub const BODY: f32 = 13.0;
    pub const TITLE: f32 = 15.0;
    pub const DISPLAY: f32 = 22.0;
    pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
    pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
}

/// Fixed geometry shared by both apps.
pub struct Metrics;

impl Metrics {
    /// The strip at the top of a window: traffic lights and toolbar on a Mac.
    pub const TOOLBAR: f32 = 44.0;
    pub const SIDEBAR: f32 = 216.0;
    pub const NAV_ROW: f32 = 28.0;
    pub const SETTINGS_ROW: f32 = 50.0;
    pub const CONTROL: f32 = 26.0;
    /// Room for the traffic lights when the title bar is the app's own.
    pub const TRAFFIC_LIGHTS: f32 = if cfg!(target_os = "macos") { 72.0 } else { 0.0 };
    /// Content column widths.
    pub const FORM: f32 = 640.0;
}

/// Every colour a view needs, for one appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    /// Text and marks.
    pub primary: Rgba,
    pub secondary: Rgba,
    pub tertiary: Rgba,
    /// Placeholders and disabled text.
    pub quaternary: Rgba,
    /// The work surface.
    pub background: Rgba,
    pub sidebar: Rgba,
    /// Menus, sheets and popovers.
    pub floating: Rgba,
    pub floating_stroke: Rgba,
    /// One-point separators.
    pub hairline: Rgba,
    /// A grouped card's fill and outline.
    pub card: Rgba,
    pub card_stroke: Rgba,
    pub hover: Rgba,
    pub pressed: Rgba,
    /// The glass pill behind a selected row or segment.
    pub selected: Rgba,
    pub selected_stroke: Rgba,
    /// A quiet button.
    pub control: Rgba,
    pub control_hover: Rgba,
    pub control_stroke: Rgba,
    /// Text fields.
    pub field: Rgba,
    pub field_stroke: Rgba,
    pub focus: Rgba,
    /// Scrim behind a sheet.
    pub scrim: Rgba,
}

impl Theme {
    pub const fn dark() -> Theme {
        let p = rgba(1.0, 1.0, 1.0, 1.0);
        Theme {
            dark: true,
            primary: rgba(0.95, 0.95, 0.96, 1.0),
            secondary: rgba(1.0, 1.0, 1.0, 0.62),
            tertiary: rgba(1.0, 1.0, 1.0, 0.42),
            quaternary: rgba(1.0, 1.0, 1.0, 0.26),
            background: rgba(0.071, 0.075, 0.086, 1.0),
            sidebar: rgba(0.110, 0.114, 0.129, 1.0),
            floating: rgba(0.145, 0.149, 0.169, 1.0),
            floating_stroke: rgba(1.0, 1.0, 1.0, 0.09),
            hairline: rgba(p.r, p.g, p.b, 0.07),
            card: rgba(p.r, p.g, p.b, 0.025),
            card_stroke: rgba(p.r, p.g, p.b, 0.07),
            hover: rgba(p.r, p.g, p.b, 0.06),
            pressed: rgba(p.r, p.g, p.b, 0.10),
            selected: rgba(1.0, 1.0, 1.0, 0.11),
            selected_stroke: rgba(1.0, 1.0, 1.0, 0.10),
            control: rgba(p.r, p.g, p.b, 0.05),
            control_hover: rgba(p.r, p.g, p.b, 0.10),
            control_stroke: rgba(p.r, p.g, p.b, 0.10),
            field: rgba(0.0, 0.0, 0.0, 0.22),
            field_stroke: rgba(p.r, p.g, p.b, 0.10),
            focus: rgba(0.976, 0.475, 0.290, 0.75),
            scrim: rgba(0.0, 0.0, 0.0, 0.45),
        }
    }

    pub const fn light() -> Theme {
        let p = rgba(0.0, 0.0, 0.0, 1.0);
        Theme {
            dark: false,
            primary: rgba(0.09, 0.09, 0.10, 1.0),
            secondary: rgba(0.0, 0.0, 0.0, 0.58),
            tertiary: rgba(0.0, 0.0, 0.0, 0.42),
            quaternary: rgba(0.0, 0.0, 0.0, 0.26),
            background: rgba(0.973, 0.969, 0.957, 1.0),
            sidebar: rgba(0.925, 0.918, 0.902, 1.0),
            floating: rgba(0.992, 0.988, 0.980, 1.0),
            floating_stroke: rgba(0.0, 0.0, 0.0, 0.10),
            hairline: rgba(p.r, p.g, p.b, 0.08),
            card: rgba(1.0, 1.0, 1.0, 0.62),
            card_stroke: rgba(p.r, p.g, p.b, 0.08),
            hover: rgba(p.r, p.g, p.b, 0.05),
            pressed: rgba(p.r, p.g, p.b, 0.09),
            selected: rgba(1.0, 1.0, 1.0, 0.78),
            selected_stroke: rgba(0.0, 0.0, 0.0, 0.09),
            control: rgba(1.0, 1.0, 1.0, 0.70),
            control_hover: rgba(1.0, 1.0, 1.0, 1.0),
            control_stroke: rgba(p.r, p.g, p.b, 0.12),
            field: rgba(1.0, 1.0, 1.0, 0.85),
            field_stroke: rgba(p.r, p.g, p.b, 0.12),
            focus: rgba(0.918, 0.392, 0.200, 0.70),
            scrim: rgba(0.0, 0.0, 0.0, 0.18),
        }
    }

    pub fn of(window: &Window) -> Theme {
        match std::env::var("PINGPONG_APPEARANCE").as_deref() {
            Ok("light") => return Theme::light(),
            Ok("dark") => return Theme::dark(),
            _ => {}
        }
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Theme::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Theme::light(),
        }
    }

    /// A status ink that reads on this surface: bright on dark, pulled halfway
    /// toward the text colour on light.
    pub fn ink(&self, hue: Rgba) -> Rgba {
        if self.dark {
            return hue;
        }
        let mix = |a: f32, b: f32| a * 0.62 + b * 0.38;
        rgba(
            mix(hue.r, self.primary.r),
            mix(hue.g, self.primary.g),
            mix(hue.b, self.primary.b),
            hue.a,
        )
    }

    /// The filled button (primary actions): the text colour itself.
    pub fn solid(&self) -> Rgba {
        self.primary
    }

    pub fn on_solid(&self) -> Rgba {
        if self.dark {
            rgba(0.07, 0.07, 0.08, 1.0)
        } else {
            rgba(1.0, 1.0, 1.0, 1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inks_stay_bright_on_dark_and_darken_on_light() {
        assert_eq!(Theme::dark().ink(Ink::FRESH), Ink::FRESH);
        let light = Theme::light().ink(Ink::FRESH);
        assert!(light.g < Ink::FRESH.g);
    }

    #[test]
    fn text_tones_step_down() {
        for t in [Theme::dark(), Theme::light()] {
            assert!(t.secondary.a > t.tertiary.a && t.tertiary.a > t.quaternary.a);
        }
    }
}

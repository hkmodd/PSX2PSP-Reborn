//! Design system: dark surfaces, PlayStation accents (blue ✕, red ○, green △, pink □),
//! generous radii, soft shadows and explicit hover/press feedback on every control.

use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector, color, theme};

pub const BG: Color = color!(0x0b0e14);
pub const SURFACE: Color = color!(0x141925);
pub const SURFACE_HI: Color = color!(0x1c2333);
pub const STROKE: Color = color!(0x2a3346);
pub const TEXT: Color = color!(0xe6ebf5);
pub const MUTED: Color = color!(0x8a94a8);
pub const BLUE: Color = color!(0x3d7bff);
pub const RED: Color = color!(0xff4d6a);
pub const GREEN: Color = color!(0x2fd58a);
pub const PINK: Color = color!(0xff6fd8);
pub const AMBER: Color = color!(0xffb547);

pub const RADIUS: f32 = 12.0;

pub fn theme() -> Theme {
    Theme::custom(
        "PSX2PSP Reborn",
        theme::Palette { background: BG, text: TEXT, primary: BLUE, success: GREEN, warning: AMBER, danger: RED },
    )
}

fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border { color: STROKE, width: 1.0, radius: RADIUS.into() },
        shadow: Shadow { color: alpha(Color::BLACK, 0.35), offset: Vector::new(0.0, 6.0), blur_radius: 18.0 },
        ..Default::default()
    }
}

pub fn panel(_: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(alpha(SURFACE, 0.6))), ..Default::default() }
}

pub fn well(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE_HI)),
        border: Border { color: STROKE, width: 1.0, radius: (RADIUS - 4.0).into() },
        ..Default::default()
    }
}

pub fn toast(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(SURFACE_HI)),
        text_color: Some(TEXT),
        border: Border { color: accent, width: 1.5, radius: RADIUS.into() },
        shadow: Shadow { color: alpha(Color::BLACK, 0.5), offset: Vector::new(0.0, 8.0), blur_radius: 24.0 },
        ..Default::default()
    }
}

/// Filled accent button with lift-on-hover.
pub fn solid(accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (bg, lift) = match status {
            button::Status::Active => (accent, 4.0),
            button::Status::Hovered => (lighten(accent, 0.12), 10.0),
            button::Status::Pressed => (darken(accent, 0.12), 1.0),
            button::Status::Disabled => (alpha(accent, 0.30), 0.0),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if status == button::Status::Disabled { alpha(Color::WHITE, 0.5) } else { Color::WHITE },
            border: Border { radius: (RADIUS - 2.0).into(), ..Default::default() },
            shadow: Shadow { color: alpha(accent, 0.35), offset: Vector::new(0.0, lift * 0.4), blur_radius: lift * 2.0 },
            snap: true,
        }
    }
}

/// Quiet outlined button for secondary actions.
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let (bg, fg, stroke) = match status {
        button::Status::Active => (Color::TRANSPARENT, TEXT, STROKE),
        button::Status::Hovered => (SURFACE_HI, Color::WHITE, BLUE),
        button::Status::Pressed => (alpha(BLUE, 0.25), Color::WHITE, BLUE),
        button::Status::Disabled => (Color::TRANSPARENT, alpha(MUTED, 0.5), alpha(STROKE, 0.5)),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: fg,
        border: Border { color: stroke, width: 1.0, radius: (RADIUS - 4.0).into() },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Segmented-control tab.
pub fn tab(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(Background::Color(if selected {
                BLUE
            } else if hovered {
                SURFACE_HI
            } else {
                Color::TRANSPARENT
            })),
            text_color: if selected || hovered { Color::WHITE } else { MUTED },
            border: Border { radius: (RADIUS - 4.0).into(), ..Default::default() },
            shadow: if selected {
                Shadow { color: alpha(BLUE, 0.45), offset: Vector::new(0.0, 2.0), blur_radius: 12.0 }
            } else {
                Shadow::default()
            },
            snap: true,
        }
    }
}

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let base = text_input::default(theme, status);
    let border = match status {
        text_input::Status::Focused { .. } => Border { color: BLUE, width: 1.5, radius: (RADIUS - 4.0).into() },
        text_input::Status::Hovered => Border { color: alpha(BLUE, 0.6), width: 1.0, radius: (RADIUS - 4.0).into() },
        _ => Border { color: STROKE, width: 1.0, radius: (RADIUS - 4.0).into() },
    };
    text_input::Style { background: Background::Color(SURFACE_HI), border, ..base }
}

fn lighten(c: Color, k: f32) -> Color {
    Color { r: c.r + (1.0 - c.r) * k, g: c.g + (1.0 - c.g) * k, b: c.b + (1.0 - c.b) * k, a: c.a }
}

fn darken(c: Color, k: f32) -> Color {
    Color { r: c.r * (1.0 - k), g: c.g * (1.0 - k), b: c.b * (1.0 - k), a: c.a }
}

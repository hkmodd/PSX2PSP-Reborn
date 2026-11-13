//! Design system: dark surfaces, PlayStation accents (blue ✕, red ○, green △, pink □),
//! generous radii, soft glows and explicit hover/press/disabled feedback on every control.
//!
//! Reusable pieces for any panel widget: [`card`] (plain surface), [`card_frame`] + [`card_body`]
//! (accent card with a gradient rim), [`primary_button`] (call to action), [`icon_button`],
//! [`chip`], [`segmented`] + [`segment`], [`toggle`], [`input`].

use iced::font::Weight;
use iced::gradient::Linear;
use iced::widget::{button, container, progress_bar, slider as sl, text_input};
use iced::{Background, Border, Color, Degrees, Font, Gradient, Shadow, Theme, Vector, color, theme};

pub const BG: Color = color!(0x0b0e14);
pub const SURFACE_LO: Color = color!(0x0e121b);
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
/// Gradient partners of the accents (call-to-action fills).
pub const VIOLET: Color = color!(0x9b6bff);
pub const TEAL: Color = color!(0x19c3b4);

pub const RADIUS: f32 = 12.0;

/// UI typeface (Inter, embedded; see `assets/fonts`).
pub const FONT: Font = Font::with_name("Inter");
pub const BOLD: Font = Font { weight: Weight::Bold, ..FONT };

pub fn theme() -> Theme {
    Theme::custom(
        "PSX2PSP Reborn",
        theme::Palette { background: BG, text: TEXT, primary: BLUE, success: GREEN, warning: AMBER, danger: RED },
    )
}

pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub fn lighten(c: Color, k: f32) -> Color {
    Color { r: c.r + (1.0 - c.r) * k, g: c.g + (1.0 - c.g) * k, b: c.b + (1.0 - c.b) * k, a: c.a }
}

fn darken(c: Color, k: f32) -> Color {
    Color { r: c.r * (1.0 - k), g: c.g * (1.0 - k), b: c.b * (1.0 - k), a: c.a }
}

fn mix(a: Color, b: Color, k: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * k, g: a.g + (b.g - a.g) * k, b: a.b + (b.b - a.b) * k, a: a.a + (b.a - a.a) * k }
}

/// Linear gradient at `deg` (CSS convention: 0 = upwards, 90 = rightwards).
fn linear(deg: f32, stops: &[(f32, Color)]) -> Background {
    let g = stops.iter().fold(Linear::new(Degrees(deg)), |g, &(at, c)| g.add_stop(at, c));
    Background::Gradient(Gradient::Linear(g))
}

fn rounded(r: f32) -> Border {
    Border { radius: r.into(), ..Default::default() }
}

fn glow(c: Color, a: f32, blur: f32, dy: f32) -> Shadow {
    Shadow { color: alpha(c, a), offset: Vector::new(0.0, dy), blur_radius: blur }
}

// ───────────────────────────────── surfaces ─────────────────────────────────

/// Plain raised surface.
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(linear(180.0, &[(0.0, SURFACE_HI), (0.25, SURFACE), (1.0, SURFACE)])),
        border: Border { color: STROKE, width: 1.0, radius: (RADIUS + 2.0).into() },
        shadow: glow(Color::BLACK, 0.40, 22.0, 8.0),
        ..Default::default()
    }
}

/// Gradient rim of an accent card: wrap [`card_body`] in it with 1 px padding.
pub fn card_frame(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(linear(155.0, &[(0.0, alpha(accent, 0.70)), (0.35, alpha(STROKE, 0.9)), (1.0, alpha(STROKE, 0.35))])),
        border: rounded(RADIUS + 3.0),
        shadow: Shadow { color: alpha(Color::BLACK, 0.5), offset: Vector::new(0.0, 10.0), blur_radius: 26.0 },
        ..Default::default()
    }
}

/// Inside of an accent card: a faint accent wash fading into the surface.
pub fn card_body(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(linear(180.0, &[(0.0, mix(SURFACE, accent, 0.09)), (0.22, SURFACE), (1.0, mix(SURFACE, BG, 0.35))])),
        border: rounded(RADIUS + 2.0),
        ..Default::default()
    }
}

/// The left column's backdrop.
pub fn panel(_: &Theme) -> container::Style {
    container::Style { background: Some(linear(180.0, &[(0.0, color!(0x111624)), (1.0, BG)])), ..Default::default() }
}

/// 1 px separator line.
pub fn hairline(c: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(Background::Color(c)), ..Default::default() }
}

pub fn well(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE_LO)),
        border: Border { color: alpha(STROKE, 0.8), width: 1.0, radius: (RADIUS - 2.0).into() },
        ..Default::default()
    }
}

/// Picture frame of an artwork tile.
pub fn thumb(_: &Theme) -> container::Style {
    container::Style {
        background: Some(linear(180.0, &[(0.0, color!(0x0c1018)), (1.0, color!(0x121826))])),
        border: Border { color: alpha(STROKE, 0.7), width: 1.0, radius: (RADIUS - 4.0).into() },
        ..Default::default()
    }
}

/// Rounded-square glyph badge (section icons).
pub fn badge(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(linear(160.0, &[(0.0, alpha(accent, 0.30)), (1.0, alpha(accent, 0.10))])),
        border: Border { color: alpha(accent, 0.55), width: 1.0, radius: 9.0.into() },
        shadow: glow(accent, 0.30, 14.0, 0.0),
        ..Default::default()
    }
}

/// Pill label: tinted fill, accent text.
pub fn chip(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(alpha(accent, 0.13))),
        text_color: Some(lighten(accent, 0.25)),
        border: Border { color: alpha(accent, 0.38), width: 1.0, radius: 999.0.into() },
        ..Default::default()
    }
}

/// Progress-tracker node: done (filled, glowing), current (outlined) or pending (dim).
pub fn step(accent: Color, done: bool, current: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let (bg, border, shadow) = if done {
            (linear(150.0, &[(0.0, lighten(accent, 0.15)), (1.0, darken(accent, 0.15))]), Border { color: lighten(accent, 0.35), width: 1.0, radius: 999.0.into() }, glow(accent, 0.55, 16.0, 0.0))
        } else if current {
            (Background::Color(alpha(accent, 0.12)), Border { color: accent, width: 1.5, radius: 999.0.into() }, glow(accent, 0.25, 12.0, 0.0))
        } else {
            (Background::Color(SURFACE_LO), Border { color: STROKE, width: 1.0, radius: 999.0.into() }, Shadow::default())
        };
        container::Style { background: Some(bg), border, shadow, ..Default::default() }
    }
}

/// Connector between two tracker nodes.
pub fn rail(from: Color, to: Color, lit: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(if lit { linear(90.0, &[(0.0, from), (1.0, to)]) } else { Background::Color(STROKE) }),
        border: rounded(2.0),
        ..Default::default()
    }
}

/// App logo plate.
pub fn logo(_: &Theme) -> container::Style {
    container::Style {
        background: Some(linear(160.0, &[(0.0, color!(0x1d2540)), (1.0, color!(0x0d111b))])),
        border: Border { color: alpha(BLUE, 0.45), width: 1.0, radius: 14.0.into() },
        shadow: glow(BLUE, 0.30, 20.0, 2.0),
        ..Default::default()
    }
}

pub fn toast(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(SURFACE_HI)),
        text_color: Some(TEXT),
        border: Border { color: accent, width: 1.5, radius: RADIUS.into() },
        shadow: glow(Color::BLACK, 0.5, 24.0, 8.0),
        ..Default::default()
    }
}

// ───────────────────────────────── buttons ─────────────────────────────────

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
            border: rounded(RADIUS - 2.0),
            shadow: glow(accent, 0.35, lift * 2.0, lift * 0.4),
            snap: true,
        }
    }
}

/// Call to action: diagonal `from → to` gradient, glow that grows on hover; flat grey when disabled.
pub fn primary_button(from: Color, to: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let fill = |k: f32, d: f32| linear(100.0, &[(0.0, darken(lighten(from, k), d)), (1.0, darken(lighten(to, k), d))]);
        let (background, shadow, text_color, border) = match status {
            button::Status::Active => (fill(0.0, 0.0), glow(from, 0.40, 18.0, 5.0), Color::WHITE, Border { color: alpha(Color::WHITE, 0.18), width: 1.0, radius: RADIUS.into() }),
            button::Status::Hovered => (fill(0.12, 0.0), glow(from, 0.60, 28.0, 7.0), Color::WHITE, Border { color: alpha(Color::WHITE, 0.32), width: 1.0, radius: RADIUS.into() }),
            button::Status::Pressed => (fill(0.0, 0.14), glow(from, 0.35, 8.0, 1.0), Color::WHITE, Border { color: alpha(Color::WHITE, 0.10), width: 1.0, radius: RADIUS.into() }),
            button::Status::Disabled => (Background::Color(SURFACE_HI), Shadow::default(), alpha(MUTED, 0.75), Border { color: STROKE, width: 1.0, radius: RADIUS.into() }),
        };
        button::Style { background: Some(background), text_color, border, shadow, snap: true }
    }
}

/// A call to action whose job is running: dimmed accent, outlined, no hover response.
pub fn busy_button(accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, _| button::Style {
        background: Some(linear(100.0, &[(0.0, alpha(accent, 0.30)), (1.0, alpha(accent, 0.12))])),
        text_color: Color::WHITE,
        border: Border { color: alpha(accent, 0.8), width: 1.0, radius: RADIUS.into() },
        shadow: glow(accent, 0.30, 16.0, 0.0),
        snap: true,
    }
}

/// Quiet outlined button for secondary actions.
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let (bg, fg, stroke) = match status {
        button::Status::Active => (alpha(SURFACE_HI, 0.35), TEXT, STROKE),
        button::Status::Hovered => (SURFACE_HI, Color::WHITE, alpha(BLUE, 0.8)),
        button::Status::Pressed => (alpha(BLUE, 0.25), Color::WHITE, BLUE),
        button::Status::Disabled => (Color::TRANSPARENT, alpha(MUTED, 0.5), alpha(STROKE, 0.5)),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: fg,
        border: Border { color: stroke, width: 1.0, radius: (RADIUS - 3.0).into() },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Square icon button; lights up in `accent` on hover.
pub fn icon_button(accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (bg, stroke, shadow) = match status {
            button::Status::Active => (alpha(SURFACE_HI, 0.55), alpha(STROKE, 0.9), Shadow::default()),
            button::Status::Hovered => (alpha(accent, 0.20), alpha(accent, 0.85), glow(accent, 0.35, 12.0, 0.0)),
            button::Status::Pressed => (alpha(accent, 0.32), accent, Shadow::default()),
            button::Status::Disabled => (Color::TRANSPARENT, alpha(STROKE, 0.45), Shadow::default()),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: TEXT,
            border: Border { color: stroke, width: 1.0, radius: 9.0.into() },
            shadow,
            snap: true,
        }
    }
}

/// Collapsible section header row.
pub fn section_header(accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered => Some(Background::Color(alpha(accent, 0.07))),
            button::Status::Pressed => Some(Background::Color(alpha(accent, 0.12))),
            _ => None,
        },
        text_color: TEXT,
        border: rounded(RADIUS - 2.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Track of a segmented control.
pub fn segmented(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE_LO)),
        border: Border { color: alpha(STROKE, 0.9), width: 1.0, radius: RADIUS.into() },
        ..Default::default()
    }
}

/// One segment; the selected one is a raised accent pill.
pub fn segment(selected: bool, accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (background, shadow) = if selected {
            (
                Some(linear(160.0, &[(0.0, lighten(accent, if hovered { 0.12 } else { 0.0 })), (1.0, darken(accent, 0.18))])),
                glow(accent, 0.45, 14.0, 2.0),
            )
        } else if hovered {
            (Some(Background::Color(SURFACE_HI)), Shadow::default())
        } else {
            (None, Shadow::default())
        };
        button::Style {
            background,
            text_color: if selected || hovered { Color::WHITE } else { MUTED },
            border: rounded(RADIUS - 3.0),
            shadow,
            snap: true,
        }
    }
}

/// Segmented-control tab (blue).
pub fn tab(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    segment(selected, BLUE)
}

/// On/off pill (patch switches).
pub fn toggle(on: bool, accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (bg, stroke, fg) = match (on, hovered) {
            (true, false) => (alpha(accent, 0.20), alpha(accent, 0.85), Color::WHITE),
            (true, true) => (alpha(accent, 0.30), lighten(accent, 0.2), Color::WHITE),
            (false, false) => (Color::TRANSPARENT, STROKE, MUTED),
            (false, true) => (alpha(SURFACE_HI, 0.8), alpha(accent, 0.6), TEXT),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: fg,
            border: Border { color: stroke, width: 1.0, radius: 999.0.into() },
            shadow: if on { glow(accent, 0.25, 10.0, 0.0) } else { Shadow::default() },
            snap: true,
        }
    }
}

/// Clickable drop target; `hot` while files are dragged over the window.
pub fn drop_zone(hot: bool, accent: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let k = match (hot, status) {
            (true, _) => 0.22,
            (_, button::Status::Hovered) => 0.12,
            (_, button::Status::Pressed) => 0.18,
            _ => 0.06,
        };
        button::Style {
            background: Some(linear(180.0, &[(0.0, alpha(accent, k + 0.03)), (1.0, alpha(accent, k * 0.5))])),
            text_color: TEXT,
            border: rounded(RADIUS),
            shadow: if hot || status == button::Status::Hovered { glow(accent, if hot { 0.55 } else { 0.25 }, 22.0, 0.0) } else { Shadow::default() },
            snap: true,
        }
    }
}

/// Artwork tile (the whole tile browses).
pub fn tile(accent: Color, filled: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (stroke, shadow, bg) = match status {
            button::Status::Hovered => (alpha(accent, 0.85), glow(accent, 0.30, 18.0, 0.0), SURFACE_HI),
            button::Status::Pressed => (accent, Shadow::default(), alpha(accent, 0.15)),
            _ => (if filled { alpha(accent, 0.35) } else { alpha(STROKE, 0.8) }, Shadow::default(), alpha(SURFACE_HI, 0.45)),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: TEXT,
            border: Border { color: stroke, width: 1.0, radius: RADIUS.into() },
            shadow,
            snap: true,
        }
    }
}

// ───────────────────────────────── inputs ─────────────────────────────────

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let base = text_input::default(theme, status);
    let r = (RADIUS - 3.0).into();
    let (border, background) = match status {
        text_input::Status::Focused { .. } => (Border { color: BLUE, width: 1.5, radius: r }, color!(0x111829)),
        text_input::Status::Hovered => (Border { color: alpha(BLUE, 0.55), width: 1.0, radius: r }, SURFACE_LO),
        _ => (Border { color: alpha(STROKE, 0.9), width: 1.0, radius: r }, SURFACE_LO),
    };
    text_input::Style { background: Background::Color(background), border, placeholder: alpha(MUTED, 0.7), selection: alpha(BLUE, 0.4), ..base }
}

pub fn slider(accent: Color) -> impl Fn(&Theme, sl::Status) -> sl::Style {
    move |_, status| {
        let ring = match status {
            sl::Status::Active => accent,
            sl::Status::Hovered | sl::Status::Dragged => lighten(accent, 0.3),
        };
        sl::Style {
            rail: sl::Rail {
                backgrounds: (linear(90.0, &[(0.0, darken(accent, 0.2)), (1.0, accent)]), Background::Color(STROKE)),
                width: 5.0,
                border: rounded(3.0),
            },
            handle: sl::Handle {
                shape: sl::HandleShape::Circle { radius: if status == sl::Status::Active { 8.0 } else { 9.0 } },
                background: Background::Color(Color::WHITE),
                border_width: 3.0,
                border_color: ring,
            },
        }
    }
}

/// Budget meter.
pub fn meter(accent: Color) -> impl Fn(&Theme) -> progress_bar::Style {
    move |_| progress_bar::Style {
        background: Background::Color(SURFACE_LO),
        bar: linear(90.0, &[(0.0, darken(accent, 0.15)), (1.0, lighten(accent, 0.15))]),
        border: Border { color: alpha(STROKE, 0.8), width: 1.0, radius: 999.0.into() },
    }
}

//! Vector icon set: tiny hand-drawn 24×24 SVGs compiled into the binary, tinted per use.
//!
//! Each icon is rasterized once per (size, colour) by the renderer's SVG atlas, so drawing them
//! every frame (the XMB preview redraws continuously) costs a textured quad each.

use std::sync::LazyLock;

use iced::widget::svg::{self, Svg};
use iced::{Color, Radians};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    // PlayStation face buttons: one per section.
    Cross,
    Circle,
    Triangle,
    Square,
    Plus,
    Close,
    ArrowUp,
    Folder,
    Image,
    Disc,
    Video,
    Music,
    Sparkles,
    Link,
    Search,
    ChevronDown,
    Check,
    Import,
    Export,
    Package,
    Wrench,
    Shield,
    Star,
    Reset,
    Layers,
    Drop,
    Alert,
    Clock,
}

const ALL: [Icon; 28] = [
    Icon::Cross,
    Icon::Circle,
    Icon::Triangle,
    Icon::Square,
    Icon::Plus,
    Icon::Close,
    Icon::ArrowUp,
    Icon::Folder,
    Icon::Image,
    Icon::Disc,
    Icon::Video,
    Icon::Music,
    Icon::Sparkles,
    Icon::Link,
    Icon::Search,
    Icon::ChevronDown,
    Icon::Check,
    Icon::Import,
    Icon::Export,
    Icon::Package,
    Icon::Wrench,
    Icon::Shield,
    Icon::Star,
    Icon::Reset,
    Icon::Layers,
    Icon::Drop,
    Icon::Alert,
    Icon::Clock,
];

impl Icon {
    /// SVG body (inside a 24×24 white-stroked root).
    fn body(self) -> &'static str {
        match self {
            Icon::Cross => r#"<path stroke-width="2.6" d="M6.5 6.5l11 11M17.5 6.5l-11 11"/>"#,
            Icon::Circle => r#"<circle stroke-width="2.6" cx="12" cy="12" r="7"/>"#,
            Icon::Triangle => r#"<path stroke-width="2.4" d="M12 5l7.5 13h-15z"/>"#,
            Icon::Square => r#"<rect stroke-width="2.4" x="5.5" y="5.5" width="13" height="13" rx="1"/>"#,
            Icon::Plus => r#"<path stroke-width="2.4" d="M12 5v14M5 12h14"/>"#,
            Icon::Close => r#"<path d="M7 7l10 10M17 7L7 17"/>"#,
            Icon::ArrowUp => r#"<path d="M12 19V5M6 11l6-6 6 6"/>"#,
            Icon::Folder => r#"<path d="M3 7.5A2.5 2.5 0 0 1 5.5 5H9l2 2.5h7.5A2.5 2.5 0 0 1 21 10v6.5a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 16.5z"/>"#,
            Icon::Image => r#"<rect x="3" y="4.5" width="18" height="15" rx="2.5"/><circle cx="8.5" cy="9.5" r="1.6"/><path d="M4 18l5-5 4 4 3-3 4.5 4.5"/>"#,
            Icon::Disc => r#"<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="2.5"/><path d="M12 6.5a5.5 5.5 0 0 1 5.5 5.5"/>"#,
            Icon::Video => r#"<rect x="2.5" y="6" width="13" height="12" rx="2.5"/><path d="M15.5 10.5l6-3.5v10l-6-3.5z"/>"#,
            Icon::Music => r#"<path d="M9 17.5V5.5l11-2v12"/><circle cx="6" cy="17.5" r="3"/><circle cx="17" cy="15.5" r="3"/>"#,
            Icon::Sparkles => r#"<path d="M10 3.5l1.9 5 5 1.9-5 1.9-1.9 5-1.9-5-5-1.9 5-1.9z"/><path stroke-width="1.6" d="M18 14.5l.9 2.2 2.2.9-2.2.9-.9 2.2-.9-2.2-2.2-.9 2.2-.9z"/>"#,
            Icon::Link => r#"<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>"#,
            Icon::Search => r#"<circle cx="11" cy="11" r="6.5"/><path d="M20 20l-4.4-4.4"/>"#,
            Icon::ChevronDown => r#"<path stroke-width="2.4" d="M6 9.5l6 6 6-6"/>"#,
            Icon::Check => r#"<path stroke-width="2.6" d="M5 12.5l4.5 4.5L19 7.5"/>"#,
            Icon::Import => r#"<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>"#,
            Icon::Export => r#"<path d="M12 15V4M7 9l5-5 5 5M5 20h14"/>"#,
            Icon::Package => r#"<path d="M12 2.8l8.2 4.6v9.2L12 21.2l-8.2-4.6V7.4z"/><path d="M3.8 7.4L12 12l8.2-4.6M12 12v9.2"/>"#,
            Icon::Wrench => r#"<path d="M14.7 6.3a4.5 4.5 0 0 0 5.5 5.5L13 19a2.1 2.1 0 0 1-3-3l7.2-7.2"/><path d="M14.7 6.3L17 4M4 20l3.5-3.5"/>"#,
            Icon::Shield => r#"<path d="M12 3l7.5 3v6c0 4.6-3.2 7.8-7.5 9.3C7.7 19.8 4.5 16.6 4.5 12V6z"/><path d="M8.5 12l2.5 2.5 4.5-5"/>"#,
            Icon::Star => r#"<path d="M12 3.5l2.6 5.4 5.9.8-4.3 4.1 1 5.8L12 16.8l-5.2 2.8 1-5.8-4.3-4.1 5.9-.8z"/>"#,
            Icon::Reset => r#"<path d="M4.5 12a7.5 7.5 0 1 0 2.4-5.5"/><path d="M4.5 4v4.5H9"/>"#,
            Icon::Layers => r#"<path d="M12 3.5l9 5-9 5-9-5z"/><path d="M3 13l9 5 9-5"/>"#,
            Icon::Drop => r#"<path d="M12 3v11M7.5 9.5L12 14l4.5-4.5"/><path d="M4 14.5v3A2.5 2.5 0 0 0 6.5 20h11a2.5 2.5 0 0 0 2.5-2.5v-3"/>"#,
            Icon::Alert => r#"<path d="M12 3.8l9 15.7H3z"/><path d="M12 10v4.2M12 17.2v.1"/>"#,
            Icon::Clock => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>"#,
        }
    }
}

static HANDLES: LazyLock<Vec<svg::Handle>> = LazyLock::new(|| {
    ALL.iter()
        .map(|i| {
            let doc = format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"##,
                i.body()
            );
            svg::Handle::from_memory(doc.into_bytes())
        })
        .collect()
});

fn handle(i: Icon) -> svg::Handle {
    HANDLES[i as usize].clone()
}

/// `i` drawn at `size` px in `color`.
pub fn icon<'a>(i: Icon, size: f32, color: Color) -> Svg<'a> {
    Svg::new(handle(i)).width(size).height(size).style(move |_, _| svg::Style { color: Some(color) })
}

/// Same, rotated (for animated chevrons).
pub fn rotated<'a>(i: Icon, size: f32, color: Color, angle: f32) -> Svg<'a> {
    icon(i, size, color).rotation(Radians(angle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_handle() {
        assert_eq!(HANDLES.len(), ALL.len());
        for (n, i) in ALL.iter().enumerate() {
            assert_eq!(ALL.iter().position(|x| x == i), Some(n), "{i:?} listed twice");
        }
    }
}

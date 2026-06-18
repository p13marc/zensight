//! Design tokens: the typographic and spacing scales.
//!
//! Color tokens live in [`super::theme`] (theme-aware `ThemeColors`). This module
//! holds the *dimensional* tokens — font sizes and an 8pt spacing grid — so that
//! every view draws from one scale instead of ad-hoc literals. Use these instead
//! of bare `.size(13)` / `.padding(10)` / `.spacing(15)` calls.
//!
//! Type scale (5 steps) and an 8pt spacing scale, per the GUI design-system plan
//! (docs/plans/gui/03-design-system.md).

/// Typographic scale (pixels). Five steps, used app-wide. `f32` so it feeds
/// `text(..).size(..)` (Iced `Pixels`) directly.
pub mod font {
    /// Captions, labels, dense table cells, metadata.
    pub const CAPTION: f32 = 12.0;
    /// Default body text.
    pub const BODY: f32 = 14.0;
    /// Emphasis / card titles / key values.
    pub const EMPHASIS: f32 = 16.0;
    /// Section headers within a page.
    pub const SECTION: f32 = 20.0;
    /// Page title (one per screen).
    pub const TITLE: f32 = 24.0;
}

/// Spacing scale (pixels) on an 8pt grid. Use for `padding` and `spacing`.
/// `XS` (4) is reserved for tight icon/label gaps; everything else is a multiple
/// of 8. `f32` so it feeds `.padding(..)`/`.spacing(..)` directly.
pub mod space {
    /// Tight gap (icon↔label). Use sparingly.
    pub const XS: f32 = 4.0;
    /// Default gap between related elements.
    pub const SM: f32 = 8.0;
    /// Gap between groups / card inner padding.
    pub const MD: f32 = 16.0;
    /// Gap between sections.
    pub const LG: f32 = 24.0;
    /// Page-level padding / large separations.
    pub const XL: f32 = 32.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_scale_is_monotonic() {
        assert!(font::CAPTION < font::BODY);
        assert!(font::BODY < font::EMPHASIS);
        assert!(font::EMPHASIS < font::SECTION);
        assert!(font::SECTION < font::TITLE);
    }

    #[test]
    fn spacing_is_on_8pt_grid() {
        // XS is the only sub-8 value; the rest are multiples of 8.
        for v in [space::SM, space::MD, space::LG, space::XL] {
            assert_eq!(v % 8.0, 0.0, "{v} is off the 8pt grid");
        }
        assert!(space::XS < space::SM);
    }
}

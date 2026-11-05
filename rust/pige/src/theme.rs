//! Shared Material 3 tokens: colours, spacing, radii, metrics and typography.
//!
//! Every colour, radius, spacing value and text style the UI draws comes from
//! [`Palette`]. Components read the active palette through [`active`] and never
//! branch on light/dark or invent their own tokens.
//!
//! Adapted from qlaunch's `theme.rs` (bitshift, MIT): the Material 3 dark
//! scheme and type scale are reused, with a light scheme, spacing scale, radii
//! and mail-specific metrics added for pige.

use std::cell::Cell;

use iced::font::Weight;
use iced::widget::{scrollable, text_input};
use iced::{Color, Font};

/// The palette every component draws with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Whether this is the dark scheme.
    pub dark: bool,
    /// The window/page colour behind panes.
    pub background: Color,
    /// The default pane colour.
    pub surface: Color,
    /// The lowest elevated container (navigation, list background).
    pub surface_container: Color,
    /// A raised container (search fields, bars).
    pub surface_container_high: Color,
    /// The most elevated container (hovered cards).
    pub surface_container_highest: Color,
    /// Primary text and icons.
    pub on_surface: Color,
    /// Secondary text and icons.
    pub on_surface_variant: Color,
    /// Strong outlines and icons.
    pub outline: Color,
    /// Hairlines and dividers.
    pub outline_variant: Color,
    /// Accent colour.
    pub primary: Color,
    /// Text and icons on `primary`.
    pub on_primary: Color,
    /// A soft accent container.
    pub primary_container: Color,
    /// Text and icons on `primary_container`.
    pub on_primary_container: Color,
    /// Selected rows and tonal surfaces.
    pub secondary_container: Color,
    /// Text and icons on `secondary_container`.
    pub on_secondary_container: Color,
    /// Destructive accent.
    pub error: Color,
    /// Text and icons on `error`.
    pub on_error: Color,
    /// A soft destructive container.
    pub error_container: Color,
    /// Text and icons on `error_container`.
    pub on_error_container: Color,
    /// Material's inverse surface. Retained as part of the scheme but no longer
    /// used by the message channel, which is now the shared toast.
    pub inverse_surface: Color,
    /// Text on [`Self::inverse_surface`].
    pub on_inverse_surface: Color,
    /// Dark text for a light filled surface (avatars, badges).
    pub on_light: Color,
    /// Light text for a dark filled surface (avatars, badges).
    pub on_dark: Color,
    /// The modal scrim.
    pub scrim: Color,
    /// The elevation shadow colour.
    pub shadow: Color,
    /// The spacing scale.
    pub space: Space,
    /// The corner-radius scale.
    pub radius: Radii,
    /// Interaction state-layer opacities.
    pub states: States,
    /// Fixed component metrics.
    pub metrics: Metrics,
    /// The font used for all chrome text.
    pub font: Font,
}

/// The spacing scale, in logical points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    pub xxs: f32,
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
    pub xxl: f32,
}

/// Interaction state-layer opacities.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct States {
    pub hover: f32,
    pub pressed: f32,
    pub disabled: f32,
    pub selection: f32,
    pub track: f32,
}

/// The corner-radius scale, in logical points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radii {
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
    pub full: f32,
}

/// Fixed geometry shared by the components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub app_bar_height: f32,
    pub list_row_height: f32,
    pub folder_row_height: f32,
    pub section_header_height: f32,
    pub field_height: f32,
    pub action_height: f32,
    pub touch_target: f32,
    pub icon_size: f32,
    pub small_icon_size: f32,
    pub nav_icon_size: f32,
    pub chip_height: f32,
    pub toast_max_width: f32,
    pub toast_radius: f32,
    pub toast_margin: f32,
    pub badge_height: f32,
    pub unread_dot: f32,
    pub avatar_size: f32,
    pub avatar_font_ratio: f32,
    pub empty_icon_size: f32,
    pub border_width: f32,
    pub focus_width: f32,
    pub scrollbar_width: f32,
    pub dialog_width: f32,
    pub dialog_padding: f32,
    pub compose_width: f32,
    pub compose_body_height: f32,
    pub dialog_shadow_blur: f32,
    pub dialog_shadow_y: f32,
    pub pull_threshold: f32,
    pub pull_limit: f32,
    pub pull_slop: f32,
    pub pull_indicator_width: f32,
    pub pull_indicator_height: f32,
}

/// The Material 3 text styles the UI draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    HeadlineSmall,
    TitleLarge,
    TitleMedium,
    TitleSmall,
    BodyLarge,
    BodyMedium,
    BodySmall,
    LabelLarge,
    LabelMedium,
    LabelSmall,
}

/// One resolved text style: size, line height and the font to shape it with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Typography {
    pub size: f32,
    pub line_height: f32,
    pub font: Font,
}

impl Typography {
    pub fn line_height(&self) -> iced::widget::text::LineHeight {
        iced::widget::text::LineHeight::Absolute(iced::Pixels(self.line_height))
    }
}

impl Palette {
    /// Resolve a Material 3 text role against this palette's font.
    pub fn role(&self, role: TextRole) -> Typography {
        let (size, line_height, weight) = match role {
            TextRole::HeadlineSmall => (24.0, 32.0, Weight::Normal),
            TextRole::TitleLarge => (22.0, 28.0, Weight::Normal),
            TextRole::TitleMedium => (16.0, 24.0, Weight::Medium),
            TextRole::TitleSmall => (14.0, 20.0, Weight::Medium),
            TextRole::BodyLarge => (16.0, 24.0, Weight::Normal),
            TextRole::BodyMedium => (14.0, 20.0, Weight::Normal),
            TextRole::BodySmall => (12.0, 16.0, Weight::Normal),
            TextRole::LabelLarge => (14.0, 20.0, Weight::Medium),
            TextRole::LabelMedium => (12.0, 16.0, Weight::Medium),
            TextRole::LabelSmall => (11.0, 16.0, Weight::Medium),
        };
        Typography {
            size,
            line_height,
            font: Font {
                weight,
                ..self.font
            },
        }
    }

    /// A readable text colour for the given background, using the palette's
    /// own light/dark text tokens.
    pub fn on_color(&self, background: Color) -> Color {
        let luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
        if luminance > 0.55 {
            Color::from_rgb8(0x1d, 0x1b, 0x20)
        } else {
            Color::from_rgb8(0xff, 0xff, 0xff)
        }
    }

    /// Apply the disabled state opacity to a colour.
    pub fn disabled(&self, color: Color) -> Color {
        Color {
            a: self.states.disabled,
            ..color
        }
    }

    /// The underline text-input style used by form and search fields.
    pub fn text_input(
        &self,
    ) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style + 'static {
        let palette = *self;
        move |_, _| text_input::Style {
            background: Color::TRANSPARENT.into(),
            border: iced::Border::default(),
            icon: palette.on_surface_variant,
            placeholder: palette.on_surface_variant,
            value: palette.on_surface,
            selection: Color {
                a: palette.states.selection,
                ..palette.primary
            },
        }
    }

    /// A scrollbar that stays out of the way until the user scrolls.
    pub fn scrollable(
        &self,
    ) -> impl Fn(&iced::Theme, scrollable::Status) -> scrollable::Style + 'static {
        let palette = *self;
        move |theme, status| {
            use iced::widget::scrollable::{Rail, Scroller};
            let mut style = scrollable::default(theme, status);
            let rail = Rail {
                background: None,
                border: iced::border::rounded(palette.metrics.scrollbar_width / 2.0),
                scroller: Scroller {
                    background: Color {
                        a: 0.55,
                        ..palette.on_surface
                    }
                    .into(),
                    border: iced::border::rounded(palette.metrics.scrollbar_width / 2.0),
                },
            };
            style.vertical_rail = rail;
            style.horizontal_rail = rail;
            style
        }
    }
}

/// The two complete Material 3 schemes.
const LIGHT: Palette = Palette {
    dark: false,
    background: Color::from_rgb8(0xf7, 0xf2, 0xfa),
    surface: Color::from_rgb8(0xfe, 0xf7, 0xff),
    surface_container: Color::from_rgb8(0xf3, 0xed, 0xf7),
    surface_container_high: Color::from_rgb8(0xec, 0xe6, 0xf0),
    surface_container_highest: Color::from_rgb8(0xe6, 0xe0, 0xe9),
    on_surface: Color::from_rgb8(0x1d, 0x1b, 0x20),
    on_surface_variant: Color::from_rgb8(0x49, 0x45, 0x4f),
    outline: Color::from_rgb8(0x79, 0x74, 0x7e),
    outline_variant: Color::from_rgb8(0xca, 0xc4, 0xd0),
    primary: Color::from_rgb8(0x67, 0x50, 0xa4),
    on_primary: Color::from_rgb8(0xff, 0xff, 0xff),
    primary_container: Color::from_rgb8(0xea, 0xdd, 0xff),
    on_primary_container: Color::from_rgb8(0x21, 0x00, 0x5d),
    secondary_container: Color::from_rgb8(0xe8, 0xde, 0xf8),
    on_secondary_container: Color::from_rgb8(0x1d, 0x19, 0x2b),
    error: Color::from_rgb8(0xb3, 0x26, 0x1e),
    on_error: Color::from_rgb8(0xff, 0xff, 0xff),
    error_container: Color::from_rgb8(0xf9, 0xde, 0xdc),
    on_error_container: Color::from_rgb8(0x41, 0x0e, 0x0b),
    inverse_surface: Color::from_rgb8(0x32, 0x2f, 0x35),
    on_inverse_surface: Color::from_rgb8(0xf5, 0xef, 0xf7),
    on_light: Color::from_rgb8(0x1d, 0x1b, 0x20),
    on_dark: Color::from_rgb8(0xff, 0xff, 0xff),
    scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.32),
    shadow: Color::from_rgba(0.0, 0.0, 0.0, 0.16),
    space: Space {
        xxs: 2.0,
        xs: 4.0,
        sm: 8.0,
        md: 12.0,
        lg: 16.0,
        xl: 24.0,
        xxl: 32.0,
    },
    radius: Radii {
        xs: 4.0,
        sm: 8.0,
        md: 12.0,
        lg: 16.0,
        xl: 28.0,
        full: 999.0,
    },
    states: STATES,
    metrics: METRICS,
    font: iced_android::DEFAULT_FONT,
};

const DARK: Palette = Palette {
    dark: true,
    background: Color::from_rgb8(0x14, 0x12, 0x18),
    surface: Color::from_rgb8(0x1d, 0x1b, 0x20),
    surface_container: Color::from_rgb8(0x21, 0x1f, 0x26),
    surface_container_high: Color::from_rgb8(0x2b, 0x29, 0x30),
    surface_container_highest: Color::from_rgb8(0x36, 0x34, 0x3b),
    on_surface: Color::from_rgb8(0xe6, 0xe0, 0xe9),
    on_surface_variant: Color::from_rgb8(0xca, 0xc4, 0xd0),
    outline: Color::from_rgb8(0x93, 0x8f, 0x99),
    outline_variant: Color::from_rgb8(0x49, 0x45, 0x4f),
    primary: Color::from_rgb8(0xd0, 0xbc, 0xff),
    on_primary: Color::from_rgb8(0x38, 0x1e, 0x72),
    primary_container: Color::from_rgb8(0x4f, 0x37, 0x8b),
    on_primary_container: Color::from_rgb8(0xea, 0xdd, 0xff),
    secondary_container: Color::from_rgb8(0x4a, 0x44, 0x58),
    on_secondary_container: Color::from_rgb8(0xe8, 0xde, 0xf8),
    error: Color::from_rgb8(0xf2, 0xb8, 0xb5),
    on_error: Color::from_rgb8(0x60, 0x14, 0x10),
    error_container: Color::from_rgb8(0x8c, 0x1d, 0x18),
    on_error_container: Color::from_rgb8(0xf9, 0xde, 0xdc),
    inverse_surface: Color::from_rgb8(0xe6, 0xe0, 0xe9),
    on_inverse_surface: Color::from_rgb8(0x32, 0x2f, 0x35),
    on_light: Color::from_rgb8(0x1d, 0x1b, 0x20),
    on_dark: Color::from_rgb8(0xff, 0xff, 0xff),
    scrim: Color::from_rgba(0.0, 0.0, 0.0, 0.54),
    shadow: Color::from_rgba(0.0, 0.0, 0.0, 0.30),
    space: Space {
        xxs: 2.0,
        xs: 4.0,
        sm: 8.0,
        md: 12.0,
        lg: 16.0,
        xl: 24.0,
        xxl: 32.0,
    },
    radius: Radii {
        xs: 4.0,
        sm: 8.0,
        md: 12.0,
        lg: 16.0,
        xl: 28.0,
        full: 999.0,
    },
    states: STATES,
    metrics: METRICS,
    font: iced_android::DEFAULT_FONT,
};

const STATES: States = States {
    hover: 0.08,
    pressed: 0.12,
    disabled: 0.38,
    selection: 0.35,
    track: 0.35,
};

const METRICS: Metrics = Metrics {
    app_bar_height: 64.0,
    list_row_height: 88.0,
    folder_row_height: 40.0,
    section_header_height: 44.0,
    field_height: 56.0,
    action_height: 40.0,
    touch_target: 48.0,
    icon_size: 24.0,
    small_icon_size: 16.0,
    nav_icon_size: 20.0,
    chip_height: 32.0,
    toast_max_width: 480.0,
    toast_radius: 8.0,
    toast_margin: 16.0,
    badge_height: 18.0,
    unread_dot: 8.0,
    avatar_size: 40.0,
    avatar_font_ratio: 0.4,
    empty_icon_size: 48.0,
    border_width: 1.0,
    focus_width: 2.0,
    scrollbar_width: 6.0,
    dialog_width: 480.0,
    dialog_padding: 24.0,
    compose_width: 640.0,
    compose_body_height: 220.0,
    dialog_shadow_blur: 16.0,
    dialog_shadow_y: 6.0,
    pull_threshold: 64.0,
    pull_limit: 96.0,
    pull_slop: 12.0,
    pull_indicator_width: 48.0,
    pull_indicator_height: 4.0,
};

/// Resolves either complete Material 3 scheme.
pub const fn palette(dark: bool) -> Palette {
    if dark { DARK } else { LIGHT }
}

thread_local! {
    /// The palette components draw with. The shell sets it once at the start of
    /// each view; tests and standalone builders get the dark default.
    static ACTIVE: Cell<Palette> = const { Cell::new(palette(true)) };
}

/// The palette components currently draw with.
pub fn active() -> Palette {
    ACTIVE.with(Cell::get)
}

/// Swaps the active palette. The shell calls this once per view; no widget is
/// reimplemented for light or dark.
pub fn set_active(palette: Palette) {
    ACTIVE.with(|active| active.set(palette));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_and_dark_palettes_are_complete_and_distinct() {
        let light = palette(false);
        let dark = palette(true);
        assert_ne!(light.surface, dark.surface);
        assert_ne!(light.on_surface, light.surface);
        assert_ne!(dark.on_surface, dark.surface);
    }

    #[test]
    fn spacing_scale_is_monotonic() {
        let p = palette(true);
        assert!(p.space.xs < p.space.sm && p.space.sm < p.space.md && p.space.md < p.space.lg);
    }

    #[test]
    fn both_schemes_carry_every_token() {
        for dark in [false, true] {
            let p = palette(dark);
            for token in [
                p.background,
                p.surface,
                p.surface_container,
                p.surface_container_high,
                p.surface_container_highest,
                p.on_surface,
                p.on_surface_variant,
                p.outline,
                p.outline_variant,
                p.primary,
                p.on_primary,
                p.primary_container,
                p.on_primary_container,
                p.secondary_container,
                p.on_secondary_container,
                p.error,
                p.on_error,
                p.error_container,
                p.on_error_container,
                p.inverse_surface,
                p.on_inverse_surface,
                p.on_light,
                p.on_dark,
                p.scrim,
                p.shadow,
            ] {
                assert!(!token.a.is_nan(), "token alpha must be finite");
            }
            assert!(p.radius.xs < p.radius.sm);
            assert!(p.radius.sm < p.radius.md);
            assert!(p.radius.md < p.radius.lg);
            assert!(p.radius.lg < p.radius.xl);
            assert!(p.metrics.app_bar_height > 0.0);
        }
    }

    #[test]
    fn chrome_type_scale_is_fixed_material_sizes() {
        let p = palette(true);
        let title = p.role(TextRole::TitleLarge);
        assert_eq!(title.size, 22.0);
        assert_eq!(title.line_height, 28.0);
        assert_eq!(title.font.weight, Weight::Normal);
        let label = p.role(TextRole::LabelLarge);
        assert_eq!(label.size, 14.0);
        assert_eq!(label.font.weight, Weight::Medium);
        assert_eq!(label.font.family, iced_android::DEFAULT_FONT.family);
    }

    #[test]
    fn contrast_text_is_readable_on_both_extremes() {
        let p = palette(true);
        assert_eq!(p.on_color(Color::WHITE), Color::from_rgb8(0x1d, 0x1b, 0x20));
        assert_eq!(p.on_color(Color::BLACK), Color::from_rgb8(0xff, 0xff, 0xff));
    }

    #[test]
    fn active_palette_round_trips() {
        let previous = active();
        set_active(palette(false));
        assert!(!active().dark);
        set_active(previous);
    }
}

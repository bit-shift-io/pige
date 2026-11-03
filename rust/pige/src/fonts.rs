//! Font registration for the shell.
//!
//! Chrome text follows the platform's own GUI font and system scale, as in
//! qlaunch's `fonts.rs` (bitshift, MIT). The launcher's user-selectable label
//! typefaces do not apply to a mail client, so this module only provides the
//! default font, the platform's bundled system fonts and the icon font.

use std::borrow::Cow;

use iced::Font;

/// The platform GUI font every piece of chrome text uses.
pub const DEFAULT_FONT: Font = iced_android::DEFAULT_FONT;

/// The bundled Material Icons family.
pub const ICON_FONT: Font = Font::with_name("Material Icons");

/// The Material Icons font, shipped alongside the crate.
pub const ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/MaterialIcons-Regular.otf");

/// Every font the shell must register: the platform's own fonts plus the
/// bundled icon font. Pass each entry to the iced daemon's `font` builder.
pub fn all() -> Vec<Cow<'static, [u8]>> {
    let mut fonts = iced_android::fonts();
    fonts.push(Cow::Borrowed(ICON_FONT_BYTES));
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_font_is_registered_last() {
        let fonts = all();
        assert_eq!(
            fonts.last().map(|font| font.as_ref()),
            Some(ICON_FONT_BYTES)
        );
    }

    #[test]
    fn the_bundled_font_is_an_open_type_file() {
        assert_eq!(&ICON_FONT_BYTES[..4], b"OTTO");
        assert_eq!(ICON_FONT.family, iced::font::Family::Name("Material Icons"));
    }
}

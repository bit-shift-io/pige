//! The bundled Material Icons font and the glyphs the UI draws.
//!
//! The font's cmap names each glyph `<icon>_baseline`; each constant below is
//! that glyph's codepoint (verified with `fontTools` against the bundled
//! `assets/MaterialIcons-Regular.otf`). Copied from qlaunch's `icons.rs`
//! (bitshift, MIT) and extended with the mail glyphs pige needs.

pub use crate::fonts::ICON_FONT as FONT;

pub const MENU: char = '\u{e3dc}';
pub const SEARCH: char = '\u{e567}';
pub const CLOSE: char = '\u{e16a}';
pub const ARROW_BACK: char = '\u{e092}';
pub const ARCHIVE: char = '\u{e091}';
pub const DELETE: char = '\u{e1b9}';
pub const DELETE_FOREVER: char = '\u{e1ba}';
pub const BACKUP: char = '\u{e0c6}';
pub const RESTORE: char = '\u{e534}';
pub const MARK_UNREAD: char = '\u{e3d0}';
pub const ATTACH_FILE: char = '\u{e0b1}';
pub const FOLDER: char = '\u{e2a3}';
pub const INBOX: char = '\u{e33a}';
pub const SEND: char = '\u{e571}';
pub const DRAFTS: char = '\u{e205}';
pub const OUTBOX: char = '\u{e462}';
pub const REPORT: char = '\u{e52a}';
pub const STAR: char = '\u{e5f9}';
pub const LABEL: char = '\u{e360}';
pub const ADD: char = '\u{e047}';
pub const ADD_CIRCLE: char = '\u{e04f}';
pub const SETTINGS: char = '\u{e57f}';
pub const MORE_VERT: char = '\u{e404}';
pub const REFRESH: char = '\u{e514}';
pub const CHECK: char = '\u{e156}';
pub const CHECK_CIRCLE: char = '\u{e159}';
pub const EXPAND_MORE: char = '\u{e246}';
pub const EXPAND_LESS: char = '\u{e245}';
pub const CHEVRON_RIGHT: char = '\u{e15f}';
pub const MAIL: char = '\u{e3c3}';
pub const EDIT: char = '\u{e21a}';
pub const REPLY: char = '\u{e528}';
pub const FORWARD: char = '\u{e2c4}';
pub const ALL_INBOX: char = '\u{e07d}';
pub const ACCOUNT_CIRCLE: char = '\u{e043}';
pub const PERSON_ADD: char = '\u{e492}';
pub const CREATE_NEW_FOLDER: char = '\u{e19e}';
pub const UNARCHIVE: char = '\u{e68b}';

/// The glyph as owned text, for `text(...).font(FONT)`.
pub fn glyph(character: char) -> String {
    character.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_codepoints_match_the_bundled_font() {
        assert_eq!(MENU, '\u{e3dc}');
        assert_eq!(SEARCH, '\u{e567}');
        assert_eq!(CLOSE, '\u{e16a}');
        assert_eq!(ARROW_BACK, '\u{e092}');
        assert_eq!(INBOX, '\u{e33a}');
        assert_eq!(SEND, '\u{e571}');
        assert_eq!(ATTACH_FILE, '\u{e0b1}');
        assert_eq!(EXPAND_MORE, '\u{e246}');
    }

    #[test]
    fn font_family_is_material_icons() {
        assert_eq!(FONT.family, iced::font::Family::Name("Material Icons"));
    }
}

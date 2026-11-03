//! Responsive layout geometry: breakpoints and the panes they show.
//!
//! Breakpoints are logical points, not physical pixels. The shell asks
//! [`panes`] which panes to draw; it never branches on window size itself.

/// The navigation rail's width, in logical points.
pub const NAV_WIDTH: f32 = 280.0;
/// The message list's width, in logical points.
pub const LIST_WIDTH: f32 = 360.0;

/// The narrower edge of the medium layout.
pub const MEDIUM_MIN: f32 = 840.0;
/// The narrower edge of the expanded layout.
pub const EXPANDED_MIN: f32 = 1200.0;

/// A responsive layout class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    /// One pane: the list; navigation is an overlay drawer.
    Compact,
    /// List and reader side by side; navigation is an overlay drawer.
    Medium,
    /// Navigation rail, list and reader in three panes.
    Expanded,
}

impl Breakpoint {
    /// Classify a logical window width.
    pub fn from_width(width: f32) -> Breakpoint {
        if width >= EXPANDED_MIN {
            Breakpoint::Expanded
        } else if width >= MEDIUM_MIN {
            Breakpoint::Medium
        } else {
            Breakpoint::Compact
        }
    }
}

/// Which panes are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub nav: bool,
    pub list: bool,
    pub reader: bool,
}

/// Resolve the visible panes for a breakpoint. On compact and medium the
/// navigation pane is only present while its drawer is open, and it overlays
/// the content rather than taking a column.
pub fn panes(bp: Breakpoint, drawer_open: bool) -> Panes {
    match bp {
        Breakpoint::Compact => Panes {
            nav: drawer_open,
            list: true,
            reader: false,
        },
        Breakpoint::Medium => Panes {
            nav: drawer_open,
            list: true,
            reader: true,
        },
        Breakpoint::Expanded => Panes {
            nav: true,
            list: true,
            reader: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_follow_logical_width() {
        assert_eq!(Breakpoint::from_width(839.0), Breakpoint::Compact);
        assert_eq!(Breakpoint::from_width(840.0), Breakpoint::Medium);
        assert_eq!(Breakpoint::from_width(1199.0), Breakpoint::Medium);
        assert_eq!(Breakpoint::from_width(1200.0), Breakpoint::Expanded);
    }

    #[test]
    fn panes_match_breakpoints() {
        assert_eq!(
            panes(Breakpoint::Compact, false),
            Panes {
                nav: false,
                list: true,
                reader: false
            }
        );
        assert_eq!(
            panes(Breakpoint::Compact, true),
            Panes {
                nav: true,
                list: true,
                reader: false
            }
        );
        assert_eq!(
            panes(Breakpoint::Medium, false),
            Panes {
                nav: false,
                list: true,
                reader: true
            }
        );
        assert_eq!(
            panes(Breakpoint::Expanded, false),
            Panes {
                nav: true,
                list: true,
                reader: true
            }
        );
    }

    #[test]
    fn edges_are_inclusive_on_the_wider_side() {
        assert_eq!(Breakpoint::from_width(0.0), Breakpoint::Compact);
        assert_eq!(Breakpoint::from_width(f32::NAN), Breakpoint::Compact);
        assert!(panes(Breakpoint::Medium, true).nav);
        assert!(panes(Breakpoint::Expanded, true).nav);
    }
}

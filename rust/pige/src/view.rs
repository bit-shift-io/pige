//! Composes the navigation, list and reader panes for one frame.

use iced::widget::{MouseArea, container, row, space, stack};
use iced::window;
use iced::{Alignment, Element, Fill};

use crate::app::App;
use crate::components::{self, PaneTone};
use crate::layout::{Breakpoint, LIST_WIDTH, NAV_WIDTH, Panes, panes_for};
use crate::message::Message;
use crate::theme;

impl App {
    pub(crate) fn view(&self, _window: window::Id) -> Element<'_, Message> {
        theme::set_active(theme::palette(self.dark));

        let breakpoint = Breakpoint::from_width(self.size.width);
        let reading = self.snapshot.selected_conversation.is_some();
        let panes = panes_for(breakpoint, self.drawer_open, reading);
        let base = if self.accounts_open {
            crate::screens::accounts::screen(self)
        } else if self.advanced_open {
            crate::screens::advanced::view(self)
        } else if self.settings_open {
            crate::screens::settings::screen(self)
        } else {
            self.content(breakpoint, panes)
        };
        let mut root: Element<'_, Message> = match &self.compose {
            Some(compose) if breakpoint == Breakpoint::Compact => {
                crate::screens::compose::screen(self, compose)
            }
            Some(compose) => stack![base, crate::screens::compose::modal(self, compose)].into(),
            None => base,
        };

        if self.toast.current.is_some() {
            root = stack![root, self.toast_overlay()].into();
        }
        if self.reader_menu_open {
            root = stack![
                root,
                crate::screens::reader::reader_menu(self, self.reader_pane_width(breakpoint))
            ]
            .into();
        }
        if self.move_picker_open {
            root = stack![root, crate::screens::reader::move_picker_modal(self)].into();
        }
        if self.delete_confirm_open {
            root = stack![root, crate::screens::reader::delete_confirm_modal()].into();
        }
        if let Some(names) = &self.restore_picker {
            root = stack![root, crate::screens::advanced::restore_picker_modal(names)].into();
        }
        if let Some(name) = &self.restore_confirm {
            root = stack![root, crate::screens::advanced::restore_confirm_modal(name)].into();
        }
        if self.confirm_reset {
            root = stack![root, crate::screens::advanced::reset_confirm_modal()].into();
        }
        if panes.nav && breakpoint != Breakpoint::Expanded {
            root = stack![root, self.drawer()].into();
        }

        // Android draws the app edge-to-edge, so inset the whole shell
        // (including drawers and overlays) by the system-bar and keyboard
        // insets. On desktop these insets are zero.
        container(root)
            .width(Fill)
            .height(Fill)
            .padding(self.insets)
            .into()
    }

    fn content(&self, breakpoint: Breakpoint, panes: Panes) -> Element<'_, Message> {
        let mut layout = row![].width(Fill).height(Fill);

        if panes.nav && breakpoint == Breakpoint::Expanded {
            layout = layout.push(crate::screens::navigation::navigation(self));
            layout = layout.push(components::divider());
        }
        if panes.list {
            layout = layout.push(crate::screens::message_list::list_pane(
                self,
                breakpoint,
                panes.reader,
            ));
        }
        if panes.reader {
            // A divider only separates panes that are shown together; on compact
            // the reader replaces the list and sits on its own.
            if panes.list {
                layout = layout.push(components::divider());
            }
            layout = layout.push(crate::screens::reader::reader_pane(
                self,
                self.reader_pane_width(breakpoint),
            ));
        }

        // Compose lives in the app bar on every width; a floating button would
        // cover the newest message and duplicate that action.
        container(layout)
            .width(Fill)
            .height(Fill)
            .style(components::pane_style(PaneTone::Page))
            .into()
    }

    /// The reader pane's width in logical points for a breakpoint, from the
    /// window size and the fixed pane widths. Used both to lay the pane out and
    /// to decide how many toolbar actions fit before the overflow.
    fn reader_pane_width(&self, breakpoint: Breakpoint) -> f32 {
        let content = (self.size.width - self.insets.left - self.insets.right).max(0.0);
        let border = theme::active().metrics.border_width;
        match breakpoint {
            Breakpoint::Compact => content,
            Breakpoint::Medium => (content - LIST_WIDTH - border).max(0.0),
            Breakpoint::Expanded => (content - NAV_WIDTH - border - LIST_WIDTH - border).max(0.0),
        }
    }

    fn drawer(&self) -> Element<'_, Message> {
        let scrim = MouseArea::new(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(components::scrim_style()),
        )
        .on_press(Message::CloseDrawer);

        row![crate::screens::navigation::navigation(self), scrim]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The shared toast: a small dark card centred at the bottom, over the
    /// current screen and inside the system-bar insets.
    fn toast_overlay(&self) -> Element<'_, Message> {
        let palette = theme::active();
        let Some((_, message)) = &self.toast.current else {
            return space().into();
        };
        let margin = palette.metrics.toast_margin;
        let insets = self.insets;
        container(components::toast(message))
            .width(Fill)
            .height(Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::End)
            .padding(iced::Padding {
                top: 0.0,
                right: insets.right + margin,
                bottom: insets.bottom + margin,
                left: insets.left + margin,
            })
            .into()
    }
}

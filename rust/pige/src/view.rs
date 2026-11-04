//! Composes the navigation, list and reader panes for one frame.

use iced::widget::{MouseArea, container, row, space, stack};
use iced::window;
use iced::{Alignment, Element, Fill};

use crate::app::App;
use crate::components::{self, PaneTone};
use crate::icons;
use crate::layout::{Breakpoint, Panes, panes_for};
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

        if self.error.is_some() {
            root = stack![root, self.error_overlay()].into();
        }
        if self.move_picker_open {
            root = stack![root, crate::screens::reader::move_picker_modal(self)].into();
        }
        if self.delete_confirm_open {
            root = stack![root, crate::screens::reader::delete_confirm_modal()].into();
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
            layout = layout.push(self.reader_pane());
        }

        let base: Element<'_, Message> = container(layout)
            .width(Fill)
            .height(Fill)
            .style(components::pane_style(PaneTone::Page))
            .into();

        // The spec asks for a floating compose button on compact layouts; wide
        // layouts keep the toolbar compose button instead.
        if breakpoint == Breakpoint::Compact && panes.list {
            let palette = theme::active();
            let fab = container(components::floating_action_button(
                icons::EDIT,
                "Compose",
                Message::Compose,
            ))
            .width(Fill)
            .height(Fill)
            .padding(palette.space.lg)
            .align_x(Alignment::End)
            .align_y(Alignment::End);
            return stack![base, fab].into();
        }
        base
    }

    fn reader_pane(&self) -> Element<'_, Message> {
        crate::screens::reader::reader_pane(self)
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

    fn error_overlay(&self) -> Element<'_, Message> {
        let palette = theme::active();
        let Some(error) = &self.error else {
            return space().into();
        };

        let bar = row![
            components::snackbar(error),
            components::icon_button(icons::CLOSE, "Dismiss", Message::DismissError),
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center)
        .width(Fill);

        container(bar)
            .width(Fill)
            .height(Fill)
            .padding(palette.space.lg)
            .align_y(Alignment::End)
            .into()
    }
}

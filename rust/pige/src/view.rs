//! Composes the navigation, list and reader panes for one frame.

use iced::widget::{MouseArea, container, row, space, stack};
use iced::window;
use iced::{Alignment, Element, Fill};

use crate::app::App;
use crate::components::{self, PaneTone};
use crate::icons;
use crate::layout::{Breakpoint, Panes, panes};
use crate::message::Message;
use crate::theme;

impl App {
    pub(crate) fn view(&self, _window: window::Id) -> Element<'_, Message> {
        theme::set_active(theme::palette(self.dark));

        let breakpoint = Breakpoint::from_width(self.size.width);
        let panes = panes(breakpoint, self.drawer_open);
        let mut root = if self.accounts_open {
            crate::screens::accounts::screen(self)
        } else {
            self.content(breakpoint, panes)
        };

        if self.error.is_some() {
            root = stack![root, self.error_overlay()].into();
        }
        if panes.nav && breakpoint != Breakpoint::Expanded {
            root = stack![root, self.drawer()].into();
        }
        root
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
            layout = layout.push(components::divider());
            layout = layout.push(self.reader_pane());
        }

        container(layout)
            .width(Fill)
            .height(Fill)
            .style(components::pane_style(PaneTone::Page))
            .into()
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

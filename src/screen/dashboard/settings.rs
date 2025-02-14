use iced::widget::{button, center, column, container, row, scrollable, text, Scrollable};
use iced::{Length, Task, Vector};

use crate::widget::{Element, Text};
use crate::window::{self, Window};
use crate::{icon, theme};

#[derive(Debug, Clone)]
pub enum Message {
    Open(Section),
}

#[derive(Debug, Clone)]
pub enum Event {}

#[derive(Debug, Clone)]
pub struct Settings {
    pub window: window::Id,
}

#[derive(Debug, Clone)]
enum Section {
    Buffer,
    ScaleFactor,
}

impl Section {
    fn list() -> Vec<Self> {
        vec![Section::Buffer, Section::ScaleFactor]
    }
}

impl Settings {
    pub fn open(main_window: &Window) -> (Self, Task<window::Id>) {
        let (window, task) = window::open(window::Settings {
            size: iced::Size::new(470.0, 300.0),
            resizable: true,
            position: main_window
                .position
                .map(|point| window::Position::Specific(point + Vector::new(20.0, 20.0)))
                .unwrap_or_default(),
            exit_on_close_request: false,
            ..window::settings()
        });

        (Self { window }, task)
    }

    pub fn update(&mut self, message: Message) -> Option<Event> {
        match message {
            Message::Open(section) => None,
        }
    }

    pub fn view<'a>(&self) -> Element<'a, Message> {
        container(row![sidebar::view(), content::view()])
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(8)
            .into()
    }
}

mod sidebar {
    use iced::{
        padding,
        widget::{button, container, scrollable, text, Column, Scrollable},
        Length,
    };

    use super::{Message, Section};

    use crate::{appearance::theme, widget::Element};

    pub fn view<'a>() -> Element<'a, Message> {
        let sections = Section::list()
            .into_iter()
            .map(|section| {
                button(text("This is a long section"))
                    .width(Length::Fill)
                    .on_press(Message::Open(section))
                    .padding(8)
                    .style(move |theme, status| {
                        theme::button::sidebar_buffer(theme, status, false, false)
                    })
                    .into()
            })
            .collect::<Vec<_>>();

        container(
            Scrollable::new(Column::with_children(sections).spacing(1)).direction(
                scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::default()
                        .width(0)
                        .scroller_width(0),
                ),
            ),
        )
        .width(125)
        .padding(padding::right(6).top(1))
        .into()
    }
}

mod content {
    use iced::{
        padding,
        widget::{button, container, scrollable, text, Column, Scrollable},
        Length,
    };

    use super::{Message, Section};

    use crate::{appearance::theme, widget::Element};

    pub fn view<'a>() -> Element<'a, Message> {
        container(
            Scrollable::new(text("all content")).direction(scrollable::Direction::Vertical(
                iced::widget::scrollable::Scrollbar::default()
                    .width(0)
                    .scroller_width(0),
            )),
        )
        .style(|theme| theme::container::buffer(theme, false))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .into()
    }
}

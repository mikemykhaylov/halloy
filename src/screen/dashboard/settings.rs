
use iced::widget::{button, center, column, container, scrollable, text, Scrollable};
use iced::{Length, Task, Vector};

use crate::widget::{Element, Text};
use crate::{icon, theme};
use crate::window::{self, Window};


#[derive(Debug, Clone)]
pub enum Message {
}

#[derive(Debug, Clone)]
pub enum Event {
}


#[derive(Debug, Clone)]
pub struct Settings {
    pub window: window::Id,
}

impl Settings {
    pub fn open(main_window: &Window) -> (Self, Task<window::Id>) {
        let (window, task) = window::open(window::Settings {
            size: iced::Size::new(470.0, 300.0),
            resizable: false,
            position: main_window
                .position
                .map(|point| window::Position::Specific(point + Vector::new(20.0, 20.0)))
                .unwrap_or_default(),
            exit_on_close_request: false,
            ..window::settings()
        });

        (
            Self {
                window,
            },
            task,
        )
    }

    pub fn update(&mut self, message: Message) -> Option<Event> {
        None
    }

    pub fn view<'a>(&self) -> Element<'a, Message> {
        container(
            Scrollable::new(text("settings"))
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(1).scroller_width(1),
                ))
                .style(theme::scrollable::hidden),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}



pub use data::stream::{self, *};
use data::{server, Config};
use iced::Subscription;

pub fn run(entry: server::Entry, config: Config) -> Subscription<stream::Update> {
    Subscription::run_with_id(entry.server.clone(), stream::run(entry, config))
}

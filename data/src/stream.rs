use chrono::{DateTime, Utc};
use futures::never::Never;
use std::collections::BTreeMap;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{future, stream, FutureExt, SinkExt, StreamExt};
use irc::proto::{self, command, Command};
use irc::{codec, connection, Connection};
use tokio::time::{self, Instant, Interval};

use crate::client::{self, Client};
use crate::server::Server;
use crate::time::Posix;
use crate::{config, server, Config, Message};

pub type Result<T = Update, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Connection(connection::Error),
}

#[derive(Debug)]
pub enum Update {
    Connected {
        server: Server,
        client: client::Handle,
        is_initial: bool,
        sent_time: DateTime<Utc>,
    },
    Disconnected {
        server: Server,
        is_initial: bool,
        error: Option<String>,
        sent_time: DateTime<Utc>,
    },
    ConnectionFailed {
        server: Server,
        error: String,
        sent_time: DateTime<Utc>,
    },
    Batch {
        server: Server,
        events: Vec<client::Event<Message>>,
        chanmap: BTreeMap<String, client::Channel>,
    },
    Quit(Server, Option<String>),
}

enum State {
    Disconnected {
        last_retry: Option<Instant>,
    },
    Connected {
        client: Client,
        stream: Stream,
        batch: Batch,
        ping_time: Interval,
        ping_timeout: Option<Interval>,
        tick: time::Interval,
    },
    Quit,
}

enum Input {
    IrcMessage(Result<codec::ParseResult, codec::Error>),
    Batch(Vec<client::Event>),
    Client(client::Request),
    Ping,
    PingTimeout,
    Tick(Instant),
}

struct Stream {
    connection: Connection<irc::Codec>,
    receiver: mpsc::Receiver<client::Request>,
}

pub fn run(server: server::Entry, config: Config) -> impl futures::Stream<Item = Update> {
    let (sender, receiver) = mpsc::unbounded();

    let runner = stream::once(async {
        // Spawn to unblock backend from iced stream which
        // has backpressure
        tokio::spawn(_run(server, config, sender));
        future::pending().await
    });

    stream::select(receiver, runner)
}

async fn _run(
    server: server::Entry,
    mut config: Config,
    sender: mpsc::UnboundedSender<Update>,
) -> Never {
    let server::Entry {
        server,
        config: server_config,
    } = server;

    let reconnect_delay = Duration::from_secs(server_config.reconnect_delay);

    let mut is_initial = true;
    let mut state = State::Disconnected { last_retry: None };

    // Notify app of initial disconnected state
    let _ = sender.unbounded_send(Update::Disconnected {
        server: server.clone(),
        is_initial,
        error: None,
        sent_time: Utc::now(),
    });

    loop {
        match &mut state {
            State::Disconnected { last_retry } => {
                if let Some(last_retry) = last_retry.as_ref() {
                    let remaining = reconnect_delay.saturating_sub(last_retry.elapsed());

                    if !remaining.is_zero() {
                        time::sleep(remaining).await;
                    }
                }

                match connect(server.clone(), server_config.clone(), config.proxy.clone()).await {
                    Ok((stream, client, handle)) => {
                        log::info!("[{server}] connected");

                        let _ = sender.unbounded_send(Update::Connected {
                            server: server.clone(),
                            client: handle,
                            is_initial,
                            sent_time: Utc::now(),
                        });

                        is_initial = false;

                        state = State::Connected {
                            client,
                            stream,
                            batch: Batch::new(),
                            ping_timeout: None,
                            ping_time: ping_time_interval(server_config.ping_time),
                            tick: time::interval(Duration::from_secs(1)),
                        };
                    }
                    Err(e) => {
                        let error = match e {
                            // unwrap Tls-specific error enums to access more error info
                            connection::Error::Tls(e) => format!("a TLS error occured: {e}"),
                            _ => e.to_string(),
                        };

                        log::warn!("[{server}] connection failed: {error}");

                        let _ = sender.unbounded_send(Update::ConnectionFailed {
                            server: server.clone(),
                            error,
                            sent_time: Utc::now(),
                        });

                        *last_retry = Some(Instant::now());
                    }
                }
            }
            State::Connected {
                stream,
                client,
                batch,
                ping_time,
                ping_timeout,
                tick,
            } => {
                let input = {
                    let mut select = stream::select_all([
                        (&mut stream.connection).map(Input::IrcMessage).boxed(),
                        (&mut stream.receiver).map(Input::Client).boxed(),
                        ping_time.tick().into_stream().map(|_| Input::Ping).boxed(),
                        batch.map(Input::Batch).boxed(),
                        tick.tick().into_stream().map(Input::Tick).boxed(),
                    ]);

                    if let Some(timeout) = ping_timeout.as_mut() {
                        select.push(
                            timeout
                                .tick()
                                .into_stream()
                                .map(|_| Input::PingTimeout)
                                .boxed(),
                        );
                    }

                    select.next().await.expect("stream input")
                };

                match input {
                    Input::IrcMessage(Ok(Ok(message))) => match message.command {
                        proto::Command::PING(token) => {
                            let _ = stream.connection.send(command!("PONG", token)).await;
                        }
                        proto::Command::PONG(_, token) => {
                            let token = token.unwrap_or_default();
                            log::trace!("[{server}] pong received: {token}");

                            *ping_timeout = None;
                        }
                        proto::Command::ERROR(error) => {
                            log::warn!("[{server}] disconnected: {error}");
                            let _ = sender.unbounded_send(Update::Disconnected {
                                server: server.clone(),
                                is_initial,
                                error: Some(error),
                                sent_time: Utc::now(),
                            });
                            state = State::Disconnected {
                                last_retry: Some(Instant::now()),
                            };
                        }
                        _ => {
                            batch.messages.extend(
                                client.receive(&mut stream.connection, message.into()).await,
                            );
                        }
                    },
                    Input::IrcMessage(Ok(Err(e))) => {
                        log::warn!("message decoding failed: {e}");
                    }
                    Input::IrcMessage(Err(e)) => {
                        log::warn!("[{server}] disconnected: {e}");
                        let _ = sender.unbounded_send(Update::Disconnected {
                            server: server.clone(),
                            is_initial,
                            error: Some(e.to_string()),
                            sent_time: Utc::now(),
                        });
                        state = State::Disconnected {
                            last_retry: Some(Instant::now()),
                        };
                    }
                    Input::Batch(events) => {
                        let events = events
                            .into_iter()
                            .filter_map(|event| event.decode(client, &config))
                            .collect();

                        let _ = sender.unbounded_send(Update::Batch {
                            server: server.clone(),
                            events,
                            chanmap: client.chanmap.clone(),
                        });
                    }
                    Input::Client(client::Request::Send(message, buffer)) => {
                        if let Command::QUIT(reason) = &message.command {
                            let reason = reason.clone();

                            let _ = stream.connection.send(message).await;
                            let _ = sender.unbounded_send(Update::Quit(server.clone(), reason));

                            log::info!("[{server}] quit");

                            state = State::Quit;
                        } else {
                            client.send(&mut stream.connection, message, buffer).await;
                        }
                    }
                    Input::Client(client::Request::ConfigUpdated(updated)) => {
                        config = updated;
                        log::debug!("[{server}] config updated");
                    }
                    Input::Ping => {
                        let now = Posix::now().as_nanos().to_string();
                        log::trace!("[{server}] ping sent: {now}");

                        let _ = stream.connection.send(command!("PING", now)).await;

                        if ping_timeout.is_none() {
                            *ping_timeout = Some(ping_timeout_interval(server_config.ping_timeout));
                        }
                    }
                    Input::PingTimeout => {
                        log::warn!("[{server}] ping timeout");
                        let _ = sender.unbounded_send(Update::Disconnected {
                            server: server.clone(),
                            is_initial,
                            error: Some("ping timeout".into()),
                            sent_time: Utc::now(),
                        });
                        state = State::Disconnected {
                            last_retry: Some(Instant::now()),
                        };
                    }
                    Input::Tick(now) => {
                        client.tick(&mut stream.connection, now.into_std()).await;
                    }
                }
            }
            State::Quit => {
                // Wait forever until this stream is dropped by the frontend
                future::pending::<()>().await;
            }
        }
    }
}

async fn connect(
    server: Server,
    config: config::Server,
    proxy: Option<config::Proxy>,
) -> Result<(Stream, Client, client::Handle), connection::Error> {
    let mut connection = Connection::new(config.connection(proxy), irc::Codec).await?;

    let (sender, receiver) = mpsc::channel(100);

    let (client, connected) =
        Client::new(&mut connection, server, config, client::Sender::new(sender)).await;

    Ok((
        Stream {
            connection,
            receiver,
        },
        client,
        connected,
    ))
}

struct Batch {
    interval: Interval,
    messages: Vec<client::Event>,
}

impl Batch {
    const INTERVAL_MILLIS: u64 = 50;

    fn new() -> Self {
        Self {
            interval: time::interval_at(
                Instant::now() + Duration::from_millis(Self::INTERVAL_MILLIS),
                Duration::from_millis(Self::INTERVAL_MILLIS),
            ),
            messages: vec![],
        }
    }
}

impl futures::Stream for Batch {
    type Item = Vec<client::Event>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let batch = self.get_mut();

        match batch.interval.poll_tick(cx) {
            std::task::Poll::Ready(_) => {
                let messages = std::mem::take(&mut batch.messages);

                if messages.is_empty() {
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(Some(messages))
                }
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

fn ping_time_interval(secs: u64) -> Interval {
    time::interval_at(
        Instant::now() + Duration::from_secs(secs),
        Duration::from_secs(secs),
    )
}

fn ping_timeout_interval(secs: u64) -> Interval {
    time::interval_at(
        Instant::now() + Duration::from_secs(secs),
        Duration::from_secs(secs),
    )
}

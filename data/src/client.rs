use chrono::{DateTime, Utc};
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt, SinkExt};
use irc::proto::{self, command, Command};
use itertools::{Either, Itertools};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

use crate::file_transfer;
use crate::message::server_time;
use crate::time::Posix;
use crate::user::{Nick, NickRef};
use crate::{config, ctcp, dcc, isupport, message, mode, Buffer, Config, Message, Server, User};

const HIGHLIGHT_BLACKOUT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub enum Status {
    Unavailable,
    Connected,
    Disconnected,
}

impl Status {
    pub fn connected(&self) -> bool {
        matches!(self, Status::Connected)
    }
}

#[derive(Debug)]
pub enum State {
    Disconnected,
    Ready(Handle),
}

#[derive(Debug)]
pub enum Notification {
    DirectMessage(User),
    Highlight(User, String),
    MonitoredOnline(Vec<User>),
    MonitoredOffline(Vec<Nick>),
}

#[derive(Debug)]
pub enum Broadcast {
    Quit {
        user: User,
        comment: Option<String>,
        channels: Vec<String>,
        sent_time: DateTime<Utc>,
    },
    Nickname {
        old_user: User,
        new_nick: Nick,
        ourself: bool,
        channels: Vec<String>,
        sent_time: DateTime<Utc>,
    },
    Invite {
        inviter: User,
        channel: String,
        user_channels: Vec<String>,
        sent_time: DateTime<Utc>,
    },
    ChangeHost {
        old_user: User,
        new_username: String,
        new_hostname: String,
        ourself: bool,
        channels: Vec<String>,
        sent_time: DateTime<Utc>,
    },
}

#[derive(Debug)]
pub enum Event<T = message::Encoded> {
    Single(T),
    WithTarget(T, message::Target),
    Broadcast(Broadcast),
    Notification(T, Notification),
    FileTransferRequest(file_transfer::ReceiveRequest),
    NickUpdated(String),
    IsupportUpdated(HashMap<isupport::Kind, isupport::Parameter>),
}

impl Event<message::Encoded> {
    pub fn decode(self, client: &Client, config: &Config) -> Option<Event<Message>> {
        let received = |message| {
            Message::received(
                message,
                client.nickname(),
                config,
                |user: &User, channel: &str| client.resolve_user_attributes(channel, user).cloned(),
            )
        };

        match self {
            Event::Single(message) => received(message).map(Event::Single),
            Event::WithTarget(message, target) => {
                received(message).map(|message| Event::WithTarget(message, target))
            }
            Event::Broadcast(broadcast) => Some(Event::Broadcast(broadcast)),
            Event::Notification(message, notification) => {
                received(message).map(|message| Event::Notification(message, notification))
            }
            Event::FileTransferRequest(request) => Some(Event::FileTransferRequest(request)),
            Event::NickUpdated(nick) => Some(Event::NickUpdated(nick)),
            Event::IsupportUpdated(isupport) => Some(Event::IsupportUpdated(isupport)),
        }
    }
}

#[derive(Debug)]
pub enum Request {
    Send(proto::Message, Option<Buffer>),
    ConfigUpdated(Config),
}

#[derive(Debug, Clone)]
pub struct Sender(mpsc::Sender<Request>);

impl Sender {
    pub fn new(sender: mpsc::Sender<Request>) -> Self {
        Self(sender)
    }

    pub async fn send(&mut self, message: proto::Message) -> Result<(), mpsc::SendError> {
        self.0.send(Request::Send(message, None)).await
    }

    #[allow(clippy::result_large_err)]
    pub fn try_send(&mut self, message: proto::Message) -> Result<(), mpsc::TrySendError<Request>> {
        self.0.try_send(Request::Send(message, None))
    }

    #[allow(clippy::result_large_err)]
    pub fn try_send_with_buffer(
        &mut self,
        message: proto::Message,
        buffer: Buffer,
    ) -> Result<(), mpsc::TrySendError<Request>> {
        self.0.try_send(Request::Send(message, Some(buffer)))
    }

    pub fn config_updated(&mut self, config: Config) -> Result<(), mpsc::TrySendError<Request>> {
        self.0.try_send(Request::ConfigUpdated(config))
    }
}

#[derive(Debug)]
pub struct Handle {
    sender: Sender,
    config: config::Server,
    resolved_nick: Option<String>,
    chanmap: BTreeMap<String, Channel>,
    channels: Vec<String>,
    users: HashMap<String, Vec<User>>,
    isupport: HashMap<isupport::Kind, isupport::Parameter>,
}

impl Handle {
    pub fn new(sender: Sender, config: config::Server) -> Self {
        Self {
            sender,
            config,
            resolved_nick: None,
            chanmap: BTreeMap::default(),
            channels: vec![],
            users: HashMap::default(),
            isupport: HashMap::default(),
        }
    }

    pub fn update_chanmap(&mut self, chanmap: BTreeMap<String, Channel>) {
        self.chanmap = chanmap;
        self.channels = self.chanmap.keys().cloned().collect();
        self.users = self
            .chanmap
            .iter()
            .map(|(channel, state)| {
                (
                    channel.clone(),
                    state.users.iter().sorted().cloned().collect(),
                )
            })
            .collect();
    }

    pub fn update_nick(&mut self, nick: String) {
        self.resolved_nick = Some(nick);
    }

    pub fn update_isupport(&mut self, isupport: HashMap<isupport::Kind, isupport::Parameter>) {
        self.isupport = isupport;
    }

    pub fn config_updated(&mut self, config: Config) {
        let _ = self.sender.config_updated(config);
    }

    fn quit(&mut self, reason: Option<String>) {
        if let Err(e) = if let Some(reason) = reason {
            self.sender.try_send(command!("QUIT", reason))
        } else {
            self.sender.try_send(command!("QUIT"))
        } {
            log::warn!("Error sending quit: {e}");
        }
    }

    fn join(&mut self, channels: &[String]) {
        let keys = HashMap::new();

        let messages = group_joins(channels, &keys);

        for message in messages {
            if let Err(e) = self.sender.try_send(message) {
                log::warn!("Error sending join: {e}");
            }
        }
    }

    fn send(&mut self, buffer: &Buffer, message: message::Encoded) {
        if let Err(e) = self
            .sender
            .try_send_with_buffer(message.into(), buffer.clone())
        {
            log::warn!("Error sending message: {e}");
        }
    }

    fn resolve_user_attributes<'a>(&'a self, channel: &str, user: &User) -> Option<&'a User> {
        self.chanmap
            .get(channel)
            .and_then(|channel| channel.users.get(user))
    }

    fn user_channels<'a>(&'a self, nick: NickRef) -> Vec<&'a str> {
        self.chanmap
            .iter()
            .filter_map(|(channel, state)| {
                state
                    .users
                    .iter()
                    .any(|user| user.nickname() == nick)
                    .then_some(channel.as_str())
            })
            .collect()
    }

    fn topic<'a>(&'a self, channel: &str) -> Option<&'a Topic> {
        self.chanmap.get(channel).map(|channel| &channel.topic)
    }

    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    pub fn users<'a>(&'a self, channel: &str) -> &'a [User] {
        self.users
            .get(channel)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn nickname(&self) -> NickRef {
        NickRef::from(
            self.resolved_nick
                .as_deref()
                .unwrap_or(&self.config.nickname),
        )
    }
}

pub struct Client {
    server: Server,
    server_config: config::Server,
    sender: Sender,
    alt_nick: Option<usize>,
    pub resolved_nick: Option<String>,
    pub chanmap: BTreeMap<String, Channel>,
    labels: HashMap<String, Context>,
    batches: HashMap<String, Batch>,
    reroute_responses_to: Option<Buffer>,
    registration_step: RegistrationStep,
    listed_caps: Vec<String>,
    supports_labels: bool,
    supports_away_notify: bool,
    supports_account_notify: bool,
    supports_extended_join: bool,
    highlight_blackout: HighlightBlackout,
    registration_required_channels: Vec<String>,
    isupport: HashMap<isupport::Kind, isupport::Parameter>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish()
    }
}

impl Client {
    pub async fn new(
        connection: &mut irc::Connection<irc::Codec>,
        server: Server,
        server_config: config::Server,
        sender: Sender,
    ) -> (Self, Handle) {
        // Begin registration
        let _ = connection.send(command!("CAP", "LS", "302")).await;
        let registration_step = RegistrationStep::List;

        // Identify
        {
            let nick = &server_config.nickname;
            let user = server_config.username.as_ref().unwrap_or(nick);
            let real = server_config.realname.as_ref().unwrap_or(nick);

            if let Some(pass) = server_config.password.as_ref() {
                let _ = connection.send(command!("PASS", pass)).await;
            }
            let _ = connection.send(command!("NICK", nick)).await;
            let _ = connection.send(command!("USER", user, real)).await;
        }

        let handle = Handle::new(sender.clone(), server_config.clone());

        (
            Self {
                server,
                server_config,
                sender,
                resolved_nick: None,
                alt_nick: None,
                chanmap: BTreeMap::default(),
                labels: HashMap::new(),
                batches: HashMap::new(),
                reroute_responses_to: None,
                registration_step,
                listed_caps: vec![],
                supports_labels: false,
                supports_away_notify: false,
                supports_account_notify: false,
                supports_extended_join: false,
                highlight_blackout: HighlightBlackout::Blackout(Instant::now()),
                registration_required_channels: vec![],
                isupport: HashMap::new(),
            },
            handle,
        )
    }

    pub async fn send(
        &mut self,
        connection: &mut irc::Connection<irc::Codec>,
        mut message: proto::Message,
        buffer: Option<Buffer>,
    ) {
        if let Some(buffer) = buffer {
            if self.supports_labels {
                use proto::Tag;

                let label = generate_label();
                let context = Context::new(&message, buffer.clone());

                self.labels.insert(label.clone(), context);

                // IRC: Encode tags
                message.tags = vec![Tag {
                    key: "label".to_string(),
                    value: Some(label),
                }];
            }

            self.reroute_responses_to = start_reroute(&message.command).then_some(buffer);
        }

        let _ = connection.send(message).await;
    }

    pub async fn receive(
        &mut self,
        connection: &mut irc::Connection<irc::Codec>,
        message: message::Encoded,
    ) -> Vec<Event> {
        log::trace!("Message received => {:?}", *message);

        let stop_reroute = stop_reroute(&message.command);

        let events = self
            .handle(connection, message, None)
            .await
            .unwrap_or_default();

        if stop_reroute {
            self.reroute_responses_to = None;
        }

        events
    }

    fn handle<'a>(
        &'a mut self,
        connection: &'a mut irc::Connection<irc::Codec>,
        mut message: message::Encoded,
        parent_context: Option<Context>,
    ) -> BoxFuture<'a, Option<Vec<Event>>> {
        use irc::proto::command::Numeric::*;

        async {
            let label_tag = remove_tag("label", message.tags.as_mut());
            let batch_tag = remove_tag("batch", message.tags.as_mut());

            let context = parent_context.or_else(|| {
                label_tag
                    // Remove context associated to label if we get resp for it
                    .and_then(|label| self.labels.remove(&label))
                    // Otherwise if we're in a batch, get it's context
                    .or_else(|| {
                        batch_tag.as_ref().and_then(|batch| {
                            self.batches
                                .get(batch)
                                .and_then(|batch| batch.context.clone())
                        })
                    })
            });

            match &message.command {
                Command::BATCH(batch, ..) => {
                    let mut chars = batch.chars();
                    let symbol = chars.next()?;
                    let reference = chars.collect::<String>();

                    match symbol {
                        '+' => {
                            let batch = Batch::new(context);
                            self.batches.insert(reference, batch);
                        }
                        '-' => {
                            if let Some(finished) = self.batches.remove(&reference) {
                                // If nested, extend events into parent batch
                                if let Some(parent) = batch_tag
                                    .as_ref()
                                    .and_then(|batch| self.batches.get_mut(batch))
                                {
                                    parent.events.extend(finished.events);
                                } else {
                                    return Some(finished.events);
                                }
                            }
                        }
                        _ => {}
                    }

                    return None;
                }
                _ if batch_tag.is_some() => {
                    let events = self.handle(connection, message, context).await?;

                    if let Some(batch) = self.batches.get_mut(&batch_tag.unwrap()) {
                        batch.events.extend(events);
                        return None;
                    } else {
                        return Some(events);
                    }
                }
                // Label context whois
                _ if context.as_ref().map(Context::is_whois).unwrap_or_default() => {
                    if let Some(source) = context
                        .map(Context::buffer)
                        .map(|buffer| buffer.server_message_target(None))
                    {
                        return Some(vec![Event::WithTarget(message, source)]);
                    }
                }
                // Reroute responses
                Command::Numeric(..) | Command::Unknown(..)
                    if self.reroute_responses_to.is_some() =>
                {
                    if let Some(source) = self
                        .reroute_responses_to
                        .clone()
                        .map(|buffer| buffer.server_message_target(None))
                    {
                        return Some(vec![Event::WithTarget(message, source)]);
                    }
                }
                Command::CAP(_, sub, a, b) if sub == "LS" => {
                    let (caps, asterisk) = match (a, b) {
                        (Some(caps), None) => (caps, None),
                        (Some(asterisk), Some(caps)) => (caps, Some(asterisk)),
                        // Unreachable
                        (None, None) | (None, Some(_)) => return None,
                    };

                    self.listed_caps.extend(caps.split(' ').map(String::from));

                    // Finished
                    if asterisk.is_none() {
                        let mut requested = vec![];

                        let contains = |s| self.listed_caps.iter().any(|cap| cap == s);

                        if contains("invite-notify") {
                            requested.push("invite-notify");
                        }
                        if contains("userhost-in-names") {
                            requested.push("userhost-in-names");
                        }
                        if contains("away-notify") {
                            requested.push("away-notify");
                        }
                        if contains("message-tags") {
                            requested.push("message-tags");
                        }
                        if contains("server-time") {
                            requested.push("server-time");
                        }
                        if contains("chghost") {
                            requested.push("chghost");
                        }
                        if contains("extended-monitor") {
                            requested.push("extended-monitor");
                        }
                        if contains("account-notify") {
                            requested.push("account-notify");

                            if contains("extended-join") {
                                requested.push("extended-join");
                            }
                        }
                        if contains("batch") {
                            requested.push("batch");
                        }
                        if contains("labeled-response") {
                            requested.push("labeled-response");

                            // We require labeled-response so we can properly tag echo-messages
                            if contains("echo-message") {
                                requested.push("echo-message");
                            }
                        }
                        if self.listed_caps.iter().any(|cap| cap.starts_with("sasl")) {
                            requested.push("sasl");
                        }
                        if contains("multi-prefix") {
                            requested.push("multi-prefix");
                        }

                        if !requested.is_empty() {
                            // Request
                            self.registration_step = RegistrationStep::Req;
                            let _ = connection
                                .send(command!("CAP", "REQ", requested.join(" ")))
                                .await;
                        } else {
                            // If none requested, end negotiation
                            self.registration_step = RegistrationStep::End;
                            let _ = connection.send(command!("CAP", "END")).await;
                        }
                    }
                }
                Command::CAP(_, sub, a, b) if sub == "ACK" => {
                    let caps = if b.is_none() { a.as_ref() } else { b.as_ref() }?;
                    log::info!("[{}] capabilities acknowledged: {caps}", self.server);

                    let caps = caps.split(' ').collect::<Vec<_>>();

                    if caps.contains(&"labeled-response") {
                        self.supports_labels = true;
                    }
                    if caps.contains(&"away-notify") {
                        self.supports_away_notify = true;
                    }
                    if caps.contains(&"account-notify") {
                        self.supports_account_notify = true;
                    }
                    if caps.contains(&"extended-join") {
                        self.supports_extended_join = true;
                    }

                    let supports_sasl = caps.iter().any(|cap| cap.contains("sasl"));

                    if let Some(sasl) = self.server_config.sasl.as_ref().filter(|_| supports_sasl) {
                        self.registration_step = RegistrationStep::Sasl;
                        let _ = connection
                            .send(command!("AUTHENTICATE", sasl.command()))
                            .await;
                    } else {
                        self.registration_step = RegistrationStep::End;
                        let _ = connection.send(command!("CAP", "END")).await;
                    }
                }
                Command::CAP(_, sub, a, b) if sub == "NAK" => {
                    let caps = if b.is_none() { a.as_ref() } else { b.as_ref() }?;
                    log::warn!("[{}] capabilities not acknowledged: {caps}", self.server);

                    // End we didn't move to sasl or already ended
                    if self.registration_step < RegistrationStep::Sasl {
                        self.registration_step = RegistrationStep::End;
                        let _ = connection.send(command!("CAP", "END")).await;
                    }
                }
                Command::CAP(_, sub, a, b) if sub == "NEW" => {
                    let caps = if b.is_none() { a.as_ref() } else { b.as_ref() }?;

                    let new_caps = caps.split(' ').map(String::from).collect::<Vec<String>>();

                    let mut requested = vec![];

                    let newly_contains = |s| new_caps.iter().any(|cap| cap == s);

                    let contains = |s| self.listed_caps.iter().any(|cap| cap == s);

                    if newly_contains("invite-notify") {
                        requested.push("invite-notify");
                    }
                    if newly_contains("userhost-in-names") {
                        requested.push("userhost-in-names");
                    }
                    if newly_contains("away-notify") {
                        requested.push("away-notify");
                    }
                    if newly_contains("message-tags") {
                        requested.push("message-tags");
                    }
                    if newly_contains("server-time") {
                        requested.push("server-time");
                    }
                    if newly_contains("chghost") {
                        requested.push("chghost");
                    }
                    if newly_contains("extended-monitor") {
                        requested.push("extended-monitor");
                    }
                    if contains("account-notify") || newly_contains("account-notify") {
                        if newly_contains("account-notify") {
                            requested.push("account-notify");
                        }

                        if newly_contains("extended-join") {
                            requested.push("extended-join");
                        }
                    }
                    if newly_contains("batch") {
                        requested.push("batch");
                    }
                    if contains("labeled-response") || newly_contains("labeled-response") {
                        if newly_contains("labeled-response") {
                            requested.push("labeled-response");
                        }

                        // We require labeled-response so we can properly tag echo-messages
                        if newly_contains("echo-message") {
                            requested.push("echo-message");
                        }
                    }
                    if newly_contains("multi-prefix") {
                        requested.push("multi-prefix");
                    }

                    if !requested.is_empty() {
                        // Request
                        let _ = connection
                            .send(command!("CAP", "REQ", requested.join(" ")))
                            .await;
                    }

                    self.listed_caps.extend(new_caps);
                }
                Command::CAP(_, sub, a, b) if sub == "DEL" => {
                    let caps = if b.is_none() { a.as_ref() } else { b.as_ref() }?;

                    let del_caps = caps.split(' ').collect::<Vec<_>>();

                    if del_caps.contains(&"labeled-response") {
                        self.supports_labels = false;
                    }
                    if del_caps.contains(&"away-notify") {
                        self.supports_away_notify = false;
                    }
                    if del_caps.contains(&"account-notify") {
                        self.supports_account_notify = false;
                    }
                    if del_caps.contains(&"extended-join") {
                        self.supports_extended_join = false;
                    }

                    self.listed_caps
                        .retain(|cap| !del_caps.iter().any(|del_cap| del_cap == cap));
                }
                Command::AUTHENTICATE(param) if param == "+" => {
                    if let Some(sasl) = self.server_config.sasl.as_ref() {
                        log::info!("[{}] sasl auth: {}", self.server, sasl.command());

                        let _ = connection
                            .send(command!("AUTHENTICATE", sasl.param()))
                            .await;
                        self.registration_step = RegistrationStep::End;
                        let _ = connection.send(command!("CAP", "END")).await;
                    }
                }
                Command::Numeric(RPL_LOGGEDIN, args) => {
                    log::info!("[{}] logged in", self.server);

                    if !self.registration_required_channels.is_empty() {
                        for message in group_joins(
                            &self.registration_required_channels,
                            &self.server_config.channel_keys,
                        ) {
                            let _ = connection.send(message).await;
                        }

                        self.registration_required_channels.clear();
                    }

                    if !self.supports_account_notify {
                        let accountname = args.first()?;

                        let old_user = User::from(self.nickname().to_owned());

                        self.chanmap.values_mut().for_each(|channel| {
                            if let Some(user) = channel.users.take(&old_user) {
                                channel.users.insert(user.with_accountname(accountname));
                            }
                        });
                    }
                }
                Command::Numeric(RPL_LOGGEDOUT, _) => {
                    log::info!("[{}] logged out", self.server);

                    if !self.supports_account_notify {
                        let old_user = User::from(self.nickname().to_owned());

                        self.chanmap.values_mut().for_each(|channel| {
                            if let Some(user) = channel.users.take(&old_user) {
                                channel.users.insert(user.with_accountname("*"));
                            }
                        });
                    }
                }
                Command::PRIVMSG(channel, text) | Command::NOTICE(channel, text) => {
                    if let Some(user) = message.user() {
                        if let Some(command) = dcc::decode(text) {
                            match command {
                                dcc::Command::Send(request) => {
                                    log::trace!("DCC Send => {request:?}");
                                    return Some(vec![Event::FileTransferRequest(
                                        file_transfer::ReceiveRequest {
                                            from: user.nickname().to_owned(),
                                            dcc_send: request,
                                            server: self.server.clone(),
                                            sender: self.sender.clone(),
                                        },
                                    )]);
                                }
                                dcc::Command::Unsupported(command) => {
                                    log::debug!("Unsupported DCC command: {command}",);
                                    return None;
                                }
                            }
                        } else {
                            // Handle CTCP queries except ACTION and DCC
                            if user.nickname() != self.nickname()
                                && ctcp::is_query(text)
                                && !message::is_action(text)
                            {
                                if let Some(query) = ctcp::parse_query(text) {
                                    if matches!(&message.command, Command::PRIVMSG(_, _)) {
                                        match query.command {
                                            ctcp::Command::Action => (),
                                            ctcp::Command::ClientInfo => {
                                                let _ = connection
                                                .send(ctcp::response_message(
                                                    &query.command,
                                                    user.nickname().to_string(),
                                                    Some(
                                                        "ACTION CLIENTINFO DCC PING SOURCE VERSION",
                                                    ),
                                                ))
                                                .await;
                                            }
                                            ctcp::Command::DCC => (),
                                            ctcp::Command::Ping => {
                                                let _ = connection
                                                    .send(ctcp::response_message(
                                                        &query.command,
                                                        user.nickname().to_string(),
                                                        query.params,
                                                    ))
                                                    .await;
                                            }
                                            ctcp::Command::Source => {
                                                let _ = connection
                                                    .send(ctcp::response_message(
                                                        &query.command,
                                                        user.nickname().to_string(),
                                                        Some(crate::environment::SOURCE_WEBSITE),
                                                    ))
                                                    .await;
                                            }
                                            ctcp::Command::Version => {
                                                let _ = connection
                                                    .send(ctcp::response_message(
                                                        &query.command,
                                                        user.nickname().to_string(),
                                                        Some(format!(
                                                            "Halloy {}",
                                                            crate::environment::VERSION
                                                        )),
                                                    ))
                                                    .await;
                                            }
                                            ctcp::Command::Unknown(command) => {
                                                log::debug!(
                                                "Ignorning CTCP command {command}: Unknown command"
                                            )
                                            }
                                        }
                                    }

                                    return None;
                                }
                            }

                            // Highlight notification
                            if message::reference_user_text(user.nickname(), self.nickname(), text)
                                && self.highlight_blackout.allow_highlights()
                            {
                                return Some(vec![Event::Notification(
                                    message.clone(),
                                    Notification::Highlight(user, channel.clone()),
                                )]);
                            } else if user.nickname() == self.nickname() && context.is_some() {
                                // If we sent (echo) & context exists (we sent from this client), ignore
                                return None;
                            }

                            // use `channel` to confirm the direct message, then send notification
                            if channel == &self.nickname().to_string() {
                                return Some(vec![Event::Notification(
                                    message.clone(),
                                    Notification::DirectMessage(user),
                                )]);
                            }
                        }
                    }
                }
                Command::INVITE(user, channel) => {
                    let user = User::from(Nick::from(user.as_str()));
                    let inviter = message.user()?;
                    let user_channels = self.user_channels(user.nickname());

                    return Some(vec![Event::Broadcast(Broadcast::Invite {
                        inviter,
                        channel: channel.clone(),
                        user_channels,
                        sent_time: server_time(&message),
                    })]);
                }
                Command::NICK(nick) => {
                    let old_user = message.user()?;
                    let ourself = self.nickname() == old_user.nickname();

                    let mut events = vec![];

                    if ourself {
                        self.resolved_nick = Some(nick.clone());
                        events.push(Event::NickUpdated(nick.clone()));
                    }

                    let new_nick = Nick::from(nick.as_str());

                    self.chanmap.values_mut().for_each(|channel| {
                        if let Some(user) = channel.users.take(&old_user) {
                            channel.users.insert(user.with_nickname(new_nick.clone()));
                        }
                    });

                    let channels = self.user_channels(old_user.nickname());

                    events.push(Event::Broadcast(Broadcast::Nickname {
                        old_user,
                        new_nick,
                        ourself,
                        channels,
                        sent_time: server_time(&message),
                    }));

                    return Some(events);
                }
                Command::Numeric(ERR_NICKNAMEINUSE | ERR_ERRONEUSNICKNAME, _)
                    if self.resolved_nick.is_none() =>
                {
                    // Try alt nicks
                    match &mut self.alt_nick {
                        Some(index) => {
                            if *index == self.server_config.alt_nicks.len() - 1 {
                                self.alt_nick = None;
                            } else {
                                *index += 1;
                            }
                        }
                        None if !self.server_config.alt_nicks.is_empty() => self.alt_nick = Some(0),
                        None => {}
                    }

                    if let Some(nick) = self
                        .alt_nick
                        .and_then(|i| self.server_config.alt_nicks.get(i))
                    {
                        let _ = connection.send(command!("NICK", nick)).await;
                    }
                }
                Command::Numeric(RPL_WELCOME, args) => {
                    let mut events = vec![];

                    // Updated actual nick
                    let nick = args.first()?;
                    self.resolved_nick = Some(nick.to_string());
                    events.push(Event::NickUpdated(nick.to_string()));

                    // Send nick password & ghost
                    if let Some(nick_pass) = self.server_config.nick_password.as_ref() {
                        // Try ghost recovery if we couldn't claim our nick
                        if self.server_config.should_ghost && nick != &self.server_config.nickname {
                            for sequence in &self.server_config.ghost_sequence {
                                let _ = connection
                                    .send(command!(
                                        "PRIVMSG",
                                        "NickServ",
                                        format!(
                                            "{sequence} {} {nick_pass}",
                                            &self.server_config.nickname
                                        )
                                    ))
                                    .await;
                            }
                        }

                        if let Some(identify_syntax) = &self.server_config.nick_identify_syntax {
                            match identify_syntax {
                                config::server::IdentifySyntax::PasswordNick => {
                                    let _ = connection
                                        .send(command!(
                                            "PRIVMSG",
                                            "NickServ",
                                            format!(
                                                "IDENTIFY {nick_pass} {}",
                                                &self.server_config.nickname
                                            )
                                        ))
                                        .await;
                                }
                                config::server::IdentifySyntax::NickPassword => {
                                    let _ = connection
                                        .send(command!(
                                            "PRIVMSG",
                                            "NickServ",
                                            format!(
                                                "IDENTIFY {} {nick_pass}",
                                                &self.server_config.nickname
                                            )
                                        ))
                                        .await;
                                }
                            }
                        } else if self.resolved_nick == Some(self.server_config.nickname.clone()) {
                            // Use nickname-less identification if possible, since it has
                            // no possible argument order issues.
                            let _ = connection
                                .send(command!(
                                    "PRIVMSG",
                                    "NickServ",
                                    format!("IDENTIFY {nick_pass}")
                                ))
                                .await;
                        } else {
                            // Default to most common syntax if unknown
                            let _ = connection
                                .send(command!(
                                    "PRIVMSG",
                                    "NickServ",
                                    format!(
                                        "IDENTIFY {} {nick_pass}",
                                        &self.server_config.nickname
                                    )
                                ))
                                .await;
                        }
                    }

                    // Send user modestring
                    if let Some(modestring) = self.server_config.umodes.as_ref() {
                        let _ = connection.send(command!("MODE", nick, modestring)).await;
                    }

                    // Loop on connect commands
                    for command in self.server_config.on_connect.iter() {
                        if let Ok(cmd) = crate::command::parse(command, None) {
                            if let Ok(command) = proto::Command::try_from(cmd) {
                                let _ = connection.send(command.into()).await;
                            };
                        };
                    }

                    // Send JOIN
                    for message in group_joins(
                        &self.server_config.channels,
                        &self.server_config.channel_keys,
                    ) {
                        let _ = connection.send(message).await;
                    }

                    events.push(Event::Single(message));
                    return Some(events);
                }
                // QUIT
                Command::QUIT(comment) => {
                    let user = message.user()?;

                    self.chanmap.values_mut().for_each(|channel| {
                        channel.users.remove(&user);
                    });

                    let channels = self.user_channels(user.nickname());

                    return Some(vec![Event::Broadcast(Broadcast::Quit {
                        user,
                        comment: comment.clone(),
                        channels,
                        sent_time: server_time(&message),
                    })]);
                }
                Command::PART(channel, _) => {
                    let user = message.user()?;

                    if user.nickname() == self.nickname() {
                        self.chanmap.remove(channel);
                    } else if let Some(channel) = self.chanmap.get_mut(channel) {
                        channel.users.remove(&user);
                    }
                }
                Command::JOIN(channel, accountname) => {
                    let user = message.user()?;

                    if user.nickname() == self.nickname() {
                        self.chanmap.insert(channel.clone(), Channel::default());

                        if let Some(state) = self.chanmap.get_mut(channel) {
                            // Sends WHO to get away state on users.
                            if self.isupport.contains_key(&isupport::Kind::WHOX) {
                                let fields = if self.supports_account_notify {
                                    "tcnfa"
                                } else {
                                    "tcnf"
                                };

                                let _ = connection
                                    .send(command!(
                                        "WHO",
                                        channel,
                                        fields,
                                        isupport::WHO_POLL_TOKEN.to_owned()
                                    ))
                                    .await;

                                state.last_who = Some(WhoStatus::Requested(
                                    Instant::now(),
                                    Some(isupport::WHO_POLL_TOKEN),
                                ));
                            } else {
                                let _ = connection.send(command!("WHO", channel)).await;
                                state.last_who = Some(WhoStatus::Requested(Instant::now(), None));
                            }
                            log::debug!("[{}] {channel} - WHO requested", self.server);
                        }
                    } else if let Some(channel) = self.chanmap.get_mut(channel) {
                        let user = if self.supports_extended_join {
                            accountname.as_ref().map_or(user.clone(), |accountname| {
                                user.with_accountname(accountname)
                            })
                        } else {
                            user
                        };

                        channel.users.insert(user);
                    }
                }
                Command::KICK(channel, victim, _) => {
                    if victim == self.nickname().as_ref() {
                        self.chanmap.remove(channel);
                    } else if let Some(channel) = self.chanmap.get_mut(channel) {
                        channel
                            .users
                            .remove(&User::from(Nick::from(victim.as_str())));
                    }
                }
                Command::Numeric(RPL_WHOREPLY, args) => {
                    let target = args.get(1)?;

                    if proto::is_channel(target) {
                        if let Some(channel) = self.chanmap.get_mut(target) {
                            channel.update_user_away(args.get(5)?, args.get(6)?);

                            if matches!(
                                channel.last_who,
                                Some(WhoStatus::Requested(_, None)) | None
                            ) {
                                channel.last_who = Some(WhoStatus::Receiving(None));
                                log::debug!("[{}] {target} - WHO receiving...", self.server);
                            }

                            if matches!(channel.last_who, Some(WhoStatus::Receiving(_))) {
                                // We requested, don't save to history
                                return None;
                            }
                        }
                    }
                }
                Command::Numeric(RPL_WHOSPCRPL, args) => {
                    let target = args.get(2)?;

                    if proto::is_channel(target) {
                        if let Some(channel) = self.chanmap.get_mut(target) {
                            channel.update_user_away(args.get(3)?, args.get(4)?);

                            if self.supports_account_notify {
                                if let (Some(user), Some(accountname)) = (args.get(3), args.get(5))
                                {
                                    channel.update_user_accountname(user, accountname);
                                }
                            }

                            if let Ok(token) = args.get(1)?.parse::<isupport::WhoToken>() {
                                if let Some(WhoStatus::Requested(_, Some(request_token))) =
                                    channel.last_who
                                {
                                    if request_token == token {
                                        channel.last_who =
                                            Some(WhoStatus::Receiving(Some(request_token)));
                                        log::debug!(
                                            "[{}] {target} - WHO receiving...",
                                            self.server
                                        );
                                    }
                                }
                            }

                            if matches!(channel.last_who, Some(WhoStatus::Receiving(_))) {
                                // We requested, don't save to history
                                return None;
                            }
                        }
                    }
                }
                Command::Numeric(RPL_ENDOFWHO, args) => {
                    let target = args.get(1)?;

                    if proto::is_channel(target) {
                        if let Some(channel) = self.chanmap.get_mut(target) {
                            if matches!(channel.last_who, Some(WhoStatus::Receiving(_))) {
                                channel.last_who = Some(WhoStatus::Done(Instant::now()));
                                log::debug!("[{}] {target} - WHO done", self.server);
                                return None;
                            }
                        }
                    }
                }
                Command::AWAY(args) => {
                    let away = args.is_some();
                    let user = message.user()?;

                    for channel in self.chanmap.values_mut() {
                        if let Some(mut user) = channel.users.take(&user) {
                            user.update_away(away);
                            channel.users.insert(user);
                        }
                    }
                }
                Command::Numeric(RPL_UNAWAY, args) => {
                    let nick = args.first()?.as_str();
                    let user = User::try_from(nick).ok()?;

                    if user.nickname() == self.nickname() {
                        for channel in self.chanmap.values_mut() {
                            if let Some(mut user) = channel.users.take(&user) {
                                user.update_away(false);
                                channel.users.insert(user);
                            }
                        }
                    }
                }
                Command::Numeric(RPL_NOWAWAY, args) => {
                    let nick = args.first()?.as_str();
                    let user = User::try_from(nick).ok()?;

                    if user.nickname() == self.nickname() {
                        for channel in self.chanmap.values_mut() {
                            if let Some(mut user) = channel.users.take(&user) {
                                user.update_away(true);
                                channel.users.insert(user);
                            }
                        }
                    }
                }
                Command::MODE(target, Some(modes), Some(args)) => {
                    if proto::is_channel(target) {
                        let modes = mode::parse::<mode::Channel>(modes, args);

                        if let Some(channel) = self.chanmap.get_mut(target) {
                            for mode in modes {
                                if let Some((op, lookup)) = mode
                                    .operation()
                                    .zip(mode.arg().map(|nick| User::from(Nick::from(nick))))
                                {
                                    if let Some(mut user) = channel.users.take(&lookup) {
                                        user.update_access_level(op, *mode.value());
                                        channel.users.insert(user);
                                    }
                                }
                            }
                        }
                    } else {
                        // Only check for being logged in via mode if account-notify is not available,
                        // since it is not standardized across networks.

                        if target == self.nickname().as_ref()
                            && !self.supports_account_notify
                            && !self.registration_required_channels.is_empty()
                        {
                            let modes = mode::parse::<mode::User>(modes, args);

                            if modes.into_iter().any(|mode| {
                                matches!(mode, mode::Mode::Add(mode::User::Registered, None))
                            }) {
                                for message in group_joins(
                                    &self.registration_required_channels,
                                    &self.server_config.channel_keys,
                                ) {
                                    let _ = connection.send(message).await;
                                }

                                self.registration_required_channels.clear();
                            }
                        }
                    }
                }
                Command::Numeric(RPL_NAMREPLY, args) if args.len() > 3 => {
                    if let Some(channel) = self.chanmap.get_mut(&args[2]) {
                        for user in args[3].split(' ') {
                            if let Ok(user) = User::try_from(user) {
                                channel.users.insert(user);
                            }
                        }

                        // Don't save to history if names list was triggered by JOIN
                        if !channel.names_init {
                            return None;
                        }
                    }
                }
                Command::Numeric(RPL_ENDOFNAMES, args) => {
                    let target = args.get(1)?;

                    if proto::is_channel(target) {
                        if let Some(channel) = self.chanmap.get_mut(target) {
                            if !channel.names_init {
                                channel.names_init = true;

                                return None;
                            }
                        }
                    }
                }
                Command::TOPIC(channel, topic) => {
                    if let Some(channel) = self.chanmap.get_mut(channel) {
                        if let Some(text) = topic {
                            channel.topic.content = Some(message::parse_fragments(text.clone()));
                        }

                        channel.topic.who = message
                            .user()
                            .map(|user| user.username().unwrap().to_string());
                        channel.topic.time = Some(server_time(&message));
                    }
                }
                Command::Numeric(RPL_TOPIC, args) => {
                    if let Some(channel) = self.chanmap.get_mut(&args[1]) {
                        channel.topic.content =
                            Some(message::parse_fragments(args.get(2)?.to_owned()));
                    }
                    // Exclude topic message from history to prevent spam during dev
                    #[cfg(feature = "dev")]
                    return None;
                }
                Command::Numeric(RPL_TOPICWHOTIME, args) => {
                    if let Some(channel) = self.chanmap.get_mut(&args[1]) {
                        channel.topic.who = Some(args.get(2)?.to_string());
                        channel.topic.time = Some(
                            args.get(3)?
                                .parse::<u64>()
                                .ok()
                                .map(Posix::from_seconds)?
                                .datetime()?,
                        );
                    }
                    // Exclude topic message from history to prevent spam during dev
                    #[cfg(feature = "dev")]
                    return None;
                }
                Command::Numeric(ERR_NOCHANMODES, args) => {
                    let channel = args.get(1)?;

                    // If the channel has not been joined but is in the configured channels,
                    // then interpret this numeric as ERR_NEEDREGGEDNICK (which has the
                    // same number as ERR_NOCHANMODES)
                    if !self.chanmap.contains_key(channel)
                        && self
                            .server_config
                            .channels
                            .iter()
                            .any(|config_channel| config_channel == channel)
                    {
                        self.registration_required_channels.push(channel.clone());
                    }
                }
                Command::Numeric(RPL_ISUPPORT, args) => {
                    let args_len = args.len();
                    for (index, arg) in args.iter().enumerate().skip(1) {
                        let operation = arg.parse::<isupport::Operation>();

                        match operation {
                            Ok(operation) => {
                                match operation {
                                    isupport::Operation::Add(parameter) => {
                                        if let Some(kind) = parameter.kind() {
                                            log::info!(
                                                "[{}] adding ISUPPORT parameter: {:?}",
                                                self.server,
                                                parameter
                                            );

                                            if let isupport::Parameter::MONITOR(target_limit) =
                                                &parameter
                                            {
                                                let messages = group_monitors(
                                                    &self.server_config.monitor,
                                                    *target_limit,
                                                );

                                                for message in messages {
                                                    let _ = connection.send(message).await;
                                                }
                                            }

                                            self.isupport.insert(kind, parameter.clone());

                                            return Some(vec![Event::IsupportUpdated(
                                                self.isupport.clone(),
                                            )]);
                                        } else {
                                            log::debug!(
                                                "[{}] ignoring ISUPPORT parameter: {:?}",
                                                self.server,
                                                parameter
                                            );
                                        }
                                    }
                                    isupport::Operation::Remove(_) => {
                                        if let Some(kind) = operation.kind() {
                                            log::info!(
                                                "[{}] removing ISUPPORT parameter: {:?}",
                                                self.server,
                                                kind
                                            );

                                            self.isupport.remove(&kind);

                                            return Some(vec![Event::IsupportUpdated(
                                                self.isupport.clone(),
                                            )]);
                                        }
                                    }
                                };
                            }
                            Err(error) => {
                                if index != args_len - 1 {
                                    log::debug!(
                                        "[{}] unable to parse ISUPPORT parameter: {} ({})",
                                        self.server,
                                        arg,
                                        error
                                    )
                                }
                            }
                        }
                    }

                    return None;
                }
                Command::TAGMSG(_) => {
                    return None;
                }
                Command::ACCOUNT(accountname) => {
                    let old_user = message.user()?;

                    self.chanmap.values_mut().for_each(|channel| {
                        if let Some(user) = channel.users.take(&old_user) {
                            channel.users.insert(user.with_accountname(accountname));
                        }
                    });

                    if old_user.nickname() == self.nickname()
                        && accountname != "*"
                        && !self.registration_required_channels.is_empty()
                    {
                        for message in group_joins(
                            &self.registration_required_channels,
                            &self.server_config.channel_keys,
                        ) {
                            let _ = connection.send(message).await;
                        }

                        self.registration_required_channels.clear();
                    }
                }
                Command::CHGHOST(new_username, new_hostname) => {
                    let old_user = message.user()?;

                    let ourself = old_user.nickname() == self.nickname();

                    self.chanmap.values_mut().for_each(|channel| {
                        if let Some(user) = channel.users.take(&old_user) {
                            channel.users.insert(user.with_username_and_hostname(
                                new_username.clone(),
                                new_hostname.clone(),
                            ));
                        }
                    });

                    let channels = self.user_channels(old_user.nickname());

                    return Some(vec![Event::Broadcast(Broadcast::ChangeHost {
                        old_user,
                        new_username: new_username.clone(),
                        new_hostname: new_hostname.clone(),
                        ourself,
                        channels,
                        sent_time: server_time(&message),
                    })]);
                }
                Command::Numeric(RPL_MONONLINE, args) => {
                    let targets = args
                        .get(1)?
                        .split(',')
                        .filter_map(|target| User::try_from(target).ok())
                        .collect::<Vec<_>>();

                    return Some(vec![Event::Notification(
                        message.clone(),
                        Notification::MonitoredOnline(targets),
                    )]);
                }
                Command::Numeric(RPL_MONOFFLINE, args) => {
                    let targets = args.get(1)?.split(',').map(Nick::from).collect::<Vec<_>>();

                    return Some(vec![Event::Notification(
                        message.clone(),
                        Notification::MonitoredOffline(targets),
                    )]);
                }
                Command::Numeric(RPL_ENDOFMONLIST, _) => {
                    return None;
                }
                _ => {}
            }

            Some(vec![Event::Single(message)])
        }
        .boxed()
    }

    fn user_channels(&self, nick: NickRef) -> Vec<String> {
        self.chanmap
            .iter()
            .filter_map(|(channel, state)| {
                state
                    .users
                    .iter()
                    .any(|user| user.nickname() == nick)
                    .then_some(channel)
            })
            .cloned()
            .collect()
    }

    fn resolve_user_attributes<'a>(&'a self, channel: &str, user: &User) -> Option<&'a User> {
        self.chanmap
            .get(channel)
            .and_then(|channel| channel.users.get(user))
    }

    fn nickname(&self) -> NickRef {
        NickRef::from(
            self.resolved_nick
                .as_deref()
                .unwrap_or(&self.server_config.nickname),
        )
    }

    pub async fn tick(&mut self, connection: &mut irc::Connection<irc::Codec>, now: Instant) {
        match self.highlight_blackout {
            HighlightBlackout::Blackout(instant) => {
                if now.duration_since(instant) >= HIGHLIGHT_BLACKOUT_INTERVAL {
                    self.highlight_blackout = HighlightBlackout::Receiving;
                }
            }
            HighlightBlackout::Receiving => {}
        }

        for (channel, state) in self.chanmap.iter_mut() {
            enum Request {
                Poll,
                Retry,
            }

            let request = match state.last_who {
                Some(WhoStatus::Done(last)) if !self.supports_away_notify => {
                    (now.duration_since(last) >= self.server_config.who_poll_interval)
                        .then_some(Request::Poll)
                }
                Some(WhoStatus::Requested(requested, _)) => (now.duration_since(requested)
                    >= self.server_config.who_retry_interval)
                    .then_some(Request::Retry),
                _ => None,
            };

            if let Some(request) = request {
                if self.isupport.contains_key(&isupport::Kind::WHOX) {
                    let fields = if self.supports_account_notify {
                        "tcnfa"
                    } else {
                        "tcnf"
                    };

                    let _ = connection
                        .send(command!(
                            "WHO",
                            channel,
                            fields,
                            isupport::WHO_POLL_TOKEN.to_owned()
                        ))
                        .await;

                    state.last_who = Some(WhoStatus::Requested(
                        Instant::now(),
                        Some(isupport::WHO_POLL_TOKEN),
                    ));
                } else {
                    let _ = connection.send(command!("WHO", channel)).await;
                    state.last_who = Some(WhoStatus::Requested(Instant::now(), None));
                }
                log::debug!(
                    "[{}] {channel} - WHO {}",
                    self.server,
                    match request {
                        Request::Poll => "poll",
                        Request::Retry => "retry",
                    }
                );
            }
        }
    }
}

#[derive(Debug)]
enum HighlightBlackout {
    Blackout(Instant),
    Receiving,
}

impl HighlightBlackout {
    fn allow_highlights(&self) -> bool {
        match self {
            HighlightBlackout::Blackout(_) => false,
            HighlightBlackout::Receiving => true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Map(BTreeMap<Server, State>);

impl Map {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn disconnected(&mut self, server: Server) {
        self.0.insert(server, State::Disconnected);
    }

    pub fn ready(&mut self, server: Server, client: Handle) {
        self.0.insert(server, State::Ready(client));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn remove(&mut self, server: &Server) -> Option<Handle> {
        self.0.remove(server).and_then(|state| match state {
            State::Disconnected => None,
            State::Ready(client) => Some(client),
        })
    }

    pub fn client(&self, server: &Server) -> Option<&Handle> {
        if let Some(State::Ready(client)) = self.0.get(server) {
            Some(client)
        } else {
            None
        }
    }

    pub fn client_mut(&mut self, server: &Server) -> Option<&mut Handle> {
        if let Some(State::Ready(client)) = self.0.get_mut(server) {
            Some(client)
        } else {
            None
        }
    }

    pub fn nickname<'a>(&'a self, server: &Server) -> Option<NickRef<'a>> {
        self.client(server).map(Handle::nickname)
    }

    pub fn send(&mut self, buffer: &Buffer, message: message::Encoded) {
        if let Some(client) = self.client_mut(buffer.server()) {
            client.send(buffer, message);
        }
    }

    pub fn join(&mut self, server: &Server, channels: &[String]) {
        if let Some(client) = self.client_mut(server) {
            client.join(channels);
        }
    }

    pub fn quit(&mut self, server: &Server, reason: Option<String>) {
        if let Some(client) = self.client_mut(server) {
            client.quit(reason);
        }
    }

    pub fn resolve_user_attributes<'a>(
        &'a self,
        server: &Server,
        channel: &str,
        user: &User,
    ) -> Option<&'a User> {
        self.client(server)
            .and_then(|client| client.resolve_user_attributes(channel, user))
    }

    pub fn get_channel_users<'a>(&'a self, server: &Server, channel: &str) -> &'a [User] {
        self.client(server)
            .map(|client| client.users(channel))
            .unwrap_or_default()
    }

    pub fn get_user_channels<'a>(&'a self, server: &Server, nick: NickRef) -> Vec<&'a str> {
        self.client(server)
            .map(|client| client.user_channels(nick))
            .unwrap_or_default()
    }

    pub fn get_channel_topic<'a>(&'a self, server: &Server, channel: &str) -> Option<&'a Topic> {
        self.client(server)
            .map(|client| client.topic(channel))
            .unwrap_or_default()
    }

    pub fn get_channels<'a>(&'a self, server: &Server) -> &'a [String] {
        self.client(server)
            .map(|client| client.channels())
            .unwrap_or_default()
    }

    pub fn get_isupport(&self, server: &Server) -> HashMap<isupport::Kind, isupport::Parameter> {
        self.client(server)
            .map(|client| client.isupport.clone())
            .unwrap_or_default()
    }

    pub fn get_sender(&self, server: &Server) -> Option<&Sender> {
        self.client(server).map(|client| &client.sender)
    }

    pub fn connected_servers(&self) -> impl Iterator<Item = &Server> {
        self.0.iter().filter_map(|(server, state)| {
            if let State::Ready(_) = state {
                Some(server)
            } else {
                None
            }
        })
    }

    pub fn iter(&self) -> std::collections::btree_map::Iter<Server, State> {
        self.0.iter()
    }

    pub fn status(&self, server: &Server) -> Status {
        self.0
            .get(server)
            .map(|s| match s {
                State::Disconnected => Status::Disconnected,
                State::Ready(_) => Status::Connected,
            })
            .unwrap_or(Status::Unavailable)
    }

    pub fn config_updated(&mut self, config: Config) {
        self.0.values_mut().for_each(|client| {
            if let State::Ready(client) = client {
                client.config_updated(config.clone());
            }
        })
    }
}

#[derive(Debug, Clone)]
pub enum Context {
    Buffer(Buffer),
    Whois(Buffer),
}

impl Context {
    fn new(message: &proto::Message, buffer: Buffer) -> Self {
        if let Command::WHOIS(_, _) = message.command {
            Self::Whois(buffer)
        } else {
            Self::Buffer(buffer)
        }
    }

    fn is_whois(&self) -> bool {
        matches!(self, Self::Whois(_))
    }

    fn buffer(self) -> Buffer {
        match self {
            Context::Buffer(buffer) => buffer,
            Context::Whois(buffer) => buffer,
        }
    }
}

#[derive(Debug)]
pub struct Batch {
    context: Option<Context>,
    events: Vec<Event>,
}

impl Batch {
    fn new(context: Option<Context>) -> Self {
        Self {
            context,
            events: vec![],
        }
    }
}

fn generate_label() -> String {
    Posix::now().as_nanos().to_string()
}

fn remove_tag(key: &str, tags: &mut Vec<irc::proto::Tag>) -> Option<String> {
    tags.remove(tags.iter().position(|tag| tag.key == key)?)
        .value
}

fn start_reroute(command: &Command) -> bool {
    use Command::*;

    if let MODE(target, _, _) = command {
        !proto::is_channel(target)
    } else {
        matches!(command, WHO(..) | WHOIS(..) | WHOWAS(..))
    }
}

fn stop_reroute(command: &Command) -> bool {
    use command::Numeric::*;

    matches!(
        command,
        Command::Numeric(
            RPL_ENDOFWHO
                | RPL_ENDOFWHOIS
                | RPL_ENDOFWHOWAS
                | ERR_NOSUCHNICK
                | ERR_NOSUCHSERVER
                | ERR_NONICKNAMEGIVEN
                | ERR_WASNOSUCHNICK
                | ERR_NEEDMOREPARAMS
                | ERR_USERSDONTMATCH
                | RPL_UMODEIS
                | ERR_UMODEUNKNOWNFLAG,
            _
        )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RegistrationStep {
    List,
    Req,
    Sasl,
    End,
}

#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub users: HashSet<User>,
    pub last_who: Option<WhoStatus>,
    pub topic: Topic,
    pub names_init: bool,
}

impl Channel {
    pub fn update_user_away(&mut self, user: &str, flags: &str) {
        let user = User::from(Nick::from(user));

        if let Some(away_flag) = flags.chars().next() {
            // H = Here, G = gone (away)
            let away = match away_flag {
                'G' => true,
                'H' => false,
                _ => return,
            };

            if let Some(mut user) = self.users.take(&user) {
                user.update_away(away);
                self.users.insert(user);
            }
        }
    }

    pub fn update_user_accountname(&mut self, user: &str, accountname: &str) {
        let user = User::from(Nick::from(user));

        if let Some(user) = self.users.take(&user) {
            self.users.insert(user.with_accountname(accountname));
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Topic {
    pub content: Option<message::Content>,
    pub who: Option<String>,
    pub time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub enum WhoStatus {
    Requested(Instant, Option<isupport::WhoToken>),
    Receiving(Option<isupport::WhoToken>),
    Done(Instant),
}

/// Group channels together into as few JOIN messages as possible
fn group_joins<'a>(
    channels: &'a [String],
    keys: &'a HashMap<String, String>,
) -> impl Iterator<Item = proto::Message> + 'a {
    const MAX_LEN: usize = proto::format::BYTE_LIMIT - b"JOIN \r\n".len();

    let (without_keys, with_keys): (Vec<_>, Vec<_>) = channels.iter().partition_map(|channel| {
        keys.get(channel)
            .map(|key| Either::Right((channel, key)))
            .unwrap_or(Either::Left(channel))
    });

    let joins_without_keys = without_keys
        .into_iter()
        .scan(0, |count, channel| {
            // Channel + a comma
            *count += channel.len() + 1;

            let chunk = *count / MAX_LEN;

            Some((chunk, channel))
        })
        .into_group_map()
        .into_values()
        .map(|channels| command!("JOIN", channels.into_iter().join(",")));

    let joins_with_keys = with_keys
        .into_iter()
        .scan(0, |count, (channel, key)| {
            // Channel + key + a comma for each
            *count += channel.len() + key.len() + 2;

            let chunk = *count / MAX_LEN;

            Some((chunk, (channel, key)))
        })
        .into_group_map()
        .into_values()
        .map(|values| {
            command!(
                "JOIN",
                values.iter().map(|(c, _)| c).join(","),
                values.iter().map(|(_, k)| k).join(",")
            )
        });

    joins_without_keys.chain(joins_with_keys)
}

fn group_monitors(
    targets: &[String],
    target_limit: Option<u16>,
) -> impl Iterator<Item = proto::Message> + '_ {
    const MAX_LEN: usize = proto::format::BYTE_LIMIT - b"MONITOR + \r\n".len();

    if let Some(target_limit) = target_limit.map(usize::from) {
        &targets[0..std::cmp::min(target_limit, targets.len())]
    } else {
        targets
    }
    .iter()
    .scan(0, |count, target| {
        // Target + a comma
        *count += target.len() + 1;

        let chunk = *count / MAX_LEN;

        Some((chunk, target))
    })
    .into_group_map()
    .into_values()
    .map(|targets| command!("MONITOR", "+", targets.into_iter().join(","),))
}

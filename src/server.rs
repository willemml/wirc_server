use crate::{api, channel, error::DataError, hub::Hub, Error, Result, ID};
use actix::prelude::*;
use parse_display::{Display, FromStr};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
};
use tantivy::{
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::QueryParser,
    schema::{Field, Schema, FAST, STORED, TEXT},
    Index, IndexReader, IndexWriter, LeasedItem, ReloadPolicy, Searcher,
};

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub struct ClientServerMessage {
    /// Client address to send the response to.
    pub client_addr: Option<Recipient<ServerResponse>>,
    /// ID of the message, server will use this to send a response. Should be generated by the client.
    pub message_id: u128,
    /// Client's message.
    pub command: ClientCommand,
}

impl From<ClientCommand> for ClientServerMessage {
    fn from(cmd: ClientCommand) -> Self {
        Self {
            client_addr: None,
            message_id: 0,
            command: cmd,
        }
    }
}

#[derive(Clone)]
pub enum ClientCommand {
    Disconnect(Recipient<ServerMessage>),
    SubscribeHub(ID, ID, Recipient<ServerMessage>),
    UnsubscribeHub(ID, Recipient<ServerMessage>),
    SubscribeChannel(ID, ID, ID, Recipient<ServerMessage>),
    UnsubscribeChannel(ID, ID, Recipient<ServerMessage>),
    StartTyping(ID, ID, ID),
    StopTyping(ID, ID, ID),
    SendMessage(ID, ID, ID, String),
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub struct ServerResponse {
    /// ID of the message the server is responding to.
    pub responding_to: u128,
    /// Server's response.
    pub message: Response,
}

#[derive(MessageResponse, Clone, Display, FromStr, Message)]
#[rtype(result = "()")]
#[display(style = "SNAKE_CASE")]
pub enum Response {
    #[display("{}({0})")]
    Error(Error),
    Success,
    #[display("{}({0})")]
    Id(ID),
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub enum ServerMessage {
    NewMessage(ID, ID, channel::Message),
    HubUpdated(ID),
    TypingStart(ID, ID, ID),
    TypingStop(ID, ID, ID),
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub enum ServerNotification {
    NewMessage(ID, ID, channel::Message),
    HubUpdated(ID),
    Stop,
}

#[derive(Clone)]
struct MessageSchemaFields {
    content: Field,
    created: Field,
    id: Field,
    sender: Field,
}

#[derive(Message)]
#[rtype(result = "Result<()>")]
struct NewMessageForIndex {
    hub_id: ID,
    channel_id: ID,
    message: channel::Message,
}

#[derive(Message)]
#[rtype(result = "Result<Vec<ID>>")]
pub struct SearchMessageIndex {
    pub hub_id: ID,
    pub channel_id: ID,
    pub limit: usize,
    pub query: String,
}

pub struct MessageServer {
    indexes: HashMap<(ID, ID), Index>,
    index_writers: HashMap<(ID, ID), IndexWriter>,
    index_readers: HashMap<(ID, ID), IndexReader>,
    pending_messages: HashMap<(ID, ID), (u128, ID)>,
    schema: Schema,
    schema_fields: MessageSchemaFields,
    commit_threshold: u8,
}

impl MessageServer {
    fn new(commit_threshold: u8) -> Self {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("content", TEXT);
        schema_builder.add_date_field("created", FAST);
        schema_builder.add_bytes_field("id", STORED | FAST);
        schema_builder.add_bytes_field("sender", ());
        let schema = schema_builder.build();
        Self {
            commit_threshold,
            schema_fields: MessageSchemaFields {
                content: schema
                    .get_field("content")
                    .expect("Failed to create a Tantivy schema correctly."),
                created: schema
                    .get_field("created")
                    .expect("Failed to create a Tantivy schema correctly."),
                id: schema
                    .get_field("id")
                    .expect("Failed to create a Tantivy schema correctly."),
                sender: schema
                    .get_field("sender")
                    .expect("Failed to create a Tantivy schema correctly."),
            },
            schema: schema,
            indexes: HashMap::new(),
            index_writers: HashMap::new(),
            index_readers: HashMap::new(),
            pending_messages: HashMap::new(),
        }
    }

    fn log_last_message(hub_id: &ID, channel_id: &ID, message_id: &ID) -> Result<()> {
        let log_path_string = format!(
            "{}/{:x}/{:x}/log",
            crate::hub::HUB_DATA_FOLDER,
            hub_id.as_u128(),
            channel_id.as_u128()
        );
        let log_path = std::path::Path::new(&log_path_string);
        let mut log_file = std::fs::File::create(log_path)?;
        log_file.write(message_id.as_bytes())?;
        log_file.flush()?;
        Ok(())
    }

    fn setup_index(&mut self, hub_id: &ID, channel_id: &ID) -> Result<()> {
        let dir_string = format!(
            "{}/{:x}/{:x}/index",
            crate::hub::HUB_DATA_FOLDER,
            hub_id.as_u128(),
            channel_id.as_u128()
        );
        let dir_path = std::path::Path::new(&dir_string);
        if !dir_path.is_dir() {
            std::fs::create_dir_all(dir_path)?;
        }
        let dir = MmapDirectory::open(dir_path).map_err(|_| DataError::Directory)?;
        let index =
            Index::open_or_create(dir, self.schema.clone()).map_err(|_| DataError::Directory)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()
            .map_err(|_| DataError::Directory)?;
        let writer = index.writer(50_000_000).map_err(|_| DataError::Directory)?;
        let key = (hub_id.clone(), channel_id.clone());
        self.indexes.insert(key.clone(), index);
        self.index_readers.insert(key.clone(), reader);
        self.index_writers.insert(key.clone(), writer);
        Ok(())
    }

    fn get_reader(&mut self, hub_id: &ID, channel_id: &ID) -> Result<&IndexReader> {
        let key = (hub_id.clone(), channel_id.clone());
        if !self.index_readers.contains_key(&key) {
            self.setup_index(hub_id, channel_id)?;
        }
        if let Some(reader) = self.index_readers.get(&key) {
            Ok(reader)
        } else {
            Err(DataError::Directory.into())
        }
    }

    fn get_searcher(&mut self, hub_id: &ID, channel_id: &ID) -> Result<LeasedItem<Searcher>> {
        let reader = self.get_reader(hub_id, channel_id)?;
        let _ = reader.reload();
        Ok(reader.searcher())
    }

    fn get_writer(&mut self, hub_id: &ID, channel_id: &ID) -> Result<&mut IndexWriter> {
        let key = (hub_id.clone(), channel_id.clone());
        if !self.index_writers.contains_key(&key) {
            self.setup_index(hub_id, channel_id)?;
        }
        if let Some(writer) = self.index_writers.get_mut(&key) {
            Ok(writer)
        } else {
            Err(DataError::Directory.into())
        }
    }
}

impl Actor for MessageServer {
    type Context = Context<Self>;

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        for (hc_id, writer) in self.index_writers.iter_mut() {
            let _ = writer.commit();
            if let Some((_, id)) = self.pending_messages.get(hc_id) {
                let _ = Self::log_last_message(&hc_id.0, &hc_id.1, id);
            }
        }
        Running::Stop
    }
}

impl Handler<SearchMessageIndex> for MessageServer {
    type Result = Result<Vec<ID>>;

    fn handle(&mut self, msg: SearchMessageIndex, _: &mut Self::Context) -> Self::Result {
        {
            let pending = self.pending_messages.clone();
            dbg!(&pending);
            if let Some((pending, message_id)) = pending.get(&(msg.hub_id, msg.channel_id)) {
                if pending != &0 {
                    let _ = self.get_writer(&msg.hub_id, &msg.channel_id)?.commit();
                    Self::log_last_message(&msg.hub_id, &msg.channel_id, message_id)?;
                } else {
                }
                self.pending_messages.insert(
                    (msg.hub_id.clone(), msg.channel_id.clone()),
                    (0, message_id.clone()),
                );
            }
        }
        let searcher = self.get_searcher(&msg.hub_id, &msg.channel_id)?;
        let query_parser =
            QueryParser::for_index(searcher.index(), vec![self.schema_fields.content.clone()]);
        let query = query_parser
            .parse_query(&msg.query)
            .map_err(|_| DataError::Directory)?;
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(msg.limit))
            .map_err(|_| DataError::Directory)?;
        let mut result = Vec::new();
        for (_score, doc_address) in top_docs {
            let retrieved_doc = searcher
                .doc(doc_address)
                .map_err(|_| DataError::Directory)?;
            if let Some(value) = retrieved_doc.get_first(self.schema_fields.id.clone()) {
                if let Some(bytes) = value.bytes_value() {
                    if let Ok (id) = bincode::deserialize::<ID>(bytes) {
                        result.push(id);
                    }
                }
            }
        }
        Ok(result)
    }
}

impl Handler<NewMessageForIndex> for MessageServer {
    type Result = Result<()>;

    fn handle(&mut self, msg: NewMessageForIndex, _: &mut Self::Context) -> Self::Result {
        let get_pending = self.pending_messages.clone();
        let commit_threshold = self.commit_threshold.clone() as u128;
        let MessageSchemaFields {
            content,
            created,
            id,
            sender,
        } = self.schema_fields.clone();
        let writer = self.get_writer(&msg.hub_id, &msg.channel_id)?;
        writer.add_document(doc!(
            id => bincode::serialize(&msg.message.id).map_err(|_| DataError::Serialize)?,
            sender => bincode::serialize(&msg.message.sender).map_err(|_| DataError::Serialize)?,
            created => msg.message.created as i64,
            content => msg.message.content,
        ));
        let mut new_pending;
        if let Some((pending, _)) = get_pending.get(&(msg.hub_id, msg.channel_id)) {
            new_pending = pending + 1;
            if pending >= &commit_threshold {
                if let Ok(_) = writer.commit() {
                    Self::log_last_message(&msg.hub_id, &msg.channel_id, &msg.message.id)?;
                    new_pending = 0;
                } else {
                    Err(DataError::Directory)?
                }
            }
        } else {
            new_pending = 1;
        }
        drop(writer);
        let _ = self
            .pending_messages
            .insert((msg.hub_id, msg.channel_id), (new_pending, msg.message.id));
        Ok(())
    }
}

pub struct Server {
    subscribed_channels: HashMap<(ID, ID), HashSet<Recipient<ServerMessage>>>,
    subscribed_hubs: HashMap<ID, HashSet<Recipient<ServerMessage>>>,
    subscribed: HashMap<Recipient<ServerMessage>, (HashSet<(ID, ID)>, HashSet<ID>)>,
    message_server: Addr<MessageServer>,
}

impl Server {
    pub fn new(commit_threshold: u8) -> Self {
        Self {
            subscribed_channels: HashMap::new(),
            subscribed_hubs: HashMap::new(),
            subscribed: HashMap::new(),
            message_server: MessageServer::new(commit_threshold).start(),
        }
    }

    async fn send_hub(
        subscribed_hubs: HashMap<ID, HashSet<Recipient<ServerMessage>>>,
        message: ServerMessage,
        hub_id: ID,
    ) {
        if let Some(subscribed) = subscribed_hubs.get(&hub_id) {
            for connection in subscribed {
                let _ = connection.do_send(message.clone());
            }
        }
    }

    async fn send_channel(
        subscribed_channels: HashMap<(ID, ID), HashSet<Recipient<ServerMessage>>>,
        message: ServerMessage,
        hub_id: ID,
        channel_id: ID,
    ) {
        if let Some(subscribed) = subscribed_channels.get(&(hub_id, channel_id)) {
            for connection in subscribed {
                let _ = connection.do_send(message.clone());
            }
        }
    }
}

impl Actor for Server {
    type Context = Context<Self>;
}

impl Handler<ClientServerMessage> for Server {
    type Result = ();

    fn handle(&mut self, msg: ClientServerMessage, ctx: &mut Self::Context) -> Self::Result {
        match msg.command.clone() {
            ClientCommand::Disconnect(addr) => {
                if let Some((channels, hubs)) = self.subscribed.get(&addr) {
                    for channel in channels {
                        self.subscribed_channels
                            .get_mut(channel)
                            .and_then(|s| Some(s.remove(&addr)));
                    }
                    for hub in hubs {
                        self.subscribed_hubs
                            .get_mut(hub)
                            .and_then(|s| Some(s.remove(&addr)));
                    }
                }
                self.subscribed.remove(&addr);
            }
            ClientCommand::SubscribeChannel(user_id, hub_id, channel_id, addr) => {
                futures::executor::block_on(async {
                    let result = Hub::load(&hub_id)
                        .await
                        .and_then(|hub| {
                            if let Ok(member) = hub.get_member(&user_id) {
                                Ok((hub, member))
                            } else {
                                Err(Error::MemberNotFound)
                            }
                        })
                        .and_then(|(hub, user)| {
                            if user.has_channel_permission(
                                &channel_id,
                                &crate::permission::ChannelPermission::ViewChannel,
                                &hub,
                            ) {
                                self.subscribed
                                    .entry(addr.clone())
                                    .or_default()
                                    .0
                                    .insert((hub_id.clone(), channel_id.clone()));
                                self.subscribed_channels
                                    .entry((hub_id, channel_id))
                                    .or_default()
                                    .insert(addr);
                                Ok(())
                            } else {
                                Err(Error::MissingChannelPermission(
                                    crate::permission::ChannelPermission::ViewChannel,
                                ))
                            }
                        });
                    let response = if let Err(error) = result {
                        Response::Error(error)
                    } else {
                        Response::Success
                    };
                    if let Some(addr) = msg.client_addr {
                        let _ = addr
                            .send(ServerResponse {
                                responding_to: msg.message_id,
                                message: response,
                            })
                            .await;
                    }
                });
            }
            ClientCommand::UnsubscribeChannel(hub_id, channel_id, recipient) => {
                if let Some(subs) = self.subscribed.get_mut(&recipient) {
                    subs.0.remove(&(hub_id, channel_id));
                }
                if let Some(entry) = self.subscribed_channels.get_mut(&(hub_id, channel_id)) {
                    entry.remove(&recipient);
                }
            }
            ClientCommand::SubscribeHub(user_id, hub_id, addr) => {
                futures::executor::block_on(async {
                    let result = if let Err(error) = Hub::load(&hub_id)
                        .await
                        .and_then(|hub| hub.get_member(&user_id))
                    {
                        Response::Error(error)
                    } else {
                        self.subscribed
                            .entry(addr.clone())
                            .or_default()
                            .1
                            .insert(hub_id.clone());
                        self.subscribed_hubs.entry(hub_id).or_default().insert(addr);
                        Response::Success
                    };
                    if let Some(addr) = msg.client_addr {
                        let _ = addr
                            .send(ServerResponse {
                                responding_to: msg.message_id,
                                message: result,
                            })
                            .await;
                    }
                });
            }
            ClientCommand::UnsubscribeHub(hub_id, recipient) => {
                if let Some(subs) = self.subscribed.get_mut(&recipient) {
                    subs.1.remove(&hub_id);
                }
                if let Some(entry) = self.subscribed_hubs.get_mut(&hub_id) {
                    entry.remove(&recipient);
                }
            }
            ClientCommand::StartTyping(user_id, hub_id, channel_id) => {
                let subscribed = self.subscribed_channels.clone();
                async move {
                    let result = if let Err(err) = {
                        let result = Hub::load(&hub_id)
                            .await
                            .and_then(|hub| hub.get_channel(&user_id, &channel_id).map(|_| ()))
                            .and_then(|_| {
                                Ok(Self::send_channel(
                                    subscribed,
                                    ServerMessage::TypingStart(hub_id, channel_id, user_id),
                                    hub_id,
                                    channel_id,
                                ))
                            });
                        if let Ok(fut) = result {
                            fut.await;
                            Ok(())
                        } else {
                            Err(result.err().unwrap())
                        }
                    } {
                        Response::Error(err)
                    } else {
                        Response::Success
                    };
                    if let Some(addr) = msg.client_addr {
                        let _ = addr
                            .send(ServerResponse {
                                responding_to: msg.message_id,
                                message: result,
                            })
                            .await;
                    }
                }
                .into_actor(self)
                .spawn(ctx);
            }
            ClientCommand::StopTyping(user_id, hub_id, channel_id) => {
                let subscribed = self.subscribed_channels.clone();
                Self::send_channel(
                    subscribed,
                    ServerMessage::TypingStop(hub_id, channel_id, user_id),
                    hub_id,
                    channel_id,
                )
                .into_actor(self)
                .spawn(ctx);
            }
            ClientCommand::SendMessage(user_id, hub_id, channel_id, message) => {
                let subscribed = self.subscribed_channels.clone();
                let message_server = self
                    .message_server
                    .clone()
                    .recipient::<NewMessageForIndex>();
                async move {
                    let res = {
                        let send = api::send_message(&user_id, &hub_id, &channel_id, message).await;
                        if let Ok(message) = send {
                            let msg_id = message.id.clone();
                            tokio::spawn(async move {
                                let _ = message_server
                                    .send(NewMessageForIndex {
                                        hub_id: hub_id.clone(),
                                        channel_id: channel_id.clone(),
                                        message: message.clone(),
                                    })
                                    .await;
                                Self::send_channel(
                                    subscribed,
                                    ServerMessage::NewMessage(hub_id, channel_id, message),
                                    hub_id,
                                    channel_id,
                                )
                                .await;
                            });
                            Response::Id(msg_id)
                        } else {
                            Response::Error(send.err().unwrap())
                        }
                    };
                    if let Some(addr) = msg.client_addr {
                        let _ = addr
                            .send(ServerResponse {
                                responding_to: msg.message_id,
                                message: res,
                            })
                            .await;
                    }
                }
                .into_actor(self)
                .spawn(ctx);
            }
        }
    }
}

impl Handler<ServerNotification> for Server {
    type Result = ();

    fn handle(&mut self, msg: ServerNotification, ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ServerNotification::NewMessage(hub_id, channel_id, message) => {
                let message_server = self.message_server.clone().recipient();
                let m = message.clone();
                async move {
                    let _ = message_server
                        .send(NewMessageForIndex {
                            hub_id: hub_id.clone(),
                            channel_id: channel_id.clone(),
                            message: message.clone(),
                        })
                        .await;
                }
                .into_actor(self)
                .spawn(ctx);
                Self::send_channel(
                    self.subscribed_channels.clone(),
                    ServerMessage::NewMessage(hub_id, channel_id, m),
                    hub_id,
                    channel_id,
                )
                .into_actor(self)
                .spawn(ctx);
            }
            ServerNotification::HubUpdated(hub_id) => {
                Self::send_hub(
                    self.subscribed_hubs.clone(),
                    ServerMessage::HubUpdated(hub_id),
                    hub_id,
                )
                .into_actor(self)
                .spawn(ctx);
            }
            ServerNotification::Stop => {
                ctx.stop();
            }
        }
    }
}

#[derive(Message)]
#[rtype(result = "Addr<MessageServer>")]
pub struct GetMessageServer;

impl Handler<GetMessageServer> for Server {
    type Result = Addr<MessageServer>;

    fn handle(&mut self, _: GetMessageServer, _: &mut Self::Context) -> Self::Result {
        self.message_server.clone()
    }
}

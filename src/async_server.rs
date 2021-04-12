use crate::{
    api, channel, check_permission,
    error::{DataError, IndexError},
    hub::Hub,
    Error, Result, ID,
};
use actix::prelude::*;
use futures::{future::LocalBoxFuture, lock::Mutex, FutureExt};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::Arc,
};
use tantivy::{
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::QueryParser,
    schema::{Schema, FAST, STORED, TEXT},
    Index, IndexReader, IndexWriter, LeasedItem, ReloadPolicy, Searcher,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

use crate::server::*;

use lazy_static::lazy_static;

pub mod client_command {
    use super::{Message, Recipient, Result, ServerMessage, ID};

    /// Disconnects the client by unsubscribing them from everything (does not drop connection).
    #[derive(Message, Clone)]
    #[rtype(result = "()")]
    pub struct Disconnect {
        pub addr: Recipient<ServerMessage>,
    }
    /// Subscribes the client to notifications on a hub (everything except for messages sent in channels in the hub).
    #[derive(Message, Clone)]
    #[rtype(result = "Result")]
    pub struct SubscribeHub {
        pub user_id: ID,
        pub hub_id: ID,
        pub addr: Recipient<ServerMessage>,
    }
    /// Unsubscribes the client from notifications in a hub, does not change channel subscriptions.
    #[derive(Message, Clone)]
    #[rtype(result = "()")]
    pub struct UnsubscribeHub {
        pub hub_id: ID,
        pub addr: Recipient<ServerMessage>,
    }
    /// Subscribes the client to notifications of new messages in the given channel.
    #[derive(Message, Clone)]
    #[rtype(result = "Result")]
    pub struct SubscribeChannel {
        pub user_id: ID,
        pub hub_id: ID,
        pub channel_id: ID,
        pub addr: Recipient<ServerMessage>,
    }
    /// Unsubscribes the client to notifications of new messages in the given channel.
    #[derive(Message, Clone)]
    #[rtype(result = "()")]
    pub struct UnsubscribeChannel {
        pub hub_id: ID,
        pub channel_id: ID,
        pub addr: Recipient<ServerMessage>,
    }
    /// Notifies other clients subscribed to the given channel that the given user has started typing.
    #[derive(Message, Clone)]
    #[rtype(result = "Result")]
    pub struct StartTyping {
        pub user_id: ID,
        pub hub_id: ID,
        pub channel_id: ID,
    }
    /// Notifies other clients subscribed to the given channel that the given user has stopped typing.
    #[derive(Message, Clone)]
    #[rtype(result = "Result")]
    pub struct StopTyping {
        pub user_id: ID,
        pub hub_id: ID,
        pub channel_id: ID,
    }
    /// Tells the server to send the given message in the given channel, also notifies other clients that are subscribed to the channel of the new message.
    #[derive(Message, Clone)]
    #[rtype(result = "Result<ID>")]
    pub struct SendMessage {
        pub user_id: ID,
        pub hub_id: ID,
        pub channel_id: ID,
        pub message: String,
    }
}

/// Tells the [`AsyncServer`] to get an address to it's [`AsyncMessageServer`].
#[derive(Message)]
#[rtype(result = "Addr<AsyncMessageServer>")]
pub struct AsyncGetMessageServer;

lazy_static! {
    static ref MESSAGE_SCHEMA: Schema = {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("content", TEXT);
        schema_builder.add_date_field("created", FAST);
        schema_builder.add_bytes_field("id", STORED | FAST);
        schema_builder.add_bytes_field("sender", ());
        schema_builder.build()
    };
    static ref MESSAGE_SCHEMA_FIELDS: MessageSchemaFields = MessageSchemaFields {
        content: MESSAGE_SCHEMA
            .get_field("content")
            .expect("Failed to create a Tantivy schema correctly."),
        created: MESSAGE_SCHEMA
            .get_field("created")
            .expect("Failed to create a Tantivy schema correctly."),
        id: MESSAGE_SCHEMA
            .get_field("id")
            .expect("Failed to create a Tantivy schema correctly."),
        sender: MESSAGE_SCHEMA
            .get_field("sender")
            .expect("Failed to create a Tantivy schema correctly."),
    };
}

pub fn add_message_to_writer(writer: &mut IndexWriter, message: channel::Message) -> Result {
    writer.add_document(doc!(
        MESSAGE_SCHEMA_FIELDS.id => bincode::serialize(&message.id).map_err(|_| DataError::Serialize)?,
        MESSAGE_SCHEMA_FIELDS.sender => bincode::serialize(&message.sender).map_err(|_| DataError::Serialize)?,
        MESSAGE_SCHEMA_FIELDS.created => message.created as i64,
        MESSAGE_SCHEMA_FIELDS.content => message.content,
    ));
    Ok(())
}

pub type IndexMap = Arc<RwLock<HashMap<(ID, ID), Arc<Index>>>>;
pub type IndexWriterMap = Arc<RwLock<HashMap<(ID, ID), Arc<Mutex<IndexWriter>>>>>;
pub type IndexReaderMap = Arc<RwLock<HashMap<(ID, ID), Arc<IndexReader>>>>;
pub type PendingMessageMap = Arc<RwLock<HashMap<(ID, ID), (u8, ID)>>>;

#[derive(Clone)]
pub struct AsyncMessageServer {
    indexes: IndexMap,
    index_writers: IndexWriterMap,
    index_readers: IndexReaderMap,
    pending_messages: PendingMessageMap,
}

impl AsyncMessageServer {
    pub fn new() -> Self {
        Self {
            indexes: Arc::new(RwLock::new(HashMap::new())),
            index_writers: Arc::new(RwLock::new(HashMap::new())),
            index_readers: Arc::new(RwLock::new(HashMap::new())),
            pending_messages: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Logs the given message ID to a file, should be called after any Tantivy commits.
    async fn log_last_message(hub_id: &ID, channel_id: &ID, message_id: &ID) -> Result {
        let log_path_string = format!(
            "{}/{:x}/{:x}/log",
            crate::hub::HUB_DATA_FOLDER,
            hub_id.as_u128(),
            channel_id.as_u128()
        );
        tokio::fs::write(log_path_string, &message_id.as_u128().to_ne_bytes()).await?;
        Ok(())
    }

    async fn log_if_nologs(hub_id: &ID, channel_id: &ID, message_id: &ID) -> Result {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(format!(
                "{}/{:x}/{:x}/log",
                crate::hub::HUB_DATA_FOLDER,
                hub_id.as_u128(),
                channel_id.as_u128()
            ))
            .await?;
        file.write(&message_id.as_u128().to_ne_bytes()).await?;
        Ok(())
    }

    /// Sets up the Tantivy index for a given channel, also makes sure that the index is up to date by commiting any messages sent after the last message sent (logged by [`log_last_message`]).
    async fn setup_index(
        indexes: &IndexMap,
        index_readers: &IndexReaderMap,
        index_writers: &IndexWriterMap,
        hub_id: &ID,
        channel_id: &ID,
    ) -> Result {
        let dir_string = format!(
            "{}/{:x}/{:x}/index",
            crate::hub::HUB_DATA_FOLDER,
            hub_id.as_u128(),
            channel_id.as_u128()
        );
        let dir_path = std::path::Path::new(&dir_string);
        if !dir_path.is_dir() {
            tokio::fs::create_dir_all(dir_path).await?;
        }
        let dir = MmapDirectory::open(dir_path).map_err(|_| DataError::Directory)?;
        let index = Index::open_or_create(dir, MESSAGE_SCHEMA.clone())
            .map_err(|_| IndexError::OpenCreateIndex)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()
            .map_err(|_| IndexError::CreateReader)?;
        let mut writer = index
            .writer(50_000_000)
            .map_err(|_| IndexError::CreateWriter)?;
        let key = (hub_id.clone(), channel_id.clone());
        let log_path_string = format!(
            "{}/{:x}/{:x}/log",
            crate::hub::HUB_DATA_FOLDER,
            hub_id.as_u128(),
            channel_id.as_u128()
        );
        let log_path = std::path::Path::new(&log_path_string);
        if log_path.is_file() {
            let mut buf: [u8; 16] = [0; 16];
            tokio::fs::read(log_path)
                .await?
                .as_slice()
                .read_exact(&mut buf)?;
            let last_id = ID::from_u128(u128::from_le_bytes(buf));
            let filename = format!("{}{:x}.json", crate::hub::HUB_INFO_FOLDER, hub_id.as_u128());
            let path = std::path::Path::new(&filename);
            if !path.exists() {
                return Err(Error::HubNotFound);
            }
            let json = tokio::fs::read_to_string(path).await?;
            let hub = serde_json::from_str::<Hub>(&json).map_err(|_| DataError::Deserialize)?;
            if let Some(channel) = hub.channels.get(channel_id) {
                let messages = channel.async_get_all_messages_from(&last_id).await;
                let last_id = if let Some(last) = messages.last() {
                    Some(last.id.clone())
                } else {
                    None
                };

                for message in messages {
                    add_message_to_writer(&mut writer, message)?;
                }
                writer.commit().map_err(|_| IndexError::Commit)?;
                if let Some(last_id) = last_id {
                    Self::log_last_message(&hub_id, &channel_id, &last_id).await?;
                }
                reader.reload().map_err(|_| IndexError::Reload)?;
            }
        }
        indexes.write().await.insert(key.clone(), Arc::new(index));
        index_readers
            .write()
            .await
            .insert(key.clone(), Arc::new(reader));
        index_writers
            .write()
            .await
            .insert(key.clone(), Arc::new(Mutex::new(writer)));
        Ok(())
    }

    /// Gets a reader for a Tantivy index, also runs [`setup_index`] if it hasn't already been run for the given channel.
    async fn get_reader(
        indexes: &IndexMap,
        index_readers: &IndexReaderMap,
        index_writers: &IndexWriterMap,
        hub_id: &ID,
        channel_id: &ID,
    ) -> Result<Arc<IndexReader>> {
        let key = (hub_id.clone(), channel_id.clone());
        if !index_readers.read().await.contains_key(&key) {
            Self::setup_index(indexes, index_readers, index_writers, hub_id, channel_id).await?;
        }
        if let Some(reader) = index_readers.read().await.get(&key) {
            Ok(Arc::clone(reader))
        } else {
            Err(IndexError::GetReader.into())
        }
    }

    /// Gets a searcher for the Tantivy index for a channel, uses [`get_reader`].
    async fn get_searcher(
        indexes: &IndexMap,
        index_readers: &IndexReaderMap,
        index_writers: &IndexWriterMap,
        hub_id: &ID,
        channel_id: &ID,
    ) -> Result<LeasedItem<Searcher>> {
        let reader =
            Self::get_reader(indexes, index_readers, index_writers, hub_id, channel_id).await?;
        let _ = reader.reload();
        Ok(reader.searcher())
    }

    /// Gets a writer for a Tantivy index, also runs [`setup_index`] if it hasn't already been run for the given channel.
    async fn get_writer(
        indexes: &IndexMap,
        index_readers: &IndexReaderMap,
        index_writers: &IndexWriterMap,
        hub_id: &ID,
        channel_id: &ID,
    ) -> Result<Arc<Mutex<IndexWriter>>> {
        let key = (hub_id.clone(), channel_id.clone());
        if !index_writers.read().await.contains_key(&key) {
            Self::setup_index(indexes, index_readers, index_writers, hub_id, channel_id).await?;
        }
        if let Some(writer) = index_writers.read().await.get(&key) {
            Ok(Arc::clone(writer))
        } else {
            Err(IndexError::GetWriter.into())
        }
    }

    fn clone_all(&self) -> (IndexMap, IndexReaderMap, IndexWriterMap, PendingMessageMap) {
        (
            Arc::clone(&self.indexes),
            Arc::clone(&self.index_readers),
            Arc::clone(&self.index_writers),
            Arc::clone(&self.pending_messages),
        )
    }
}

impl Actor for AsyncMessageServer {
    type Context = Context<Self>;

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        let writers = Arc::clone(&self.index_writers);
        let stop = async move {
            for (hc_id, writer_arc) in writers.write().await.iter() {
                let _ = writer_arc.lock().await.commit();
                if let Some((_, id)) = self.pending_messages.read().await.get(&hc_id) {
                    let _ = Self::log_last_message(&hc_id.0, &hc_id.1, id);
                }
            }
        };
        futures::executor::block_on(stop);
        Running::Stop
    }
}

impl Handler<SearchMessageIndex> for AsyncMessageServer {
    type Result = LocalBoxFuture<'static, Result<Vec<ID>>>;

    fn handle(&mut self, msg: SearchMessageIndex, _: &mut Self::Context) -> Self::Result {
        let (indexes, index_readers, index_writers, pending_messages) = self.clone_all();
        async move {
            {
                if let Some(pending) = pending_messages
                    .read()
                    .await
                    .get(&(msg.hub_id, msg.channel_id))
                {
                    if pending.0 != 0 {
                        let _ = Self::get_writer(
                            &indexes,
                            &index_readers,
                            &index_writers,
                            &msg.hub_id,
                            &msg.channel_id,
                        )
                        .await?
                        .lock()
                        .await
                        .commit();
                        Self::log_last_message(&msg.hub_id, &msg.channel_id, &pending.1).await?;

                        pending_messages.write().await.insert(
                            (msg.hub_id.clone(), msg.channel_id.clone()),
                            (0, pending.1.clone()),
                        );
                    }
                }
            }
            let searcher = Self::get_searcher(
                &indexes,
                &index_readers,
                &index_writers,
                &msg.hub_id,
                &msg.channel_id,
            )
            .await?;
            let query_parser = QueryParser::for_index(
                searcher.index(),
                vec![MESSAGE_SCHEMA_FIELDS.content.clone()],
            );
            let query = query_parser
                .parse_query(&msg.query)
                .map_err(|_| IndexError::ParseQuery)?;
            let top_docs = searcher
                .search(&query, &TopDocs::with_limit(msg.limit))
                .map_err(|_| IndexError::Search)?;
            let mut result = Vec::new();
            for (_score, doc_address) in top_docs {
                let retrieved_doc = searcher.doc(doc_address).map_err(|_| IndexError::GetDoc)?;
                if let Some(value) = retrieved_doc.get_first(MESSAGE_SCHEMA_FIELDS.id.clone()) {
                    if let Some(bytes) = value.bytes_value() {
                        if let Ok(id) = bincode::deserialize::<ID>(bytes) {
                            result.push(id);
                        }
                    }
                }
            }
            Ok(result)
        }
        .boxed_local()
    }
}

impl Handler<NewMessageForIndex> for AsyncMessageServer {
    type Result = LocalBoxFuture<'static, Result>;

    fn handle(&mut self, msg: NewMessageForIndex, _: &mut Self::Context) -> Self::Result {
        let (indexes, index_readers, index_writers, pending_messages) = self.clone_all();
        async move {
            let writer_arc = Self::get_writer(
                &indexes,
                &index_readers,
                &index_writers,
                &msg.hub_id,
                &msg.channel_id,
            )
            .await?;
            let mut writer = writer_arc.lock().await;
            let message_id = msg.message.id.clone();
            add_message_to_writer(&mut writer, msg.message)?;
            let mut new_pending: u8;
            if let Some((pending, _)) = pending_messages
                .read()
                .await
                .get(&(msg.hub_id, msg.channel_id))
            {
                new_pending = pending + 1;
                if pending >= &crate::TANTIVY_COMMIT_THRESHOLD {
                    if let Ok(_) = writer.commit() {
                        Self::log_last_message(&msg.hub_id, &msg.channel_id, &message_id).await?;
                        new_pending = 0;
                    } else {
                        Err(IndexError::Commit)?
                    }
                } else {
                    Self::log_if_nologs(&msg.hub_id, &msg.channel_id, &message_id).await?;
                }
            } else {
                new_pending = 1;
                Self::log_if_nologs(&msg.hub_id, &msg.channel_id, &message_id).await?;
            }
            drop(writer);
            let _ = pending_messages
                .write()
                .await
                .insert((msg.hub_id, msg.channel_id), (new_pending, message_id));
            Ok(())
        }
        .boxed_local()
    }
}

pub type SubscribedChannelMap =
    Arc<RwLock<HashMap<(ID, ID), Arc<RwLock<HashSet<Recipient<ServerMessage>>>>>>>;
pub type SubscribedHubMap =
    Arc<RwLock<HashMap<ID, Arc<RwLock<HashSet<Recipient<ServerMessage>>>>>>>;
pub type SubscribedMap =
    Arc<RwLock<HashMap<Recipient<ServerMessage>, Arc<RwLock<(HashSet<(ID, ID)>, HashSet<ID>)>>>>>;

/// Server that handles socket clients and manages notifying them of new messages/changes as well as sending messages to be indexed by Tantivy.
pub struct AsyncServer {
    subscribed_channels: SubscribedChannelMap,
    subscribed_hubs: SubscribedHubMap,
    subscribed: SubscribedMap,
    message_server: Addr<AsyncMessageServer>,
}

impl AsyncServer {
    /// Creates a new server with default options, also creates a [`MessageServer`] with the given `commit_threshold` (how many messages should be added to the search index before commiting to the index).
    pub fn new() -> Self {
        Self {
            subscribed_channels: Arc::new(RwLock::new(HashMap::new())),
            subscribed_hubs: Arc::new(RwLock::new(HashMap::new())),
            subscribed: Arc::new(RwLock::new(HashMap::new())),
            message_server: AsyncMessageServer::new().start(),
        }
    }

    fn clone_all(&self) -> (SubscribedChannelMap, SubscribedHubMap, SubscribedMap) {
        (
            Arc::clone(&self.subscribed_channels),
            Arc::clone(&self.subscribed_hubs),
            Arc::clone(&self.subscribed),
        )
    }

    fn clone_hub(&self) -> (SubscribedHubMap, SubscribedMap) {
        (
            Arc::clone(&self.subscribed_hubs),
            Arc::clone(&self.subscribed),
        )
    }

    fn clone_hub_channel(&self) -> (SubscribedHubMap, SubscribedChannelMap) {
        (
            Arc::clone(&self.subscribed_hubs),
            Arc::clone(&self.subscribed_channels),
        )
    }

    fn clone_channel(&self) -> (SubscribedChannelMap, SubscribedMap) {
        (
            Arc::clone(&self.subscribed_channels),
            Arc::clone(&self.subscribed),
        )
    }

    /// Sends a [`ServreMessage`] to all clients subscribed to notifications for the given hub.
    async fn send_hub(subscribed_hubs: SubscribedHubMap, message: ServerMessage, hub_id: &ID) {
        if let Some(subscribed_arc) = subscribed_hubs.read().await.get(hub_id) {
            for connection in subscribed_arc.read().await.iter() {
                let _ = connection.do_send(message.clone());
            }
        }
    }

    /// Sends a [`ServreMessage`] to all clients subscribed to notifications for the given channel.
    async fn send_channel(
        subscribed_channels: SubscribedChannelMap,
        message: ServerMessage,
        hub_id: ID,
        channel_id: ID,
    ) {
        if let Some(subscribed_arc) = subscribed_channels.read().await.get(&(hub_id, channel_id)) {
            for connection in subscribed_arc.read().await.iter() {
                let _ = connection.do_send(message.clone());
            }
        }
    }
}

impl Actor for AsyncServer {
    type Context = Context<Self>;
}

impl Handler<client_command::Disconnect> for AsyncServer {
    type Result = LocalBoxFuture<'static, ()>;

    fn handle(&mut self, msg: client_command::Disconnect, _: &mut Self::Context) -> Self::Result {
        let (subscribed_channels, subscribed_hubs, subscribed) = self.clone_all();
        async move {
            if let Some(subscribed) = subscribed.write().await.remove(&msg.addr) {
                let subscribed = subscribed.write().await;
                let subscribed_channels = subscribed_channels.write().await;
                for channel in subscribed.0.iter() {
                    if let Some(subs) = subscribed_channels.get(&channel) {
                        subs.write().await.remove(&msg.addr);
                    }
                }
                drop(subscribed_channels);
                let subscribed_hubs = subscribed_hubs.write().await;
                for hub in subscribed.1.iter() {
                    if let Some(subs) = subscribed_hubs.get(&hub) {
                        subs.write().await.remove(&msg.addr);
                    }
                }
            }
        }
        .boxed_local()
    }
}

impl Handler<client_command::SubscribeHub> for AsyncServer {
    type Result = LocalBoxFuture<'static, Result>;

    fn handle(&mut self, msg: client_command::SubscribeHub, _: &mut Self::Context) -> Self::Result {
        let (subscribed_hubs, subscribed) = self.clone_hub();
        async move {
            Hub::load(&msg.hub_id)
                .await
                .and_then(|hub| hub.get_member(&msg.user_id))?;
            subscribed
                .write()
                .await
                .entry(msg.addr.clone())
                .or_default()
                .write()
                .await
                .1
                .insert(msg.hub_id.clone());
            subscribed_hubs
                .write()
                .await
                .entry(msg.hub_id)
                .or_default()
                .write()
                .await
                .insert(msg.addr);
            Ok(())
        }
        .boxed_local()
    }
}

impl Handler<client_command::UnsubscribeHub> for AsyncServer {
    type Result = LocalBoxFuture<'static, ()>;

    fn handle(
        &mut self,
        msg: client_command::UnsubscribeHub,
        _: &mut Self::Context,
    ) -> Self::Result {
        let (subscribed_hubs, subscribed) = self.clone_hub();
        async move {
            if let Some(subs) = subscribed.write().await.get(&msg.addr) {
                subs.write().await.1.remove(&msg.hub_id);
            }
            if let Some(subs) = subscribed_hubs.write().await.get(&msg.hub_id) {
                subs.write().await.remove(&msg.addr);
            }
        }
        .boxed_local()
    }
}

impl Handler<client_command::SubscribeChannel> for AsyncServer {
    type Result = LocalBoxFuture<'static, Result>;

    fn handle(
        &mut self,
        msg: client_command::SubscribeChannel,
        _: &mut Self::Context,
    ) -> Self::Result {
        let (subscibed_channels, subscribed) = self.clone_channel();
        async move {
            Hub::load(&msg.hub_id)
                .await
                .and_then(|hub| {
                    if let Ok(member) = hub.get_member(&msg.user_id) {
                        Ok((hub, member))
                    } else {
                        Err(Error::MemberNotFound)
                    }
                })
                .and_then(|(hub, user)| {
                    check_permission!(
                        user,
                        &msg.channel_id,
                        crate::permission::ChannelPermission::Read,
                        hub
                    );
                    Ok(())
                })?;
            let key = (msg.hub_id, msg.channel_id);
            subscribed
                .write()
                .await
                .entry(msg.addr.clone())
                .or_default()
                .write()
                .await
                .0
                .insert(key.clone());
            subscibed_channels
                .write()
                .await
                .entry(key)
                .or_default()
                .write()
                .await
                .insert(msg.addr);
            Ok(())
        }
        .boxed_local()
    }
}

impl Handler<client_command::UnsubscribeChannel> for AsyncServer {
    type Result = LocalBoxFuture<'static, ()>;

    fn handle(
        &mut self,
        msg: client_command::UnsubscribeChannel,
        _: &mut Self::Context,
    ) -> Self::Result {
        let (subscribed_channels, subscribed) = self.clone_channel();
        async move {
            let key = (msg.hub_id, msg.channel_id);
            if let Some(subs) = subscribed.write().await.get(&msg.addr) {
                subs.write().await.0.remove(&key);
            }
            if let Some(subs) = subscribed_channels.write().await.get(&key) {
                subs.write().await.remove(&msg.addr);
            }
        }
        .boxed_local()
    }
}

impl Handler<client_command::StartTyping> for AsyncServer {
    type Result = LocalBoxFuture<'static, Result>;

    fn handle(&mut self, msg: client_command::StartTyping, _: &mut Self::Context) -> Self::Result {
        let subscribed_channels = Arc::clone(&self.subscribed_channels);
        async move {
            Hub::load(&msg.hub_id)
                .await
                .and_then(|hub| {
                    if let Ok(member) = hub.get_member(&msg.user_id) {
                        Ok((hub, member))
                    } else {
                        Err(Error::MemberNotFound)
                    }
                })
                .and_then(|(hub, user)| {
                    check_permission!(
                        user,
                        &msg.channel_id,
                        crate::permission::ChannelPermission::Write,
                        hub
                    );
                    Ok(())
                })?;
            Self::send_channel(
                subscribed_channels,
                ServerMessage::TypingStart(msg.user_id, msg.hub_id.clone(), msg.channel_id.clone()),
                msg.hub_id,
                msg.channel_id,
            )
            .await;
            Ok(())
        }
        .boxed_local()
    }
}

impl Handler<client_command::StopTyping> for AsyncServer {
    type Result = LocalBoxFuture<'static, Result>;

    fn handle(&mut self, msg: client_command::StopTyping, _: &mut Self::Context) -> Self::Result {
        let subscribed_channels = Arc::clone(&self.subscribed_channels);
        async move {
            Hub::load(&msg.hub_id)
                .await
                .and_then(|hub| {
                    if let Ok(member) = hub.get_member(&msg.user_id) {
                        Ok((hub, member))
                    } else {
                        Err(Error::MemberNotFound)
                    }
                })
                .and_then(|(hub, user)| {
                    check_permission!(
                        user,
                        &msg.channel_id,
                        crate::permission::ChannelPermission::Write,
                        hub
                    );
                    Ok(())
                })?;
            Self::send_channel(
                subscribed_channels,
                ServerMessage::TypingStop(msg.user_id, msg.hub_id.clone(), msg.channel_id.clone()),
                msg.hub_id,
                msg.channel_id,
            )
            .await;
            Ok(())
        }
        .boxed_local()
    }
}

impl Handler<client_command::SendMessage> for AsyncServer {
    type Result = LocalBoxFuture<'static, Result<ID>>;

    fn handle(&mut self, msg: client_command::SendMessage, _: &mut Self::Context) -> Self::Result {
        let subscribed_channels = Arc::clone(&self.subscribed_channels);
        async move {
            Hub::load(&msg.hub_id)
                .await
                .and_then(|hub| {
                    if let Ok(member) = hub.get_member(&msg.user_id) {
                        Ok((hub, member))
                    } else {
                        Err(Error::MemberNotFound)
                    }
                })
                .and_then(|(hub, user)| {
                    check_permission!(
                        user,
                        &msg.channel_id,
                        crate::permission::ChannelPermission::Write,
                        hub
                    );
                    Ok(())
                })?;
            let message =
                api::send_message(&msg.user_id, &msg.hub_id, &msg.channel_id, msg.message).await?;
            let message_id = message.id.clone();
            Self::send_channel(
                subscribed_channels,
                ServerMessage::NewMessage(msg.hub_id.clone(), msg.channel_id.clone(), message),
                msg.hub_id,
                msg.channel_id,
            )
            .await;
            Ok(message_id)
        }
        .boxed_local()
    }
}

impl Handler<ServerNotification> for AsyncServer {
    type Result = LocalBoxFuture<'static, ()>;

    fn handle(&mut self, msg: ServerNotification, _: &mut Self::Context) -> Self::Result {
        let (subscribed_hubs, subscribed_channels) = self.clone_hub_channel();
        let message_server = self.message_server.clone();
        async move {
            match msg {
                ServerNotification::NewMessage(hub_id, channel_id, message) => {
                    let message_server = message_server.recipient();
                    let m = message.clone();
                    let _ = message_server
                        .send(NewMessageForIndex {
                            hub_id: hub_id.clone(),
                            channel_id: channel_id.clone(),
                            message: message.clone(),
                        })
                        .await;
                    Self::send_channel(
                        subscribed_channels,
                        ServerMessage::NewMessage(hub_id, channel_id, m),
                        hub_id,
                        channel_id,
                    )
                    .await
                }
                ServerNotification::HubUpdated(hub_id, update_type) => {
                    Self::send_hub(
                        subscribed_hubs,
                        ServerMessage::HubUpdated(hub_id.clone(), update_type),
                        &hub_id,
                    )
                    .await
                }
            }
        }
        .boxed_local()
    }
}

impl Handler<AsyncGetMessageServer> for AsyncServer {
    type Result = Addr<AsyncMessageServer>;

    fn handle(&mut self, _: AsyncGetMessageServer, _: &mut Self::Context) -> Self::Result {
        self.message_server.clone()
    }
}

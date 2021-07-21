use async_std::{
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{Mutex, RwLock},
    task::{sleep, spawn},
};
use async_tungstenite::{
    accept_async, client_async,
    tungstenite::{error::Error as WsError, Message},
    WebSocketStream,
};
use bincode::Options;
use dashmap::DashMap;
use futures::{channel::oneshot, future::BoxFuture, prelude::*};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};
use tracing_unwrap::ResultExt as RXT;

use std::{
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use super::BoxedFuture;
use crate::networking::{
    CouldNotDeliver, ExecutorError, FailedToBindListener, NetworkError, NetworkingImplementation,
    NoSocketsError, SocketDecodeError, WError,
};
use crate::PubKey;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
/// Represents a network message
pub enum Command<T> {
    /// A message that was broadcast to all nodes
    Broadcast {
        /// Message being sent
        inner: T,
        /// Who is sending it
        from: PubKey,
        /// Message ID
        id: u64,
    },
    /// A message that was sent directly to this node
    Direct {
        /// Message being sent
        inner: T,
        /// Who is sending it
        from: PubKey,
        /// Who its being sent to
        to: PubKey,
        /// Message ID
        id: u64,
    },
    /// A message identifying the sending node
    Identify {
        /// Who the message is from
        from: PubKey,
        /// Message ID
        id: u64,
    },
    /// Ping keepalive message
    Ping {
        /// Message ID
        id: u64,
    },
    /// Acknowledge
    Ack {
        /// Message being acknowledged
        ack_id: u64,
        /// Message ID
        id: u64,
    },
}

impl<T> Command<T> {
    /// Returns the id of this `Command`
    pub fn id(&self) -> u64 {
        match self {
            Command::Broadcast { id, .. }
            | Command::Direct { id, .. }
            | Command::Identify { id, .. }
            | Command::Ping { id, .. }
            | Command::Ack { id, .. } => *id,
        }
    }
}

/// The handle used for interacting with a `WNetwork` connection
#[derive(Clone)]
struct Handle<T> {
    /// Messages to be sent by this node
    outbound: flume::Sender<Command<T>>,
    /// The address of the remote
    remote_socket: SocketAddr,
    /// Indicate that the handle should be closed
    shutdown: Arc<RwLock<bool>>,
    /// The last time the remote sent us a message
    last_message: Arc<Mutex<Instant>>,
}

/// The inner shared state of a `WNetwork` instance
struct WNetworkInner<T> {
    /// The handles for each known `PubKey`
    handles: DashMap<PubKey, Handle<T>>,
    /// The `PubKey` of this node
    pub_key: PubKey,
    /// The global message counter
    counter: Arc<AtomicU64>,
    /// The `SocketAddr` that this `WNetwork` listens on
    socket: SocketAddr,
    /// The currently pending `Waiters`
    waiters: Waiters,
    /// The inputs to the internal queues
    inputs: Inputs<T>,
    /// The outputs to the internal queues
    outputs: Outputs<T>,
    /// Keeps track of if the tasks have been started
    tasks_started: AtomicBool,
    /// Holds onto to a TCP socket between binding and task start
    socket_holder: Mutex<Option<TcpListener>>,
    /// Duration in between keepalive pings
    keep_alive_duration: Duration,
}

/// Shared waiting state for a `WNetwork` instance
struct Waiters {
    /// Waiting on a message to be delivered
    delivered: DashMap<u64, oneshot::Sender<()>>,
    /// Waiting on a message to be acked
    acked: DashMap<u64, oneshot::Sender<()>>,
}

/// Holds onto the input queues for a `WNetwork`
#[derive(Clone)]
struct Inputs<T> {
    /// Input to broadcast queue
    broadcast: flume::Sender<T>,
    /// Input to direct queue
    direct: flume::Sender<T>,
}

/// Holds onto the output queues for a `WNetwork`
#[derive(Clone)]
struct Outputs<T> {
    /// Output from broadcast queue
    broadcast: flume::Receiver<T>,
    /// Output from direct queue
    direct: flume::Receiver<T>,
}

/// Internal enum for combining message and command streams
enum Combo<T> {
    /// Inbound message
    Message(Message),
    /// Outbound command
    Command(Command<T>),
    /// Error
    Error(WsError),
}

#[derive(Clone)]
/// Handle to the underlying networking implementation
pub struct WNetwork<T> {
    /// Pointer to the internal state of this `WNetwork`
    inner: Arc<WNetworkInner<T>>,
}

impl<T: Clone + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug + 'static>
    WNetwork<T>
{
    /// Processes an individual `Command`
    #[instrument(
        name="WNetworking::process_command",
        skip(self,inputs),
        fields(node_id = ?self.inner.pub_key.nonce, own_socket = ?self.inner.socket)
    )]
    async fn process_command(
        &self,
        command: Command<T>,
        inputs: &Inputs<T>,
    ) -> Result<Option<Command<T>>, NetworkError> {
        trace!("Processing command");
        match command {
            Command::Broadcast { inner, .. } => {
                debug!(?inner, "Broadcast");
                let res = inputs.broadcast.send_async(inner).await;
                match res {
                    Ok(_) => Ok(None),
                    Err(_) => Err(NetworkError::ChannelSend),
                }
            }
            Command::Direct { inner, .. } => {
                debug!(?inner, "Broadcast");
                let res = inputs.direct.send_async(inner).await;
                match res {
                    Ok(_) => Ok(None),
                    Err(_) => Err(NetworkError::ChannelSend),
                }
            }
            Command::Ack { ack_id, .. } => {
                debug!(?ack_id, "Got an ack");
                let waiter = &self.inner.waiters.acked;
                let waiter = waiter.remove(&ack_id);
                match waiter {
                    Some(c) => {
                        trace!("Signaling waiter for ack");
                        let _res = c.1.send(());
                        Ok(None)
                    }
                    None => Ok(None),
                }
            }
            // Identify and Ping commands require special handling inside the task, since they
            // require an ack, and an identify command requires piping the information back out
            m => Ok(Some(m)),
        }
    }

    /// Atomically increments the message counter and returns the previous value
    fn get_next_message_id(&self) -> u64 {
        self.inner.counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Spawns the task for handling a connection to a node
    #[allow(clippy::too_many_lines)]
    #[instrument(
        name="WNetwork::spawn_task",
        skip(self, stream),
        fields(node_id = ?self.inner.pub_key.nonce, own_socket = ?self.inner.socket)
    )]
    async fn spawn_task(
        &self,
        key: Option<PubKey>,
        mut stream: WebSocketStream<TcpStream>,
        remote_socket: SocketAddr,
    ) -> Result<(PubKey, Handle<T>), NetworkError> {
        info!("Spawning task to handle connection");
        let (s_outbound, r_outbound) = flume::bounded(128);
        trace!("Opened channels");
        let shutdown = Arc::new(RwLock::new(false));
        let last_message = Arc::new(Mutex::new(Instant::now()));
        let handle = Handle {
            outbound: s_outbound,
            remote_socket,
            shutdown: shutdown.clone(),
            last_message: last_message.clone(),
        };
        // For the wire format, we use bincode with the following options:
        //   - Limit of 16KiB per message
        //   - Litte endian encoding
        //   - Varint encoding
        //   - Reject trailing bytes
        let bincode_options = bincode::DefaultOptions::new().with_limit(16_384);
        let w = self.clone();
        let inputs = w.inner.inputs.clone();
        let (pk_s, pk_r) = oneshot::channel();
        let mut pk_s = Some(pk_s);
        // Identify before spawning task
        let waiter_ident = if key.is_some() {
            debug!("Identifying");
            let ident_id = w.get_next_message_id();
            let (s, r) = oneshot::channel();
            self.inner.waiters.acked.insert(ident_id, s);
            let command = Command::<T>::Identify {
                from: w.inner.pub_key.clone(),
                id: ident_id,
            };
            // Unwrap is safe, as this serialization can't fail
            let bytes = bincode_options.serialize(&command).unwrap();
            let res = stream.send(Message::Binary(bytes)).await;
            if res.is_err() {
                error!("Failed to ident, closing stream");
                *shutdown.write().await = true;
                return Err(NetworkError::IdentityHandshake);
            }
            trace!("Ident successful");
            Some(r)
        } else {
            None
        };
        spawn(async move {
            trace!("Entering setup");
            let (mut ws_sink, ws_stream) = stream.split();
            let ws_stream = ws_stream.map(|x| match x {
                Ok(x) => Combo::Message(x),
                Err(x) => Combo::Error(x),
            });
            let ob_stream =  r_outbound.stream().map(Combo::Command);
            let mut combined_stream = futures::stream::select(ws_stream,ob_stream);
            debug!("Entering processing loop");
            while let Some(m) = combined_stream.next().await {
                // Check for shutdown signal
                if *shutdown.read().await {
                    info!("Received shutdown");
                    break;
                }
                match m {
                    Combo::Message(m) => {
                        trace!(?m, "Incoming websockets message");
                        // Update the message timer
                        // Do this inside a block to make sure the lock doesn't leak
                        {
                            let mut lock = last_message.lock().await;
                            *lock = Instant::now();
                        }
                        // Attempt to decode the message
                        match m {
                            Message::Binary(vec) => {
                                trace!(?vec, "Attempting to decode binary message");
                                let res: Result<Command<T>, _> = bincode_options.deserialize(&vec);
                                match res {
                                    Ok(command) => {
                                        match w.process_command(command, &inputs).await {
                                            Ok(Some(command)) => match command {
                                                Command::Identify { from, id } => {
                                                    debug!("Identity received");
                                                    // Identifying twice isn't an error, but repeated
                                                    // identifies are ignored
                                                    let pk_s = pk_s.take();
                                                    if let Some(pk_s) = pk_s {
                                                        if pk_s.send(from).is_err() {
                                                            error!("Listener is gone, closing stream");
                                                            *shutdown.write().await = true;
                                                            break;
                                                        }
                                                    }
                                                    trace!("Acking identify");
                                                    let command =
                                                        Command::<T>::Ack{
                                                            ack_id: id,
                                                            id: w.get_next_message_id()
                                                        };
                                                    // Unwrap is safe, as this serialization can't fail
                                                    let bytes = bincode_options
                                                        .serialize(&command)
                                                        .unwrap();
                                                    let res = ws_sink.send(Message::Binary(bytes)).await;
                                                    if res.is_err() {
                                                        error!("Failed to ack, closing stream");
                                                        *shutdown.write().await = true;
                                                        break;
                                                    }
                                                },
                                                Command::Ping { id } => {
                                                    debug!("Received ping, acking");
                                                    let command =
                                                        Command::<T>::Ack{
                                                            ack_id: id,
                                                            id: w.get_next_message_id()
                                                        };
                                                    // Unwrap is safe, as this serialization can't fail
                                                    let bytes = bincode_options
                                                        .serialize(&command)
                                                        .unwrap();
                                                    let res = ws_sink.send(Message::Binary(bytes)).await;
                                                    if res.is_err() {
                                                        error!("Failed to ack, closing stream");
                                                        *shutdown.write().await = true;
                                                        break;
                                                    }
                                                },
                                                _ => {
                                                    error!("Command was invalidly passed to us");
                                                    error!("In an invalid state, closing stream.");
                                                    *shutdown.write().await = true;
                                                    break;
                                                }
                                            },
                                            Ok(None) => trace!("Processed command"),
                                            Err(e) => warn!(?e, "Error processing command, skipping"),
                                        }
                                    },
                                    Err(e) => warn!(?vec,?e, "Error deserializing message, skipping"),
                                }
                            },
                            Message::Close(c) => {
                                // Log and close
                                info!(?c, "Received close message, closing stream.");
                                *shutdown.write().await = true;
                                break;
                            },
                            m => warn!(?m, "Received unsupported message type, ignoring")
                        }
                    },
                    Combo::Command(c) => {
                        trace!(?c, "Sending command");
                        // serializing
                        let bytes = bincode_options
                            .serialize(&c)
                            .expect_or_log("Failed to serialize a command. Having types that can fail serialization is not supported.");
                        // Sending down the pipe
                        trace!("Sending serialized command");
                        let res = ws_sink.send(Message::Binary(bytes)).await;
                        match res {
                            Ok(_) => {
                                // Log and notify the water if there is any
                                trace!("Message fed to stream");
                                let waiter = &w.inner.waiters.delivered;
                                if waiter.contains_key(&c.id()) {
                                    // Unwrap is safe, as we just verified the key exists
                                    let (_, oneshot) = waiter.remove(&c.id()).unwrap();
                                    let res = oneshot.send(());
                                    if res.is_err() {
                                        warn!("Failed to message waiter for message {}", c.id());
                                    }
                                }
                            },
                            Err(e) => {
                                // log error and shutdown
                                error!(?e, "Error sending message to remote, closing stream.");
                                *shutdown.write().await = true;
                                break;
                            },
                        }
                    },
                    Combo::Error(e) => {
                        // log the error and close the stream
                        error!(?e, "A websockets error occurred! Closing stream.");
                        // Note the shutdown status and break
                        *shutdown.write().await = true;
                        break;
                    },
                }
            }
        }.instrument(tracing::info_span!("Background Stream Handler",
                                      self.node_id = self.inner.pub_key.nonce,
                                      self.socket = ?self.inner.socket,
                                      other.node_id = ?key,
                                         other.socket = ?remote_socket)));
        trace!("Task spawned");

        if let Some(pk) = key {
            if let Some(waiter_ident) = waiter_ident {
                trace!("Waiting for remote to ack the ident");
                waiter_ident.await.unwrap();
                trace!("Remote acked");
            }
            Ok((pk, handle))
        } else {
            let pk = pk_r.await.map_err(|_| NetworkError::IdentityHandshake)?;
            Ok((pk, handle))
        }
    }
    /// Creates a connection to the given node.
    ///
    /// If the connection does not succeed immediately, pause and retry. Use `connection_table_size()`
    /// to get the number of connected nodes.
    ///
    /// # Errors
    ///
    /// Will error if an underlying networking error occurs
    #[instrument(name = "WNetwork::connect_to", skip(self), err)]
    pub async fn connect_to(
        &self,
        key: PubKey,
        addr: impl ToSocketAddrs + Debug,
    ) -> Result<(), NetworkError> {
        /*
        First check to see if we have the node in the map
        */
        if self.inner.handles.contains_key(&key) {
            debug!(?key, "Already have a connection to node");
            Ok(())
        } else {
            let socket = TcpStream::connect(addr).await.context(ExecutorError)?;
            let addr = socket.peer_addr().context(SocketDecodeError {
                input: "connect_to",
            })?;
            info!(?addr, "Connecting to remote with decoded address");
            let url = format!("ws://{}", addr);
            trace!(?url);
            let (web_socket, _) = client_async(url, socket).await.context(WError)?;
            trace!("Websocket connection created");
            let (pub_key, handle) = self.spawn_task(Some(key), web_socket, addr).await?;
            trace!("Task created");
            self.inner.handles.insert(pub_key, handle);
            trace!("Handle noted");
            Ok(())
        }
    }
    /// Sends a raw message to the specified node
    ///
    /// # Errors
    ///
    /// Will error if an underlying network error occurs
    #[instrument(level = "trace", name = "WNetwork::send_raw_message", err, skip(self))]
    async fn send_raw_message(
        &self,
        node: &PubKey,
        message: Command<T>,
    ) -> Result<(), NetworkError> {
        let handle = &self.inner.handles.get(node);
        if let Some(handle) = handle {
            let res = handle.outbound.send_async(message).await;
            match res {
                Ok(_) => Ok(()),
                Err(_) => Err(NetworkError::CouldNotDeliver),
            }
        } else {
            Err(NetworkError::NoSuchNode)
        }
    }

    /// Creates a new `WNetwork` preloaded with connections to the nodes in `node_list`
    ///
    /// # Errors
    ///
    /// Will error if an underlying networking error occurs
    #[instrument(level = "trace", name = "WNetwork::new_from_strings", err)]
    pub async fn new(
        own_key: PubKey,
        port: u16,
        keep_alive_duration: Option<Duration>,
    ) -> Result<Self, NetworkError> {
        let (s_direct, r_direct) = flume::bounded(128);
        let (s_broadcast, r_broadcast) = flume::bounded(128);
        let keep_alive_duration = keep_alive_duration.unwrap_or_else(|| Duration::from_millis(500));
        trace!("Created queues");
        let s_string = format!("localhost:{}", port);
        let s_addr = match s_string.to_socket_addrs().await {
            Ok(mut x) => x.next().context(NoSocketsError { input: s_string })?,
            Err(e) => {
                return Err(NetworkError::SocketDecodeError {
                    input: s_string,
                    source: e,
                })
            }
        };
        info!(?s_addr, "Binding socket");
        let listener = TcpListener::bind(&s_addr)
            .await
            .context(FailedToBindListener)?;
        debug!("Successfully bound socket");

        let inner = WNetworkInner {
            handles: DashMap::new(),
            pub_key: own_key,
            counter: Arc::new(AtomicU64::new(0)),
            socket: s_addr,
            waiters: Waiters {
                delivered: DashMap::new(),
                acked: DashMap::new(),
            },
            inputs: Inputs {
                broadcast: s_broadcast,
                direct: s_direct,
            },
            outputs: Outputs {
                broadcast: r_broadcast,
                direct: r_direct,
            },
            tasks_started: AtomicBool::new(false),
            socket_holder: Mutex::new(Some(listener)),
            keep_alive_duration,
        };
        let w = Self {
            inner: Arc::new(inner),
        };
        trace!("Self constructed");
        Ok(w)
    }

    /// Generates the background processing task
    ///
    /// Will only generate the task once, subsequent calls will return `None`
    ///
    /// # Panics
    ///
    /// Will panic if the
    #[instrument(skip(self,sync),fields(id = ?self.inner.pub_key.nonce))]
    pub fn generate_task(
        &self,
        sync: oneshot::Sender<()>,
    ) -> Option<Vec<BoxedFuture<Result<(), NetworkError>>>> {
        let generated = self
            .inner
            .tasks_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .unwrap_or(true);
        if generated {
            warn!("Task already generated, returning nothing");
            None
        } else {
            trace!("Creating tasks");
            let w = self.clone();
            /*
            Create the listener background task

            This task is responsible for accepting incoming connections.
            */
            let listener_future = async move {
                debug!("Launching server");
                // Unwrap is safe due to atomic guard
                let listener: TcpListener = w.inner.socket_holder.lock().await.take().unwrap();
                trace!("Acquired socket");
                let mut incoming = listener.incoming();
                // Port is open, send signal
                sync.send(())
                    .expect_or_log("Failed to send port alive sync signal");
                // Loop over inbound connections and open tasks for them
                while let Some(stream) = incoming.next().await {
                    debug!("Processing incoming connection");
                    match stream {
                        Ok(stream) => {
                            let addr = stream.peer_addr().unwrap();
                            trace!(?addr, "Connected incoming stream");
                            let ws_stream = accept_async(stream).await;
                            match ws_stream {
                                Ok(ws_stream) => {
                                    trace!(?addr, "stream accepted");
                                    let res = w.spawn_task(None, ws_stream, addr).await;
                                    match res {
                                        Ok((pub_key, handle)) => {
                                            trace!(?addr, "Spawned task for stream");
                                            w.inner.handles.insert(pub_key, handle);
                                            trace!(?addr, "Stored handle for stream");
                                        }
                                        Err(e) => error!(
                                            ?e,
                                            ?addr,
                                            "Error spawning task for incoming stream"
                                        ),
                                    }
                                }
                                Err(e) => warn!(
                                    ?e,
                                    ?addr,
                                    "Error accepting incoming connection, ignoring."
                                ),
                            }
                        }
                        Err(e) => warn!(?e, "Failed to connect incoming stream, ignoring"),
                    }
                }
                todo!()
            };
            let w = self.clone();
            /*
            Create the patrol background task

            This task is responsible for checking each task to make sure that the timeout is not exceeded,
            sending a ping, and removing the task from the pool if no response is received.
            */
            let patrol_future = async move {
                let sleep_dur = w.inner.keep_alive_duration;
                loop {
                    trace!("going to sleep");
                    // Sleep for timeout duration.
                    // We don't bother checking if we have slept the correct amount of time, since
                    // it doesn't really matter in this case. Patrolling for stale nodes _too_
                    // frequently won't really hurt.
                    sleep(sleep_dur).await;
                    debug!("Patrol task woken up");
                    // Get a copy of all the handles
                    let handles: Vec<_> = w
                        .inner
                        .handles
                        .iter()
                        .map(|x| (x.key().clone(), x.value().clone()))
                        .collect();
                    trace!("Handles collected");
                    // Get current instant
                    let now = Instant::now();
                    trace!(?now);
                    // Loop through the handles
                    for (pub_key, handle) in handles {
                        trace!("Checking handle {:?}", handle.remote_socket);
                        // Get the last message time inside a block, to make sure we don't hold the
                        // lock for longer than needed
                        let last_message_time = { *handle.last_message.lock().await };
                        let duration = now.checked_duration_since(last_message_time);
                        if let Some(duration) = duration {
                            trace!(?handle.remote_socket, "Grabbed duration");
                            if duration >= sleep_dur {
                                debug!(?handle.remote_socket, ?duration, "Remote has gone stale, pinging");
                                let w = w.clone();
                                spawn(async move {
                                    w.ping_remote(pub_key, handle).await;
                                });
                            } else {
                                trace!(?handle.remote_socket, ?duration, "Remote has recent message");
                            }
                        } else {
                            trace!(?handle.remote_socket, "Last message was after we started patrol");
                        }
                    }
                }
            };
            Some(vec![
                listener_future
                    .instrument(info_span!("WNetwork Server",
                                        id = ?self.inner.pub_key.nonce,
                                        addr = ?self.inner.socket))
                    .boxed(),
                patrol_future
                    .instrument(info_span!("WNetwork Patrol",
                                           id = ?self.inner.pub_key.nonce,
                                           addr = ?self.inner.socket
                    ))
                    .boxed(),
            ])
        }
    }
    /// Returns the size of the internal connection table
    pub async fn connection_table_size(&self) -> usize {
        self.inner.handles.len()
    }
    /// Pings a remote, removing the remote from the handles table if the ping fails
    #[instrument(skip(self,handle), fields(id = ?self.inner.pub_key.nonce))]
    async fn ping_remote(&self, remote: PubKey, handle: Handle<T>) {
        trace!("Packing up ping command");
        let id = self.get_next_message_id();
        let command = Command::Ping { id };
        trace!("Registering ack waiter");
        let (send, recv) = oneshot::channel();
        self.inner.waiters.acked.insert(id, send);
        trace!("Waiter inserted");
        let res = handle.outbound.send_async(command).await;
        if res.is_ok() {
            debug!("Ping sent to remote");
            let duration = self.inner.keep_alive_duration;
            if let Ok(Ok(_)) = async_std::future::timeout(duration, recv).await {
                debug!("Received ping from remote");
            } else {
                error!("Remote did not respond in time! Removing from node map");
                self.inner.handles.remove(&remote);
            }
        } else {
            error!("Handle has been shutdown! Removing from node map");
            self.inner.handles.remove(&remote);
        }
    }
}

impl<T: Clone + Serialize + DeserializeOwned + Send + std::fmt::Debug + Sync + 'static>
    NetworkingImplementation<T> for WNetwork<T>
{
    fn broadcast_message(&self, message: T) -> BoxFuture<'_, Result<(), super::NetworkError>> {
        async move {
            debug!(?message, "Broadcasting message");
            // Visit each handle in the map
            for x in self.inner.handles.iter() {
                // "Destruct" the RefMulti
                let (key, handle) = x.pair();
                trace!(?key, "Attempting to message remote");
                // Flag an error if this handle has shut down
                if *handle.shutdown.read().await {
                    error!(?key, "Handle to remote node shut down");
                    return Err(NetworkError::CouldNotDeliver);
                }
                // Pack up the message into a command
                let id = self.get_next_message_id();
                let command = Command::Broadcast {
                    inner: message.clone(),
                    from: self.inner.pub_key.clone(),
                    id,
                };
                trace!(?command, "Packed up command");
                // send message down pipe
                handle
                    .outbound
                    .send_async(command)
                    .await
                    .ok()
                    .context(CouldNotDeliver)?;
                trace!("Command sent to task");
            }
            Ok(())
        }
        .instrument(info_span!("WNetwork::broadcast_message",
                               self.id = ?self.inner.pub_key.nonce,))
        .boxed()
    }

    fn message_node(
        &self,
        message: T,
        recipient: PubKey,
    ) -> BoxFuture<'_, Result<(), super::NetworkError>> {
        let r_id = recipient.nonce;
        async move {
            debug!(?message, "Messaging node");
            // Attempt to locate node
            if let Some(h) = self.inner.handles.get(&recipient) {
                trace!("Handle found");
                let handle = h.value();
                // Flag an error if this handle was shut down
                if *handle.shutdown.read().await {
                    error!(?recipient, "Handle to remote node shut down");
                    return Err(NetworkError::CouldNotDeliver);
                }
                // Pack up the message into a command
                let id = self.get_next_message_id();
                let command = Command::Direct {
                    inner: message,
                    from: self.inner.pub_key.clone(),
                    to: recipient,
                    id,
                };
                trace!(?command, "Packed up command");
                // Send the message down the pipe
                handle
                    .outbound
                    .send_async(command)
                    .await
                    .ok()
                    .context(CouldNotDeliver)?;
                trace!("Command sent to task");
                Ok(())
            } else {
                error!(?message, ?recipient, "Node did not exist");
                Err(NetworkError::NoSuchNode)
            }
        }
        .instrument(info_span!("WNetwork::message_node",
                              self.id = ?self.inner.pub_key.nonce,
                              other.id = ?r_id))
        .boxed()
    }

    fn broadcast_queue(&self) -> BoxFuture<'_, Result<Vec<T>, super::NetworkError>> {
        async move {
            let mut ret = Vec::new();
            // Wait for the first message to come up
            let first = self.inner.outputs.broadcast.recv_async().await;
            if let Ok(first) = first {
                trace!(?first, "First message in broadcast queue found");
                ret.push(first);
                while let Ok(x) = self.inner.outputs.broadcast.try_recv() {
                    ret.push(x);
                }
                Ok(ret)
            } else {
                error!("The underlying WNetwork has shutdown");
                Err(NetworkError::ShutDown)
            }
        }
        .instrument(info_span!("WNetwork::broadcast_queue", self.id = ?self.inner.pub_key.nonce))
        .boxed()
    }

    fn next_broadcast(&self) -> BoxFuture<'_, Result<T, super::NetworkError>> {
        async move {
            debug!("Awaiting next broadcast");
            let x = self.inner.outputs.broadcast.recv_async().await;
            if let Ok(x) = x {
                trace!(?x, "Found Broadcast");
                Ok(x)
            } else {
                error!("The underlying WNetwork has shutdown");
                Err(NetworkError::ShutDown)
            }
        }
        .instrument(info_span!("WNetwork::next_broadcast", self.id = ?self.inner.pub_key.nonce))
        .boxed()
    }

    fn direct_queue(&self) -> BoxFuture<'_, Result<Vec<T>, super::NetworkError>> {
        async move {
            let mut ret = Vec::new();
            // Wait for the first message to come up
            let first = self.inner.outputs.direct.recv_async().await;
            if let Ok(first) = first {
                trace!(?first, "First message in direct queue found");
                ret.push(first);
                while let Ok(x) = self.inner.outputs.direct.try_recv() {
                    ret.push(x);
                }
                Ok(ret)
            } else {
                error!("The underlying WNetwork has shutdown");
                Err(NetworkError::ShutDown)
            }
        }
        .instrument(info_span!("WNetwork::direct_queue", self.id = ?self.inner.pub_key.nonce))
        .boxed()
    }

    fn next_direct(&self) -> BoxFuture<'_, Result<T, super::NetworkError>> {
        async move {
            debug!("Awaiting next direct message");
            let x = self.inner.outputs.direct.recv_async().await;
            if let Ok(x) = x {
                trace!(?x, "Found direct message");
                Ok(x)
            } else {
                error!("The underlying WNetwork has shutdown");
                Err(NetworkError::ShutDown)
            }
        }
        .instrument(info_span!("WNetwork::next_direct", self.id = ?self.inner.pub_key.nonce))
        .boxed()
    }

    fn known_nodes(&self) -> BoxFuture<'_, Vec<PubKey>> {
        async move { self.inner.handles.iter().map(|x| x.key().clone()).collect() }.boxed()
    }

    fn obj_clone(&self) -> Box<dyn NetworkingImplementation<T> + 'static> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utility::test_util::setup_logging;
    use rand::Rng;

    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Test {
        message: u64,
    }

    #[instrument]
    async fn get_wnetwork() -> (PubKey, WNetwork<Test>, u16) {
        let mut rng = rand::thread_rng();
        let nonce: u64 = rng.gen();
        debug!(?nonce, "Generating PubKey with id");
        let pub_key = PubKey::random(nonce);
        for _ in 0..10 {
            let port: u16 = rng.gen_range(3000, 8000);
            debug!(?port, "Attempting port");
            let res = WNetwork::new(pub_key.clone(), port, None).await;
            if let Ok(n) = res {
                return (pub_key, n, port);
            } else {
                warn!(?port, "Port opening failed");
            }
        }
        panic!("Failed to generate a connection");
    }

    #[instrument]
    async fn get_wnetwork_timeout(timeout: u64) -> (PubKey, WNetwork<Test>, u16) {
        let timeout = Duration::from_millis(timeout);
        let mut rng = rand::thread_rng();
        let nonce: u64 = rng.gen();
        debug!(?nonce, "Generating PubKey with id");
        let pub_key = PubKey::random(nonce);
        for _ in 0..10 {
            let port: u16 = rng.gen_range(3000, 8000);
            debug!(?port, "Attempting port");
            let res = WNetwork::new(pub_key.clone(), port, Some(timeout)).await;
            if let Ok(n) = res {
                return (pub_key, n, port);
            } else {
                warn!(?port, "Port opening failed");
            }
        }
        panic!("Failed to generate a connection");
    }

    // Generating the tasks should once and only once
    #[async_std::test]
    async fn task_only_once() {
        setup_logging();
        let (_key, network, _port) = get_wnetwork().await;
        let (sync, _r) = oneshot::channel();
        let x = network.generate_task(sync);
        let (sync, _r) = oneshot::channel();
        let y = network.generate_task(sync);
        assert!(x.is_some());
        assert!(y.is_none());
    }

    // Spawning a single WNetwork and starting the task should produce no errors
    #[async_std::test]
    async fn spawn_single() {
        setup_logging();
        let (_key, network, _port) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
    }

    // Spawning two WNetworks and connecting them should produce no errors
    #[async_std::test]
    async fn spawn_double() {
        setup_logging();
        // Spawn first wnetwork
        let (_key1, network1, _port1) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network1
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Spawn second wnetwork
        let (key2, network2, port2) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network2
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Connect 1 to 2
        let addr = format!("localhost:{}", port2);
        network1
            .connect_to(key2.clone(), &addr)
            .await
            .expect("Failed to connect nodes");
    }

    // Check to make sure direct queue works
    #[async_std::test]
    async fn direct_queue() {
        setup_logging();
        // Create some dummy messages
        let messages: Vec<Test> = (0..5).map(|x| Test { message: x }).collect();

        // Spawn first wnetwork
        let (key1, network1, _port1) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network1
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Spawn second wnetwork
        let (key2, network2, port2) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network2
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Connect 1 to 2
        let addr = format!("localhost:{}", port2);
        network1
            .connect_to(key2.clone(), &addr)
            .await
            .expect("Failed to connect nodes");

        // Test 1 -> 2
        // Send messages
        for message in &messages {
            network1
                .message_node(message.clone(), key2.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network2
                .next_direct()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);

        // Test 2 -> 1
        // Send messages
        for message in &messages {
            network2
                .message_node(message.clone(), key1.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network1
                .next_direct()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);
    }

    // Check to make sure broadcast queue works
    #[async_std::test]
    async fn broadcast_queue() {
        setup_logging();
        // Create some dummy messages
        let messages: Vec<Test> = (0..5).map(|x| Test { message: x }).collect();

        // Spawn first wnetwork
        let (_key1, network1, _port1) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network1
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Spawn second wnetwork
        let (key2, network2, port2) = get_wnetwork().await;
        let (sync, r) = oneshot::channel();
        let x = network2
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Connect 1 to 2
        let addr = format!("localhost:{}", port2);
        network1
            .connect_to(key2.clone(), &addr)
            .await
            .expect("Failed to connect nodes");

        // Test 1 -> 2
        // Send messages
        for message in &messages {
            network1
                .broadcast_message(message.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network2
                .next_broadcast()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);

        // Test 2 -> 1
        // Send messages
        for message in &messages {
            network2
                .broadcast_message(message.clone())
                .await
                .expect("Failed to message node");
        }
        let mut output = Vec::new();
        while output.len() < messages.len() {
            let message = network1
                .next_broadcast()
                .await
                .expect("Failed to receive message");
            output.push(message);
        }
        output.sort();
        // Check for equality
        assert_eq!(output, messages);
    }

    // Check to make sure the patrol task doesn't crash anything
    #[async_std::test]
    async fn patrol_task() {
        setup_logging();
        // Spawn two w_networks with a timeout of 25ms
        // Spawn first wnetwork
        let (_key1, network1, _port1) = get_wnetwork_timeout(25).await;
        let (sync, r) = oneshot::channel();
        let x = network1
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Spawn second wnetwork
        let (key2, network2, port2) = get_wnetwork_timeout(25).await;
        let (sync, r) = oneshot::channel();
        let x = network2
            .generate_task(sync)
            .expect("Failed to generate task");
        x.into_iter().for_each(|x| {
            spawn(x);
        });
        r.await.unwrap();
        // Connect 1 to 2
        let addr = format!("localhost:{}", port2);
        network1
            .connect_to(key2.clone(), &addr)
            .await
            .expect("Failed to connect nodes");
        // Wait 100ms to make sure that nothing crashes
        // Currently, the log output needs to be inspected to make sure that nothing bad happened
        sleep(Duration::from_millis(100)).await
    }
}
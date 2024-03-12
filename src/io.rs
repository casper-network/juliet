//! `juliet` IO layer
//!
//! The IO layer combines a lower-level transport like a TCP Stream with the
//! [`JulietProtocol`](crate::protocol::JulietProtocol) protocol implementation and some memory
//! buffers to provide a working high-level transport for juliet messages. It allows users of this
//! layer to send messages over multiple channels, without having to worry about frame multiplexing
//! or request limits.
//!
//! ## Usage
//!
//! Most, if not all functionality is provided by the [`IoCore<N, R, W>`] type, which is constructed
//! using an [`IoCoreBuilder`] (see [`IoCoreBuilder::new`]). Similarly to [`JulietProtocol<N>`] the
//! `N` denotes the number of predefined channels.
//!
//! ## Incoming data
//!
//! Once instantiated, the [`IoCore`] **must** have its [`IoCore::next_event`] function called
//! continuously, see its documentation for details. Doing so will also yield all incoming events
//! and data.
//!
//! ## Outgoing data
//!
//! The [`RequestHandle`] provided by [`IoCoreBuilder::build`] is used to send requests to the peer.
//! It should also be kept around even if no requests are sent, as dropping it is used to signal the
//! [`IoCore`] to close the connection.

use std::{
    collections::VecDeque,
    fmt::{self, Display, Formatter},
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use bimap::BiMap;
use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        mpsc::{self, error::TryRecvError, UnboundedReceiver, UnboundedSender},
        OwnedSemaphorePermit, Semaphore, TryAcquireError,
    },
};

use crate::{
    header::{ErrorKind, Header},
    protocol::{
        payload_is_multi_frame, CompletedRead, FrameIter, JulietProtocol, LocalProtocolViolation,
        OutgoingFrame, OutgoingMessage, ProtocolBuilder,
    },
    util::PayloadFormat,
    ChannelId, Id, Outcome,
};

/// Maximum number of bytes to pre-allocate in buffers.
const MAX_ALLOC: usize = 32 * 1024; // 32 KiB

/// An item in the outgoing queue.
///
/// Requests are not transformed into messages in the queue to conserve limited request ID space.
#[derive(Debug)]
enum QueuedItem {
    /// An outgoing request.
    Request {
        /// Channel to send it out on.
        channel: ChannelId,
        /// [`IoId`] mapped to the request.
        io_id: IoId,
        /// The requests payload.
        payload: Option<Bytes>,
        /// The semaphore permit for the request.
        permit: OwnedSemaphorePermit,
    },
    /// Cancellation of one of our own requests.
    RequestCancellation {
        /// [`IoId`] mapped to the request that should be cancelled.
        io_id: IoId,
    },
    /// Outgoing response to a received request.
    Response {
        /// Channel the original request was received on.
        channel: ChannelId,
        /// Id of the original request.
        id: Id,
        /// Payload to send along with the response.
        payload: Option<Bytes>,
    },
    /// A cancellation response.
    ResponseCancellation {
        /// Channel the original request was received on.
        channel: ChannelId,
        /// Id of the original request.
        id: Id,
    },
    /// An error.
    Error {
        /// Channel to send error on.
        channel: ChannelId,
        /// Id to send with error.
        id: Id,
        /// Error payload.
        payload: Bytes,
    },
}

impl Display for QueuedItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            QueuedItem::Request {
                channel,
                io_id,
                payload,
                permit: _,
            } => {
                write!(f, "Request {{ channel: {}, io_id: {}", channel, io_id)?;
                if let Some(payload) = payload {
                    write!(f, ", payload: {}", PayloadFormat(payload))?;
                }
                f.write_str(" }")
            }
            QueuedItem::RequestCancellation { io_id } => {
                write!(f, "RequestCancellation {{ io_id: {} }}", io_id)
            }
            QueuedItem::Response {
                channel,
                id,
                payload,
            } => {
                write!(f, "Response {{ channel: {}, id: {}", channel, id)?;
                if let Some(payload) = payload {
                    write!(f, ", payload: {}", PayloadFormat(payload))?;
                }
                f.write_str(" }")
            }
            QueuedItem::ResponseCancellation { channel, id } => {
                write!(
                    f,
                    "ResponseCancellation {{ channel: {}, id: {} }}",
                    channel, id
                )
            }
            QueuedItem::Error {
                channel,
                id,
                payload,
            } => {
                write!(
                    f,
                    "Error {{ channel: {}, id: {}, payload: {} }}",
                    channel,
                    id,
                    PayloadFormat(payload)
                )
            }
        }
    }
}

impl QueuedItem {
    /// Retrieves the payload from the queued item.
    fn into_payload(self) -> Option<Bytes> {
        match self {
            QueuedItem::Request { payload, .. } => payload,
            QueuedItem::Response { payload, .. } => payload,
            QueuedItem::RequestCancellation { .. } => None,
            QueuedItem::ResponseCancellation { .. } => None,
            QueuedItem::Error { payload, .. } => Some(payload),
        }
    }
}

/// [`IoCore`] event processing error.
///
/// A [`CoreError`] always indicates that the underlying [`IoCore`] has encountered a fatal error
/// and no further communication should take part.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Failed to read from underlying reader.
    #[error("read failed")]
    ReadFailed(#[source] io::Error),
    /// Failed to write using underlying writer.
    #[error("write failed")]
    WriteFailed(#[source] io::Error),

    /// Could not send an error in time.
    ///
    /// A limit is imposed on how long a peer may take to receive an error to avoid denial of
    /// service through receiving these very slowly.
    #[error("peer did not accept error in timely manner")]
    ErrorWriteTimeout,
    /// Remote peer will/has disconnect(ed), but sent us an error message before.
    #[error("remote peer sent error [channel {}/id {}]: {} (payload: {} bytes)",
        header.channel(),
        header.id(),
        header.error_kind(),
        data.as_ref().map(|b| b.len()).unwrap_or(0))
    ]
    RemoteReportedError {
        /// Header of the reported error.
        header: Header,
        /// The error payload, if the error kind was
        /// [`ErrorKind::Other`](crate::header::ErrorKind::Other).
        data: Option<Bytes>,
    },
    /// The remote peer violated the protocol and has been sent an error.
    #[error("error sent to peer: {0}")]
    RemoteProtocolViolation(Header),
    #[error("local protocol violation")]
    /// Local protocol violation - caller violated the crate's API.
    LocalProtocolViolation(#[from] LocalProtocolViolation),
    /// Internal error.
    ///
    /// An error occurred that should be impossible, this is indicative of a bug in this library.
    #[error("internal consistency error: {0}")]
    InternalError(&'static str),
}

/// An IO layer request ID.
///
/// Request layer IO IDs are unique across the program per request that originated from the local
/// endpoint. They are used to allow for buffering large numbers of items without exhausting the
/// pool of protocol level request IDs, which are limited to `u16`s.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IoId(u64);

impl Display for IoId {
    #[inline(always)]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

/// IO layer for the juliet protocol.
///
/// The central structure for the IO layer built on top of the juliet protocol, one instance per
/// connection. It manages incoming (`R`) and outgoing (`W`) transports, as well as a queue for
/// items to be sent.
///
/// Once instantiated, a continuous polling of [`IoCore::next_event`] is expected.
#[derive(Debug)]
pub struct IoCore<const N: usize, R, W> {
    /// The actual protocol state.
    juliet: JulietProtocol<N>,

    /// Underlying transport, reader.
    reader: R,
    /// Underlying transport, writer.
    writer: W,
    /// Read buffer for incoming data.
    buffer: BytesMut,
    /// How many bytes are required until the next parse.
    ///
    /// Used to ensure we don't attempt to parse too often.
    next_parse_at: usize,
    /// The error queued to be sent before shutting down.
    pending_error: Option<OutgoingFrame>,
    /// The maximum time allowed for a peer to receive an error.
    error_timeout: Duration,

    /// The frame in the process of being sent, which may be partially transferred already. Also
    /// indicates if the current frame is the final frame of a message.
    current_frame: Option<(OutgoingFrame, bool)>,
    /// The headers of active current multi-frame transfers.
    active_multi_frame: [Option<Header>; N],
    /// Frames waiting to be sent.
    ready_queue: VecDeque<FrameIter>,
    /// Messages that are not yet ready to be sent.
    wait_queue: [VecDeque<QueuedItem>; N],
    /// Receiver for new messages to be queued.
    receiver: UnboundedReceiver<QueuedItem>,
    /// Mapping for outgoing requests, mapping internal IDs to public ones.
    request_map: BiMap<IoId, (ChannelId, Id)>,
}

/// Shared data between handles and the core itself.
#[derive(Debug)]
#[repr(transparent)]
struct IoShared<const N: usize> {
    /// Tracks how many requests are in the wait queue.
    ///
    /// Tickets are freed once the item is in the wait queue, thus the semaphore permit count
    /// controls how many requests can be buffered in addition to those already permitted due to
    /// the protocol.
    ///
    /// The maximum number of available tickets must be >= 1 for the IO layer to function.
    buffered_requests: [Arc<Semaphore>; N],
}

/// Events produced by the IO layer.
///
/// Every event must be handled, see event details on how to do so.
#[derive(Debug)]
#[must_use]
pub enum IoEvent {
    /// A new request has been received.
    ///
    /// Eventually a received request must be handled by one of the following:
    ///
    /// * A response sent (through [`Handle::enqueue_response`]).
    /// * A response cancellation sent (through [`Handle::enqueue_response_cancellation`]).
    /// * The connection being closed, either regularly or due to an error, on either side.
    /// * The reception of an [`IoEvent::RequestCancelled`] with the same ID and channel.
    NewRequest {
        /// Channel the new request arrived on.
        channel: ChannelId,
        /// Request ID (set by peer).
        id: Id,
        /// The payload provided with the request.
        payload: Option<Bytes>,
    },
    /// A received request has been cancelled.
    RequestCancelled {
        /// Channel the original request arrived on.
        channel: ChannelId,
        /// Request ID (set by peer).
        id: Id,
    },
    /// A response has been received.
    ///
    /// For every [`IoId`] there will eventually be exactly either one
    /// [`IoEvent::ReceivedResponse`] or [`IoEvent::ReceivedCancellationResponse`], unless the
    /// connection is shutdown beforehand.
    ReceivedResponse {
        /// The local request ID for which the response was sent.
        io_id: IoId,
        /// The payload of the response.
        payload: Option<Bytes>,
    },
    /// A response cancellation has been received.
    ///
    /// Indicates the peer is not going to answer the request.
    ///
    /// For every [`IoId`] there will eventually be exactly either one
    /// [`IoEvent::ReceivedResponse`] or [`IoEvent::ReceivedCancellationResponse`], unless the
    /// connection is shutdown beforehand.
    ReceivedCancellationResponse {
        /// The local request ID which will not be answered.
        io_id: IoId,
    },
}

impl Display for IoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            IoEvent::NewRequest {
                channel,
                id,
                payload,
            } => {
                write!(f, "NewRequest {{ channel: {}, id: {}", channel, id)?;
                if let Some(ref payload) = payload {
                    write!(f, ", payload: {}", PayloadFormat(payload))?;
                }
                f.write_str(" }")
            }

            IoEvent::RequestCancelled { channel, id } => {
                write!(f, "RequestCancalled {{ channel: {}, id: {} }}", channel, id)
            }
            IoEvent::ReceivedResponse { io_id, payload } => {
                write!(f, "ReceivedResponse {{ io_id: {}", io_id)?;
                if let Some(ref payload) = payload {
                    write!(f, ", payload: {}", PayloadFormat(payload))?;
                }
                f.write_str(" }")
            }
            IoEvent::ReceivedCancellationResponse { io_id } => {
                write!(f, "ReceivedCancellationResponse {{ io_id: {} }}", io_id)
            }
        }
    }
}

/// A builder for the [`IoCore`].
#[derive(Debug)]
pub struct IoCoreBuilder<const N: usize> {
    /// The builder for the underlying protocol.
    protocol: ProtocolBuilder<N>,
    /// Number of additional requests to buffer, per channel.
    buffer_size: [usize; N],
    /// The maximum time allowed for a peer to receive an error.
    error_timeout: Duration,
}

impl<const N: usize> IoCoreBuilder<N> {
    /// Creates a new builder for an [`IoCore`].
    #[inline]
    pub const fn new(protocol: ProtocolBuilder<N>) -> Self {
        Self::with_default_buffer_size(protocol, 1)
    }

    /// Creates a new builder for an [`IoCore`], initializing all buffer sizes to the given default.
    #[inline]
    pub const fn with_default_buffer_size(
        protocol: ProtocolBuilder<N>,
        default_buffer_size: usize,
    ) -> Self {
        Self {
            protocol,
            buffer_size: [default_buffer_size; N],
            error_timeout: Duration::from_secs(10),
        }
    }

    /// Sets the wait queue buffer size for a given channel.
    ///
    /// # Panics
    ///
    /// Will panic if given an invalid channel or a size less than one.
    pub const fn buffer_size(mut self, channel: ChannelId, size: usize) -> Self {
        assert!(size > 0, "cannot have a memory buffer size of zero");

        self.buffer_size[channel.get() as usize] = size;

        self
    }

    /// Sets the maximum time a peer is allowed to take to receive an error.
    ///
    /// This is a grace time given to peers to be notified of bad behavior before the connection
    /// will be closed.
    pub const fn error_timeout(mut self, error_timeout: Duration) -> Self {
        self.error_timeout = error_timeout;
        self
    }

    /// Builds a new [`IoCore`] with a [`RequestHandle`].
    ///
    /// See [`IoCore::next_event`] for details on how to handle the core. The [`RequestHandle`] can
    /// be used to send requests.
    pub fn build<R, W>(&self, reader: R, writer: W) -> (IoCore<N, R, W>, RequestHandle<N>) {
        let (sender, receiver) = mpsc::unbounded_channel();

        let core = IoCore {
            juliet: self.protocol.build(),
            reader,
            writer,
            buffer: BytesMut::new(),
            next_parse_at: 0,
            pending_error: None,
            error_timeout: self.error_timeout,
            current_frame: None,
            active_multi_frame: [Default::default(); N],
            ready_queue: Default::default(),
            wait_queue: array_init::array_init(|_| Default::default()),
            receiver,
            request_map: Default::default(),
        };

        let shared = Arc::new(IoShared {
            buffered_requests: array_init::map_array_init(&self.buffer_size, |&sz| {
                Arc::new(Semaphore::new(sz))
            }),
        });
        let handle = RequestHandle {
            shared,
            sender,
            next_io_id: Default::default(),
        };

        (core, handle)
    }
}

impl<const N: usize, R, W> IoCore<N, R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Retrieves the next event.
    ///
    /// This is the central loop of the IO layer. It polls all underlying transports and
    /// reads/writes if data is available, until enough processing has been done to produce an
    /// [`IoEvent`]. Thus any application using the IO layer should loop over calling this function.
    ///
    /// Polling of this function must continue only until `Err(_)` or `Ok(None)` is returned,
    /// indicating that the connection should be closed or has been closed.
    pub async fn next_event(&mut self) -> Result<Option<IoEvent>, CoreError> {
        loop {
            if let Some(ref mut pending_error) = self.pending_error {
                tokio::time::timeout(self.error_timeout, self.writer.write_all_buf(pending_error))
                    .await
                    .map_err(|_elapsed| CoreError::ErrorWriteTimeout)?
                    .map_err(CoreError::WriteFailed)?;

                // We succeeded writing, clear the error.
                let peers_crime = self
                    .pending_error
                    .take()
                    .expect("pending_error should not have disappeared")
                    .header();
                return Err(CoreError::RemoteProtocolViolation(peers_crime));
            }

            if self.next_parse_at <= self.buffer.remaining() {
                // Simplify reasoning about this code.
                self.next_parse_at = 0;

                match self.juliet.process_incoming(&mut self.buffer) {
                    Outcome::Incomplete(n) => {
                        // Simply reset how many bytes we need until the next parse.
                        self.next_parse_at = self.buffer.remaining() + n.get() as usize;
                    }
                    Outcome::Fatal(err_msg) => {
                        // The remote messed up, begin shutting down due to an error.
                        #[cfg(feature = "tracing")]
                        tracing::debug!(err_msg_header=%err_msg.header(), "injecting error due to fatal outcome");
                        self.inject_error(err_msg);
                    }
                    Outcome::Success(successful_read) => {
                        // If we received a response, we may have additional capacity available to
                        // send out more requests, so we process the wait queue.
                        if let CompletedRead::ReceivedResponse { channel, .. } = &successful_read {
                            self.process_wait_queue(*channel)?;
                        }

                        // Check if we have produced an event.
                        return self.handle_completed_read(successful_read).map(Some);
                    }
                }
            }

            // TODO: Can we find something more elegant than this abomination?
            #[inline(always)]
            async fn write_all_buf_if_some<W: AsyncWrite + Unpin>(
                writer: &mut W,
                buf: Option<&mut impl Buf>,
            ) -> Result<(), io::Error> {
                if let Some(buf) = buf {
                    writer.write_all_buf(buf).await
                } else {
                    Ok(())
                }
            }

            if self.current_frame.is_none() && !self.ready_queue.is_empty() {
                self.ready_next_frame()?;
            }

            tokio::select! {
                biased;  // We actually like the bias, avoid the randomness overhead.

                write_result = write_all_buf_if_some(&mut self.writer,
                                                     self.current_frame.as_mut()
                                                        .map(|(ref mut frame, _)| frame))
                , if self.current_frame.is_some() => {

                    write_result.map_err(CoreError::WriteFailed)?;

                    // Clear `current_frame` via `Option::take` and examine what was sent.
                    if let Some((frame_sent, was_final)) = self.current_frame.take() {
                        #[cfg(feature = "tracing")]
                        tracing::trace!(frame=%frame_sent, "sent");

                        let header_sent = frame_sent.header();

                        // If we finished the active multi frame send, clear it.
                        if was_final {
                            let channel_idx = header_sent.channel().get() as usize;
                            if let Some(ref active_multi_frame) =
                                self.active_multi_frame[channel_idx] {
                                if header_sent == *active_multi_frame {
                                    self.active_multi_frame[channel_idx] = None;
                                }
                            }
                        }

                        if header_sent.is_error() {
                            // We finished sending an error frame, time to exit.
                            return Err(CoreError::RemoteProtocolViolation(header_sent));
                        }

                        // TODO: We should restrict the dirty-queue processing here a little bit
                        //       (only check when completing a multi-frame message).
                        // A message has completed sending, process the wait queue in case we have
                        // to start sending a multi-frame message like a response that was delayed
                        // only because of the one-multi-frame-per-channel restriction.
                        self.process_wait_queue(header_sent.channel())?;
                    } else {
                        #[cfg(feature = "tracing")]
                        tracing::error!("current frame should not disappear");
                    }
                }

                // Reading incoming data.
                read_result = read_until_bytesmut(&mut self.reader, &mut self.buffer, self.next_parse_at) => {
                    // Our read function will not return before `read_until_bytesmut` has completed.
                    let read_complete = read_result.map_err(CoreError::ReadFailed)?;

                    if !read_complete {
                        // Remote peer hung up.
                        return Ok(None);
                    }

                    // Fall through to start of loop, which parses data read.
                }

                // Processing locally queued things.
                incoming = self.receiver.recv() => {
                    match incoming {
                        Some(item) => {
                            self.handle_incoming_item(item)?;
                        }
                        None => {
                            // If the receiver was closed we locally shut down the connection.
                            #[cfg(feature = "tracing")]
                            tracing::info!("local shutdown");
                            return Ok(None);
                        }
                    }

                    loop {
                        match self.receiver.try_recv() {
                            Ok(item) => {
                                self.handle_incoming_item(item)?;
                            }
                            Err(TryRecvError::Disconnected) => {
                                // While processing incoming items, the last handle was closed.
                                #[cfg(feature = "tracing")]
                                tracing::info!("last local io handle closed, shutting down");
                                return Ok(None);
                            }
                            Err(TryRecvError::Empty) => {
                                // Everything processed.
                                break
                            }
                        }
                    }
                }
            }
        }
    }

    /// Ensures the next message sent is an error message.
    ///
    /// Clears all buffers related to sending and closes the local incoming channel.
    fn inject_error(&mut self, mut err_msg: OutgoingMessage) {
        // Stop accepting any new local data.
        self.receiver.close();

        // We do not continue parsing, ever again.
        self.next_parse_at = usize::MAX;

        // Clear queues and data structures that are no longer needed.
        self.buffer.clear();
        self.ready_queue.clear();
        self.request_map.clear();
        for queue in &mut self.wait_queue {
            queue.clear();
        }

        // Ensure the error message is the next frame sent, truncating as needed.
        let max_frame_size = self.juliet.max_frame_size();
        err_msg.truncate_to_single_frame(max_frame_size);
        let (frame, _remainder) = err_msg.frames().next_owned(max_frame_size);
        debug_assert!(
            _remainder.is_none(),
            "should not have more than one frame after truncating to fit into single frame"
        );
        self.pending_error = Some(frame);
    }

    /// Processes a completed read into a potential event.
    fn handle_completed_read(
        &mut self,
        completed_read: CompletedRead,
    ) -> Result<IoEvent, CoreError> {
        #[cfg(feature = "tracing")]
        tracing::debug!(%completed_read, "completed read");
        match completed_read {
            CompletedRead::ErrorReceived { header, data } => {
                // We've received an error from the peer, they will be closing the connection.
                Err(CoreError::RemoteReportedError { header, data })
            }
            CompletedRead::NewRequest {
                channel,
                id,
                payload,
            } => {
                // Requests have their id passed through, since they are not given an `IoId`.
                Ok(IoEvent::NewRequest {
                    channel,
                    id,
                    payload,
                })
            }
            CompletedRead::RequestCancellation { channel, id } => {
                Ok(IoEvent::RequestCancelled { channel, id })
            }

            // It is not our job to ensure we do not receive duplicate responses or cancellations;
            // this is taken care of by `JulietProtocol`.
            CompletedRead::ReceivedResponse {
                channel,
                id,
                payload,
            } => self
                .request_map
                .remove_by_right(&(channel, id))
                .ok_or(CoreError::InternalError(
                    "juliet protocol should have dropped response after cancellation",
                ))
                .map(move |(io_id, _)| IoEvent::ReceivedResponse { io_id, payload }),
            CompletedRead::ResponseCancellation { channel, id } => {
                // Responses are mapped to the respective `IoId`.
                self.request_map
                    .remove_by_right(&(channel, id))
                    .ok_or(CoreError::InternalError(
                        "juliet protocol should not have allowed fictitious response through",
                    ))
                    .map(|(io_id, _)| IoEvent::ReceivedCancellationResponse { io_id })
            }
        }
    }

    /// Handles a new item to send out that arrived through the incoming channel.
    fn handle_incoming_item(&mut self, item: QueuedItem) -> Result<(), LocalProtocolViolation> {
        // Check if the item is sendable immediately.
        if let Some(channel) = item_should_wait(&item, &self.juliet, &self.active_multi_frame)? {
            #[cfg(feature = "tracing")]
            tracing::debug!(%item, "postponing send");
            self.wait_queue[channel.get() as usize].push_back(item);
            return Ok(());
        }

        #[cfg(feature = "tracing")]
        tracing::debug!(%item, "ready to send");
        self.send_to_ready_queue(item)
    }

    /// Sends an item directly to the ready queue, causing it to be sent out eventually.
    fn send_to_ready_queue(&mut self, item: QueuedItem) -> Result<(), LocalProtocolViolation> {
        match item {
            QueuedItem::Request {
                io_id,
                channel,
                payload,
                permit,
            } => {
                let msg = self.juliet.create_request(channel, payload)?;
                let id = msg.header().id();
                self.request_map.insert(io_id, (channel, id));
                if msg.is_multi_frame(self.juliet.max_frame_size()) {
                    self.active_multi_frame[channel.get() as usize] = Some(msg.header());
                }
                self.ready_queue.push_back(msg.frames());

                drop(permit);
            }
            QueuedItem::RequestCancellation { io_id } => {
                if let Some((channel, id)) = self.request_map.get_by_left(&io_id) {
                    if let Some(msg) = self.juliet.cancel_request(*channel, *id)? {
                        self.ready_queue.push_back(msg.frames());
                    }
                } else {
                    // Already cancelled or answered by peer - no need to do anything.
                }
            }

            // `juliet` already tracks whether we still need to send the cancellation.
            // Unlike requests, we do not attempt to fish responses out of the queue,
            // cancelling a response after it has been created should be rare.
            QueuedItem::Response {
                id,
                channel,
                payload,
            } => {
                if let Some(msg) = self.juliet.create_response(channel, id, payload)? {
                    if msg.is_multi_frame(self.juliet.max_frame_size()) {
                        self.active_multi_frame[channel.get() as usize] = Some(msg.header());
                    }
                    self.ready_queue.push_back(msg.frames())
                }
            }
            QueuedItem::ResponseCancellation { id, channel } => {
                if let Some(msg) = self.juliet.cancel_response(channel, id)? {
                    self.ready_queue.push_back(msg.frames());
                }
            }

            // Errors go straight to the front of the line.
            QueuedItem::Error {
                id,
                channel,
                payload,
            } => {
                let err_msg = self.juliet.custom_error(channel, id, payload)?;
                self.inject_error(err_msg);
            }
        }

        Ok(())
    }

    /// Clears a potentially finished frame and returns the next frame to send.
    ///
    /// Note that there may be frames waiting that cannot be sent due them being multi-frame
    /// messages when there already is a multi-frame message in progress, or request limits are
    /// being hit.
    ///
    /// The caller needs to ensure that the current frame is empty (i.e. has been sent).
    fn ready_next_frame(&mut self) -> Result<(), LocalProtocolViolation> {
        debug_assert!(self.current_frame.is_none()); // Must be guaranteed by caller.

        // Try to fetch a frame from the ready queue.
        let (frame, additional_frames) = match self.ready_queue.pop_front() {
            Some(item) => item,
            None => return Ok(()),
        }
        .next_owned(self.juliet.max_frame_size());

        // If there are more frames after this one, schedule the remainder.
        let is_final = if let Some(next_frame_iter) = additional_frames {
            self.ready_queue.push_back(next_frame_iter);
            false
        } else {
            true
        };

        self.current_frame = Some((frame, is_final));
        Ok(())
    }

    /// Process the wait queue of a given channel, promoting messages that are ready to be sent.
    fn process_wait_queue(&mut self, channel: ChannelId) -> Result<(), LocalProtocolViolation> {
        let mut remaining = self.wait_queue[channel.get() as usize].len();

        while let Some(item) = self.wait_queue[channel.get() as usize].pop_front() {
            if item_should_wait(&item, &self.juliet, &self.active_multi_frame)?.is_some() {
                // Put it right back into the queue.
                self.wait_queue[channel.get() as usize].push_back(item);
            } else {
                self.send_to_ready_queue(item)?;

                // No need to look further if we have saturated the channel.
                if !self.juliet.allowed_to_send_request(channel)? {
                    break;
                }
            }

            // Ensure we do not loop endlessly if we cannot find anything.
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }

        Ok(())
    }
}

impl CoreError {
    /// If the error stems from a peer sending us a custom error, return its header and payload.
    #[inline(always)]
    pub fn as_other_error(&self) -> Option<(Header, &Bytes)> {
        match self {
            CoreError::RemoteReportedError { header, data }
                if header.is_error() && header.error_kind() == ErrorKind::Other =>
            {
                debug_assert!(data.is_some(), "`CoreError::RemoteReportedError` should never have no `data` for `OTHER` kind of error");
                Some((*header, data.as_ref()?))
            }
            _ => None,
        }
    }
}

/// Determines whether an item is ready to be moved from the wait queue to the ready queue.
///
/// Returns `None` if the item does not need to wait. Otherwise, the item's channel ID is returned.
fn item_should_wait<const N: usize>(
    item: &QueuedItem,
    juliet: &JulietProtocol<N>,
    active_multi_frame: &[Option<Header>; N],
) -> Result<Option<ChannelId>, LocalProtocolViolation> {
    let (payload, channel) = match item {
        QueuedItem::Request {
            channel, payload, ..
        } => {
            // Check if we cannot schedule due to the message exceeding the request limit.
            if !juliet.allowed_to_send_request(*channel)? {
                return Ok(Some(*channel));
            }

            (payload, channel)
        }
        QueuedItem::Response {
            channel, payload, ..
        } => (payload, channel),

        // Other messages are always ready.
        QueuedItem::RequestCancellation { .. }
        | QueuedItem::ResponseCancellation { .. }
        | QueuedItem::Error { .. } => return Ok(None),
    };

    let active_multi_frame = active_multi_frame[channel.get() as usize];

    // Check if we cannot schedule due to the message being multi-frame and there being a
    // multi-frame send in progress:
    if active_multi_frame.is_some() {
        if let Some(payload) = payload {
            if payload_is_multi_frame(juliet.max_frame_size(), payload.len()) {
                return Ok(Some(*channel));
            }
        }
    }

    // Otherwise, this should be a legitimate add to the run queue.
    Ok(None)
}

/// A handle to the input queue to the [`IoCore`] that allows sending requests and responses.
///
/// The handle is roughly three pointers in size and can be cloned at will. Dropping the last handle
/// will cause the [`IoCore`] to shutdown and close the connection.
///
/// ## Sending requests
///
/// To send a request, a holder of this handle must first reserve a slot in the memory buffer of the
/// [`IoCore`] using either [`RequestHandle::try_reserve_request`] or
/// [`RequestHandle::reserve_request`], then [`RequestHandle::downgrade`] this request handle to a
/// regular [`Handle`] and [`Handle::enqueue_request`] with the given [`RequestTicket`].
#[derive(Clone, Debug)]
pub struct RequestHandle<const N: usize> {
    /// Shared portion of the [`IoCore`], required for backpressuring onto clients.
    shared: Arc<IoShared<N>>,
    /// Sender for queue items.
    sender: UnboundedSender<QueuedItem>,
    /// The next generation [`IoId`].
    ///
    /// IoIDs are just generated sequentially until they run out (which at 1 billion at second
    /// takes roughly 10^22 years).
    next_io_id: Arc<AtomicU64>,
}

/// Simple [`IoCore`] handle.
///
/// Functions similarly to [`RequestHandle`], but has a no capability of creating new requests, as
/// it lacks access to the internal [`IoId`] generator.
///
/// Like [`RequestHandle`], the existance of this handle will keep [`IoCore`] alive; dropping the
/// last one will shut it down.
///
/// ## Usage
///
/// To send any sort of message, response, cancellation or error, use one of the `enqueue_*`
/// methods. The [`io`] layer does some, but not complete bookkeeping, if a complete solution is
/// required, use the [`rpc`](crate::rpc) layer instead.
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct Handle {
    /// Sender for queue items.
    sender: UnboundedSender<QueuedItem>,
}

/// An error that can occur while attempting to enqueue an item.
#[derive(Debug, Error)]
pub enum EnqueueError {
    /// The IO core was shut down, there is no connection anymore to send through.
    #[error("IO closed")]
    Closed(Option<Bytes>),
    /// The request limit for locally buffered requests was hit, try again.
    #[error("request limit hit")]
    BufferLimitHit(Option<Bytes>),
}

/// A reserved slot in the memory buffer of [`IoCore`], on a specific channel.
///
/// Dropping the ticket will free up the slot again.
#[derive(Debug)]
pub struct RequestTicket {
    /// Channel the slot is reserved in.
    channel: ChannelId,
    /// The semaphore permit that makes it work.
    permit: OwnedSemaphorePermit,
    /// Pre-allocated [`IoId`].
    io_id: IoId,
}

impl Display for RequestTicket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RequestTicket {{ channel: {}, io_id: {} }}",
            self.channel, self.io_id
        )
    }
}

/// A failure to reserve a slot in the queue.
pub enum ReservationError {
    /// No buffer space available.
    ///
    /// The caller is free to retry later.
    NoBufferSpaceAvailable,
    /// Connection closed.
    ///
    /// The [`IoCore`] has shutdown or is shutting down, it is no longer possible to reserve slots.
    Closed,
}

impl<const N: usize> RequestHandle<N> {
    /// Attempts to reserve a new request ticket.
    #[inline]
    pub fn try_reserve_request(
        &self,
        channel: ChannelId,
    ) -> Result<RequestTicket, ReservationError> {
        match self.shared.buffered_requests[channel.get() as usize]
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => Ok(RequestTicket {
                channel,
                permit,
                io_id: IoId(self.next_io_id.fetch_add(1, Ordering::Relaxed)),
            }),

            Err(TryAcquireError::Closed) => Err(ReservationError::Closed),
            Err(TryAcquireError::NoPermits) => Err(ReservationError::NoBufferSpaceAvailable),
        }
    }

    /// Reserves a new request ticket.
    #[inline]
    pub async fn reserve_request(&self, channel: ChannelId) -> Option<RequestTicket> {
        self.shared.buffered_requests[channel.get() as usize]
            .clone()
            .acquire_owned()
            .await
            .map(|permit| RequestTicket {
                channel,
                permit,
                io_id: IoId(self.next_io_id.fetch_add(1, Ordering::Relaxed)),
            })
            .ok()
    }

    /// Downgrades a [`RequestHandle`] to a [`Handle`].
    #[inline(always)]
    pub fn downgrade(self) -> Handle {
        Handle {
            sender: self.sender,
        }
    }
}

impl Handle {
    /// Enqueues a new request.
    ///
    /// Returns an [`IoId`] that can be used to refer to the request if successful. Returns the
    /// payload as an error if the underlying IO layer has been closed.
    ///
    /// See [`RequestHandle`] for details on how to obtain a [`RequestTicket`].
    #[inline]
    pub fn enqueue_request(
        &mut self,
        RequestTicket {
            channel,
            permit,
            io_id,
        }: RequestTicket,
        payload: Option<Bytes>,
    ) -> Result<IoId, Option<Bytes>> {
        // TODO: Panic if given semaphore ticket from wrong instance?

        self.sender
            .send(QueuedItem::Request {
                io_id,
                channel,
                payload,
                permit,
            })
            .map(|()| {
                #[cfg(feature = "tracing")]
                tracing::debug!(%io_id, %channel, "successfully enqueued");
            })
            .map_err(|send_err| {
                #[cfg(feature = "tracing")]
                tracing::debug!("failed to enqueue, remote closed");
                send_err.0.into_payload()
            })?;

        Ok(io_id)
    }

    /// Enqueues a response to an existing request.
    ///
    /// Callers are supposed to send only one response or cancellation per incoming request.
    pub fn enqueue_response(
        &self,
        channel: ChannelId,
        id: Id,
        payload: Option<Bytes>,
    ) -> Result<(), EnqueueError> {
        self.sender
            .send(QueuedItem::Response {
                channel,
                id,
                payload,
            })
            .map_err(|send_err| EnqueueError::Closed(send_err.0.into_payload()))
    }

    /// Enqueues a cancellation to an existing outgoing request.
    ///
    /// If the request has already been answered or cancelled, the enqueue cancellation will
    /// ultimately have no effect.
    pub fn enqueue_request_cancellation(&self, io_id: IoId) -> Result<(), EnqueueError> {
        self.sender
            .send(QueuedItem::RequestCancellation { io_id })
            .map_err(|send_err| EnqueueError::Closed(send_err.0.into_payload()))
    }

    /// Enqueues a cancellation as a response to a received request.
    ///
    /// Callers are supposed to send only one response or cancellation per incoming request.
    pub fn enqueue_response_cancellation(
        &self,
        channel: ChannelId,
        id: Id,
    ) -> Result<(), EnqueueError> {
        self.sender
            .send(QueuedItem::ResponseCancellation { id, channel })
            .map_err(|send_err| EnqueueError::Closed(send_err.0.into_payload()))
    }

    /// Enqueues an error.
    ///
    /// Enqueuing an error causes the [`IoCore`] to begin shutting down immediately, only making an
    /// effort to finish sending the error before doing so.
    ///
    /// If payload exceeds what is possible to send in a single frame, it is truncated.
    pub fn enqueue_error(
        &self,
        channel: ChannelId,
        id: Id,
        payload: Bytes,
    ) -> Result<(), EnqueueError> {
        self.sender
            .send(QueuedItem::Error {
                id,
                channel,
                payload,
            })
            .map_err(|send_err| EnqueueError::Closed(send_err.0.into_payload()))
    }
}

/// Read bytes into a buffer.
///
/// Similar to [`AsyncReadExt::read_buf`], except it performs zero or more read calls until at least
/// `target` bytes are in `buf`. Specifically, this function will
///
/// 1. Read bytes from `reader`, put them into `buf`, until there are at least `target` bytes
///    available in `buf` ready for consumption.
/// 2. Immediately retry when encountering any [`io::ErrorKind::Interrupted`] errors.
/// 3. Propagate upwards any other errors.
/// 4. Return `false` with less than `target` bytes available in `buf if the connection was closed.
/// 5. Return `true` on success, i.e. `buf` contains at least `target` bytes.
///
/// # Cancellation safety
///
/// This function is cancellation safe in the same way that [`AsyncReadExt::read_buf`] is.
async fn read_until_bytesmut<'a, R>(
    reader: &'a mut R,
    buf: &mut BytesMut,
    target: usize,
) -> io::Result<bool>
where
    R: AsyncReadExt + Sized + Unpin,
{
    let extra_required = target.saturating_sub(buf.remaining());
    // Note: `reserve` is purely an optimization -- `BufMut::remaining_mut(&mut buf)` will always
    //       return 2**64-1, which is the number `read_buf` looks at for exiting early.
    buf.reserve(extra_required.min(MAX_ALLOC));

    while buf.remaining() < target {
        match reader.read_buf(buf).await {
            Ok(0) => return Ok(false),
            Ok(_) => {
                // We read some more bytes, continue.
            }
            Err(err) if matches!(err.kind(), io::ErrorKind::Interrupted) => {
                // Ignore `Interrupted` errors, just retry.
            }
            Err(err) => return Err(err),
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use bytes::BytesMut;
    use futures::{Future, FutureExt};
    use proptest_attr_macro::proptest;
    use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

    use super::read_until_bytesmut;

    /// A reader simulating a stuttering transmission.
    #[derive(Debug, Default)]
    struct StutteringReader {
        /// Input events happening in the future.
        input: VecDeque<io::Result<Option<Box<[u8]>>>>,
    }

    impl StutteringReader {
        /// Adds a successful read to the reader.
        fn push_data<T: Into<Box<[u8]>>>(&mut self, data: T) {
            self.input.push_back(Ok(Some(data.into())));
        }

        /// Adds a delay, causing `Poll::Pending` to be returned by `AsyncRead::poll_read`.
        fn push_pause(&mut self) {
            self.input.push_back(Ok(None));
        }

        /// Adds an error to be produced by the reader.
        fn push_error(&mut self, e: io::Error) {
            self.input.push_back(Err(e))
        }

        /// Splits up a sequence of bytes into a series of reads, delays and intermittent
        /// `Interrupted` errors.
        ///
        /// Assumes that `input_sequence` is a randomized byte string, as it will be used as a
        /// source of entropy.
        fn push_randomized_sequence(&mut self, mut input_sequence: &[u8]) {
            /// Prime group order and maximum sequence length.
            const ORDER: u8 = 13;

            fn gadd(a: u8, b: u8) -> u8 {
                (a % ORDER + b % ORDER) % ORDER
            }

            // State manipulated for pseudo-randomness.
            let mut state = 5;

            while !input_sequence.is_empty() {
                // Mix in bytes from the input sequence.
                state = gadd(state, input_sequence[0]);

                // Decide what to do next:
                match state {
                    // 1/ORDER chance of a pause.
                    3 => self.push_pause(),
                    // 1/ORDER chance of an "interrupted" error.
                    7 => self.push_error(io::Error::new(io::ErrorKind::Interrupted, "interrupted")),
                    // otherwise, determine a random chunk length and add a successful read.
                    _ => {
                        // We will read 1-13 bytes.
                        let max_run_length =
                            ((input_sequence[0] % ORDER + 1) as usize).min(input_sequence.len());

                        assert!(max_run_length > 0);

                        self.push_data(&input_sequence[..max_run_length]);

                        // Remove from input sequence.
                        input_sequence = &input_sequence[max_run_length..];

                        if input_sequence.is_empty() {
                            break;
                        }
                    }
                }

                // Increment state if it would be cyclical otherwise.
                if state == gadd(state, input_sequence[0]) {
                    state = (state + 1) % ORDER;
                }
            }
        }
    }

    impl AsyncRead for StutteringReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.input.pop_front() {
                Some(Ok(Some(data))) => {
                    // Slightly slower to initialize twice, but safer. We don't need peak
                    // performance for this test code.
                    let dest = buf.initialize_unfilled();
                    let split_point = dest.len().min(data.len());

                    let (to_write, remainder) = data.split_at(split_point);
                    dest[0..split_point].copy_from_slice(to_write);
                    buf.advance(to_write.len());

                    // If we did not read the entire chunk, add back to input stream.
                    if !remainder.is_empty() {
                        self.input.push_front(Ok(Some(remainder.into())));
                    }

                    Poll::Ready(Ok(()))
                }
                Some(Ok(None)) => {
                    // Return one pending, but ensure we're woken up immediately afterwards.

                    let waker = cx.waker().clone();
                    waker.wake();

                    Poll::Pending
                }
                Some(Err(e)) => {
                    // Return the scheduled error.
                    Poll::Ready(Err(e))
                }
                None => {
                    // No data to read, the 0-byte read will be detected by the caller.

                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    #[test]
    fn stuttering_reader_reads_correctly() {
        let mut reader = StutteringReader::default();

        reader.push_data(&b"foo"[..]);
        reader.push_error(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        reader.push_data(&b"bar"[..]);
        reader.push_pause();
        reader.push_data(&b"baz"[..]);
        reader.push_pause();
        reader.push_error(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));

        let mut buf = [0u8; 1024];

        let bytes_read = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect("should not fail");

        assert_eq!(bytes_read, 3);
        assert_eq!(&buf[..3], b"foo");

        // Interrupted error.
        let interrupted_err = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect_err("should fail");
        assert_eq!(interrupted_err.to_string(), "interrupted");

        // Let's try a partial read next.

        let bytes_read = reader
            .read(&mut buf[0..2])
            .now_or_never()
            .expect("should be ready")
            .expect("should not fail");

        assert_eq!(bytes_read, 2);
        assert_eq!(&buf[..2], b"ba");

        let bytes_read = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect("should not fail");

        assert_eq!(bytes_read, 1);
        assert_eq!(&buf[..1], b"r");

        assert!(
            reader.read(&mut buf).now_or_never().is_none(),
            "expected pending read"
        );

        // The waker has been called again already, so we attempt another read.
        let bytes_read = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect("should not fail");

        assert_eq!(bytes_read, 3);
        assert_eq!(&buf[..3], b"baz");

        assert!(
            reader.read(&mut buf).now_or_never().is_none(),
            "expected pending read"
        );

        let broken_pipe_err = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect_err("should fail");
        assert_eq!(broken_pipe_err.to_string(), "broken pipe");

        // The final read should be a 0-length read.
        let bytes_read = reader
            .read(&mut buf)
            .now_or_never()
            .expect("should be ready")
            .expect("should not fail");

        assert_eq!(bytes_read, 0);
    }

    #[proptest]
    fn randomized_sequences_build_correctly(input: Vec<u8>) {
        let mut reader = StutteringReader::default();
        reader.push_randomized_sequence(&input);

        let mut output: Vec<u8> = Vec::with_capacity(input.len());
        let mut buffer = [0u8; 512];
        loop {
            match reader.read(&mut buffer).now_or_never() {
                None => {
                    // `Poll::Pending`, ignore and try again.
                }
                Some(Ok(0)) => {
                    // We are done reading.
                    break;
                }
                Some(Ok(n)) => {
                    output.extend(&buffer[..n]);
                }
                Some(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {
                    // Try again.
                }
                Some(Err(e)) => {
                    panic!("did not expect error {}", e);
                }
            }
        }

        assert_eq!(output, input);
    }

    /// Polls a future in a busy loop.
    fn poll_forever<F: Future>(mut fut: F) -> <F as Future>::Output {
        loop {
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            let fut_pinned = unsafe { Pin::new_unchecked(&mut fut) };
            match fut_pinned.poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => continue,
            }
        }
    }

    #[proptest]
    fn read_until_bytesmut_into_empty_buffer_succeeds(input: Vec<u8>) {
        // We are trying to read any sequence that is guaranteed to finish into an empty buffer:
        for n in 1..(input.len()) {
            let mut reader = StutteringReader::default();
            reader.push_randomized_sequence(&input);

            let mut buf = BytesMut::new();
            let read_successful = poll_forever(read_until_bytesmut(&mut reader, &mut buf, n))
                .expect("reading should not fail");

            assert!(read_successful);
            assert_eq!(buf[..n], input[..n]);
        }
    }

    #[proptest]
    fn read_until_bytesmut_eventually_fills_buffer(input: Vec<u8>) {
        // Given a stuttering reader with the correct amount of input available, check if we can
        // fill it going one-by-one.
        let mut reader = StutteringReader::default();
        reader.push_randomized_sequence(&input);

        let mut buf = BytesMut::new();

        for target in 0..=input.len() {
            let read_complete = poll_forever(read_until_bytesmut(&mut reader, &mut buf, target))
                .expect("reading should not fail");

            assert!(read_complete);
        }

        assert_eq!(buf.to_vec(), input);
    }

    #[proptest]
    fn read_until_bytesmut_gives_up_if_not_enough_available(input: Vec<u8>) {
        for read_past in 1..(3 * input.len()) {
            // Trying to read past a closed connection should result in `false` being returned.
            let mut reader = StutteringReader::default();
            reader.push_randomized_sequence(&input);

            let mut buf = BytesMut::new();

            let read_complete = poll_forever(read_until_bytesmut(
                &mut reader,
                &mut buf,
                input.len() + read_past,
            ))
            .expect("reading should not fail");

            assert!(!read_complete);

            // We still should find out input in `buf`.
            assert_eq!(buf.to_vec(), input);
        }
    }
}

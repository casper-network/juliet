use std::{cmp, collections::VecDeque};

use bytes::Bytes;
use tokio::sync::OwnedSemaphorePermit;
#[cfg(feature = "tracing")]
use tracing::debug;

use super::{IoId, QueuedItem};
use crate::{
    header::Header,
    protocol::{payload_is_multi_frame, JulietProtocol, LocalProtocolViolation},
    ChannelId, Id,
};

/// A single-frame request in the wait queue, converted from a `QueuedItem`, pending conversion
/// back to a `QueuedItem` when being moved to the ready queue.
#[derive(Debug)]
struct SingleFrameRequest {
    wait_index: u64,
    channel: ChannelId,
    io_id: IoId,
    payload: Option<Bytes>,
    permit: OwnedSemaphorePermit,
}

impl From<SingleFrameRequest> for QueuedItem {
    fn from(sf_req: SingleFrameRequest) -> Self {
        QueuedItem::Request {
            channel: sf_req.channel,
            io_id: sf_req.io_id,
            payload: sf_req.payload,
            permit: sf_req.permit,
        }
    }
}

/// A multi-frame request in the wait queue, converted from a `QueuedItem`, pending conversion
/// back to a `QueuedItem` when being moved to the ready queue.
#[derive(Debug)]
struct MultiFrameRequest {
    wait_index: u64,
    channel: ChannelId,
    io_id: IoId,
    payload: Option<Bytes>,
    permit: OwnedSemaphorePermit,
}

impl From<MultiFrameRequest> for QueuedItem {
    fn from(mf_req: MultiFrameRequest) -> Self {
        QueuedItem::Request {
            channel: mf_req.channel,
            io_id: mf_req.io_id,
            payload: mf_req.payload,
            permit: mf_req.permit,
        }
    }
}

/// A multi-frame response in the wait queue, converted from a `QueuedItem`, pending conversion
/// back to a `QueuedItem` when being moved to the ready queue.
#[derive(Debug)]
struct MultiFrameResponse {
    wait_index: u64,
    channel: ChannelId,
    id: Id,
    payload: Option<Bytes>,
}

impl From<MultiFrameResponse> for QueuedItem {
    fn from(mf_resp: MultiFrameResponse) -> Self {
        QueuedItem::Response {
            channel: mf_resp.channel,
            id: mf_resp.id,
            payload: mf_resp.payload,
        }
    }
}

/// The outcome of trying to push a new item to the wait queue.
pub(super) enum PushOutcome {
    Pushed,
    NotPushed(QueuedItem),
}

/// The wait queue: an ordered collection of items waiting for the ready queue to become available
/// for new items.
#[derive(Default, Debug)]
pub(super) struct WaitQueue {
    single_frame_requests: VecDeque<SingleFrameRequest>,
    multi_frame_requests: VecDeque<MultiFrameRequest>,
    multi_frame_responses: VecDeque<MultiFrameResponse>,
}

impl WaitQueue {
    /// Add the given item to the back of the wait queue.
    pub(super) fn try_push_back<const N: usize>(
        &mut self,
        item: QueuedItem,
        juliet: &JulietProtocol<N>,
        active_multi_frame: &[Option<Header>; N],
    ) -> Result<PushOutcome, LocalProtocolViolation> {
        match item {
            QueuedItem::Request {
                channel,
                io_id,
                payload,
                permit,
            } => {
                if !juliet.allowed_to_send_request(channel)? {
                    #[cfg(feature = "tracing")]
                    if payload_is_multi_frame(
                        juliet.max_frame_size(),
                        payload
                            .as_ref()
                            .map(|payld| payld.len())
                            .unwrap_or_default(),
                    ) {
                        debug!(%channel, %io_id, "multi-frame request postponed: channel full");
                    } else {
                        debug!(%channel, %io_id, "single-frame request postponed: channel full");
                    }
                    self.push_request(channel, io_id, payload, permit, juliet);
                    return Ok(PushOutcome::Pushed);
                }

                if self.has_active_multi_frame(channel, &payload, juliet, active_multi_frame) {
                    #[cfg(feature = "tracing")]
                    debug!(%channel, %io_id, "multi-frame request postponed: other in progress");
                    self.push_multi_frame_request(channel, io_id, payload, permit);
                    return Ok(PushOutcome::Pushed);
                }

                // We don't need to wait - rebuild the item and return it.
                let item = QueuedItem::Request {
                    channel,
                    io_id,
                    payload,
                    permit,
                };
                Ok(PushOutcome::NotPushed(item))
            }
            QueuedItem::Response {
                channel,
                id,
                payload,
            } => {
                if self.has_active_multi_frame(channel, &payload, juliet, active_multi_frame) {
                    #[cfg(feature = "tracing")]
                    debug!(%channel, %id, "multi-frame response postponed: other in progress");
                    self.push_response(channel, id, payload);
                    return Ok(PushOutcome::Pushed);
                }

                // We don't need to wait - rebuild the item and return it.
                let item = QueuedItem::Response {
                    channel,
                    id,
                    payload,
                };
                Ok(PushOutcome::NotPushed(item))
            }
            QueuedItem::RequestCancellation { .. }
            | QueuedItem::ResponseCancellation { .. }
            | QueuedItem::Error { .. } => Ok(PushOutcome::NotPushed(item)),
        }
    }

    /// Returns the wait index to assign to a new item being added to the wait queue.
    fn next_wait_index(&self) -> u64 {
        let mut current_max = 0;

        if let Some(index) = self
            .single_frame_requests
            .back()
            .map(|item| item.wait_index)
        {
            current_max = index;
        }

        if let Some(index) = self.multi_frame_requests.back().map(|item| item.wait_index) {
            current_max = cmp::max(current_max, index);
        }

        if let Some(index) = self
            .multi_frame_responses
            .back()
            .map(|item| item.wait_index)
        {
            current_max = cmp::max(current_max, index);
        }

        current_max.wrapping_add(1)
    }

    /// Pushes a request onto either the single-frame request queue or the multi-frame one.
    fn push_request<const N: usize>(
        &mut self,
        channel: ChannelId,
        io_id: IoId,
        payload: Option<Bytes>,
        permit: OwnedSemaphorePermit,
        juliet: &JulietProtocol<N>,
    ) {
        if payload_is_multi_frame(
            juliet.max_frame_size(),
            payload
                .as_ref()
                .map(|payld| payld.len())
                .unwrap_or_default(),
        ) {
            self.push_multi_frame_request(channel, io_id, payload, permit);
        } else {
            let wait_index = self.next_wait_index();
            let sf_req = SingleFrameRequest {
                wait_index,
                channel,
                io_id,
                payload,
                permit,
            };
            self.single_frame_requests.push_back(sf_req);
        }
    }

    /// Pushes a request onto the multi-frame request queue.
    fn push_multi_frame_request(
        &mut self,
        channel: ChannelId,
        io_id: IoId,
        payload: Option<Bytes>,
        permit: OwnedSemaphorePermit,
    ) {
        let wait_index = self.next_wait_index();
        let mf_req = MultiFrameRequest {
            wait_index,
            channel,
            io_id,
            payload,
            permit,
        };
        self.multi_frame_requests.push_back(mf_req);
    }

    /// Pushes a response onto the multi-frame response queue.
    fn push_response(&mut self, channel: ChannelId, id: Id, payload: Option<Bytes>) {
        let wait_index = self.next_wait_index();
        let mf_resp = MultiFrameResponse {
            wait_index,
            channel,
            id,
            payload,
        };
        self.multi_frame_responses.push_back(mf_resp);
    }

    /// Checks if we cannot schedule due to the message being multi-frame and there being a
    /// multi-frame send in progress.
    fn has_active_multi_frame<const N: usize>(
        &self,
        channel: ChannelId,
        payload: &Option<Bytes>,
        juliet: &JulietProtocol<N>,
        active_multi_frame: &[Option<Header>; N],
    ) -> bool {
        if active_multi_frame[channel.get() as usize].is_none() {
            return false;
        };

        if let Some(payload) = payload {
            return payload_is_multi_frame(juliet.max_frame_size(), payload.len());
        }

        false
    }

    /// Removes and returns the next "sendable" item (i.e. the oldest item) from the wait queue,
    /// where "sendable" is decided based on whether requests and/or multi-frame messages are
    /// allowed.
    pub(super) fn next_item<const N: usize>(
        &mut self,
        channel: ChannelId,
        juliet: &JulietProtocol<N>,
        active_multi_frame: &[Option<Header>; N],
    ) -> Result<Option<QueuedItem>, LocalProtocolViolation> {
        let request_allowed = juliet.allowed_to_send_request(channel)?;
        let multi_frame_allowed = active_multi_frame[channel.get() as usize].is_none();
        let maybe_item = match (request_allowed, multi_frame_allowed) {
            (false, false) => None,
            (false, true) => self.multi_frame_responses.pop_front().map(QueuedItem::from),
            (true, false) => self.single_frame_requests.pop_front().map(QueuedItem::from),
            (true, true) => {
                #[derive(Clone, Copy)]
                enum QueueToPop {
                    SingleFrameRequests(u64),
                    MultiFrameRequests(u64),
                    MultiFrameResponses(u64),
                }

                impl QueueToPop {
                    fn index(&self) -> u64 {
                        match self {
                            QueueToPop::SingleFrameRequests(index)
                            | QueueToPop::MultiFrameRequests(index)
                            | QueueToPop::MultiFrameResponses(index) => *index,
                        }
                    }
                }

                let mut queue_to_pop: Option<QueueToPop> = None;

                if let Some(wait_index) = self
                    .single_frame_requests
                    .front()
                    .map(|sf_req| sf_req.wait_index)
                {
                    queue_to_pop = Some(QueueToPop::SingleFrameRequests(wait_index))
                }

                if let Some(wait_index) = self
                    .multi_frame_requests
                    .front()
                    .map(|mf_req| mf_req.wait_index)
                {
                    match queue_to_pop {
                        Some(qtp) => {
                            if wait_index < qtp.index() {
                                queue_to_pop = Some(QueueToPop::MultiFrameRequests(wait_index));
                            }
                        }
                        None => queue_to_pop = Some(QueueToPop::MultiFrameRequests(wait_index)),
                    };
                }

                if let Some(index) = self
                    .multi_frame_responses
                    .front()
                    .map(|mf_resp| mf_resp.wait_index)
                {
                    match queue_to_pop {
                        Some(qtp) => {
                            if index < qtp.index() {
                                queue_to_pop = Some(QueueToPop::MultiFrameResponses(index));
                            }
                        }
                        None => queue_to_pop = Some(QueueToPop::MultiFrameResponses(index)),
                    };
                }

                match queue_to_pop {
                    Some(QueueToPop::SingleFrameRequests(_)) => {
                        self.single_frame_requests.pop_front().map(QueuedItem::from)
                    }
                    Some(QueueToPop::MultiFrameRequests(_)) => {
                        self.multi_frame_requests.pop_front().map(QueuedItem::from)
                    }
                    Some(QueueToPop::MultiFrameResponses(_)) => {
                        self.multi_frame_responses.pop_front().map(QueuedItem::from)
                    }
                    None => None,
                }
            }
        };
        Ok(maybe_item)
    }

    pub(super) fn clear(&mut self) {
        self.single_frame_requests.clear();
        self.multi_frame_requests.clear();
        self.multi_frame_responses.clear();
    }
}

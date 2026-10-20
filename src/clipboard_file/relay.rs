use base::message_proto::{
    cliprdr, Cliprdr, CliprdrFileContentsRequest, CliprdrFileContentsResponse,
    CliprdrServerFormatDataRequest, CliprdrServerFormatDataResponse, CliprdrTryEmpty,
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

pub(super) type PeerId = u64;
const MAX_PENDING: usize = 128;
const MAX_TARGETS: usize = 64;
const MAX_FILES: usize = 4096;
const MAX_READ: usize = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Endpoint {
    Local(PeerId),
    Remote(PeerId),
}

impl Endpoint {
    fn peer(self) -> PeerId {
        match self {
            Self::Local(peer) | Self::Remote(peer) => peer,
        }
    }
}

#[derive(Clone)]
pub(super) struct Dispatch {
    pub endpoint: Endpoint,
    pub message: Cliprdr,
    stamp: Stamp,
}

#[derive(Clone, Copy)]
enum Stamp {
    Offer(PeerId, u64),
    Fetch(PeerId, u64),
    Descriptor(PeerId, u64),
    StreamRequest(PeerId, i32),
    StreamResponse(PeerId, i32),
    Cleanup,
}

#[derive(Default)]
pub(super) struct Outcome {
    pub message: Option<Cliprdr>,
    pub outbound: Vec<Dispatch>,
}

struct File {
    size: u64,
    directory: bool,
}

struct Descriptor {
    response: Cliprdr,
    files: Vec<File>,
    id: i32,
}

struct Offer {
    source: PeerId,
    epoch: u64,
    format_id: i32,
    targets: HashSet<PeerId>,
    waiters: HashSet<Endpoint>,
    descriptor: Option<Descriptor>,
    waiting_since: Option<Instant>,
}

struct FormatRequest {
    // None observes a request on the original local OLE path.
    epoch: Option<u64>,
    issued: bool,
    expired: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamState {
    Queued,
    Issued,
    Responded,
    Cancelled,
}

struct StreamRequest {
    epoch: u64,
    target: PeerId,
    original_id: i32,
    requested: usize,
    size_request: bool,
    state: StreamState,
    since: Instant,
}

#[derive(Default)]
pub(super) struct Relay {
    offer: Option<Offer>,
    epoch: u64,
    next_stream: i32,
    formats: HashMap<PeerId, FormatRequest>,
    streams: HashMap<(PeerId, i32), StreamRequest>,
    local_negative_streams: HashSet<(PeerId, i32)>,
    // A stale target must not fall through to serving unrelated local files.
    targets: HashSet<PeerId>,
    sources: HashSet<PeerId>,
}

fn packet(union: cliprdr::Union) -> Cliprdr {
    Cliprdr {
        union: Some(union),
        ..Default::default()
    }
}

fn format_failure() -> Cliprdr {
    packet(cliprdr::Union::FormatDataResponse(
        CliprdrServerFormatDataResponse {
            msg_flags: 2,
            ..Default::default()
        },
    ))
}

fn stream_failure(stream_id: i32) -> Cliprdr {
    packet(cliprdr::Union::FileContentsResponse(
        CliprdrFileContentsResponse {
            msg_flags: 2,
            stream_id,
            ..Default::default()
        },
    ))
}

fn cleanup(endpoint: Endpoint, message: Cliprdr) -> Dispatch {
    Dispatch {
        endpoint,
        message,
        stamp: Stamp::Cleanup,
    }
}

fn descriptor(message: &Cliprdr) -> Option<Descriptor> {
    let Some(cliprdr::Union::FormatDataResponse(response)) = &message.union else {
        return None;
    };
    if response.msg_flags != 1 {
        return None;
    }
    let bytes = response.format_data.as_ref();
    let count = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if count == 0 || count > MAX_FILES || bytes.len() != 4 + count * 592 {
        return None;
    }
    let mut files = Vec::with_capacity(count);
    for entry in bytes[4..].chunks_exact(592) {
        let attributes = u32::from_le_bytes(entry[36..40].try_into().ok()?);
        let high = u32::from_le_bytes(entry[64..68].try_into().ok()?);
        let low = u32::from_le_bytes(entry[68..72].try_into().ok()?);
        files.push(File {
            size: ((high as u64) << 32) | low as u64,
            directory: attributes & 0x10 != 0,
        });
    }
    let id = bytes.iter().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(16777619)
    }) as i32;
    Some(Descriptor {
        response: message.clone(),
        files,
        id,
    })
}

impl Relay {
    pub fn handle(
        &mut self,
        peer: PeerId,
        local: bool,
        message: Cliprdr,
        allowed: &HashSet<PeerId>,
        active: bool,
        enabled: bool,
    ) -> Outcome {
        let mut out = self.reconcile(allowed, enabled);
        out.message = Some(message);
        let Some(message) = out.message.as_ref() else {
            return out;
        };
        match message.union.as_ref() {
            Some(cliprdr::Union::FormatList(list)) => {
                if local {
                    self.cancel_offer(&mut out.outbound);
                    self.targets.remove(&peer);
                } else if enabled && active && allowed.contains(&peer) {
                    self.cancel_offer(&mut out.outbound);
                    let format_id = list
                        .formats
                        .iter()
                        .find(|format| format.format == "FileGroupDescriptorW")
                        .map(|format| format.id);
                    let contents = list
                        .formats
                        .iter()
                        .any(|format| format.format == "FileContents");
                    if let (Some(format_id), true, Some(epoch)) =
                        (format_id, contents, self.epoch.checked_add(1))
                    {
                        self.epoch = epoch;
                        let targets: HashSet<_> = allowed
                            .iter()
                            .copied()
                            .filter(|target| *target != peer)
                            .take(MAX_TARGETS)
                            .collect();
                        if !targets.is_empty() {
                            self.targets.extend(&targets);
                            self.sources.insert(peer);
                            for target in &targets {
                                out.outbound.push(Dispatch {
                                    endpoint: Endpoint::Remote(*target),
                                    message: message.clone(),
                                    stamp: Stamp::Offer(peer, epoch),
                                });
                            }
                            self.offer = Some(Offer {
                                source: peer,
                                epoch,
                                format_id,
                                targets,
                                waiters: HashSet::new(),
                                descriptor: None,
                                waiting_since: None,
                            });
                        }
                    }
                }
            }
            Some(cliprdr::Union::FormatDataRequest(request)) => {
                let endpoint = if local {
                    Endpoint::Local(peer)
                } else {
                    Endpoint::Remote(peer)
                };
                let participates = self.offer.as_ref().map_or(false, |offer| {
                    if local {
                        offer.source == peer
                    } else {
                        offer.targets.contains(&peer)
                    }
                });
                if participates {
                    let valid = self.offer.as_ref().map_or(false, |offer| {
                        request.requested_format_id == offer.format_id
                    });
                    if valid {
                        self.request_descriptor(endpoint, &mut out.outbound);
                    } else {
                        out.outbound.push(cleanup(endpoint, format_failure()));
                    }
                    out.message = None;
                } else if !local && self.targets.contains(&peer)
                    || local && self.formats.get(&peer).map_or(false, |p| p.epoch.is_some())
                {
                    out.outbound.push(cleanup(endpoint, format_failure()));
                    out.message = None;
                } else if local && self.formats.len() < MAX_PENDING {
                    self.formats.entry(peer).or_insert(FormatRequest {
                        epoch: None,
                        issued: true,
                        expired: false,
                    });
                }
            }
            Some(cliprdr::Union::FormatDataResponse(_)) if !local => {
                if let Some(pending) = self.formats.remove(&peer) {
                    if let Some(epoch) = pending.epoch {
                        if self
                            .offer
                            .as_ref()
                            .map_or(false, |offer| offer.source == peer && offer.epoch == epoch)
                        {
                            if let Some(descriptor) = descriptor(message) {
                                if let Some(offer) = self.offer.as_mut() {
                                    for endpoint in &offer.waiters {
                                        out.outbound.push(Dispatch {
                                            endpoint: *endpoint,
                                            message: message.clone(),
                                            stamp: Stamp::Descriptor(peer, epoch),
                                        });
                                    }
                                    offer.descriptor = Some(descriptor);
                                }
                            } else {
                                self.cancel_offer(&mut out.outbound);
                            }
                        }
                        out.message = None;
                    }
                    self.fetch_descriptor(&mut out.outbound);
                } else if self.sources.contains(&peer) {
                    out.message = None;
                }
            }
            Some(cliprdr::Union::FileContentsRequest(request)) => {
                if local {
                    if request.stream_id < 0 {
                        if self.streams.contains_key(&(peer, request.stream_id)) {
                            out.outbound.push(cleanup(
                                Endpoint::Local(peer),
                                stream_failure(request.stream_id),
                            ));
                            out.message = None;
                        } else {
                            self.local_negative_streams
                                .insert((peer, request.stream_id));
                        }
                    }
                } else if self.targets.contains(&peer) {
                    self.request_stream(peer, request, &mut out.outbound);
                    out.message = None;
                }
            }
            Some(cliprdr::Union::FileContentsResponse(response)) if !local => {
                let key = (peer, response.stream_id);
                if self.local_negative_streams.remove(&key) {
                    return out;
                }
                if let Some(pending) = self.streams.get_mut(&key) {
                    if pending.state == StreamState::Issued {
                        let valid_size = response.msg_flags == 1
                            && if pending.size_request {
                                response.requested_data.len() == 8
                            } else {
                                response.requested_data.len() <= pending.requested
                            };
                        let mut reply = if valid_size {
                            response.clone()
                        } else {
                            CliprdrFileContentsResponse {
                                msg_flags: 2,
                                ..Default::default()
                            }
                        };
                        reply.stream_id = pending.original_id;
                        pending.state = StreamState::Responded;
                        out.outbound.push(Dispatch {
                            endpoint: Endpoint::Remote(pending.target),
                            message: packet(cliprdr::Union::FileContentsResponse(reply)),
                            stamp: Stamp::StreamResponse(peer, response.stream_id),
                        });
                    } else if pending.state == StreamState::Cancelled {
                        self.streams.remove(&key);
                    }
                    out.message = None;
                } else if response.stream_id < 0 && self.sources.contains(&peer) {
                    out.message = None;
                }
            }
            Some(cliprdr::Union::TryEmpty(_)) => {
                if local
                    && self
                        .offer
                        .as_ref()
                        .map_or(false, |o| o.targets.contains(&peer))
                {
                    out.message = None;
                } else if !local && self.offer.as_ref().map_or(false, |o| o.source == peer) {
                    self.cancel_offer(&mut out.outbound);
                }
            }
            Some(cliprdr::Union::FormatListResponse(_)) if !local => {
                if self.targets.contains(&peer) {
                    out.message = None;
                }
            }
            _ => {}
        }
        out
    }

    pub fn reconcile(&mut self, allowed: &HashSet<PeerId>, enabled: bool) -> Outcome {
        let mut out = Outcome::default();
        let expired_source = self.offer.as_ref().map_or(false, |offer| {
            offer.descriptor.is_none()
                && offer
                    .waiting_since
                    .map_or(false, |since| since.elapsed() >= REQUEST_TIMEOUT)
        });
        if !enabled
            || self
                .offer
                .as_ref()
                .map_or(false, |offer| !allowed.contains(&offer.source))
            || expired_source
        {
            if expired_source {
                if let Some(offer) = &self.offer {
                    if let Some(request) = self.formats.get_mut(&offer.source) {
                        request.expired = true;
                    }
                }
            }
            self.cancel_offer(&mut out.outbound);
        }
        let removed: Vec<_> = self
            .offer
            .as_ref()
            .map(|offer| offer.targets.difference(allowed).copied().collect())
            .unwrap_or_default();
        for target in removed {
            self.cancel_target(target, &mut out.outbound);
        }
        self.streams.retain(|_, pending| {
            if matches!(pending.state, StreamState::Queued | StreamState::Issued)
                && pending.since.elapsed() >= REQUEST_TIMEOUT
            {
                out.outbound.push(cleanup(
                    Endpoint::Remote(pending.target),
                    stream_failure(pending.original_id),
                ));
                if pending.state == StreamState::Queued {
                    return false;
                }
                pending.state = StreamState::Cancelled;
            }
            true
        });
        out
    }

    pub fn clear(&mut self, allowed: &HashSet<PeerId>, enabled: bool) -> Outcome {
        let mut out = self.reconcile(allowed, enabled);
        self.cancel_offer(&mut out.outbound);
        out
    }

    pub fn disconnect(
        &mut self,
        peer: PeerId,
        allowed: &HashSet<PeerId>,
        enabled: bool,
    ) -> Outcome {
        let mut out = self.reconcile(allowed, enabled);
        if self
            .offer
            .as_ref()
            .map_or(false, |offer| offer.source == peer)
        {
            self.cancel_offer(&mut out.outbound);
        } else {
            self.cancel_target(peer, &mut out.outbound);
        }
        self.formats.remove(&peer);
        self.streams.retain(|(source, _), _| *source != peer);
        self.local_negative_streams
            .retain(|(source, _)| *source != peer);
        self.targets.remove(&peer);
        self.sources.remove(&peer);
        out
    }

    pub fn validate(
        &mut self,
        dispatch: &Dispatch,
        allowed: &HashSet<PeerId>,
        enabled: bool,
    ) -> bool {
        if matches!(dispatch.stamp, Stamp::Cleanup) {
            return true;
        }
        if !enabled || !allowed.contains(&dispatch.endpoint.peer()) {
            return false;
        }
        let Some(offer) = self.offer.as_mut() else {
            return false;
        };
        if !allowed.contains(&offer.source) {
            return false;
        }
        match dispatch.stamp {
            Stamp::Offer(source, epoch) => {
                source == offer.source
                    && epoch == offer.epoch
                    && offer.targets.contains(&dispatch.endpoint.peer())
            }
            Stamp::Fetch(source, epoch) => {
                if source != offer.source || epoch != offer.epoch {
                    return false;
                }
                if let Some(request) = self.formats.get_mut(&source) {
                    if request.epoch == Some(epoch) && !request.expired {
                        request.issued = true;
                        return true;
                    }
                }
                false
            }
            Stamp::Descriptor(source, epoch) => {
                source == offer.source
                    && epoch == offer.epoch
                    && offer.waiters.remove(&dispatch.endpoint)
            }
            Stamp::StreamRequest(source, wire) | Stamp::StreamResponse(source, wire) => {
                if source != offer.source {
                    return false;
                }
                let Some(request) = self.streams.get_mut(&(source, wire)) else {
                    return false;
                };
                if request.epoch != offer.epoch || !allowed.contains(&request.target) {
                    return false;
                }
                if matches!(dispatch.stamp, Stamp::StreamRequest(..)) {
                    if request.state != StreamState::Queued {
                        return false;
                    }
                    request.state = StreamState::Issued;
                } else {
                    if request.state != StreamState::Responded {
                        return false;
                    }
                    self.streams.remove(&(source, wire));
                }
                true
            }
            Stamp::Cleanup => true,
        }
    }

    fn request_descriptor(&mut self, endpoint: Endpoint, outbound: &mut Vec<Dispatch>) {
        if let Some(offer) = self.offer.as_mut() {
            if offer.waiters.len() >= MAX_TARGETS + 1 {
                outbound.push(cleanup(endpoint, format_failure()));
                return;
            }
            offer.waiters.insert(endpoint);
            if let Some(descriptor) = &offer.descriptor {
                outbound.push(Dispatch {
                    endpoint,
                    message: descriptor.response.clone(),
                    stamp: Stamp::Descriptor(offer.source, offer.epoch),
                });
            } else if offer.waiting_since.is_none() {
                offer.waiting_since = Some(Instant::now());
            }
        }
        self.fetch_descriptor(outbound);
    }

    fn fetch_descriptor(&mut self, outbound: &mut Vec<Dispatch>) {
        let Some(offer) = self.offer.as_ref() else {
            return;
        };
        if offer.waiters.is_empty()
            || offer.descriptor.is_some()
            || self.formats.contains_key(&offer.source)
        {
            return;
        }
        if self.formats.len() + self.streams.len() >= MAX_PENDING {
            self.cancel_offer(outbound);
            return;
        }
        self.formats.insert(
            offer.source,
            FormatRequest {
                epoch: Some(offer.epoch),
                issued: false,
                expired: false,
            },
        );
        outbound.push(Dispatch {
            endpoint: Endpoint::Remote(offer.source),
            message: packet(cliprdr::Union::FormatDataRequest(
                CliprdrServerFormatDataRequest {
                    requested_format_id: offer.format_id,
                    ..Default::default()
                },
            )),
            stamp: Stamp::Fetch(offer.source, offer.epoch),
        });
    }

    fn request_stream(
        &mut self,
        peer: PeerId,
        request: &CliprdrFileContentsRequest,
        outbound: &mut Vec<Dispatch>,
    ) {
        let accepted = self.offer.as_ref().and_then(|offer| {
            if !offer.targets.contains(&peer) {
                return None;
            }
            let descriptor = offer.descriptor.as_ref()?;
            let index = usize::try_from(request.list_index).ok()?;
            let file = descriptor.files.get(index)?;
            let requested = usize::try_from(request.cb_requested).ok()?;
            let position = ((request.n_position_high as u32 as u64) << 32)
                | request.n_position_low as u32 as u64;
            if file.directory
                || !matches!(request.dw_flags, 1 | 2)
                || requested > MAX_READ
                || position > file.size
                || position.checked_add(requested as u64).is_none()
                || !request.have_clip_data_id
                || request.clip_data_id != descriptor.id
                || request.dw_flags == 1 && requested != 8
            {
                return None;
            }
            Some((offer.source, offer.epoch, requested))
        });
        let Some((source, epoch, requested)) = accepted else {
            outbound.push(cleanup(
                Endpoint::Remote(peer),
                stream_failure(request.stream_id),
            ));
            return;
        };
        if self.formats.len() + self.streams.len() >= MAX_PENDING
            || self.streams.values().any(|pending| {
                pending.target == peer
                    && pending.original_id == request.stream_id
                    && pending.state != StreamState::Cancelled
            })
        {
            outbound.push(cleanup(
                Endpoint::Remote(peer),
                stream_failure(request.stream_id),
            ));
            return;
        }
        let wire = loop {
            let Some(wire) = self.next_stream.checked_sub(1) else {
                outbound.push(cleanup(
                    Endpoint::Remote(peer),
                    stream_failure(request.stream_id),
                ));
                return;
            };
            self.next_stream = wire;
            if !self.local_negative_streams.contains(&(source, wire)) {
                break wire;
            }
        };
        self.streams.insert(
            (source, wire),
            StreamRequest {
                epoch,
                target: peer,
                original_id: request.stream_id,
                requested,
                size_request: request.dw_flags == 1,
                state: StreamState::Queued,
                since: Instant::now(),
            },
        );
        let mut forwarded = request.clone();
        forwarded.stream_id = wire;
        outbound.push(Dispatch {
            endpoint: Endpoint::Remote(source),
            message: packet(cliprdr::Union::FileContentsRequest(forwarded)),
            stamp: Stamp::StreamRequest(source, wire),
        });
    }

    fn cancel_target(&mut self, peer: PeerId, outbound: &mut Vec<Dispatch>) {
        if let Some(offer) = self.offer.as_mut() {
            if offer.targets.remove(&peer) {
                if offer.waiters.remove(&Endpoint::Remote(peer)) {
                    outbound.push(cleanup(Endpoint::Remote(peer), format_failure()));
                }
                outbound.push(cleanup(
                    Endpoint::Remote(peer),
                    packet(cliprdr::Union::TryEmpty(CliprdrTryEmpty::default())),
                ));
            }
        }
        self.cancel_streams(|pending| pending.target == peer, outbound);
    }

    fn cancel_offer(&mut self, outbound: &mut Vec<Dispatch>) {
        let Some(offer) = self.offer.take() else {
            return;
        };
        for endpoint in offer.waiters {
            outbound.push(cleanup(endpoint, format_failure()));
        }
        for peer in offer.targets {
            outbound.push(cleanup(
                Endpoint::Remote(peer),
                packet(cliprdr::Union::TryEmpty(CliprdrTryEmpty::default())),
            ));
        }
        if self.formats.get(&offer.source).map_or(false, |pending| {
            pending.epoch == Some(offer.epoch) && !pending.issued
        }) {
            self.formats.remove(&offer.source);
        }
        self.cancel_streams(|pending| pending.epoch == offer.epoch, outbound);
    }

    fn cancel_streams(
        &mut self,
        matches: impl Fn(&StreamRequest) -> bool,
        outbound: &mut Vec<Dispatch>,
    ) {
        self.streams.retain(|_, pending| {
            if !matches(pending) || pending.state == StreamState::Cancelled {
                return true;
            }
            outbound.push(cleanup(
                Endpoint::Remote(pending.target),
                stream_failure(pending.original_id),
            ));
            if pending.state == StreamState::Issued {
                pending.state = StreamState::Cancelled;
                true
            } else {
                false
            }
        });
    }
}

#[cfg(test)]
#[path = "relay/transfer_tests.rs"]
mod transfer_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{CliprdrFormat, CliprdrServerFormatList};

    fn formats() -> Cliprdr {
        packet(cliprdr::Union::FormatList(CliprdrServerFormatList {
            formats: [(100, "FileGroupDescriptorW"), (101, "FileContents")]
                .into_iter()
                .map(|(id, name)| CliprdrFormat {
                    id,
                    format: name.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }))
    }

    fn description(size: u32) -> Cliprdr {
        let mut bytes = vec![0; 4 + 2 * 592];
        bytes[..4].copy_from_slice(&2u32.to_le_bytes());
        bytes[4 + 36..4 + 40].copy_from_slice(&0x10u32.to_le_bytes());
        bytes[4 + 592 + 68..4 + 592 + 72].copy_from_slice(&size.to_le_bytes());
        packet(cliprdr::Union::FormatDataResponse(
            CliprdrServerFormatDataResponse {
                msg_flags: 1,
                format_data: bytes.into(),
                ..Default::default()
            },
        ))
    }

    fn describe() -> Cliprdr {
        packet(cliprdr::Union::FormatDataRequest(
            CliprdrServerFormatDataRequest {
                requested_format_id: 100,
                ..Default::default()
            },
        ))
    }

    fn read(stream_id: i32, index: i32, length: i32) -> Cliprdr {
        let list_id = descriptor(&description(3)).unwrap().id;
        packet(cliprdr::Union::FileContentsRequest(
            CliprdrFileContentsRequest {
                stream_id,
                list_index: index,
                dw_flags: 2,
                cb_requested: length,
                have_clip_data_id: true,
                clip_data_id: list_id,
                ..Default::default()
            },
        ))
    }

    fn reply(stream_id: i32) -> Cliprdr {
        packet(cliprdr::Union::FileContentsResponse(
            CliprdrFileContentsResponse {
                stream_id,
                msg_flags: 1,
                requested_data: b"abc".to_vec().into(),
                ..Default::default()
            },
        ))
    }

    fn validate(relay: &mut Relay, allowed: &HashSet<PeerId>, outcome: &Outcome) {
        for dispatch in &outcome.outbound {
            assert!(relay.validate(dispatch, allowed, true));
        }
    }

    fn ready(relay: &mut Relay, allowed: &HashSet<PeerId>) {
        let copied = relay.handle(1, false, formats(), allowed, true, true);
        validate(relay, allowed, &copied);
        let request = relay.handle(2, false, describe(), allowed, false, true);
        validate(relay, allowed, &request);
        let described = relay.handle(1, false, description(3), allowed, false, true);
        validate(relay, allowed, &described);
    }

    #[test]
    fn shares_one_descriptor_with_local_and_remote_consumers_and_routes_streams() {
        let allowed = HashSet::from([1, 2, 3]);
        let mut relay = Relay::default();
        let copied = relay.handle(1, false, formats(), &allowed, true, true);
        assert!(copied.message.is_some());
        assert_eq!(copied.outbound.len(), 2);
        validate(&mut relay, &allowed, &copied);
        let requested = relay.handle(2, false, describe(), &allowed, false, true);
        assert_eq!(requested.outbound.len(), 1);
        validate(&mut relay, &allowed, &requested);
        assert!(relay
            .handle(1, true, describe(), &allowed, false, true)
            .outbound
            .is_empty());
        assert!(relay
            .handle(3, false, describe(), &allowed, false, true)
            .outbound
            .is_empty());
        let described = relay.handle(1, false, description(3), &allowed, false, true);
        assert!(described.message.is_none());
        assert_eq!(described.outbound.len(), 3);
        assert!(described
            .outbound
            .iter()
            .any(|d| d.endpoint == Endpoint::Local(1)));
        validate(&mut relay, &allowed, &described);
        let cached = relay.handle(3, false, describe(), &allowed, false, true);
        assert_eq!(cached.outbound.len(), 1);
        assert_eq!(cached.outbound[0].endpoint, Endpoint::Remote(3));
        validate(&mut relay, &allowed, &cached);
        let b = relay.handle(2, false, read(7, 1, 3), &allowed, false, true);
        let c = relay.handle(3, false, read(7, 1, 3), &allowed, false, true);
        let wire = |out: &Outcome| match &out.outbound[0].message.union {
            Some(cliprdr::Union::FileContentsRequest(request)) => request.stream_id,
            _ => panic!("expected file request"),
        };
        assert!(wire(&b) < 0 && wire(&c) < 0 && wire(&b) != wire(&c));
        validate(&mut relay, &allowed, &b);
        validate(&mut relay, &allowed, &c);
        for (out, peer) in [(&c, 3), (&b, 2)] {
            let response = relay.handle(1, false, reply(wire(out)), &allowed, false, true);
            assert!(response.message.is_none());
            assert_eq!(response.outbound[0].endpoint, Endpoint::Remote(peer));
            assert!(
                matches!(&response.outbound[0].message.union, Some(cliprdr::Union::FileContentsResponse(r)) if r.stream_id == 7)
            );
            validate(&mut relay, &allowed, &response);
        }
        for invalid in [
            read(8, 0, 3),
            read(8, 2, 3),
            read(8, 1, MAX_READ as i32 + 1),
        ] {
            let denied = relay.handle(2, false, invalid, &allowed, false, true);
            assert!(
                matches!(&denied.outbound[0].message.union, Some(cliprdr::Union::FileContentsResponse(r)) if r.msg_flags == 2)
            );
        }
        let mut unversioned = read(8, 1, 3);
        if let Some(cliprdr::Union::FileContentsRequest(request)) = &mut unversioned.union {
            request.have_clip_data_id = false;
        }
        let denied = relay.handle(2, false, unversioned, &allowed, false, true);
        assert!(denied.message.is_none());
        assert_eq!(denied.outbound[0].endpoint, Endpoint::Remote(2));
        assert!(
            matches!(&denied.outbound[0].message.union, Some(cliprdr::Union::FileContentsResponse(r)) if r.msg_flags == 2 && r.stream_id == 8 && r.requested_data.is_empty())
        );
        let mut wrong_list = read(9, 1, 3);
        if let Some(cliprdr::Union::FileContentsRequest(request)) = &mut wrong_list.union {
            request.clip_data_id ^= 1;
        }
        let denied = relay.handle(2, false, wrong_list, &allowed, false, true);
        assert!(
            matches!(&denied.outbound[0].message.union, Some(cliprdr::Union::FileContentsResponse(r)) if r.msg_flags == 2 && r.stream_id == 9)
        );
        for (flags, bytes) in [
            (1, 3u64.to_le_bytes().to_vec()),
            (1, vec![0; 9]),
            (2, vec![0; 9]),
        ] {
            let mut size = read(10, 1, 8);
            if let Some(cliprdr::Union::FileContentsRequest(request)) = &mut size.union {
                request.dw_flags = 1;
            }
            let requested = relay.handle(2, false, size, &allowed, false, true);
            validate(&mut relay, &allowed, &requested);
            let response = relay.handle(
                1,
                false,
                packet(cliprdr::Union::FileContentsResponse(
                    CliprdrFileContentsResponse {
                        stream_id: wire(&requested),
                        msg_flags: flags,
                        requested_data: bytes.clone().into(),
                        ..Default::default()
                    },
                )),
                &allowed,
                false,
                true,
            );
            let Some(cliprdr::Union::FileContentsResponse(reply)) =
                &response.outbound[0].message.union
            else {
                panic!("expected file response");
            };
            assert_eq!(reply.stream_id, 10);
            if flags == 1 && bytes.len() == 8 {
                assert_eq!(reply.msg_flags, 1);
                assert_eq!(reply.requested_data.as_ref(), bytes);
            } else {
                assert_eq!(reply.msg_flags, 2);
                assert!(reply.requested_data.is_empty());
            }
            validate(&mut relay, &allowed, &response);
        }
    }

    #[test]
    fn cancels_stale_work_and_quarantines_uncorrelated_late_descriptors() {
        for (count, length) in [
            (0, 4),
            (MAX_FILES + 1, 4 + (MAX_FILES + 1) * 592),
            (2, 4 + 2 * 592 - 1),
            (2, 4 + 2 * 592 + 1),
        ] {
            let mut malformed = description(3);
            if let Some(cliprdr::Union::FormatDataResponse(response)) = &mut malformed.union {
                let mut bytes = vec![0; length];
                bytes[..4].copy_from_slice(&(count as u32).to_le_bytes());
                response.format_data = bytes.into();
            }
            assert!(descriptor(&malformed).is_none());
        }
        let allowed = HashSet::from([1, 2, 3]);
        let mut relay = Relay::default();
        let copied = relay.handle(1, false, formats(), &allowed, true, true);
        validate(&mut relay, &allowed, &copied);
        let old = relay.handle(2, false, describe(), &allowed, false, true);
        validate(&mut relay, &allowed, &old);
        let recopied = relay.handle(1, false, formats(), &allowed, true, true);
        assert!(recopied.outbound.iter().any(|d| matches!(&d.message.union, Some(cliprdr::Union::FormatDataResponse(r)) if r.msg_flags == 2)));
        validate(&mut relay, &allowed, &recopied);
        assert!(relay
            .handle(2, false, describe(), &allowed, false, true)
            .outbound
            .is_empty());
        let late = relay.handle(1, false, description(1), &allowed, false, true);
        assert!(late.message.is_none());
        assert_eq!(late.outbound.len(), 1);
        assert!(matches!(
            late.outbound[0].message.union,
            Some(cliprdr::Union::FormatDataRequest(_))
        ));
        validate(&mut relay, &allowed, &late);
        let fresh = relay.handle(1, false, description(3), &allowed, false, true);
        validate(&mut relay, &allowed, &fresh);
        let mut old_index = read(6, 1, 1);
        if let Some(cliprdr::Union::FileContentsRequest(request)) = &mut old_index.union {
            request.clip_data_id = descriptor(&description(1)).unwrap().id;
        }
        let denied = relay.handle(2, false, old_index, &allowed, false, true);
        assert!(
            matches!(&denied.outbound[0].message.union, Some(cliprdr::Union::FileContentsResponse(r)) if r.msg_flags == 2 && r.stream_id == 6)
        );
        let b = relay.handle(2, false, read(7, 1, 3), &allowed, false, true);
        validate(&mut relay, &allowed, &b);
        let b_wire = match &b.outbound[0].message.union {
            Some(cliprdr::Union::FileContentsRequest(r)) => r.stream_id,
            _ => panic!("expected file request"),
        };
        let allowed_c = HashSet::from([1, 3]);
        let revoked = relay.reconcile(&allowed_c, true);
        assert!(!revoked.outbound.is_empty());
        assert!(relay
            .handle(1, false, reply(b_wire), &allowed_c, false, true)
            .outbound
            .is_empty());
        let c = relay.handle(3, false, read(7, 1, 3), &allowed_c, false, true);
        let cleared = relay.clear(&allowed_c, true);
        assert!(!cleared.outbound.is_empty());
        assert!(!relay.validate(&c.outbound[0], &allowed_c, true));
        assert!(relay
            .handle(3, false, read(7, 1, 3), &allowed_c, false, true)
            .message
            .is_none());
        ready(&mut relay, &allowed);
        let queued = relay.handle(2, false, read(9, 1, 3), &allowed, false, true);
        assert!(!relay
            .disconnect(1, &HashSet::from([2, 3]), true)
            .outbound
            .is_empty());
        assert!(!relay.validate(&queued.outbound[0], &allowed, true));
        ready(&mut relay, &allowed);
        let pending = relay.handle(2, false, read(10, 1, 3), &allowed, false, true);
        validate(&mut relay, &allowed, &pending);
        for request in relay.streams.values_mut() {
            request.since = Instant::now() - REQUEST_TIMEOUT;
        }
        assert!(!relay.reconcile(&allowed, true).outbound.is_empty());
        assert!(relay
            .streams
            .values()
            .all(|r| r.state == StreamState::Cancelled));
        let mut relay = Relay::default();
        let copied = relay.handle(1, false, formats(), &allowed, true, true);
        validate(&mut relay, &allowed, &copied);
        let pending = relay.handle(2, false, describe(), &allowed, false, true);
        validate(&mut relay, &allowed, &pending);
        relay.offer.as_mut().unwrap().waiting_since = Some(Instant::now() - REQUEST_TIMEOUT);
        let timed_out = relay.reconcile(&allowed, true);
        assert!(relay.offer.is_none());
        assert!(timed_out.outbound.iter().any(|d| d.endpoint == Endpoint::Remote(2)
            && matches!(&d.message.union, Some(cliprdr::Union::FormatDataResponse(r)) if r.msg_flags == 2)));
        assert!(relay.formats[&1].issued && relay.formats[&1].expired);
        validate(&mut relay, &allowed, &timed_out);
        let late = relay.handle(1, false, description(3), &allowed, false, true);
        assert!(late.message.is_none() && late.outbound.is_empty());
        assert!(relay.offer.is_none() && !relay.formats.contains_key(&1));
    }

    #[test]
    fn disabled_and_original_local_paths_keep_their_messages() {
        let allowed = HashSet::from([1, 2, 3]);
        let mut relay = Relay::default();
        let off = relay.handle(1, false, formats(), &allowed, true, false);
        assert!(off.message.is_some() && off.outbound.is_empty());
        let local = relay.handle(1, true, describe(), &allowed, false, false);
        assert!(local.message.is_some());
        let copied = relay.handle(1, false, formats(), &allowed, true, true);
        validate(&mut relay, &allowed, &copied);
        assert!(relay
            .handle(2, false, describe(), &allowed, false, true)
            .outbound
            .is_empty());
        let original = relay.handle(1, false, description(1), &allowed, false, true);
        assert!(original.message.is_some());
        assert_eq!(original.outbound.len(), 1);
        validate(&mut relay, &allowed, &original);
        let described = relay.handle(1, false, description(3), &allowed, false, true);
        validate(&mut relay, &allowed, &described);
        let local_read = relay.handle(1, true, read(42, 1, 3), &allowed, false, true);
        assert!(local_read.message.is_some() && local_read.outbound.is_empty());
        let local_response = relay.handle(1, false, reply(42), &allowed, false, true);
        assert!(local_response.message.is_some() && local_response.outbound.is_empty());
        let local_copy = relay.handle(2, true, formats(), &allowed, false, true);
        assert!(local_copy.message.is_some());
        assert!(relay
            .handle(2, false, describe(), &allowed, false, true)
            .message
            .is_some());
        ready(&mut relay, &allowed);
        let request = relay.handle(2, false, read(1, 1, 3), &allowed, false, true);
        assert!(!relay.reconcile(&allowed, false).outbound.is_empty());
        assert!(!relay.validate(&request.outbound[0], &allowed, false));
        let stale = relay.handle(2, false, read(1, 1, 3), &allowed, false, false);
        assert!(stale.message.is_none());
    }
}

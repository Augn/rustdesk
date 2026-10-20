use super::relay::{Dispatch, Endpoint, Outcome, PeerId, Relay};
use crate::{client::Data, ui_session_interface::SessionPermissionConfig};
use base::message_proto::{message, Cliprdr, Message};
use hbb_common::{log, tokio::sync::mpsc::UnboundedSender};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct QueuedMessage(Queued);

#[derive(Clone)]
enum Queued {
    Relay(Dispatch),
    LocalOffer(PeerId, Arc<()>, Cliprdr),
}

struct Connection {
    native_id: i32,
    permission: SessionPermissionConfig,
    sender: UnboundedSender<Data>,
}

#[derive(Default)]
struct State {
    relay: Relay,
    connections: HashMap<PeerId, Connection>,
    next_id: PeerId,
    clipboard_revision: Arc<()>,
    relayed: bool,
}

lazy_static::lazy_static! {
    static ref STATE: Mutex<State> = Mutex::new(State::default());
}

impl State {
    fn allowed(&self) -> HashSet<PeerId> {
        self.connections
            .iter()
            .filter_map(|(id, connection)| {
                let permission = &connection.permission;
                let lc = permission.lc.read().unwrap();
                #[cfg(feature = "unix-file-copy-paste")]
                if !crate::is_support_file_copy_paste_num(lc.version) {
                    return None;
                }
                (!connection.sender.is_closed()
                    && *permission.server_keyboard_enabled.read().unwrap()
                    && *permission.server_file_transfer_enabled.read().unwrap()
                    && lc.enable_file_copy_paste.v
                    && !lc.view_only.v)
                    .then_some(*id)
            })
            .collect()
    }

    fn enqueue(&self, outbound: Vec<Dispatch>) {
        for dispatch in outbound {
            let id = match dispatch.endpoint {
                Endpoint::Local(id) | Endpoint::Remote(id) => id,
            };
            if let Some(connection) = self.connections.get(&id) {
                if let Err(error) = connection
                    .sender
                    .send(Data::ClipboardRelay(QueuedMessage(Queued::Relay(dispatch))))
                {
                    log::debug!("File clipboard relay receiver closed: {error}");
                }
            }
        }
    }

    fn clear(&mut self, enabled: bool) {
        let allowed = self.allowed();
        self.clipboard_revision = Arc::new(());
        let outcome = self.relay.clear(&allowed, enabled);
        self.enqueue(outcome.outbound);
    }

    fn process(
        &mut self,
        id: PeerId,
        local: bool,
        message: Cliprdr,
        active: bool,
        enabled: bool,
    ) -> Option<Cliprdr> {
        if !self.connections.contains_key(&id) || !enabled && !self.relayed {
            return Some(message);
        }
        let allowed = self.allowed();
        if !local
            && active
            && allowed.contains(&id)
            && matches!(
                message.union.as_ref(),
                Some(base::message_proto::cliprdr::Union::FormatList(_))
            )
        {
            self.clipboard_revision = Arc::new(());
            self.relayed |= enabled;
        }
        let local_offer = local
            && matches!(
                message.union.as_ref(),
                Some(base::message_proto::cliprdr::Union::FormatList(_))
            );
        let Outcome { message, outbound } = self
            .relay
            .handle(id, local, message, &allowed, active, enabled);
        self.enqueue(outbound);
        if local_offer {
            if let Some(message) = message {
                // Cleanup and its replacement must use the same queue, including native callbacks.
                let revision = self.clipboard_revision.clone();
                if let Some(connection) = self.connections.get(&id) {
                    if let Err(error) = connection.sender.send(Data::ClipboardRelay(QueuedMessage(
                        Queued::LocalOffer(id, revision, message),
                    ))) {
                        log::debug!("File clipboard offer receiver closed: {error}");
                    }
                }
            }
            return None;
        }
        message
    }

    fn prepare(
        &mut self,
        id: PeerId,
        queued: QueuedMessage,
        enabled: bool,
    ) -> Option<(Cliprdr, bool)> {
        let allowed = self.allowed();
        let dispatch = match queued.0 {
            Queued::LocalOffer(owner, revision, message) => {
                return (owner == id
                    && allowed.contains(&id)
                    && Arc::ptr_eq(&revision, &self.clipboard_revision))
                .then_some((message, false));
            }
            Queued::Relay(dispatch) => dispatch,
        };
        let endpoint = dispatch.endpoint;
        if !matches!(endpoint, Endpoint::Local(target) | Endpoint::Remote(target) if target == id)
            || !self.connections.contains_key(&id)
        {
            return None;
        }
        let outcome = self.relay.reconcile(&allowed, enabled);
        self.enqueue(outcome.outbound);
        self.relay
            .validate(&dispatch, &allowed, enabled)
            .then_some((dispatch.message, matches!(endpoint, Endpoint::Local(_))))
    }
}

pub(crate) fn register(
    native_id: i32,
    permission: SessionPermissionConfig,
    sender: UnboundedSender<Data>,
) -> PeerId {
    let mut state = STATE.lock().unwrap();
    let Some(id) = state.next_id.checked_add(1) else {
        log::error!("File clipboard relay connection identifiers exhausted");
        return 0;
    };
    state.next_id = id;
    state.connections.insert(
        id,
        Connection {
            native_id,
            permission,
            sender,
        },
    );
    id
}

pub(crate) fn disconnect(id: PeerId) {
    let enabled = crate::clipboard::is_sync_clipboard_between_sessions_enabled();
    let mut state = STATE.lock().unwrap();
    state.connections.remove(&id);
    let allowed = state.allowed();
    let outcome = state.relay.disconnect(id, &allowed, enabled);
    state.enqueue(outcome.outbound);
}

pub(crate) fn refresh() {
    let enabled = crate::clipboard::is_sync_clipboard_between_sessions_enabled();
    let mut state = STATE.lock().unwrap();
    let allowed = state.allowed();
    let outcome = state.relay.reconcile(&allowed, enabled);
    state.enqueue(outcome.outbound);
}

pub(crate) fn clear() {
    let enabled = crate::clipboard::is_sync_clipboard_between_sessions_enabled();
    STATE.lock().unwrap().clear(enabled);
}

pub(crate) fn local_text_changed() {
    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::winuser::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
        // File copies can also advertise text; that text must not invalidate the file offer.
        let name: Vec<u16> = "FileGroupDescriptorW\0".encode_utf16().collect();
        if IsClipboardFormatAvailable(15) != 0
            || IsClipboardFormatAvailable(RegisterClipboardFormatW(name.as_ptr())) != 0
        {
            return;
        }
    }
    clear();
}

fn process(id: PeerId, local: bool, message: Cliprdr, active: bool) -> Option<Cliprdr> {
    let enabled = crate::clipboard::is_sync_clipboard_between_sessions_enabled();
    STATE
        .lock()
        .unwrap()
        .process(id, local, message, active, enabled)
}

pub(crate) fn from_peer(id: PeerId, message: Cliprdr, active: bool) -> Option<Cliprdr> {
    process(id, false, message, active)
}

pub(crate) fn from_local_message(id: PeerId, message: Cliprdr) -> Option<Cliprdr> {
    process(id, true, message, true)
}

pub(crate) fn from_local(id: PeerId, clip: &clipboard::ClipboardFile) -> bool {
    if matches!(
        clip,
        clipboard::ClipboardFile::Files { .. }
            | clipboard::ClipboardFile::NotifyCallback { .. }
            | clipboard::ClipboardFile::FormatDataResponse { .. }
            | clipboard::ClipboardFile::FileContentsResponse { .. }
    ) {
        return true;
    }
    let message = super::clip_2_msg(clip.clone());
    match message.union {
        Some(message::Union::Cliprdr(message)) => process(id, true, message, true).is_some(),
        _ => true,
    }
}

pub(crate) fn prepare(id: PeerId, queued: QueuedMessage) -> Option<(Cliprdr, bool)> {
    let enabled = crate::clipboard::is_sync_clipboard_between_sessions_enabled();
    STATE.lock().unwrap().prepare(id, queued, enabled)
}

pub(crate) fn message(clip: Cliprdr) -> Message {
    Message {
        union: Some(message::Union::Cliprdr(clip)),
        ..Default::default()
    }
}

pub(crate) fn deliver_local(id: PeerId, message: Cliprdr) -> Vec<Message> {
    let native_id = {
        let state = STATE.lock().unwrap();
        let Some(connection) = state.connections.get(&id) else {
            return vec![];
        };
        connection.native_id
    };
    let Some(clip) = super::msg_2_clip(message) else {
        return vec![];
    };
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        #[cfg(target_os = "macos")]
        let native = clipboard::platform::unix::macos::should_handle_msg(&clip);
        #[cfg(target_os = "windows")]
        let native = true;
        if native {
            if let Err(error) = clipboard::ContextSend::proc(|context| {
                context
                    .server_clip_file(native_id, clip.clone())
                    .map_err(Into::into)
            }) {
                log::error!("Failed to deliver relayed file clipboard response: {error}");
            }
            return vec![];
        }
    }
    #[cfg(feature = "unix-file-copy-paste")]
    return super::unix_file_clip::serve_clip_messages(
        crate::clipboard::ClipboardSide::Client,
        clip,
        native_id,
    )
    .into_iter()
    .filter_map(|response| {
        if let Some(message::Union::Cliprdr(clip)) = response.union {
            from_local_message(id, clip).map(self::message)
        } else {
            Some(response)
        }
    })
    .collect();
    #[cfg(not(feature = "unix-file-copy-paste"))]
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{cliprdr, CliprdrFormat, CliprdrServerFormatList};
    use hbb_common::tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use std::sync::RwLock;

    fn connection(native_id: i32) -> (Connection, UnboundedReceiver<Data>) {
        let mut login = crate::client::LoginConfigHandler::default();
        login.version = hbb_common::get_version_number("1.5.1");
        login.get_config().enable_file_copy_paste.v = true;
        login.get_config().view_only.v = false;
        let permission = SessionPermissionConfig {
            lc: Arc::new(RwLock::new(login)),
            server_keyboard_enabled: Arc::new(RwLock::new(true)),
            server_file_transfer_enabled: Arc::new(RwLock::new(true)),
            server_clipboard_enabled: Arc::new(RwLock::new(true)),
        };
        let (sender, receiver) = unbounded_channel();
        (
            Connection {
                native_id,
                permission,
                sender,
            },
            receiver,
        )
    }

    fn formats() -> Cliprdr {
        Cliprdr {
            union: Some(cliprdr::Union::FormatList(CliprdrServerFormatList {
                formats: [(100, "FileGroupDescriptorW"), (101, "FileContents")]
                    .into_iter()
                    .map(|(id, name)| CliprdrFormat {
                        id,
                        format: name.into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn queued(receiver: &mut UnboundedReceiver<Data>) -> QueuedMessage {
        match receiver.try_recv().unwrap() {
            Data::ClipboardRelay(message) => message,
            _ => panic!("expected a file clipboard queue entry"),
        }
    }

    #[test]
    fn preserves_queue_order_permissions_and_connection_revisions() {
        let mut state = State::default();
        let (source, _source_rx) = connection(11);
        let (target, mut target_rx) = connection(12);
        let permission = target.permission.clone();
        let (closed, closed_rx) = connection(13);
        drop(closed_rx);
        state
            .connections
            .extend([(1, source), (2, target), (3, closed)]);
        assert_eq!(state.allowed(), HashSet::from([1, 2]));
        for flag in [
            &permission.server_keyboard_enabled,
            &permission.server_file_transfer_enabled,
        ] {
            *flag.write().unwrap() = false;
            assert!(!state.allowed().contains(&2));
            *flag.write().unwrap() = true;
        }
        permission
            .lc
            .write()
            .unwrap()
            .get_config()
            .enable_file_copy_paste
            .v = false;
        assert!(!state.allowed().contains(&2));
        permission
            .lc
            .write()
            .unwrap()
            .get_config()
            .enable_file_copy_paste
            .v = true;
        permission.lc.write().unwrap().get_config().view_only.v = true;
        assert!(!state.allowed().contains(&2));
        permission.lc.write().unwrap().get_config().view_only.v = false;
        #[cfg(feature = "unix-file-copy-paste")]
        {
            permission.lc.write().unwrap().version = 0;
            assert!(!state.allowed().contains(&2));
            permission.lc.write().unwrap().version = hbb_common::get_version_number("1.5.1");
        }
        assert_eq!(state.allowed(), HashSet::from([1, 2]));

        let offer = formats();
        assert_eq!(
            state.process(2, true, offer.clone(), true, false),
            Some(offer.clone())
        );
        assert!(target_rx.try_recv().is_err());
        assert!(!state.relayed);
        assert_eq!(
            state.process(1, false, offer.clone(), true, true),
            Some(offer.clone())
        );
        assert!(state.relayed);
        let remote_offer = queued(&mut target_rx);
        assert_eq!(
            state.prepare(2, remote_offer, true),
            Some((offer.clone(), false))
        );
        assert!(state.process(2, true, offer.clone(), true, true).is_none());
        let cleanup = queued(&mut target_rx);
        let local_offer = queued(&mut target_rx);
        let (cleanup, local) = state.prepare(2, cleanup, true).unwrap();
        assert!(!local);
        assert!(matches!(cleanup.union, Some(cliprdr::Union::TryEmpty(_))));
        assert_eq!(
            state.prepare(2, local_offer.clone(), true),
            Some((offer.clone(), false))
        );
        assert_eq!(
            state.prepare(2, local_offer.clone(), false),
            Some((offer.clone(), false))
        );
        *permission.server_file_transfer_enabled.write().unwrap() = false;
        assert!(state.prepare(2, local_offer.clone(), true).is_none());
        *permission.server_file_transfer_enabled.write().unwrap() = true;

        let (replacement, mut replacement_rx) = connection(12);
        state.connections.remove(&2);
        state.connections.insert(4, replacement);
        assert!(state.allowed().contains(&4));
        assert!(state.prepare(4, local_offer.clone(), true).is_none());
        assert!(state.prepare(2, local_offer, true).is_none());

        assert!(state.process(4, true, offer.clone(), true, true).is_none());
        let stale_local = queued(&mut replacement_rx);
        state.process(1, false, offer.clone(), true, true);
        let remote_offer = queued(&mut replacement_rx);
        assert_eq!(
            state.prepare(4, remote_offer, true),
            Some((offer.clone(), false))
        );
        assert!(state.prepare(4, stale_local, true).is_none());

        assert!(state.process(4, true, offer, true, true).is_none());
        let cleanup = queued(&mut replacement_rx);
        assert!(state.prepare(4, cleanup, true).is_some());
        let stale_local = queued(&mut replacement_rx);
        state.clear(true);
        assert!(state.prepare(4, stale_local, true).is_none());
        assert!(replacement_rx.try_recv().is_err());
        for clip in [
            clipboard::ClipboardFile::FormatDataResponse {
                msg_flags: 1,
                format_data: vec![0; 16],
            },
            clipboard::ClipboardFile::FileContentsResponse {
                msg_flags: 1,
                stream_id: 7,
                requested_data: vec![0; 16],
            },
        ] {
            assert!(from_local(0, &clip));
        }
    }
}

use super::{
    paste_observer::PasteObserver,
    paste_task::{FileContentsResponse, PasteTask},
};
use crate::{
    platform::unix::{
        filetype::FileDescription, FILECONTENTS_FORMAT_NAME, FILEDESCRIPTORW_FORMAT_NAME,
    },
    send_data, ClipboardFile, CliprdrError, CliprdrServiceContext, ProgressPercent,
};
use hbb_common::{bail, log, ResultType};
use objc2::{msg_send_id, rc::autoreleasepool, rc::Id, ClassType};
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL};
use objc2_foundation::{NSArray, NSString};
use std::{
    collections::{hash_map::Entry, HashMap},
    fs::{self, Metadata, OpenOptions},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        mpsc::{channel, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

pub const TEMP_FILE_PREFIX: &str = ".rustdesk_";

#[derive(Default, Debug, Clone, PartialEq)]
pub(super) struct PasteObserverInfo {
    pub file_descriptor_id: i32,
    pub conn_id: i32,
    pub source_path: String,
    pub target_path: String,
}

struct Placeholder {
    path: String,
    metadata: Metadata,
}

impl Placeholder {
    fn new() -> io::Result<Self> {
        let path = format!("/tmp/{TEMP_FILE_PREFIX}{}", uuid::Uuid::new_v4());
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        match file.metadata() {
            Ok(metadata) => Ok(Self { path, metadata }),
            Err(error) => {
                if let Err(cleanup) = fs::remove_file(&path) {
                    log::warn!("Failed to remove uninitialized clipboard placeholder: {cleanup}");
                }
                Err(error)
            }
        }
    }

    fn matches_source(&self, metadata: &Metadata) -> bool {
        metadata.dev() == self.metadata.dev() && metadata.ino() == self.metadata.ino()
    }
}

impl Drop for Placeholder {
    fn drop(&mut self) {
        let result = fs::symlink_metadata(&self.path).and_then(|metadata| {
            if self.matches_source(&metadata) {
                fs::remove_file(&self.path)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!("Failed to remove clipboard placeholder: {error}");
            }
        }
    }
}

struct Offer {
    conn_id: i32,
    change_count: isize,
    source: Placeholder,
}

struct PendingPaste {
    source_path: String,
    target_dir: PathBuf,
}

#[derive(Default)]
struct PasteState {
    offer: Option<Offer>,
    pending: HashMap<i32, PendingPaste>,
}

impl PasteState {
    fn take_response(&mut self, conn_id: i32) -> Option<PendingPaste> {
        let pending = self.pending.remove(&conn_id)?;
        let offer = self.offer.as_ref()?;
        (offer.source.path == pending.source_path).then_some(pending)
    }
}

pub struct PasteboardContext {
    pasteboard: Id<NSPasteboard>,
    observer: Arc<Mutex<PasteObserver>>,
    state: Arc<Mutex<PasteState>>,
    stop_cleanup: Sender<()>,
    cleanup_thread: Option<thread::JoinHandle<()>>,
    tx_paste_task: Sender<FileContentsResponse>,
    paste_task: Arc<Mutex<PasteTask>>,
}

unsafe impl Send for PasteboardContext {}
unsafe impl Sync for PasteboardContext {}

impl Drop for PasteboardContext {
    fn drop(&mut self) {
        self.empty_clipboard_(0);
        if let Err(error) = self.stop_cleanup.send(()) {
            log::debug!("Clipboard cleanup thread already stopped: {error}");
        }
        if let Some(thread) = self.cleanup_thread.take() {
            if thread.join().is_err() {
                log::warn!("Clipboard cleanup thread panicked");
            }
        }
    }
}

impl CliprdrServiceContext for PasteboardContext {
    fn set_is_stopped(&mut self) -> Result<(), CliprdrError> {
        Ok(())
    }

    fn empty_clipboard(&mut self, conn_id: i32) -> Result<bool, CliprdrError> {
        Ok(self.empty_clipboard_(conn_id))
    }

    fn server_clip_file(&mut self, conn_id: i32, msg: ClipboardFile) -> Result<(), CliprdrError> {
        self.server_clip_file_(conn_id, msg)
    }

    fn get_progress_percent(&self) -> Option<ProgressPercent> {
        self.paste_task.lock().unwrap().progress_percent()
    }

    fn cancel(&mut self) {
        self.paste_task.lock().unwrap().cancel();
    }
}

impl PasteboardContext {
    fn empty_clipboard_(&mut self, conn_id: i32) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(offer) = state
            .offer
            .as_ref()
            .filter(|offer| conn_id == 0 || conn_id == offer.conn_id)
        {
            autoreleasepool(|_| unsafe {
                if self.pasteboard.changeCount() == offer.change_count {
                    self.pasteboard.clearContents();
                }
            });
            state.offer = None;
            self.observer.lock().unwrap().stop();
        }
        true
    }

    fn server_clip_file_(&mut self, conn_id: i32, msg: ClipboardFile) -> Result<(), CliprdrError> {
        match msg {
            ClipboardFile::FormatList { format_list } => {
                if !self.paste_task.lock().unwrap().is_finished() {
                    return Err(CliprdrError::CommonError {
                        description: "previous file paste task is not finished".to_string(),
                    });
                }
                self.handle_format_list(conn_id, format_list)?;
            }
            ClipboardFile::FormatDataResponse {
                msg_flags,
                format_data,
            } => {
                self.handle_format_data_response(conn_id, msg_flags, format_data)?;
            }
            ClipboardFile::FileContentsResponse {
                msg_flags,
                stream_id,
                requested_data,
            } => {
                self.handle_file_contents_response(conn_id, msg_flags, stream_id, requested_data)?;
            }
            ClipboardFile::TryEmpty => self.handle_try_empty(conn_id),
            _ => {}
        }
        Ok(())
    }

    fn handle_format_list(
        &mut self,
        conn_id: i32,
        format_list: Vec<(i32, String)>,
    ) -> Result<(), CliprdrError> {
        if !format_list
            .iter()
            .any(|(_, name)| name == FILECONTENTS_FORMAT_NAME)
        {
            return Err(CliprdrError::CommonError {
                description: "no file contents format found".to_string(),
            });
        }
        let Some(file_descriptor_id) = format_list
            .iter()
            .find(|(_, name)| name == FILEDESCRIPTORW_FORMAT_NAME)
            .map(|(id, _)| *id)
        else {
            return Err(CliprdrError::CommonError {
                description: "no file descriptor format found".to_string(),
            });
        };
        autoreleasepool(|_| self.set_clipboard_item(conn_id, file_descriptor_id))
    }

    fn set_clipboard_item(
        &mut self,
        conn_id: i32,
        file_descriptor_id: i32,
    ) -> Result<(), CliprdrError> {
        let source = Placeholder::new().map_err(|error| CliprdrError::CommonError {
            description: format!("Failed to create clipboard placeholder: {error}"),
        })?;
        let url = NSString::from_str(&format!("file://{}", source.path));
        let item = unsafe {
            let item = NSPasteboardItem::new();
            if !item.setString_forType(&url, NSPasteboardTypeFileURL) {
                return Err(CliprdrError::CommonError {
                    description: "failed to set the clipboard file URL".to_string(),
                });
            }
            item
        };
        let mut state = self.state.lock().unwrap();
        let mut observer = self.observer.lock().unwrap();
        observer
            .start(PasteObserverInfo {
                file_descriptor_id,
                conn_id,
                source_path: source.path.clone(),
                target_path: String::new(),
            })
            .map_err(|error| CliprdrError::CommonError {
                description: format!("Failed to watch for file paste: {error}"),
            })?;
        unsafe {
            self.pasteboard.clearContents();
            if !self
                .pasteboard
                .writeObjects(&Id::cast(NSArray::from_vec(vec![item])))
            {
                state.offer = None;
                observer.stop();
                return Err(CliprdrError::CommonError {
                    description: "failed to write clipboard item".to_string(),
                });
            }
            state.offer = Some(Offer {
                conn_id,
                change_count: self.pasteboard.changeCount(),
                source,
            });
        }
        Ok(())
    }

    fn handle_format_data_response(
        &self,
        conn_id: i32,
        msg_flags: i32,
        format_data: Vec<u8>,
    ) -> Result<(), CliprdrError> {
        let mut state = self.state.lock().unwrap();
        let Some(pending) = state.take_response(conn_id) else {
            return Ok(());
        };
        if state.offer.as_ref().map(|offer| offer.change_count)
            != Some(unsafe { self.pasteboard.changeCount() })
        {
            return Ok(());
        }

        let mut task = self.paste_task.lock().unwrap();
        drop(state);
        if msg_flags != 0x1 {
            return Err(CliprdrError::InvalidRequest {
                description: "The peer could not provide the clipboard file list".to_string(),
            });
        }
        if !pending.target_dir.is_dir() {
            return Err(CliprdrError::CommonError {
                description: "target path does not exist".to_string(),
            });
        }
        let files = FileDescription::parse_file_descriptors(format_data, conn_id)?;
        task.start(pending.target_dir, files);
        Ok(())
    }

    fn handle_file_contents_response(
        &self,
        conn_id: i32,
        msg_flags: i32,
        stream_id: i32,
        requested_data: Vec<u8>,
    ) -> Result<(), CliprdrError> {
        if let Err(error) = self.tx_paste_task.send(FileContentsResponse {
            conn_id,
            msg_flags,
            stream_id,
            requested_data,
        }) {
            log::trace!("File clipboard worker has stopped: {error}");
        }
        Ok(())
    }

    fn handle_try_empty(&mut self, conn_id: i32) {
        log::debug!("empty_clipboard called");
        let ret = self.empty_clipboard_(conn_id);
        log::debug!(
            "empty_clipboard called, conn_id {}, return {}",
            conn_id,
            ret
        );
    }
}

fn clipboard_change_count() -> Option<isize> {
    autoreleasepool(|_| unsafe {
        let pasteboard: Option<Id<NSPasteboard>> =
            msg_send_id![NSPasteboard::class(), generalPasteboard];
        pasteboard.map(|pasteboard| pasteboard.changeCount())
    })
}

fn copied_placeholder_target(target: &Path, source: &Placeholder) -> Option<PathBuf> {
    let source_path = Path::new(&source.path);
    if !target.is_absolute() || target.file_name()? != source_path.file_name()? {
        return None;
    }
    let parent = target.parent()?;
    let metadata = fs::symlink_metadata(target).ok()?;
    if !metadata.is_file() || metadata.len() != 0 || source.matches_source(&metadata) {
        return None;
    }
    Some(parent.to_path_buf())
}

fn handle_paste_result(
    info: &PasteObserverInfo,
    state: &Mutex<PasteState>,
    task: &Mutex<PasteTask>,
) {
    let state_guard = state.lock().unwrap();
    let Some(offer) = &state_guard.offer else {
        return;
    };
    if info.source_path != offer.source.path
        || info.conn_id != offer.conn_id
        || clipboard_change_count() != Some(offer.change_count)
    {
        return;
    }
    let target = Path::new(&info.target_path);
    let Some(target_dir) = copied_placeholder_target(target, &offer.source) else {
        return;
    };
    // macOS can wait for folder-access consent inside unlink.
    drop(state_guard);
    if let Err(error) = fs::remove_file(target) {
        log::warn!("Failed to remove the Finder clipboard placeholder: {error}");
        return;
    }
    let mut state = state.lock().unwrap();
    let Some(offer) = &state.offer else {
        return;
    };
    if info.source_path != offer.source.path
        || info.conn_id != offer.conn_id
        || clipboard_change_count() != Some(offer.change_count)
    {
        return;
    }
    {
        let channels = crate::VEC_MSG_CHANNEL.read().unwrap();
        state.pending.retain(|conn_id, _| {
            channels
                .iter()
                .any(|channel| channel.conn_id == *conn_id && !channel.sender.is_closed())
        });
    }
    // Replies contain no request ID, so each live connection keeps its reservation
    // until the response arrives, even if another connection takes over the clipboard.
    let Entry::Vacant(reservation) = state.pending.entry(info.conn_id) else {
        return;
    };
    if !task.lock().unwrap().is_finished() {
        return;
    }
    let data = ClipboardFile::FormatDataRequest {
        requested_format_id: info.file_descriptor_id,
    };
    if let Err(error) = send_data(info.conn_id, data) {
        log::warn!("Failed to request clipboard file data: {error}");
        return;
    }
    reservation.insert(PendingPaste {
        source_path: info.source_path.clone(),
        target_dir,
    });
}

#[inline]
pub fn create_pasteboard_context() -> ResultType<Box<PasteboardContext>> {
    let pasteboard: Option<Id<NSPasteboard>> =
        unsafe { msg_send_id![NSPasteboard::class(), generalPasteboard] };
    let Some(pasteboard) = pasteboard else {
        bail!("failed to get general pasteboard");
    };
    let state = Arc::new(Mutex::new(PasteState::default()));
    let (tx, rx) = channel();
    let paste_task = Arc::new(Mutex::new(PasteTask::new(rx)));
    let observer_state = state.clone();
    let observer_task = paste_task.clone();
    let mut observer = PasteObserver::new();
    observer.init(move |info| handle_paste_result(info, &observer_state, &observer_task))?;
    let observer = Arc::new(Mutex::new(observer));

    let (stop_cleanup, cleanup) = channel();
    let cleanup_state = state.clone();
    let cleanup_observer = observer.clone();
    let cleanup_thread = thread::Builder::new()
        .name("clipboard-offer".into())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = cleanup.recv_timeout(Duration::from_secs(1))
            {
                let mut state = cleanup_state.lock().unwrap();
                if let Some(offer) = &state.offer {
                    if clipboard_change_count().is_some_and(|count| count != offer.change_count) {
                        state.offer = None;
                        cleanup_observer.lock().unwrap().stop();
                    }
                }
            }
        })?;

    Ok(Box::new(PasteboardContext {
        pasteboard,
        observer,
        state,
        stop_cleanup,
        cleanup_thread: Some(cleanup_thread),
        tx_paste_task: tx,
        paste_task,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_source_survives_repeated_finder_copies() {
        let source = Placeholder::new().unwrap();
        let source_path = PathBuf::from(&source.path);
        let directory = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&directory).unwrap();
        let target = directory.join(source_path.file_name().unwrap());

        assert!(copied_placeholder_target(&source_path, &source).is_none());
        for _ in 0..2 {
            fs::copy(&source_path, &target).unwrap();
            assert_eq!(
                copied_placeholder_target(&target, &source),
                Some(directory.clone())
            );
            fs::remove_file(&target).unwrap();
            assert!(source_path.is_file());
        }

        fs::remove_dir(directory).unwrap();
        drop(source);
        assert!(!source_path.exists());
    }

    #[test]
    fn descriptor_is_bound_to_its_connection_and_original_offer() {
        let mut state = PasteState::default();
        let publish = |state: &mut PasteState, conn_id| -> String {
            let source = Placeholder::new().unwrap();
            let path = source.path.clone();
            state.offer = Some(Offer {
                conn_id,
                change_count: 1,
                source,
            });
            path
        };
        let reserve = |state: &mut PasteState, conn_id, source_path| {
            let Entry::Vacant(reservation) = state.pending.entry(conn_id) else {
                return false;
            };
            reservation.insert(PendingPaste {
                source_path,
                target_dir: PathBuf::from("/tmp"),
            });
            true
        };

        let source_a = publish(&mut state, 1);
        assert!(reserve(&mut state, 1, source_a));
        let source_b = publish(&mut state, 2);
        assert!(reserve(&mut state, 2, source_b));
        assert_eq!(
            state.take_response(2).map(|pending| pending.target_dir),
            Some(PathBuf::from("/tmp"))
        );

        let new_source_a = publish(&mut state, 1);
        assert!(!reserve(&mut state, 1, new_source_a.clone()));
        assert!(state.take_response(2).is_none());
        assert!(state.take_response(1).is_none());
        assert!(reserve(&mut state, 1, new_source_a));
        assert_eq!(
            state.take_response(1).map(|pending| pending.target_dir),
            Some(PathBuf::from("/tmp"))
        );
    }
}

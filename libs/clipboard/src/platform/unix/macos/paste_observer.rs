use super::pasteboard_context::PasteObserverInfo;
use hbb_common::{bail, log, ResultType};
use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

#[path = "paste_observer_native.rs"]
mod native;

enum FseventControl {
    Start(Sender<ResultType<()>>),
    Stop,
}

struct FseventThreadInfo {
    tx: Sender<FseventControl>,
    handle: thread::JoinHandle<()>,
}

pub struct PasteObserver {
    observer_info: Arc<Mutex<Option<PasteObserverInfo>>>,
    tx_handle_fsevent_thread: Option<FseventThreadInfo>,
    handle_observer_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for PasteObserver {
    fn drop(&mut self) {
        self.observer_info.lock().unwrap().take();
        if let Some(FseventThreadInfo { tx, handle }) = self.tx_handle_fsevent_thread.take() {
            drop(tx);
            if handle.join().is_err() {
                log::trace!("File event control thread panicked");
            }
        }
        if let Some(handle) = self.handle_observer_thread.take() {
            if handle.join().is_err() {
                log::trace!("Paste observer thread panicked");
            }
        }
    }
}

impl PasteObserver {
    pub fn new() -> Self {
        Self {
            observer_info: Default::default(),
            tx_handle_fsevent_thread: None,
            handle_observer_thread: None,
        }
    }

    pub fn init(
        &mut self,
        cb_pasted: impl Fn(&PasteObserverInfo) + Send + 'static,
    ) -> ResultType<()> {
        let Some(home_dir) = dirs::home_dir() else {
            bail!("No home dir is set, do not observe.");
        };

        let (tx_observer, rx_observer) = channel();
        let handle_observer =
            Self::init_thread_observer(self.observer_info.clone(), rx_observer, cb_pasted);
        self.handle_observer_thread = Some(handle_observer);
        let (tx_control, rx_control) = channel::<FseventControl>();
        let handle_fsevent = Self::init_thread_fsevent(
            home_dir.to_string_lossy().to_string(),
            tx_observer,
            rx_control,
        );
        self.tx_handle_fsevent_thread = Some(FseventThreadInfo {
            tx: tx_control,
            handle: handle_fsevent,
        });
        Ok(())
    }

    #[inline]
    fn get_file_from_path(path: &str) -> &str {
        let last_slash = path.rfind('/').or_else(|| path.rfind('\\'));
        match last_slash {
            Some(index) => &path[index + 1..],
            None => path,
        }
    }

    fn init_thread_observer(
        observer_info: Arc<Mutex<Option<PasteObserverInfo>>>,
        rx_observer: Receiver<String>,
        cb_pasted: impl Fn(&PasteObserverInfo) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while let Ok(path) = rx_observer.recv() {
                let matching = observer_info
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|info| {
                        Self::get_file_from_path(&info.source_path)
                            == Self::get_file_from_path(&path)
                    })
                    .cloned();
                if let Some(mut info) = matching {
                    info.target_path = path;
                    cb_pasted(&info);
                }
            }
        })
    }

    fn init_thread_fsevent(
        home_dir: String,
        tx_observer: Sender<String>,
        rx_control: Receiver<FseventControl>,
    ) -> thread::JoinHandle<()> {
        log::debug!("fsevent observe dir: {}", &home_dir);
        thread::spawn(move || {
            let mut fsevent = None;
            while let Ok(command) = rx_control.recv() {
                match command {
                    FseventControl::Start(ready) => {
                        let result = if fsevent.is_some() {
                            Ok(())
                        } else {
                            native::FseventStream::start(&home_dir, tx_observer.clone())
                                .map(|stream| fsevent = Some(stream))
                        };
                        if ready.send(result).is_err() {
                            log::trace!(
                                "Paste observer stopped before starting its file event stream"
                            );
                        }
                    }
                    FseventControl::Stop => {
                        fsevent = None;
                    }
                }
            }
        })
    }

    pub fn start(&mut self, observer_info: PasteObserverInfo) -> ResultType<()> {
        let Some(control) = &self.tx_handle_fsevent_thread else {
            bail!("Paste observer is not initialized");
        };
        let (ready, result) = channel();
        control.tx.send(FseventControl::Start(ready))?;
        result.recv()??;
        self.observer_info.lock().unwrap().replace(observer_info);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.observer_info.lock().unwrap().take();
        if let Some(control) = &self.tx_handle_fsevent_thread {
            if control.tx.send(FseventControl::Stop).is_err() {
                log::trace!("Paste observer file event control already stopped");
            }
        }
    }
}

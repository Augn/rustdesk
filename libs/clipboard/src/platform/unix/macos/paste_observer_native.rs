use fsevent_sys::{
    core_foundation::{
        kCFAllocatorDefault, kCFStringEncodingUTF8, kCFTypeArrayCallBacks, CFArrayAppendValue,
        CFArrayCreateMutable, CFRelease, CFStringCreateWithCString,
    },
    kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamEventFlagItemCreated, kFSEventStreamEventFlagItemIsFile,
    kFSEventStreamEventFlagItemRemoved, kFSEventStreamEventIdSinceNow, FSEventStreamContext,
    FSEventStreamCreate, FSEventStreamEventFlags, FSEventStreamEventId, FSEventStreamInvalidate,
    FSEventStreamRef, FSEventStreamRelease, FSEventStreamStart, FSEventStreamStop,
};
use hbb_common::{bail, ResultType};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    ptr,
    sync::{mpsc::Sender, Arc},
};

type DispatchQueue = *mut c_void;

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
}

#[link(name = "System")]
extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_release(queue: DispatchQueue);
}

pub(super) struct FseventStream {
    stream: FSEventStreamRef,
    queue: DispatchQueue,
}

impl FseventStream {
    pub(super) fn start(home: &str, events: Sender<String>) -> ResultType<Self> {
        let home = CString::new(home)?;
        let events = Arc::new(events);
        // CoreServices retains the sender through the stream context callbacks.
        let context = FSEventStreamContext {
            version: 0,
            info: Arc::as_ptr(&events) as *mut c_void,
            retain: Some(retain_events),
            release: Some(release_events),
            copy_description: None,
        };
        unsafe {
            let path = CFStringCreateWithCString(
                kCFAllocatorDefault,
                home.as_ptr(),
                kCFStringEncodingUTF8,
            );
            if path.is_null() {
                bail!("Could not convert the home directory to a file event path");
            }
            let paths = CFArrayCreateMutable(kCFAllocatorDefault, 1, &kCFTypeArrayCallBacks);
            if paths.is_null() {
                CFRelease(path);
                bail!("Could not create the file event paths array");
            }
            CFArrayAppendValue(paths, path);
            CFRelease(path);
            let stream = FSEventStreamCreate(
                kCFAllocatorDefault,
                on_events,
                &context,
                paths,
                kFSEventStreamEventIdSinceNow,
                0.0,
                kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer,
            );
            CFRelease(paths);
            if stream.is_null() {
                bail!("Could not create the file event stream for the home directory");
            }

            let queue =
                dispatch_queue_create(b"rustdesk.paste-fsevents\0".as_ptr().cast(), ptr::null());
            if queue.is_null() {
                FSEventStreamRelease(stream);
                bail!("Could not create the file event queue");
            }
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                bail!("Could not start the file event stream for the home directory");
            }
            Ok(Self { stream, queue })
        }
    }
}

impl Drop for FseventStream {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
        }
    }
}

extern "C" fn retain_events(info: *const c_void) -> *const c_void {
    unsafe { Arc::increment_strong_count(info.cast::<Sender<String>>()) };
    info
}

extern "C" fn release_events(info: *const c_void) {
    unsafe { Arc::decrement_strong_count(info.cast::<Sender<String>>()) };
}

extern "C" fn on_events(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const FSEventStreamEventFlags,
    _ids: *const FSEventStreamEventId,
) {
    // Without kFSEventStreamCreateFlagUseCFTypes, paths is an array of C strings.
    let paths = paths.cast::<*const c_char>();
    let events = unsafe { &*info.cast::<Sender<String>>() };
    for index in 0..count {
        let flag = unsafe { *flags.add(index) };
        if flag & kFSEventStreamEventFlagItemCreated == 0
            || flag & kFSEventStreamEventFlagItemRemoved != 0
            || flag & kFSEventStreamEventFlagItemIsFile == 0
        {
            continue;
        }
        let path = unsafe { CStr::from_ptr(*paths.add(index)) };
        if let Ok(path) = path.to_str() {
            if events.send(path.to_owned()).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, sync::mpsc::channel, time::Duration};

    #[test]
    fn reports_file_created_immediately_after_start() -> ResultType<()> {
        let folder = env::temp_dir()
            .canonicalize()?
            .join(format!("rustdesk-fsevents-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder)?;
        let file = folder.join("paste-marker");
        let result = (|| -> ResultType<String> {
            let (tx, rx) = channel();
            let _stream = FseventStream::start(&folder.to_string_lossy(), tx)?;
            fs::write(&file, [])?;
            Ok(rx.recv_timeout(Duration::from_secs(5))?)
        })();
        fs::remove_dir_all(folder)?;
        assert_eq!(result?, file.to_string_lossy());
        Ok(())
    }
}

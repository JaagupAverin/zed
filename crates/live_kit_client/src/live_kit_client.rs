use anyhow::{anyhow, Context, Result};
use core_foundation::{
    array::{CFArray, CFArrayRef},
    base::{CFRelease, CFRetain, TCFType},
    string::{CFString, CFStringRef},
};
use futures::{
    channel::{mpsc, oneshot},
    Future,
};
use media::core_video::{CVImageBuffer, CVImageBufferRef};
use parking_lot::Mutex;
use std::{
    ffi::c_void,
    sync::{Arc, Weak},
};

pub type Sid = String;

extern "C" {
    fn LKRoomDelegateCreate(
        callback_data: *mut c_void,
        on_did_subscribe_to_remote_video_track: extern "C" fn(
            callback_data: *mut c_void,
            publisher_id: CFStringRef,
            track_id: CFStringRef,
            remote_track: *const c_void,
        ),
        on_did_unsubscribe_from_remote_video_track: extern "C" fn(
            callback_data: *mut c_void,
            publisher_id: CFStringRef,
            track_id: CFStringRef,
        ),
    ) -> *const c_void;

    fn LKRoomCreate(delegate: *const c_void) -> *const c_void;
    fn LKRoomConnect(
        room: *const c_void,
        url: CFStringRef,
        token: CFStringRef,
        callback: extern "C" fn(*mut c_void, CFStringRef),
        callback_data: *mut c_void,
    );
    fn LKRoomDisconnect(room: *const c_void);
    fn LKRoomPublishVideoTrack(
        room: *const c_void,
        track: *const c_void,
        callback: extern "C" fn(*mut c_void, CFStringRef),
        callback_data: *mut c_void,
    );
    fn LKRoomVideoTracksForRemoteParticipant(
        room: *const c_void,
        participant_id: CFStringRef,
    ) -> CFArrayRef;

    fn LKVideoRendererCreate(
        callback_data: *mut c_void,
        on_frame: extern "C" fn(callback_data: *mut c_void, frame: CVImageBufferRef),
        on_drop: extern "C" fn(callback_data: *mut c_void),
    ) -> *const c_void;

    fn LKVideoTrackAddRenderer(track: *const c_void, renderer: *const c_void);
    fn LKRemoteVideoTrackGetSid(track: *const c_void) -> CFStringRef;

    fn LKDisplaySources(
        callback_data: *mut c_void,
        callback: extern "C" fn(
            callback_data: *mut c_void,
            sources: CFArrayRef,
            error: CFStringRef,
        ),
    );
    fn LKCreateScreenShareTrackForDisplay(display: *const c_void) -> *const c_void;
}

pub struct Room {
    native_room: *const c_void,
    remote_video_track_subscribers: Mutex<Vec<mpsc::UnboundedSender<RemoteVideoTrackUpdate>>>,
    _delegate: RoomDelegate,
}

impl Room {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak_room| {
            let delegate = RoomDelegate::new(weak_room.clone());
            Self {
                native_room: unsafe { LKRoomCreate(delegate.native_delegate) },
                remote_video_track_subscribers: Default::default(),
                _delegate: delegate,
            }
        })
    }

    pub fn connect(&self, url: &str, token: &str) -> impl Future<Output = Result<()>> {
        let url = CFString::new(url);
        let token = CFString::new(token);
        let (did_connect, tx, rx) = Self::build_done_callback();
        unsafe {
            LKRoomConnect(
                self.native_room,
                url.as_concrete_TypeRef(),
                token.as_concrete_TypeRef(),
                did_connect,
                tx,
            )
        }

        async { rx.await.unwrap().context("error connecting to room") }
    }

    pub fn publish_video_track(&self, track: &LocalVideoTrack) -> impl Future<Output = Result<()>> {
        let (did_publish, tx, rx) = Self::build_done_callback();
        unsafe {
            LKRoomPublishVideoTrack(self.native_room, track.0, did_publish, tx);
        }
        async { rx.await.unwrap().context("error publishing video track") }
    }

    pub fn remote_video_tracks(&self, participant_id: &str) -> Vec<Arc<RemoteVideoTrack>> {
        unsafe {
            let tracks = LKRoomVideoTracksForRemoteParticipant(
                self.native_room,
                CFString::new(participant_id).as_concrete_TypeRef(),
            );

            if tracks.is_null() {
                Vec::new()
            } else {
                let tracks = CFArray::wrap_under_get_rule(tracks);
                tracks
                    .into_iter()
                    .map(|native_track| {
                        let native_track = *native_track;
                        let id =
                            CFString::wrap_under_get_rule(LKRemoteVideoTrackGetSid(native_track))
                                .to_string();
                        Arc::new(RemoteVideoTrack::new(
                            native_track,
                            id,
                            participant_id.into(),
                        ))
                    })
                    .collect()
            }
        }
    }

    pub fn remote_video_track_updates(&self) -> mpsc::UnboundedReceiver<RemoteVideoTrackUpdate> {
        let (tx, rx) = mpsc::unbounded();
        self.remote_video_track_subscribers.lock().push(tx);
        rx
    }

    fn did_subscribe_to_remote_video_track(&self, track: RemoteVideoTrack) {
        let track = Arc::new(track);
        self.remote_video_track_subscribers.lock().retain(|tx| {
            tx.unbounded_send(RemoteVideoTrackUpdate::Subscribed(track.clone()))
                .is_ok()
        });
    }

    fn did_unsubscribe_from_remote_video_track(&self, publisher_id: String, track_id: String) {
        self.remote_video_track_subscribers.lock().retain(|tx| {
            tx.unbounded_send(RemoteVideoTrackUpdate::Unsubscribed {
                publisher_id: publisher_id.clone(),
                track_id: track_id.clone(),
            })
            .is_ok()
        });
    }

    fn build_done_callback() -> (
        extern "C" fn(*mut c_void, CFStringRef),
        *mut c_void,
        oneshot::Receiver<Result<()>>,
    ) {
        let (tx, rx) = oneshot::channel();
        extern "C" fn done_callback(tx: *mut c_void, error: CFStringRef) {
            let tx = unsafe { Box::from_raw(tx as *mut oneshot::Sender<Result<()>>) };
            if error.is_null() {
                let _ = tx.send(Ok(()));
            } else {
                let error = unsafe { CFString::wrap_under_get_rule(error).to_string() };
                let _ = tx.send(Err(anyhow!(error)));
            }
        }
        (
            done_callback,
            Box::into_raw(Box::new(tx)) as *mut c_void,
            rx,
        )
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        unsafe {
            LKRoomDisconnect(self.native_room);
            CFRelease(self.native_room);
        }
    }
}

struct RoomDelegate {
    native_delegate: *const c_void,
    weak_room: *const Room,
}

impl RoomDelegate {
    fn new(weak_room: Weak<Room>) -> Self {
        let weak_room = Weak::into_raw(weak_room);
        let native_delegate = unsafe {
            LKRoomDelegateCreate(
                weak_room as *mut c_void,
                Self::on_did_subscribe_to_remote_video_track,
                Self::on_did_unsubscribe_from_remote_video_track,
            )
        };
        Self {
            native_delegate,
            weak_room,
        }
    }

    extern "C" fn on_did_subscribe_to_remote_video_track(
        room: *mut c_void,
        publisher_id: CFStringRef,
        track_id: CFStringRef,
        track: *const c_void,
    ) {
        let room = unsafe { Weak::from_raw(room as *mut Room) };
        let publisher_id = unsafe { CFString::wrap_under_get_rule(publisher_id).to_string() };
        let track_id = unsafe { CFString::wrap_under_get_rule(track_id).to_string() };
        let track = RemoteVideoTrack::new(track, track_id, publisher_id);
        if let Some(room) = room.upgrade() {
            room.did_subscribe_to_remote_video_track(track);
        }
    }

    extern "C" fn on_did_unsubscribe_from_remote_video_track(
        room: *mut c_void,
        publisher_id: CFStringRef,
        track_id: CFStringRef,
    ) {
        let room = unsafe { Weak::from_raw(room as *mut Room) };
        let publisher_id = unsafe { CFString::wrap_under_get_rule(publisher_id).to_string() };
        let track_id = unsafe { CFString::wrap_under_get_rule(track_id).to_string() };
        if let Some(room) = room.upgrade() {
            room.did_unsubscribe_from_remote_video_track(publisher_id, track_id);
        }
        let _ = Weak::into_raw(room);
    }
}

impl Drop for RoomDelegate {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.native_delegate);
            let _ = Weak::from_raw(self.weak_room);
        }
    }
}

pub struct LocalVideoTrack(*const c_void);

impl LocalVideoTrack {
    pub fn screen_share_for_display(display: &MacOSDisplay) -> Self {
        Self(unsafe { LKCreateScreenShareTrackForDisplay(display.0) })
    }
}

impl Drop for LocalVideoTrack {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

#[derive(Debug)]
pub struct RemoteVideoTrack {
    native_track: *const c_void,
    sid: Sid,
    publisher_id: String,
}

impl RemoteVideoTrack {
    fn new(native_track: *const c_void, sid: Sid, publisher_id: String) -> Self {
        unsafe {
            CFRetain(native_track);
        }
        Self {
            native_track,
            sid,
            publisher_id,
        }
    }

    pub fn sid(&self) -> &str {
        &self.sid
    }

    pub fn publisher_id(&self) -> &str {
        &self.publisher_id
    }

    pub fn add_renderer<F>(&self, callback: F)
    where
        F: 'static + FnMut(CVImageBuffer),
    {
        extern "C" fn on_frame<F>(callback_data: *mut c_void, frame: CVImageBufferRef)
        where
            F: FnMut(CVImageBuffer),
        {
            unsafe {
                let buffer = CVImageBuffer::wrap_under_get_rule(frame);
                let callback = &mut *(callback_data as *mut F);
                callback(buffer);
            }
        }

        extern "C" fn on_drop<F>(callback_data: *mut c_void) {
            unsafe {
                let _ = Box::from_raw(callback_data as *mut F);
            }
        }

        let callback_data = Box::into_raw(Box::new(callback));
        unsafe {
            let renderer =
                LKVideoRendererCreate(callback_data as *mut c_void, on_frame::<F>, on_drop::<F>);
            LKVideoTrackAddRenderer(self.native_track, renderer);
        }
    }
}

impl Drop for RemoteVideoTrack {
    fn drop(&mut self) {
        unsafe { CFRelease(self.native_track) }
    }
}

pub enum RemoteVideoTrackUpdate {
    Subscribed(Arc<RemoteVideoTrack>),
    Unsubscribed { publisher_id: Sid, track_id: Sid },
}

pub struct MacOSDisplay(*const c_void);

impl MacOSDisplay {
    fn new(ptr: *const c_void) -> Self {
        unsafe {
            CFRetain(ptr);
        }
        Self(ptr)
    }
}

impl Drop for MacOSDisplay {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

pub fn display_sources() -> impl Future<Output = Result<Vec<MacOSDisplay>>> {
    extern "C" fn callback(tx: *mut c_void, sources: CFArrayRef, error: CFStringRef) {
        unsafe {
            let tx = Box::from_raw(tx as *mut oneshot::Sender<Result<Vec<MacOSDisplay>>>);

            if sources.is_null() {
                let _ = tx.send(Err(anyhow!("{}", CFString::wrap_under_get_rule(error))));
            } else {
                let sources = CFArray::wrap_under_get_rule(sources)
                    .into_iter()
                    .map(|source| MacOSDisplay::new(*source))
                    .collect();

                let _ = tx.send(Ok(sources));
            }
        }
    }

    let (tx, rx) = oneshot::channel();

    unsafe {
        LKDisplaySources(Box::into_raw(Box::new(tx)) as *mut _, callback);
    }

    async move { rx.await.unwrap() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_client() {}
}

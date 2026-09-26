//! "Lenny" on Windows 11: the custom media source behind `MFCreateVirtualCamera` (architecture.md §7.4b). The
//! desktop app registers the virtual camera with this CLSID; Frame Server (svchost, LOCAL SERVICE, session 0)
//! creates our IMFActivate and from it the media source, one NV12 1280x720 30 fps capture stream. Frames come from
//! the same shared-memory ring as the DirectShow filter; it must be the `Global\` one, since session 0 can't see
//! `Local\` names (the desktop app falls back to `Local\` without the privilege: then this shows the placeholder).
//!
//! Shape follows Microsoft's SimpleMediaSource virtual camera sample. Same crash containment as the filter: a
//! crash here would kill Frame Server's worker and with it every camera on the system.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::Weak;

use crate::frames::Frames;
use crate::media::{FRAME_TIME, HEIGHT, WIDTH};
use crate::{guard, Sendable};

/// lenny_framebuf::MF_SOURCE_CLSID.
pub const SOURCE_CLSID: GUID = GUID::from_u128(0x5F0F9024_D043_45C1_B2F6_03DAB6CE130A);

fn startup() {
    static ONCE: Once = Once::new();
    // Frame Server has MF running already; this is for any other host. Never shut down (it's refcounted).
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });
}

fn ks_unsupported() -> Result<()> {
    Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
}

// ---- activate: what Frame Server CoCreates ----

#[implement(IMFActivate)]
struct Activate {
    attrs: IMFAttributes,
    source: Mutex<Option<IMFMediaSource>>,
}

pub fn create_activate() -> Result<IMFActivate> {
    startup();
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    Ok(Activate { attrs: attrs.ok_or(E_POINTER)?, source: Mutex::new(None) }.into())
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        guard(|| {
            let mut slot = self.source.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(create_source()?);
            }
            let source = slot.as_ref().ok_or(E_POINTER)?;
            unsafe { source.query(riid, ppv).ok() }
        })
    }

    fn ShutdownObject(&self) -> Result<()> {
        guard(|| {
            if let Some(s) = self.source.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = unsafe { s.Shutdown() };
            }
            Ok(())
        })
    }

    fn DetachObject(&self) -> Result<()> {
        guard(|| {
            self.source.lock().unwrap_or_else(|e| e.into_inner()).take();
            Ok(())
        })
    }
}

/// IMFActivate is an IMFAttributes: forward everything to a real attribute store.
impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItem(key, (!value.is_null()).then_some(value)) }
    }
    fn GetItemType(&self, key: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        unsafe { self.attrs.GetItemType(key) }
    }
    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<BOOL> {
        unsafe { self.attrs.CompareItem(key, value) }
    }
    fn Compare(&self, theirs: Ref<IMFAttributes>, how: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> {
        unsafe { self.attrs.Compare(theirs.as_ref(), how) }
    }
    fn GetUINT32(&self, key: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetUINT32(key) }
    }
    fn GetUINT64(&self, key: *const GUID) -> Result<u64> {
        unsafe { self.attrs.GetUINT64(key) }
    }
    fn GetDouble(&self, key: *const GUID) -> Result<f64> {
        unsafe { self.attrs.GetDouble(key) }
    }
    fn GetGUID(&self, key: *const GUID) -> Result<GUID> {
        unsafe { self.attrs.GetGUID(key) }
    }
    fn GetStringLength(&self, key: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetStringLength(key) }
    }
    fn GetString(&self, key: *const GUID, buf: PWSTR, len: u32, out_len: *mut u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetString)(self.attrs.as_raw(), key, buf, len, out_len).ok() }
    }
    fn GetAllocatedString(&self, key: *const GUID, out: *mut PWSTR, len: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedString(key, out, len) }
    }
    fn GetBlobSize(&self, key: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetBlobSize(key) }
    }
    fn GetBlob(&self, key: *const GUID, buf: *mut u8, size: u32, out_size: *mut u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetBlob)(self.attrs.as_raw(), key, buf, size, out_size).ok() }
    }
    fn GetAllocatedBlob(&self, key: *const GUID, buf: *mut *mut u8, size: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedBlob(key, buf, size) }
    }
    fn GetUnknown(&self, key: *const GUID, riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetUnknown)(self.attrs.as_raw(), key, riid, ppv).ok() }
    }
    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.SetItem(key, value) }
    }
    fn DeleteItem(&self, key: *const GUID) -> Result<()> {
        unsafe { self.attrs.DeleteItem(key) }
    }
    fn DeleteAllItems(&self) -> Result<()> {
        unsafe { self.attrs.DeleteAllItems() }
    }
    fn SetUINT32(&self, key: *const GUID, v: u32) -> Result<()> {
        unsafe { self.attrs.SetUINT32(key, v) }
    }
    fn SetUINT64(&self, key: *const GUID, v: u64) -> Result<()> {
        unsafe { self.attrs.SetUINT64(key, v) }
    }
    fn SetDouble(&self, key: *const GUID, v: f64) -> Result<()> {
        unsafe { self.attrs.SetDouble(key, v) }
    }
    fn SetGUID(&self, key: *const GUID, v: *const GUID) -> Result<()> {
        unsafe { self.attrs.SetGUID(key, v) }
    }
    fn SetString(&self, key: *const GUID, v: &PCWSTR) -> Result<()> {
        unsafe { self.attrs.SetString(key, *v) }
    }
    fn SetBlob(&self, key: *const GUID, buf: *const u8, size: u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).SetBlob)(self.attrs.as_raw(), key, buf, size).ok() }
    }
    fn SetUnknown(&self, key: *const GUID, v: Ref<IUnknown>) -> Result<()> {
        unsafe { self.attrs.SetUnknown(key, v.as_ref()) }
    }
    fn LockStore(&self) -> Result<()> {
        unsafe { self.attrs.LockStore() }
    }
    fn UnlockStore(&self) -> Result<()> {
        unsafe { self.attrs.UnlockStore() }
    }
    fn GetCount(&self) -> Result<u32> {
        unsafe { self.attrs.GetCount() }
    }
    fn GetItemByIndex(&self, i: u32, key: *mut GUID, value: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItemByIndex(i, key, (!value.is_null()).then_some(value)) }
    }
    fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> Result<()> {
        unsafe { self.attrs.CopyAllItems(dest.as_ref()) }
    }
}

// ---- event generator plumbing, shared by source and stream ----

macro_rules! event_generator {
    ($t:ty) => {
        impl IMFMediaEventGenerator_Impl for $t {
            fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
                guard(|| unsafe { self.queue.GetEvent(flags.0) })
            }
            fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
                guard(|| unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) })
            }
            fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
                guard(|| unsafe { self.queue.EndGetEvent(result.as_ref()) })
            }
            fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, value: *const PROPVARIANT) -> Result<()> {
                guard(|| unsafe { self.queue.QueueEventParamVar(met, ext, hr, value) })
            }
        }

        /// Frame Server probes camera properties through IKsControl; we have none.
        impl IKsControl_Impl for $t {
            fn KsProperty(
                &self,
                _: *const KSIDENTIFIER,
                _: u32,
                _: *mut core::ffi::c_void,
                _: u32,
                _: *mut u32,
            ) -> Result<()> {
                ks_unsupported()
            }
            fn KsMethod(
                &self,
                _: *const KSIDENTIFIER,
                _: u32,
                _: *mut core::ffi::c_void,
                _: u32,
                _: *mut u32,
            ) -> Result<()> {
                ks_unsupported()
            }
            fn KsEvent(
                &self,
                _: *const KSIDENTIFIER,
                _: u32,
                _: *mut core::ffi::c_void,
                _: u32,
                _: *mut u32,
            ) -> Result<()> {
                ks_unsupported()
            }
        }
    };
}

// ---- media source ----

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
struct Source {
    queue: IMFMediaEventQueue,
    attrs: IMFAttributes,
    pd: IMFPresentationDescriptor,
    stream: windows_core::ComObject<Stream>,
    shut: AtomicBool,
}

fn media_type() -> Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((WIDTH as u64) << 32) | HEIGHT as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, WIDTH as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        mt.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        mt.SetUINT32(&MF_MT_SAMPLE_SIZE, frame_bytes() as u32)?;
        Ok(mt)
    }
}

fn frame_bytes() -> usize {
    lenny_framebuf::nv12_bytes(WIDTH as u32, HEIGHT as u32)
}

fn create_source() -> Result<IMFMediaSource> {
    unsafe {
        let mt = media_type()?;
        let sd = MFCreateStreamDescriptor(0, &[Some(mt.clone())])?;
        sd.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
        sd.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        sd.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        sd.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        sd.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
        pd.SelectStream(0)?;
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;

        let stream = windows_core::ComObject::new(Stream {
            queue: MFCreateEventQueue()?,
            sd,
            source: Mutex::new(Weak::new()),
            state: Mutex::new(MF_STREAM_STATE_STOPPED),
            worker: Mutex::new(None),
            faulted: Arc::new(AtomicBool::new(false)),
        });
        let source: IMFMediaSourceEx = Source {
            queue: MFCreateEventQueue()?,
            attrs: attrs.ok_or(E_POINTER)?,
            pd,
            stream: stream.clone(),
            shut: AtomicBool::new(false),
        }
        .into();
        let source: IMFMediaSource = source.cast()?;
        // Weak back-reference: source and stream would otherwise keep each other alive forever.
        if let Ok(weak) = source.downgrade() {
            *stream.source.lock().unwrap_or_else(|e| e.into_inner()) = weak;
        }
        Ok(source)
    }
}

impl Source_Impl {
    fn check(&self) -> Result<()> {
        if self.shut.load(Ordering::Relaxed) {
            Err(MF_E_SHUTDOWN.into())
        } else {
            Ok(())
        }
    }
}

event_generator!(Source_Impl);

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        guard(|| {
            self.check()?;
            Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
        })
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        guard(|| {
            self.check()?;
            unsafe { self.pd.Clone() }
        })
    }

    fn Start(
        &self,
        _pd: Ref<IMFPresentationDescriptor>,
        _format: *const GUID,
        start: *const PROPVARIANT,
    ) -> Result<()> {
        guard(|| unsafe {
            self.check()?;
            let stream: IMFMediaStream2 = self.stream.to_interface(); // an IMFMediaStream too
            self.queue.QueueEventParamUnk(MENewStream.0 as u32, &GUID::zeroed(), S_OK, &stream)?;
            self.stream.start(start)?;
            self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, start)
        })
    }

    fn Stop(&self) -> Result<()> {
        guard(|| unsafe {
            self.check()?;
            self.stream.stop()?;
            self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null())
        })
    }

    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into()) // live source
    }

    fn Shutdown(&self) -> Result<()> {
        guard(|| {
            if !self.shut.swap(true, Ordering::Relaxed) {
                self.stream.shutdown();
                let _ = unsafe { self.queue.Shutdown() };
            }
            Ok(())
        })
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        guard(|| {
            self.check()?;
            Ok(self.attrs.clone())
        })
    }

    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        guard(|| {
            self.check()?;
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            self.stream.sd.cast()
        })
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> Result<()> {
        Ok(()) // system-memory samples only
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(&self, _: *const GUID, _: *const GUID, _: *mut *mut core::ffi::c_void) -> Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

// ---- stream ----

/// Delivers one sample per RequestSample, paced to 30 fps, off the caller's thread.
struct Worker {
    requests: Sender<Sendable<Option<IUnknown>>>,
    thread: JoinHandle<()>,
}

#[implement(IMFMediaStream2, IKsControl)]
struct Stream {
    queue: IMFMediaEventQueue,
    sd: IMFStreamDescriptor,
    source: Mutex<Weak<IMFMediaSource>>,
    state: Mutex<MF_STREAM_STATE>,
    worker: Mutex<Option<Worker>>,
    /// A caught panic: placeholder only from now on.
    faulted: Arc<AtomicBool>,
}

impl Stream {
    fn start(&self, at: *const PROPVARIANT) -> Result<()> {
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if worker.is_none() {
            let (tx, rx) = mpsc::channel::<Sendable<Option<IUnknown>>>();
            let job = Sendable((self.queue.clone(), self.faulted.clone()));
            let thread = std::thread::Builder::new()
                .name("lenny-mf".into())
                .spawn(move || {
                    let (queue, faulted) = job.get();
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| deliver(&queue, &rx, &faulted)));
                })
                .map_err(|_| windows::Win32::Foundation::E_OUTOFMEMORY)?;
            *worker = Some(Worker { requests: tx, thread });
        }
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = MF_STREAM_STATE_RUNNING;
        unsafe { self.queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, at) }
    }

    fn stop_worker(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = MF_STREAM_STATE_STOPPED;
        if let Some(w) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            drop(w.requests); // ends the worker's loop
            let _ = w.thread.join();
        }
    }

    fn stop(&self) -> Result<()> {
        self.stop_worker();
        unsafe { self.queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }

    fn shutdown(&self) {
        self.stop_worker();
        let _ = unsafe { self.queue.Shutdown() };
    }
}

fn make_sample(frame: &[u8], token: Option<&IUnknown>) -> Result<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(frame.len() as u32)?;
        let mut p = std::ptr::null_mut();
        let mut max = 0u32;
        buf.Lock(&mut p, Some(&mut max), None)?;
        if !p.is_null() && max as usize >= frame.len() {
            std::ptr::copy_nonoverlapping(frame.as_ptr(), p, frame.len());
        }
        buf.Unlock()?;
        buf.SetCurrentLength(frame.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buf)?;
        sample.SetSampleTime(MFGetSystemTime())?;
        sample.SetSampleDuration(FRAME_TIME)?;
        if let Some(t) = token {
            sample.SetUnknown(&MFSampleExtension_Token, t)?;
        }
        Ok(sample)
    }
}

// ponytail: one fresh MF buffer per frame; an IMFVideoSampleAllocator pool if Frame Server shows allocation cost.
fn deliver(queue: &IMFMediaEventQueue, requests: &mpsc::Receiver<Sendable<Option<IUnknown>>>, faulted: &AtomicBool) {
    let mut frames = Frames::new();
    let period = Duration::from_nanos(100 * FRAME_TIME as u64);
    let mut next = Instant::now();
    while let Ok(token) = requests.recv() {
        let token = token.get();
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next = next.max(now) + period;
        let picture = frames.next(faulted);
        let Ok(sample) = make_sample(picture, token.as_ref()) else { continue };
        let _ = unsafe { queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample) };
    }
}

event_generator!(Stream_Impl);

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        guard(|| self.source.lock().unwrap_or_else(|e| e.into_inner()).upgrade().ok_or_else(|| MF_E_SHUTDOWN.into()))
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        guard(|| {
            if *self.state.lock().unwrap_or_else(|e| e.into_inner()) != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_INVALIDREQUEST.into());
            }
            let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            let w = worker.as_ref().ok_or(MF_E_INVALIDREQUEST)?;
            w.requests.send(Sendable(token.cloned())).map_err(|_| MF_E_SHUTDOWN.into())
        })
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        guard(|| {
            match value {
                MF_STREAM_STATE_RUNNING => self.start(std::ptr::null())?,
                MF_STREAM_STATE_STOPPED => self.stop()?,
                _ => return Err(MF_E_INVALID_STATE_TRANSITION.into()),
            }
            Ok(())
        })
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(*self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

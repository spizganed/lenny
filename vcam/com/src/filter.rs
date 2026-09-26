//! The filter object, its one output pin ("Capture"), the enumerators, and the streaming thread.
//! Hand-rolled on plain COM (no DirectShow BaseClasses), following the CSource/CSourceStream behaviour: the pin
//! connects and picks an allocator, the streaming thread starts on Pause and delivers one sample per 1/30 s until
//! Stop.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lenny_framebuf as fb;
use windows::core::{implement, Interface, Ref, Result, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::*;
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::{
    AMPROPSETID_Pin, CLSID_MemoryAllocator, FORMAT_VideoInfo, AM_MEDIA_TYPE, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemAlloc, IPersist_Impl, CLSCTX_INPROC_SERVER};
use windows_core::{IUnknownImpl, Weak};

use crate::frames::Frames;
use crate::media::{self, Pixel, FORMATS, FRAME_TIME, HEIGHT, WIDTH};
use crate::server::FILTER_CLSID;
use crate::{guard, guard_hr, Sendable};

/// ksmedia.h; windows-rs only has it under DirectSound.
const KSPROPERTY_SUPPORT_GET: u32 = 1;
const PIN_NAME: &str = "Capture";

struct State {
    filter_state: FILTER_STATE,
    /// Non-owning, as DirectShow requires (the graph owns the filter).
    graph: *mut core::ffi::c_void,
    name: Vec<u16>,
    clock: Option<IReferenceClock>,
    pixel: Pixel,
    peer: Option<IPin>,
    input: Option<IMemInputPin>,
    alloc: Option<IMemAllocator>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

unsafe impl Send for State {}

struct Shared {
    st: Mutex<State>,
    /// A caught panic: placeholder only from now on, in this process.
    faulted: Arc<AtomicBool>,
    /// Run's tStart (reference-clock time of stream time 0); NOT_RUNNING otherwise.
    run_start: Arc<AtomicI64>,
}

const NOT_RUNNING: i64 = i64::MIN;

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Creates a filter (IBaseFilter) with its pin.
pub fn create_filter() -> Result<IBaseFilter> {
    let shared = Arc::new(Shared {
        st: Mutex::new(State {
            filter_state: State_Stopped,
            graph: std::ptr::null_mut(),
            name: vec![],
            clock: None,
            pixel: Pixel::Nv12,
            peer: None,
            input: None,
            alloc: None,
            worker: None,
            stop: Arc::new(AtomicBool::new(false)),
        }),
        faulted: Arc::new(AtomicBool::new(false)),
        run_start: Arc::new(AtomicI64::new(NOT_RUNNING)),
    });
    let pin = windows_core::ComObject::new(Pin { shared: shared.clone(), filter: Mutex::new(Weak::new()) });
    let pin_iface: IPin = pin.to_interface();
    let filter: IBaseFilter = Filter { shared, pin: pin_iface }.into();
    // The pin points back at its filter weakly: a strong reference both ways would never be freed.
    if let Ok(weak) = filter.downgrade() {
        *pin.filter.lock().unwrap_or_else(|e| e.into_inner()) = weak;
    }
    Ok(filter)
}

// ---- filter ----

#[implement(IBaseFilter)]
struct Filter {
    shared: Arc<Shared>,
    pin: IPin,
}

impl IPersist_Impl for Filter_Impl {
    fn GetClassID(&self) -> Result<GUID> {
        Ok(FILTER_CLSID)
    }
}

impl IMediaFilter_Impl for Filter_Impl {
    fn Stop(&self) -> Result<()> {
        guard(|| {
            self.shared.run_start.store(NOT_RUNNING, Ordering::Relaxed);
            let mut st = self.shared.lock();
            stop_streaming(&mut st);
            if let Some(a) = &st.alloc {
                unsafe {
                    let _ = a.Decommit();
                }
            }
            st.filter_state = State_Stopped;
            Ok(())
        })
    }

    fn Pause(&self) -> Result<()> {
        guard(|| {
            let mut st = self.shared.lock();
            if st.filter_state == State_Stopped && st.input.is_some() {
                if let Some(a) = &st.alloc {
                    unsafe { a.Commit()? };
                }
                // Like CSourceStream: the streaming thread runs from Pause on, so renderers get their preroll
                // sample and the graph reaches Paused (a live source can't return VFW_S_CANT_CUE through this API).
                start_streaming(&mut st, &self.shared);
            }
            self.shared.run_start.store(NOT_RUNNING, Ordering::Relaxed);
            st.filter_state = State_Paused;
            Ok(())
        })
    }

    fn Run(&self, tstart: i64) -> Result<()> {
        guard(|| {
            let was_stopped = self.shared.lock().filter_state == State_Stopped;
            if was_stopped {
                IMediaFilter_Impl::Pause(self)?;
            }
            self.shared.run_start.store(tstart, Ordering::Relaxed);
            self.shared.lock().filter_state = State_Running;
            Ok(())
        })
    }

    fn GetState(&self, _timeout: u32) -> Result<FILTER_STATE> {
        guard(|| Ok(self.shared.lock().filter_state))
    }

    fn SetSyncSource(&self, clock: Ref<IReferenceClock>) -> Result<()> {
        guard(|| {
            self.shared.lock().clock = clock.cloned();
            Ok(())
        })
    }

    fn GetSyncSource(&self) -> Result<IReferenceClock> {
        guard(|| self.shared.lock().clock.clone().ok_or_else(|| S_FALSE.into()))
    }
}

impl IBaseFilter_Impl for Filter_Impl {
    fn EnumPins(&self) -> Result<IEnumPins> {
        guard(|| Ok(PinEnum { pin: self.pin.clone(), pos: Mutex::new(0) }.into()))
    }

    fn FindPin(&self, id: &PCWSTR) -> Result<IPin> {
        guard(|| {
            if id.is_null() {
                return Err(E_POINTER.into());
            }
            if unsafe { id.to_string() }.ok().as_deref() == Some(PIN_NAME) {
                Ok(self.pin.clone())
            } else {
                Err(VFW_E_NOT_FOUND.into())
            }
        })
    }

    fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> Result<()> {
        guard(|| {
            let info = unsafe { info.as_mut() }.ok_or(E_POINTER)?;
            let st = self.shared.lock();
            info.achName = [0; 128];
            let n = st.name.len().min(127);
            info.achName[..n].copy_from_slice(&st.name[..n]);
            // AddRef'd for the caller, per the interface contract.
            let graph = unsafe { IFilterGraph::from_raw_borrowed(&st.graph) }.cloned();
            info.pGraph = std::mem::ManuallyDrop::new(graph);
            Ok(())
        })
    }

    fn JoinFilterGraph(&self, graph: Ref<IFilterGraph>, name: &PCWSTR) -> Result<()> {
        guard(|| {
            let mut st = self.shared.lock();
            st.graph = graph.as_ref().map_or(std::ptr::null_mut(), |g| g.as_raw());
            st.name = if name.is_null() { vec![] } else { unsafe { name.as_wide() }.to_vec() };
            Ok(())
        })
    }

    fn QueryVendorInfo(&self) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
}

// ---- pin ----

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
struct Pin {
    shared: Arc<Shared>,
    filter: Mutex<Weak<IBaseFilter>>,
}

impl Pin_Impl {
    fn try_connect(&self, receiver: &IPin, pixel: Pixel) -> Result<()> {
        let mt = media::media_type(pixel).ok_or(E_UNEXPECTED)?;
        let me: IPin = self.to_interface();
        let r = unsafe { receiver.ReceiveConnection(&me, &mt) };
        media::free_format(&mt);
        r?;
        match decide_allocator(receiver, pixel) {
            Ok((input, alloc)) => {
                let mut st = self.shared.lock();
                st.peer = Some(receiver.clone());
                st.input = Some(input);
                st.alloc = Some(alloc);
                st.pixel = pixel;
                Ok(())
            }
            Err(e) => {
                unsafe {
                    let _ = receiver.Disconnect();
                }
                Err(e)
            }
        }
    }
}

fn decide_allocator(receiver: &IPin, pixel: Pixel) -> Result<(IMemInputPin, IMemAllocator)> {
    let input: IMemInputPin = receiver.cast()?;
    let req = unsafe { input.GetAllocatorRequirements() }.unwrap_or_default();
    let alloc = match unsafe { input.GetAllocator() } {
        Ok(a) => a,
        Err(_) => unsafe { CoCreateInstance(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)? },
    };
    let want = ALLOCATOR_PROPERTIES {
        cBuffers: req.cBuffers.max(2),
        cbBuffer: (pixel.image_size() as i32).max(req.cbBuffer),
        cbAlign: req.cbAlign.max(1),
        cbPrefix: req.cbPrefix,
    };
    let got = unsafe { alloc.SetProperties(&want)? };
    if (got.cbBuffer as usize) < pixel.image_size() {
        return Err(E_UNEXPECTED.into());
    }
    unsafe { input.NotifyAllocator(&alloc, false)? };
    Ok((input, alloc))
}

impl IPin_Impl for Pin_Impl {
    fn Connect(&self, receiver: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        guard(|| {
            let receiver = receiver.ok()?;
            {
                let st = self.shared.lock();
                if st.peer.is_some() {
                    return Err(VFW_E_ALREADY_CONNECTED.into());
                }
                if st.filter_state != State_Stopped {
                    return Err(VFW_E_NOT_STOPPED.into());
                }
            }
            let asked = unsafe { media::match_type(pmt) };
            if !pmt.is_null() && asked.is_none() {
                return Err(VFW_E_NO_ACCEPTABLE_TYPES.into());
            }
            let preferred = self.shared.lock().pixel;
            let order = asked.map_or([preferred, other(preferred)], |p| [p, p]);
            for p in order {
                if self.try_connect(receiver, p).is_ok() {
                    return Ok(());
                }
            }
            Err(VFW_E_NO_ACCEPTABLE_TYPES.into())
        })
    }

    fn ReceiveConnection(&self, _connector: Ref<IPin>, _pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        Err(E_UNEXPECTED.into()) // output pins don't receive connections
    }

    fn Disconnect(&self) -> Result<()> {
        guard(|| {
            let mut st = self.shared.lock();
            if st.filter_state != State_Stopped {
                return Err(VFW_E_NOT_STOPPED.into());
            }
            if let Some(a) = st.alloc.take() {
                unsafe {
                    let _ = a.Decommit();
                }
            }
            st.peer = None;
            st.input = None;
            Ok(())
        })
    }

    fn ConnectedTo(&self) -> Result<IPin> {
        guard(|| self.shared.lock().peer.clone().ok_or_else(|| VFW_E_NOT_CONNECTED.into()))
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> Result<()> {
        guard(|| {
            let out = unsafe { pmt.as_mut() }.ok_or(E_POINTER)?;
            let st = self.shared.lock();
            if st.peer.is_none() {
                return Err(VFW_E_NOT_CONNECTED.into());
            }
            *out = media::media_type(st.pixel).ok_or(E_UNEXPECTED)?;
            Ok(())
        })
    }

    fn QueryPinInfo(&self, info: *mut PIN_INFO) -> Result<()> {
        guard(|| {
            let info = unsafe { info.as_mut() }.ok_or(E_POINTER)?;
            let filter = self.filter.lock().unwrap_or_else(|e| e.into_inner()).upgrade();
            info.pFilter = std::mem::ManuallyDrop::new(filter);
            info.dir = PINDIR_OUTPUT;
            info.achName = [0; 128];
            for (d, s) in info.achName.iter_mut().zip(PIN_NAME.encode_utf16()) {
                *d = s;
            }
            Ok(())
        })
    }

    fn QueryDirection(&self) -> Result<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> Result<PWSTR> {
        guard(|| {
            let w: Vec<u16> = PIN_NAME.encode_utf16().chain([0]).collect();
            let p = unsafe { CoTaskMemAlloc(w.len() * 2) } as *mut u16;
            if p.is_null() {
                return Err(E_UNEXPECTED.into());
            }
            unsafe { std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len()) };
            Ok(PWSTR(p))
        })
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        guard_hr(|| if unsafe { media::match_type(pmt) }.is_some() { S_OK } else { S_FALSE })
    }

    fn EnumMediaTypes(&self) -> Result<IEnumMediaTypes> {
        guard(|| Ok(TypeEnum { first: self.shared.lock().pixel, pos: Mutex::new(0) }.into()))
    }

    fn QueryInternalConnections(&self, _pins: windows_core::OutRef<IPin>, _n: *mut u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndOfStream(&self) -> Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn BeginFlush(&self) -> Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn EndFlush(&self) -> Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> Result<()> {
        Ok(())
    }
}

fn other(p: Pixel) -> Pixel {
    if p == Pixel::Nv12 {
        Pixel::Yuy2
    } else {
        Pixel::Nv12
    }
}

impl IAMStreamConfig_Impl for Pin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        guard(|| {
            let p = unsafe { media::match_type(pmt) }.ok_or(VFW_E_INVALIDMEDIATYPE)?;
            let mut st = self.shared.lock();
            if st.peer.is_some() && st.pixel != p {
                return Err(VFW_E_NOT_STOPPED.into()); // reconnect with the new type instead
            }
            st.pixel = p;
            Ok(())
        })
    }

    fn GetFormat(&self) -> Result<*mut AM_MEDIA_TYPE> {
        guard(|| {
            let mt = media::alloc_media_type(self.shared.lock().pixel);
            if mt.is_null() {
                Err(E_UNEXPECTED.into())
            } else {
                Ok(mt)
            }
        })
    }

    fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> Result<()> {
        guard(|| {
            let (c, s) = unsafe { (count.as_mut(), size.as_mut()) };
            *c.ok_or(E_POINTER)? = FORMATS.len() as i32;
            *s.ok_or(E_POINTER)? = std::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
            Ok(())
        })
    }

    fn GetStreamCaps(&self, index: i32, ppmt: *mut *mut AM_MEDIA_TYPE, pscc: *mut u8) -> Result<()> {
        guard(|| {
            let p = *FORMATS.get(index as usize).ok_or(S_FALSE)?;
            if ppmt.is_null() || pscc.is_null() {
                return Err(E_POINTER.into());
            }
            let mt = media::alloc_media_type(p);
            if mt.is_null() {
                return Err(E_UNEXPECTED.into());
            }
            let size = windows::Win32::Foundation::SIZE { cx: WIDTH, cy: HEIGHT };
            let bits = (p.image_size() * 8 * 30) as i32;
            let caps = VIDEO_STREAM_CONFIG_CAPS {
                guid: FORMAT_VideoInfo,
                InputSize: size,
                MinCroppingSize: size,
                MaxCroppingSize: size,
                CropGranularityX: 1,
                CropGranularityY: 1,
                CropAlignX: 1,
                CropAlignY: 1,
                MinOutputSize: size,
                MaxOutputSize: size,
                OutputGranularityX: 1,
                OutputGranularityY: 1,
                MinFrameInterval: FRAME_TIME,
                MaxFrameInterval: FRAME_TIME,
                MinBitsPerSecond: bits,
                MaxBitsPerSecond: bits,
                ..Default::default()
            };
            unsafe {
                *ppmt = mt;
                std::ptr::write_unaligned(pscc as *mut VIDEO_STREAM_CONFIG_CAPS, caps);
            }
            Ok(())
        })
    }
}

/// Apps find capture pins by asking for the pin category.
impl IKsPropertySet_Impl for Pin_Impl {
    fn Set(
        &self,
        _: *const GUID,
        _: u32,
        _: *const core::ffi::c_void,
        _: u32,
        _: *const core::ffi::c_void,
        _: u32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Get(
        &self,
        set: *const GUID,
        id: u32,
        _instance: *const core::ffi::c_void,
        _instance_len: u32,
        data: *mut core::ffi::c_void,
        data_len: u32,
        returned: *mut u32,
    ) -> Result<()> {
        guard(|| {
            if unsafe { set.as_ref() } != Some(&AMPROPSETID_Pin) {
                return Err(E_PROP_SET_UNSUPPORTED.into());
            }
            if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
                return Err(E_PROP_ID_UNSUPPORTED.into());
            }
            if data.is_null() || (data_len as usize) < std::mem::size_of::<GUID>() {
                return Err(E_POINTER.into());
            }
            unsafe {
                std::ptr::write_unaligned(data as *mut GUID, PIN_CATEGORY_CAPTURE);
                if let Some(r) = returned.as_mut() {
                    *r = std::mem::size_of::<GUID>() as u32;
                }
            }
            Ok(())
        })
    }

    fn QuerySupported(&self, set: *const GUID, id: u32) -> Result<u32> {
        guard(|| {
            if unsafe { set.as_ref() } != Some(&AMPROPSETID_Pin) {
                return Err(E_PROP_SET_UNSUPPORTED.into());
            }
            if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
                return Err(E_PROP_ID_UNSUPPORTED.into());
            }
            Ok(KSPROPERTY_SUPPORT_GET)
        })
    }
}

// ---- enumerators ----

#[implement(IEnumPins)]
struct PinEnum {
    pin: IPin,
    pos: Mutex<u32>,
}

impl IEnumPins_Impl for PinEnum_Impl {
    fn Next(&self, count: u32, pins: *mut Option<IPin>, fetched: *mut u32) -> HRESULT {
        guard_hr(|| {
            if pins.is_null() || (count != 1 && fetched.is_null()) {
                return E_POINTER;
            }
            let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
            let n = if *pos == 0 && count > 0 {
                unsafe { pins.write(Some(self.pin.clone())) };
                *pos = 1;
                1
            } else {
                0
            };
            if let Some(f) = unsafe { fetched.as_mut() } {
                *f = n;
            }
            if n == count {
                S_OK
            } else {
                S_FALSE
            }
        })
    }

    fn Skip(&self, count: u32) -> Result<()> {
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        *pos = pos.saturating_add(count);
        if *pos <= 1 {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> Result<()> {
        *self.pos.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumPins> {
        let pos = *self.pos.lock().unwrap_or_else(|e| e.into_inner());
        Ok(PinEnum { pin: self.pin.clone(), pos: Mutex::new(pos) }.into())
    }
}

#[implement(IEnumMediaTypes)]
struct TypeEnum {
    first: Pixel,
    pos: Mutex<usize>,
}

impl IEnumMediaTypes_Impl for TypeEnum_Impl {
    fn Next(&self, count: u32, types: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        guard_hr(|| {
            if types.is_null() || (count != 1 && fetched.is_null()) {
                return E_POINTER;
            }
            let order = [self.first, other(self.first)];
            let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
            let mut n = 0;
            while n < count as usize && *pos < order.len() {
                let mt = media::alloc_media_type(order[*pos]);
                if mt.is_null() {
                    break;
                }
                unsafe { types.add(n).write(mt) };
                n += 1;
                *pos += 1;
            }
            if let Some(f) = unsafe { fetched.as_mut() } {
                *f = n as u32;
            }
            if n == count as usize {
                S_OK
            } else {
                S_FALSE
            }
        })
    }

    fn Skip(&self, count: u32) -> Result<()> {
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        *pos += count as usize;
        if *pos <= FORMATS.len() {
            Ok(())
        } else {
            Err(S_FALSE.into())
        }
    }

    fn Reset(&self) -> Result<()> {
        *self.pos.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumMediaTypes> {
        let pos = *self.pos.lock().unwrap_or_else(|e| e.into_inner());
        Ok(TypeEnum { first: self.first, pos: Mutex::new(pos) }.into())
    }
}

// ---- streaming ----

fn start_streaming(st: &mut State, shared: &Shared) {
    let (Some(input), Some(alloc)) = (st.input.clone(), st.alloc.clone()) else { return };
    let stop = Arc::new(AtomicBool::new(false));
    st.stop = stop.clone();
    let (faulted, run_start) = (shared.faulted.clone(), shared.run_start.clone());
    let job = Sendable((input, alloc, st.pixel, st.clock.clone()));
    st.worker = std::thread::Builder::new()
        .name("lenny-dshow".into())
        .spawn(move || {
            let (input, alloc, pixel, clock) = job.get();
            let timing = Timing { clock, run_start };
            // Last line of defence: a panic here must not take the host app down.
            let _ = catch_unwind(AssertUnwindSafe(|| stream(&input, &alloc, pixel, &timing, &stop, &faulted)));
        })
        .ok();
}

fn stop_streaming(st: &mut State) {
    st.stop.store(true, Ordering::Relaxed);
    if let Some(a) = &st.alloc {
        unsafe {
            let _ = a.Decommit(); // unblocks a GetBuffer wait
        }
    }
    if let Some(t) = st.worker.take() {
        let _ = t.join();
    }
}

/// Sample times: capture time on the graph clock, relative to Run's start. Unstamped (shown on arrival) while
/// paused or without a clock, so a slow Pause -> Run never delays live video.
struct Timing {
    clock: Option<IReferenceClock>,
    run_start: Arc<AtomicI64>,
}

impl Timing {
    fn now(&self) -> Option<i64> {
        let start = self.run_start.load(Ordering::Relaxed);
        let now = unsafe { self.clock.as_ref()?.GetTime() }.ok()?;
        (start != NOT_RUNNING).then(|| (now - start).max(0))
    }
}

/// One sample every 1/30 s until stopped. Everything is allocated before the loop.
fn stream(
    input: &IMemInputPin,
    alloc: &IMemAllocator,
    pixel: Pixel,
    timing: &Timing,
    stop: &AtomicBool,
    faulted: &AtomicBool,
) {
    let mut frames = Frames::new();
    let start = Instant::now();
    let mut n: i64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let due = start + Duration::from_nanos(n as u64 * 100 * FRAME_TIME as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > Duration::from_millis(200) {
            n = ((now - start).as_nanos() / (100 * FRAME_TIME as u128)) as i64; // fell behind: skip ahead
        }
        let mut sample: Option<IMediaSample> = None;
        if unsafe { alloc.GetBuffer(&mut sample, None, None, 0) }.is_err() {
            break; // decommitted: stopping
        }
        let Some(sample) = sample else { break };

        let picture = frames.next(faulted);

        let delivered = unsafe {
            let (Ok(ptr), size) = (sample.GetPointer(), sample.GetSize()) else { break };
            if ptr.is_null() || (size as usize) < pixel.image_size() {
                break;
            }
            let out = std::slice::from_raw_parts_mut(ptr, pixel.image_size());
            match pixel {
                Pixel::Nv12 => out.copy_from_slice(picture),
                Pixel::Yuy2 => {
                    fb::nv12_to_yuy2(picture, WIDTH as usize, HEIGHT as usize, out);
                }
            }
            match timing.now() {
                Some(t0) => {
                    let t1 = t0 + FRAME_TIME;
                    let _ = sample.SetTime(Some(&t0), Some(&t1));
                }
                None => {
                    let _ = sample.SetTime(None, None);
                }
            }
            let _ = sample.SetSyncPoint(true);
            let _ = sample.SetActualDataLength(pixel.image_size() as i32);
            input.Receive(&sample)
        };
        drop(sample); // back to the allocator
        if delivered.is_err() {
            break; // downstream stopped or was disconnected
        }
        n += 1;
    }
}

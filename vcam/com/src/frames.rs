//! The picture both cameras send: the app's latest frame from the shared-memory ring, the last good one for up to
//! 500 ms when the app stalls (architecture.md §7.2), else the placeholder. Always NV12 at media::WIDTH x HEIGHT.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lenny_framebuf as fb;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::media::{HEIGHT, WIDTH};

const HOLD_LAST: Duration = Duration::from_millis(500);

/// Read side of the shared frame buffer: opened lazily, reopened when the app (re)starts.
struct Source {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    reader: Option<fb::Reader>,
    next_try: Instant,
}

impl Source {
    fn open(&mut self) {
        if self.reader.is_some() || Instant::now() < self.next_try {
            return;
        }
        self.next_try = Instant::now() + Duration::from_secs(1);
        for name in [fb::MAPPING_NAME, fb::LOCAL_MAPPING_NAME] {
            let name = windows::core::HSTRING::from(name);
            let Ok(m) = (unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) }) else { continue };
            let view = unsafe { MapViewOfFile(m, FILE_MAP_READ, 0, 0, fb::mapping_bytes()) };
            match unsafe { fb::View::new(view.Value as *mut u8, fb::mapping_bytes()) } {
                Some(v) => {
                    self.mapping = m;
                    self.view = view;
                    self.reader = Some(fb::Reader::new(v));
                    return;
                }
                None => unsafe {
                    if !view.Value.is_null() {
                        let _ = UnmapViewOfFile(view);
                    }
                    let _ = CloseHandle(m);
                },
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            if !self.view.Value.is_null() {
                let _ = UnmapViewOfFile(self.view);
            }
            if !self.mapping.is_invalid() {
                let _ = CloseHandle(self.mapping);
            }
        }
    }
}

fn placeholder_nv12(text: &str) -> Vec<u8> {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let i420 = lenny_vcam::frame::placeholder_i420(w, h, text);
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    nv12[..w * h].copy_from_slice(&i420[..w * h]);
    let (u, v) = i420[w * h..].split_at(w * h / 4);
    for (i, pair) in nv12[w * h..].chunks_exact_mut(2).enumerate() {
        pair[0] = u[i];
        pair[1] = v[i];
    }
    nv12
}

/// Everything allocated up front: `next` doesn't allocate.
pub struct Frames {
    src: Source,
    live: Vec<u8>,
    waiting: Vec<u8>,
    last_good: Option<Instant>,
}

impl Frames {
    pub fn new() -> Self {
        Frames {
            src: Source {
                mapping: HANDLE::default(),
                view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
                reader: None,
                next_try: Instant::now(),
            },
            live: vec![0u8; fb::nv12_bytes(WIDTH as u32, HEIGHT as u32)],
            waiting: placeholder_nv12("lenny - waiting for phone"),
            last_good: None,
        }
    }

    /// The picture to send now. A panic while reading sets `faulted`: placeholder only from then on.
    pub fn next(&mut self, faulted: &AtomicBool) -> &[u8] {
        let (src, live, last_good) = (&mut self.src, &mut self.live, &mut self.last_good);
        let ok = catch_unwind(AssertUnwindSafe(|| {
            if faulted.load(Ordering::Relaxed) {
                return false;
            }
            src.open();
            let Some(r) = &src.reader else { return false };
            let fresh = r.heartbeat_age(unsafe { GetTickCount64() }) < fb::STALE_MS;
            let fits = r.format().is_ok_and(|f| f.width == WIDTH as u32 && f.height == HEIGHT as u32);
            if fresh && fits && r.read_nv12(live).is_ok() {
                *last_good = Some(Instant::now());
            }
            last_good.is_some_and(|t| t.elapsed() < HOLD_LAST)
        }));
        match ok {
            Ok(true) => &self.live,
            Ok(false) => &self.waiting,
            Err(_) => {
                faulted.store(true, Ordering::Relaxed);
                &self.waiting
            }
        }
    }
}

//! Shared-memory frame ring (architecture.md §7.3). The desktop app is the only writer; the virtual camera
//! backends (DirectShow filter DLL, MF media source DLL) read it from other processes. OS mapping is not here:
//! this crate only knows the byte layout, so it's testable anywhere and the readers depend on nothing else.
//!
//! Layout (little-endian, fixed):
//! ```text
//! Header, 4096 bytes:
//!   0  magic 'LNYF'      4  version (1)      8  slot_count      12 width     16 height
//!   20 format ('NV12')   24 stride_y         28 stride_uv       32 write_index u64 (frames completed, monotonic)
//!   40 producer_pid      44 reserved         48 heartbeat_ms u64 (writer's clock, GetTickCount64 on Windows)
//!   56 state (NO_SOURCE / LIVE / RECONNECTING / ERROR)           60 orientation (always 0: frames arrive upright)
//!   64 slot_bytes u64 (pixel bytes per slot)
//! Slot i at 4096 + i * (16 + slot_bytes rounded up to 64):
//!   seq u64 (seqlock: odd while writing), pts_100ns i64, then NV12 pixels (Y plane, then interleaved UV)
//! ```
//! Readers take the newest completed slot and check `seq` didn't change while copying. Every offset a reader
//! uses is checked against the mapping length first: a corrupt header must never cause an out-of-bounds read.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = u32::from_le_bytes(*b"LNYF");
pub const VERSION: u32 = 1;
pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const HEADER_BYTES: usize = 4096;
pub const SLOT_COUNT: u32 = 3;
/// Largest frame a mapping is sized for (architecture.md §7.3).
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;
/// Windows object names. `Global\` so the MF media source in Frame Server (session 0) sees them too.
pub const MAPPING_NAME: &str = "Global\\LennyFrames_v1";
pub const EVENT_NAME: &str = "Global\\LennyFrameReady_v1";
/// Fallback when the app can't create `Global\` objects (no SeCreateGlobalPrivilege and no broker service):
/// DirectShow consumers in the same session still work, Frame Server doesn't.
pub const LOCAL_MAPPING_NAME: &str = "Local\\LennyFrames_v1";
pub const LOCAL_EVENT_NAME: &str = "Local\\LennyFrameReady_v1";
/// COM class ids of the two camera DLLs. The installer (or regsvr32) registers them; the app checks for them.
pub const DSHOW_FILTER_CLSID: &str = "{BEEEF45F-D1F1-4A35-8F7D-17E756BC2046}";
pub const MF_SOURCE_CLSID: &str = "{5F0F9024-D043-45C1-B2F6-03DAB6CE130A}";
/// A reader shows its placeholder when the heartbeat is older than this.
pub const STALE_MS: u64 = 1000;

pub const STATE_NO_SOURCE: u32 = 0;
pub const STATE_LIVE: u32 = 1;
pub const STATE_RECONNECTING: u32 = 2;
pub const STATE_ERROR: u32 = 3;

const O_MAGIC: usize = 0;
const O_VERSION: usize = 4;
const O_SLOTS: usize = 8;
const O_WIDTH: usize = 12;
const O_HEIGHT: usize = 16;
const O_FORMAT: usize = 20;
const O_STRIDE_Y: usize = 24;
const O_STRIDE_UV: usize = 28;
const O_WRITE_INDEX: usize = 32;
const O_PID: usize = 40;
const O_HEARTBEAT: usize = 48;
const O_STATE: usize = 56;
const O_ORIENTATION: usize = 60;
const O_SLOT_BYTES: usize = 64;
const SLOT_HEADER: usize = 16;

pub const fn nv12_bytes(w: u32, h: u32) -> usize {
    (w as usize) * (h as usize) * 3 / 2
}

const fn slot_stride(slot_bytes: usize) -> usize {
    (SLOT_HEADER + slot_bytes).div_ceil(64) * 64
}

/// Mapping size for frames up to `MAX_WIDTH` x `MAX_HEIGHT`: what the writer creates and readers expect.
pub const fn mapping_bytes() -> usize {
    HEADER_BYTES + SLOT_COUNT as usize * slot_stride(nv12_bytes(MAX_WIDTH, MAX_HEIGHT))
}

/// A mapped view: base pointer (at least 8-byte aligned; OS mappings are page-aligned) and its length.
#[derive(Clone, Copy)]
pub struct View {
    base: *mut u8,
    len: usize,
}

unsafe impl Send for View {}
unsafe impl Sync for View {}

impl View {
    /// # Safety
    /// `base` must point to `len` bytes that stay mapped (readable, and writable for a `Writer`) while the view
    /// is used, aligned to 8.
    pub unsafe fn new(base: *mut u8, len: usize) -> Option<View> {
        (!base.is_null() && base as usize % 8 == 0 && len >= HEADER_BYTES).then_some(View { base, len })
    }

    fn u32(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off + 4 <= HEADER_BYTES);
        // SAFETY: in the header (checked at construction), 4-aligned offsets on an 8-aligned base.
        unsafe { &*(self.base.add(off) as *const AtomicU32) }
    }

    fn u64(&self, off: usize) -> Option<&AtomicU64> {
        (off % 8 == 0 && off + 8 <= self.len).then(|| unsafe { &*(self.base.add(off) as *const AtomicU64) })
    }
}

/// Header fields a reader needs, validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub slot_bytes: usize,
}

// ---- writer (the desktop app) ----

pub struct Writer {
    view: View,
    fmt: Format,
}

impl Writer {
    /// Initializes the header for `width` x `height` NV12 frames. None if the mapping is too small.
    pub fn new(view: View, width: u32, height: u32, pid: u32) -> Option<Writer> {
        let slot_bytes = nv12_bytes(width, height);
        if width == 0 || height == 0 || width % 2 == 1 || height % 2 == 1 {
            return None;
        }
        if HEADER_BYTES + SLOT_COUNT as usize * slot_stride(slot_bytes) > view.len {
            return None;
        }
        let v = &view;
        v.u32(O_MAGIC).store(0, Ordering::Relaxed); // readers ignore it until it's complete
        v.u32(O_VERSION).store(VERSION, Ordering::Relaxed);
        v.u32(O_SLOTS).store(SLOT_COUNT, Ordering::Relaxed);
        v.u32(O_WIDTH).store(width, Ordering::Relaxed);
        v.u32(O_HEIGHT).store(height, Ordering::Relaxed);
        v.u32(O_FORMAT).store(FOURCC_NV12, Ordering::Relaxed);
        v.u32(O_STRIDE_Y).store(width, Ordering::Relaxed);
        v.u32(O_STRIDE_UV).store(width, Ordering::Relaxed);
        v.u32(O_PID).store(pid, Ordering::Relaxed);
        v.u32(O_STATE).store(STATE_NO_SOURCE, Ordering::Relaxed);
        v.u32(O_ORIENTATION).store(0, Ordering::Relaxed);
        v.u64(O_SLOT_BYTES)?.store(slot_bytes as u64, Ordering::Relaxed);
        v.u64(O_WRITE_INDEX)?.store(0, Ordering::Relaxed);
        for i in 0..SLOT_COUNT as usize {
            v.u64(HEADER_BYTES + i * slot_stride(slot_bytes))?.store(0, Ordering::Relaxed);
        }
        v.u32(O_MAGIC).store(MAGIC, Ordering::Release);
        Some(Writer { view, fmt: Format { width, height, slot_bytes } })
    }

    pub fn format(&self) -> Format {
        self.fmt
    }

    /// Publishes one I420 frame (converted to NV12 on the way in). Never blocks.
    pub fn write_i420(&mut self, i420: &[u8], pts_100ns: i64, now_ms: u64) -> bool {
        let (w, h) = (self.fmt.width as usize, self.fmt.height as usize);
        if i420.len() != w * h * 3 / 2 {
            return false;
        }
        let (y, uv) = i420.split_at(w * h);
        let (u, v) = uv.split_at(w * h / 4);
        self.write_with(pts_100ns, now_ms, |dst| {
            dst[..w * h].copy_from_slice(y);
            for (i, pair) in dst[w * h..].chunks_exact_mut(2).enumerate() {
                pair[0] = u[i];
                pair[1] = v[i];
            }
        })
    }

    /// Publishes one NV12 frame.
    pub fn write_nv12(&mut self, nv12: &[u8], pts_100ns: i64, now_ms: u64) -> bool {
        if nv12.len() != self.fmt.slot_bytes {
            return false;
        }
        self.write_with(pts_100ns, now_ms, |dst| dst.copy_from_slice(nv12))
    }

    fn write_with(&mut self, pts_100ns: i64, now_ms: u64, fill: impl FnOnce(&mut [u8])) -> bool {
        let v = self.view;
        let Some(wi) = v.u64(O_WRITE_INDEX) else { return false };
        let next = wi.load(Ordering::Relaxed) + 1;
        let off = HEADER_BYTES + (next % SLOT_COUNT as u64) as usize * slot_stride(self.fmt.slot_bytes);
        let (Some(seq), Some(pts)) = (v.u64(off), v.u64(off + 8)) else { return false };
        seq.fetch_add(1, Ordering::Relaxed); // odd: writing
        fence(Ordering::Release);
        pts.store(pts_100ns as u64, Ordering::Relaxed);
        // SAFETY: slot bounds were checked in new().
        let dst = unsafe { std::slice::from_raw_parts_mut(v.base.add(off + SLOT_HEADER), self.fmt.slot_bytes) };
        fill(dst);
        seq.fetch_add(1, Ordering::Release); // even: complete
        wi.store(next, Ordering::Release);
        self.heartbeat(now_ms);
        true
    }

    /// Call at least every few hundred ms even without frames, so readers know the app is alive.
    pub fn heartbeat(&self, now_ms: u64) {
        if let Some(h) = self.view.u64(O_HEARTBEAT) {
            h.store(now_ms, Ordering::Release);
        }
    }

    pub fn set_state(&self, state: u32) {
        self.view.u32(O_STATE).store(state, Ordering::Release);
    }
}

// ---- reader (virtual camera DLLs) ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Header missing, wrong version, or fields that don't fit the mapping.
    Invalid,
    /// No frame written yet.
    Empty,
    /// The writer kept overwriting the slot while we copied (retry next tick).
    Torn,
    /// Output buffer has the wrong size for this format.
    BadOutput,
}

pub struct Reader {
    view: View,
}

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub index: u64,
    pub pts_100ns: i64,
}

impl Reader {
    pub fn new(view: View) -> Reader {
        Reader { view }
    }

    /// The format, if the header is complete and consistent with the mapping size.
    pub fn format(&self) -> Result<Format, ReadError> {
        let v = &self.view;
        if v.u32(O_MAGIC).load(Ordering::Acquire) != MAGIC
            || v.u32(O_VERSION).load(Ordering::Relaxed) != VERSION
            || v.u32(O_SLOTS).load(Ordering::Relaxed) != SLOT_COUNT
            || v.u32(O_FORMAT).load(Ordering::Relaxed) != FOURCC_NV12
        {
            return Err(ReadError::Invalid);
        }
        let (w, h) = (v.u32(O_WIDTH).load(Ordering::Relaxed), v.u32(O_HEIGHT).load(Ordering::Relaxed));
        let slot_bytes = v.u64(O_SLOT_BYTES).ok_or(ReadError::Invalid)?.load(Ordering::Relaxed) as usize;
        let fits = w > 0
            && h > 0
            && w <= MAX_WIDTH
            && h <= MAX_HEIGHT
            && v.u32(O_STRIDE_Y).load(Ordering::Relaxed) == w
            && v.u32(O_STRIDE_UV).load(Ordering::Relaxed) == w
            && slot_bytes == nv12_bytes(w, h)
            && HEADER_BYTES + SLOT_COUNT as usize * slot_stride(slot_bytes) <= v.len;
        if fits {
            Ok(Format { width: w, height: h, slot_bytes })
        } else {
            Err(ReadError::Invalid)
        }
    }

    pub fn state(&self) -> u32 {
        self.view.u32(O_STATE).load(Ordering::Acquire)
    }

    /// Milliseconds since the writer's last heartbeat, given the reader's clock (same clock as the writer's).
    pub fn heartbeat_age(&self, now_ms: u64) -> u64 {
        self.view.u64(O_HEARTBEAT).map_or(u64::MAX, |h| now_ms.saturating_sub(h.load(Ordering::Acquire)))
    }

    /// Copies the newest complete frame (NV12) into `out`, which must be exactly `format().slot_bytes` long.
    pub fn read_nv12(&self, out: &mut [u8]) -> Result<Frame, ReadError> {
        let fmt = self.format()?;
        if out.len() != fmt.slot_bytes {
            return Err(ReadError::BadOutput);
        }
        let v = &self.view;
        let index = v.u64(O_WRITE_INDEX).ok_or(ReadError::Invalid)?.load(Ordering::Acquire);
        if index == 0 {
            return Err(ReadError::Empty);
        }
        let off = HEADER_BYTES + (index % SLOT_COUNT as u64) as usize * slot_stride(fmt.slot_bytes);
        let (seq, pts) = (v.u64(off).ok_or(ReadError::Invalid)?, v.u64(off + 8).ok_or(ReadError::Invalid)?);
        let s1 = seq.load(Ordering::Acquire);
        if s1 % 2 == 1 {
            return Err(ReadError::Torn);
        }
        let pts = pts.load(Ordering::Relaxed) as i64;
        // ponytail: plain copy of memory another process may be writing; the seqlock check below discards any torn
        // copy. Byte-exact atomics would cost ~3 MB of atomic loads per frame for no practical gain.
        // SAFETY: bounds checked by format() (slot fits the mapping).
        unsafe { std::ptr::copy_nonoverlapping(v.base.add(off + SLOT_HEADER), out.as_mut_ptr(), fmt.slot_bytes) };
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) != s1 {
            return Err(ReadError::Torn);
        }
        Ok(Frame { index, pts_100ns: pts })
    }
}

// ---- pixel formats consumers ask for ----

/// NV12 -> YUY2 (packed 4:2:2, what many DirectShow consumers prefer). `out` is w*h*2 bytes.
pub fn nv12_to_yuy2(nv12: &[u8], w: usize, h: usize, out: &mut [u8]) -> bool {
    if nv12.len() != w * h * 3 / 2 || out.len() != w * h * 2 || w % 2 == 1 || h % 2 == 1 {
        return false;
    }
    let (y, uv) = nv12.split_at(w * h);
    for row in 0..h {
        let yr = &y[row * w..][..w];
        let uvr = &uv[(row / 2) * w..][..w];
        let o = &mut out[row * w * 2..][..w * 2];
        for x in (0..w).step_by(2) {
            o[x * 2] = yr[x];
            o[x * 2 + 1] = uvr[x];
            o[x * 2 + 2] = yr[x + 1];
            o[x * 2 + 3] = uvr[x + 1];
        }
    }
    true
}

/// NV12 frame of one flat colour (limited range), for readers' placeholder backgrounds.
pub fn fill_nv12(out: &mut [u8], w: usize, h: usize, y: u8, u: u8, v: u8) {
    let (yp, uvp) = out.split_at_mut((w * h).min(out.len()));
    yp.fill(y);
    for pair in uvp.chunks_exact_mut(2) {
        pair[0] = u;
        pair[1] = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping() -> (Vec<u64>, View) {
        let mut mem = vec![0u64; mapping_bytes() / 8 + 1];
        let view = unsafe { View::new(mem.as_mut_ptr() as *mut u8, mem.len() * 8) }.unwrap();
        (mem, view)
    }

    #[test]
    fn write_then_read() {
        let (_mem, view) = mapping();
        let mut w = Writer::new(view, 4, 2, 7).unwrap();
        let r = Reader::new(view);
        let mut out = vec![0u8; nv12_bytes(4, 2)];
        assert_eq!(r.read_nv12(&mut out).unwrap_err(), ReadError::Empty);
        // I420 4x2: Y 0..8, U [100,101], V [200,201] -> NV12 UV interleaved.
        let i420: Vec<u8> = (0..8).chain([100, 101, 200, 201]).collect();
        assert!(w.write_i420(&i420, 42, 1000));
        let f = r.read_nv12(&mut out).unwrap();
        assert_eq!((f.index, f.pts_100ns), (1, 42));
        assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 100, 200, 101, 201]);
        assert_eq!(r.heartbeat_age(1500), 500);
        assert_eq!(r.format().unwrap(), Format { width: 4, height: 2, slot_bytes: 12 });
    }

    #[test]
    fn corrupt_headers_are_rejected_not_read() {
        let (mut mem, view) = mapping();
        Writer::new(view, 1280, 720, 1).unwrap();
        let r = Reader::new(view);
        assert!(r.format().is_ok());
        let hdr = unsafe { std::slice::from_raw_parts_mut(mem.as_mut_ptr() as *mut u8, HEADER_BYTES) };
        hdr[O_WIDTH..O_WIDTH + 4].copy_from_slice(&60000u32.to_le_bytes()); // would read far past the mapping
        assert_eq!(r.format().unwrap_err(), ReadError::Invalid);
        let small = unsafe { View::new(mem.as_mut_ptr() as *mut u8, HEADER_BYTES + 100) }.unwrap();
        hdr[O_WIDTH..O_WIDTH + 4].copy_from_slice(&1280u32.to_le_bytes());
        assert_eq!(Reader::new(small).format().unwrap_err(), ReadError::Invalid); // slots don't fit this view
        assert_eq!(Reader::new(small).read_nv12(&mut [0; 10]).unwrap_err(), ReadError::Invalid);
    }

    #[test]
    fn mapping_too_small_or_odd_size_refused() {
        let mut mem = vec![0u64; 1024];
        let view = unsafe { View::new(mem.as_mut_ptr() as *mut u8, mem.len() * 8) }.unwrap();
        assert!(Writer::new(view, 1280, 720, 1).is_none());
        let (_m, big) = mapping();
        assert!(Writer::new(big, 1281, 720, 1).is_none());
        assert!(Writer::new(big, 1920, 1080, 1).is_some());
    }

    /// A reader racing a writer never returns a torn frame: every frame is one constant byte value.
    #[test]
    fn seqlock_never_returns_torn_frames() {
        let (_mem, view) = mapping();
        let (w, h) = (640u32, 360u32);
        let mut writer = Writer::new(view, w, h, 1).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let st = stop.clone();
        let t = std::thread::spawn(move || {
            let mut frame = vec![0u8; nv12_bytes(w, h)];
            let mut n = 0u8;
            while !st.load(Ordering::Relaxed) {
                n = n.wrapping_add(1);
                frame.fill(n);
                writer.write_nv12(&frame, n as i64, 0);
            }
        });
        let r = Reader::new(view);
        let mut out = vec![0u8; nv12_bytes(w, h)];
        let (mut ok, mut torn) = (0, 0);
        while ok < 300 {
            match r.read_nv12(&mut out) {
                Ok(f) => {
                    assert!(out.iter().all(|&b| b == out[0]), "torn frame returned");
                    assert_eq!(out[0] as i64, f.pts_100ns);
                    ok += 1;
                }
                Err(ReadError::Torn) | Err(ReadError::Empty) => torn += 1,
                Err(e) => panic!("{e:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        println!("{ok} good reads, {torn} retries");
    }

    #[test]
    fn yuy2_conversion() {
        let nv12 = [10, 11, 12, 13, 20, 21, 22, 23, 100, 200, 101, 201]; // 4x2
        let mut out = [0u8; 16];
        assert!(nv12_to_yuy2(&nv12, 4, 2, &mut out));
        assert_eq!(out, [10, 100, 11, 200, 12, 101, 13, 201, 20, 100, 21, 200, 22, 101, 23, 201]);
    }
}

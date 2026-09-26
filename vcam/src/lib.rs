//! The virtual camera other apps (Zoom, Chrome, OBS...) see, behind one trait so the desktop app never special-cases
//! the OS. Backends: `V4l2LoopbackCamera` (Linux, real device) and `NullVirtualCamera` (anywhere: writes frame
//! metadata and a rolling sample frame to files). Windows adds DirectShow + Media Foundation backends implementing
//! the same trait (CLAUDE.md, "Desktop build order").
//!
//! Frames are I420 (YUV 4:2:0 planar) at the size given to `open`. The app composes every decoded frame into that
//! fixed canvas (`frame::compose_i420`), so the device format never changes under a consumer that's reading it.

pub mod frame;
mod null;
#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(windows)]
mod windows;

pub use null::NullVirtualCamera;
#[cfg(target_os = "linux")]
pub use v4l2::V4l2LoopbackCamera;
#[cfg(windows)]
pub use windows::WindowsCamera;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl FrameFormat {
    /// Bytes in one I420 frame.
    pub fn frame_size(&self) -> usize {
        frame::i420_size(self.width as usize, self.height as usize)
    }
}

#[allow(clippy::upper_case_acronyms)]
pub trait IVirtualCamera: Send {
    fn open(&mut self, format: FrameFormat) -> Result<()>;
    /// One I420 frame of exactly `format.frame_size()` bytes.
    fn write_frame(&mut self, frame: &[u8]) -> Result<()>;
    fn close(&mut self);
    /// False for the null backend: nothing outside this app can see the camera.
    fn is_real(&self) -> bool;
    /// One line for the UI and logs, e.g. "v4l2loopback /dev/video4" or why it fell back to null.
    fn describe(&self) -> String;
}

/// The best backend this machine can run, opened. Never fails: without a real device it falls back to the null
/// backend and logs why (expected in containers and CI, where kernel modules can't be loaded).
pub fn open_best(format: FrameFormat) -> Box<dyn IVirtualCamera> {
    #[cfg(target_os = "linux")]
    let why = match V4l2LoopbackCamera::find_or_load().and_then(|mut c| c.open(format).map(|_| c)) {
        Ok(cam) => {
            log::info!("virtual camera: {}", cam.describe());
            return Box::new(cam);
        }
        Err(e) => e,
    };
    #[cfg(windows)]
    let why = {
        let mut cam = WindowsCamera::new();
        match cam.open(format) {
            Ok(()) => {
                log::info!("virtual camera: {}", cam.describe());
                return Box::new(cam);
            }
            Err(e) => e,
        }
    };
    #[cfg(not(any(target_os = "linux", windows)))]
    let why = String::from("no virtual camera backend for this OS yet");
    log::warn!("virtual camera: running in NULL mode (frames go to files, no app can see them): {why}");
    let mut null = NullVirtualCamera::new(null::default_dir(), why);
    if let Err(e) = null.open(format) {
        log::warn!("null virtual camera: {e}");
    }
    Box::new(null)
}

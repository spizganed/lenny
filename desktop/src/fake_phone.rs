//! A phone without a phone: synthetic camera -> openh264 -> the core's sender, over real TCP. For trying the desktop
//! app, screenshots and end-to-end tests where no phone or emulator is available (this sandbox has no /dev/kvm).
//! It answers controls like a phone would (zoom and pan move its synthetic "sensor"; focus/exposure are echoed).
//! Not a camera test: autofocus, exposure and real lenses need a real phone.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lenny_core::session::{SenderConfig, Session};
use lenny_core::wire::{Lens, PAN_CENTER};
use lenny_core::*;
use openh264::encoder::{Encoder, FrameType};
use openh264::formats::YUVBuffer;

#[derive(Default)]
struct Cam {
    want_key: AtomicBool,
    zoom: AtomicI32, // x100
    pan: AtomicU32,  // x << 16 | y
    state: Mutex<lenny_control_state>,
    size: Mutex<(usize, usize, u32)>,
    changed: AtomicBool,
}

unsafe extern "C" fn on_config(u: *mut c_void, req: *const lenny_stream_settings, eff: *mut lenny_stream_settings) {
    let c = &*(u as *const Cam);
    let m = (*req).mode;
    *c.size.lock().unwrap() = (m.width as usize, m.height as usize, (m.fps_num / m.fps_den.max(1)) as u32);
    c.changed.store(true, Relaxed);
    (*eff).bitrate_kbps = (*req).bitrate_kbps.min(4000);
}

unsafe extern "C" fn on_control(u: *mut c_void, ctl: *const lenny_control) -> i32 {
    let c = &*(u as *const Cam);
    let ctl = &*ctl;
    let mut s = c.state.lock().unwrap();
    match ctl.cmd {
        x if x == LENNY_CTL_KEYFRAME_REQUEST as u16 => c.want_key.store(true, Relaxed),
        x if x == LENNY_CTL_ZOOM as u16 => {
            s.zoom = ctl.value.clamp(100, 400) as u16;
            c.zoom.store(s.zoom as i32, Relaxed);
        }
        x if x == LENNY_CTL_PAN as u16 => {
            (s.pan_x, s.pan_y) = (ctl.x, ctl.y);
            c.pan.store((ctl.x as u32) << 16 | ctl.y as u32, Relaxed);
        }
        x if x == LENNY_CTL_FOCUS_AT as u16 => s.af_mode = 1,
        x if x == LENNY_CTL_EXPOSURE_COMP as u16 => s.exposure_comp = ctl.value,
        x if x == LENNY_CTL_EXPOSURE_LOCK as u16 => s.exposure_lock = ctl.value as u8,
        x if x == LENNY_CTL_TORCH as u16 => s.torch = ctl.value as u8,
        x if x == LENNY_CTL_SELECT_LENS as u16 => s.lens_id = ctl.value as u8,
        x if x == LENNY_CTL_RESET_AUTO as u16 => {
            (s.af_mode, s.exposure_comp, s.exposure_lock, s.wb_lock) = (0, 0, 0, 0);
        }
        _ => return LENNY_ACK_UNSUPPORTED,
    }
    c.changed.store(true, Relaxed);
    LENNY_ACK_OK
}

pub struct FakePhone {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakePhone {
    /// Connects to host:port and streams until dropped. `portrait` sends frames that need a 90 degree turn.
    pub fn start(host: &str, port: u16, id: u8, portrait: bool) -> FakePhone {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let host = host.to_string();
        let thread = std::thread::spawn(move || run(&host, port, id, portrait, &st));
        FakePhone { stop, thread: Some(thread) }
    }
}

impl Drop for FakePhone {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn mode(w: u16, h: u16, fps: u16) -> lenny_mode {
    lenny_mode { width: w, height: h, fps_num: fps, fps_den: 1 }
}

fn run(host: &str, port: u16, id: u8, portrait: bool, stop: &AtomicBool) {
    let cam = Box::new(Cam::default());
    cam.zoom.store(100, Relaxed);
    cam.pan.store((PAN_CENTER as u32) << 16 | PAN_CENTER as u32, Relaxed);
    *cam.state.lock().unwrap() =
        lenny_control_state { zoom: 100, battery: 76, pan_x: PAN_CENTER, pan_y: PAN_CENTER, ..Default::default() };
    let back =
        vec![mode(1920, 1080, 30), mode(1280, 720, 30), mode(1280, 720, 60), mode(1440, 1080, 30), mode(960, 720, 30)];
    let front = vec![mode(1280, 720, 30), mode(960, 720, 30)];
    let lenses = vec![
        Lens {
            id: 0,
            facing: 0,
            label: b"1x".to_vec(),
            modes: back.clone(),
            zoom_min: 100,
            zoom_max: 400,
            zoom_base: 100,
        },
        Lens { id: 1, facing: 1, label: b"Front".to_vec(), modes: front, zoom_min: 100, zoom_max: 200, zoom_base: 100 },
    ];
    let cb = lenny_sender_callbacks {
        user: &*cam as *const Cam as *mut c_void,
        on_state: None,
        on_stream_config: Some(on_config),
        on_control: Some(on_control),
        on_bitrate: None,
    };
    let session = Session::new_sender(
        SenderConfig {
            identity: lenny_identity {
                device_id: [id; 16],
                device_name: std::ptr::null(),
                app_version: std::ptr::null(),
                platform: LENNY_PLATFORM_ANDROID,
            },
            device_name: b"Fake phone".to_vec(),
            app_version: b"test".to_vec(),
            modes: back,
            max_bitrate_kbps: 8000,
            controls: LENNY_CAP_FOCUS
                | LENNY_CAP_EXPOSURE_COMP
                | LENNY_CAP_EXPOSURE_LOCK
                | LENNY_CAP_TORCH
                | LENNY_CAP_LENS
                | LENNY_CAP_ZOOM
                | LENNY_CAP_PAN,
            lenses,
            exposure_comp_min: -2000,
            exposure_comp_max: 2000,
            exposure_comp_step_milli: 333,
        },
        cb,
    );
    session.connect(host, port, None);
    let mut enc: Option<(Encoder, usize, usize)> = None;
    let mut t = 0u32;
    let start = Instant::now();
    while !stop.load(Relaxed) {
        let (w, h, fps) = *cam.size.lock().unwrap();
        if session.state() != LENNY_STATE_STREAMING as i32 || w == 0 {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if cam.changed.swap(false, Relaxed) {
            session.send_control_state(&cam.state.lock().unwrap());
        }
        // Encode sensor-native landscape; "portrait" reports orientation 1 like a phone held upright.
        let (ew, eh) = (w & !1, h & !1);
        if enc.as_ref().is_none_or(|e| (e.1, e.2) != (ew, eh)) {
            enc = Encoder::new().ok().map(|e| (e, ew, eh));
            cam.want_key.store(true, Relaxed);
        }
        let Some((e, _, _)) = enc.as_mut() else { return };
        if cam.want_key.swap(false, Relaxed) {
            e.force_intra_frame();
        }
        let yuv = pattern(ew, eh, t, cam.zoom.load(Relaxed), cam.pan.load(Relaxed));
        if let Ok(bs) = e.encode(&yuv) {
            let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            let data = bs.to_vec();
            if key {
                session.send_video_config(&parameter_sets(&data));
            }
            let flags = if key { LENNY_FRAME_KEYFRAME } else { 0 };
            if !data.is_empty() {
                session.send_video_frame(&data, lenny_now_us(), portrait as u8, flags);
            }
        }
        t += 1;
        let next = start + Duration::from_secs_f64(t as f64 / fps.max(1) as f64);
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
    }
    drop(session); // joins the core threads before `cam` (their callback target) goes away
}

/// SPS + PPS NAL units (types 7 and 8) out of an Annex-B keyframe.
fn parameter_sets(au: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let starts: Vec<usize> = (0..au.len().saturating_sub(3)).filter(|&i| au[i..i + 3] == [0, 0, 1]).collect();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(au.len(), |&n| if n > 0 && au[n - 1] == 0 { n - 1 } else { n });
        if let Some(&hdr) = au.get(s + 3) {
            if matches!(hdr & 0x1F, 7 | 8) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&au[s + 3..end]);
            }
        }
    }
    out
}

/// Test card: colour bars over a grid, a moving stripe, and the zoom/pan applied like a sensor crop would.
fn pattern(w: usize, h: usize, t: u32, zoom: i32, pan: u32) -> YUVBuffer {
    let z = zoom.max(100) as f32 / 100.0;
    let (px, py) = ((pan >> 16) as f32 / 65535.0, (pan & 0xFFFF) as f32 / 65535.0);
    let (vw, vh) = (w as f32 / z, h as f32 / z);
    let (x0, y0) = (px * (w as f32 - vw), py * (h as f32 - vh));
    let mut buf = vec![0u8; w * h * 3 / 2];
    let (yp, uv) = buf.split_at_mut(w * h);
    let (up, vp) = uv.split_at_mut(w * h / 4);
    const BARS: [(u8, u8, u8); 6] =
        [(180, 128, 128), (162, 44, 142), (131, 156, 44), (112, 72, 58), (84, 184, 198), (65, 100, 212)];
    let stripe = (t as usize * 8) % w;
    for y in 0..h {
        for x in 0..w {
            let sx = x0 + x as f32 / z; // "sensor" coordinates
            let sy = y0 + y as f32 / z;
            let bar = BARS[(sx as usize * BARS.len() / w).min(BARS.len() - 1)];
            let grid = (sx as usize % 80 < 3) || (sy as usize % 80 < 3);
            let mut yv = if grid { 235 } else { bar.0 };
            if (sx as usize).abs_diff(stripe) < 6 {
                yv = 30;
            }
            yp[y * w + x] = yv;
            if y % 2 == 0 && x % 2 == 0 {
                up[(y / 2) * (w / 2) + x / 2] = bar.1;
                vp[(y / 2) * (w / 2) + x / 2] = bar.2;
            }
        }
    }
    YUVBuffer::from_vec(buf, w, h)
}

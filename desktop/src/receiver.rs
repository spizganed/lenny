//! Everything the desktop does besides drawing: the core receiver session, H.264 decoding, the virtual camera,
//! LAN discovery answers, QR pairing tokens and the list of phones allowed before.
//!
//! Threads: the core's I/O thread (callbacks, never blocked: they only queue), one decoder thread, one virtual-camera
//! thread writing at a steady 30 fps (live frame, or the placeholder when there's none), one discovery thread.
//! Portable as-is except what `lenny_vcam::open_best` picks (v4l2loopback on Linux; DirectShow/MF on Windows later).

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::raw::{c_char, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lenny_core::session::{ReceiverConfig, Session};
use lenny_core::*;
use lenny_vcam::frame::{compose_i420, i420_size, i420_to_rgba, placeholder_i420, upright, I420};
use lenny_vcam::FrameFormat;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// The virtual camera's fixed format; every frame is fitted into it, so consumers never see it change.
pub const VCAM: FrameFormat = FrameFormat { width: 1280, height: 720, fps: 30 };
/// Preview frames are scaled to at most this width (CPU conversion; the virtual camera gets full quality).
const PREVIEW_MAX_W: usize = 960;
/// Last good frame stays on the virtual camera this long, then the placeholder (architecture.md §7.2).
const STALE: Duration = Duration::from_millis(500);
const TOKEN_RENEW: Duration = Duration::from_secs(80); // tokens live 90 s (protocol.md §6.4)

enum Packet {
    Config(Vec<u8>),
    Frame { data: Vec<u8>, orientation: u8, key: bool },
    Stop,
}

/// Upright picture for the preview, exactly the video's aspect ratio (no bars: the UI sizes its box to it).
pub struct Preview {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    /// What the decoder really produced (before rotation), not what was asked for.
    pub source: (usize, usize),
}

struct Shared {
    tx: SyncSender<Packet>,
    repaint: eframe::egui::Context,
    want_keyframe: AtomicBool,
    stream_status: Mutex<Option<(u8, String)>>,
    preview: Mutex<Option<Preview>>,
    preview_seq: AtomicU64,
    vcam_frame: Mutex<(Vec<u8>, Option<Instant>)>,
    placeholder_text: Mutex<String>,
    decoded: AtomicU64,
    stop: AtomicBool,
}

pub struct Engine {
    pub session: Arc<Session>,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    pub name: String,
    vcam_status: Arc<Mutex<(bool, String)>>,
    token: Mutex<Option<(String, Instant)>>,
    known: Mutex<BTreeMap<[u8; 16], String>>,
    last_state: i32,
}

// ---- core callbacks (I/O thread: never block) ----
unsafe fn shared<'a>(u: *mut c_void) -> &'a Shared {
    &*(u as *const Shared)
}
unsafe extern "C" fn on_state(u: *mut c_void, _: i32, _: i32) {
    shared(u).repaint.request_repaint();
}
unsafe extern "C" fn on_approval(u: *mut c_void, _: *const u8, _: *const c_char) {
    shared(u).repaint.request_repaint();
}
unsafe extern "C" fn on_stream_start(u: *mut c_void, _: *const lenny_stream_settings) {
    let s = shared(u);
    *s.stream_status.lock().unwrap() = None;
    s.repaint.request_repaint();
}
unsafe extern "C" fn on_stream_status(u: *mut c_void, state: u8, reason: *const c_char) {
    let s = shared(u);
    let reason = if reason.is_null() { String::new() } else { CStr::from_ptr(reason).to_string_lossy().into_owned() };
    *s.stream_status.lock().unwrap() = (state != LENNY_STREAM_LIVE).then_some((state, reason));
    s.repaint.request_repaint();
}
unsafe extern "C" fn on_video_config(u: *mut c_void, data: *const u8, size: usize) {
    let _ = shared(u).tx.try_send(Packet::Config(std::slice::from_raw_parts(data, size).to_vec()));
}
unsafe extern "C" fn on_video_frame(u: *mut c_void, f: *const lenny_video_frame) {
    let s = shared(u);
    let f = &*f;
    let p = Packet::Frame {
        data: std::slice::from_raw_parts(f.data, f.size).to_vec(),
        orientation: f.orientation,
        key: f.flags & LENNY_FRAME_KEYFRAME != 0,
    };
    // Decoder behind: drop, and restart from a keyframe (the decoder skips P-frames until then).
    if let Err(TrySendError::Full(_)) = s.tx.try_send(p) {
        s.want_keyframe.store(true, Relaxed);
    }
}
unsafe extern "C" fn on_control_state(u: *mut c_void, _: *const lenny_control_state) {
    shared(u).repaint.request_repaint();
}

impl Engine {
    pub fn start(ctx: eframe::egui::Context, port: u16) -> Result<Engine, String> {
        let (tx, rx) = sync_channel(16);
        let shared = Arc::new(Shared {
            tx,
            repaint: ctx,
            want_keyframe: AtomicBool::new(false),
            stream_status: Mutex::new(None),
            preview: Mutex::new(None),
            preview_seq: AtomicU64::new(0),
            vcam_frame: Mutex::new((vec![], None)),
            placeholder_text: Mutex::new("waiting for phone".into()),
            decoded: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let name = host_name();
        let cb = lenny_receiver_callbacks {
            user: Arc::as_ptr(&shared) as *mut c_void,
            on_state: Some(on_state),
            on_approval_needed: Some(on_approval),
            on_stream_start: Some(on_stream_start),
            on_stream_status: Some(on_stream_status),
            on_video_config: Some(on_video_config),
            on_video_frame: Some(on_video_frame),
            on_control_state: Some(on_control_state),
            on_control_ack: None,
        };
        let session = Arc::new(Session::new_receiver(
            ReceiverConfig {
                identity: lenny_identity {
                    device_id: [0; 16],
                    device_name: std::ptr::null(),
                    app_version: std::ptr::null(),
                    platform: if cfg!(windows) { LENNY_PLATFORM_WINDOWS } else { LENNY_PLATFORM_LINUX },
                },
                device_name: name.clone().into_bytes(),
                app_version: env!("CARGO_PKG_VERSION").as_bytes().to_vec(),
                port,
                preferred: lenny_stream_settings {
                    codec: LENNY_CODEC_H264,
                    mode: lenny_mode { width: 1920, height: 1080, fps_num: 30, fps_den: 1 },
                    bitrate_kbps: 8000,
                    has_lens: 0,
                    lens_id: 0,
                },
            },
            cb,
        ));
        let known = load_known();
        for id in known.keys() {
            session.trust(id);
        }
        if session.start() != LENNY_OK {
            return Err(format!("can't listen on TCP port {port} (another Lenny Desktop running?)"));
        }
        let vcam_status = Arc::new(Mutex::new((false, "starting".to_string())));
        let mut threads = vec![];
        let s = shared.clone();
        let sess = session.clone();
        threads
            .push(std::thread::Builder::new().name("decoder".into()).spawn(move || decode_loop(rx, s, sess)).unwrap());
        let s = shared.clone();
        let vs = vcam_status.clone();
        threads.push(std::thread::Builder::new().name("vcam".into()).spawn(move || vcam_loop(s, vs)).unwrap());
        let s = shared.clone();
        let (n, p) = (name.clone(), session.port());
        threads
            .push(std::thread::Builder::new().name("discovery".into()).spawn(move || discovery_loop(s, n, p)).unwrap());
        Ok(Engine {
            session,
            shared,
            threads,
            name,
            vcam_status,
            token: Mutex::new(None),
            known: Mutex::new(known),
            last_state: -1,
        })
    }

    /// Once per UI frame: bookkeeping that reacts to state changes.
    pub fn tick(&mut self) {
        let st = self.session.state();
        if st != self.last_state && st == LENNY_STATE_STREAMING as i32 {
            // A phone that streamed once reconnects without asking; a used QR token is replaced right away.
            if let (true, p) = self.session.peer() {
                let name = c_chars(&p.name);
                self.known.lock().unwrap().insert(p.device_id, name);
                save_known(&self.known.lock().unwrap());
            }
            *self.token.lock().unwrap() = None;
        }
        if st != self.last_state && st != LENNY_STATE_STREAMING as i32 {
            *self.shared.stream_status.lock().unwrap() = None;
        }
        self.last_state = st;
        let text = match st {
            x if x == LENNY_STATE_STREAMING as i32 => match &*self.shared.stream_status.lock().unwrap() {
                Some((LENNY_STREAM_CAMERA_LOST, _)) => "phone camera busy",
                Some(_) => "phone paused",
                None => "waiting for video",
            },
            x if x == LENNY_STATE_AWAITING_APPROVAL as i32 => "allow the phone on the pc",
            x if x == LENNY_STATE_HANDSHAKE as i32 => "connecting",
            _ => "waiting for phone",
        };
        *self.shared.placeholder_text.lock().unwrap() = text.into();
        if self.shared.want_keyframe.swap(false, Relaxed) {
            let mut c = lenny_control { cmd: LENNY_CTL_KEYFRAME_REQUEST as u16, ..Default::default() };
            self.session.send_control(&mut c);
        }
    }

    /// The QR payload (protocol.md §6.4): every IPv4 address, the port, a fresh single-use token, this PC's name.
    pub fn pair_uri(&self) -> String {
        let mut t = self.token.lock().unwrap();
        if t.as_ref().is_none_or(|(_, at)| at.elapsed() > TOKEN_RENEW) {
            let token = self.session.new_pair_token(90_000);
            let uri = pair_uri(&local_ipv4s(), self.session.port(), Some(&token), &self.name);
            *t = Some((uri, Instant::now()));
        }
        t.as_ref().unwrap().0.clone()
    }

    pub fn port(&self) -> u16 {
        self.session.port()
    }

    pub fn take_preview(&self, seen: &mut u64) -> Option<Preview> {
        let seq = self.shared.preview_seq.load(Relaxed);
        if seq == *seen {
            return None;
        }
        *seen = seq;
        self.shared.preview.lock().unwrap().take()
    }

    /// When the last frame was decoded, for "waiting for video" and stats.
    pub fn last_frame_at(&self) -> Option<Instant> {
        self.shared.vcam_frame.lock().unwrap().1
    }

    pub fn decoded_frames(&self) -> u64 {
        self.shared.decoded.load(Relaxed)
    }

    pub fn stream_status(&self) -> Option<(u8, String)> {
        self.shared.stream_status.lock().unwrap().clone()
    }

    /// (real device?, one-line description).
    pub fn vcam_status(&self) -> (bool, String) {
        self.vcam_status.lock().unwrap().clone()
    }

    pub fn known_phones(&self) -> Vec<([u8; 16], String)> {
        self.known.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Stops remembering a phone. The running session can't un-trust it, so it takes effect at the next start.
    pub fn forget(&self, id: &[u8; 16]) {
        let mut k = self.known.lock().unwrap();
        k.remove(id);
        save_known(&k);
    }

    pub fn control(&self, cmd: lenny_control_cmd, x: u16, y: u16, value: i32) {
        let mut c = lenny_control { cmd: cmd as u16, x, y, value, ..Default::default() };
        let r = self.session.send_control(&mut c);
        if r != LENNY_OK {
            log::debug!("control {cmd:?} not sent: {r}");
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        let _ = self.shared.tx.send(Packet::Stop);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Last Arc<Session>: joins the I/O thread (no callbacks after this), sends GOODBYE(NORMAL) so the phone
        // keeps retrying and comes back when the app does.
    }
}

// ---- decoder thread ----
fn decode_loop(rx: Receiver<Packet>, s: Arc<Shared>, session: Arc<Session>) {
    let mut dec = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::error!("H.264 decoder unavailable: {e}");
            return;
        }
    };
    let mut need_key = true;
    let mut canvas = vec![0u8; VCAM.frame_size()];
    let mut last_key_request = Instant::now() - Duration::from_secs(10);
    let mut request_key = |need_key: &mut bool| {
        *need_key = true;
        if last_key_request.elapsed() > Duration::from_secs(1) {
            let mut c = lenny_control { cmd: LENNY_CTL_KEYFRAME_REQUEST as u16, ..Default::default() };
            session.send_control(&mut c);
            last_key_request = Instant::now();
        }
    };
    while let Ok(p) = rx.recv() {
        match p {
            Packet::Stop => return,
            // SPS/PPS: the decoder keeps them for the frames that follow.
            Packet::Config(c) => {
                if let Err(e) = dec.decode(&c) {
                    log::warn!("bad video config: {e}");
                }
            }
            Packet::Frame { data, orientation, key } => {
                if s.want_keyframe.load(Relaxed) {
                    need_key = true;
                }
                if need_key && !key {
                    continue; // P-frames before the next keyframe would decode as garbage
                }
                match dec.decode(&data) {
                    Ok(Some(yuv)) => {
                        need_key = false;
                        let (w, h) = yuv.dimensions();
                        let (ys, us, _) = yuv.strides();
                        let pic = I420 {
                            y: yuv.y(),
                            u: yuv.u(),
                            v: yuv.v(),
                            y_stride: ys,
                            uv_stride: us,
                            width: w,
                            height: h,
                        };
                        present(&s, &pic, orientation, &mut canvas);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Corrupt or missing reference: ask for a keyframe; the virtual camera keeps the last good
                        // frame for 500 ms, then shows the placeholder.
                        log::warn!("decode error: {e}");
                        request_key(&mut need_key);
                    }
                }
            }
        }
    }
}

/// One decoded frame to the virtual camera (fitted into its fixed canvas) and to the preview (upright, own aspect).
fn present(s: &Shared, pic: &I420, rotation: u8, canvas: &mut Vec<u8>) {
    compose_i420(pic, rotation, canvas, VCAM.width as usize, VCAM.height as usize);
    {
        let mut v = s.vcam_frame.lock().unwrap();
        std::mem::swap(&mut v.0, canvas);
        v.1 = Some(Instant::now());
        if canvas.len() != VCAM.frame_size() {
            canvas.resize(VCAM.frame_size(), 0);
        }
    }
    let (rw, rh) = upright(pic.width, pic.height, rotation);
    let pw = rw.min(PREVIEW_MAX_W) & !1;
    let ph = ((rh * pw / rw.max(1)) & !1).max(2);
    let mut small = vec![0u8; i420_size(pw, ph)];
    compose_i420(pic, rotation, &mut small, pw, ph);
    let mut rgba = vec![0u8; pw * ph * 4];
    i420_to_rgba(&small, pw, ph, &mut rgba);
    *s.preview.lock().unwrap() = Some(Preview { width: pw, height: ph, rgba, source: (pic.width, pic.height) });
    s.preview_seq.fetch_add(1, Relaxed);
    s.decoded.fetch_add(1, Relaxed);
    s.repaint.request_repaint();
}

// ---- virtual camera thread ----
fn vcam_loop(s: Arc<Shared>, status: Arc<Mutex<(bool, String)>>) {
    let mut cam = lenny_vcam::open_best(VCAM);
    *status.lock().unwrap() = (cam.is_real(), cam.describe());
    s.repaint.request_repaint();
    let mut placeholder = (String::new(), vec![]);
    let mut failing = false;
    let period = Duration::from_secs(1) / VCAM.fps;
    let mut next = Instant::now();
    while !s.stop.load(Relaxed) {
        {
            let f = s.vcam_frame.lock().unwrap();
            let live = f.1.is_some_and(|t| t.elapsed() < STALE);
            let r = if live {
                cam.write_frame(&f.0)
            } else {
                drop(f);
                let text = s.placeholder_text.lock().unwrap().clone();
                if placeholder.0 != text {
                    placeholder = (text.clone(), placeholder_i420(VCAM.width as usize, VCAM.height as usize, &text));
                }
                cam.write_frame(&placeholder.1)
            };
            // Never let the device go away: log once, keep trying (a consumer may be holding it open).
            match r {
                Err(e) if !failing => {
                    log::warn!("virtual camera write failed: {e}");
                    failing = true;
                }
                Ok(()) => failing = false,
                _ => {}
            }
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    cam.close();
}

// ---- discovery (protocol.md §2): answer "LENNY?1" on UDP 47474 with a host-less link ----
fn discovery_loop(s: Arc<Shared>, name: String, tcp_port: u16) {
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, LENNY_DEFAULT_PORT)) {
        Ok(sock) => sock,
        Err(e) => {
            log::warn!("discovery off (UDP {LENNY_DEFAULT_PORT}: {e}); phones can still scan the QR code");
            return;
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
    let answer = pair_uri(&[], tcp_port, None, &name);
    let mut buf = [0u8; 64];
    while !s.stop.load(Relaxed) {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if &buf[..n] == b"LENNY?1" {
                let _ = sock.send_to(answer.as_bytes(), from);
            }
        }
    }
}

// ---- helpers ----

/// `lenny://c?v=1&h=<ip>,<ip>&p=<port>&t=<token>&n=<name>` (protocol.md §6.4; the phone parses it with Dart's Uri).
pub fn pair_uri(hosts: &[String], port: u16, token: Option<&[u8; 16]>, name: &str) -> String {
    let mut q = vec![("v", "1".to_string())];
    if !hosts.is_empty() {
        q.push(("h", hosts.join(",")));
    }
    q.push(("p", port.to_string()));
    if let Some(t) = token {
        q.push(("t", base64url(t)));
    }
    if !name.is_empty() {
        q.push(("n", name.to_string()));
    }
    let enc = |v: &str| -> String {
        v.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let query: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
    format!("lenny://c?{}", query.join("&"))
}

fn base64url(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out // unpadded, like the phone expects
}

/// This PC's IPv4 addresses a phone could reach (no loopback, no link-local).
pub fn local_ipv4s() -> Vec<String> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(a) if !a.is_loopback() && !a.is_link_local() => Some(a.to_string()),
            _ => None,
        })
        .collect()
}

fn host_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Lenny Desktop".into())
}

pub fn c_chars(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn known_path() -> Option<PathBuf> {
    let base = std::env::var_os("LENNY_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join("lenny")))
        .or_else(|| std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Lenny")))
        .or_else(|| std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".config/lenny")))?;
    Some(base.join("known_phones.txt"))
}

/// "<32 hex chars> <name>" per line.
fn load_known() -> BTreeMap<[u8; 16], String> {
    let text = known_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (hex, name) = l.split_once(' ').unwrap_or((l, ""));
            let mut id = [0u8; 16];
            for (i, b) in id.iter_mut().enumerate() {
                *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
            }
            Some((id, name.to_string()))
        })
        .collect()
}

fn save_known(k: &BTreeMap<[u8; 16], String>) {
    let Some(p) = known_path() else { return };
    let text: String = k
        .iter()
        .map(|(id, name)| format!("{} {name}\n", id.iter().map(|b| format!("{b:02x}")).collect::<String>()))
        .collect();
    if let Err(e) = p.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&p, text)) {
        log::warn!("can't save known phones to {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_uri_matches_the_phone_parser_format() {
        let t = [0xFBu8; 16];
        let uri = pair_uri(&["192.168.1.20".into(), "10.0.0.2".into()], 47474, Some(&t), "My PC");
        assert_eq!(uri, "lenny://c?v=1&h=192.168.1.20%2C10.0.0.2&p=47474&t=-_v7-_v7-_v7-_v7-_v7-w&n=My%20PC");
        assert_eq!(pair_uri(&[], 1, None, ""), "lenny://c?v=1&p=1");
    }

    #[test]
    fn base64url_is_unpadded() {
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0u8; 16]).len(), 22);
    }

    #[test]
    fn known_phones_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lenny-known-{}", std::process::id()));
        std::env::set_var("LENNY_CONFIG_DIR", &dir);
        let mut k = BTreeMap::new();
        k.insert([0xAB; 16], "Pixel 9".to_string());
        save_known(&k);
        assert_eq!(load_known(), k);
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! Session: handshake, pairing/approval, capability negotiation, keepalive, reconnect (protocol.md §7).
//!
//! Threads: one I/O thread per session (connect/accept, read, dispatch, keepalive), plus a video writer thread
//! on senders so a slow network never blocks the encoder (protocol.md §9). Callbacks run on those threads.

use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU8, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::abi::*;
use crate::pairing::{TokenStore, DEFAULT_TTL_US};
use crate::timing::{now_us, ClockSync};
use crate::transport::{tcp_connect, TcpListener, Transport};
use crate::wire::{self, msg, DeviceId, Message};

const MS: i64 = 1000;
const SEC: i64 = 1000 * MS;
const HELLO_TIMEOUT: i64 = 5 * SEC;
const PAIR_TIMEOUT: i64 = 30 * SEC;
const CAPS_TIMEOUT: i64 = 5 * SEC;
const APPROVAL_TIMEOUT: i64 = 30 * SEC;
/// Sender waits for CAPS_SELECT while the receiver's user may be looking at the approve prompt.
const SENDER_CAPS_TIMEOUT: i64 = APPROVAL_TIMEOUT + 5 * SEC;
const PING_INTERVAL: i64 = SEC;
const SILENCE_LIMIT: i64 = 3 * SEC;
const CONNECT_TIMEOUT_MS: i32 = 2000;
const BACKOFF_MS: [u64; 4] = [250, 500, 1000, 2000];
const DEFAULT_BITRATE_KBPS: u32 = 8000;
const MAX_VIDEO_CONFIG: usize = 60000; // must fit one TLV field
/// Congestion control (protocol.md §9): more than this much video waiting to be sent = we're behind.
/// Measured in capture time, not bytes: a single keyframe can be bigger than 250 ms of average bitrate.
const QUEUE_BUDGET_US: i64 = 250 * MS;
const MIN_BITRATE_KBPS: u32 = 1000;
const RAISE_INTERVAL: i64 = 5 * SEC; // healthy for this long -> +10% bitrate
/// Kernel send buffer on the phone. Autotuned buffers can grow to megabytes and hide a second of backlog from us.
const SENDER_SOCKET_BUFFER: usize = 128 * 1024;
/// run_link / dispatch result: keep going. Anything else is a LENNY_REASON_*.
const CONTINUE: i32 = -1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Sender = 1,
    Receiver = 2,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    HelloWait,
    PairWait,
    PairOrCaps,
    ApprovalWait,
    CapsWait,
    StreamStartWait,
    Streaming,
}

/// C callback tables hold a `void* user` the caller promises is usable from our threads.
#[derive(Clone, Copy)]
struct Cb<T>(T);
unsafe impl<T> Send for Cb<T> {}
unsafe impl<T> Sync for Cb<T> {}

fn fatal_for_sender(reason: i32) -> bool {
    matches!(reason, LENNY_REASON_VERSION | LENNY_REASON_ROLE | LENNY_REASON_PAIR_DENIED | LENNY_REASON_USER)
}

/// Bytes up to the first NUL, as a C string (what a C++ `c_str()` of the same bytes reads as).
fn c_string(b: &[u8]) -> CString {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    CString::new(&b[..end]).unwrap_or_default()
}

fn copy_c(dst: &mut [std::os::raw::c_char], src: &[u8]) {
    let n = src.len().min(dst.len() - 1);
    for (d, s) in dst.iter_mut().zip(&src[..n]) {
        *d = *s as std::os::raw::c_char;
    }
    dst[n] = 0;
}

fn fps(m: &lenny_mode) -> f64 {
    if m.fps_den != 0 {
        m.fps_num as f64 / m.fps_den as f64
    } else {
        0.0
    }
}

/// Receiver's pick: the sender mode closest to the preferred one (area first, then fps).
pub fn choose_settings(caps: &wire::Caps, pref: lenny_stream_settings) -> lenny_stream_settings {
    let mut s = pref;
    if s.codec == 0 {
        s.codec = LENNY_CODEC_H264;
    }
    if !caps.codecs.is_empty() && !caps.codecs.iter().any(|c| c.id == s.codec) {
        s.codec = caps.codecs[0].id;
    }
    let area = pref.mode.width as f64 * pref.mode.height as f64;
    let score = |m: &lenny_mode| ((m.width as f64 * m.height as f64 - area).abs(), (fps(m) - fps(&pref.mode)).abs());
    let mut best: Option<(lenny_mode, (f64, f64))> = None;
    for m in &caps.modes {
        let sc = score(m);
        if best.is_none_or(|(_, b)| sc.0 < b.0 || (sc.0 == b.0 && sc.1 < b.1)) {
            best = Some((*m, sc));
        }
    }
    if let Some((m, _)) = best {
        s.mode = m;
    }
    if s.bitrate_kbps == 0 {
        s.bitrate_kbps = DEFAULT_BITRATE_KBPS;
    }
    if caps.max_bitrate_kbps != 0 {
        s.bitrate_kbps = s.bitrate_kbps.min(caps.max_bitrate_kbps);
    }
    if s.has_lens != 0 && !caps.lenses.iter().any(|l| l.id == s.lens_id) {
        s.has_lens = 0;
    }
    s
}

fn make_hello(id: &lenny_identity, role: Role, name: Vec<u8>, version: Vec<u8>) -> wire::Hello {
    wire::Hello {
        role: role as u8,
        device_id: id.device_id,
        device_name: name,
        app_version: version,
        platform: id.platform,
        ..Default::default()
    }
}

/// Sender config, already copied out of the C structs.
pub struct SenderConfig {
    pub identity: lenny_identity,
    pub device_name: Vec<u8>,
    pub app_version: Vec<u8>,
    pub modes: Vec<lenny_mode>,
    pub max_bitrate_kbps: u32,
    pub controls: u32,
    pub lenses: Vec<wire::Lens>,
    pub exposure_comp_min: i32,
    pub exposure_comp_max: i32,
    pub exposure_comp_step_milli: u32,
}

pub struct ReceiverConfig {
    pub identity: lenny_identity,
    pub device_name: Vec<u8>,
    pub app_version: Vec<u8>,
    pub port: u16,
    pub preferred: lenny_stream_settings,
}

#[derive(Default)]
struct Info {
    has_peer: bool,
    has_settings: bool,
    has_control_state: bool,
    has_caps: bool, // peer_caps_copy is from the current peer
    peer_info: lenny_peer_info,
    settings: lenny_stream_settings,
    control_state: lenny_control_state,
    peer_caps_copy: wire::Caps, // receiver: last CAPS, for select_stream from the UI thread
    preferred: lenny_stream_settings, // receiver (the UI changes it)
}

struct Outgoing {
    msg: Vec<u8>,
    payload: usize, // video bytes, for stats
    pts_us: i64,    // frames only
    frame: bool,
    key: bool,
}

impl Outgoing {
    fn config(msg: Vec<u8>) -> Self {
        Self { msg, payload: 0, pts_us: 0, frame: false, key: false }
    }
}

/// Sender video queue (protocol.md §9). The encoder thread copies frames in; the video writer sends them.
struct Vq {
    q: VecDeque<Outgoing>,
    bytes: usize,
    drop_until_key: bool,
    stop: bool,
    video_config: Vec<u8>, // resent before every keyframe
    frame_seq: u32,
    target_kbps: u32,  // negotiated
    current_kbps: u32, // after congestion control
    last_congestion: i64,
    next_raise: i64,
}

impl Vq {
    /// Keep only the newest queued keyframe (and the config right before it): it's what the receiver can restart
    /// from. Everything else, especially P-frames that depend on dropped frames, goes.
    fn drop_backlog(&mut self) {
        let mut keep = VecDeque::new();
        if let Some(i) = self.q.iter().rposition(|o| o.key) {
            if i > 0 && !self.q[i - 1].frame {
                keep.push_back(self.q.remove(i - 1).unwrap());
                keep.push_back(self.q.remove(i - 1).unwrap());
            } else {
                keep.push_back(self.q.remove(i).unwrap());
            }
        }
        self.q = keep;
        self.bytes = self.q.iter().map(|o| o.msg.len()).sum();
    }
}

struct Stats {
    s: lenny_stats,
    clock: ClockSync,
}

struct Inner {
    role: Role,
    hello: wire::Hello, // ours
    scb: Cb<lenny_sender_callbacks>,
    rcb: Cb<lenny_receiver_callbacks>,
    caps: wire::Caps, // sender: ours
    listen_port: u16,

    stop: AtomicBool,
    io_done: AtomicBool,
    last_reason: AtomicI32,
    /// GOODBYE sent when stopping: USER from disconnect() (phone stops retrying), NORMAL from drop
    /// (app quitting/restarting: the phone keeps retrying and resumes when the app is back).
    bye_reason: AtomicI32,
    sleep_mu: Mutex<()>,
    sleep_cv: Condvar,
    state: AtomicI32,

    listener: Mutex<Cb<(lenny_event_fn, *mut c_void)>>,
    info: Mutex<Info>,

    /// Current link. Written by the I/O thread; read by senders on other threads.
    link: Mutex<Option<Arc<dyn Transport>>>,
    send_mu: Mutex<()>, // serializes whole messages on the socket
    streaming: AtomicBool,
    minor: AtomicU8, // negotiated; read by sender threads
    ping_seq: AtomicU32,
    ever_streamed: AtomicBool, // counts reconnects

    vq: Mutex<Vq>,
    vq_cv: Condvar,

    // Receiver
    bound_port: AtomicU16,
    tokens: TokenStore,
    trusted: Mutex<HashSet<DeviceId>>,
    approval: AtomicI32, // -1 pending, 0 deny, 1 accept
    next_req_id: AtomicU32,

    stats: Mutex<Stats>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking callback must not wedge the session forever: keep using the data.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Session {
    inner: Arc<Inner>,
    io: Mutex<Option<JoinHandle<()>>>,
    video_writer: Option<JoinHandle<()>>,
}

impl Session {
    fn with(
        role: Role,
        hello: wire::Hello,
        scb: lenny_sender_callbacks,
        rcb: lenny_receiver_callbacks,
        caps: wire::Caps,
        port: u16,
        preferred: lenny_stream_settings,
    ) -> Session {
        let inner = Arc::new(Inner {
            role,
            hello,
            scb: Cb(scb),
            rcb: Cb(rcb),
            caps,
            listen_port: port,
            stop: AtomicBool::new(false),
            io_done: AtomicBool::new(true),
            last_reason: AtomicI32::new(0),
            bye_reason: AtomicI32::new(LENNY_REASON_USER),
            sleep_mu: Mutex::new(()),
            sleep_cv: Condvar::new(),
            state: AtomicI32::new(LENNY_STATE_IDLE as i32),
            listener: Mutex::new(Cb((None, std::ptr::null_mut()))),
            info: Mutex::new(Info { preferred, ..Default::default() }),
            link: Mutex::new(None),
            send_mu: Mutex::new(()),
            streaming: AtomicBool::new(false),
            minor: AtomicU8::new(wire::VERSION_MINOR),
            ping_seq: AtomicU32::new(0),
            ever_streamed: AtomicBool::new(false),
            vq: Mutex::new(Vq {
                q: VecDeque::new(),
                bytes: 0,
                drop_until_key: true,
                stop: false,
                video_config: vec![],
                frame_seq: 0,
                target_kbps: 0,
                current_kbps: 0,
                last_congestion: 0,
                next_raise: 0,
            }),
            vq_cv: Condvar::new(),
            bound_port: AtomicU16::new(0),
            tokens: TokenStore::default(),
            trusted: Mutex::new(HashSet::new()),
            approval: AtomicI32::new(-1),
            next_req_id: AtomicU32::new(1),
            stats: Mutex::new(Stats { s: lenny_stats::default(), clock: ClockSync::default() }),
        });
        let video_writer = (role == Role::Sender).then(|| {
            let i = inner.clone();
            std::thread::Builder::new().name("lenny-video".into()).spawn(move || i.video_writer_loop()).expect("spawn")
        });
        Session { inner, io: Mutex::new(None), video_writer }
    }

    pub fn new_sender(cfg: SenderConfig, cb: lenny_sender_callbacks) -> Session {
        let mut caps = wire::Caps {
            codecs: vec![wire::Codec { id: LENNY_CODEC_H264, profile: 0, level: 0 }],
            modes: cfg.modes,
            max_bitrate_kbps: cfg.max_bitrate_kbps,
            controls: cfg.controls as u64,
            lenses: cfg.lenses,
            ..Default::default()
        };
        if cfg.controls & LENNY_CAP_EXPOSURE_COMP != 0 {
            caps.has_exposure_range = true;
            caps.exposure_min = cfg.exposure_comp_min;
            caps.exposure_max = cfg.exposure_comp_max;
            caps.exposure_step_milli = cfg.exposure_comp_step_milli;
        }
        let hello = make_hello(&cfg.identity, Role::Sender, cfg.device_name, cfg.app_version);
        // SAFETY: all-zero is a valid callback table (null user, no callbacks).
        let none: lenny_receiver_callbacks = unsafe { std::mem::zeroed() };
        Self::with(Role::Sender, hello, cb, none, caps, 0, lenny_stream_settings::default())
    }

    pub fn new_receiver(cfg: ReceiverConfig, cb: lenny_receiver_callbacks) -> Session {
        let hello = make_hello(&cfg.identity, Role::Receiver, cfg.device_name, cfg.app_version);
        // SAFETY: as above.
        let none: lenny_sender_callbacks = unsafe { std::mem::zeroed() };
        Self::with(Role::Receiver, hello, none, cb, wire::Caps::default(), cfg.port, cfg.preferred)
    }

    pub fn role(&self) -> Role {
        self.inner.role
    }

    /// False if the previous I/O thread is still running.
    fn join_finished_thread(&self) -> bool {
        let mut io = lock(&self.io);
        if io.is_none() {
            return true;
        }
        if !self.inner.io_done.load(SeqCst) {
            return false;
        }
        let _ = io.take().unwrap().join();
        true
    }

    fn spawn_io(&self, f: impl FnOnce(Arc<Inner>) + Send + 'static) {
        let i = self.inner.clone();
        i.stop.store(false, SeqCst);
        i.io_done.store(false, SeqCst);
        let h = std::thread::Builder::new()
            .name("lenny-io".into())
            .spawn(move || {
                // Marks the thread finished even if a callback panics, so destroy() never waits forever.
                struct Done(Arc<Inner>);
                impl Drop for Done {
                    fn drop(&mut self) {
                        self.0.io_done.store(true, SeqCst);
                    }
                }
                let d = Done(i.clone());
                f(i);
                drop(d);
            })
            .expect("spawn");
        *lock(&self.io) = Some(h);
    }

    // ---- sender ----
    pub fn connect(&self, host: &str, port: u16, pair_token: Option<&[u8; LENNY_PAIR_TOKEN_SIZE]>) -> i32 {
        if self.inner.role != Role::Sender || host.is_empty() || port == 0 {
            return LENNY_E_INVALID_ARG;
        }
        if !self.join_finished_thread() {
            return LENNY_E_STATE;
        }
        let token = pair_token.map(|t| t.to_vec()).unwrap_or_default();
        let host = host.to_owned();
        self.spawn_io(move |i| i.sender_loop(&host, port, token));
        LENNY_OK
    }

    pub fn send_video_config(&self, data: &[u8]) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Sender {
            return LENNY_E_STATE;
        }
        if data.is_empty() || data.len() > MAX_VIDEO_CONFIG {
            return LENNY_E_INVALID_ARG;
        }
        {
            let mut vq = lock(&s.vq);
            vq.video_config = data.to_vec();
            // Queued (not sent directly) so it stays in order with the frames around it.
            if s.streaming.load(SeqCst) {
                let m = wire::to_message(
                    &wire::VideoConfig { codec: LENNY_CODEC_H264, config: vq.video_config.clone() },
                    s.minor.load(SeqCst),
                );
                vq.bytes += m.len();
                vq.q.push_back(Outgoing::config(m));
            }
        }
        s.vq_cv.notify_one();
        LENNY_OK
    }

    pub fn send_video_frame(&self, data: &[u8], pts_us: i64, orientation: u8, flags: u8) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Sender {
            return LENNY_E_STATE;
        }
        if data.is_empty() || data.len() > wire::MAX_VIDEO_PAYLOAD as usize - wire::VIDEO_FRAME_META_SIZE {
            return LENNY_E_INVALID_ARG;
        }
        if !s.streaming.load(SeqCst) {
            return LENNY_E_STATE;
        }
        let key = flags & LENNY_FRAME_KEYFRAME != 0;
        let mut congested = false;
        let mut new_kbps = 0u32;
        {
            let mut vq = lock(&s.vq);
            if vq.drop_until_key && !key {
                return LENNY_OK; // P-frames without their keyframe are useless
            }
            let oldest = vq.q.iter().find(|o| o.frame).map(|o| o.pts_us);
            if !key && oldest.is_some_and(|o| pts_us - o > QUEUE_BUDGET_US) {
                // Behind by more than ~250 ms: that video is too old to be worth sending. Drop the backlog, restart
                // from a fresh keyframe, and send less from now on.
                vq.drop_backlog();
                vq.drop_until_key = true;
                congested = true;
                vq.last_congestion = now_us();
                new_kbps = MIN_BITRATE_KBPS.max(vq.current_kbps * 8 / 10);
                if new_kbps == vq.current_kbps {
                    new_kbps = 0;
                }
                if new_kbps != 0 {
                    vq.current_kbps = new_kbps;
                }
            } else {
                let minor = s.minor.load(SeqCst);
                if key {
                    vq.drop_until_key = false;
                    // SPS/PPS before every keyframe, so a receiver that just joined or lost its decoder recovers (§6.8).
                    if !vq.video_config.is_empty() {
                        let m = wire::to_message(
                            &wire::VideoConfig { codec: LENNY_CODEC_H264, config: vq.video_config.clone() },
                            minor,
                        );
                        vq.bytes += m.len();
                        vq.q.push_back(Outgoing::config(m));
                    }
                }
                let meta_len = wire::VIDEO_FRAME_META_SIZE;
                let mut m = vec![0u8; wire::HEADER_SIZE + meta_len + data.len()];
                let h = wire::Header {
                    ver_minor: minor,
                    typ: msg::VIDEO_FRAME,
                    length: (meta_len + data.len()) as u32,
                    ..Default::default()
                };
                wire::put_header(&mut m, &h);
                let seq = vq.frame_seq;
                vq.frame_seq = seq.wrapping_add(1);
                wire::encode_video_meta(
                    &mut m[wire::HEADER_SIZE..],
                    &wire::VideoFrameMeta { frame_seq: seq, pts_us, orientation: orientation & 3, flags },
                );
                m[wire::HEADER_SIZE + meta_len..].copy_from_slice(data);
                vq.bytes += m.len();
                vq.q.push_back(Outgoing { msg: m, payload: data.len(), pts_us, frame: true, key });
            }
        }
        if congested {
            {
                let mut st = lock(&s.stats);
                st.s.dropped_frames += 1;
                if new_kbps != 0 {
                    st.s.bitrate_kbps = new_kbps;
                }
            }
            s.keyframe_request();
            if new_kbps != 0 {
                if let Some(f) = s.scb.0.on_bitrate {
                    unsafe { f(s.scb.0.user, new_kbps) };
                }
            }
            return LENNY_OK;
        }
        s.vq_cv.notify_one();
        LENNY_OK
    }

    pub fn update_stream(&self, eff: &lenny_stream_settings) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Sender || !s.streaming.load(SeqCst) {
            return LENNY_E_STATE;
        }
        lock(&s.info).settings = *eff;
        if s.send(&wire::StreamStart(*eff)) {
            LENNY_OK
        } else {
            LENNY_E_IO
        }
    }

    pub fn send_control_state(&self, cs: &lenny_control_state) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Sender || !s.streaming.load(SeqCst) {
            return LENNY_E_STATE;
        }
        if s.send(&wire::ControlState(*cs)) {
            LENNY_OK
        } else {
            LENNY_E_IO
        }
    }

    pub fn send_stream_status(&self, state: u8, reason: &[u8]) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Sender || !s.streaming.load(SeqCst) {
            return LENNY_E_STATE;
        }
        if s.send(&wire::StreamStatus { state, reason: reason.to_vec() }) {
            LENNY_OK
        } else {
            LENNY_E_IO
        }
    }

    // ---- receiver ----
    pub fn start(&self) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Receiver {
            return LENNY_E_STATE;
        }
        if !self.join_finished_thread() {
            return LENNY_E_STATE;
        }
        let Some(listener) = TcpListener::listen(s.listen_port) else { return LENNY_E_IO };
        s.bound_port.store(listener.port(), SeqCst);
        self.spawn_io(move |i| i.receiver_loop(listener));
        LENNY_OK
    }

    pub fn port(&self) -> u16 {
        self.inner.bound_port.load(SeqCst)
    }

    pub fn new_pair_token(&self, ttl_ms: u32) -> wire::PairToken {
        let ttl = if ttl_ms != 0 { ttl_ms as i64 * MS } else { DEFAULT_TTL_US };
        self.inner.tokens.issue(now_us(), ttl)
    }

    pub fn trust(&self, id: &DeviceId) {
        self.inner.trust(id)
    }

    pub fn approve(&self, accept: bool) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Receiver || s.state.load(SeqCst) != LENNY_STATE_AWAITING_APPROVAL as i32 {
            return LENNY_E_STATE;
        }
        s.approval.store(accept as i32, SeqCst);
        LENNY_OK
    }

    pub fn send_control(&self, c: &mut lenny_control) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Receiver {
            return LENNY_E_STATE;
        }
        if c.cmd < LENNY_CTL_KEYFRAME_REQUEST as u16 || c.cmd > LENNY_CTL_PAN as u16 {
            return LENNY_E_INVALID_ARG;
        }
        if !s.streaming.load(SeqCst) {
            return LENNY_E_STATE;
        }
        if c.cmd == LENNY_CTL_PAN as u16 && s.minor.load(SeqCst) < 1 {
            return LENNY_E_STATE; // a 1.0 phone doesn't know pan (§4)
        }
        c.req_id = s.next_req_id.fetch_add(1, SeqCst);
        if s.send(&wire::Control(*c)) {
            LENNY_OK
        } else {
            LENNY_E_IO
        }
    }

    pub fn select_stream(&self, preferred: &lenny_stream_settings) -> i32 {
        let s = &*self.inner;
        if s.role != Role::Receiver {
            return LENNY_E_STATE;
        }
        let caps = {
            let mut info = lock(&s.info);
            info.preferred = *preferred;
            info.peer_caps_copy.clone()
        };
        if !s.streaming.load(SeqCst) {
            return LENNY_OK; // used at the next CAPS
        }
        // The phone answers with STREAM_START + a new VIDEO_CONFIG and keyframe (protocol.md §6.6).
        if s.send(&wire::CapsSelect(choose_settings(&caps, *preferred))) {
            LENNY_OK
        } else {
            LENNY_E_IO
        }
    }

    // ---- common ----
    pub fn set_event_listener(&self, f: lenny_event_fn, user: *mut c_void) {
        *lock(&self.inner.listener) = Cb((f, user));
    }

    pub fn peer(&self) -> (bool, lenny_peer_info) {
        let i = lock(&self.inner.info);
        (i.has_peer, i.peer_info)
    }

    pub fn stream_settings(&self) -> (bool, lenny_stream_settings) {
        let i = lock(&self.inner.info);
        (i.has_settings, i.settings)
    }

    /// Receiver: the phone's full CAPS, including per-lens modes and zoom ranges (1.1), which lenny_peer_info doesn't
    /// carry. None until CAPS arrived.
    pub fn peer_caps(&self) -> Option<wire::Caps> {
        let i = lock(&self.inner.info);
        i.has_caps.then(|| i.peer_caps_copy.clone())
    }

    pub fn control_state(&self) -> (bool, lenny_control_state) {
        let i = lock(&self.inner.info);
        (i.has_control_state, i.control_state)
    }

    pub fn state(&self) -> i32 {
        self.inner.state.load(SeqCst)
    }

    pub fn stats(&self) -> lenny_stats {
        let st = lock(&self.inner.stats);
        lenny_stats { rtt_us: st.clock.rtt_us(), clock_offset_us: st.clock.offset_us(), ..st.s }
    }

    pub fn disconnect(&self) {
        self.inner.disconnect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let s = &*self.inner;
        s.bye_reason.store(LENNY_REASON_NORMAL, SeqCst);
        s.disconnect();
        if let Some(io) = lock(&self.io).take() {
            // Normally the I/O thread exits within ~50 ms. If it's stuck in a blocked send, force the socket shut.
            for _ in 0..20 {
                if s.io_done.load(SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !s.io_done.load(SeqCst) {
                if let Some(l) = lock(&s.link).as_ref() {
                    l.shutdown();
                }
            }
            let _ = io.join();
        }
        if let Some(w) = self.video_writer.take() {
            lock(&s.vq).stop = true;
            s.vq_cv.notify_all();
            let _ = w.join();
        }
    }
}

// ---- shared helpers + threads ----
impl Inner {
    fn disconnect(&self) {
        self.stop.store(true, SeqCst);
        let _g = lock(&self.sleep_mu);
        self.sleep_cv.notify_all();
    }

    fn sleep_interruptible(&self, ms: u64) {
        let g = lock(&self.sleep_mu);
        let _ = self.sleep_cv.wait_timeout_while(g, Duration::from_millis(ms), |_| !self.stop.load(SeqCst));
    }

    fn trust(&self, id: &DeviceId) {
        lock(&self.trusted).insert(*id);
    }

    fn trusted(&self, id: &DeviceId) -> bool {
        lock(&self.trusted).contains(id)
    }

    fn emit(&self, event: lenny_event, a: i32, b: i32) {
        // Called under the lock on purpose: once set_event_listener(NULL) returns, no call is in flight, so the UI
        // can free its callback (Dart: NativeCallable.close) right after.
        let l = lock(&self.listener);
        if let Some(f) = l.0 .0 {
            unsafe { f(l.0 .1, event as i32, a, b) };
        }
    }

    fn set_state(&self, st: lenny_state, reason: i32) {
        let old = self.state.swap(st as i32, SeqCst);
        let old_reason = self.last_reason.swap(reason, SeqCst);
        if old == st as i32 && old_reason == reason {
            return;
        }
        let f = if self.role == Role::Sender {
            (self.scb.0.on_state, self.scb.0.user)
        } else {
            (self.rcb.0.on_state, self.rcb.0.user)
        };
        if let Some(cb) = f.0 {
            unsafe { cb(f.1, st as i32, reason) };
        }
        self.emit(LENNY_EVENT_STATE, st as i32, reason);
    }

    fn keyframe_request(&self) {
        let key = lenny_control { cmd: LENNY_CTL_KEYFRAME_REQUEST as u16, ..Default::default() };
        if let Some(f) = self.scb.0.on_control {
            unsafe { f(self.scb.0.user, &key) };
        }
    }

    fn current_link(&self) -> Option<Arc<dyn Transport>> {
        lock(&self.link).clone()
    }

    fn send_raw(&self, bytes: &[u8]) -> bool {
        let Some(t) = self.current_link() else { return false };
        let _g = lock(&self.send_mu);
        let ok = t.send_all(bytes);
        if !ok {
            t.shutdown(); // I/O thread sees the dead link immediately and reconnects
        }
        ok
    }

    fn send<M: Message>(&self, m: &M) -> bool {
        self.send_raw(&wire::to_message(m, self.minor.load(SeqCst)))
    }

    fn goodbye(&self, reason: i32) -> i32 {
        let bytes = wire::to_message(&wire::Goodbye { reason: reason as u16, detail: vec![] }, self.minor.load(SeqCst));
        // Best effort: if another thread is stuck mid-send, don't wait on it just to say goodbye.
        if let Some(t) = self.current_link() {
            for _ in 0..20 {
                if let Ok(_g) = self.send_mu.try_lock() {
                    t.send_all(&bytes);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        reason
    }

    fn reset_video_queue(&self) {
        let mut vq = lock(&self.vq);
        vq.q.clear();
        vq.bytes = 0;
        vq.drop_until_key = true;
    }

    fn video_writer_loop(&self) {
        loop {
            let mut o = None;
            let mut raised = 0;
            {
                let g = lock(&self.vq);
                let (mut vq, _) = self
                    .vq_cv
                    .wait_timeout_while(g, Duration::from_secs(1), |v| !v.stop && v.q.is_empty())
                    .unwrap_or_else(|e| e.into_inner());
                if vq.stop {
                    return;
                }
                // Healthy for a while: creep back up towards the negotiated bitrate.
                let now = now_us();
                if vq.current_kbps < vq.target_kbps && now - vq.last_congestion > RAISE_INTERVAL && now >= vq.next_raise
                {
                    vq.current_kbps = vq.target_kbps.min(vq.current_kbps * 11 / 10 + 1);
                    raised = vq.current_kbps;
                    vq.next_raise = now + RAISE_INTERVAL;
                }
                if let Some(x) = vq.q.pop_front() {
                    vq.bytes -= vq.bytes.min(x.msg.len());
                    o = Some(x);
                }
            }
            if raised != 0 {
                lock(&self.stats).s.bitrate_kbps = raised;
                if let Some(f) = self.scb.0.on_bitrate {
                    unsafe { f(self.scb.0.user, raised) };
                }
            }
            let Some(o) = o else { continue };
            if !self.send_raw(&o.msg) {
                self.reset_video_queue(); // link is gone; the next link starts from a keyframe
                continue;
            }
            if o.payload != 0 {
                let mut st = lock(&self.stats);
                st.s.frames += 1;
                st.s.bytes += o.payload as u64;
            }
        }
    }

    fn sender_loop(self: &Arc<Self>, host: &str, port: u16, pair_token: Vec<u8>) {
        let mut io = Io::new(self, None, pair_token);
        let mut attempt = 0;
        let mut reason = LENNY_REASON_NORMAL;
        self.set_state(LENNY_STATE_CONNECTING, 0);
        while !self.stop.load(SeqCst) {
            if let Some(t) = tcp_connect(host, port, CONNECT_TIMEOUT_MS, &self.stop, SENDER_SOCKET_BUFFER) {
                reason = io.run_link(Arc::from(t));
                if io.reached_streaming {
                    attempt = 0;
                }
                if fatal_for_sender(reason) {
                    break;
                }
            } else {
                reason = LENNY_REASON_LINK_LOST;
            }
            if self.stop.load(SeqCst) {
                break;
            }
            self.set_state(LENNY_STATE_RECONNECTING, reason);
            self.sleep_interruptible(BACKOFF_MS[attempt.min(3)]);
            attempt += 1;
        }
        let r = if self.stop.load(SeqCst) { LENNY_REASON_USER } else { reason };
        self.set_state(LENNY_STATE_CLOSED, r);
    }

    fn receiver_loop(self: &Arc<Self>, listener: TcpListener) {
        let mut reason = LENNY_REASON_NORMAL; // why the last phone left, for the UI's error state
        {
            let mut io = Io::new(self, Some(&listener), vec![]);
            while !self.stop.load(SeqCst) {
                self.set_state(LENNY_STATE_CONNECTING, reason);
                if let Some(t) = listener.accept(200) {
                    reason = io.run_link(Arc::from(t));
                }
            }
        }
        drop(listener);
        self.bound_port.store(0, SeqCst);
        self.set_state(LENNY_STATE_CLOSED, LENNY_REASON_USER);
    }
}

/// I/O-thread-only link state.
struct Io<'a> {
    s: &'a Inner,
    listener: Option<&'a TcpListener>,
    phase: Phase,
    phase_deadline: i64,
    last_rx: i64,
    next_ping: i64,
    peer: wire::Hello,
    peer_caps: wire::Caps,
    pair_token: Vec<u8>,     // sender: QR token still to redeem
    reached_streaming: bool, // this link got to STREAMING (resets sender backoff)
}

impl<'a> Io<'a> {
    fn new(s: &'a Inner, listener: Option<&'a TcpListener>, pair_token: Vec<u8>) -> Self {
        Io {
            s,
            listener,
            phase: Phase::HelloWait,
            phase_deadline: 0,
            last_rx: 0,
            next_ping: 0,
            peer: wire::Hello::default(),
            peer_caps: wire::Caps::default(),
            pair_token,
            reached_streaming: false,
        }
    }

    fn enter(&mut self, p: Phase, now: i64, timeout_us: i64) {
        self.phase = p;
        self.phase_deadline = now + timeout_us;
    }

    /// Runs one connection until it ends. Returns a LENNY_REASON_*.
    fn run_link(&mut self, t: Arc<dyn Transport>) -> i32 {
        let s = self.s;
        *lock(&s.link) = Some(t.clone());
        let start = now_us();
        self.enter(Phase::HelloWait, start, HELLO_TIMEOUT);
        self.last_rx = start;
        self.peer = wire::Hello::default();
        self.peer_caps = wire::Caps::default();
        s.minor.store(wire::VERSION_MINOR, SeqCst);
        self.reached_streaming = false;
        s.approval.store(-1, SeqCst);
        lock(&s.stats).clock = ClockSync::default();
        s.set_state(LENNY_STATE_HANDSHAKE, 0);

        let mut result = CONTINUE;
        if s.role == Role::Sender && !s.send(&s.hello) {
            result = LENNY_REASON_LINK_LOST;
        }

        let mut reader = wire::MessageReader::default();
        let mut buf = vec![0u8; 64 * 1024];
        while result == CONTINUE {
            if s.stop.load(SeqCst) {
                result = s.goodbye(s.bye_reason.load(SeqCst));
                break;
            }
            let n = t.recv(&mut buf, 50);
            let now = now_us();
            if n < 0 {
                result = LENNY_REASON_LINK_LOST;
                break;
            }
            if n > 0 {
                self.last_rx = now;
                reader.feed(&buf[..n as usize]);
                loop {
                    match reader.next() {
                        wire::Status::NeedMore => break,
                        wire::Status::Message(h, p) => {
                            result = self.dispatch(&h, p, now);
                            if result != CONTINUE {
                                break;
                            }
                        }
                        // TCP has no resync point after garbage, so framing errors end the link (§3 rules 1, 3).
                        wire::Status::BadMagic | wire::Status::TooLarge => {
                            result = s.goodbye(LENNY_REASON_PROTOCOL_ERROR);
                            break;
                        }
                    }
                }
            }
            if result == CONTINUE {
                result = self.tick(now);
            }
        }

        s.streaming.store(false, SeqCst);
        if s.role == Role::Sender {
            s.reset_video_queue();
        }
        *lock(&s.link) = None;
        t.shutdown(); // also wakes any sender thread blocked in send_all
        result
    }

    fn tick(&mut self, now: i64) -> i32 {
        let s = self.s;
        if now - self.last_rx > SILENCE_LIMIT {
            return LENNY_REASON_LINK_LOST;
        }
        if self.phase == Phase::ApprovalWait {
            match s.approval.load(SeqCst) {
                0 => return s.goodbye(LENNY_REASON_PAIR_DENIED),
                1 => {
                    s.trust(&self.peer.device_id);
                    self.on_caps_complete(now);
                }
                _ => {}
            }
        }
        if self.phase != Phase::Streaming && now > self.phase_deadline {
            return s.goodbye(if self.phase == Phase::ApprovalWait {
                LENNY_REASON_PAIR_DENIED
            } else {
                LENNY_REASON_TIMEOUT
            });
        }
        if self.phase != Phase::HelloWait && now >= self.next_ping {
            self.next_ping = now + PING_INTERVAL;
            let seq = s.ping_seq.fetch_add(1, SeqCst).wrapping_add(1);
            if !s.send(&wire::Ping { seq, t1: now }) {
                return LENNY_REASON_LINK_LOST;
            }
        }
        if let Some(l) = self.listener {
            // One phone at a time (§7). Tell extra callers right away instead of leaving them hanging.
            if let Some(extra) = l.accept(0) {
                // The close may RST before the phone reads this; it then sees LINK_LOST instead. Both mean "retry".
                let bye = wire::to_message(
                    &wire::Goodbye { reason: LENNY_REASON_BUSY as u16, detail: b"another phone is streaming".to_vec() },
                    wire::VERSION_MINOR,
                );
                extra.send_all(&bye);
            }
        }
        CONTINUE
    }

    fn send_caps(&mut self, now: i64) {
        self.s.send(&self.s.caps);
        self.enter(Phase::CapsWait, now, SENDER_CAPS_TIMEOUT);
    }

    fn on_caps_complete(&mut self, now: i64) {
        let s = self.s;
        let pref = lock(&s.info).preferred;
        s.send(&wire::CapsSelect(choose_settings(&self.peer_caps, pref)));
        self.enter(Phase::StreamStartWait, now, CAPS_TIMEOUT);
        s.set_state(LENNY_STATE_HANDSHAKE, 0);
    }

    fn on_hello(&mut self, payload: &[u8], now: i64) -> i32 {
        let s = self.s;
        if self.phase != Phase::HelloWait {
            return CONTINUE; // duplicate HELLO: ignore
        }
        let Some(h) = wire::Hello::decode(payload) else { return s.goodbye(LENNY_REASON_PROTOCOL_ERROR) };
        if h.proto_major != wire::VERSION_MAJOR {
            return s.goodbye(LENNY_REASON_VERSION);
        }
        if h.role == s.hello.role {
            return s.goodbye(LENNY_REASON_ROLE);
        }
        {
            let mut info = lock(&s.info);
            let mut p = lenny_peer_info { device_id: h.device_id, platform: h.platform, ..Default::default() };
            copy_c(&mut p.name, &h.device_name);
            info.peer_info = p;
            info.has_peer = true;
            info.has_caps = false;
        }
        s.minor.store(wire::VERSION_MINOR.min(h.proto_minor), SeqCst);
        self.peer = h;
        self.next_ping = now + PING_INTERVAL;
        if s.role == Role::Receiver {
            if !s.send(&s.hello) {
                return LENNY_REASON_LINK_LOST;
            }
            self.enter(Phase::PairOrCaps, now, CAPS_TIMEOUT);
        } else if !self.pair_token.is_empty() {
            let mut r = wire::PairRequest::default();
            r.token.copy_from_slice(&self.pair_token);
            s.send(&r);
            self.enter(Phase::PairWait, now, PAIR_TIMEOUT);
        } else {
            self.send_caps(now);
        }
        CONTINUE
    }

    fn dispatch(&mut self, h: &wire::Header, p: &[u8], now: i64) -> i32 {
        let s = self.s;
        if h.ver_major != wire::VERSION_MAJOR {
            return s.goodbye(LENNY_REASON_VERSION);
        }
        match h.typ {
            msg::HELLO => return self.on_hello(p, now),
            msg::GOODBYE => return wire::Goodbye::decode(p).map_or(LENNY_REASON_PROTOCOL_ERROR, |g| g.reason as i32),
            _ => {}
        }
        if self.phase == Phase::HelloWait {
            return CONTINUE; // nothing else is valid before HELLO
        }
        match h.typ {
            msg::PING => {
                if let Some(ping) = wire::Ping::decode(p) {
                    if !s.send(&wire::Pong { seq: ping.seq, t1: ping.t1, t2: now, t3: now_us() }) {
                        return LENNY_REASON_LINK_LOST;
                    }
                }
                return CONTINUE;
            }
            msg::PONG => {
                if let Some(pong) = wire::Pong::decode(p) {
                    lock(&s.stats).clock.add(pong.t1, pong.t2, pong.t3, now);
                }
                return CONTINUE;
            }
            _ => {}
        }
        if s.role == Role::Sender {
            self.dispatch_sender(h, p, now)
        } else {
            self.dispatch_receiver(h, p, now)
        }
    }

    fn dispatch_sender(&mut self, h: &wire::Header, p: &[u8], now: i64) -> i32 {
        let s = self.s;
        let cb = s.scb.0;
        match h.typ {
            msg::PAIR_RESULT => {
                let Some(r) = wire::PairResult::decode(p).filter(|_| self.phase == Phase::PairWait) else {
                    return CONTINUE;
                };
                if r.result != LENNY_PAIR_OK {
                    return LENNY_REASON_PAIR_DENIED;
                }
                self.pair_token.clear(); // single-use; the receiver now trusts our device_id
                self.send_caps(now);
            }
            msg::CAPS_SELECT => {
                if self.phase != Phase::CapsWait && self.phase != Phase::Streaming {
                    return CONTINUE;
                }
                let Some(sel) = wire::CapsSelect::decode(p) else { return CONTINUE };
                let mut eff = sel.0;
                if let Some(f) = cb.on_stream_config {
                    unsafe { f(cb.user, &sel.0, &mut eff) };
                }
                {
                    let mut vq = lock(&s.vq);
                    vq.target_kbps = if eff.bitrate_kbps != 0 { eff.bitrate_kbps } else { DEFAULT_BITRATE_KBPS };
                    vq.current_kbps = vq.target_kbps;
                    vq.last_congestion = 0;
                    vq.next_raise = 0;
                }
                lock(&s.stats).s.bitrate_kbps = eff.bitrate_kbps;
                if !s.send(&wire::StreamStart(eff)) {
                    return LENNY_REASON_LINK_LOST;
                }
                {
                    let mut info = lock(&s.info);
                    info.settings = eff;
                    info.has_settings = true;
                }
                s.emit(LENNY_EVENT_STREAM_START, 0, 0);
                if self.phase != Phase::Streaming {
                    self.enter(Phase::Streaming, now, 0);
                    self.reached_streaming = true;
                    if s.ever_streamed.swap(true, SeqCst) {
                        lock(&s.stats).s.reconnects += 1;
                    }
                    s.streaming.store(true, SeqCst);
                    s.set_state(LENNY_STATE_STREAMING, 0);
                }
                // New stream or new settings: the receiver needs a keyframe (plus config) to start decoding.
                s.keyframe_request();
            }
            msg::CONTROL => {
                if self.phase != Phase::Streaming {
                    return CONTINUE;
                }
                let Some(c) = wire::Control::decode(p) else {
                    lock(&s.stats).s.bad_messages += 1;
                    return CONTINUE;
                };
                let r = match cb.on_control {
                    Some(f) => unsafe { f(cb.user, &c.0) },
                    None => LENNY_ACK_UNSUPPORTED,
                };
                s.send(&wire::ControlAck { req_id: c.0.req_id, result: r as u8 });
            }
            _ => {} // unknown or not for us: ignore (§3 rule 4)
        }
        CONTINUE
    }

    fn dispatch_receiver(&mut self, h: &wire::Header, p: &[u8], now: i64) -> i32 {
        let s = self.s;
        let cb = s.rcb.0;
        let bad = || lock(&s.stats).s.bad_messages += 1;
        match h.typ {
            msg::PAIR_REQUEST => {
                if self.phase == Phase::PairOrCaps {
                    if let Some(r) = wire::PairRequest::decode(p) {
                        let result = s.tokens.redeem(&r.token, now);
                        s.send(&wire::PairResult { result });
                        if result != LENNY_PAIR_OK {
                            return s.goodbye(LENNY_REASON_PAIR_DENIED);
                        }
                        s.trust(&self.peer.device_id);
                        self.enter(Phase::PairOrCaps, now, CAPS_TIMEOUT);
                    }
                }
            }
            msg::CAPS => {
                if self.phase == Phase::PairOrCaps {
                    let Some(caps) = wire::Caps::decode(p) else { return s.goodbye(LENNY_REASON_PROTOCOL_ERROR) };
                    self.peer_caps = caps;
                    {
                        let mut info = lock(&s.info);
                        let pc = &self.peer_caps;
                        let pi = &mut info.peer_info;
                        pi.controls = pc.controls as u32;
                        pi.lens_count = 0;
                        for l in pc.lenses.iter().take(LENNY_MAX_PEER_LENSES) {
                            let i = pi.lens_count as usize;
                            pi.lens_count += 1;
                            pi.lens_ids[i] = l.id;
                            pi.lens_facing[i] = l.facing;
                            copy_c(&mut pi.lens_labels[i], &l.label);
                        }
                        if pc.has_exposure_range {
                            pi.exposure_min = pc.exposure_min;
                            pi.exposure_max = pc.exposure_max;
                            pi.exposure_step_milli = pc.exposure_step_milli;
                        }
                        let n = pc.modes.len().min(LENNY_MAX_PEER_MODES);
                        pi.mode_count = n as u8;
                        pi.modes[..n].copy_from_slice(&pc.modes[..n]);
                        info.peer_caps_copy = pc.clone();
                        info.has_caps = true;
                    }
                    if s.trusted(&self.peer.device_id) {
                        self.on_caps_complete(now);
                    } else {
                        s.approval.store(-1, SeqCst);
                        self.enter(Phase::ApprovalWait, now, APPROVAL_TIMEOUT);
                        s.set_state(LENNY_STATE_AWAITING_APPROVAL, 0);
                        if let Some(f) = cb.on_approval_needed {
                            let name = c_string(&self.peer.device_name);
                            unsafe { f(cb.user, self.peer.device_id.as_ptr(), name.as_ptr()) };
                        }
                        s.emit(LENNY_EVENT_APPROVAL_NEEDED, 0, 0);
                    }
                }
            }
            msg::STREAM_START if self.phase == Phase::StreamStartWait || self.phase == Phase::Streaming => {
                if let Some(st) = wire::StreamStart::decode(p) {
                    if self.phase != Phase::Streaming {
                        self.enter(Phase::Streaming, now, 0);
                        s.streaming.store(true, SeqCst);
                        s.set_state(LENNY_STATE_STREAMING, 0);
                    }
                    {
                        let mut info = lock(&s.info);
                        info.settings = st.0;
                        info.has_settings = true;
                    }
                    if let Some(f) = cb.on_stream_start {
                        unsafe { f(cb.user, &st.0) };
                    }
                    s.emit(LENNY_EVENT_STREAM_START, 0, 0);
                }
            }
            _ => {}
        }
        if self.phase != Phase::Streaming {
            return CONTINUE;
        }

        match h.typ {
            msg::VIDEO_CONFIG => match wire::VideoConfig::decode(p) {
                None => bad(),
                Some(vc) => {
                    if let Some(f) = cb.on_video_config {
                        unsafe { f(cb.user, vc.config.as_ptr(), vc.config.len()) };
                    }
                }
            },
            msg::VIDEO_FRAME => match wire::decode_video_meta(p) {
                None => bad(),
                Some((m, data)) => {
                    let mut f = lenny_video_frame {
                        frame_seq: m.frame_seq,
                        pts_us: m.pts_us,
                        local_pts_us: 0,
                        orientation: m.orientation,
                        flags: m.flags,
                        data: data.as_ptr(),
                        size: data.len(),
                    };
                    {
                        let mut st = lock(&s.stats);
                        if st.clock.valid() {
                            f.local_pts_us = m.pts_us - st.clock.offset_us();
                            let age = now - f.local_pts_us; // capture -> received on this machine
                            st.s.latency_us = if st.s.latency_us < 0 { age } else { (st.s.latency_us * 7 + age) / 8 };
                        }
                        st.s.frames += 1;
                        st.s.bytes += data.len() as u64;
                    }
                    if let Some(cbf) = cb.on_video_frame {
                        unsafe { cbf(cb.user, &f) };
                    }
                }
            },
            msg::CONTROL_STATE => match wire::ControlState::decode(p) {
                None => bad(),
                Some(cs) => {
                    {
                        let mut info = lock(&s.info);
                        info.control_state = cs.0;
                        info.has_control_state = true;
                    }
                    if let Some(f) = cb.on_control_state {
                        unsafe { f(cb.user, &cs.0) };
                    }
                    s.emit(LENNY_EVENT_CONTROL_STATE, 0, 0);
                }
            },
            msg::CONTROL_ACK => match wire::ControlAck::decode(p) {
                None => bad(),
                Some(a) => {
                    if let Some(f) = cb.on_control_ack {
                        unsafe { f(cb.user, a.req_id, a.result) };
                    }
                    s.emit(LENNY_EVENT_CONTROL_ACK, a.req_id as i32, a.result as i32);
                }
            },
            msg::STREAM_STATUS => match wire::StreamStatus::decode(p) {
                None => bad(),
                Some(st) => {
                    if let Some(f) = cb.on_stream_status {
                        let r = c_string(&st.reason);
                        unsafe { f(cb.user, st.state, r.as_ptr()) };
                    }
                    s.emit(LENNY_EVENT_STREAM_STATUS, st.state as i32, 0);
                }
            },
            _ => {}
        }
        CONTINUE
    }
}

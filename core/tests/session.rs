//! End-to-end sender -> receiver over TCP on 127.0.0.1, through the public C ABI only
//! (port of core/tests/test_session.cpp).

use std::os::raw::{c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU16, AtomicU32, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lenny_core::*;

fn wait_for(pred: impl Fn() -> bool) -> bool {
    wait_for_ms(pred, 5000)
}

fn wait_for_ms(pred: impl Fn() -> bool, ms: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    while !pred() {
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

#[derive(Default)]
struct Recv {
    approvals: AtomicI32,
    configs: AtomicI32,
    frames: AtomicI32,
    acks: AtomicI32,
    starts: AtomicI32,
    last_ack_result: AtomicI32,
    last_ack_id: AtomicU32,
    torch: AtomicI32,
    saw_config_before_key: AtomicI32,
    last_local_pts: AtomicI64,
    last_frame: Mutex<Vec<u8>>,
    started: Mutex<lenny_stream_settings>,
}

#[derive(Default)]
struct Send {
    keyframe_requests: AtomicI32,
    focus_calls: AtomicI32,
    configs: AtomicI32,
    last_reason: AtomicI32,
    focus_x: AtomicU16,
}

fn ident(id_byte: u8, name: &'static std::ffi::CStr, platform: u8) -> lenny_identity {
    lenny_identity { device_id: [id_byte; 16], device_name: name.as_ptr(), app_version: c"test".as_ptr(), platform }
}

fn settings(w: u16, h: u16, kbps: u32) -> lenny_stream_settings {
    lenny_stream_settings {
        codec: LENNY_CODEC_H264,
        mode: lenny_mode { width: w, height: h, fps_num: 30, fps_den: 1 },
        bitrate_kbps: kbps,
        has_lens: 0,
        lens_id: 0,
    }
}

unsafe fn r<'a>(u: *mut c_void) -> &'a Recv {
    &*(u as *const Recv)
}
unsafe fn s<'a>(u: *mut c_void) -> &'a Send {
    &*(u as *const Send)
}

unsafe extern "C" fn rx_approval(u: *mut c_void, _: *const u8, name: *const c_char) {
    if std::ffi::CStr::from_ptr(name).to_bytes() == b"Pixel" {
        r(u).approvals.fetch_add(1, SeqCst);
    }
}
unsafe extern "C" fn rx_start(u: *mut c_void, st: *const lenny_stream_settings) {
    *r(u).started.lock().unwrap() = *st;
    r(u).starts.fetch_add(1, SeqCst);
}
unsafe extern "C" fn rx_config(u: *mut c_void, _: *const u8, _: usize) {
    r(u).configs.fetch_add(1, SeqCst);
}
unsafe extern "C" fn rx_frame(u: *mut c_void, f: *const lenny_video_frame) {
    let rv = r(u);
    let f = &*f;
    if f.flags & LENNY_FRAME_KEYFRAME != 0 && rv.configs.load(SeqCst) > 0 {
        rv.saw_config_before_key.store(1, SeqCst);
    }
    *rv.last_frame.lock().unwrap() = std::slice::from_raw_parts(f.data, f.size).to_vec();
    rv.last_local_pts.store(f.local_pts_us, SeqCst);
    rv.frames.fetch_add(1, SeqCst);
}
unsafe extern "C" fn rx_ack(u: *mut c_void, id: u32, res: u8) {
    let rv = r(u);
    rv.last_ack_id.store(id, SeqCst);
    rv.last_ack_result.store(res as i32, SeqCst);
    rv.acks.fetch_add(1, SeqCst);
}
unsafe extern "C" fn rx_control_state(u: *mut c_void, st: *const lenny_control_state) {
    r(u).torch.store((*st).torch as i32, SeqCst);
}

fn make_receiver(rv: &Recv, port: u16) -> *mut lenny_session {
    let cfg = lenny_receiver_config {
        identity: ident(0xDD, c"Desk", LENNY_PLATFORM_WINDOWS),
        port,
        preferred: settings(1920, 1080, 20000),
    };
    let mut cb: lenny_receiver_callbacks = unsafe { std::mem::zeroed() };
    cb.user = rv as *const Recv as *mut c_void;
    cb.on_approval_needed = Some(rx_approval);
    cb.on_stream_start = Some(rx_start);
    cb.on_video_config = Some(rx_config);
    cb.on_video_frame = Some(rx_frame);
    cb.on_control_ack = Some(rx_ack);
    cb.on_control_state = Some(rx_control_state);
    unsafe {
        let s = lenny_receiver_create(&cfg, &cb);
        assert!(!s.is_null());
        assert_eq!(lenny_receiver_start(s), LENNY_OK);
        assert_ne!(lenny_receiver_port(s), 0);
        s
    }
}

const MODES: [lenny_mode; 3] = [
    lenny_mode { width: 1280, height: 720, fps_num: 30, fps_den: 1 },
    lenny_mode { width: 1920, height: 1080, fps_num: 30, fps_den: 1 },
    lenny_mode { width: 3840, height: 2160, fps_num: 30, fps_den: 1 },
];

/// Per-lens caps (ABI 1.3): the wide lens does 720p and 1080p, the front one only 720p.
struct LensCaps([lenny_lens_caps; 2]);
unsafe impl Sync for LensCaps {}
static LENS_CAPS: LensCaps = LensCaps([
    lenny_lens_caps { modes: MODES.as_ptr(), mode_count: 2, zoom_min: 100, zoom_max: 800, zoom_base: 100 },
    lenny_lens_caps { modes: MODES.as_ptr(), mode_count: 1, zoom_min: 0, zoom_max: 0, zoom_base: 0 },
]);
impl LensCaps {
    fn as_ptr(&self) -> *const lenny_lens_caps {
        self.0.as_ptr()
    }
}

unsafe extern "C" fn tx_state(u: *mut c_void, st: i32, reason: i32) {
    if st == LENNY_STATE_CLOSED as i32 {
        s(u).last_reason.store(reason, SeqCst);
    }
}
unsafe extern "C" fn tx_stream_config(
    u: *mut c_void,
    _: *const lenny_stream_settings,
    eff: *mut lenny_stream_settings,
) {
    s(u).configs.fetch_add(1, SeqCst);
    (*eff).bitrate_kbps = 10000; // platform clamps
}
unsafe extern "C" fn tx_control(u: *mut c_void, c: *const lenny_control) -> i32 {
    let sd = s(u);
    let c = &*c;
    if c.cmd == LENNY_CTL_KEYFRAME_REQUEST as u16 {
        sd.keyframe_requests.fetch_add(1, SeqCst);
        return LENNY_ACK_OK;
    }
    if c.cmd == LENNY_CTL_FOCUS_AT as u16 || c.cmd == LENNY_CTL_PAN as u16 {
        sd.focus_x.store(c.x, SeqCst);
        sd.focus_calls.fetch_add(1, SeqCst);
        return LENNY_ACK_OK;
    }
    LENNY_ACK_UNSUPPORTED
}

fn make_sender(sd: &Send, id_byte: u8) -> *mut lenny_session {
    sd.last_reason.store(-1, SeqCst);
    let lenses = [
        lenny_lens { lens_id: 0, facing: 0, label: c"Wide".as_ptr() },
        lenny_lens { lens_id: 1, facing: 1, label: c"Front".as_ptr() },
    ];
    let cfg = lenny_sender_config {
        identity: ident(id_byte, c"Pixel", LENNY_PLATFORM_ANDROID),
        modes: MODES.as_ptr(),
        mode_count: 3,
        max_bitrate_kbps: 12000,
        controls: LENNY_CAP_FOCUS | LENNY_CAP_TORCH | LENNY_CAP_EXPOSURE_COMP,
        lenses: lenses.as_ptr(),
        lens_count: 2,
        exposure_comp_min: -2000,
        exposure_comp_max: 2000,
        exposure_comp_step_milli: 333,
        lens_caps: LENS_CAPS.as_ptr(),
    };
    let cb = lenny_sender_callbacks {
        user: sd as *const Send as *mut c_void,
        on_state: Some(tx_state),
        on_stream_config: Some(tx_stream_config),
        on_control: Some(tx_control),
        on_bitrate: None,
    };
    let s = unsafe { lenny_sender_create(&cfg, &cb) };
    assert!(!s.is_null());
    s
}

fn state(s: *mut lenny_session) -> i32 {
    unsafe { lenny_session_state(s) }
}
fn streaming(s: *mut lenny_session) -> bool {
    state(s) == LENNY_STATE_STREAMING as i32
}
fn stats(s: *mut lenny_session) -> lenny_stats {
    let mut st = lenny_stats::default();
    unsafe { lenny_session_get_stats(s, &mut st) };
    st
}
fn connect(tx: *mut lenny_session, port: u16, token: *const u8) -> i32 {
    unsafe { lenny_sender_connect(tx, c"127.0.0.1".as_ptr(), port, token) }
}
fn trust(rx: *mut lenny_session, b: u8) {
    unsafe { lenny_receiver_trust_device(rx, [b; 16].as_ptr()) };
}
fn destroy(s: *mut lenny_session) {
    unsafe { lenny_session_destroy(s) }
}
fn frame(tx: *mut lenny_session, data: &[u8], pts: i64, orientation: u8, flags: u8) -> i32 {
    unsafe { lenny_sender_send_video_frame(tx, data.as_ptr(), data.len(), pts, orientation, flags) }
}

#[test]
fn abi_version() {
    assert_eq!(lenny_abi_version(), (1 << 16) | 3);
}

#[test]
fn full_session_with_approval_video_and_controls() {
    let rv = Recv::default();
    let sd = Send::default();
    let rx = make_receiver(&rv, 0);
    let tx = make_sender(&sd, 0x11);
    assert_eq!(connect(tx, unsafe { lenny_receiver_port(rx) }, null()), LENNY_OK);

    // Unknown phone: receiver asks the user.
    assert!(wait_for(|| rv.approvals.load(SeqCst) == 1));
    assert_eq!(state(rx), LENNY_STATE_AWAITING_APPROVAL as i32);
    assert_eq!(frame(tx, b"x", 0, 0, 0), LENNY_E_STATE); // not yet
    assert_eq!(unsafe { lenny_receiver_approve(rx, 1) }, LENNY_OK);
    assert!(wait_for(|| streaming(rx) && streaming(tx)));

    // Negotiation: receiver preferred 1080p30 @ 20 Mbps, sender caps max 12 Mbps, platform clamped to 10.
    assert!(wait_for(|| rv.starts.load(SeqCst) == 1));
    {
        let st = rv.started.lock().unwrap();
        assert!(st.mode.width == 1920 && st.mode.height == 1080 && st.bitrate_kbps == 10000);
    }
    assert_eq!(sd.configs.load(SeqCst), 1);
    assert!(wait_for(|| sd.keyframe_requests.load(SeqCst) == 1)); // fresh stream -> keyframe

    // Config is cached and resent ahead of each keyframe.
    let sps = [0, 0, 0, 1, 0x67, 0x42];
    let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84];
    let p = [0, 0, 0, 1, 0x41, 0x9A];
    assert_eq!(unsafe { lenny_sender_send_video_config(tx, sps.as_ptr(), sps.len()) }, LENNY_OK);
    assert!(wait_for(|| rv.configs.load(SeqCst) == 1));
    assert_eq!(frame(tx, &idr, lenny_now_us(), 1, LENNY_FRAME_KEYFRAME), LENNY_OK);
    assert_eq!(frame(tx, &p, lenny_now_us(), 1, 0), LENNY_OK);
    assert!(wait_for(|| rv.frames.load(SeqCst) == 2));
    assert!(rv.configs.load(SeqCst) == 2 && rv.saw_config_before_key.load(SeqCst) == 1);
    assert_eq!(*rv.last_frame.lock().unwrap(), p);

    // Remote tap-to-focus -> phone callback -> ACK back.
    let mut focus = lenny_control { cmd: LENNY_CTL_FOCUS_AT as u16, x: 30000, y: 20000, ..Default::default() };
    assert_eq!(unsafe { lenny_receiver_send_control(rx, &mut focus) }, LENNY_OK);
    assert_ne!(focus.req_id, 0);
    assert!(wait_for(|| rv.acks.load(SeqCst) == 1));
    assert!(sd.focus_calls.load(SeqCst) == 1 && sd.focus_x.load(SeqCst) == 30000);
    assert!(rv.last_ack_id.load(SeqCst) == focus.req_id && rv.last_ack_result.load(SeqCst) == LENNY_ACK_OK);
    let mut zoom = lenny_control { cmd: LENNY_CTL_ZOOM as u16, value: 200, ..Default::default() };
    assert_eq!(unsafe { lenny_receiver_send_control(rx, &mut zoom) }, LENNY_OK);
    assert!(wait_for(|| rv.acks.load(SeqCst) == 2));
    assert_eq!(rv.last_ack_result.load(SeqCst), LENNY_ACK_UNSUPPORTED);

    let cs = lenny_control_state { torch: 1, zoom: 100, ..Default::default() };
    assert_eq!(unsafe { lenny_sender_send_control_state(tx, &cs) }, LENNY_OK);
    assert!(wait_for(|| rv.torch.load(SeqCst) == 1));

    // Keepalive runs every second: after ~1.2 s both sides have an RTT and the receiver maps pts to local time.
    assert!(wait_for(|| stats(rx).rtt_us >= 0));
    assert_eq!(frame(tx, &p, lenny_now_us(), 0, 0), LENNY_OK);
    assert!(wait_for(|| rv.frames.load(SeqCst) == 3));
    // Same machine, same clock: mapped pts is within a few ms of the original.
    let lp = rv.last_local_pts.load(SeqCst);
    assert!(lp != 0 && (lp - lenny_now_us()).abs() < 1_000_000);
    assert_eq!(stats(tx).frames, 3);

    // Phone user disconnects: sender closes for good, receiver goes back to listening.
    assert_eq!(unsafe { lenny_session_disconnect(tx) }, LENNY_OK);
    assert!(wait_for(|| state(tx) == LENNY_STATE_CLOSED as i32));
    assert_eq!(sd.last_reason.load(SeqCst), LENNY_REASON_USER);
    assert!(wait_for(|| state(rx) == LENNY_STATE_CONNECTING as i32));

    destroy(tx);
    destroy(rx);
}

#[derive(Default)]
struct Ev {
    approval: AtomicI32,
    start: AtomicI32,
    streaming: AtomicI32,
    ack: AtomicI32,
}

unsafe extern "C" fn on_event(u: *mut c_void, e: i32, a: i32, _: i32) {
    let ev = &*(u as *const Ev);
    if e == LENNY_EVENT_APPROVAL_NEEDED as i32 {
        ev.approval.fetch_add(1, SeqCst);
    }
    if e == LENNY_EVENT_STREAM_START as i32 {
        ev.start.fetch_add(1, SeqCst);
    }
    if e == LENNY_EVENT_STATE as i32 && a == LENNY_STATE_STREAMING as i32 {
        ev.streaming.fetch_add(1, SeqCst);
    }
    if e == LENNY_EVENT_CONTROL_ACK as i32 {
        ev.ack.fetch_add(1, SeqCst);
    }
}

fn c_str(b: &[c_char]) -> &[u8] {
    let b: &[u8] = unsafe { std::slice::from_raw_parts(b.as_ptr() as *const u8, b.len()) };
    &b[..b.iter().position(|&c| c == 0).unwrap()]
}

#[test]
fn ui_events_carry_ints_and_getters_have_details() {
    let ev = Ev::default();
    let rv = Recv::default();
    let sd = Send::default();
    let rx = make_receiver(&rv, 0);
    let tx = make_sender(&sd, 0x11);
    let mut peer = lenny_peer_info::default();
    unsafe {
        assert_eq!(lenny_session_peer(rx, &mut peer), LENNY_E_STATE); // nobody yet
        lenny_session_set_event_listener(rx, Some(on_event), &ev as *const Ev as *mut c_void);
    }
    connect(tx, unsafe { lenny_receiver_port(rx) }, null());
    assert!(wait_for(|| ev.approval.load(SeqCst) == 1));
    assert_eq!(unsafe { lenny_session_peer(rx, &mut peer) }, LENNY_OK);
    assert!(c_str(&peer.name) == b"Pixel" && peer.platform == LENNY_PLATFORM_ANDROID && peer.device_id[0] == 0x11);
    // The phone's camera capabilities, for the desktop's remote controls.
    assert!(peer.controls & LENNY_CAP_TORCH != 0 && peer.controls & LENNY_CAP_FOCUS != 0);
    assert!(peer.lens_count == 2 && c_str(&peer.lens_labels[1]) == b"Front" && peer.lens_facing[1] == 1);
    assert!(peer.exposure_min == -2000 && peer.exposure_max == 2000 && peer.exposure_step_milli == 333);
    unsafe { lenny_receiver_approve(rx, 1) };
    assert!(wait_for(|| ev.start.load(SeqCst) == 1 && ev.streaming.load(SeqCst) == 1));
    let mut st = lenny_stream_settings::default();
    assert!(unsafe { lenny_session_stream_settings(rx, &mut st) } == LENNY_OK && st.mode.width == 1920);
    let mut tx_st = lenny_stream_settings::default();
    assert!(unsafe { lenny_session_stream_settings(tx, &mut tx_st) } == LENNY_OK && tx_st.bitrate_kbps == 10000);
    let mut key = lenny_control { cmd: LENNY_CTL_KEYFRAME_REQUEST as u16, ..Default::default() };
    unsafe { lenny_receiver_send_control(rx, &mut key) };
    assert!(wait_for(|| ev.ack.load(SeqCst) == 1));
    destroy(tx);
    destroy(rx);
}

#[derive(Default)]
struct Slow {
    frames: AtomicI32,
}
unsafe extern "C" fn slow_frame(u: *mut c_void, _: *const lenny_video_frame) {
    (*(u as *const Slow)).frames.fetch_add(1, SeqCst);
    std::thread::sleep(Duration::from_millis(50));
}
#[derive(Default)]
struct Tx {
    keyframes: AtomicI32,
    kbps: AtomicU32,
}
unsafe extern "C" fn tx_key(u: *mut c_void, c: *const lenny_control) -> i32 {
    if (*c).cmd == LENNY_CTL_KEYFRAME_REQUEST as u16 {
        (*(u as *const Tx)).keyframes.fetch_add(1, SeqCst);
    }
    LENNY_ACK_OK
}
unsafe extern "C" fn tx_bitrate(u: *mut c_void, kbps: u32) {
    (*(u as *const Tx)).kbps.store(kbps, SeqCst);
}

#[test]
fn slow_network_drops_to_keyframe_and_lowers_bitrate_without_blocking() {
    // A receiver that falls behind (it sleeps in the frame callback, so its socket stops draining) must not make the
    // phone's encoder thread block, and the phone must shed load: drop backlog, ask for a keyframe, lower bitrate.
    let slow = Slow::default();
    let rcfg = lenny_receiver_config {
        identity: ident(0xDD, c"Desk", LENNY_PLATFORM_WINDOWS),
        port: 0,
        preferred: settings(1280, 720, 2000),
    };
    let mut rcb: lenny_receiver_callbacks = unsafe { std::mem::zeroed() };
    rcb.user = &slow as *const Slow as *mut c_void;
    rcb.on_video_frame = Some(slow_frame);
    let rx = unsafe { lenny_receiver_create(&rcfg, &rcb) };
    unsafe { lenny_receiver_start(rx) };
    trust(rx, 0x61);

    let txs = Tx::default();
    let scfg = lenny_sender_config {
        identity: ident(0x61, c"Pixel", LENNY_PLATFORM_ANDROID),
        modes: MODES.as_ptr(),
        mode_count: 3,
        max_bitrate_kbps: 2000,
        controls: 0,
        lenses: null(),
        lens_count: 0,
        exposure_comp_min: 0,
        exposure_comp_max: 0,
        exposure_comp_step_milli: 0,
        lens_caps: null(),
    };
    let scb = lenny_sender_callbacks {
        user: &txs as *const Tx as *mut c_void,
        on_state: None,
        on_stream_config: None,
        on_control: Some(tx_key),
        on_bitrate: Some(tx_bitrate),
    };
    let tx = unsafe { lenny_sender_create(&scfg, &scb) };
    connect(tx, unsafe { lenny_receiver_port(rx) }, null());
    assert!(wait_for(|| streaming(tx) && streaming(rx)));

    // 2 Mbps target, but we push 60 KB frames (~14 Mbps at 30 fps) as fast as we can: far more than the slow
    // receiver takes. Keyframe every 10th frame. Timestamps advance like a real 30 fps camera.
    let data = vec![0xABu8; 60 * 1024];
    let mut worst = Duration::ZERO;
    let base = lenny_now_us();
    for i in 0..300 {
        let t0 = Instant::now();
        assert_eq!(
            frame(tx, &data, base + i * 33_333, 0, if i % 10 == 0 { LENNY_FRAME_KEYFRAME } else { 0 }),
            LENNY_OK
        );
        worst = worst.max(t0.elapsed());
    }
    assert!(worst < Duration::from_millis(20), "encoder thread waited {worst:?}"); // never waited on the network
    let st = stats(tx);
    assert!(st.dropped_frames > 0);
    assert!(txs.keyframes.load(SeqCst) > 1); // one at stream start, more after drops
    let kbps = txs.kbps.load(SeqCst);
    assert!(kbps > 0 && kbps < 2000); // bitrate stepped down
    assert_eq!(st.bitrate_kbps, kbps);
    assert!(wait_for(|| slow.frames.load(SeqCst) > 0)); // some got through...
    assert!(slow.frames.load(SeqCst) < 300); // ...but not the whole backlog
    destroy(tx);
    destroy(rx);
}

#[test]
fn update_stream_reaches_receiver_and_latency_is_measured() {
    let rv = Recv::default();
    let sd = Send::default();
    let rx = make_receiver(&rv, 0);
    trust(rx, 0x71);
    let tx = make_sender(&sd, 0x71);
    connect(tx, unsafe { lenny_receiver_port(rx) }, null());
    assert!(wait_for(|| rv.starts.load(SeqCst) == 1 && streaming(tx)));

    // Camera picked 1280x720 instead of the requested 1080p.
    let actual = settings(1280, 720, 10000);
    assert_eq!(unsafe { lenny_sender_update_stream(tx, &actual) }, LENNY_OK);
    assert!(wait_for(|| rv.starts.load(SeqCst) == 2));
    {
        let st = rv.started.lock().unwrap();
        assert!(st.mode.width == 1280 && st.mode.height == 720);
    }

    // Latency appears once clock sync has a sample (first PING after ~1 s).
    let idr = [0, 0, 0, 1, 0x65];
    assert!(wait_for(|| {
        frame(tx, &idr, lenny_now_us(), 0, LENNY_FRAME_KEYFRAME);
        std::thread::sleep(Duration::from_millis(30));
        stats(rx).latency_us >= 0
    }));
    assert!(stats(rx).latency_us < 200_000); // same machine: well under 200 ms
    destroy(tx);
    destroy(rx);
}

#[test]
fn receiver_lists_modes_and_switches_mid_stream() {
    let rv = Recv::default();
    let sd = Send::default();
    let rx = make_receiver(&rv, 0);
    trust(rx, 0x72);
    let tx = make_sender(&sd, 0x72);
    connect(tx, unsafe { lenny_receiver_port(rx) }, null());
    assert!(wait_for(|| rv.starts.load(SeqCst) == 1 && streaming(tx)));

    let mut p = lenny_peer_info::default();
    assert_eq!(unsafe { lenny_session_peer(rx, &mut p) }, LENNY_OK);
    assert!(p.mode_count == 3 && p.modes[2].width == 3840 && p.modes[2].height == 2160);

    // Ask for 720p mid-stream: the phone gets CAPS_SELECT, the receiver a new STREAM_START.
    let want = settings(1280, 720, 5000);
    assert_eq!(unsafe { lenny_receiver_select_stream(rx, &want) }, LENNY_OK);
    assert!(wait_for(|| rv.starts.load(SeqCst) == 2 && sd.configs.load(SeqCst) == 2));
    {
        let st = rv.started.lock().unwrap();
        assert!(st.mode.width == 1280 && st.mode.height == 720);
    }
    assert!(streaming(tx) && streaming(rx)); // no reconnect
    assert_eq!(unsafe { lenny_receiver_select_stream(tx, &want) }, LENNY_E_STATE); // senders don't choose
    destroy(tx);
    destroy(rx);
}

#[test]
fn qr_token_skips_approval_and_is_single_use() {
    let rv = Recv::default();
    let (a, b) = (Send::default(), Send::default());
    let rx = make_receiver(&rv, 0);
    let mut token = [0u8; LENNY_PAIR_TOKEN_SIZE];
    assert_eq!(unsafe { lenny_receiver_new_pair_token(rx, 0, token.as_mut_ptr()) }, LENNY_OK);
    let port = unsafe { lenny_receiver_port(rx) };

    let tx = make_sender(&a, 0x21);
    assert_eq!(connect(tx, port, token.as_ptr()), LENNY_OK);
    assert!(wait_for(|| streaming(rx) && streaming(tx)));
    assert_eq!(rv.approvals.load(SeqCst), 0);
    destroy(tx);

    // A photo of the same QR code, different phone: rejected, no retry.
    let tx2 = make_sender(&b, 0x22);
    assert!(wait_for(|| state(rx) == LENNY_STATE_CONNECTING as i32));
    assert_eq!(connect(tx2, port, token.as_ptr()), LENNY_OK);
    assert!(wait_for(|| state(tx2) == LENNY_STATE_CLOSED as i32));
    assert_eq!(b.last_reason.load(SeqCst), LENNY_REASON_PAIR_DENIED);
    destroy(tx2);
    destroy(rx);
}

#[test]
fn trusted_device_skips_prompt_denied_device_is_closed() {
    let rv = Recv::default();
    let (ok, denied) = (Send::default(), Send::default());
    let rx = make_receiver(&rv, 0);
    let port = unsafe { lenny_receiver_port(rx) };
    trust(rx, 0x31);

    let tx = make_sender(&ok, 0x31);
    assert_eq!(connect(tx, port, null()), LENNY_OK);
    assert!(wait_for(|| streaming(rx) && streaming(tx)));
    assert_eq!(rv.approvals.load(SeqCst), 0);
    destroy(tx);

    let tx2 = make_sender(&denied, 0x32);
    assert!(wait_for(|| state(rx) == LENNY_STATE_CONNECTING as i32));
    assert_eq!(connect(tx2, port, null()), LENNY_OK);
    assert!(wait_for(|| rv.approvals.load(SeqCst) == 1));
    assert_eq!(unsafe { lenny_receiver_approve(rx, 0) }, LENNY_OK);
    assert!(wait_for(|| state(tx2) == LENNY_STATE_CLOSED as i32));
    assert_eq!(denied.last_reason.load(SeqCst), LENNY_REASON_PAIR_DENIED);
    destroy(tx2);
    destroy(rx);
}

#[test]
fn second_phone_gets_busy_first_keeps_streaming() {
    let rv = Recv::default();
    let (a, b) = (Send::default(), Send::default());
    let rx = make_receiver(&rv, 0);
    let port = unsafe { lenny_receiver_port(rx) };
    trust(rx, 0x41);
    trust(rx, 0x42);

    let tx = make_sender(&a, 0x41);
    connect(tx, port, null());
    assert!(wait_for(|| streaming(tx)));
    let tx2 = make_sender(&b, 0x42);
    connect(tx2, port, null());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!streaming(tx2));
    assert_eq!(state(tx2), LENNY_STATE_RECONNECTING as i32); // BUSY is not fatal: it keeps trying
    assert!(streaming(tx) && streaming(rx));

    // First phone leaves -> the waiting one gets in on its next retry.
    destroy(tx);
    assert!(wait_for(|| streaming(tx2)));
    destroy(tx2);
    destroy(rx);
}

#[test]
fn sender_reconnects_after_receiver_restart() {
    let (r1, r2) = (Recv::default(), Recv::default());
    let sd = Send::default();
    let mut rx = make_receiver(&r1, 0);
    let port = unsafe { lenny_receiver_port(rx) };
    trust(rx, 0x51);
    let tx = make_sender(&sd, 0x51);
    connect(tx, port, null());
    assert!(wait_for(|| streaming(tx)));

    // Desktop app crashes / restarts on the same port.
    destroy(rx);
    assert!(wait_for(|| state(tx) == LENNY_STATE_RECONNECTING as i32));
    rx = make_receiver(&r2, port);
    trust(rx, 0x51);
    assert!(wait_for(|| streaming(tx) && streaming(rx)));
    assert_eq!(sd.keyframe_requests.load(SeqCst), 2); // one per stream
    assert_eq!(stats(tx).reconnects, 1);
    destroy(tx);
    destroy(rx);
}

#[test]
fn nothing_listening_keeps_retrying_and_disconnect_is_fast() {
    let sd = Send::default();
    let tx = make_sender(&sd, 0x11);
    // Port 1 on localhost: refused immediately, so this exercises the backoff loop.
    assert_eq!(connect(tx, 1, null()), LENNY_OK);
    assert!(wait_for(|| state(tx) == LENNY_STATE_RECONNECTING as i32));
    assert_eq!(connect(tx, 1, null()), LENNY_E_STATE); // already running
    let t0 = Instant::now();
    destroy(tx);
    assert!(t0.elapsed() < Duration::from_secs(1));
}

#[test]
fn null_and_bad_args_do_not_crash() {
    unsafe {
        assert!(lenny_sender_create(null(), null()).is_null());
        assert!(lenny_receiver_create(null(), null()).is_null());
        assert_eq!(lenny_sender_connect(null_mut(), c"x".as_ptr(), 1, null()), LENNY_E_INVALID_ARG);
        assert_eq!(lenny_session_state(null_mut()), LENNY_STATE_CLOSED as i32);
        lenny_session_destroy(null_mut());
        let sd = Send::default();
        let tx = make_sender(&sd, 0x11);
        assert_eq!(lenny_sender_connect(tx, c"".as_ptr(), 1, null()), LENNY_E_INVALID_ARG);
        assert_eq!(lenny_sender_send_video_config(tx, null(), 0), LENNY_E_INVALID_ARG);
        assert_eq!(lenny_receiver_new_pair_token(tx, 0, null_mut()), LENNY_E_INVALID_ARG);
        let mut c = lenny_control { cmd: 999, ..Default::default() };
        assert_eq!(lenny_receiver_send_control(tx, &mut c), LENNY_E_STATE); // sender can't send controls
        lenny_session_destroy(tx);
    }
}

/// Struct layouts must match the C header byte for byte (Dart/Kotlin/C++ callers compile against lenny.h).
/// tests/abi_layout_64.txt is the output of tools/abi_layout.c compiled against lenny.h (see tools/abi_check.sh).
#[test]
#[cfg(target_pointer_width = "64")]
fn struct_layouts_match_header() {
    use std::fmt::Write;
    use std::mem::{align_of, offset_of, size_of};
    let mut out = String::new();
    macro_rules! s {
        ($t:ident) => {
            writeln!(out, "{} size={} align={}", stringify!($t), size_of::<$t>(), align_of::<$t>()).unwrap()
        };
    }
    macro_rules! o {
        ($t:ident, $f:ident) => {
            writeln!(out, "  {}.{} @{}", stringify!($t), stringify!($f), offset_of!($t, $f)).unwrap()
        };
    }
    s!(lenny_mode);
    o!(lenny_mode, width);
    o!(lenny_mode, height);
    o!(lenny_mode, fps_num);
    o!(lenny_mode, fps_den);
    s!(lenny_stream_settings);
    o!(lenny_stream_settings, codec);
    o!(lenny_stream_settings, mode);
    o!(lenny_stream_settings, bitrate_kbps);
    o!(lenny_stream_settings, has_lens);
    o!(lenny_stream_settings, lens_id);
    s!(lenny_lens);
    o!(lenny_lens, lens_id);
    o!(lenny_lens, facing);
    o!(lenny_lens, label);
    s!(lenny_control);
    o!(lenny_control, req_id);
    o!(lenny_control, cmd);
    o!(lenny_control, x);
    o!(lenny_control, y);
    o!(lenny_control, value);
    s!(lenny_control_state);
    o!(lenny_control_state, af_mode);
    o!(lenny_control_state, exposure_comp);
    o!(lenny_control_state, exposure_lock);
    o!(lenny_control_state, wb_lock);
    o!(lenny_control_state, torch);
    o!(lenny_control_state, lens_id);
    o!(lenny_control_state, zoom);
    o!(lenny_control_state, battery);
    o!(lenny_control_state, charging);
    o!(lenny_control_state, pan_x);
    o!(lenny_control_state, pan_y);
    s!(lenny_video_frame);
    o!(lenny_video_frame, frame_seq);
    o!(lenny_video_frame, pts_us);
    o!(lenny_video_frame, local_pts_us);
    o!(lenny_video_frame, orientation);
    o!(lenny_video_frame, flags);
    o!(lenny_video_frame, data);
    o!(lenny_video_frame, size);
    s!(lenny_stats);
    o!(lenny_stats, rtt_us);
    o!(lenny_stats, clock_offset_us);
    o!(lenny_stats, frames);
    o!(lenny_stats, bytes);
    o!(lenny_stats, bad_messages);
    o!(lenny_stats, reconnects);
    o!(lenny_stats, latency_us);
    o!(lenny_stats, dropped_frames);
    o!(lenny_stats, bitrate_kbps);
    s!(lenny_identity);
    o!(lenny_identity, device_id);
    o!(lenny_identity, device_name);
    o!(lenny_identity, app_version);
    o!(lenny_identity, platform);
    s!(lenny_sender_config);
    o!(lenny_sender_config, identity);
    o!(lenny_sender_config, modes);
    o!(lenny_sender_config, mode_count);
    o!(lenny_sender_config, max_bitrate_kbps);
    o!(lenny_sender_config, controls);
    o!(lenny_sender_config, lenses);
    o!(lenny_sender_config, lens_count);
    o!(lenny_sender_config, exposure_comp_min);
    o!(lenny_sender_config, exposure_comp_max);
    o!(lenny_sender_config, exposure_comp_step_milli);
    o!(lenny_sender_config, lens_caps);
    s!(lenny_lens_caps);
    o!(lenny_lens_caps, modes);
    o!(lenny_lens_caps, mode_count);
    o!(lenny_lens_caps, zoom_min);
    o!(lenny_lens_caps, zoom_max);
    o!(lenny_lens_caps, zoom_base);
    s!(lenny_sender_callbacks);
    o!(lenny_sender_callbacks, user);
    o!(lenny_sender_callbacks, on_state);
    o!(lenny_sender_callbacks, on_stream_config);
    o!(lenny_sender_callbacks, on_control);
    o!(lenny_sender_callbacks, on_bitrate);
    s!(lenny_receiver_config);
    o!(lenny_receiver_config, identity);
    o!(lenny_receiver_config, port);
    o!(lenny_receiver_config, preferred);
    s!(lenny_receiver_callbacks);
    o!(lenny_receiver_callbacks, user);
    o!(lenny_receiver_callbacks, on_state);
    o!(lenny_receiver_callbacks, on_approval_needed);
    o!(lenny_receiver_callbacks, on_stream_start);
    o!(lenny_receiver_callbacks, on_stream_status);
    o!(lenny_receiver_callbacks, on_video_config);
    o!(lenny_receiver_callbacks, on_video_frame);
    o!(lenny_receiver_callbacks, on_control_state);
    o!(lenny_receiver_callbacks, on_control_ack);
    s!(lenny_peer_info);
    o!(lenny_peer_info, device_id);
    o!(lenny_peer_info, name);
    o!(lenny_peer_info, platform);
    o!(lenny_peer_info, controls);
    o!(lenny_peer_info, lens_count);
    o!(lenny_peer_info, lens_ids);
    o!(lenny_peer_info, lens_facing);
    o!(lenny_peer_info, lens_labels);
    o!(lenny_peer_info, exposure_min);
    o!(lenny_peer_info, exposure_max);
    o!(lenny_peer_info, exposure_step_milli);
    o!(lenny_peer_info, mode_count);
    o!(lenny_peer_info, modes);
    // A Windows checkout may turn the fixture into CRLF.
    assert_eq!(out, include_str!("abi_layout_64.txt").replace("\r\n", "\n"));
}

#[test]
fn per_lens_caps_pan_and_control_state_reach_the_receiver() {
    use lenny_core::session::{ReceiverConfig, Session};
    let rv = Recv::default();
    let mut cb: lenny_receiver_callbacks = unsafe { std::mem::zeroed() };
    cb.user = &rv as *const Recv as *mut c_void;
    cb.on_stream_start = Some(rx_start);
    cb.on_control_ack = Some(rx_ack);
    // The desktop app uses the Rust API directly (no FFI); the phone goes through the C ABI.
    let rx = Session::new_receiver(
        ReceiverConfig {
            identity: ident(0xDD, c"Desk", LENNY_PLATFORM_LINUX),
            device_name: b"Desk".to_vec(),
            app_version: vec![],
            port: 0,
            preferred: settings(1920, 1080, 8000),
        },
        cb,
    );
    assert!(rx.peer_caps().is_none());
    rx.trust(&[0x81; 16]);
    assert_eq!(rx.start(), LENNY_OK);
    let sd = Send::default();
    let tx = make_sender(&sd, 0x81);
    connect(tx, rx.port(), null());
    assert!(wait_for(|| rx.state() == LENNY_STATE_STREAMING as i32 && streaming(tx)));

    let caps = rx.peer_caps().unwrap();
    assert_eq!(caps.lenses[0].modes, MODES[..2].to_vec());
    assert!(caps.lenses[0].zoom_min == 100 && caps.lenses[0].zoom_max == 800);
    assert_eq!(caps.lenses[1].modes, MODES[..1].to_vec());

    let mut pan = lenny_control { cmd: LENNY_CTL_PAN as u16, x: 50000, y: 32768, ..Default::default() };
    assert_eq!(rx.send_control(&mut pan), LENNY_OK);
    assert!(wait_for(|| rv.acks.load(SeqCst) == 1 && sd.focus_x.load(SeqCst) == 50000));

    let cs = lenny_control_state { pan_x: 50000, pan_y: 1234, zoom: 250, ..Default::default() };
    assert_eq!(unsafe { lenny_sender_send_control_state(tx, &cs) }, LENNY_OK);
    assert!(wait_for(|| rx.control_state().1.pan_y == 1234));
    destroy(tx);
}

/// A 1.0 phone (the C++ core before 1.1): pan is refused locally instead of sent, the rest works.
#[test]
fn pan_needs_a_1_1_phone() {
    use lenny_core::wire::*;
    use std::io::{Read, Write};
    let rv = Recv::default();
    let rx = make_receiver(&rv, 0);
    trust(rx, 0x91);
    let mut phone = std::net::TcpStream::connect(("127.0.0.1", unsafe { lenny_receiver_port(rx) })).unwrap();
    let hello =
        Hello { proto_minor: 0, role: 1, device_id: [0x91; 16], device_name: b"Old".to_vec(), ..Default::default() };
    phone.write_all(&to_message(&hello, 0)).unwrap();
    let caps = Caps { modes: MODES.to_vec(), controls: LENNY_CAP_PAN as u64, ..Default::default() };
    phone.write_all(&to_message(&caps, 0)).unwrap();
    // Read until CAPS_SELECT, then start streaming with what was asked.
    let mut reader = MessageReader::default();
    let mut buf = [0u8; 4096];
    let sel = 'outer: loop {
        let n = phone.read(&mut buf).unwrap();
        assert!(n > 0);
        reader.feed(&buf[..n]);
        while let Status::Message(h, p) = reader.next() {
            assert_eq!(h.ver_minor, 0); // everything after our HELLO uses the negotiated 1.0 (§3)
            if h.typ == msg::HELLO {
                assert_eq!(Hello::decode(p).unwrap().proto_minor, 1); // while announcing its own 1.1
            }
            if h.typ == msg::CAPS_SELECT {
                break 'outer CapsSelect::decode(p).unwrap();
            }
        }
    };
    phone.write_all(&to_message(&StreamStart(sel.0), 0)).unwrap();
    assert!(wait_for(|| streaming(rx)));
    let mut pan = lenny_control { cmd: LENNY_CTL_PAN as u16, x: 1, y: 1, ..Default::default() };
    assert_eq!(unsafe { lenny_receiver_send_control(rx, &mut pan) }, LENNY_E_STATE);
    let mut key = lenny_control { cmd: LENNY_CTL_KEYFRAME_REQUEST as u16, ..Default::default() };
    assert_eq!(unsafe { lenny_receiver_send_control(rx, &mut key) }, LENNY_OK);
    destroy(rx);
}

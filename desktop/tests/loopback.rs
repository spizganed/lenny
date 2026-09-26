//! Whole desktop pipeline without a phone: fake phone (openh264) -> core over TCP -> Engine (decode, preview,
//! virtual camera, controls). Runs headless.

use std::time::{Duration, Instant};

use lenny_core::*;
use lenny_desktop::fake_phone::FakePhone;
use lenny_desktop::receiver::Engine;

fn wait(engine: &mut Engine, what: &str, mut ok: impl FnMut(&mut Engine) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !ok(engine) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        engine.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn fake_phone_streams_through_the_desktop_pipeline() {
    let dir = std::env::temp_dir().join(format!("lenny-desktop-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("known_phones.txt"), format!("{} Fake phone\n", "fa".repeat(16))).unwrap();
    std::env::set_var("LENNY_CONFIG_DIR", &dir);

    let mut engine = Engine::start(eframe::egui::Context::default(), 0).expect("engine");
    assert!(engine.pair_uri().starts_with("lenny://c?v=1&"));
    let _phone = FakePhone::start("127.0.0.1", engine.port(), 0xFA, false);

    // Trusted phone: straight to streaming, video decoded, preview at the stream's own aspect ratio.
    wait(&mut engine, "decoded video", |e| e.decoded_frames() > 10);
    let mut seen = 0;
    let p = engine.take_preview(&mut seen).expect("preview");
    assert_eq!(p.source, (1920, 1080));
    assert_eq!((p.width, p.height), (960, 540));
    assert_eq!(p.rgba.len(), 960 * 540 * 4);

    // Per-lens caps arrived (protocol 1.1).
    let caps = engine.session.peer_caps().unwrap();
    assert_eq!(caps.lenses[1].modes.len(), 2);
    assert_eq!(caps.lenses[0].zoom_max, 400);

    // Zoom + pan go to the phone, and its CONTROL_STATE comes back.
    engine.control(lenny_control_cmd::LENNY_CTL_ZOOM, 0, 0, 200);
    engine.control(lenny_control_cmd::LENNY_CTL_PAN, 60000, 1000, 0);
    wait(&mut engine, "zoom/pan state", |e| {
        let cs = e.session.control_state().1;
        cs.zoom == 200 && cs.pan_x == 60000 && cs.pan_y == 1000
    });

    // Manual focus, then back to Auto.
    engine.control(lenny_control_cmd::LENNY_CTL_FOCUS_AT, 30000, 30000, 0);
    wait(&mut engine, "manual", |e| e.session.control_state().1.af_mode == 1);
    engine.control(lenny_control_cmd::LENNY_CTL_RESET_AUTO, 0, 0, 0);
    wait(&mut engine, "auto", |e| e.session.control_state().1.af_mode == 0);

    // Mode switch mid-stream: the decoder follows the new size.
    let (_, s) = engine.session.stream_settings();
    let want = lenny_mode { width: 1280, height: 720, fps_num: 30, fps_den: 1 };
    assert_eq!(engine.session.select_stream(&lenny_stream_settings { mode: want, ..s }), LENNY_OK);
    wait(&mut engine, "720p frames", |e| e.take_preview(&mut seen).is_some_and(|p| p.source == (1280, 720)));

    // The virtual camera backend loaded (null in containers, v4l2loopback where the module is available).
    let (_, describe) = engine.vcam_status();
    assert!(!describe.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

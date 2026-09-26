//! Wire format, test vectors, clock sync, pairing tokens (port of core/tests/test_wire.cpp).

use lenny_core::pairing::TokenStore;
use lenny_core::timing::ClockSync;
use lenny_core::wire::*;
use lenny_core::*;

fn load_vector(name: &str) -> Vec<u8> {
    let path = format!("{}/../protocol/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .flat_map(|l| {
            l.split('#')
                .next()
                .unwrap()
                .split_whitespace()
                .map(|b| u8::from_str_radix(b, 16).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Feeds `msg` through a reader and returns the single message inside.
fn read_one(msg: &[u8]) -> Option<(Header, Vec<u8>)> {
    let mut r = MessageReader::default();
    r.feed(msg);
    let (h, p) = match r.next() {
        Status::Message(h, p) => (h, p.to_vec()),
        _ => return None,
    };
    matches!(r.next(), Status::NeedMore).then_some((h, p))
}

fn roundtrip<M: Message>(m: &M) -> M {
    let (h, p) = read_one(&to_message(m, VERSION_MINOR)).expect("one message");
    assert_eq!(h.typ, M::TYPE);
    M::decode(&p).expect("decodes")
}

#[test]
fn vector_hello_decodes_and_reencodes() {
    let v = load_vector("hello_sender_min.hex");
    assert_eq!(v.len(), 54);
    let (h, p) = read_one(&v).unwrap();
    assert!(h.typ == msg::HELLO && h.length == 42);
    let m = Hello::decode(&p).unwrap();
    assert!(m.role == 1 && m.proto_major == 1 && m.proto_minor == 0 && m.device_name == b"Pix");
    assert!(m.device_id[0] == 0x00 && m.device_id[15] == 0x0F);
    assert_eq!(to_message(&m, h.ver_minor), v); // a 1.0 vector: re-encoded as 1.0
}

#[test]
fn vector_control_focus_at() {
    let v = load_vector("control_focus_at.hex");
    let (h, p) = read_one(&v).unwrap();
    let c = Control::decode(&p).unwrap();
    assert!(c.0.req_id == 7 && c.0.cmd == LENNY_CTL_FOCUS_AT as u16 && c.0.x == 0x8000 && c.0.y == 0x4000);
    assert_eq!(to_message(&c, h.ver_minor), v);
}

#[test]
fn vector_video_frame() {
    let v = load_vector("video_frame_key.hex");
    let (h, p) = read_one(&v).unwrap();
    assert_eq!(h.typ, msg::VIDEO_FRAME);
    let (m, data) = decode_video_meta(&p).unwrap();
    assert!(m.frame_seq == 1 && m.pts_us == 1_000_000 && m.orientation == 1 && m.flags == LENNY_FRAME_KEYFRAME);
    assert!(data.len() == 6 && data[4] == 0x65);
    let mut meta = [0u8; VIDEO_FRAME_META_SIZE];
    encode_video_meta(&mut meta, &m);
    assert_eq!(&meta[..], &v[HEADER_SIZE..HEADER_SIZE + VIDEO_FRAME_META_SIZE]);
}

#[test]
fn reader_handles_byte_by_byte_and_back_to_back() {
    let mut stream = to_message(&Ping { seq: 1, t1: 10 }, 0);
    stream.extend(to_message(&Ping { seq: 2, t1: 20 }, 0));
    let mut r = MessageReader::default();
    let mut got = 0u32;
    for b in stream {
        r.feed(&[b]);
        while let Status::Message(_, p) = r.next() {
            assert_eq!(Ping::decode(p).unwrap().seq, got + 1);
            got += 1;
        }
    }
    assert_eq!(got, 2);
}

#[test]
fn reader_rejects_bad_magic_and_oversize() {
    let mut r = MessageReader::default();
    let mut bad = to_message(&Ping { seq: 1, t1: 1 }, 0);
    bad[0] = b'X';
    r.feed(&bad);
    assert!(matches!(r.next(), Status::BadMagic));

    let mut big = [0u8; HEADER_SIZE];
    put_header(&mut big, &Header { typ: msg::HELLO, length: MAX_CONTROL_PAYLOAD + 1, ..Default::default() });
    let mut r2 = MessageReader::default();
    r2.feed(&big);
    assert!(matches!(r2.next(), Status::TooLarge));

    // Video frames get the bigger limit.
    put_header(&mut big, &Header { typ: msg::VIDEO_FRAME, length: MAX_CONTROL_PAYLOAD + 1, ..Default::default() });
    let mut r3 = MessageReader::default();
    r3.feed(&big);
    assert!(matches!(r3.next(), Status::NeedMore));
    // ...up to its own limit.
    put_header(&mut big, &Header { typ: msg::VIDEO_FRAME, length: MAX_VIDEO_PAYLOAD + 1, ..Default::default() });
    let mut r4 = MessageReader::default();
    r4.feed(&big);
    assert!(matches!(r4.next(), Status::TooLarge));
}

#[test]
fn unknown_type_passes_through_reader() {
    let mut m = vec![0xAB; HEADER_SIZE + 3];
    put_header(&mut m, &Header { typ: 0x7777, length: 3, ..Default::default() });
    m.extend(to_message(&Ping { seq: 9, t1: 9 }, 0));
    let mut r = MessageReader::default();
    r.feed(&m);
    assert!(matches!(r.next(), Status::Message(h, p) if h.typ == 0x7777 && p.len() == 3));
    assert!(matches!(r.next(), Status::Message(h, _) if h.typ == msg::PING));
}

#[test]
fn unknown_tlv_tags_are_skipped() {
    let mut payload = vec![];
    let mut w = TlvWriter(&mut payload);
    w.u32(1, 5);
    w.str(999, b"from the future"); // unknown tag
    w.i64(2, 77);
    let p = Ping::decode(&payload).unwrap();
    assert!(p.seq == 5 && p.t1 == 77);
}

#[test]
fn truncated_or_missing_fields_rejected() {
    let mut payload = vec![];
    TlvWriter(&mut payload).u32(1, 5);
    assert!(Ping::decode(&payload).is_none()); // t1 missing
    TlvWriter(&mut payload).i64(2, 7);
    payload.pop(); // truncated field
    assert!(Ping::decode(&payload).is_none());
    let mut wrong_size = vec![];
    let mut w2 = TlvWriter(&mut wrong_size);
    w2.u16(1, 5); // seq must be u32
    w2.i64(2, 7);
    assert!(Ping::decode(&wrong_size).is_none());
}

#[test]
fn roundtrip_all_messages() {
    let mut hello = Hello {
        role: 2,
        device_name: b"Desk".to_vec(),
        app_version: b"0.1".to_vec(),
        platform: LENNY_PLATFORM_WINDOWS,
        pairing_required: true,
        features: 1,
        ..Default::default()
    };
    hello.device_id[3] = 9;
    let h2 = roundtrip(&hello);
    assert_eq!(h2, hello);

    assert_eq!(roundtrip(&Goodbye { reason: LENNY_REASON_BUSY as u16, detail: b"busy".to_vec() }).detail, b"busy");
    let pong = roundtrip(&Pong { seq: 3, t1: 1, t2: 2, t3: 3 });
    assert!(pong.seq == 3 && pong.t3 == 3);

    let mut pr = PairRequest::default();
    pr.token[15] = 0xEE;
    assert_eq!(roundtrip(&pr).token[15], 0xEE);
    assert_eq!(roundtrip(&PairResult { result: LENNY_PAIR_EXPIRED }).result, LENNY_PAIR_EXPIRED);

    let caps = Caps {
        codecs: vec![Codec { id: LENNY_CODEC_H264, profile: 66, level: 31 }],
        modes: vec![
            lenny_mode { width: 1280, height: 720, fps_num: 30, fps_den: 1 },
            lenny_mode { width: 1920, height: 1080, fps_num: 30000, fps_den: 1001 },
        ],
        max_bitrate_kbps: 12000,
        controls: (LENNY_CAP_FOCUS | LENNY_CAP_TORCH) as u64,
        lenses: vec![
            Lens { id: 0, facing: 0, label: b"Wide".to_vec(), ..Default::default() },
            Lens { id: 1, facing: 1, label: b"Front".to_vec(), ..Default::default() },
        ],
        has_exposure_range: true,
        exposure_min: -2000,
        exposure_max: 2000,
        exposure_step_milli: 333,
    };
    assert_eq!(roundtrip(&caps), caps);

    let mut s = lenny_stream_settings {
        codec: LENNY_CODEC_H264,
        mode: lenny_mode { width: 1920, height: 1080, fps_num: 30, fps_den: 1 },
        bitrate_kbps: 8000,
        has_lens: 1,
        lens_id: 2,
    };
    let sel = roundtrip(&CapsSelect(s));
    assert!(sel.0.mode.width == 1920 && sel.0.bitrate_kbps == 8000 && sel.0.has_lens == 1 && sel.0.lens_id == 2);
    s.has_lens = 0;
    assert_eq!(roundtrip(&StreamStart(s)).0.has_lens, 0);

    assert_eq!(roundtrip(&StreamStatus { state: LENNY_STREAM_CAMERA_LOST, reason: b"call".to_vec() }).reason, b"call");
    let vc = VideoConfig { codec: LENNY_CODEC_H264, config: vec![0, 0, 0, 1, 0x67] };
    assert_eq!(roundtrip(&vc).config, vc.config);

    for cmd in LENNY_CTL_KEYFRAME_REQUEST as u16..=LENNY_CTL_PAN as u16 {
        let mut c = lenny_control { req_id: 42, cmd, ..Default::default() };
        if cmd == LENNY_CTL_EXPOSURE_COMP as u16 {
            c.value = -1333;
        }
        if cmd == LENNY_CTL_ZOOM as u16 {
            c.value = 250;
        }
        if cmd == LENNY_CTL_TORCH as u16 || cmd == LENNY_CTL_SELECT_LENS as u16 {
            c.value = 1;
        }
        let out = roundtrip(&Control(c));
        assert!(out.0.cmd == cmd && out.0.req_id == 42 && out.0.value == c.value);
    }
    assert_eq!(roundtrip(&ControlAck { req_id: 42, result: LENNY_ACK_FAILED as u8 }).result, LENNY_ACK_FAILED as u8);
    let cs = lenny_control_state {
        af_mode: 1,
        exposure_comp: -500,
        exposure_lock: 1,
        wb_lock: 0,
        torch: 1,
        lens_id: 2,
        zoom: 150,
        battery: 87,
        charging: 1,
        pan_x: 1000,
        pan_y: 60000,
    };
    assert_eq!(roundtrip(&ControlState(cs)).0, cs);
    // an old phone sends no battery or pan tags
    let old = ControlState::decode(&[]).unwrap().0;
    assert!(old.battery == 255 && old.pan_x == PAN_CENTER && old.pan_y == PAN_CENTER);
}

#[test]
fn clock_sync_prefers_lowest_rtt() {
    let mut c = ClockSync::default();
    assert!(!c.valid() && c.rtt_us() == -1);
    // Remote clock is +1000 ahead. Symmetric 100 us each way.
    c.add(0, 1100, 1100, 200);
    assert!(c.rtt_us() == 200 && c.offset_us() == 1000);
    // Slow, asymmetric sample (queued on the way back) must not win.
    c.add(10000, 11100, 11100, 15000);
    assert!(c.rtt_us() == 200 && c.offset_us() == 1000);
    c.add(5, 5, 4, 0); // negative rtt: ignored
    assert_eq!(c.rtt_us(), 200);
}

#[test]
fn pair_tokens_single_use_and_expiry() {
    let ts = TokenStore::default();
    let a = ts.issue(0, 1000);
    let b = ts.issue(0, 1000);
    assert_ne!(a, b);
    assert_eq!(ts.redeem(&a, 500), LENNY_PAIR_OK);
    assert_eq!(ts.redeem(&a, 600), LENNY_PAIR_ALREADY_USED);
    assert_eq!(ts.redeem(&b, 2000), LENNY_PAIR_EXPIRED);
    assert_eq!(ts.redeem(&[0; 16], 0), LENNY_PAIR_UNKNOWN_TOKEN);
}

#[test]
fn vector_control_pan() {
    let v = load_vector("control_pan.hex");
    let (_, p) = read_one(&v).unwrap();
    let c = Control::decode(&p).unwrap();
    assert!(c.0.req_id == 9 && c.0.cmd == LENNY_CTL_PAN as u16 && c.0.x == 0xC000 && c.0.y == 0x8000);
    assert_eq!(to_message(&c, 1), v);
}

#[test]
fn vector_caps_per_lens_1_1() {
    let v = load_vector("caps_lens_1_1.hex");
    let (h, p) = read_one(&v).unwrap();
    assert_eq!(h.ver_minor, 1);
    let c = Caps::decode(&p).unwrap();
    let hd = lenny_mode { width: 1280, height: 720, fps_num: 30, fps_den: 1 };
    assert_eq!(c.modes, vec![lenny_mode { width: 1920, height: 1080, fps_num: 30, fps_den: 1 }]);
    assert_eq!(c.controls, (LENNY_CAP_LENS | LENNY_CAP_ZOOM | LENNY_CAP_PAN) as u64);
    let lens =
        Lens { id: 0, facing: 0, label: b"1x".to_vec(), modes: vec![hd], zoom_min: 60, zoom_max: 1000, zoom_base: 100 };
    assert_eq!(c.lenses, vec![lens]);
    assert_eq!(to_message(&c, 1), v);
    // A 1.0 peer gets the same CAPS without the per-lens fields, and still decodes it.
    let old = Caps::decode(&read_one(&to_message(&c, 0)).unwrap().1).unwrap();
    assert!(old.lenses[0].modes.is_empty() && old.lenses[0].zoom_max == 0 && old.lenses[0].label == b"1x");
}

#[test]
fn newer_fields_are_left_out_for_older_peers() {
    let cs = ControlState(lenny_control_state { pan_x: 7, pan_y: 8, ..Default::default() });
    assert_eq!(ControlState::decode(&read_one(&to_message(&cs, 1)).unwrap().1).unwrap().0.pan_x, 7);
    assert_eq!(ControlState::decode(&read_one(&to_message(&cs, 0)).unwrap().1).unwrap().0.pan_x, PAN_CENTER);
}

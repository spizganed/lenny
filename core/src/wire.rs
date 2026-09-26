//! Wire format: framing, TLV, message codecs (docs/protocol.md §3–§6). Pure code, no I/O.

use crate::abi::*;

pub const MAGIC0: u8 = 0x4C; // 'L'
pub const MAGIC1: u8 = 0x59; // 'Y'
pub const VERSION_MAJOR: u8 = 1;
/// 1.1: per-lens modes and zoom range in CAPS, CONTROL pan, pan in CONTROL_STATE (protocol.md §11).
pub const VERSION_MINOR: u8 = 1;
pub const HEADER_SIZE: usize = 12;
pub const MAX_VIDEO_PAYLOAD: u32 = 4 << 20;
pub const MAX_CONTROL_PAYLOAD: u32 = 64 << 10;
pub const VIDEO_FRAME_META_SIZE: usize = 16;
/// CONTROL pan / CONTROL_STATE pan_x, pan_y: 0..65535 across the pannable range, this = centered.
pub const PAN_CENTER: u16 = 32768;

/// Message type ids (protocol.md §5). Plain u16s: unknown ids are valid on the wire and get skipped.
pub mod msg {
    pub const HELLO: u16 = 0x0001;
    pub const GOODBYE: u16 = 0x0002;
    pub const PING: u16 = 0x0003;
    pub const PONG: u16 = 0x0004;
    pub const PAIR_REQUEST: u16 = 0x0010;
    pub const PAIR_RESULT: u16 = 0x0011;
    pub const CAPS: u16 = 0x0020;
    pub const CAPS_SELECT: u16 = 0x0021;
    pub const STREAM_START: u16 = 0x0022;
    pub const STREAM_STATUS: u16 = 0x0023;
    pub const VIDEO_CONFIG: u16 = 0x0030;
    pub const VIDEO_FRAME: u16 = 0x0031;
    pub const CONTROL: u16 = 0x0040;
    pub const CONTROL_STATE: u16 = 0x0041;
    pub const CONTROL_ACK: u16 = 0x0042;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub ver_major: u8,
    pub ver_minor: u8,
    pub typ: u16,
    pub flags: u16,
    pub length: u32,
}

impl Default for Header {
    fn default() -> Self {
        Self { ver_major: VERSION_MAJOR, ver_minor: VERSION_MINOR, typ: 0, flags: 0, length: 0 }
    }
}

pub fn put_header(out: &mut [u8], h: &Header) {
    out[0] = MAGIC0;
    out[1] = MAGIC1;
    out[2] = h.ver_major;
    out[3] = h.ver_minor;
    out[4..6].copy_from_slice(&h.typ.to_le_bytes());
    out[6..8].copy_from_slice(&h.flags.to_le_bytes());
    out[8..12].copy_from_slice(&h.length.to_le_bytes());
}

fn max_payload(typ: u16) -> u32 {
    if typ == msg::VIDEO_FRAME {
        MAX_VIDEO_PAYLOAD
    } else {
        MAX_CONTROL_PAYLOAD
    }
}

fn le16(p: &[u8]) -> u16 {
    u16::from_le_bytes([p[0], p[1]])
}

pub enum Status<'a> {
    Message(Header, &'a [u8]),
    NeedMore,
    BadMagic,
    TooLarge,
}

/// Incremental reassembly of messages from a byte stream. Unknown types come out like any other
/// message; the caller ignores them (protocol.md §3 rule 4).
#[derive(Default)]
pub struct MessageReader {
    buf: Vec<u8>,
    pos: usize,
}

impl MessageReader {
    pub fn feed(&mut self, data: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
    }

    #[allow(clippy::should_implement_trait)] // lends from its own buffer, so it cannot be an Iterator
    pub fn next(&mut self) -> Status<'_> {
        let p = &self.buf[self.pos..];
        if p.len() < HEADER_SIZE {
            return Status::NeedMore;
        }
        if p[0] != MAGIC0 || p[1] != MAGIC1 {
            return Status::BadMagic;
        }
        let h = Header {
            ver_major: p[2],
            ver_minor: p[3],
            typ: le16(&p[4..]),
            flags: le16(&p[6..]),
            length: u32::from_le_bytes([p[8], p[9], p[10], p[11]]),
        };
        if h.length > max_payload(h.typ) {
            return Status::TooLarge;
        }
        let end = HEADER_SIZE + h.length as usize;
        if p.len() < end {
            return Status::NeedMore;
        }
        let start = self.pos;
        self.pos += end;
        Status::Message(h, &self.buf[start + HEADER_SIZE..start + end])
    }
}

// ---- TLV ---------------------------------------------------------------
pub struct TlvWriter<'a>(pub &'a mut Vec<u8>);

impl TlvWriter<'_> {
    fn head(&mut self, tag: u16, len: usize) {
        self.0.extend_from_slice(&tag.to_le_bytes());
        self.0.extend_from_slice(&(len as u16).to_le_bytes());
    }
    fn raw(&mut self, tag: u16, v: &[u8]) {
        self.head(tag, v.len());
        self.0.extend_from_slice(v);
    }
    pub fn u8(&mut self, tag: u16, v: u8) {
        self.raw(tag, &[v])
    }
    pub fn u16(&mut self, tag: u16, v: u16) {
        self.raw(tag, &v.to_le_bytes())
    }
    pub fn u32(&mut self, tag: u16, v: u32) {
        self.raw(tag, &v.to_le_bytes())
    }
    pub fn u64(&mut self, tag: u16, v: u64) {
        self.raw(tag, &v.to_le_bytes())
    }
    pub fn i32(&mut self, tag: u16, v: i32) {
        self.raw(tag, &v.to_le_bytes())
    }
    pub fn i64(&mut self, tag: u16, v: i64) {
        self.raw(tag, &v.to_le_bytes())
    }
    /// Truncated to 65535 bytes.
    // ponytail: byte truncation can split a UTF-8 sequence; only matters for >64 KiB names.
    pub fn str(&mut self, tag: u16, v: &[u8]) {
        self.raw(tag, &v[..v.len().min(0xFFFF)])
    }
    /// Caller keeps it <= 65535 bytes.
    pub fn bytes(&mut self, tag: u16, v: &[u8]) {
        self.raw(tag, v)
    }
    pub fn empty(&mut self, tag: u16) {
        self.head(tag, 0)
    }
    pub fn begin_list(&mut self, tag: u16) -> usize {
        self.head(tag, 0);
        self.0.len()
    }
    pub fn end_list(&mut self, mark: usize) {
        let len = (self.0.len() - mark) as u16;
        self.0[mark - 2..mark].copy_from_slice(&len.to_le_bytes());
    }
}

pub struct TlvReader<'a> {
    v: &'a [u8],
    pos: usize,
    pub bad: bool,
}

impl<'a> TlvReader<'a> {
    pub fn new(v: &'a [u8]) -> Self {
        Self { v, pos: 0, bad: false }
    }
}

impl<'a> Iterator for TlvReader<'a> {
    type Item = (u16, &'a [u8]);
    /// None at the end, or on a truncated field (then `bad` is set).
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos == self.v.len() {
            return None;
        }
        if self.v.len() - self.pos < 4 {
            self.bad = true;
            return None;
        }
        let tag = le16(&self.v[self.pos..]);
        let len = le16(&self.v[self.pos + 2..]) as usize;
        self.pos += 4;
        if self.v.len() - self.pos < len {
            self.bad = true;
            return None;
        }
        let val = &self.v[self.pos..self.pos + len];
        self.pos += len;
        Some((tag, val))
    }
}

/// Exact-size little-endian integer in a TLV value.
pub trait Le: Sized + Copy {
    fn get(v: &[u8]) -> Option<Self>;
}
macro_rules! le {
    ($($t:ty),*) => {$(
        impl Le for $t {
            fn get(v: &[u8]) -> Option<Self> { Some(<$t>::from_le_bytes(v.try_into().ok()?)) }
        }
    )*};
}
le!(u8, u16, u32, u64, i32, i64);

/// Stores the value on success; false if the size is wrong (message rejected).
fn set<T: Le>(dst: &mut T, v: &[u8]) -> bool {
    match T::get(v) {
        Some(x) => {
            *dst = x;
            true
        }
        None => false,
    }
}

/// Iterates fields; `f(tag, value)` returns false to reject the message. Unknown tags: return true.
fn each(v: &[u8], mut f: impl FnMut(u16, &[u8]) -> bool) -> bool {
    let mut r = TlvReader::new(v);
    for (tag, val) in r.by_ref() {
        if !f(tag, val) {
            return false;
        }
    }
    !r.bad
}

// ---- Messages ----------------------------------------------------------
pub type DeviceId = [u8; LENNY_DEVICE_ID_SIZE];
pub type PairToken = [u8; LENNY_PAIR_TOKEN_SIZE];
/// Strings are kept as raw bytes, exactly as received (the spec says UTF-8, but a bad peer must not crash us).
pub type Str = Vec<u8>;

pub trait Message: Sized {
    const TYPE: u16;
    /// `minor` = negotiated protocol minor: fields newer than it are left out (§4).
    fn encode(&self, w: &mut TlvWriter, minor: u8);
    /// None = malformed or missing a required field; the message must be ignored (§6).
    fn decode(v: &[u8]) -> Option<Self>;
}

/// Full message (header + TLV payload).
pub fn to_message<M: Message>(m: &M, minor: u8) -> Vec<u8> {
    let mut out = vec![0; HEADER_SIZE];
    m.encode(&mut TlvWriter(&mut out), minor);
    let h = Header { ver_minor: minor, typ: M::TYPE, length: (out.len() - HEADER_SIZE) as u32, ..Default::default() };
    put_header(&mut out, &h);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub proto_major: u8,
    pub proto_minor: u8,
    pub role: u8, // 1 sender, 2 receiver
    pub device_id: DeviceId,
    pub device_name: Str,
    pub app_version: Str, // optional (empty = absent)
    pub platform: u8,     // optional (0 = absent)
    pub pairing_required: bool,
    pub features: u64,
}

impl Default for Hello {
    fn default() -> Self {
        Self {
            proto_major: VERSION_MAJOR,
            proto_minor: VERSION_MINOR,
            role: 0,
            device_id: [0; 16],
            device_name: vec![],
            app_version: vec![],
            platform: 0,
            pairing_required: false,
            features: 0,
        }
    }
}

impl Message for Hello {
    const TYPE: u16 = msg::HELLO;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u8(1, self.proto_major);
        w.u8(2, self.proto_minor);
        w.u8(3, self.role);
        w.bytes(4, &self.device_id);
        w.str(5, &self.device_name);
        if !self.app_version.is_empty() {
            w.str(6, &self.app_version);
        }
        if self.platform != 0 {
            w.u8(7, self.platform);
        }
        if self.pairing_required {
            w.u8(8, 1);
        }
        if self.features != 0 {
            w.u64(9, self.features);
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = Hello::default();
        let mut seen = 0u32;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen |= 1;
                set(&mut m.proto_major, val)
            }
            2 => {
                seen |= 2;
                set(&mut m.proto_minor, val)
            }
            3 => {
                seen |= 4;
                set(&mut m.role, val)
            }
            4 => {
                seen |= 8;
                match val.try_into() {
                    Ok(id) => {
                        m.device_id = id;
                        true
                    }
                    Err(_) => false,
                }
            }
            5 => {
                seen |= 16;
                m.device_name = val.to_vec();
                true
            }
            6 => {
                m.app_version = val.to_vec();
                true
            }
            7 => set(&mut m.platform, val),
            8 => {
                let mut b = 0u8;
                let ok = set(&mut b, val);
                m.pairing_required = b != 0;
                ok
            }
            9 => set(&mut m.features, val),
            _ => true,
        });
        (ok && seen == 31).then_some(m)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Goodbye {
    pub reason: u16,
    pub detail: Str,
}

impl Message for Goodbye {
    const TYPE: u16 = msg::GOODBYE;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u16(1, self.reason);
        if !self.detail.is_empty() {
            w.str(2, &self.detail);
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = Goodbye::default();
        let mut seen = false;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen = true;
                set(&mut m.reason, val)
            }
            2 => {
                m.detail = val.to_vec();
                true
            }
            _ => true,
        });
        (ok && seen).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ping {
    pub seq: u32,
    pub t1: i64,
}

impl Message for Ping {
    const TYPE: u16 = msg::PING;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u32(1, self.seq);
        w.i64(2, self.t1);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = Ping::default();
        let mut seen = 0;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen |= 1;
                set(&mut m.seq, val)
            }
            2 => {
                seen |= 2;
                set(&mut m.t1, val)
            }
            _ => true,
        });
        (ok && seen == 3).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pong {
    pub seq: u32,
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
}

impl Message for Pong {
    const TYPE: u16 = msg::PONG;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u32(1, self.seq);
        w.i64(2, self.t1);
        w.i64(3, self.t2);
        w.i64(4, self.t3);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = Pong::default();
        let mut seen = 0;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen |= 1;
                set(&mut m.seq, val)
            }
            2 => {
                seen |= 2;
                set(&mut m.t1, val)
            }
            3 => {
                seen |= 4;
                set(&mut m.t2, val)
            }
            4 => {
                seen |= 8;
                set(&mut m.t3, val)
            }
            _ => true,
        });
        (ok && seen == 15).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PairRequest {
    pub token: PairToken,
}

impl Message for PairRequest {
    const TYPE: u16 = msg::PAIR_REQUEST;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.bytes(1, &self.token);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = PairRequest::default();
        let mut seen = false;
        let ok = each(v, |tag, val| {
            if tag != 1 {
                return true;
            }
            match val.try_into() {
                Ok(t) => {
                    m.token = t;
                    seen = true;
                    true
                }
                Err(_) => false,
            }
        });
        (ok && seen).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PairResult {
    pub result: u8,
}

impl Message for PairResult {
    const TYPE: u16 = msg::PAIR_RESULT;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u8(1, self.result);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = PairResult::default();
        let mut seen = false;
        let ok = each(v, |tag, val| {
            if tag == 1 {
                seen = true;
                set(&mut m.result, val)
            } else {
                true
            }
        });
        (ok && seen).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Codec {
    pub id: u8,
    pub profile: u8,
    pub level: u8,
}

impl Default for Codec {
    fn default() -> Self {
        Self { id: LENNY_CODEC_H264, profile: 0, level: 0 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lens {
    pub id: u8,
    pub facing: u8,
    pub label: Str,
    /// 1.1: modes this lens can stream (empty = the CAPS-level modes).
    pub modes: Vec<lenny_mode>,
    /// 1.1: CONTROL zoom range for this lens, ratio x100 relative to the lens (0/0 = unknown).
    pub zoom_min: u16,
    pub zoom_max: u16,
    /// 1.1: the lens's own zoom ratio x100 on its camera (60 for a 0.6x sensor of a logical camera; 0 = 100).
    /// Pan works over the camera's field of view at ratio 1, so this is what a receiver needs to map a drag.
    pub zoom_base: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub codecs: Vec<Codec>,
    pub modes: Vec<lenny_mode>,
    pub max_bitrate_kbps: u32,
    pub controls: u64,
    pub lenses: Vec<Lens>,
    pub has_exposure_range: bool,
    pub exposure_min: i32,
    pub exposure_max: i32,
    pub exposure_step_milli: u32,
}

fn encode_mode(w: &mut TlvWriter, tag: u16, md: &lenny_mode) {
    let l = w.begin_list(tag);
    w.u16(1, md.width);
    w.u16(2, md.height);
    w.u16(3, md.fps_num);
    w.u16(4, md.fps_den);
    w.end_list(l);
}

fn decode_mode(v: &[u8]) -> Option<lenny_mode> {
    let mut md = lenny_mode::default();
    let mut seen = 0;
    let ok = each(v, |t, x| match t {
        1 => {
            seen |= 1;
            set(&mut md.width, x)
        }
        2 => {
            seen |= 2;
            set(&mut md.height, x)
        }
        3 => {
            seen |= 4;
            set(&mut md.fps_num, x)
        }
        4 => {
            seen |= 8;
            set(&mut md.fps_den, x)
        }
        _ => true,
    });
    (ok && seen == 15 && md.fps_den != 0).then_some(md)
}

impl Message for Caps {
    const TYPE: u16 = msg::CAPS;
    fn encode(&self, w: &mut TlvWriter, minor: u8) {
        for c in &self.codecs {
            let l = w.begin_list(1);
            w.u8(1, c.id);
            w.u8(2, c.profile);
            w.u8(3, c.level);
            w.end_list(l);
        }
        for md in &self.modes {
            encode_mode(w, 2, md);
        }
        w.u32(3, self.max_bitrate_kbps);
        w.u64(4, self.controls);
        for lens in &self.lenses {
            let l = w.begin_list(5);
            w.u8(1, lens.id);
            w.u8(2, lens.facing);
            w.str(3, &lens.label);
            if minor >= 1 {
                for md in &lens.modes {
                    encode_mode(w, 4, md);
                }
                if lens.zoom_max != 0 {
                    let z = w.begin_list(5);
                    w.u16(1, lens.zoom_min);
                    w.u16(2, lens.zoom_max);
                    if lens.zoom_base != 0 {
                        w.u16(3, lens.zoom_base);
                    }
                    w.end_list(z);
                }
            }
            w.end_list(l);
        }
        if self.has_exposure_range {
            let l = w.begin_list(6);
            w.i32(1, self.exposure_min);
            w.i32(2, self.exposure_max);
            w.u32(3, self.exposure_step_milli);
            w.end_list(l);
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = Caps::default();
        let ok = each(v, |tag, val| match tag {
            1 => {
                let mut c = Codec::default();
                let ok = each(val, |t, x| match t {
                    1 => set(&mut c.id, x),
                    2 => set(&mut c.profile, x),
                    3 => set(&mut c.level, x),
                    _ => true,
                });
                m.codecs.push(c);
                ok
            }
            2 => decode_mode(val).map(|md| m.modes.push(md)).is_some(),
            3 => set(&mut m.max_bitrate_kbps, val),
            4 => set(&mut m.controls, val),
            5 => {
                let mut l = Lens::default();
                let ok = each(val, |t, x| match t {
                    1 => set(&mut l.id, x),
                    2 => set(&mut l.facing, x),
                    3 => {
                        l.label = x.to_vec();
                        true
                    }
                    4 => decode_mode(x).map(|md| l.modes.push(md)).is_some(),
                    5 => each(x, |zt, zx| match zt {
                        1 => set(&mut l.zoom_min, zx),
                        2 => set(&mut l.zoom_max, zx),
                        3 => set(&mut l.zoom_base, zx),
                        _ => true,
                    }),
                    _ => true,
                });
                m.lenses.push(l);
                ok
            }
            6 => {
                m.has_exposure_range = true;
                each(val, |t, x| match t {
                    1 => set(&mut m.exposure_min, x),
                    2 => set(&mut m.exposure_max, x),
                    3 => set(&mut m.exposure_step_milli, x),
                    _ => true,
                })
            }
            _ => true,
        });
        ok.then_some(m)
    }
}

fn encode_settings(w: &mut TlvWriter, s: &lenny_stream_settings) {
    w.u8(1, s.codec);
    w.u16(2, s.mode.width);
    w.u16(3, s.mode.height);
    w.u16(4, s.mode.fps_num);
    w.u16(5, s.mode.fps_den);
    w.u32(6, s.bitrate_kbps);
    if s.has_lens != 0 {
        w.u8(7, s.lens_id);
    }
}

fn decode_settings(v: &[u8]) -> Option<lenny_stream_settings> {
    let mut s = lenny_stream_settings::default();
    let mut seen = 0;
    let ok = each(v, |tag, val| match tag {
        1 => {
            seen |= 1;
            set(&mut s.codec, val)
        }
        2 => {
            seen |= 2;
            set(&mut s.mode.width, val)
        }
        3 => {
            seen |= 4;
            set(&mut s.mode.height, val)
        }
        4 => {
            seen |= 8;
            set(&mut s.mode.fps_num, val)
        }
        5 => {
            seen |= 16;
            set(&mut s.mode.fps_den, val)
        }
        6 => set(&mut s.bitrate_kbps, val),
        7 => {
            s.has_lens = 1;
            set(&mut s.lens_id, val)
        }
        _ => true,
    });
    (ok && seen == 31 && s.mode.fps_den != 0).then_some(s)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapsSelect(pub lenny_stream_settings);

impl Message for CapsSelect {
    const TYPE: u16 = msg::CAPS_SELECT;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        encode_settings(w, &self.0)
    }
    fn decode(v: &[u8]) -> Option<Self> {
        decode_settings(v).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStart(pub lenny_stream_settings);

impl Message for StreamStart {
    const TYPE: u16 = msg::STREAM_START;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        encode_settings(w, &self.0)
    }
    fn decode(v: &[u8]) -> Option<Self> {
        decode_settings(v).map(Self)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamStatus {
    pub state: u8,
    pub reason: Str,
}

impl Message for StreamStatus {
    const TYPE: u16 = msg::STREAM_STATUS;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u8(1, self.state);
        if !self.reason.is_empty() {
            w.str(2, &self.reason);
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = StreamStatus::default();
        let mut seen = false;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen = true;
                set(&mut m.state, val)
            }
            2 => {
                m.reason = val.to_vec();
                true
            }
            _ => true,
        });
        (ok && seen).then_some(m)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoConfig {
    pub codec: u8,
    pub config: Vec<u8>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self { codec: LENNY_CODEC_H264, config: vec![] }
    }
}

impl Message for VideoConfig {
    const TYPE: u16 = msg::VIDEO_CONFIG;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u8(1, self.codec);
        w.bytes(2, &self.config);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = VideoConfig::default();
        let mut seen = false;
        let ok = each(v, |tag, val| match tag {
            1 => set(&mut m.codec, val),
            2 => {
                seen = true;
                m.config = val.to_vec();
                true
            }
            _ => true,
        });
        (ok && seen).then_some(m)
    }
}

/// VIDEO_FRAME is binary, not TLV (protocol.md §6.8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoFrameMeta {
    pub frame_seq: u32,
    pub pts_us: i64,
    pub orientation: u8,
    pub flags: u8,
}

pub fn encode_video_meta(out: &mut [u8], m: &VideoFrameMeta) {
    out[0..4].copy_from_slice(&m.frame_seq.to_le_bytes());
    out[4..12].copy_from_slice(&m.pts_us.to_le_bytes());
    out[12] = m.orientation;
    out[13] = m.flags;
    out[14] = 0;
    out[15] = 0;
}

pub fn decode_video_meta(payload: &[u8]) -> Option<(VideoFrameMeta, &[u8])> {
    if payload.len() < VIDEO_FRAME_META_SIZE {
        return None;
    }
    let m = VideoFrameMeta {
        frame_seq: u32::from_le_bytes(payload[0..4].try_into().ok()?),
        pts_us: i64::from_le_bytes(payload[4..12].try_into().ok()?),
        orientation: payload[12] & 3,
        flags: payload[13],
    };
    Some((m, &payload[VIDEO_FRAME_META_SIZE..]))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Control(pub lenny_control);

const CTL_FIRST: u16 = LENNY_CTL_KEYFRAME_REQUEST as u16;
const CTL_LAST: u16 = LENNY_CTL_PAN as u16;

impl Message for Control {
    const TYPE: u16 = msg::CONTROL;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        let c = &self.0;
        w.u32(1, c.req_id);
        const FOCUS_AT: u16 = LENNY_CTL_FOCUS_AT as u16;
        const FOCUS_LOCK: u16 = LENNY_CTL_FOCUS_LOCK as u16;
        const EXPOSURE_LOCK: u16 = LENNY_CTL_EXPOSURE_LOCK as u16;
        const WB_LOCK: u16 = LENNY_CTL_WB_LOCK as u16;
        const TORCH: u16 = LENNY_CTL_TORCH as u16;
        const SELECT_LENS: u16 = LENNY_CTL_SELECT_LENS as u16;
        const ZOOM: u16 = LENNY_CTL_ZOOM as u16;
        const EXPOSURE_COMP: u16 = LENNY_CTL_EXPOSURE_COMP as u16;
        const PAN: u16 = LENNY_CTL_PAN as u16;
        match c.cmd {
            FOCUS_AT | PAN => {
                let l = w.begin_list(c.cmd);
                w.u16(1, c.x);
                w.u16(2, c.y);
                w.end_list(l);
            }
            FOCUS_LOCK | EXPOSURE_LOCK | WB_LOCK | TORCH => w.u8(c.cmd, (c.value != 0) as u8),
            SELECT_LENS => w.u8(c.cmd, c.value as u8),
            ZOOM => w.u16(c.cmd, c.value as u16),
            EXPOSURE_COMP => w.i32(c.cmd, c.value),
            _ => w.empty(c.cmd), // KEYFRAME_REQUEST, FOCUS_AUTO, RESET_AUTO
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut c = lenny_control::default();
        let mut has_id = false;
        let ok = each(v, |tag, val| {
            if tag == 1 {
                has_id = true;
                return set(&mut c.req_id, val);
            }
            if !(CTL_FIRST..=CTL_LAST).contains(&tag) || c.cmd != 0 {
                return true;
            }
            c.cmd = tag;
            match tag {
                t if t == LENNY_CTL_FOCUS_AT as u16 || t == LENNY_CTL_PAN as u16 => each(val, |t, x| match t {
                    1 => set(&mut c.x, x),
                    2 => set(&mut c.y, x),
                    _ => true,
                }),
                t if t == LENNY_CTL_FOCUS_LOCK as u16
                    || t == LENNY_CTL_EXPOSURE_LOCK as u16
                    || t == LENNY_CTL_WB_LOCK as u16
                    || t == LENNY_CTL_TORCH as u16
                    || t == LENNY_CTL_SELECT_LENS as u16 =>
                {
                    let mut b = 0u8;
                    let ok = set(&mut b, val);
                    if ok {
                        c.value = b as i32;
                    }
                    ok
                }
                t if t == LENNY_CTL_ZOOM as u16 => {
                    let mut z = 0u16;
                    let ok = set(&mut z, val);
                    if ok {
                        c.value = z as i32;
                    }
                    ok
                }
                t if t == LENNY_CTL_EXPOSURE_COMP as u16 => set(&mut c.value, val),
                _ => true,
            }
        });
        (ok && has_id && c.cmd != 0).then_some(Self(c))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlAck {
    pub req_id: u32,
    pub result: u8,
}

impl Message for ControlAck {
    const TYPE: u16 = msg::CONTROL_ACK;
    fn encode(&self, w: &mut TlvWriter, _minor: u8) {
        w.u32(1, self.req_id);
        w.u8(2, self.result);
    }
    fn decode(v: &[u8]) -> Option<Self> {
        let mut m = ControlAck::default();
        let mut seen = 0;
        let ok = each(v, |tag, val| match tag {
            1 => {
                seen |= 1;
                set(&mut m.req_id, val)
            }
            2 => {
                seen |= 2;
                set(&mut m.result, val)
            }
            _ => true,
        });
        (ok && seen == 3).then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlState(pub lenny_control_state);

impl Message for ControlState {
    const TYPE: u16 = msg::CONTROL_STATE;
    fn encode(&self, w: &mut TlvWriter, minor: u8) {
        let s = &self.0;
        w.u8(1, s.af_mode);
        w.i32(2, s.exposure_comp);
        w.u8(3, s.exposure_lock);
        w.u8(4, s.wb_lock);
        w.u8(5, s.torch);
        w.u8(6, s.lens_id);
        w.u16(7, s.zoom);
        w.u8(8, s.battery);
        w.u8(9, s.charging);
        if minor >= 1 {
            w.u16(10, s.pan_x);
            w.u16(11, s.pan_y);
        }
    }
    fn decode(v: &[u8]) -> Option<Self> {
        // Older phones send no battery (1.0 before battery) or pan (1.0): unknown battery, centered.
        let mut s = lenny_control_state { battery: 255, pan_x: PAN_CENTER, pan_y: PAN_CENTER, ..Default::default() };
        let ok = each(v, |tag, val| match tag {
            1 => set(&mut s.af_mode, val),
            2 => set(&mut s.exposure_comp, val),
            3 => set(&mut s.exposure_lock, val),
            4 => set(&mut s.wb_lock, val),
            5 => set(&mut s.torch, val),
            6 => set(&mut s.lens_id, val),
            7 => set(&mut s.zoom, val),
            8 => set(&mut s.battery, val),
            9 => set(&mut s.charging, val),
            10 => set(&mut s.pan_x, val),
            11 => set(&mut s.pan_y, val),
            _ => true,
        });
        ok.then_some(Self(s))
    }
}

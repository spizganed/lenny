//! C ABI types and constants, mirroring include/lenny/lenny.h one to one (names, field order, layout).
//! cbindgen regenerates a header from this file; `tools/abi_check.sh` diffs it against lenny.h.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_void};

pub const LENNY_ABI_VERSION_MAJOR: u32 = 1;
pub const LENNY_ABI_VERSION_MINOR: u32 = 3;
pub const LENNY_DEFAULT_PORT: u16 = 47474;
pub const LENNY_DEVICE_ID_SIZE: usize = 16;
pub const LENNY_PAIR_TOKEN_SIZE: usize = 16;

// ---- Results ----
pub const LENNY_OK: i32 = 0;
pub const LENNY_E_INVALID_ARG: i32 = -1;
pub const LENNY_E_STATE: i32 = -2;
pub const LENNY_E_IO: i32 = -3;
pub const LENNY_E_INTERNAL: i32 = -4;

/// Session state (reported via on_state). Functions and callbacks carry it as int32_t.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum lenny_state {
    LENNY_STATE_IDLE = 0,
    LENNY_STATE_CONNECTING = 1,
    LENNY_STATE_HANDSHAKE = 2,
    LENNY_STATE_AWAITING_APPROVAL = 3,
    LENNY_STATE_STREAMING = 4,
    LENNY_STATE_RECONNECTING = 5,
    LENNY_STATE_CLOSED = 6,
}
pub use lenny_state::*;

// GOODBYE reasons (protocol.md §6.2), also used as on_state reason.
pub const LENNY_REASON_NORMAL: i32 = 0;
pub const LENNY_REASON_VERSION: i32 = 1;
pub const LENNY_REASON_ROLE: i32 = 2;
pub const LENNY_REASON_PAIR_DENIED: i32 = 3;
pub const LENNY_REASON_BUSY: i32 = 4;
pub const LENNY_REASON_TIMEOUT: i32 = 5;
pub const LENNY_REASON_PROTOCOL_ERROR: i32 = 6;
pub const LENNY_REASON_USER: i32 = 7;
pub const LENNY_REASON_LINK_LOST: i32 = 100;

// PAIR_RESULT codes (protocol.md §6.4).
pub const LENNY_PAIR_OK: u8 = 0;
pub const LENNY_PAIR_UNKNOWN_TOKEN: u8 = 1;
pub const LENNY_PAIR_EXPIRED: u8 = 2;
pub const LENNY_PAIR_ALREADY_USED: u8 = 3;
pub const LENNY_PAIR_DENIED: u8 = 4;

pub const LENNY_PLATFORM_ANDROID: u8 = 1;
pub const LENNY_PLATFORM_IOS: u8 = 2;
pub const LENNY_PLATFORM_WINDOWS: u8 = 3;
pub const LENNY_PLATFORM_LINUX: u8 = 5;
pub const LENNY_CODEC_H264: u8 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lenny_mode {
    pub width: u16,
    pub height: u16,
    pub fps_num: u16,
    pub fps_den: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lenny_stream_settings {
    pub codec: u8,
    pub mode: lenny_mode,
    pub bitrate_kbps: u32,
    pub has_lens: u8,
    pub lens_id: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_lens {
    pub lens_id: u8,
    pub facing: u8,
    pub label: *const c_char,
}

/// Camera controls (protocol.md §6.9).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum lenny_control_cmd {
    LENNY_CTL_KEYFRAME_REQUEST = 10,
    LENNY_CTL_FOCUS_AT = 11,
    LENNY_CTL_FOCUS_LOCK = 12,
    LENNY_CTL_FOCUS_AUTO = 13,
    LENNY_CTL_EXPOSURE_COMP = 14,
    LENNY_CTL_EXPOSURE_LOCK = 15,
    LENNY_CTL_WB_LOCK = 16,
    LENNY_CTL_TORCH = 17,
    LENNY_CTL_SELECT_LENS = 18,
    LENNY_CTL_ZOOM = 19,
    LENNY_CTL_RESET_AUTO = 20,
    LENNY_CTL_PAN = 21,
}
pub use lenny_control_cmd::*;

// CAPS controls bitmask bits.
pub const LENNY_CAP_FOCUS: u32 = 1 << 0;
pub const LENNY_CAP_FOCUS_LOCK: u32 = 1 << 1;
pub const LENNY_CAP_EXPOSURE_COMP: u32 = 1 << 2;
pub const LENNY_CAP_EXPOSURE_LOCK: u32 = 1 << 3;
pub const LENNY_CAP_WB_LOCK: u32 = 1 << 4;
pub const LENNY_CAP_TORCH: u32 = 1 << 5;
pub const LENNY_CAP_LENS: u32 = 1 << 6;
pub const LENNY_CAP_ZOOM: u32 = 1 << 7;
pub const LENNY_CAP_PAN: u32 = 1 << 8;

pub const LENNY_ACK_OK: i32 = 0;
pub const LENNY_ACK_UNSUPPORTED: i32 = 1;
pub const LENNY_ACK_FAILED: i32 = 2;
pub const LENNY_ACK_BUSY: i32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lenny_control {
    pub req_id: u32,
    pub cmd: u16,
    pub x: u16,
    pub y: u16,
    pub value: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lenny_control_state {
    pub af_mode: u8,
    pub exposure_comp: i32,
    pub exposure_lock: u8,
    pub wb_lock: u8,
    pub torch: u8,
    pub lens_id: u8,
    pub zoom: u16,
    pub battery: u8,
    pub charging: u8,
    pub pan_x: u16,
    pub pan_y: u16,
}

// STREAM_STATUS states.
pub const LENNY_STREAM_LIVE: u8 = 0;
pub const LENNY_STREAM_PAUSED: u8 = 1;
pub const LENNY_STREAM_CAMERA_LOST: u8 = 2;
pub const LENNY_STREAM_THERMAL: u8 = 3;

// VIDEO_FRAME flags.
pub const LENNY_FRAME_KEYFRAME: u8 = 1 << 0;
pub const LENNY_FRAME_MIRROR: u8 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_video_frame {
    pub frame_seq: u32,
    pub pts_us: i64,
    pub local_pts_us: i64,
    pub orientation: u8,
    pub flags: u8,
    pub data: *const u8,
    pub size: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct lenny_stats {
    pub rtt_us: i64,
    pub clock_offset_us: i64,
    pub frames: u64,
    pub bytes: u64,
    pub bad_messages: u32,
    pub reconnects: u32,
    pub latency_us: i64,
    pub dropped_frames: u32,
    pub bitrate_kbps: u32,
}

impl Default for lenny_stats {
    fn default() -> Self {
        Self {
            rtt_us: -1,
            clock_offset_us: 0,
            frames: 0,
            bytes: 0,
            bad_messages: 0,
            reconnects: 0,
            latency_us: -1,
            dropped_frames: 0,
            bitrate_kbps: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_identity {
    pub device_id: [u8; LENNY_DEVICE_ID_SIZE],
    pub device_name: *const c_char,
    pub app_version: *const c_char,
    pub platform: u8,
}

/// Opaque session handle (a `Box<Session>` behind the pointer).
pub struct lenny_session {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_sender_config {
    pub identity: lenny_identity,
    pub modes: *const lenny_mode,
    pub mode_count: usize,
    pub max_bitrate_kbps: u32,
    pub controls: u32,
    pub lenses: *const lenny_lens,
    pub lens_count: usize,
    pub exposure_comp_min: i32,
    pub exposure_comp_max: i32,
    pub exposure_comp_step_milli: u32,
    pub lens_caps: *const lenny_lens_caps,
}

/// Per-lens capabilities (ABI 1.3), parallel to lenny_sender_config.lenses.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_lens_caps {
    pub modes: *const lenny_mode,
    pub mode_count: usize,
    pub zoom_min: u16,
    pub zoom_max: u16,
    pub zoom_base: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct lenny_sender_callbacks {
    pub user: *mut c_void,
    pub on_state: Option<unsafe extern "C" fn(user: *mut c_void, state: i32, reason: i32)>,
    pub on_stream_config: Option<
        unsafe extern "C" fn(
            user: *mut c_void,
            requested: *const lenny_stream_settings,
            effective: *mut lenny_stream_settings,
        ),
    >,
    pub on_control: Option<unsafe extern "C" fn(user: *mut c_void, control: *const lenny_control) -> i32>,
    pub on_bitrate: Option<unsafe extern "C" fn(user: *mut c_void, kbps: u32)>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lenny_receiver_config {
    pub identity: lenny_identity,
    pub port: u16,
    pub preferred: lenny_stream_settings,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct lenny_receiver_callbacks {
    pub user: *mut c_void,
    pub on_state: Option<unsafe extern "C" fn(user: *mut c_void, state: i32, reason: i32)>,
    pub on_approval_needed:
        Option<unsafe extern "C" fn(user: *mut c_void, device_id: *const u8, device_name: *const c_char)>,
    pub on_stream_start: Option<unsafe extern "C" fn(user: *mut c_void, effective: *const lenny_stream_settings)>,
    pub on_stream_status: Option<unsafe extern "C" fn(user: *mut c_void, state: u8, reason: *const c_char)>,
    pub on_video_config: Option<unsafe extern "C" fn(user: *mut c_void, data: *const u8, size: usize)>,
    pub on_video_frame: Option<unsafe extern "C" fn(user: *mut c_void, frame: *const lenny_video_frame)>,
    pub on_control_state: Option<unsafe extern "C" fn(user: *mut c_void, state: *const lenny_control_state)>,
    pub on_control_ack: Option<unsafe extern "C" fn(user: *mut c_void, req_id: u32, result: u8)>,
}

/// UI events (see lenny_session_set_event_listener).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum lenny_event {
    LENNY_EVENT_STATE = 1,
    LENNY_EVENT_APPROVAL_NEEDED = 2,
    LENNY_EVENT_STREAM_START = 3,
    LENNY_EVENT_STREAM_STATUS = 4,
    LENNY_EVENT_CONTROL_STATE = 5,
    LENNY_EVENT_CONTROL_ACK = 6,
}
pub use lenny_event::*;

pub const LENNY_MAX_PEER_LENSES: usize = 8;
pub const LENNY_MAX_PEER_MODES: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct lenny_peer_info {
    pub device_id: [u8; LENNY_DEVICE_ID_SIZE],
    pub name: [c_char; 64],
    pub platform: u8,
    pub controls: u32,
    pub lens_count: u8,
    pub lens_ids: [u8; LENNY_MAX_PEER_LENSES],
    pub lens_facing: [u8; LENNY_MAX_PEER_LENSES],
    pub lens_labels: [[c_char; 32]; LENNY_MAX_PEER_LENSES],
    pub exposure_min: i32,
    pub exposure_max: i32,
    pub exposure_step_milli: u32,
    pub mode_count: u8,
    pub modes: [lenny_mode; LENNY_MAX_PEER_MODES],
}

impl Default for lenny_peer_info {
    fn default() -> Self {
        // SAFETY: plain integers and arrays of integers; all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

pub(crate) type lenny_event_fn = Option<unsafe extern "C" fn(user: *mut c_void, event: i32, a: i32, b: i32)>;

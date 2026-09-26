//! C ABI over `Session` (include/lenny/lenny.h). Nothing may unwind past this file (architecture.md §4.1):
//! every export catches panics and returns LENNY_E_INTERNAL (or NULL / a neutral value).

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::abi::*;
use crate::session::{ReceiverConfig, Role, SenderConfig, Session};
use crate::timing::now_us;
use crate::wire;

fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(LENNY_E_INTERNAL)
}

unsafe fn cstr(p: *const c_char) -> Vec<u8> {
    if p.is_null() {
        vec![]
    } else {
        CStr::from_ptr(p).to_bytes().to_vec()
    }
}

unsafe fn session<'a>(s: *mut lenny_session) -> Option<&'a Session> {
    (s as *const Session).as_ref()
}

fn into_handle(s: Session) -> *mut lenny_session {
    Box::into_raw(Box::new(s)) as *mut lenny_session
}

#[no_mangle]
pub extern "C" fn lenny_abi_version() -> u32 {
    (LENNY_ABI_VERSION_MAJOR << 16) | LENNY_ABI_VERSION_MINOR
}

#[no_mangle]
pub extern "C" fn lenny_now_us() -> i64 {
    now_us()
}

// ---- Sender ----

/// # Safety
/// Pointers must be NULL or valid for the documented sizes (lenny.h).
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_create(
    config: *const lenny_sender_config,
    callbacks: *const lenny_sender_callbacks,
) -> *mut lenny_session {
    let Some(c) = config.as_ref() else { return std::ptr::null_mut() };
    if (c.mode_count != 0 && c.modes.is_null()) || (c.lens_count != 0 && c.lenses.is_null()) {
        return std::ptr::null_mut();
    }
    catch_unwind(AssertUnwindSafe(|| {
        let modes = if c.mode_count != 0 { std::slice::from_raw_parts(c.modes, c.mode_count).to_vec() } else { vec![] };
        let lenses = if c.lens_count != 0 { std::slice::from_raw_parts(c.lenses, c.lens_count) } else { &[] };
        let cfg = SenderConfig {
            identity: c.identity,
            device_name: cstr(c.identity.device_name),
            app_version: cstr(c.identity.app_version),
            modes,
            max_bitrate_kbps: c.max_bitrate_kbps,
            controls: c.controls,
            lenses: lenses
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    // ABI 1.3: optional per-lens caps, parallel to `lenses`.
                    let lc = if c.lens_caps.is_null() { None } else { Some(&*c.lens_caps.add(i)) };
                    let modes = match lc {
                        Some(lc) if lc.mode_count != 0 && !lc.modes.is_null() => {
                            std::slice::from_raw_parts(lc.modes, lc.mode_count).to_vec()
                        }
                        _ => vec![],
                    };
                    wire::Lens {
                        id: l.lens_id,
                        facing: l.facing,
                        label: cstr(l.label),
                        modes,
                        zoom_min: lc.map_or(0, |lc| lc.zoom_min),
                        zoom_max: lc.map_or(0, |lc| lc.zoom_max),
                        zoom_base: lc.map_or(0, |lc| lc.zoom_base),
                    }
                })
                .collect(),
            exposure_comp_min: c.exposure_comp_min,
            exposure_comp_max: c.exposure_comp_max,
            exposure_comp_step_milli: c.exposure_comp_step_milli,
        };
        let cb = callbacks.as_ref().copied().unwrap_or(std::mem::zeroed());
        into_handle(Session::new_sender(cfg, cb))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `s` from lenny_*_create; `host` NUL-terminated; `pair_token` NULL or 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_connect(
    s: *mut lenny_session,
    host: *const c_char,
    port: u16,
    pair_token: *const u8,
) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    if host.is_null() {
        return LENNY_E_INVALID_ARG;
    }
    guard(|| {
        let host = String::from_utf8_lossy(CStr::from_ptr(host).to_bytes()).into_owned();
        let token = (pair_token as *const [u8; LENNY_PAIR_TOKEN_SIZE]).as_ref();
        s.connect(&host, port, token)
    })
}

/// # Safety
/// `data` valid for `size` bytes.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_send_video_config(s: *mut lenny_session, data: *const u8, size: usize) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    if s.role() != Role::Sender {
        return LENNY_E_STATE;
    }
    if data.is_null() {
        return LENNY_E_INVALID_ARG;
    }
    guard(|| s.send_video_config(std::slice::from_raw_parts(data, size)))
}

/// # Safety
/// `data` valid for `size` bytes.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_send_video_frame(
    s: *mut lenny_session,
    data: *const u8,
    size: usize,
    pts_us: i64,
    orientation: u8,
    flags: u8,
) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    if s.role() != Role::Sender {
        return LENNY_E_STATE;
    }
    if data.is_null() {
        return LENNY_E_INVALID_ARG;
    }
    guard(|| s.send_video_frame(std::slice::from_raw_parts(data, size), pts_us, orientation, flags))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_update_stream(
    s: *mut lenny_session,
    effective: *const lenny_stream_settings,
) -> i32 {
    let (Some(s), Some(e)) = (session(s), effective.as_ref()) else { return LENNY_E_INVALID_ARG };
    guard(|| s.update_stream(e))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_send_control_state(
    s: *mut lenny_session,
    state: *const lenny_control_state,
) -> i32 {
    let (Some(s), Some(st)) = (session(s), state.as_ref()) else { return LENNY_E_INVALID_ARG };
    guard(|| s.send_control_state(st))
}

/// # Safety
/// `reason` NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn lenny_sender_send_stream_status(
    s: *mut lenny_session,
    state: u8,
    reason: *const c_char,
) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    guard(|| s.send_stream_status(state, &cstr(reason)))
}

// ---- Receiver ----

/// # Safety
/// Pointers NULL or valid.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_create(
    config: *const lenny_receiver_config,
    callbacks: *const lenny_receiver_callbacks,
) -> *mut lenny_session {
    let Some(c) = config.as_ref() else { return std::ptr::null_mut() };
    catch_unwind(AssertUnwindSafe(|| {
        let cfg = ReceiverConfig {
            identity: c.identity,
            device_name: cstr(c.identity.device_name),
            app_version: cstr(c.identity.app_version),
            port: c.port,
            preferred: c.preferred,
        };
        let cb = callbacks.as_ref().copied().unwrap_or(std::mem::zeroed());
        into_handle(Session::new_receiver(cfg, cb))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_start(s: *mut lenny_session) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    guard(|| s.start())
}

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_port(s: *mut lenny_session) -> u16 {
    session(s).map_or(0, |s| s.port())
}

/// # Safety
/// `out` valid for 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_new_pair_token(s: *mut lenny_session, ttl_ms: u32, out: *mut u8) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    if out.is_null() || s.role() != Role::Receiver {
        return LENNY_E_INVALID_ARG;
    }
    guard(|| {
        let t = s.new_pair_token(ttl_ms);
        std::ptr::copy_nonoverlapping(t.as_ptr(), out, t.len());
        LENNY_OK
    })
}

/// # Safety
/// `device_id` valid for 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_trust_device(s: *mut lenny_session, device_id: *const u8) -> i32 {
    let (Some(s), Some(id)) = (session(s), (device_id as *const wire::DeviceId).as_ref()) else {
        return LENNY_E_INVALID_ARG;
    };
    guard(|| {
        s.trust(id);
        LENNY_OK
    })
}

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_approve(s: *mut lenny_session, accept: i32) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    guard(|| s.approve(accept != 0))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_select_stream(
    s: *mut lenny_session,
    preferred: *const lenny_stream_settings,
) -> i32 {
    let (Some(s), Some(p)) = (session(s), preferred.as_ref()) else { return LENNY_E_INVALID_ARG };
    guard(|| s.select_stream(p))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_receiver_send_control(s: *mut lenny_session, control: *mut lenny_control) -> i32 {
    let (Some(s), Some(c)) = (session(s), control.as_mut()) else { return LENNY_E_INVALID_ARG };
    guard(|| s.send_control(c))
}

// ---- Common ----

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_set_event_listener(
    s: *mut lenny_session,
    f: Option<unsafe extern "C" fn(user: *mut c_void, event: i32, a: i32, b: i32)>,
    user: *mut c_void,
) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    guard(|| {
        s.set_event_listener(f, user);
        LENNY_OK
    })
}

fn out_or_state<T>(r: (bool, T), out: &mut T) -> i32 {
    *out = r.1;
    if r.0 {
        LENNY_OK
    } else {
        LENNY_E_STATE
    }
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_peer(s: *mut lenny_session, out: *mut lenny_peer_info) -> i32 {
    let (Some(s), Some(o)) = (session(s), out.as_mut()) else { return LENNY_E_INVALID_ARG };
    guard(|| out_or_state(s.peer(), o))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_stream_settings(s: *mut lenny_session, out: *mut lenny_stream_settings) -> i32 {
    let (Some(s), Some(o)) = (session(s), out.as_mut()) else { return LENNY_E_INVALID_ARG };
    guard(|| out_or_state(s.stream_settings(), o))
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_control_state(s: *mut lenny_session, out: *mut lenny_control_state) -> i32 {
    let (Some(s), Some(o)) = (session(s), out.as_mut()) else { return LENNY_E_INVALID_ARG };
    guard(|| out_or_state(s.control_state(), o))
}

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_state(s: *mut lenny_session) -> i32 {
    session(s).map_or(LENNY_STATE_CLOSED as i32, |s| s.state())
}

/// # Safety
/// Valid pointers.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_get_stats(s: *mut lenny_session, out: *mut lenny_stats) -> i32 {
    let (Some(s), Some(o)) = (session(s), out.as_mut()) else { return LENNY_E_INVALID_ARG };
    guard(|| {
        *o = s.stats();
        LENNY_OK
    })
}

/// # Safety
/// `s` NULL or from lenny_*_create.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_disconnect(s: *mut lenny_session) -> i32 {
    let Some(s) = session(s) else { return LENNY_E_INVALID_ARG };
    guard(|| {
        s.disconnect();
        LENNY_OK
    })
}

/// # Safety
/// `s` NULL or from lenny_*_create, not used afterwards, not called from inside a callback.
#[no_mangle]
pub unsafe extern "C" fn lenny_session_destroy(s: *mut lenny_session) {
    if !s.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(s as *mut Session))));
    }
}

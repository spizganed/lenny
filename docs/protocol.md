# Lenny Wire Protocol

Protocol version: **1.1**. Status: **for review**.
Test vectors live in `/protocol/vectors/` (added in M1). This file is the spec.

## 1. Principles
- One TCP connection per session carries everything: control, video, keepalive.
- Every message = fixed header (type + length + version) + payload.
- **Unknown message types are skipped** (length is always known). Unknown TLV fields are skipped.
  Neither is ever an error.
- Little-endian everywhere. No padding. No floats on the wire, only fixed-point.
- Additive evolution: new message types or new TLV tags = minor bump. Changing the meaning
  of an existing field or header = major bump.

## 2. Transport
- Default port **47474/TCP**. The receiver listens, and the sender connects (all transports: Wi-Fi,
  `adb reverse`, USB tethering).
- `TCP_NODELAY` on both ends. The sender's socket send buffer is kept small (~256 KiB) so backpressure
  shows up quickly (see §9).
- Discovery, **47474/UDP**: the sender sends the ASCII probe `LENNY?1` (broadcast, or unicast to the
  hosts from a QR code). A receiver answers to the probe's source with `lenny://c?v=1&p=<tcp port>&n=<name>`
  (the §6.4 link without `h` and `t`); the answer's source address is the host. Anything else on the
  port is ignored.

## 3. Framing

```
offset size  field
0      2     magic         0x4C 0x59  ("LY")
2      1     ver_major     1
3      1     ver_minor     0
4      2     type          u16, see §5
6      2     flags         u16, per-type; bit 15 reserved = 0
8      4     length        u32, payload length in bytes (header excluded)
12     …     payload
```

Header = 12 bytes.

Receive rules:
1. Bad magic → close connection (stream is desynced; there's no safe resync on TCP).
2. `ver_major` ≠ negotiated major → close with `GOODBYE(reason=VERSION)` if possible.
3. `length` > limit → close. Limits: 4 MiB for `VIDEO_FRAME`, 64 KiB for everything else.
4. Unknown `type` → read and discard `length` bytes, continue.
5. `ver_minor` higher than ours → accepted. Parse what we know and skip the rest.

Before HELLO is exchanged, the header's version is the sender's own max version. After HELLO, both
sides use the negotiated version (§4).

## 4. Versioning
- Each side sends its `(major, minor)` in HELLO.
- Negotiated major must match exactly, otherwise `GOODBYE(VERSION)`.
- Negotiated minor = `min(a.minor, b.minor)`. Neither side sends message types or fields newer
  than the negotiated minor.

## 5. Message types

| Type | Name | Dir | Since |
|---|---|---|---|
| 0x0001 | HELLO | both | 1.0 |
| 0x0002 | GOODBYE | both | 1.0 |
| 0x0003 | PING | both | 1.0 |
| 0x0004 | PONG | both | 1.0 |
| 0x0010 | PAIR_REQUEST | S→R | 1.0 |
| 0x0011 | PAIR_RESULT | R→S | 1.0 |
| 0x0020 | CAPS | S→R | 1.0 |
| 0x0021 | CAPS_SELECT | R→S | 1.0 |
| 0x0022 | STREAM_START | S→R | 1.0 |
| 0x0023 | STREAM_STATUS | S→R | 1.0 |
| 0x0030 | VIDEO_CONFIG | S→R | 1.0 |
| 0x0031 | VIDEO_FRAME | S→R | 1.0 |
| 0x0040 | CONTROL | R→S | 1.0 |
| 0x0041 | CONTROL_STATE | S→R | 1.0 |
| 0x0042 | CONTROL_ACK | S→R | 1.0 |
| 0x0050 | STATS | both | 1.0 |
| 0x8000–0xFFFF | vendor / experimental | — | never assigned by spec |

S = sender (phone), R = receiver (desktop).

## 6. Payload encoding: TLV

All control-plane payloads (everything except `VIDEO_FRAME` bodies) are a sequence of TLV:

```
tag   u16
len   u16
value len bytes
```

Value types (implied by tag): `u8/u16/u32/u64/i32/i64` (LE, exact size), `str` (UTF-8, no NUL),
`bytes`, `list` (nested TLV sequence). A tag may repeat when the field is a list item.
Unknown tags are skipped. Missing required tags → message ignored and a `STATS` warning counter increments
(the connection doesn't drop).

### 6.1 HELLO (0x0001)
| Tag | Name | Type | Req |
|---|---|---|---|
| 1 | proto_major | u8 | ✓ |
| 2 | proto_minor | u8 | ✓ |
| 3 | role | u8 (1=sender, 2=receiver) | ✓ |
| 4 | device_id | bytes[16] (random, persisted per install) | ✓ |
| 5 | device_name | str | ✓ |
| 6 | app_version | str | |
| 7 | platform | u8 (1=android, 2=ios, 3=windows, 4=reserved (was macOS, dropped), 5=linux) | |
| 8 | pairing_required | u8 bool (receiver only) | |
| 9 | features | u64 bitmask (bit0 = TLS_UPGRADE reserved, others reserved) | |

Sender sends HELLO first. Receiver replies with HELLO. Same role on both sides → `GOODBYE(ROLE)`.

### 6.2 GOODBYE (0x0002)
Tag 1 `reason` u16: 0 NORMAL, 1 VERSION, 2 ROLE, 3 PAIR_DENIED, 4 BUSY (another phone is streaming),
5 TIMEOUT, 6 PROTOCOL_ERROR, 7 USER. Tag 2 `detail` str (optional). The side sending it closes afterwards.
The sender (phone) doesn't try to reconnect after reasons 1, 2, 3 or 7. A receiver that is quitting or
restarting sends 0 (NORMAL), so the phone keeps retrying and resumes by itself; 7 (USER) means the desktop
user pressed disconnect.

### 6.3 PING / PONG (0x0003 / 0x0004) — keepalive + clock sync
PING: tag 1 `seq` u32, tag 2 `t1` i64 (sender-of-ping monotonic µs).
PONG: tag 1 `seq`, tag 2 `t1` (echo), tag 3 `t2` i64 (receipt, responder clock), tag 4 `t3` i64 (send, responder clock).
Both sides PING every 1000 ms. No bytes received for 3000 ms → link dead → close and reconnect.
Clock offset/RTT: NTP formulas with `t4` = local receipt time. Keep a min-RTT filter over the last 16 samples.
The receiver uses the offset to turn sender PTS into local time (latency stats + jitter buffer).

### 6.4 PAIR_REQUEST / PAIR_RESULT (0x0010 / 0x0011) — QR pairing
Sent right after HELLO, before CAPS, when the sender got here via a QR code.

PAIR_REQUEST: tag 1 `token` bytes[16] (from QR `t=`, base64url-decoded).
PAIR_RESULT: tag 1 `result` u8: 0 OK, 1 UNKNOWN_TOKEN, 2 EXPIRED, 3 ALREADY_USED, 4 DENIED.

Receiver rules:
- Tokens: 128-bit CSPRNG, single-use, TTL 90 s, memory only. Comparison is constant-time.
- On OK the receiver marks the token used and trusts `device_id` for this session (and remembers it as
  "known" so future non-QR connects skip the accept prompt).
- On failure: send PAIR_RESULT, then `GOODBYE(PAIR_DENIED)`. The phone falls back to manual entry and
  shows the reason.
- No PAIR_REQUEST: the receiver applies its policy. Known device → proceed. Unknown → the user
  sees an accept/deny prompt, and if it's not accepted within 30 s the receiver sends `GOODBYE(PAIR_DENIED)`.

The token isn't a new trust boundary. It's proof of recent physical line-of-sight to the screen, used
instead of the accept prompt. The session then continues with the normal CAPS negotiation.

QR payload (not a wire message, documented here so both sides agree):
```
lenny://c?v=1&h=192.168.1.20,192.168.42.129&p=47474&t=<22-char base64url>&n=<urlencoded pc name>
```
`v` = protocol major. The phone rejects an unknown `v` with a clear error ("Update Lenny").

### 6.5 CAPS (0x0020) — sender capabilities
| Tag | Name | Type |
|---|---|---|
| 1 | codec | list, repeated: {1 codec_id u8 (1=H264, 2=HEVC reserved), 2 profile u8, 3 level u8} |
| 2 | mode | list, repeated: {1 width u16, 2 height u16, 3 fps_num u16, 4 fps_den u16} |
| 3 | max_bitrate_kbps | u32 |
| 4 | controls | u64 bitmask (§6.9) |
| 5 | lenses | list, repeated: {1 lens_id u8, 2 facing u8 (0 back, 1 front, 2 external), 3 label str, 4 mode (1.1), 5 zoom_range (1.1)} |
| 6 | exposure_comp_range | list {1 min i32, 2 max i32, 3 step_milli u32} (EV × 1000) |

Lens sub-fields since 1.1 (a 1.0 receiver skips them):
- `4 mode`: repeated, same shape as tag 2. The modes **this lens** can stream. Phones differ per lens (the front camera
  often tops out below the back one), so a receiver offers only the selected lens's modes and never a mode that would
  fall back when streaming starts. No tag 4 = the CAPS-level modes (tag 2), which stay the default lens's modes for 1.0
  receivers.
- `5 zoom_range`: list {1 min u16, 2 max u16, 3 base u16}. min/max: ratio × 100 relative to this lens, the range
  CONTROL `zoom` accepts (min may be below 100 on a logical multi-camera, where zooming out selects the ultrawide
  sensor). base: the lens's own ratio × 100 on its camera (200 for a 2× sensor of a logical camera; absent = 100).
  Pan works over the camera's field of view at ratio 1, so a receiver needs it to map drags (below).

### 6.6 CAPS_SELECT (0x0021) — receiver's choice
Tag 1 codec_id u8, 2 width u16, 3 height u16, 4 fps_num u16, 5 fps_den u16, 6 bitrate_kbps u32,
7 lens_id u8 (optional). May be re-sent mid-stream to change settings. The sender then sends
`VIDEO_CONFIG` + keyframe with the new settings. If a value isn't in CAPS, the sender clamps to
the closest supported mode and reports it in `STREAM_START`.

### 6.7 STREAM_START (0x0022) / STREAM_STATUS (0x0023)
STREAM_START: the effective settings (same tags as CAPS_SELECT). It's sent before the first VIDEO_CONFIG.
STREAM_STATUS: tag 1 `state` u8 (0 LIVE, 1 PAUSED, 2 CAMERA_LOST, 3 THERMAL_THROTTLE), tag 2 `reason` str.
The receiver shows the placeholder + reason for anything other than LIVE, without disconnecting.

### 6.8 VIDEO_CONFIG (0x0030) / VIDEO_FRAME (0x0031)
VIDEO_CONFIG payload: TLV, 1 codec_id u8, 2 `config` bytes (H.264: Annex-B SPS+PPS).
Sent after STREAM_START, on every settings change, and **before every keyframe**, so a receiver joining or
recovering never needs state from earlier. (The core caches the last config and resends it before each
keyframe itself, so platforms only hand it over once.)

VIDEO_FRAME is binary (not TLV), for speed:
```
offset size field
0      4    frame_seq      u32, +1 per frame, wraps
4      8    pts_us         i64, sender monotonic µs at capture (from the camera timestamp)
12     1    orientation    u8: 0, 1, 2, 3 = 0°, 90°, 180°, 270° clockwise to upright
13     1    frame_flags    u8: bit0 KEYFRAME, bit1 MIRROR (front camera), others 0
14     2    reserved       u16 = 0
16     …    data           Annex-B H.264 access unit (start codes included)
```
Header flags (§3) for VIDEO_FRAME: none defined in 1.0.

Receiver rules: a gap in `frame_seq` (can't happen on TCP unless the sender dropped a frame deliberately) is
fine for P-frames the encoder already skipped. If the decoder reports corruption → send
`CONTROL(keyframe_request)` and show the last good frame until the keyframe arrives.

### 6.9 CONTROL (0x0040) / CONTROL_STATE (0x0041) / CONTROL_ACK (0x0042)
CONTROL payload: tag 1 `req_id` u32, then exactly one command tag:

| Tag | Command | Value | Controls bit |
|---|---|---|---|
| 10 | keyframe_request | (empty) | always |
| 11 | focus_at | {1 x u16, 2 y u16} normalized 0–65535, upright image coords. Focuses there and **holds** (Manual mode) until focus_auto or reset_auto | 0 |
| 12 | focus_lock | u8 bool | 1 |
| 13 | focus_auto | (empty) — back to continuous AF | 0 |
| 14 | exposure_comp | i32 EV×1000 (0 = auto, no bias) | 2 |
| 15 | exposure_lock | u8 bool | 3 |
| 16 | wb_lock | u8 bool | 4 |
| 17 | torch | u8 bool | 5 |
| 18 | select_lens | u8 lens_id | 6 |
| 19 | zoom | u16 ratio×100 | 7 |
| 20 | reset_auto | (empty) — focus, exposure and white balance back to Auto (continuous AF, EV 0, no locks). Lens, zoom, pan and torch stay | always |
| 21 | pan (1.1) | {1 x u16, 2 y u16}: where the zoomed crop sits, 0–65535 across the pannable range per axis, 32768 = centred, upright image coords | 8 |

**Pan (1.1).** Zoom and pan happen on the camera, not on finished frames: zoom is `CONTROL_ZOOM_RATIO` (so a logical
multi-camera switches sensors itself), pan moves `SCALER_CROP_REGION` over the lens's full field of view. At zoom Z
= base × zoom (both from CAPS/CONTROL_STATE, as ratios) the visible crop is 1/Z of the camera's field of view per
axis; pan 0 puts it against the left (top) edge, 65535 against the right (bottom) edge. At Z ≤ 1 there's nothing to
pan and it's ignored. A receiver that drags the picture by d (fraction of the visible width) changes pan by
−d · (1/Z) / (1 − 1/Z) · 65535. Sent only when
the negotiated minor is ≥ 1.

CONTROL_ACK: tag 1 `req_id`, tag 2 `result` u8 (0 OK, 1 UNSUPPORTED, 2 FAILED, 3 BUSY).
CONTROL_STATE: full current state, sent after STREAM_START, after every change, and on reconnect:
1 af_mode u8 (0 continuous, 1 locked, 2 focusing), 2 exposure_comp i32, 3 exposure_lock u8,
4 wb_lock u8, 5 torch u8, 6 lens_id u8, 7 zoom u16, 8 battery u8 (percent, 255 unknown; absent = unknown),
9 charging u8 bool, 10 pan_x u16 (1.1), 11 pan_y u16 (1.1; both 32768 when absent). The phone also resends it every
60 s so the battery level stays current.

**Default state is Auto.** Every connect and every reconnect starts with continuous AF, AE on, AWB auto, EV 0, torch
off, zoom 1×, pan centred, whichever side set them before. The UIs show two modes: **Auto** (nothing manual) and
**Manual** (tap-to-focus, which holds, plus exposure compensation and exposure lock); going back to Auto sends
`reset_auto`. A settings change mid-stream (CAPS_SELECT) keeps the current controls.

### 6.10 STATS (0x0050)
Optional, every 1 s. Tag 1 `encoded_fps_x100` u32, 2 `bitrate_kbps` u32, 3 `dropped_frames` u32,
4 `rtt_us` u32, 5 `queue_ms` u32, 6 `temperature_c_x10` i32 (sender), 7 `bad_messages` u32.
Informational only; never required for correctness.

## 7. Session state machine

```
            connect
 IDLE ─────────────► HELLO_WAIT ── HELLO ok ──► [PAIRING] ── ok ──► CAPS_WAIT
   ▲                    │                           │                  │ CAPS_SELECT
   │                    │ timeout 5s / bad          │ denied           ▼
   │                    ▼                           ▼              STREAMING ◄─┐
   └──── backoff ◄── CLOSED ◄──────────────────────────────────────── │  CAPS_SELECT
                        ▲                                             │ (reconfig)─┘
                        └──── 3s silence / GOODBYE / socket error ────┘
```

Timeouts: HELLO 5 s, PAIR (incl. user prompt) 30 s, CAPS 5 s. Reconnect backoff (sender) 250 ms → 500 ms →
1 s → 2 s, then 2 s steady. The receiver never initiates.

One streaming sender per receiver in 1.0. A second sender gets `GOODBYE(BUSY)`.

## 8. Timing and jitter
- `pts_us` is the camera capture timestamp (Android `SENSOR_TIMESTAMP` → monotonic µs), so latency stats
  measure glass-to-glass minus display.
- The receiver converts to local time with the §6.3 offset, then puts frames into the jitter buffer. The
  target delay adapts between 0 and 60 ms based on the observed arrival jitter (p95 over 2 s).
  Late frames (past target + 100 ms) are decoded but not displayed, except keyframes.

## 9. Backpressure (sender)
The sender queues outgoing video and writes it from its own thread, so a slow network never blocks the encoder.
When the frames waiting in that queue span more than 250 ms of capture time, the sender drops the backlog (keeping only
the newest queued keyframe), drops new frames until the next keyframe, requests one from its encoder, and lowers the
bitrate one step (−20%, min 1 Mbps; +10% back per 5 s without congestion, up to the negotiated rate). The kernel send
buffer is capped at 128 KiB so the backlog stays visible to this check. Old video is never queued, because freshness
matters more than completeness.

## 10. Examples (hex)
HELLO from a sender, protocol 1.0, minimal:
```
4C 59 01 00  01 00  00 00  2A 00 00 00         header, type=HELLO, len=42
01 00 01 00 01                                  proto_major=1
02 00 01 00 00                                  proto_minor=0
03 00 01 00 01                                  role=sender
04 00 10 00 <16 bytes device_id>                device_id
05 00 03 00 50 69 78                            device_name="Pix"
```
(5 + 5 + 5 + 20 + 7 = 42.) Binary test vectors with expected decodes ship in `/protocol/vectors/` in M1.

## 11. Change log
- 1.0 (draft) — initial. CONTROL_STATE battery/charging (tags 8, 9) were added later without a version bump; absent
  = unknown.
- 1.1 — CAPS lens entries carry their own modes and zoom range; CONTROL `pan` (21) and controls bit 8; CONTROL_STATE
  pan_x/pan_y (10, 11); focus_at holds until Auto; controls reset to Auto on every reconnect. Test vectors
  `control_pan.hex`, `caps_lens_1_1.hex`.

# Lenny — Architecture

Status: living document. Phase A (core port to Rust) is done; the Phase B desktop receiver exists for Linux
(§7a) and replaces the Flutter/C++ Windows receiver described in §7 once its Windows virtual-camera backends exist.

Lenny turns a phone into a webcam. **Sender** = phone app (Android now, iOS later).
**Receiver** = desktop app (Windows now, Linux later; macOS is out of scope) that decodes the stream
and feeds an OS-level **virtual camera** seen by Zoom, Teams, Discord, browsers, OBS.

## 1. Goals and non-goals

Goals (phase 1):
- Android → Windows 10/11 over Wi-Fi, USB-ADB, USB-tethering; one TCP code path.
- < 150 ms glass-to-glass on LAN (M3).
- Auto focus/exposure/WB by default; manual controls are opt-in overlays.
- The virtual camera never crashes the host app and never shows black/frozen video.
- Adding iOS/Linux = new platform modules only. Core and protocol stay as they are.

Non-goals (phase 1): audio, multiple phones at once, internet/relay streaming,
encryption (deferred, see §9), macOS receiver (dropped, see §11), mascot art.

## 2. Component map

```
 SENDER (Android)                                   RECEIVER (Windows)
 ┌──────────────────────────────┐                   ┌──────────────────────────────────────┐
 │ Flutter UI (/app)            │                   │ Flutter UI (/app)                    │
 │  Riverpod providers          │                   │  Riverpod providers                  │
 │  Services (Dart) ── FFI ─┐   │                   │  Services (Dart) ── FFI ──┐          │
 │        │ method channel  │   │                   │        │ method channel   │          │
 ├────────┼─────────────────┼───┤                   ├────────┼──────────────────┼──────────┤
 │ android_camera plugin    │   │                   │ windows_receiver plugin   │          │
 │ (Kotlin)                 │   │                   │ (C++)                     │          │
 │  Camera2                 │   │                   │  MF H.264 decoder ──┐     │          │
 │  MediaCodec H.264 ─┐     │   │                   │                     ▼     │          │
 │  NsdManager        │     │   │                   │  Shared-mem writer (Global\)         │
 │  Foreground service│     │   │                   ├─────────────────────┬────────────────┤
 ├────────────────────▼─────▼───┤   TCP (Wi-Fi /    │ lenny_core (Rust)   │                │
 │ lenny_core (Rust, C ABI)     │◄─ ADB fwd / RNDIS)►│ protocol, session,  │                │
 │ protocol, session, transport │                   │ transport, jitter   │                │
 └──────────────────────────────┘                   └─────────────────────┼────────────────┘
                                                           Global\ shared memory ring
                                                    ┌─────────────────────┴────────────────┐
                                                    │ windows_vcam                         │
                                                    │  a) DirectShow source filter (x86+x64│
                                                    │     COM DLL, loaded in Zoom/Chrome…) │
                                                    │  b) MF virtual camera (Win11, runs in│
                                                    │     Frame Server as Local Service)   │
                                                    └──────────────────────────────────────┘
```

Video path: camera → MediaCodec → core (framing) → TCP → core (reassembly, jitter)
→ MF decoder → shared memory → vcam. **Dart never touches a frame.** Dart handles
UI, settings, and control commands only. Preview on the phone is a native
`Texture` (Flutter texture registry). Preview on desktop is also a native texture,
fed from the same decoded NV12 frames.

## 3. Repo layout

```
/app        Flutter app (one codebase: Android + Windows now; iOS/Linux later)
  lib/
    brand.dart            ← the ONLY place "Lenny" / "Lenny Desktop" strings live
    theme/                ← colors, StickerDecoration, sticker theme extension
    ui/components/        ← StickerButton, StickerCard, StatusChip, ConnectionControl, QrCard
    ui/screens/sender/    ← phone screens
    ui/screens/receiver/  ← desktop screens
    services/             ← the ONLY code that calls FFI / method channels
    state/                ← Riverpod providers
    core_bindings/        ← ffigen output (generated, don't edit)
/core       Rust crate lenny_core (cdylib + staticlib + rlib), C ABI in include/lenny/lenny.h, tests in core/tests
/framebuf   Rust crate lenny_framebuf: §7.3 shared-memory ring format (writer + bounds-checked reader), no deps
/vcam       Rust crate lenny_vcam: IVirtualCamera trait + backends (v4l2loopback, Windows shm writer, null)
  com/                  lenny_vcam_com.dll: DirectShow filter + MF media source (§7.4), Rust, reads framebuf only
/desktop    Rust desktop receiver (egui): Linux now, Windows next (§7a)
/plugins
  android_camera/         Kotlin: Camera2, MediaCodec encoder, NsdManager, foreground service
  windows_receiver/       C++: MF decoder, shm writer, DNS-SD browse, adb helper, preview texture
  windows_vcam/           C++: DirectShow filter DLL (x86+x64), MF vcam media source DLL
  qr_scan/                one-off QR scan (Android), see §8
/protocol   protocol spec source-of-truth pointer + binary test vectors
/ios /linux               README only (future work)
/installer  WiX project (see ADR-0004)
/docs       architecture.md, protocol.md, testing.md, compat-matrix.md, adr/
```

## 4. Shared core (`/core`)

Rust (ADR-0006; it was C++20 until the port, with the C ABI kept identical). No OS-specific code: sockets are
the one place that differs per OS, and they sit behind the `Transport` trait (`transport.rs`), built on std +
socket2, which cover Winsock and BSD sockets alike. The only `cfg` there is one errno check for a non-blocking
connect in progress.

Modules:

| Module | Responsibility |
|---|---|
| `wire` | Framing, header encode/decode, TLV payload serialization, version checks. Pure functions, fuzzable. |
| `session` | State machine: HELLO → (PAIR) → CAPS → STREAMING, keepalive, timeouts, reconnect backoff. |
| `transport` | `ITransport` interface, `TcpTransport`, `LoopbackTransport` (tests). |
| `timing` | Clock sync (NTP-style offset estimation over PING/PONG), frame timestamping, jitter buffer. |
| `framebuf` | Shared-memory ring buffer **format** (header layout, slot layout, seqlock rules). Not the OS mapping itself. |

### 4.1 C ABI

One header, `core/include/lenny/lenny.h`. Rules:
- Opaque handles (`lenny_session_t*`), plain C structs, `int32_t` error codes.
- No panics cross the ABI. Every exported function is wrapped in `catch_unwind` and returns
  `LENNY_E_INTERNAL` (or NULL) on failure.
- The header is hand-written (it's the documentation); `core/tools/abi_check.sh` regenerates one with cbindgen
  from the Rust source and fails CI on any difference in struct layout, constant value or function type.
- Callbacks are C function pointers + `void* user`. They are invoked on core-owned
  threads. The docs say which thread, and callers must not block in them.
- ABI version: `lenny_abi_version()`. Additive changes only within a major version.

Two kinds of callers:
- **Dart (ffi)** calls control functions only: create/destroy session, connect, disconnect,
  send control, read stats, register a status callback (via `NativeCallable.listener`).
- **Native plugins** (Kotlin via JNI, Windows C++) call the media functions:
  `lenny_send_video_frame(...)` on the sender and `on_video_frame` callback on the receiver.
  So encoded bytes go native → core → native, and never into Dart.

Session handles are shared between the Dart side and the plugin side by passing the
opaque pointer as an `int64` over the method channel once, at session creation.

### 4.2 Interfaces (platform layers)

Defined as C++ abstract classes in the plugins' shared header space (not in core,
because they wrap OS APIs). Core only sees bytes and timestamps.

| Interface | Android (P1) | Windows (P1) | Later |
|---|---|---|---|
| `ICameraSource` | Camera2 (Kotlin) | — | iOS AVFoundation |
| `IEncoder` | MediaCodec H.264 | — | VideoToolbox |
| `IDecoder` | — | Media Foundation H.264 → NV12 | VideoToolbox, VA-API/FFmpeg |
| `IVirtualCamera` | — | DirectShow filter, MF vcam | v4l2loopback |
| Discovery | UDP probe (Dart, `discovery.dart`) | UDP responder (Dart) | same Dart code |

On Android these are Kotlin interfaces (same shape). The "interface" is the contract
documented here, not a shared C++ header, because Kotlin can't implement a C++ class.

### 4.3 Threading (core)

- One I/O thread per session (blocking socket reads with a timeout; no async framework).
- One timer tick per session (keepalive, timeouts) driven from the I/O thread's poll timeout.
- Callbacks fire on the I/O thread. Receivers hand frames to their decoder queue and return.

## 5. Session lifecycle

```
Receiver listens on TCP 47474 (default) and advertises _lenny._tcp.
Sender connects (manual IP, discovery result, QR payload, adb-forwarded localhost, or tethering IP).

SENDER                                  RECEIVER
  HELLO(proto ver, role, device info) ──►
                                     ◄── HELLO(proto ver, role, device info, pairing_required)
  [PAIR_REQUEST(token)] ──────────────►   (only if QR/pairing is used)
                                     ◄── [PAIR_RESULT(ok | reason)]
  CAPS(codecs, resolutions, fps, controls) ──►
                                     ◄── CAPS_SELECT(codec, res, fps, bitrate)
  STREAM_START ─────────────────────────►
  VIDEO_FRAME … (keyframe first) ───────►
                                     ◄── CONTROL(focus_at, torch, lens, …) any time
  CONTROL_STATE ────────────────────────►  (current auto/manual state)
  PING/PONG both ways every 1 s; 3 s silence = dead link → reconnect.
```

Direction note: the **sender connects to the receiver**. This keeps one code path
for all transports. With ADB we use `adb reverse tcp:47474 tcp:47474`, so the
phone's `localhost:47474` reaches the PC. With tethering and Wi-Fi the phone
connects to the PC's IP. The receiver knows its own IP, which is what makes the
QR flow work.

Reconnect: sender retries with backoff of 250 ms, 500 ms, 1 s, then 2 s repeated
until the user cancels. The receiver keeps the vcam alive on the placeholder frame
during this time. On reconnect the sender always sends a fresh keyframe and the
default **Auto** control state (protocol.md §6.9: every connect and reconnect starts in Auto).

## 6. Android sender

- Camera2 capture session straight into MediaCodec's input `Surface`; the on-screen Flutter texture
  becomes a second output of the same session. Camera2 rather than CameraX: CameraX only sees
  `CameraManager.cameraIdList`, and phones often leave the ultrawide and tele out of it while still
  letting apps open them (Nothing Phone (3a): list = main + front; ultrawide, tele and a logical
  0.6x-10x camera are hidden but open fine).
- Lenses: every openable camera (listed ids plus probed hidden ids). A logical multi-camera becomes one
  lens per physical sensor, at the zoom ratio that selects it (from 35 mm-equivalent focal lengths):
  "0.6x", "1x", "2x". Switching between those is a new repeating request with another
  `CONTROL_ZOOM_RATIO`, no session restart; other lenses reopen the camera. Output size is picked
  explicitly (exact, else same aspect ratio), so the stream keeps the negotiated size on every lens.
- Request: `CONTROL_AF_MODE_CONTINUOUS_VIDEO`, `CONTROL_AE_MODE_ON`,
  `CONTROL_AE_TARGET_FPS_RANGE=[30,30]` (or the chosen fps), `CONTROL_AE_ANTIBANDING_MODE_AUTO`,
  `CONTROL_AWB_MODE_AUTO`, `CONTROL_VIDEO_STABILIZATION_MODE_OFF` (EIS buffers frames ahead).
- Orientation: from the orientation sensor (`OrientationEventListener`), so it's right whichever way the
  phone stands; flat on a table keeps the last value.
- Two modes, Auto and Manual (protocol.md §6.9). Tap-to-focus (Manual): receiver sends `CONTROL focus_at(x,y)` in
  normalized upright coords. Sender maps it to an AF/AE region in the visible part of the active array, triggers AF
  and holds focus there until Auto. Every connect and reconnect starts in Auto.
- Capabilities per lens (sent in CAPS, protocol 1.1): the modes each lens can stream, straight from its
  StreamConfigurationMap, AE fps ranges and the AVC encoder, no hardcoded size list; and its zoom range
  (`CONTROL_ZOOM_RATIO_RANGE`, or 1..`SCALER_AVAILABLE_MAX_DIGITAL_ZOOM` before API 30, crop only). Discovery runs at
  every Connect. The stream only ever uses a mode the current lens lists.
- Zoom and pan on the sensor readout: `CONTROL_ZOOM_RATIO` (the platform switches physical sensors) plus
  `SCALER_CROP_REGION` to move the zoomed window over the full field of view; never a transform of finished frames.
- MediaCodec: `video/avc`, `COLOR_FormatSurface`, CBR/VBR bitrate from CAPS_SELECT,
  `KEY_I_FRAME_INTERVAL=1` s, `KEY_LOW_LATENCY=1` (API 30+), `KEY_MAX_B_FRAMES=0`,
  Baseline or Constrained High profile. Keyframe on demand via `PARAMETER_KEY_REQUEST_SYNC_FRAME`.
- Output buffers go from MediaCodec to JNI `lenny_send_video_frame(ptr, len, pts_us, flags, orientation)`.
  SPS/PPS (`BUFFER_FLAG_CODEC_CONFIG`) are sent as a `VIDEO_CONFIG` message and repeated before every keyframe.
- Rotation: the encoder always encodes sensor-native landscape. The current display
  rotation goes in each frame's `orientation` field, and the receiver rotates.
  This avoids reconfiguring the encoder on rotation, which is a common source of crashes.
- Foreground service (`camera` + `connectedDevice` types, API 34 rules). It owns the camera,
  encoder and core session, so screen-off, app switching and incoming calls don't tear
  down the pipeline. The Flutter activity only binds to it.
- Phone events: on camera eviction (e.g. another app takes the camera during a call),
  send `STREAM_STATUS(paused, reason)` so the receiver shows the placeholder with the
  reason, and reacquire the camera automatically when it's available again.

## 7. Windows receiver

### 7.1 UI and native language (see ADR-0003)
The UI is Flutter. Native parts are C++ only, with no C#/.NET, because:
- Media Foundation, DirectShow and `MFCreateVirtualCamera` are native COM APIs. C# would need interop for all of them.
- The DirectShow filter is loaded **into other processes** (Zoom, Chrome). Loading a CLR there is out of the question.
- The core is C++, so there's one toolchain, one debugger, and no marshalling layer.

### 7.2 Decode
MF H.264 decoder MFT (hardware via `MF_SA_D3D11_AWARE` + DXGI device manager, software
fallback automatically). Output NV12. `CODECAPI_AVLowLatencyMode = TRUE`. If the decoder
errors, flush and wait for the next keyframe (request one via `CONTROL keyframe_request`),
and the vcam shows the last good frame for ≤ 500 ms, then the placeholder.

### 7.3 Shared memory (`framebuf` format, Windows mapping)
- Names: `Global\LennyFrames_v1` (mapping), `Global\LennyFrameReady_v1` (auto-reset event).
- `Global\` because the MF vcam's media source runs inside **Frame Server** (svchost,
  `LOCAL SERVICE`), in session 0. Session-local names are invisible there.
- Creating `Global\` objects needs `SeCreateGlobalPrivilege`, which normal user processes
  don't have. The object is therefore created by a small **Lenny broker service** (LocalSystem,
  installed by the installer, start on demand). The receiver app and both vcam backends open it.
  The DACL grants: SYSTEM full, Administrators full, INTERACTIVE read/write, LOCAL SERVICE read,
  plus Low-IL read label so sandboxed consumers (Chrome's renderer is not the loader,
  but some apps run at low IL) can map it. *OS quirk: this is exactly what OBS works around too;
  verify on a standard (non-admin) account in M4.*
- Layout (all little-endian, fixed, versioned):
  ```
  Header (4 KiB aligned):
    magic 'LNYF', version u32, slot_count u32 (3), width u32, height u32,
    format u32 (NV12), stride_y u32, stride_uv u32,
    write_index u64 (monotonic), producer_pid u32, producer_heartbeat_ms u64,
    state u32 (NO_SOURCE | LIVE | RECONNECTING | ERROR), orientation u32
  Slot[3]: seq u64 (seqlock: odd = writing), pts_100ns i64, bytes[]
  ```
  Max size is sized for 1920x1080 NV12. Triple buffering: the writer never blocks and
  readers take the newest completed slot and verify `seq` is unchanged after copying (seqlock).
- A reader treats the source as dead if `producer_heartbeat_ms` is > 1 s old and switches to the placeholder.

### 7.4 Virtual camera backends
Implementation (Phase B): both live in one Rust COM DLL, `vcam/com` (`lenny_vcam_com.dll`, x64 and x86, windows-rs, no
lenny_core). The writer is `lenny_vcam::WindowsCamera` in the desktop app. No broker service yet: the app creates
`Global\` names when it may, else falls back to `Local\`, which the DirectShow filter sees and Frame Server doesn't.
The C++/plugin wording below is the original plan; the design (shm, crash containment, naming) carries over as is.

Both read the same shared memory, and both do their own scale/letterbox and color convert
to the format the consumer picked, so the receiver app writes one frame at one size.

**a) DirectShow source filter** (`lenny_dshow_x64.dll`, `lenny_dshow_x86.dll`)
- Push source with `IAMStreamConfig` advertising 1280x720 and 1920x1080 at 30 fps, NV12 and YUY2.
  (Chrome prefers YUY2/NV12; Zoom and Teams accept both.)
- Written against plain COM (no DirectShow BaseClasses from the SDK samples, to avoid licensing
  and linking pain); small hand-rolled `CSource`-equivalent. OBS's implementation is the
  study reference only (GPL, no copying).
- **Crash containment** (the #1 requirement): every COM entry point and the streaming thread
  body are wrapped in SEH `__try/__except` + C++ `try/catch`. Any fault puts the filter into
  "placeholder only" mode for the rest of its life in that process. No allocation on the
  streaming path after start. Shared memory reads are bounds-checked against the header
  and the mapping size, because a corrupted header must never cause an out-of-bounds read.
  The filter has **no** dependency on lenny_core or any network code. It only knows the shm format.
- Placeholder frame: generated in-DLL (solid background + "Lenny — waiting for phone" text
  rendered from an embedded bitmap). The mascot drop-in spot is noted in §10.

**b) Media Foundation virtual camera** (Windows 11 22000+)
- A custom media source COM DLL registered for Frame Server, plus `MFCreateVirtualCamera`
  called by the receiver app at runtime when `MFCreateVirtualCamera` is present in `mfsensorgroup.dll`.
  `MFVirtualCameraLifetime_Session` first, `System` later if we want it to show up without the app running.
- Runs in Frame Server as LOCAL SERVICE, hence the `Global\` + ACL design above.
- Same crash-containment rules. A crash here kills Frame Server's worker, not the consumer app,
  but it still breaks every camera on the system until it restarts, so it's treated just as seriously.
- If both backends are present, MF-aware apps may list two "Lenny" cameras. The MF vcam is named
  "Lenny" and the DirectShow one "Lenny (Classic)" on Win11. Revisit after M5 testing.

### 7.5 Discovery and USB helpers
- LAN discovery (replaces the earlier mDNS plan): the phone broadcasts the UDP probe `LENNY?1` to port
  47474, and every Lenny Desktop answers with a `lenny://` link without host or token (protocol.md §2).
  The answer's source address is the host. Pure Dart (`app/lib/services/discovery.dart`), the same on
  every platform, no native mDNS APIs. Limit: IPv4 broadcast reaches only the local subnet; mDNS stays
  the upgrade if multi-subnet setups matter.
- QR pairing: the desktop shows a `lenny://` link listing all its IPv4 addresses, the port and a
  single-use token (90 s, renewed every 80 s and after each pairing). The phone probes the listed
  addresses with the same UDP probe, connects to the first that answers, and the token skips the
  approval prompt (protocol.md §6.4).
- Trusted phones: a phone that streamed once is saved by device id (shared_preferences) and passed to
  `lenny_receiver_trust_device` at start, so it reconnects without a prompt. The phone saves the last
  PC that streamed and prefills the form.
- ADB: bundled `adb.exe` (platform-tools, Apache 2.0) under the install dir. The receiver polls
  `adb devices` every 2 s, and for each authorized device runs `adb reverse tcp:47474 tcp:47474`
  and tells the phone app to connect to localhost via `adb shell am broadcast`, or the phone
  auto-tries localhost when USB is attached. If the device shows `unauthorized`, the UI shows the setup
  guide step "Allow USB debugging on your phone".
- Tethering (RNDIS): no special code. The PC gets a `192.168.42.x`-style address on a new adapter.
  The receiver listens on all interfaces, and the phone connects to the tethering gateway's peer.
  The receiver lists the RNDIS adapter IP in the USB dropdown ("USB tethering: 192.168.42.129").

## 7a. Desktop receiver in Rust (Phase B; Linux now, Windows next — ADR-0007)

`/desktop` (crate `lenny_desktop`): eframe/egui over winit, linking `lenny_core` and `lenny_vcam` as Rust crates
(no FFI). Replaces the Flutter desktop + `plugins/windows_receiver` on each OS once that OS has its virtual camera
backends.

- **Window**: borderless (`with_decorations(false)`); the title bar is drawn per docs/design.md and wired to real
  viewport commands: minimize, maximize/restore, close, `StartDrag` on the bar, double-click to maximize,
  `BeginResize` on the window edges. Same winit calls on Windows.
- **Layout**: computed from the window size every frame (fractions + clamps). Wide: preview hero + a control column
  (31 % of the width, 340–440 px). Narrow (< 1000 px): one scrolling column.
- **Engine** (`receiver.rs`): the core receiver session; callbacks only queue (bounded, drop → keyframe request).
  Decoder thread: openh264 (built from source by the crate: no system libraries, same on MSVC, BSD licence).
  OpenH264 officially decodes Constrained Baseline, which is what Android's encoder produces by default (we don't
  request a profile); it also has CABAC (Main/High) decoding, and it rejects frames over 1 MB (check 4K at high
  bitrates on a real phone). Decoded I420 goes (a) into the fixed 1280×720 virtual-camera canvas (rotated
  upright, letterboxed) and (b) into the preview at the frame's own aspect ratio.
  Virtual-camera thread: steady 30 fps; the last good frame for ≤ 500 ms, then a placeholder (dotted page + reason:
  "waiting for phone", "phone paused"...). Discovery thread: answers `LENNY?1` on UDP 47474. QR tokens: 90 s, renewed
  every 80 s and after each pairing. Phones that streamed once are saved (`known_phones.txt` in the config dir).
- **Preview** (Task 6): the box is sized to the decoded frame's real aspect ratio after rotation, never the
  requested one and never a hardcoded 16:9; it re-measures when the size changes, and never stretches.
- **Controls**: lens, zoom (slider and mouse wheel), drag-to-pan on the preview when zoomed (protocol 1.1 pan, capture-
  level on the phone), torch; Auto | Manual (tap-to-focus holds, EV, exposure lock); video modes from the selected
  lens's own list; stats; virtual camera status ("active" or "unavailable in this environment").
- **`lenny_vcam`**: `IVirtualCamera { open(format), write_frame(&[u8]), close(), is_real(), describe() }`, I420.
  Backends: `V4l2LoopbackCamera` (Linux; finds a v4l2loopback device or tries `modprobe` once),
  `WindowsCamera` (writes the §7.3 ring as NV12 for the `vcam/com` DLL's two cameras, and registers the MF camera
  on Win11) and `NullVirtualCamera` (writes `frames.log` and a rolling `latest.i420` sample). `open_best` falls back
  to null and logs why.
- **Testing without a phone**: `lenny_desktop::fake_phone` (synthetic camera → openh264 → core sender; zoom/pan move
  its fake sensor crop). `cargo run -p lenny_desktop --example fake_phone -- 127.0.0.1 47474 [--portrait]`.

## 8. QR quick-connect

- Desktop renders `lenny://c?v=1&h=<ip>[,<ip>…]&p=47474&t=<token>&n=<pc name>` (URI, not JSON,
  because it makes a denser, more scannable QR). Multiple IPs are listed when the PC has several adapters,
  and the phone tries them in parallel and keeps the first HELLO that succeeds (handles VPN/VLAN cases).
- Token: 128-bit random, base64url, **single-use**, expires after **90 s** or when the drawer closes,
  held only in receiver memory. Regenerated on drawer open, on expiry, and on IP change
  (`NotifyIpInterfaceChange`). Stale → "Refresh" action on the card.
- Phone: "Scan to connect" opens a one-off scanner. For the scanner we use Google Code Scanner
  (`play-services-code-scanner`), which has no camera permission and runs in the system UI. This is the
  OS-camera reuse the prompt asks for, and it's separate from the streaming camera pipeline.
  Fallback for devices without Play services: typing the address.
- Trust: the token only proves "this phone saw this screen recently". It goes through `PAIR_REQUEST`
  in the normal session (protocol.md §6). No token = the receiver's policy decides: phase 1 default is
  **accept unpaired LAN connections but show an accept/deny prompt on first connect of an unknown
  device id**. The QR path skips the prompt because scanning the screen *is* the consent.

## 9. Security posture (phase 1)
- LAN-only, no cloud, no accounts.
- **Encryption: deferred** (decision can be made any time; see ADR-0005). The threat model assumes a
  trusted home LAN. Three hooks are kept from day one so adding TLS later is a contained change, not a rewrite:
  1. All bytes go through `ITransport`, so TLS becomes a `TlsTransport` wrapping `TcpTransport`, and
     session/protocol code doesn't change.
  2. HELLO `features` bit0 `TLS_UPGRADE` is reserved. Both sides advertise it, then upgrade right after HELLO (STARTTLS-style),
     so old and new builds interoperate.
  3. `device_id` is stable per install, and the QR parser ignores unknown params, so a later `&f=<cert fingerprint>`
     param doesn't break old phones.
  Hard rule to keep these hooks valid: **no code outside `transport/` may touch sockets directly**.
- The receiver caps message sizes (protocol.md §3) and drops the connection on malformed framing.
  Unknown message types are skipped, not fatal.
- Debug logs, crash dumps and captured streams stay on the local machine and are gitignored. They are never committed.
- The receiver caps message sizes (protocol.md §3) and drops the connection on malformed framing.
  Unknown message types are skipped, not fatal.

## 10. Flutter app structure

- **State: Riverpod** (ADR-0002). Services are plain Dart classes exposed via providers, and widgets
  read providers only. Widgets never import `dart:ffi` or `MethodChannel`, which is enforced by a
  custom lint / `import_lint` rule on `lib/ui/**`.
- Services: `CoreService` (FFI session control, stats stream), `CameraService` (sender: method channel
  to android_camera), `ReceiverService` (desktop: windows_receiver), `DiscoveryService`, `UsbService`,
  `PairingService` (QR token generation/parsing).
- Role is chosen by platform: Android/iOS → sender screens, Windows/Linux → receiver screens.
- Brand: `lib/brand.dart` holds `appName = 'Lenny'`, `desktopAppName = 'Lenny Desktop'`,
  `storeName = 'Lenny – Phone Webcam'` (store listings only; in-app it's just "Lenny"),
  `appId = 'com.spizganed.lenny'` (Android applicationId, iOS bundle id, Windows AppUserModelID prefix).
  The app id can never change after the first store release. Native projects
  (AndroidManifest label, Windows resource file, installer product name) get it from a generated
  `brand.json` at build time so the name really lives in one place.
- **Mascot slot:** `MascotSlot` widget on the home/connection screen, sized box with a
  `// TODO(mascot): drop sprite assets here` marker and no placeholder art. The same spot exists in the
  vcam placeholder frame, which is plain text for now.

### 10.1 Sticker style
- `lib/theme/lenny_colors.dart`: bg `#14162b`, outline `#b9b5cc`, shadow `#0c0c18`, primary yellow,
  danger red/pink, info, success. These are the only color definitions in the app.
- `StickerStyle` `ThemeExtension` (outline width 3, radius 18, pill radius, shadow offset 5,5) +
  `StickerDecoration` builder producing `BoxDecoration(border: 3px outline, boxShadow: [BoxShadow(color: shadow, offset: Offset(5,5), blurRadius: 0, spreadRadius: 0)])`.
- `Pressable` wrapper: pointer down animates shadow offset → 0 and translates the child by (5,5)
  with a spring (flutter_animate / `SpringSimulation`, ~250 ms). `MediaQuery.disableAnimations` → fade only.
  Every StickerButton/Card/Chip uses it, so nothing is built ad hoc per screen.
- Background: `CustomPainter` dot grid at ~4% opacity.
- `QrCard`: sticker card whose inner area is a flat white box with ≥ 4-module quiet zone. The QR
  image itself gets no outline or shadow.

### 10.2 ConnectionControl
One widget, both apps. Collapsed pill: left zone = status dot + label (tap = disconnect
when connected), right zone = chevron (expand/collapse only). Expanded (only when disconnected) = the
same container grows via `AnimatedSize` + `AnimatedOpacity` (ease curve, not spring) into:
Wi-Fi section (IP, port, Connect, discovered list, QR: desktop shows QrCard, phone shows "Scan to connect")
and USB section (device dropdown: ADB / tethering, Connect). Chevron `AnimatedRotation` 0.5 turns.
Connect success → auto-collapse.

## 11. Future platforms (designed, not built)

| Platform | New code only | Notes |
|---|---|---|
| iOS sender | Swift plugin: AVFoundation capture, VideoToolbox H.264, NWBrowser discovery, background mode limits | iOS can't keep the camera running in the background, so streaming requires the app in the foreground. Tell users. Needs `NSLocalNetworkUsageDescription` + `NSBonjourServices` (`_lenny._tcp`) in Info.plist, or iOS 14+ silently blocks LAN connects. No ADB on iOS: USB path is Personal Hotspot over USB (same idea as tethering). |
| Linux receiver | Built (§7a): Rust desktop app, openh264 decode, v4l2loopback via `lenny_vcam`, UDP discovery | v4l2loopback is a DKMS module; Secure Boot requires a MOK-signed module. PipeWire camera backend as a later option. |

The core, protocol and Flutter UI are unchanged in both.

macOS receiver: **dropped** (decision 2026-09-25). The protocol keeps `platform=4` reserved so the numbering never shifts.

## 12. Build and CI
- Core: a Cargo workspace at the repo root (`core`, `vcam`, `desktop`). `cargo test` runs the wire, vector and
  full-session tests; `core/tests/c_abi/` is the old C++ session test, built against lenny.h and linked to the Rust
  static library under ASan/UBSan. Android: cargo-ndk from the android_camera Gradle build (arm64, armv7, x86_64,
  x86). `core/CMakeLists.txt` wraps cargo for CMake consumers (Windows, not yet built there).
- Flutter: `flutter build apk` / `flutter build windows`. ffigen runs in CI and a diff check keeps bindings in sync.
- CI: GitHub Actions `core.yml` — fmt, clippy, tests, ABI check and the C ABI test on Linux; cargo-ndk build for Android.
  `windows` job: clippy + workspace tests on MSVC x64, the virtual camera tests as x86 too, regsvr32 round trip. Fuzzing of `wire` decode (cargo-fuzz) is still to do.

## 13. Milestone risk register (top items)

| # | Risk | Mitigation |
|---|---|---|
| R1 | DShow filter crash takes down Zoom/Teams | No core/network code in filter, SEH everywhere, bounds-checked shm, fault → placeholder-only mode, fuzz shm header parsing |
| R2 | `Global\` shm permissions (non-admin users, Frame Server) | Broker service creates the objects. Test on standard user in M4, M5 |
| R3 | Android surface limits (encoder + preview) | GL fan-out fallback, device matrix |
| R4 | OEM camera quirks (AE fps range ignored, KEY_LOW_LATENCY ignored) | Query `CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES`, log effective values in stats |
| R5 | Two Lenny devices on Win11 confuse users | Naming, and possibly hide DShow on Win11 by default (decide after M5) |
| R6 | Latency target on congested Wi-Fi | Jitter buffer target adaptive 0–60 ms, drop-to-keyframe policy, bitrate step-down on RTT increase |
| R7 | Bundled adb conflicts with a user's adb server version | Use user's adb if on PATH and running. Else bundled on non-default port (`ANDROID_ADB_SERVER_PORT`) |

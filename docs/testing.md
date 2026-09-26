# Testing

Automated:
- `core/`: `cargo test -p lenny_core` (wire format, test vectors, full sessions over localhost TCP, struct layouts).
  `core/tools/abi_check.sh` (cbindgen header vs lenny.h). `core/tests/c_abi/`: the C++ session test against the Rust
  library, under ASan/UBSan in CI.
- `vcam/`: `cargo test -p lenny_vcam` (compose/rotate/letterbox, placeholder, null backend, v4l2 ioctl numbers).
- `desktop/`: `cargo test -p lenny_desktop`: QR link format, known phones, and `tests/loopback.rs`: fake phone ->
  core -> decode -> preview/virtual camera, zoom/pan/focus/Auto round trips, mid-stream mode switch. Headless.
- `plugins/android_camera`: `./gradlew :android_camera:testDebugUnitTest` (from `app/android`): zoom/pan crop math,
  mode matching (JVM, no device).
- `app/test/mode_controls_test.dart`: Auto/Manual widget behaviour.
- `app/`: `flutter test`.
- `plugins/windows_receiver/windows/test/nv12_test.cpp`: preview converter (rotation + letterbox).

Tools:
- `cargo run --release --bin lenny_probe -- [port] [seconds]`: headless receiver that auto-accepts phones and prints fps, bitrate,
  keyframes, RTT and latency once a second. Use it to test a sender without the desktop app, and for soak tests (M6).
- Emulator: the phone reaches the PC at `10.0.2.2`. The AVD's back camera should be `virtualscene`. Needs hardware
  acceleration (KVM / HAXM / Hyper-V); the cloud sandbox has none, so it isn't used there.
- Fake phone: `cargo run -p lenny_desktop --example fake_phone -- <host> <port> [--portrait]`: a synthetic sender
  (openh264) for trying the desktop app without a phone. Not a camera test.
- `lenny-desktop --screenshot out.png [--after s] [--size WxH]`: renders, saves a PNG, quits (Xvfb-friendly).
- `tools/linux-test-vm.ps1` (Windows, elevated PowerShell): VirtualBox + Ubuntu 24.04 Xfce VM with v4l2loopback,
  OBS, Discord, Chromium and Lenny Desktop built from a branch, bridged so a phone can connect. First boot ~20-40 min.

Debug output (logs, dumps, captured streams) stays local and is gitignored. Never commit it.

## Manual smoke test, M2 (Android -> Windows preview over Wi-Fi)

Last run: 2026-09-25, emulator (Android 16, x86_64) -> Windows 11, Lenny Desktop debug build.

| # | Step | Expected | Result |
|---|---|---|---|
| 1 | Start Lenny Desktop | "Waiting for a phone", shows this PC's IPs and port | ✅ |
| 2 | Phone: enter PC address, Connect | Desktop: "Allow this phone?" with the phone's name | ✅ |
| 3 | Allow | Both sides "Streaming"; live upright preview; stats line (1080p, ~30 fps) | ✅ 1080p30, ~7.5 Mbps, RTT 1 ms |
| 4 | Decline (or wait 30 s) | Phone: "The PC declined the connection", stops retrying | covered by core tests |
| 5 | Phone: Disconnect | Phone "Disconnected"; desktop back to waiting | ✅ |
| 6 | Quit desktop app while streaming | Phone: "Connection lost. Reconnecting…" | ✅ |
| 7 | Hard-kill desktop app, start it again | Phone reconnects by itself (asks for approval again: trust isn't persisted yet) | ✅ |
| 8 | Phone held in portrait | Desktop preview upright, letterboxed | ✅ |
| 9 | Screen off / app in background while streaming | Stream continues (camera foreground service) | not yet run |
| 10 | Real phone over Wi-Fi (not emulator) | Same as 2–8 | not yet run (phone busy) |

## Manual smoke test, M3 (camera controls, latency)

Last run: 2026-09-25, emulator (Android 16, x86_64) -> Windows 11. Latency is capture -> received, from
`lenny_probe` or the desktop stats line.

| # | Step | Expected | Result |
|---|---|---|---|
| 1 | Streaming, back camera | Latency shown, steady (not climbing) | ✅ 25–55 ms, levels off |
| 2 | Switch to Front | Stream continues, preview flips, latency still shown | ✅ ~3 ms (emulator front clock is off, so it counts from encoder output) |
| 3 | Switch back to Back | Same as 1 | ✅ |
| 4 | Torch, Lock focus, Auto, exposure ± | Phone applies it, desktop controls mirror phone state | ✅ |
| 5 | Tap the desktop preview | Phone focuses there | ✅ |
| 6 | Real phone over Wi-Fi | Same as 1–5, glass-to-glass < 150 ms | ✅ Nothing Phone (3a): 30 fps 1080p, 110–140 ms capture -> shown (release desktop; a debug desktop adds 100+ ms converting the preview) |
| 7 | Lens buttons (0.6×, 1×, 2×, Front on a phone with those) | Each switches, stream stays at the negotiated size | ✅ 1920×1080 on every lens |
| 8 | Phone standing in landscape | Desktop preview upright | ✅ |

Known: white specks along dark edges in low light (seen on 0.6× and front). Converter math checked; source not
yet isolated (phone ISP sharpening vs decoder concealment).

## Desktop (Rust, Linux), cloud sandbox run

Last run: 2026-09-26, Ubuntu 24.04 container, Xvfb + openbox, fake phone (no real phone, no emulator, no
v4l2loopback). Screenshots in `docs/screenshots/desktop-linux-*.png`.

| # | Step | Expected | Result |
|---|---|---|---|
| 1 | Start `lenny-desktop` | Waiting state, QR, IP/port chips, virtual camera status | ✅ "Virtual camera: unavailable in this environment" (null backend: no v4l2loopback device, modprobe unavailable) |
| 2 | Unknown phone connects | "Allow this phone?" dialog, Allow/Decline | ✅ |
| 3 | Known phone connects | Streams without prompt; preview + stats | ✅ 1920x1080, null camera writing 30.0 fps |
| 4 | Portrait phone (orientation 90°) | Preview box becomes 9:16, upright, not stretched | ✅ |
| 5 | Mouse wheel on preview, then drag | Zoom to the lens max (4.0x), drag pans the phone's crop | ✅ (fake sensor crop moves) |
| 6 | Title bar: maximize, restore, double-click, drag, minimize, close; edge resize | Real window changes | ✅ via xdotool: 1200x800 -> 1600x1000 -> back; moved; hidden; exit 0; 1200 -> 840 px wide |
| 7 | Narrow window (560 px) | One column: preview, then cards | ✅ |
| 8 | Real phone over Wi-Fi, real v4l2loopback consumers (Chrome, OBS, Zoom) | Camera visible and live in each | not run: needs a local machine |
| 9 | Windows 10/11 | — | not started (ADR-0007) |

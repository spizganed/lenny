# Lenny

Phone-as-webcam: a phone (sender) streams its camera to a desktop (receiver),
which exposes it as a real OS-level virtual camera to Zoom/Teams/Discord/OBS/
browsers. Wi-Fi or USB.

Always read these before making changes in their area:
- @docs/architecture.md — overall system shape
- @docs/protocol.md — wire protocol (byte-for-byte spec, do not improvise)
- @docs/design.md — visual design system (colors, shape, shadow, motion rules)
- @docs/testing.md — manual smoke-test checklists per milestone
- @docs/adr/ — one decision per file; read before revisiting a past decision

## Current phase vs. target architecture — read this first

This project is mid-migration. Don't assume either state without checking
which phase is active (see "Where we are" below).

**Phase A (current, interim):** C++ core is being ported to Rust with the C
ABI held identical, so the existing Flutter app and its `dart:ffi` bindings
keep working unchanged throughout. Flutter is scaffolding here — it exists so
the Rust core can be validated end-to-end (real sender ↔ real receiver)
without also rewriting the UI at the same time. Do not add new Flutter
screens or features during this phase; only keep it building.

**Phase B (planned, not started):** once the Rust core is fully ported and
tested, Flutter is removed entirely. Each platform gets a normal native app:
- Android: Kotlin + Jetpack Compose, calling the Rust core via JNI (or UniFFI
  if the boundary gets complex). No Flutter, no `dart:ffi`.
- Windows: a Rust desktop app (egui or iced), linking the Rust core crate
  directly — no FFI layer needed since it's Rust calling Rust. Spike this
  early: confirm `windows-rs` can register a DirectShow filter and drive
  `MFCreateVirtualCamera` before writing any UI. Only fall back to a small
  isolated C++ shim for a specific COM interface `windows-rs` genuinely
  doesn't expose — never for the whole app.
- iOS/macOS/Linux: come later, same pattern (native per platform).

**The UI is never shared as code across platforms, only as a spec.**
`docs/design.md` is the single source of truth for how every screen should
look and animate (colors, outline width, hard offset shadow, press-sink
animation, corner radii). Every platform's native UI must match it visually;
none of them share UI code or a UI framework to do so. When `docs/design.md`
changes, every platform's implementation needs a matching update — check for
drift, it won't happen automatically like it would in one shared codebase.

**Where we are:** check the current branch and `docs/adr/` for the latest
superseding ADR before assuming which phase applies. If unclear, ask rather
than guessing which architecture is currently in force.

Status (2026-09-26, end of the cloud session):
- **Core**: ported to Rust and tested (ADR-0006). C ABI unchanged and checked by
  `core/tools/abi_check.sh`; wire protocol now 1.1 (per-lens modes/zoom, pan).
- **Linux desktop receiver** (`/desktop`, Rust/egui, ADR-0007): working end to
  end with a synthetic phone. Virtual camera ran on the **null backend** (the
  sandbox can't load v4l2loopback); the real v4l2loopback path needs a local run.
- **Android**: per-lens camera discovery, capture-level zoom/pan, Auto/Manual
  modes, all on Camera2. APK builds with the Rust core via cargo-ndk. Real
  camera behaviour (AF, exposure, lenses) is untested — needs a real phone.
- **Windows**: entirely pending; next up (see the Handoff section below).

## Handoff: next session runs on the Windows PC (CLI agent)

The cloud session (branch `rust-rewrite-cloud`, PR #3, CI green) did Tasks 1–8:
Rust core behind the unchanged C ABI, protocol 1.1, Android per-lens discovery /
capture-level zoom+pan / Auto-Manual, the Rust egui desktop (Linux) with
`lenny_vcam` (`IVirtualCamera`, v4l2loopback + null backends), fake phone,
`tools/linux-test-vm.ps1`. Details: ADR-0006, ADR-0007, architecture §7a,
testing.md "Desktop (Rust, Linux)". Nothing was run on real hardware.

Do these in order:

1. **Verify PR #3 on real hardware, then merge it.** Checklist is in the PR
   description. Record results in `docs/testing.md` (fill in row 8 of the
   Desktop table, add a row per consumer). Fastest Linux setup on the PC:
   `tools/linux-test-vm.ps1` (untested on Windows itself: if the VirtualBox
   install or the IMAPI2 seed-ISO step fails, fix the script first).
2. **Next task: Windows virtual camera backends**, both of them (non-negotiable
   rule above), behind the existing `IVirtualCamera` trait so `lenny_desktop`
   needs no Windows special-casing beyond `lenny_vcam::open_best`.
   - Spike first (Phase B note above): prove `windows-rs` can implement and
     register a DirectShow source filter (`#[implement]` COM classes,
     `DllGetClassObject` / `DllRegisterServer`) and call `MFCreateVirtualCamera`
     (mfsensorgroup.dll, Win11 22000+) before building out. C++ shim only for a
     COM interface windows-rs really lacks.
   - Shape (architecture §7.3–7.4, keep it): the desktop app writes frames into
     a `Global\LennyFrames_v1` shared-memory ring (seqlock slots, heartbeat,
     state); both backends only read it. Put the ring **format** in a small
     crate/module with no lenny_core or network dependency, shared by writer and
     readers. `Global\` needs `SeCreateGlobalPrivilege`: broker service per §7.3
     (or prove a simpler creation path works for a standard user and the MF
     Frame Server, and record that in an ADR).
   - Writer side: new `lenny_vcam` backends (`DirectShowCamera`,
     `MfVirtualCamera`, or one shm writer plus MF registration) implementing
     `IVirtualCamera`; `open_best` on Windows opens both. Frames arrive as I420
     1280×720 (`receiver.rs` `VCAM`); convert to NV12/YUY2 in the readers.
   - DirectShow filter DLL: its own cdylib, built for x64 **and** x86
     (`x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`), no lenny_core. It runs
     inside Zoom/Teams/Chrome: every COM entry and the streaming thread wrapped
     in `catch_unwind` (so no `panic = "abort"` for that crate), no raw reads
     outside the checked mapping size, fault ⇒ placeholder-only mode, never a
     crash. Rust can't catch SEH access violations, so bounds-check everything.
     Name "Lenny (Classic)" on Win11, "Lenny" on Win10.
   - MF media source DLL: registered for Frame Server (runs as LOCAL SERVICE,
     session 0: hence `Global\` + the ACL in §7.3). Name "Lenny".
   - Placeholder frame when the heartbeat is > 1 s old: reuse
     `lenny_vcam::frame::placeholder_i420`'s look.
   - Test on Windows 10 **and** 11 with Zoom, Teams, Discord, Chrome/Edge
     (getUserMedia) and OBS, on a standard (non-admin) user too. Add a
     Windows smoke-test table to `docs/testing.md`.
   - Add a `windows-latest` job to `.github/workflows/core.yml` (workspace
     build/test with MSVC, plus the x86 filter build).
3. **Then:** build `lenny_desktop` on Windows (expected to work as-is: winit
   chrome, openh264 from source), retire the Flutter desktop +
   `plugins/windows_receiver` once the Rust app has the Windows camera
   (ADR), WiX installer per ADR-0004 (registers both backends).

Loose ends: design fonts not committed (drop OFL TTFs in
`desktop/assets/fonts`, see `theme.rs`); openh264 rejects frames over 1 MB
(check 4K keyframes); the Android encoder doesn't request a profile (Baseline
by default, which openh264 needs); `core/CMakeLists.txt` cargo wrapper never
built on Windows; `lenny-prompt.md` can be deleted.

Checks before any push: `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -- -D warnings` (CI uses the latest stable, whose lints are
stricter than older toolchains), `cargo test --workspace`,
`core/tools/abi_check.sh` (needs cbindgen + clang), and for app changes
`flutter analyze && flutter test` in `app/` plus
`./gradlew :android_camera:testDebugUnitTest` in `app/android`.

**Desktop build order: Linux first, Windows later, most of it shared.** The
Rust desktop receiver (egui or iced, over `winit`) is being built and tested
on Linux first, in a cloud sandbox with no Windows machine available. This
is not a Linux-only detour: `winit`-based custom window chrome (the
borderless window + hand-drawn title bar from `docs/design.md`) works the
same way on Windows and Linux, so that UI layer is expected to carry over to
Windows with little to no change. The one genuinely OS-specific piece is the
virtual camera backend, already isolated behind `IVirtualCamera`:
`v4l2loopback` on Linux now, DirectShow + `MFCreateVirtualCamera` on Windows
later. Because `v4l2loopback` is a kernel module, it likely can't be loaded
inside a sandboxed container at all — a "null" `IVirtualCamera` backend
(writes decoded frames to a file/log instead of a real device) exists
specifically so the rest of the pipeline (capture → encode → network →
decode) can be built and tested without kernel privileges. The real
`v4l2loopback` wiring and, later, the Windows backend both get implemented
and tested locally, using the null-backend-validated pipeline as the known-
good base to build on top of — port the *shape* of the Linux
`IVirtualCamera` implementation, not the syscalls themselves.

## Non-negotiable engineering rules (apply in both phases)

- **Auto mode is the default, always.** Continuous autofocus, auto exposure/
  ISO, auto white balance, from first connect. Manual controls are opt-in
  overlays, never a required setup step.
- **Never let a native failure crash the host app.** The DirectShow filter /
  MF virtual camera runs inside Zoom/Teams/whatever process consumes it. Any
  native-side error must fail safely: log it, show a placeholder frame, keep
  the device alive. A crash here takes down someone else's call, which is
  worse than any other class of bug in this project.
- **Reconnect automatically** through phone sleep/lock, app switching,
  rotation, and Wi-Fi/USB hot-swap. Never leave a frozen or black frame
  without an explanatory UI state.
- **QR quick-connect uses a short-lived, single-use pairing token** — never
  bake a permanent secret into the QR payload. See `docs/protocol.md` for the
  pairing message types.
- **Windows needs both virtual camera backends**, not one: DirectShow filter
  (Win10+11 baseline, x86 and x64) and `MFCreateVirtualCamera` (Win11 extra,
  for MF-only consumers). Skipping either silently breaks some apps.
- **Wire protocol is versioned and forward-tolerant**: unknown message types
  are ignored, not fatal. Don't change framing/magic/limits without updating
  `docs/protocol.md` and `protocol/vectors/` together.
- **Test against real consumers, not just a preview window**: Zoom, Discord,
  Teams, Chrome/Edge (getUserMedia), and OBS, on both Windows 10 and 11.

## Code-minimalism tools

If a "write less code" style skill/rule is active in a session, it does not
override the architecture above. The core/platform boundaries, the dual
Windows virtual-camera backends, and the design-spec-not-shared-code approach
to UI are intentional, even where a shorter single-platform hack would work.
Apply minimalism inside a component's implementation, not by removing these
boundaries.

## Repo hygiene

- `lenny-prompt.md` at the repo root is a historical one-time kickoff prompt,
  not standing instructions — this file (CLAUDE.md) replaced it. Safe to
  delete once its content is confirmed covered here and in `docs/`.
- One ADR per real decision in `docs/adr/`. Superseded ADRs stay in the repo
  (marked superseded), never deleted — they're the record of why we changed
  course.

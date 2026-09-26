# ADR-0007: Desktop receiver built and tested on Linux first, Windows next

Status: accepted (2026-09-26)

**Context.** Phase B replaces the Flutter Windows desktop with a native Rust app (egui over winit) that links the
Rust core (ADR-0006) directly. The work happens in a cloud sandbox: Linux, free, fully scriptable (Xvfb, a window
manager, xdotool, the whole pipeline in one process tree), but no Windows machine, and none planned soon. Building the
Windows app blind would mean shipping code nobody has run.

**Decision.** Build the real desktop receiver on Linux first (`/desktop`, crate `lenny_desktop`), in a shape where
only one piece is OS-specific:

| Part | Portable to Windows as-is | Linux-specific now → Windows later |
|---|---|---|
| Window chrome: borderless window, drawn title bar, min/max/close, drag, double-click, edge resize (`ui.rs`) | yes (winit viewport commands) | — |
| Layout, sticker widgets, design tokens (`ui.rs`, `theme.rs`) | yes | fonts load from `assets/fonts` |
| Receiver engine: core session, decode, discovery, pairing, known phones (`receiver.rs`) | yes (std, openh264 builds from source on MSVC) | config dir: `APPDATA` already handled |
| Virtual camera (`lenny_vcam`) | the `IVirtualCamera` trait, frame compose/placeholder helpers, null backend | `V4l2LoopbackCamera` → `DirectShowCamera` + `MfVirtualCamera` implementing the same trait |

The null virtual-camera backend exists so the rest of the pipeline (network → decode → compose → camera writes at a
steady 30 fps) is exercised without kernel access; `v4l2loopback` is used automatically where the module is loaded.

**Why.**
- Everything above the virtual camera can be built, run and tested here now, including the real window controls
  (checked under Xvfb + openbox with xdotool) and end-to-end streaming (`desktop/tests/loopback.rs`, fake phone).
- winit gives borderless windows and app-drawn chrome the same way on Linux and Windows, so the UI layer is not a
  Linux detour.
- The OS-specific part was already isolated behind one trait; porting it means writing backends, not rewriting the app.

**Cost / open.**
- Windows is untested: first local session builds the app with MSVC, then spikes `windows-rs` for the DirectShow
  filter and `MFCreateVirtualCamera` (CLAUDE.md), then writes the two backends. The DirectShow filter DLL runs inside
  other apps' processes and keeps the crash-containment rules in architecture.md §7.4.
- The sandbox had no `v4l2loopback` (no kernel module loading), so the real Linux device path is only unit-tested
  (ioctl numbers, fallback) and needs a local run with `modprobe v4l2loopback exclusive_caps=1`.

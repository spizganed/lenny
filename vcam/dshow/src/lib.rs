//! "Lenny (Classic)": a DirectShow video capture source filter (architecture.md §7.4a). Zoom, Teams, Chrome, OBS
//! and every other DirectShow consumer load this DLL into their own process and pull frames from it; the frames
//! come from the desktop app through the `lenny_framebuf` shared-memory ring. When the app isn't running (or is
//! stale), the filter shows the placeholder instead.
//!
//! Crash containment is the #1 rule: every COM entry point and the streaming thread run inside `catch_unwind`;
//! a caught panic puts the filter into placeholder-only mode for the rest of its life in that process. Shared
//! memory is only read through `lenny_framebuf::Reader`, which bounds-checks everything against the mapping.
//! Register (elevated): `regsvr32 lenny_vcam_dshow.dll` (the x86 build too, for 32-bit apps).
#![cfg(windows)]
#![allow(non_snake_case)] // COM method names

mod filter;
mod media;
mod server;

pub use server::{DllCanUnloadNow, DllGetClassObject, DllMain, DllRegisterServer, DllUnregisterServer};

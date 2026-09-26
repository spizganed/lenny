//! Lenny's Windows virtual cameras, one COM DLL with two classes (architecture.md §7.4):
//! - "Lenny" / "Lenny (Classic)": a DirectShow video capture source filter (`filter`). Zoom, Teams, Chrome, OBS
//!   and every other DirectShow consumer load this DLL into their own process and pull frames from it.
//! - "Lenny" on Windows 11: the Media Foundation virtual camera source (`mf`), run by Frame Server.
//!
//! Both read the desktop app's frames from the `lenny_framebuf` shared-memory ring and show the placeholder when
//! the app isn't running (or is stale). Neither links lenny_core or any network code.
//!
//! Crash containment is the #1 rule: every COM entry point and the streaming threads run inside `catch_unwind`;
//! a caught panic switches to placeholder-only for the rest of the object's life. Shared memory is only read
//! through `lenny_framebuf::Reader`, which bounds-checks everything against the mapping.
//! Register (elevated): `regsvr32 lenny_vcam_com.dll`, and the x86 build too, for 32-bit DirectShow apps.
#![cfg(windows)]
#![allow(non_snake_case)] // COM method names

mod filter;
mod frames;
mod media;
mod mf;
mod server;

use std::panic::{catch_unwind, AssertUnwindSafe};

use windows::core::{Result, HRESULT};
use windows::Win32::Foundation::E_UNEXPECTED;

pub use server::{DllCanUnloadNow, DllGetClassObject, DllMain, DllRegisterServer, DllUnregisterServer};

/// Runs `f`, turning a panic into E_UNEXPECTED: nothing may unwind into the host app.
pub(crate) fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

pub(crate) fn guard_hr(f: impl FnOnce() -> HRESULT) -> HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_UNEXPECTED)
}

/// DirectShow objects are free-threaded; windows-rs doesn't mark these interfaces Send.
pub(crate) struct Sendable<T>(pub(crate) T);
unsafe impl<T> Send for Sendable<T> {}

impl<T> Sendable<T> {
    /// Moves the whole wrapper into a closure (edition 2021 would otherwise capture the non-Send fields).
    pub(crate) fn get(self) -> T {
        self.0
    }
}

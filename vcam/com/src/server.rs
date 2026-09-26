//! COM DLL plumbing for both classes: class factory, DllGetClassObject, and regsvr32 registration. The DirectShow
//! filter gets a COM class plus a VideoInputDeviceCategory entry (that's what makes it a webcam); the MF source only
//! needs its COM class, since the desktop app creates the virtual camera with MFCreateVirtualCamera at runtime.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::core::{implement, w, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, E_UNEXPECTED, HINSTANCE, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::Media::DirectShow::{
    IFilterMapper2, MERIT_DO_NOT_USE, REGFILTER2, REGFILTER2_0, REGFILTER2_0_1, REGFILTERPINS2, REGPINTYPES,
    REG_PINFLAG_B_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_FilterMapper2, CLSID_VideoInputDeviceCategory, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, IClassFactory, IClassFactory_Impl, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{RegDeleteTreeW, RegSetKeyValueW, HKEY_CLASSES_ROOT, REG_SZ};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use lenny_framebuf as fb;

use crate::filter::create_filter;
use crate::media::FORMATS;
use crate::mf::{create_activate, SOURCE_CLSID};

/// lenny_framebuf::DSHOW_FILTER_CLSID.
pub const FILTER_CLSID: GUID = GUID::from_u128(0xBEEEF45F_D1F1_4A35_8F7D_17E756BC2046);

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn guard_hr(f: impl FnOnce() -> Result<()>) -> HRESULT {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => S_OK,
        Ok(Err(e)) => e.code(),
        Err(_) => E_UNEXPECTED,
    }
}

#[no_mangle]
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0, Ordering::Relaxed);
    }
    true.into()
}

#[implement(IClassFactory)]
struct Factory {
    clsid: GUID,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, out: *mut *mut c_void) -> Result<()> {
        let hr = guard_hr(|| {
            if out.is_null() || riid.is_null() {
                return Err(E_POINTER.into());
            }
            unsafe { *out = std::ptr::null_mut() };
            if outer.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let obj: IUnknown =
                if self.clsid == SOURCE_CLSID { create_activate()?.cast()? } else { create_filter()?.cast()? };
            unsafe { obj.query(riid, out).ok() }
        });
        hr.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
/// # Safety
/// COM passes valid pointers or null (null is checked).
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    guard_hr(|| {
        if clsid.is_null() || riid.is_null() || out.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *out = std::ptr::null_mut() };
        let clsid = unsafe { *clsid };
        if clsid != FILTER_CLSID && clsid != SOURCE_CLSID {
            return Err(CLASS_E_CLASSNOTAVAILABLE.into());
        }
        let factory: IClassFactory = Factory { clsid }.into();
        unsafe { factory.query(riid, out).ok() }
    })
}

/// Never unloaded: a streaming thread may still be winding down after the last Release.
// ponytail: no object counting; S_FALSE keeps the DLL mapped until process exit, which is what hosts do anyway.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set_value(key: &str, name: PCWSTR, value: &str) -> Result<()> {
    let key = wide(key);
    let value = wide(value);
    unsafe {
        RegSetKeyValueW(
            HKEY_CLASSES_ROOT,
            PCWSTR(key.as_ptr()),
            name,
            REG_SZ.0,
            Some(value.as_ptr() as *const c_void),
            (value.len() * 2) as u32,
        )
        .ok()
    }
}

/// Windows 11 has MFCreateVirtualCamera; there the MF camera is "Lenny" and this one "Lenny (Classic)"
/// (architecture.md §7.4).
fn friendly_name() -> &'static str {
    let win11 = unsafe {
        LoadLibraryW(w!("mfsensorgroup.dll"))
            .map(|m| GetProcAddress(m, windows::core::s!("MFCreateVirtualCamera")).is_some())
            .unwrap_or(false)
    };
    if win11 {
        "Lenny (Classic)"
    } else {
        "Lenny"
    }
}

fn register_class(clsid: &str, name: &str, dll: &str) -> Result<()> {
    let key = format!("CLSID\\{clsid}");
    set_value(&key, PCWSTR::null(), name)?;
    set_value(&format!("{key}\\InprocServer32"), PCWSTR::null(), dll)?;
    set_value(&format!("{key}\\InprocServer32"), w!("ThreadingModel"), "Both")
}

fn mapper() -> Result<IFilterMapper2> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CLSID_FilterMapper2, None, CLSCTX_INPROC_SERVER)
    }
}

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    guard_hr(|| {
        let mut path = [0u16; 1024];
        let module = HMODULE(MODULE.load(Ordering::Relaxed));
        let n = unsafe { GetModuleFileNameW(Some(module), &mut path) } as usize;
        if n == 0 || n >= path.len() {
            return Err(E_UNEXPECTED.into());
        }
        let path = String::from_utf16_lossy(&path[..n]);
        let name = friendly_name();
        register_class(fb::DSHOW_FILTER_CLSID, name, &path)?;
        register_class(fb::MF_SOURCE_CLSID, "Lenny", &path)?;

        let subtypes: Vec<GUID> = FORMATS.iter().map(|p| p.subtype()).collect();
        let types: Vec<REGPINTYPES> =
            subtypes.iter().map(|s| REGPINTYPES { clsMajorType: &MEDIATYPE_Video, clsMinorType: s }).collect();
        let pin = REGFILTERPINS2 {
            dwFlags: REG_PINFLAG_B_OUTPUT.0 as u32,
            cInstances: 1,
            nMediaTypes: types.len() as u32,
            lpMediaType: types.as_ptr(),
            clsPinCategory: &PIN_CATEGORY_CAPTURE,
            ..Default::default()
        };
        let reg = REGFILTER2 {
            dwVersion: 2,
            // Found through the device enumerator, never picked by graph-building intelligent connect.
            dwMerit: MERIT_DO_NOT_USE.0 as u32,
            Anonymous: REGFILTER2_0 { Anonymous2: REGFILTER2_0_1 { cPins2: 1, rgPins2: &pin } },
        };
        let name = wide(name);
        let instance = wide(fb::DSHOW_FILTER_CLSID);
        unsafe {
            mapper()?.RegisterFilter(
                &FILTER_CLSID,
                PCWSTR(name.as_ptr()),
                None,
                &CLSID_VideoInputDeviceCategory,
                PCWSTR(instance.as_ptr()),
                &reg,
            )
        }
    })
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    guard_hr(|| {
        let instance = wide(fb::DSHOW_FILTER_CLSID);
        if let Ok(m) = mapper() {
            let _ = unsafe {
                m.UnregisterFilter(&CLSID_VideoInputDeviceCategory, PCWSTR(instance.as_ptr()), &FILTER_CLSID)
            };
        }
        for clsid in [fb::DSHOW_FILTER_CLSID, fb::MF_SOURCE_CLSID] {
            let key = wide(&format!("CLSID\\{clsid}"));
            let _ = unsafe { RegDeleteTreeW(HKEY_CLASSES_ROOT, PCWSTR(key.as_ptr())) };
        }
        Ok(())
    })
}

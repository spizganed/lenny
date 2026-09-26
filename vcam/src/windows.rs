//! Windows: one writer for both virtual cameras. Frames go into the `lenny_framebuf` shared-memory ring
//! (architecture.md §7.3); the DirectShow filter DLL (Win10+11, loaded by Zoom/Chrome/OBS...) and the MF media
//! source DLL (Win11, loaded by Frame Server) read it. This side also registers the MF virtual camera with
//! `MFCreateVirtualCamera` for as long as the app runs (Win11 22000+; looked up at runtime, Win10 has no such
//! export). Both DLLs must be registered (installer, or `regsvr32` for development) for apps to see them.

use lenny_framebuf as fb;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFShutdown, MFStartup, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, HKEY, HKEY_CLASSES_ROOT, KEY_READ};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent};

use crate::{FrameFormat, IVirtualCamera, Result};

/// DACL for the shared objects (architecture.md §7.3): SYSTEM and Administrators full, interactive users
/// read/write (the app), LOCAL SERVICE read (Frame Server), and a low-integrity read label for sandboxed consumers.
const SDDL: PCWSTR = w!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)(A;;GR;;;LS)S:(ML;;NRNX;;;LW)");

pub struct WindowsCamera {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
    writer: Option<fb::Writer>,
    global: bool,
    mf: Option<IMFVirtualCamera>,
    mf_status: String,
    frames: i64,
}

// Handles and the COM pointer are only used from the virtual-camera thread that owns this object.
unsafe impl Send for WindowsCamera {}

impl WindowsCamera {
    pub fn new() -> Self {
        WindowsCamera {
            mapping: HANDLE::default(),
            view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
            event: HANDLE::default(),
            writer: None,
            global: false,
            mf: None,
            mf_status: String::new(),
            frames: 0,
        }
    }

    /// Creates the mapping + event, `Global\` first, `Local\` if this user can't (no SeCreateGlobalPrivilege and no
    /// broker service yet: DirectShow consumers in this session still work, the MF camera doesn't).
    fn create_shared(&mut self) -> Result<()> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(SDDL, SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| format!("security descriptor: {e}"))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let size = fb::mapping_bytes() as u64;
        let mut last = String::new();
        for (global, map_name, event_name) in
            [(true, fb::MAPPING_NAME, fb::EVENT_NAME), (false, fb::LOCAL_MAPPING_NAME, fb::LOCAL_EVENT_NAME)]
        {
            let name = HSTRING::from(map_name);
            let m = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    Some(&sa),
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    PCWSTR(name.as_ptr()),
                )
            };
            match m {
                Ok(m) => {
                    let ev =
                        unsafe { CreateEventW(Some(&sa), false, false, PCWSTR(HSTRING::from(event_name).as_ptr())) };
                    self.mapping = m;
                    self.event = ev.unwrap_or_default();
                    self.global = global;
                    break;
                }
                Err(e) => last = format!("{map_name}: {e}"),
            }
        }
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if self.mapping.is_invalid() {
            return Err(format!("can't create the shared frame buffer ({last})"));
        }
        self.view = unsafe { MapViewOfFile(self.mapping, FILE_MAP_ALL_ACCESS, 0, 0, fb::mapping_bytes()) };
        if self.view.Value.is_null() {
            return Err(format!("MapViewOfFile: {:?}", unsafe { GetLastError() }));
        }
        Ok(())
    }

    /// Win11: have Frame Server expose our MF media source as a camera while the app runs.
    fn register_mf(&mut self) {
        type Create = unsafe extern "system" fn(
            i32,
            i32,
            i32,
            PCWSTR,
            PCWSTR,
            *const windows::core::GUID,
            u32,
            *mut Option<IMFVirtualCamera>,
        ) -> windows::core::HRESULT;
        if !self.global {
            self.mf_status = "MF camera off: no Global\\ frame buffer (Frame Server can't see Local\\)".into();
            return;
        }
        if !clsid_registered(fb::MF_SOURCE_CLSID) {
            self.mf_status = "MF camera not registered (run the installer or regsvr32 lenny_vcam_mf.dll)".into();
            return;
        }
        unsafe {
            let Ok(lib) = LoadLibraryW(w!("mfsensorgroup.dll")) else {
                self.mf_status = "MF camera needs Windows 11".into();
                return;
            };
            let Some(f) = GetProcAddress(lib, windows::core::s!("MFCreateVirtualCamera")) else {
                self.mf_status = "MF camera needs Windows 11 (no MFCreateVirtualCamera)".into();
                return;
            };
            let create: Create = std::mem::transmute(f);
            // The virtual-camera thread may not have COM yet; MTA is fine for these calls.
            let _ =
                windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                self.mf_status = "MFStartup failed".into();
                return;
            }
            let id = HSTRING::from(fb::MF_SOURCE_CLSID);
            let mut cam = None;
            let hr = create(
                MFVirtualCameraType_SoftwareCameraSource.0,
                MFVirtualCameraLifetime_Session.0,
                MFVirtualCameraAccess_CurrentUser.0,
                w!("Lenny"),
                PCWSTR(id.as_ptr()),
                std::ptr::null(),
                0,
                &mut cam,
            );
            match (hr.ok(), cam) {
                (Ok(()), Some(cam)) => match cam.Start(None) {
                    Ok(()) => {
                        self.mf_status = "MF camera \"Lenny\" on".into();
                        self.mf = Some(cam);
                    }
                    Err(e) => self.mf_status = format!("MF camera start failed: {e}"),
                },
                (r, _) => self.mf_status = format!("MFCreateVirtualCamera failed: {r:?}"),
            }
        }
    }
}

impl Default for WindowsCamera {
    fn default() -> Self {
        Self::new()
    }
}

fn clsid_registered(clsid: &str) -> bool {
    let key = HSTRING::from(format!("CLSID\\{clsid}\\InprocServer32"));
    let mut h = HKEY::default();
    let ok = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, PCWSTR(key.as_ptr()), None, KEY_READ, &mut h) }.is_ok();
    if ok {
        unsafe {
            let _ = RegCloseKey(h);
        }
    }
    ok
}

impl IVirtualCamera for WindowsCamera {
    fn open(&mut self, f: FrameFormat) -> Result<()> {
        if self.view.Value.is_null() {
            self.create_shared()?;
        }
        let view = unsafe { fb::View::new(self.view.Value as *mut u8, fb::mapping_bytes()) }.ok_or("bad view")?;
        let writer = fb::Writer::new(view, f.width, f.height, unsafe { GetCurrentProcessId() })
            .ok_or_else(|| format!("{}x{} doesn't fit the frame buffer", f.width, f.height))?;
        writer.set_state(fb::STATE_LIVE);
        writer.heartbeat(unsafe { GetTickCount64() });
        self.writer = Some(writer);
        if self.mf.is_none() {
            self.register_mf();
        }
        Ok(())
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let w = self.writer.as_mut().ok_or("not open")?;
        self.frames += 1;
        let pts = self.frames * 333_333; // 100 ns units at 30 fps; readers restamp against their own clock
        if !w.write_i420(frame, pts, unsafe { GetTickCount64() }) {
            return Err(format!("frame is {} bytes, not I420 at the open size", frame.len()));
        }
        if !self.event.is_invalid() {
            unsafe {
                let _ = SetEvent(self.event);
            }
        }
        Ok(())
    }

    fn close(&mut self) {
        if let Some(w) = &self.writer {
            w.set_state(fb::STATE_NO_SOURCE);
        }
        self.writer = None;
        if let Some(cam) = self.mf.take() {
            unsafe {
                let _ = cam.Remove();
                let _ = MFShutdown();
            }
        }
        unsafe {
            if !self.view.Value.is_null() {
                let _ = UnmapViewOfFile(self.view);
                self.view = MEMORY_MAPPED_VIEW_ADDRESS::default();
            }
            if !self.event.is_invalid() {
                let _ = CloseHandle(self.event);
            }
            if !self.mapping.is_invalid() {
                let _ = CloseHandle(self.mapping);
            }
        }
        self.mapping = HANDLE::default();
        self.event = HANDLE::default();
    }

    fn is_real(&self) -> bool {
        self.writer.is_some() && clsid_registered(fb::DSHOW_FILTER_CLSID)
    }

    fn describe(&self) -> String {
        let buffer = if self.global { "Global\\" } else { "Local\\ (DirectShow only)" };
        let dshow = if clsid_registered(fb::DSHOW_FILTER_CLSID) {
            "DirectShow \"Lenny (Classic)\" registered"
        } else {
            "DirectShow filter not registered (regsvr32 lenny_vcam_dshow.dll)"
        };
        format!("shared frames in {buffer}; {dshow}; {}", self.mf_status)
    }
}

impl Drop for WindowsCamera {
    fn drop(&mut self) {
        self.close();
    }
}

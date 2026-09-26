//! The formats the pin offers, as DirectShow media types. The shared frame buffer is a fixed 1280x720 canvas
//! (desktop `receiver.rs` VCAM), so that's the one size offered, in NV12 and YUY2 (Chrome prefers YUY2/NV12;
//! Zoom and Teams take both).
// ponytail: one size. Add 1920x1080 (and scaling in the pin) once the app writes a 1080p canvas.

use windows::core::GUID;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::MediaFoundation::{
    FORMAT_VideoInfo, MEDIATYPE_Video, AM_MEDIA_TYPE, MEDIASUBTYPE_NV12, MEDIASUBTYPE_YUY2, VIDEOINFOHEADER,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};

pub const WIDTH: i32 = 1280;
pub const HEIGHT: i32 = 720;
/// 30 fps in 100 ns units.
pub const FRAME_TIME: i64 = 333_333;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pixel {
    Nv12,
    Yuy2,
}

pub const FORMATS: [Pixel; 2] = [Pixel::Nv12, Pixel::Yuy2];

impl Pixel {
    pub fn subtype(self) -> GUID {
        match self {
            Pixel::Nv12 => MEDIASUBTYPE_NV12,
            Pixel::Yuy2 => MEDIASUBTYPE_YUY2,
        }
    }
    fn fourcc(self) -> u32 {
        u32::from_le_bytes(*match self {
            Pixel::Nv12 => b"NV12",
            Pixel::Yuy2 => b"YUY2",
        })
    }
    fn bits(self) -> u16 {
        match self {
            Pixel::Nv12 => 12,
            Pixel::Yuy2 => 16,
        }
    }
    pub fn image_size(self) -> usize {
        WIDTH as usize * HEIGHT as usize * self.bits() as usize / 8
    }
}

fn vih(p: Pixel) -> VIDEOINFOHEADER {
    VIDEOINFOHEADER {
        dwBitRate: (p.image_size() * 8 * 30) as u32,
        AvgTimePerFrame: FRAME_TIME,
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: WIDTH,
            biHeight: HEIGHT,
            biPlanes: 1,
            biBitCount: p.bits(),
            biCompression: p.fourcc(),
            biSizeImage: p.image_size() as u32,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A media type whose format block is CoTaskMem-allocated, as DirectShow callers expect (they free it with
/// DeleteMediaType / FreeMediaType). None if allocation fails.
pub fn media_type(p: Pixel) -> Option<AM_MEDIA_TYPE> {
    let size = std::mem::size_of::<VIDEOINFOHEADER>();
    let fmt = unsafe { CoTaskMemAlloc(size) } as *mut VIDEOINFOHEADER;
    if fmt.is_null() {
        return None;
    }
    unsafe { fmt.write(vih(p)) };
    Some(AM_MEDIA_TYPE {
        majortype: MEDIATYPE_Video,
        subtype: p.subtype(),
        bFixedSizeSamples: true.into(),
        bTemporalCompression: false.into(),
        lSampleSize: p.image_size() as u32,
        formattype: FORMAT_VideoInfo,
        pUnk: std::mem::ManuallyDrop::new(None),
        cbFormat: size as u32,
        pbFormat: fmt as *mut u8,
    })
}

/// A whole AM_MEDIA_TYPE on the CoTaskMem heap (IEnumMediaTypes::Next, GetFormat, GetStreamCaps).
pub fn alloc_media_type(p: Pixel) -> *mut AM_MEDIA_TYPE {
    let Some(mt) = media_type(p) else { return std::ptr::null_mut() };
    let out = unsafe { CoTaskMemAlloc(std::mem::size_of::<AM_MEDIA_TYPE>()) } as *mut AM_MEDIA_TYPE;
    if out.is_null() {
        free_format(&mt);
        return out;
    }
    unsafe { out.write(mt) };
    out
}

/// Frees the format block of a media type we created (FreeMediaType).
pub fn free_format(mt: &AM_MEDIA_TYPE) {
    if !mt.pbFormat.is_null() {
        unsafe { CoTaskMemFree(Some(mt.pbFormat as *const _)) };
    }
}

/// Which of our formats `mt` asks for. Partial types are fine (GUID_NULL = "any"), as graph builders send them.
/// A given format block must describe our size.
///
/// # Safety
/// `mt` points to a valid AM_MEDIA_TYPE whose pbFormat holds cbFormat bytes.
pub unsafe fn match_type(mt: *const AM_MEDIA_TYPE) -> Option<Pixel> {
    let mt = mt.as_ref()?;
    let any = GUID::zeroed();
    if mt.majortype != MEDIATYPE_Video && mt.majortype != any {
        return None;
    }
    let p = if mt.subtype == any { Pixel::Nv12 } else { *FORMATS.iter().find(|p| p.subtype() == mt.subtype)? };
    if mt.formattype == FORMAT_VideoInfo
        && !mt.pbFormat.is_null()
        && mt.cbFormat as usize >= std::mem::size_of::<VIDEOINFOHEADER>()
    {
        let v = std::ptr::read_unaligned(mt.pbFormat as *const VIDEOINFOHEADER);
        if v.bmiHeader.biWidth != WIDTH || v.bmiHeader.biHeight.abs() != HEIGHT {
            return None;
        }
    } else if mt.formattype != any && mt.formattype != FORMAT_VideoInfo {
        return None;
    }
    Some(p)
}

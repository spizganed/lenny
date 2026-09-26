//! Both cameras driven the way their hosts drive them, in-process (no registration needed): the DirectShow filter in
//! a real filter graph, the MF source through IMFActivate like Frame Server. No desktop app is running, so both must
//! deliver the placeholder. Windows only (CI's windows job).
#![cfg(windows)]

use std::time::{Duration, Instant};

use windows::core::{w, IUnknown, Interface, GUID};
use windows::Win32::Media::DirectShow::*;
use windows::Win32::Media::KernelStreaming::IKsPropertySet;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IClassFactory, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

const FILTER: GUID = GUID::from_u128(0xBEEEF45F_D1F1_4A35_8F7D_17E756BC2046);
const SOURCE: GUID = GUID::from_u128(0x5F0F9024_D043_45C1_B2F6_03DAB6CE130A);

fn create<T: Interface>(clsid: GUID) -> T {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut p = std::ptr::null_mut();
        lenny_vcam_com::DllGetClassObject(&clsid, &IClassFactory::IID, &mut p).unwrap();
        IClassFactory::from_raw(p).CreateInstance(None).unwrap()
    }
}

fn capture_pin(filter: &IBaseFilter) -> IPin {
    unsafe {
        let pins = filter.EnumPins().unwrap();
        let mut pin = [None];
        pins.Next(&mut pin, None).unwrap();
        pin[0].take().unwrap()
    }
}

#[test]
fn filter_offers_a_capture_pin() {
    let filter: IBaseFilter = create(FILTER);
    let pin = capture_pin(&filter);
    unsafe {
        assert_eq!(pin.QueryDirection().unwrap(), PINDIR_OUTPUT);

        let mut category = GUID::zeroed();
        let mut got = 0;
        let ks: IKsPropertySet = pin.cast().unwrap();
        ks.Get(
            &AMPROPSETID_Pin,
            AMPROPERTY_PIN_CATEGORY.0 as u32,
            std::ptr::null(),
            0,
            &mut category as *mut GUID as *mut _,
            std::mem::size_of::<GUID>() as u32,
            &mut got,
        )
        .unwrap();
        assert_eq!(category, PIN_CATEGORY_CAPTURE);

        let config: IAMStreamConfig = pin.cast().unwrap();
        let (mut count, mut size) = (0, 0);
        config.GetNumberOfCapabilities(&mut count, &mut size).unwrap();
        assert_eq!(count, 2);
        let mut caps = vec![0u8; size as usize];
        for i in 0..count {
            let mut mt = std::ptr::null_mut();
            config.GetStreamCaps(i, &mut mt, caps.as_mut_ptr()).unwrap();
            let vih = &*((*mt).pbFormat as *const VIDEOINFOHEADER);
            assert_eq!((vih.bmiHeader.biWidth, vih.bmiHeader.biHeight), (1280, 720));
            assert!([MEDIASUBTYPE_NV12, MEDIASUBTYPE_YUY2].contains(&(*mt).subtype));
            CoTaskMemFree(Some((*mt).pbFormat as *const _));
            CoTaskMemFree(Some(mt as *const _));
        }
    }
}

#[test]
fn filter_streams_in_a_graph() {
    let filter: IBaseFilter = create(FILTER);
    unsafe {
        let graph: IGraphBuilder = CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER).unwrap();
        graph.AddFilter(&filter, w!("Lenny")).unwrap();
        graph.Render(&capture_pin(&filter)).unwrap(); // connects a renderer: types, allocator
        let control: IMediaControl = graph.cast().unwrap();
        let _ = control.Run(); // S_FALSE while the renderer prerolls
        assert_eq!(control.GetState(5000).unwrap(), State_Running.0);
        std::thread::sleep(Duration::from_millis(500)); // placeholder frames flowing
        control.Stop().unwrap();
    }
}

fn wait_event(events: &IMFMediaEventGenerator, want: MF_EVENT_TYPE) -> PROPVARIANT {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no event {want:?}");
        let Ok(e) = (unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) }) else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        if unsafe { e.GetType() }.unwrap() == want.0 as u32 {
            return unsafe { e.GetValue() }.unwrap();
        }
    }
}

#[test]
fn mf_source_delivers_samples() {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).unwrap() };
    let activate: IMFActivate = create(SOURCE);
    let source: IMFMediaSource = unsafe { activate.ActivateObject() }.unwrap();
    unsafe {
        assert_eq!(source.GetCharacteristics().unwrap() & MFMEDIASOURCE_IS_LIVE.0 as u32, 1);
        let ex: IMFMediaSourceEx = source.cast().unwrap();
        let attrs = ex.GetStreamAttributes(0).unwrap();
        assert_eq!(
            attrs.GetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY).unwrap(),
            windows::Win32::Media::KernelStreaming::PINNAME_VIDEO_CAPTURE
        );

        let pd = source.CreatePresentationDescriptor().unwrap();
        source.Start(&pd, std::ptr::null(), &PROPVARIANT::default()).unwrap();
        let stream: IMFMediaStream =
            IUnknown::try_from(&wait_event(&source.cast().unwrap(), MENewStream)).unwrap().cast().unwrap();
        wait_event(&source.cast().unwrap(), MESourceStarted);

        let started = Instant::now();
        for _ in 0..3 {
            stream.RequestSample(None).unwrap();
            let sample: IMFSample =
                IUnknown::try_from(&wait_event(&stream.cast().unwrap(), MEMediaSample)).unwrap().cast().unwrap();
            assert_eq!(sample.GetTotalLength().unwrap(), 1280 * 720 * 3 / 2);
        }
        assert!(started.elapsed() >= Duration::from_millis(60), "paced to 30 fps");

        source.Stop().unwrap();
        source.Shutdown().unwrap();
        activate.ShutdownObject().unwrap();
    }
}

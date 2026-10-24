//! The encoder logic against a mock libnvmpi session: sequencing, back-pressure, sample entries,
//! settings and cleanup, without hardware.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use filmcraft_codecs::hw::NalCodec;
use filmcraft_export::{
    BitrateMode, ColorSignal, EncoderFrame, ExportError, ExportSettings, FieldOrder, Format, H264Pass, H264Profile, HardwareEncoding, VideoEncoder,
};
use filmcraft_isobmff::CodecConfig;
use filmcraft_time::FrameRate;

use super::backend::{EncBackend, EncOut, EncPacket, EncParams};
use super::cap::try_take;
use super::enc::{NvmpiEncoder, factory_with, params};
use super::ffi::NVMPI_ERR_EAGAIN;

// Parameter sets of a real 1280x720 HEVC Main stream (the ones `nvenc::hevc` is tested with).
const VPS: &str = "40010c01ffff01600000030090000003000003005d9940c0000003004000000614";
const SPS: &str = "42010101600000030090000003000003005da00280802e1f1396654a421191bff0c05a8080808a0000030002000003003010";
const PPS: &str = "4401c1937c0cc9";

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len()).step_by(2).filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
}

#[derive(Default)]
struct Log {
    /// (plane sizes, strides, pts) of every accepted picture.
    frames: Vec<([usize; 3], [u32; 3], i64)>,
    eagain_seen: usize,
    eos: usize,
    counter_at_drop: Option<usize>,
}

/// A session that answers a picture with its access unit after `latency` further pictures, and
/// refuses (`EAGAIN`) a picture while `capacity` are in flight.
struct Mock {
    log: Arc<Mutex<Log>>,
    header: Vec<u8>,
    idr: Vec<u8>,
    latency: usize,
    capacity: usize,
    pending: VecDeque<EncPacket>,
    ready: VecDeque<EncPacket>,
    count: i64,
    eos: bool,
    put_error: Option<i32>,
    refuse_always: bool,
    silent: bool,
    retime: bool,
    drop_one_at_eos: bool,
    counter: Option<&'static AtomicUsize>,
}

fn h264_header() -> Vec<u8> {
    annex_b(&[&[0x67, 0x64, 0x00, 0x28, 0xAC], &[0x68, 0xEE, 0x3C, 0x80]])
}

impl Mock {
    fn h264(latency: usize) -> (Self, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let m = Mock {
            log: log.clone(),
            header: h264_header(),
            idr: annex_b(&[&[0x65, 0x88, 0x84, 0x21]]),
            latency,
            capacity: 100,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            count: 0,
            eos: false,
            put_error: None,
            refuse_always: false,
            silent: false,
            retime: false,
            drop_one_at_eos: false,
            counter: None,
        };
        (m, log)
    }

    fn hevc(latency: usize) -> (Self, Arc<Mutex<Log>>) {
        let (mut m, log) = Self::h264(latency);
        m.header = annex_b(&[&unhex(VPS), &unhex(SPS), &unhex(PPS)]);
        m.idr = annex_b(&[&[0x26, 0x01, 0xAF, 0x21]]);
        (m, log)
    }
}

impl EncBackend for Mock {
    fn put_frame(&mut self, planes: [&[u8]; 3], strides: [u32; 3], pts: i64) -> i32 {
        if let Some(code) = self.put_error {
            return code;
        }
        if self.refuse_always || self.pending.len() + self.ready.len() >= self.capacity {
            self.log.lock().unwrap().eagain_seen += 1;
            return NVMPI_ERR_EAGAIN;
        }
        self.log.lock().unwrap().frames.push(([planes[0].len(), planes[1].len(), planes[2].len()], strides, pts));
        let data = if self.count == 0 { [self.header.clone(), self.idr.clone()].concat() } else { annex_b(&[&[0x41, 0x9A, 0x01, 0x22]]) };
        let stamp = if self.retime { pts + 1 } else { pts };
        self.pending.push_back(EncPacket { data, key: self.count == 0, pts: stamp });
        self.count += 1;
        while self.pending.len() > self.latency {
            let p = self.pending.pop_front().unwrap();
            self.ready.push_back(p);
        }
        0
    }

    fn put_eos(&mut self) -> i32 {
        self.eos = true;
        self.log.lock().unwrap().eos += 1;
        if self.drop_one_at_eos {
            self.pending.pop_back();
        }
        0
    }

    fn get_packet(&mut self, wait: bool) -> EncOut {
        if self.silent {
            return EncOut::None;
        }
        if let Some(p) = self.ready.pop_front() {
            return EncOut::Packet(p);
        }
        // a blocking read gives the hardware time to finish its oldest picture
        if (wait || self.eos)
            && let Some(p) = self.pending.pop_front()
        {
            return EncOut::Packet(p);
        }
        if self.eos { EncOut::Eos } else { EncOut::None }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.log.lock().unwrap().counter_at_drop = self.counter.map(|c| c.load(Ordering::Acquire));
    }
}

fn frame(w: u32, h: u32, index: u64, rgba: &[u8]) -> EncoderFrame<'_> {
    EncoderFrame { width: w, height: h, rgba, hdr: None, index }
}

fn encoder(mock: Mock, codec: NalCodec, w: u32, h: u32) -> NvmpiEncoder {
    NvmpiEncoder::with_backend(Box::new(mock), codec, w, h, FrameRate { num: 24000, den: 1001 }, None)
}

#[test]
fn h264_packets_are_length_prefixed_in_order_and_the_entry_has_the_first_units_parameter_sets() {
    let (mock, log) = Mock::h264(1);
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    let rgba = vec![90u8; 16 * 16 * 4];
    // latency 1: the first picture's unit is not out yet, but the call waits for it (the container is
    // created from the sample entry right after the first batch)
    let first = e.encode(&frame(16, 16, 0, &rgba)).unwrap();
    assert_eq!(first.len(), 1);
    let entry = e.sample_entry();
    let CodecConfig::Avc(avc) = &entry.codec else { panic!("avcC") };
    assert_eq!((avc.sps.as_slice(), avc.pps.as_slice()), (&[vec![0x67, 0x64, 0x00, 0x28, 0xAC]][..], &[vec![0x68, 0xEE, 0x3C, 0x80]][..]));
    assert_eq!((avc.profile, avc.level, avc.length_size), (0x64, 0x28, 4));
    let mut all = first;
    for i in 1..4u64 {
        all.extend(e.encode(&frame(16, 16, i, &rgba)).unwrap());
    }
    all.extend(e.flush().unwrap());
    assert_eq!(all.len(), 4);
    // parameter sets are stripped (they live in the avcC); the slice is length-prefixed
    assert_eq!(all[0].data, [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x21]);
    assert_eq!(all[1].data, [0, 0, 0, 4, 0x41, 0x9A, 0x01, 0x22]);
    assert_eq!(all.iter().map(|p| p.key).collect::<Vec<_>>(), [true, false, false, false]);
    assert!(all.iter().all(|p| p.duration == 1001 && p.composition_offset == 0));
    assert_eq!(e.timescale(), 24000);
    // the library got planar 4:2:0 with the picture index as its timestamp
    let log = log.lock().unwrap();
    assert_eq!(log.frames.len(), 4);
    assert_eq!(log.frames[2], ([256, 64, 64], [16, 8, 8], 2));
    assert_eq!(log.eos, 1);
}

#[test]
fn a_full_queue_is_drained_and_the_same_picture_retried_without_loss() {
    let (mut mock, log) = Mock::h264(100);
    mock.capacity = 2;
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    let rgba = vec![10u8; 16 * 16 * 4];
    let mut all = Vec::new();
    for i in 0..6u64 {
        all.extend(e.encode(&frame(16, 16, i, &rgba)).unwrap());
    }
    all.extend(e.flush().unwrap());
    assert_eq!(all.len(), 6, "every picture comes out exactly once");
    assert!(log.lock().unwrap().eagain_seen >= 3, "the queue did fill");
    let pts: Vec<_> = log.lock().unwrap().frames.iter().map(|f| f.2).collect();
    assert_eq!(pts, [0, 1, 2, 3, 4, 5]);

    // a library that never takes the picture is an error after a bounded number of retries
    let (mut mock, _) = Mock::h264(0);
    mock.refuse_always = true;
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    assert!(matches!(e.encode(&frame(16, 16, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("-11")));
}

#[test]
fn mistimed_dropped_silent_and_refused_pictures_are_errors() {
    let rgba = vec![10u8; 16 * 16 * 4];
    let (mut mock, _) = Mock::h264(0);
    mock.retime = true;
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    assert!(matches!(e.encode(&frame(16, 16, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("reordered or retimed")));

    let (mut mock, _) = Mock::h264(3);
    mock.drop_one_at_eos = true;
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    // latency 3: the first picture's unit is only out after the wait, the rest at the end of the stream
    for i in 0..3u64 {
        e.encode(&frame(16, 16, i, &rgba)).unwrap();
    }
    assert!(matches!(e.flush(), Err(ExportError::Encode(m)) if m.contains("dropped")));

    let (mut mock, _) = Mock::h264(0);
    mock.silent = true;
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    assert!(matches!(e.encode(&frame(16, 16, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("no output")));

    let (mut mock, _) = Mock::h264(0);
    mock.put_error = Some(-22);
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    assert!(matches!(e.encode(&frame(16, 16, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("-22")));

    let (mock, _) = Mock::h264(0);
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    assert!(e.encode(&frame(32, 16, 0, &rgba)).is_err(), "a picture of the wrong size");
    let hdr = [0.5f32; 16 * 16 * 3];
    let with_hdr = EncoderFrame { hdr: Some(&hdr), ..frame(16, 16, 0, &rgba) };
    assert!(matches!(e.encode(&with_hdr), Err(ExportError::Unsupported(_))));
}

#[test]
fn flush_ends_the_stream_once_and_nothing_goes_in_after_it() {
    let (mock, log) = Mock::h264(0);
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    let rgba = vec![1u8; 16 * 16 * 4];
    e.encode(&frame(16, 16, 0, &rgba)).unwrap();
    assert!(e.flush().unwrap().is_empty());
    assert!(e.flush().unwrap().is_empty(), "a second flush does nothing");
    assert_eq!(log.lock().unwrap().eos, 1);
    assert!(e.encode(&frame(16, 16, 1, &rgba)).is_err());
}

#[test]
fn hevc_entry_is_built_from_the_vps_sps_pps_and_checks_the_size() {
    let (mock, _) = Mock::hevc(0);
    let mut e = encoder(mock, NalCodec::Hevc, 1280, 720);
    let rgba = vec![60u8; 1280 * 720 * 4];
    let out = e.encode(&frame(1280, 720, 0, &rgba)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].data, [0, 0, 0, 4, 0x26, 0x01, 0xAF, 0x21], "VPS / SPS / PPS are in the hvcC, not the sample");
    assert!(out[0].key, "an IDR slice is a keyframe whatever the flag says");
    let entry = e.sample_entry();
    let CodecConfig::Hevc(cfg) = &entry.codec else { panic!("hvcC") };
    assert_eq!((cfg.vps(), cfg.sps(), cfg.pps()), (vec![&unhex(VPS)[..]], vec![&unhex(SPS)[..]], vec![&unhex(PPS)[..]]));
    assert_eq!((cfg.bit_depth_luma, cfg.general_profile_idc), (8, 1));

    // the same SPS for another export size is a stream that is not the one asked for
    let (mock, _) = Mock::hevc(0);
    let mut e = encoder(mock, NalCodec::Hevc, 1920, 1080);
    let rgba = vec![60u8; 1920 * 1080 * 4];
    assert!(matches!(e.encode(&frame(1920, 1080, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("1280x720")));

    // a first access unit without parameter sets cannot make a sample entry
    let (mut mock, _) = Mock::hevc(0);
    mock.header = Vec::new();
    let mut e = encoder(mock, NalCodec::Hevc, 1280, 720);
    let rgba = vec![60u8; 1280 * 720 * 4];
    assert!(matches!(e.encode(&frame(1280, 720, 0, &rgba)), Err(ExportError::Encode(m)) if m.contains("VPS")));
}

fn settings(format: Format) -> ExportSettings {
    ExportSettings { format, hardware_encoding: HardwareEncoding::Auto, bitrate_kbps: 4000, keyframe_distance: Some(48), ..Default::default() }
}

#[test]
fn the_settings_map_to_session_parameters() {
    let p = params(
        Format::H264,
        1920,
        1080,
        FrameRate::FPS_24,
        &ExportSettings { h264_profile: H264Profile::Main, h264_level: Some(41), ..settings(Format::H264) },
    )
    .unwrap();
    assert_eq!((p.codec, p.width, p.height, p.fps), (NalCodec::H264, 1920, 1080, (24, 1)));
    assert_eq!((p.bitrate_bps, p.peak_bitrate_bps, p.keyint, p.h264_profile, p.level), (4_000_000, 6_000_000, 48, 77, 41));
    // H.265: the H.264 level and profile mean nothing
    let h = params(Format::Hevc, 1280, 720, FrameRate::FPS_24, &ExportSettings { h264_level: Some(41), ..settings(Format::Hevc) }).unwrap();
    assert_eq!((h.codec, h.level), (NalCodec::Hevc, 0));
    // CBR / VBR, and a default GOP of two seconds
    let cbr = ExportSettings { bitrate_mode: BitrateMode::Cbr, keyframe_distance: None, ..settings(Format::H264) };
    let p = params(Format::H264, 640, 360, FrameRate { num: 30000, den: 1001 }, &cbr).unwrap();
    assert!(!p.vbr);
    assert_eq!(p.keyint, 60);
    assert!(params(Format::H264, 640, 360, FrameRate::FPS_24, &ExportSettings { bitrate_mode: BitrateMode::Vbr1Pass, ..settings(Format::H264) }).unwrap().vbr);
    // the bitrate ceiling does not wrap
    let big = ExportSettings { bitrate_kbps: u32::MAX, ..settings(Format::H264) };
    let p = params(Format::H264, 640, 360, FrameRate::FPS_24, &big).unwrap();
    assert_eq!((p.bitrate_bps, p.peak_bitrate_bps), (u32::MAX, u32::MAX));
}

#[test]
fn what_nvmpi_cannot_write_is_declined_with_a_reason() {
    let ok = settings(Format::Hevc);
    for (what, s) in [
        ("HDR", ExportSettings { signal: ColorSignal::PQ, ..ok.clone() }),
        ("an unwritable colour signal", ExportSettings { signal: ColorSignal { primaries: 9, transfer: 1, matrix: 1 }, ..ok.clone() }),
        ("two-pass", ExportSettings { bitrate_mode: BitrateMode::Vbr2Pass, ..ok.clone() }),
        ("CRF", ExportSettings { bitrate_mode: BitrateMode::Crf, ..ok.clone() }),
        ("analysis pass", ExportSettings { h264_pass: H264Pass::First, ..ok.clone() }),
        ("interlaced", ExportSettings { field_order: FieldOrder::UpperFirst, ..ok.clone() }),
        ("non-square pixels", ExportSettings { pixel_aspect: Some((4, 3)), ..ok.clone() }),
        ("MXF", ExportSettings { format: Format::MxfOp1a, ..ok.clone() }),
    ] {
        assert!(params(Format::Hevc, 1280, 720, FrameRate::FPS_24, &s).is_err(), "{what}");
    }
    assert!(params(Format::ProRes, 1280, 720, FrameRate::FPS_24, &ok).is_err());
    // hostile sizes and frame rates
    for (w, h) in [(0, 64), (64, 0), (641, 360), (640, 361), (70_000, 64), (8194, 64), (64, 8194)] {
        assert!(params(Format::Hevc, w, h, FrameRate::FPS_24, &ok).is_err(), "{w}x{h}");
    }
    for rate in [FrameRate { num: 0, den: 1 }, FrameRate { num: 24, den: 0 }, FrameRate { num: -24, den: 1 }, FrameRate { num: i64::MAX, den: 1 }] {
        assert!(params(Format::Hevc, 1280, 720, rate, &ok).is_err(), "{rate:?}");
    }
}

#[test]
fn the_factory_leaves_h264_to_the_software_encoder_and_makes_hevc_an_error() {
    let never = |_: &EncParams| -> Result<_, String> { panic!("must not open a session") };
    // Hardware encoding Off, other formats and settings nvmpi cannot take never open a session
    let off = ExportSettings { hardware_encoding: HardwareEncoding::Off, ..settings(Format::H264) };
    assert!(factory_with(Format::H264, 640, 360, FrameRate::FPS_24, &off, never).is_none());
    assert!(factory_with(Format::ProRes, 640, 360, FrameRate::FPS_24, &settings(Format::ProRes), never).is_none());
    assert!(factory_with(Format::H264, 641, 360, FrameRate::FPS_24, &settings(Format::H264), never).is_none());
    // H.265 has no software encoder: the toggle does not apply and a request nvmpi cannot take is an error
    let hevc_off = ExportSettings { hardware_encoding: HardwareEncoding::Off, ..settings(Format::Hevc) };
    assert!(matches!(factory_with(Format::Hevc, 641, 360, FrameRate::FPS_24, &hevc_off, never), Some(Err(ExportError::Unsupported(m))) if m.contains("even")));

    // a session that does not open: H.264 declines, H.265 says why
    let busy = |_: &EncParams| -> Result<_, String> { Err("device busy".into()) };
    assert!(factory_with(Format::H264, 640, 360, FrameRate::FPS_24, &settings(Format::H264), busy).is_none());
    assert!(
        matches!(factory_with(Format::Hevc, 640, 360, FrameRate::FPS_24, &settings(Format::Hevc), busy), Some(Err(ExportError::Unsupported(m))) if m.contains("device busy"))
    );

    // an open session makes the encoder
    let open = |p: &EncParams| {
        assert_eq!((p.codec, p.width, p.height), (NalCodec::H264, 640, 360));
        let (mock, _) = Mock::h264(0);
        Ok((Box::new(mock) as Box<dyn EncBackend>, None))
    };
    let Some(Ok(enc)) = factory_with(Format::H264, 640, 360, FrameRate::FPS_24, &settings(Format::H264), open) else { panic!("an encoder") };
    assert_eq!(enc.timescale(), 24);
}

#[test]
fn the_encoder_slot_is_given_back_after_the_session_closed() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let permit = try_take(&COUNT, 1).expect("slot");
    assert!(try_take(&COUNT, 1).is_none());
    let (mut mock, log) = Mock::h264(0);
    mock.counter = Some(&COUNT);
    let e = NvmpiEncoder::with_backend(Box::new(mock), NalCodec::H264, 16, 16, FrameRate::FPS_24, Some(permit));
    drop(e);
    assert_eq!(log.lock().unwrap().counter_at_drop, Some(1), "the backend closes while the slot is still held");
    assert_eq!(COUNT.load(Ordering::Acquire), 0);
}

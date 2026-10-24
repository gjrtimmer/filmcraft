//! The decoder logic against a mock libnvmpi session: sequencing, timestamps, planes, errors and
//! cleanup, without hardware.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use filmcraft_codecs::hw::{NalCodec, NalStreamInfo};
use filmcraft_codecs::{CodecError, VideoDecoder};
use filmcraft_frame::PixelData;

use super::backend::{DecBackend, DecOut, PlaneBuf};
use super::cap::try_take;
use super::dec::{NvmpiDecoder, takes};

#[derive(Default)]
struct Log {
    puts: Vec<Vec<u8>>,
    waits: Vec<bool>,
    flushes: usize,
    /// The session counter when the backend was dropped.
    counter_at_drop: Option<usize>,
}

/// A session that answers a packet with its picture after `latency` further packets.
struct Mock {
    log: Arc<Mutex<Log>>,
    latency: usize,
    pending: VecDeque<i64>,
    ready: VecDeque<i64>,
    ten_bit: bool,
    reported: Option<(u32, u32)>,
    size: (u32, u32),
    put_error: Option<i32>,
    get_error: Option<i32>,
    counter: Option<&'static AtomicUsize>,
}

impl Mock {
    fn new(size: (u32, u32), latency: usize) -> (Self, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let m = Mock {
            log: log.clone(),
            latency,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            ten_bit: false,
            reported: None,
            size,
            put_error: None,
            get_error: None,
            counter: None,
        };
        (m, log)
    }
}

impl DecBackend for Mock {
    fn put_packet(&mut self, annex_b: &[u8], pts: i64) -> Result<(), i32> {
        self.log.lock().unwrap().puts.push(annex_b.to_vec());
        if let Some(code) = self.put_error {
            return Err(code);
        }
        self.pending.push_back(pts);
        while self.pending.len() > self.latency {
            let p = self.pending.pop_front().unwrap();
            self.ready.push_back(p);
        }
        Ok(())
    }

    fn get_frame(&mut self, buf: &mut PlaneBuf, wait: bool) -> DecOut {
        self.log.lock().unwrap().waits.push(wait);
        if let Some(code) = self.get_error {
            return DecOut::Failed(code);
        }
        if self.ready.is_empty() && wait {
            // a blocking wait gives the hardware time to finish its oldest picture
            if let Some(p) = self.pending.pop_front() {
                self.ready.push_back(p);
            }
        }
        let Some(pts) = self.ready.pop_front() else { return DecOut::None };
        // luma sample = pts, chroma pair = (pts + 1, pts + 2): 8-bit as bytes, 10-bit as `v << 6` words
        let (w, h) = self.size;
        let ten_bit = self.ten_bit;
        let put = |plane: &mut Vec<u8>, stride: usize, row: usize, col: usize, v: u16| {
            if ten_bit {
                plane[row * stride + col * 2..row * stride + col * 2 + 2].copy_from_slice(&(v << 6).to_le_bytes());
            } else {
                plane[row * stride + col] = v as u8;
            }
        };
        let stride = buf.stride;
        for y in 0..h as usize {
            for x in 0..w as usize {
                put(&mut buf.luma, stride, y, x, pts as u16);
            }
        }
        for y in 0..(h as usize).div_ceil(2) {
            for x in 0..(w as usize).div_ceil(2) {
                put(&mut buf.chroma, stride, y, x * 2, pts as u16 + 1);
                put(&mut buf.chroma, stride, y, x * 2 + 1, pts as u16 + 2);
            }
        }
        let (width, height) = self.reported.unwrap_or(self.size);
        DecOut::Frame { width, height, pts }
    }

    fn flush(&mut self) {
        self.log.lock().unwrap().flushes += 1;
        self.pending.clear();
        self.ready.clear();
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.log.lock().unwrap().counter_at_drop = self.counter.map(|c| c.load(Ordering::Acquire));
    }
}

fn h264(size: (u32, u32), reorder: usize) -> NalStreamInfo {
    NalStreamInfo {
        codec: NalCodec::H264,
        length_size: 4,
        highest_tid: None,
        parameter_sets: vec![vec![0x67, 1, 2], vec![0x68, 3]],
        coded: size,
        crop: (0, 0, size.0, size.1),
        chroma_format_idc: 1,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        interlaced: false,
        profile_idc: 100,
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        reorder,
    }
}

fn hevc(size: (u32, u32), bits: u32) -> NalStreamInfo {
    NalStreamInfo {
        codec: NalCodec::Hevc,
        parameter_sets: vec![vec![0x40, 1, 9], vec![0x42, 1, 9], vec![0x44, 1, 9]],
        bit_depth_luma: bits,
        bit_depth_chroma: bits,
        profile_idc: if bits == 10 { 2 } else { 1 },
        ..h264(size, 0)
    }
}

/// An H.264 IDR slice (nal_ref_idc 3, type 5) as a length-prefixed sample.
const IDR: [u8; 6] = [0, 0, 0, 2, 0x65, 9];

fn decoder(info: NalStreamInfo, mock: Mock) -> NvmpiDecoder {
    NvmpiDecoder::with_backend(info, Box::new(mock), None).expect("decoder")
}

fn pts_of(frames: &[filmcraft_codecs::DecodedFrame]) -> Vec<i64> {
    frames.iter().map(|f| f.pts).collect()
}

#[test]
fn pictures_come_out_in_order_with_their_timestamps_and_planes() {
    let (mock, log) = Mock::new((4, 2), 2);
    let mut d = decoder(h264((4, 2), 2), mock);
    let mut got = Vec::new();
    for k in 1..=5i64 {
        got.extend(d.decode(&IDR, k * 100).unwrap());
    }
    // the hardware answers two packets late: three pictures are out after five samples
    assert_eq!(pts_of(&got), [100, 200, 300]);
    got.extend(d.flush());
    assert_eq!(pts_of(&got), [100, 200, 300, 400, 500]);
    let f = &got[1].frame;
    assert_eq!((f.width, f.height), (4, 2));
    let PixelData::Yuv8 { planes, .. } = &f.data else { panic!("8-bit planes") };
    assert!(planes[0].iter().all(|&s| s == 200), "luma carries the pts");
    assert_eq!((&planes[1][..], &planes[2][..]), (&[201u8, 201][..], &[202u8, 202][..]), "NV12 chroma is deinterleaved");
    // the first packet brings the parameter sets, later ones only the slice, in Annex B form
    let log = log.lock().unwrap();
    assert_eq!(&log.puts[0][..11], [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1]);
    assert_eq!(log.puts[1], [0, 0, 0, 1, 0x65, 9]);
}

#[test]
fn flush_restarts_the_pipeline_and_the_next_packet_brings_the_headers_again() {
    let (mock, log) = Mock::new((4, 2), 3);
    let mut d = decoder(h264((4, 2), 3), mock);
    for k in 0..3i64 {
        assert!(d.decode(&IDR, k).unwrap().is_empty());
    }
    assert_eq!(pts_of(&d.flush()), [0, 1, 2]);
    assert_eq!(log.lock().unwrap().flushes, 1);
    d.decode(&IDR, 10).unwrap();
    let puts = log.lock().unwrap().puts.clone();
    assert_eq!(puts[3][4], 0x67, "parameter sets come first after a flush");
    // reset forgets what was in flight and restarts too
    d.reset();
    assert_eq!(log.lock().unwrap().flushes, 2);
    assert!(d.flush().is_empty(), "nothing in flight after a reset");
    d.decode(&IDR, 11).unwrap();
    assert_eq!(log.lock().unwrap().puts[4][4], 0x67);
}

#[test]
fn a_far_behind_hardware_gets_one_short_wait_not_a_stall_per_call() {
    let (mock, log) = Mock::new((4, 2), 100);
    let mut d = decoder(h264((4, 2), 0), mock);
    assert!(d.decode(&IDR, 1).unwrap().is_empty());
    assert!(d.decode(&IDR, 2).unwrap().is_empty());
    // three samples in flight with a reorder depth of 0: past the slack, so it waits for one
    let out = d.decode(&IDR, 3).unwrap();
    assert_eq!(pts_of(&out), [1]);
    let waits = log.lock().unwrap().waits.clone();
    assert_eq!(waits.iter().filter(|w| **w).count(), 1, "{waits:?}");
}

#[test]
fn refusals_size_changes_and_oversized_units_are_errors_not_panics() {
    let (mut mock, _) = Mock::new((4, 2), 0);
    mock.put_error = Some(-3);
    let mut d = decoder(h264((4, 2), 0), mock);
    assert!(matches!(d.decode(&IDR, 0), Err(CodecError::Decode(m)) if m.contains("-3")));

    let (mut mock, _) = Mock::new((4, 2), 0);
    mock.reported = Some((6, 2));
    let mut d = decoder(h264((4, 2), 0), mock);
    assert!(matches!(d.decode(&IDR, 0), Err(CodecError::Decode(m)) if m.contains("6x2")));

    let (mut mock, _) = Mock::new((4, 2), 0);
    mock.get_error = Some(-90);
    let mut d = decoder(h264((4, 2), 0), mock);
    assert!(matches!(d.decode(&IDR, 0), Err(CodecError::Decode(m)) if m.contains("-90")));
    assert!(d.flush().is_empty(), "a failing session ends a flush quietly");

    let (mock, log) = Mock::new((4, 2), 0);
    let mut d = decoder(h264((4, 2), 0), mock);
    let mut big = vec![0, 0, 0, 0, 0x65];
    big.resize(11 * 1024 * 1024, 7);
    let n = (big.len() - 4) as u32;
    big[..4].copy_from_slice(&n.to_be_bytes());
    assert!(d.decode(&big, 0).is_err());
    assert!(log.lock().unwrap().puts.is_empty(), "an access unit above the input buffer never reaches the library");
    for bad in [&[][..], &[0, 0, 0], &[0, 0, 0, 9, 1], &[0, 0, 0, 0]] {
        assert!(d.decode(bad, 0).is_err(), "{bad:?}");
    }
}

#[test]
fn ten_bit_hevc_comes_out_as_ten_bit_planes() {
    let (mut mock, log) = Mock::new((4, 2), 0);
    mock.ten_bit = true;
    let mut d = decoder(hevc((4, 2), 10), mock);
    // an IDR_W_RADL slice (type 19) of an HEVC stream
    let idr = [0, 0, 0, 3, 0x26, 1, 0xAA];
    let out = d.decode(&idr, 700).unwrap();
    let PixelData::Yuv16 { planes, bits, .. } = &out[0].frame.data else { panic!("16-bit planes") };
    assert_eq!(*bits, 10);
    assert!(planes[0].iter().all(|&s| s == 700));
    assert_eq!((&planes[1][..], &planes[2][..]), (&[701u16, 701][..], &[702u16, 702][..]));
    // VPS, SPS, PPS then the slice
    assert_eq!(&log.lock().unwrap().puts[0][..5], [0, 0, 0, 1, 0x40]);
}

#[test]
fn rasl_pictures_after_starting_at_a_cra_are_not_fed() {
    let (mock, log) = Mock::new((4, 2), 0);
    let mut d = decoder(hevc((4, 2), 8), mock);
    let cra = [0, 0, 0, 3, 0x2A, 1, 0xAA]; // CRA_NUT (21)
    let rasl = [0, 0, 0, 3, 0x10, 1, 0xBB]; // RASL_N (8)
    let trail = [0, 0, 0, 3, 0x02, 1, 0xCC]; // TRAIL_R (1)
    d.reset();
    assert_eq!(d.decode(&cra, 0).unwrap().len(), 1);
    assert!(d.decode(&rasl, 1).unwrap().is_empty(), "RASL of a CRA start is dropped, as the software decoder does");
    assert_eq!(d.decode(&trail, 2).unwrap().len(), 1);
    assert_eq!(log.lock().unwrap().puts.len(), 2);
}

#[test]
fn the_session_slot_is_given_back_after_the_session_closed() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let permit = try_take(&COUNT, 1).expect("slot");
    assert!(try_take(&COUNT, 1).is_none());
    let (mut mock, log) = Mock::new((4, 2), 0);
    mock.counter = Some(&COUNT);
    let d = NvmpiDecoder::with_backend(h264((4, 2), 0), Box::new(mock), Some(permit)).unwrap();
    drop(d);
    assert_eq!(log.lock().unwrap().counter_at_drop, Some(1), "the backend closes while the slot is still held");
    assert_eq!(COUNT.load(Ordering::Acquire), 0, "the slot is free afterwards");
}

#[test]
fn what_nvmpi_does_not_decode_is_declined_up_front() {
    assert!(takes(&h264((1920, 1080), 2)).is_ok());
    assert!(takes(&hevc((3840, 2160), 10)).is_ok());
    assert!(takes(&hevc((3840, 2160), 8)).is_ok());
    let tweak = |f: fn(&mut NalStreamInfo)| {
        let mut i = h264((640, 360), 0);
        f(&mut i);
        takes(&i).is_err()
    };
    assert!(tweak(|i| i.interlaced = true));
    assert!(tweak(|i| i.chroma_format_idc = 2));
    assert!(tweak(|i| i.chroma_format_idc = 3));
    assert!(
        tweak(|i| {
            i.bit_depth_luma = 10;
            i.bit_depth_chroma = 10;
        }),
        "10-bit H.264"
    );
    assert!(tweak(|i| i.bit_depth_chroma = 10));
    assert!(tweak(|i| i.crop = (0, 0, 0, 360)));
    assert!(tweak(|i| i.crop = (0, 0, 8193, 360)));
    assert!(takes(&hevc((640, 360), 12)).is_err());
    // a declined stream never opens a session: `new` fails before touching the cap or the library
    let before = super::cap::in_use(super::cap::Kind::Decoder);
    let mut i = h264((640, 360), 0);
    i.interlaced = true;
    assert!(NvmpiDecoder::new(i).is_err());
    assert_eq!(super::cap::in_use(super::cap::Kind::Decoder), before);
}

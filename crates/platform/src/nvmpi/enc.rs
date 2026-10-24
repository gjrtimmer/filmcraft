//! nvmpi as an Export encoder: [`factory`] is registered in front of the other encoders
//! (`filmcraft_export::register_encoder`). Safe code over an [`EncBackend`] session (libnvmpi's, see
//! `session_enc`).
//!
//! - H.264: takes an export only when Export ▸ Hardware encoding is Auto and nvmpi can do what the settings
//!   ask; otherwise it returns `None` and the software encoder runs, exactly as before.
//! - H.265 (Main, 8-bit 4:2:0 SDR BT.709, progressive): there is no software encoder, so the Hardware encoding
//!   toggle does not apply and choosing the format is the opt-in; what nvmpi cannot do is an error that says why.
//!   [`hevc_available`] tells the export crate the format exists (the library loads; no session is opened).
//!   HDR (Main 10) is not written: HDR sequences are tone-mapped to SDR as before.
//!
//! The sample entry (SPS / PPS, and VPS for HEVC) is read from the first access unit, where the library
//! repeats them (`insert_spspps_idr`); [`NvmpiEncoder::encode`] waits for that unit, so the container can be
//! created after the first batch. No B-frames are asked for: pictures come out in order with no delay, and
//! a picture that comes out anywhere else is an error rather than a mistimed file.

use std::collections::VecDeque;

use filmcraft_codecs::hw::NalCodec;
use filmcraft_export::{
    BitrateMode, ColorSignal, EncodedPacket, EncoderFrame, ExportError, ExportSettings, FieldOrder, Format, H264Pass, H264Profile, HardwareEncoding, Result,
    VideoEncoder,
};
use filmcraft_isobmff::{AvcConfig, HevcConfig, SampleEntry};
use filmcraft_time::FrameRate;

use super::backend::{EncBackend, EncOut, EncPacket, EncParams};
use super::cap::{self, Kind, Permit};
use super::ffi::NVMPI_ERR_EAGAIN;
use crate::nvenc::{Codec, annex_b_nals, annex_b_to_length_prefixed_for, hevc::hevc_config, nal_type, split_hevc_parameter_sets, split_parameter_sets};

/// Largest picture side (the Orin's encoders stop at 8K).
const MAX_SIDE: u32 = 8192;
/// Full queue (`EAGAIN`): packets read and the picture retried at most this often before giving up.
const MAX_BACKPRESSURE_RETRIES: u32 = 20;
/// Waits (each up to the session's wait timeout) for the first access unit.
const FIRST_PACKET_WAITS: u32 = 40;
/// Consecutive empty waits that end a flush as a failure.
const FLUSH_IDLE_WAITS: u32 = 100;

/// The parameter sets the sample entry is built from.
enum Headers {
    H264 { sps: Vec<u8>, pps: Vec<u8> },
    Hevc(Box<HevcConfig>),
}

/// The nvmpi export encoder.
pub struct NvmpiEncoder {
    backend: Box<dyn EncBackend>,
    codec: NalCodec,
    w: u32,
    h: u32,
    rate: FrameRate,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    headers: Option<Headers>,
    /// Indexes of the pictures submitted and not yet answered, oldest first.
    submitted: VecDeque<u64>,
    /// Whether end of stream was sent and drained.
    finished: bool,
    /// Declared last: the encoder slot is given back only after the backend closed its session.
    _permit: Option<Permit>,
}

fn codec_of(c: NalCodec) -> Codec {
    match c {
        NalCodec::H264 => Codec::H264,
        NalCodec::Hevc => Codec::Hevc,
    }
}

fn fail(msg: impl std::fmt::Display) -> ExportError {
    ExportError::Encode(format!("nvmpi: {msg}"))
}

impl NvmpiEncoder {
    /// An encoder over `backend` for `w` x `h` pictures at `rate`.
    pub(crate) fn with_backend(backend: Box<dyn EncBackend>, codec: NalCodec, w: u32, h: u32, rate: FrameRate, permit: Option<Permit>) -> Self {
        Self {
            backend,
            codec,
            w,
            h,
            rate,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            headers: None,
            submitted: VecDeque::new(),
            finished: false,
            _permit: permit,
        }
    }

    /// One access unit out of the session into `out`, learning the parameter sets from the first one.
    /// `Ok(true)`: a packet.
    fn take(&mut self, wait: bool, out: &mut Vec<EncodedPacket>) -> Result<bool> {
        match self.backend.get_packet(wait) {
            EncOut::None => Ok(false),
            EncOut::Eos => Err(fail("the encoder ended the stream early")),
            EncOut::Failed(code) => Err(fail(format!("libnvmpi could not return a packet ({code})"))),
            EncOut::Packet(p) => {
                out.push(self.convert(p)?);
                Ok(true)
            }
        }
    }

    fn convert(&mut self, p: EncPacket) -> Result<EncodedPacket> {
        // pictures go in and come out in order (no B-frames): anything else would be a mistimed file
        let expected = self.submitted.pop_front().ok_or_else(|| fail("the encoder returned more packets than pictures"))?;
        if u64::try_from(p.pts).ok() != Some(expected) {
            return Err(fail(format!("the encoder returned picture {} where {expected} was expected (it reordered or retimed pictures)", p.pts)));
        }
        if self.headers.is_none() {
            self.headers = Some(self.read_headers(&p.data)?);
        }
        let codec = codec_of(self.codec);
        let idr = annex_b_nals(&p.data).iter().any(|n| matches!((self.codec, nal_type(codec, n)), (NalCodec::H264, Some(5)) | (NalCodec::Hevc, Some(19 | 20))));
        let data = annex_b_to_length_prefixed_for(codec, &p.data).map_err(fail)?;
        let duration = u32::try_from(self.rate.den).unwrap_or(u32::MAX);
        Ok(EncodedPacket { data, key: p.key || idr, duration, composition_offset: 0 })
    }

    fn read_headers(&self, annex_b: &[u8]) -> Result<Headers> {
        match self.codec {
            NalCodec::H264 => {
                let (sps, pps) = split_parameter_sets(annex_b).map_err(|e| fail(format!("{e} in the first access unit")))?;
                Ok(Headers::H264 { sps, pps })
            }
            NalCodec::Hevc => {
                let (vps, sps, pps) = split_hevc_parameter_sets(annex_b).map_err(|e| fail(format!("{e} in the first access unit")))?;
                // the record is refused when the SPS is not 8-bit Main 4:2:0 of the exported size
                let cfg = hevc_config(&vps, &sps, &pps, (self.w, self.h), 8).map_err(fail)?;
                Ok(Headers::Hevc(Box::new(cfg)))
            }
        }
    }
}

impl VideoEncoder for NvmpiEncoder {
    fn sample_entry(&self) -> SampleEntry {
        // `config` declines sizes above u16::MAX
        let (w, h) = (u16::try_from(self.w).unwrap_or(u16::MAX), u16::try_from(self.h).unwrap_or(u16::MAX));
        match &self.headers {
            Some(Headers::H264 { sps, pps }) => SampleEntry::avc(AvcConfig::new(vec![sps.clone()], vec![pps.clone()], 4), w, h),
            Some(Headers::Hevc(cfg)) => SampleEntry::hevc((**cfg).clone(), w, h),
            // only before the first picture was encoded: the exporter asks after the first batch
            None if self.codec == NalCodec::H264 => SampleEntry::avc(AvcConfig::new(Vec::new(), Vec::new(), 4), w, h),
            None => SampleEntry::hevc(HevcConfig::default(), w, h),
        }
    }

    fn timescale(&self) -> u32 {
        u32::try_from(self.rate.num).unwrap_or(u32::MAX)
    }

    fn encode(&mut self, f: &EncoderFrame) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Err(fail("a picture after the end of the stream"));
        }
        if f.width != self.w || f.height != self.h {
            return Err(fail(format!("a {}x{} picture for a {}x{} encoder", f.width, f.height, self.w, self.h)));
        }
        if f.hdr.is_some() {
            return Err(ExportError::Unsupported("nvmpi encodes 8-bit pictures: it does not take HDR pictures".into()));
        }
        let (w, h) = (self.w as usize, self.h as usize);
        filmcraft_export::timed(filmcraft_export::Stage::Convert, || filmcraft_export::rgba_to_yuv420_8(f.rgba, w, h, &mut self.y, &mut self.u, &mut self.v));
        let cw = u32::try_from(w / 2).map_err(|_| fail("picture too wide"))?;
        let pts = i64::try_from(f.index).map_err(|_| fail("picture index too large"))?;
        let mut out = Vec::new();
        let mut retries = 0;
        loop {
            let rc = self.backend.put_frame([&self.y, &self.u, &self.v], [self.w, cw, cw], pts);
            if rc == 0 {
                break;
            }
            // The library holds as many pictures as it has empty packets and refuses the next one (not queued)
            // until the caller reads packets: take one out, waiting for it, and offer the same picture again.
            if rc != NVMPI_ERR_EAGAIN || retries >= MAX_BACKPRESSURE_RETRIES {
                return Err(fail(format!("libnvmpi refused a picture ({rc})")));
            }
            retries += 1;
            self.take(true, &mut out)?;
        }
        self.submitted.push_back(f.index);
        while self.take(false, &mut out)? {}
        // The container is created from the sample entry after the first batch: wait for the first access
        // unit, which carries the parameter sets.
        let mut waits = 0;
        while self.headers.is_none() {
            if waits >= FIRST_PACKET_WAITS {
                return Err(fail("the encoder produced no output for the first picture"));
            }
            waits += 1;
            self.take(true, &mut out)?;
        }
        filmcraft_export::note_hw_encode_frame();
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let rc = self.backend.put_eos();
        if rc != 0 {
            return Err(fail(format!("libnvmpi refused the end of the stream ({rc})")));
        }
        let mut out = Vec::new();
        let mut idle = 0;
        // after end of stream a read blocks until a packet arrives or the stream is over
        loop {
            match self.backend.get_packet(true) {
                EncOut::Eos => break,
                EncOut::None if idle < FLUSH_IDLE_WAITS => idle += 1,
                EncOut::None => return Err(fail("the encoder did not finish the stream")),
                EncOut::Failed(code) => return Err(fail(format!("libnvmpi could not finish the stream ({code})"))),
                EncOut::Packet(p) => {
                    idle = 0;
                    out.push(self.convert(p)?);
                }
            }
        }
        if !self.submitted.is_empty() {
            return Err(fail(format!("the encoder dropped {} picture(s)", self.submitted.len())));
        }
        Ok(out)
    }
}

/// Whether nvmpi can write H.265 here: the library loads (no session is opened to find out).
pub fn hevc_available() -> bool {
    super::probe().is_ok()
}

/// Why nvmpi does not take this export (the software encoder does, for H.264), or the session parameters.
pub fn params(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> std::result::Result<EncParams, String> {
    let hevc = format == Format::Hevc;
    if !(format == Format::H264 || hevc) || s.format.is_mxf() {
        return Err("only MP4 / MOV H.264 and H.265 exports".into());
    }
    // Main 8-bit SDR BT.709: HDR needs Main 10, and any other colour description is not written
    if s.signal != ColorSignal::default() {
        return Err(format!("the colour signal {:?} (nvmpi writes BT.709 SDR)", s.signal));
    }
    if !matches!(s.h264_pass, H264Pass::Single) || s.bitrate_mode == BitrateMode::Vbr2Pass {
        return Err("two-pass VBR".into());
    }
    if s.bitrate_mode == BitrateMode::Crf {
        return Err("CRF (constant quality)".into());
    }
    // libnvmpi has no pixel aspect ratio parameter
    if s.pixel_aspect.is_some_and(|(n, d)| n != d) {
        return Err("non-square pixels".into());
    }
    if s.field_order != FieldOrder::Progressive {
        return Err("interlaced output".into());
    }
    // 4:2:0 cannot signal an odd size, and the library would code the even-rounded one
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("{w}x{h}: nvmpi needs even sizes up to {MAX_SIDE}"));
    }
    let (Ok(num), Ok(den)) = (u32::try_from(rate.num), u32::try_from(rate.den)) else { return Err("frame rate".into()) };
    if num == 0 || den == 0 {
        return Err("frame rate".into());
    }
    let kbps = s.bitrate_kbps.max(100);
    let max_kbps = s.max_bitrate_kbps.filter(|m| *m >= kbps).unwrap_or_else(|| kbps.saturating_add(kbps / 2));
    let bps = |k: u32| k.saturating_mul(1000);
    Ok(EncParams {
        codec: if hevc { NalCodec::Hevc } else { NalCodec::H264 },
        width: w,
        height: h,
        fps: (num, den),
        bitrate_bps: bps(kbps),
        peak_bitrate_bps: bps(max_kbps),
        vbr: s.bitrate_mode != BitrateMode::Cbr,
        keyint: s.keyframe_distance.filter(|k| *k > 0).unwrap_or_else(|| (f64::from(num) / f64::from(den) * 2.0).round().max(1.0) as u32),
        h264_profile: match s.h264_profile {
            H264Profile::Baseline => 66,
            H264Profile::Main => 77,
            H264Profile::High => 100,
        },
        // the H.264 level setting means nothing for H.265: the encoder picks the level
        level: if hevc { 0 } else { s.h264_level.map_or(0, u32::from) },
    })
}

/// A session opened for `EncParams`, and the cap slot it holds.
type Opened = std::result::Result<(Box<dyn EncBackend>, Option<Permit>), String>;

/// The Export encoder factory (see the module documentation).
pub fn factory(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    factory_with(format, w, h, rate, s, |p| {
        let permit = cap::acquire(Kind::Encoder).ok_or("an nvmpi encoder is already open (the Tegra driver takes one session)")?;
        let session = super::session_enc::HwEncoder::open(p)?;
        Ok((Box::new(session) as Box<dyn EncBackend>, Some(permit)))
    })
}

/// [`factory`] with the session opener passed in, so tests can stand in for libnvmpi.
pub(crate) fn factory_with(
    format: Format,
    w: u32,
    h: u32,
    rate: FrameRate,
    s: &ExportSettings,
    open: impl FnOnce(&EncParams) -> Opened,
) -> Option<Result<Box<dyn VideoEncoder>>> {
    let hevc = format == Format::Hevc;
    if !(hevc || format == Format::H264) || (!hevc && s.hardware_encoding != HardwareEncoding::Auto) {
        return None;
    }
    // H.264 falls through to the software encoder; H.265 has none, so a request nvmpi cannot take is an error
    let declined = |why: &str| {
        log::info!("nvmpi encoding declined: {why}");
        filmcraft_export::note_hw_encode_declined();
        if hevc { Some(Err(ExportError::Unsupported(format!("H.265 export with nvmpi: {why}")))) } else { None }
    };
    let p = match params(format, w, h, rate, s) {
        Ok(p) => p,
        Err(why) => return declined(&why),
    };
    match open(&p) {
        Ok((backend, permit)) => {
            filmcraft_export::note_hw_encode_session();
            Some(Ok(Box::new(NvmpiEncoder::with_backend(backend, p.codec, w, h, rate, permit))))
        }
        Err(why) => declined(&why),
    }
}

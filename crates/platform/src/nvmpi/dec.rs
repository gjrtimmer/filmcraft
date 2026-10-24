//! The nvmpi decoder: Annex B access units in, pictures out in presentation order, through a
//! [`DecBackend`] session (libnvmpi's, see `session_dec`). Safe code.
//!
//! libnvmpi decodes on its own capture thread, so a picture is rarely ready right after its packet:
//! [`NvmpiDecoder::decode`] returns what is ready, waits (briefly) only when the hardware lags far
//! behind the stream's reorder depth, and [`NvmpiDecoder::flush`] sends the end-of-stream marker and
//! waits for the rest; the finished session is then replaced by a new one. A seek
//! ([`VideoDecoder::reset`]) replaces the session without the marker. libnvmpi 4.6.5 stops its
//! capture thread at the marker and drops the pictures it still holds for reordering (the last 2-4
//! of a stream; jetson-nvmpi GitLab #16): those are lost until a libnvmpi with the fix is installed.
//!
//! No call blocks without bound. libnvmpi's `put_packet` waits with no timeout for a free input
//! buffer once all of them are queued, so a packet goes in only while fewer than [`INPUT_SLOTS`]
//! packets are unanswered; otherwise the decoder first waits for pictures, and a hardware decoder
//! that returns none for [`STALL_POLLS`] waits (one that never starts, or stopped) is an error: the
//! hybrid decoder's cue to continue in software.
//!
//! 8-bit H.264 and 8-/10-bit HEVC 4:2:0 progressive; everything else is declined for the software
//! decoders. Timestamps go through the library bit for bit (`nvmpi.h` pts contract).

use filmcraft_codecs::hw::{NalCodec, NalStreamInfo};
use filmcraft_codecs::{CodecError, DecodedFrame, Result, VideoDecoder};

use super::backend::{DecBackend, DecOut, DecParams, PlaneBuf};
use super::cap::{self, Kind, Permit};
use super::ffi::NVMPI_ENC_CHUNK_SIZE;
use crate::annexb::to_annex_b;
use crate::biplanar::{Biplanar, Geometry, to_frame};

/// Largest picture side taken (the Orin's decoders stop at 8K).
const MAX_SIDE: u32 = 8192;
/// Largest picture buffer, in bytes.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
/// Pictures the hardware may lag behind `reorder` before a call waits for one.
const LAG_SLACK: usize = 2;
/// Consecutive empty waits (each up to the session's wait timeout, 50 ms) that end the drain after the
/// end-of-stream marker: not the first one, as the hardware may still be finishing a picture.
const FLUSH_IDLE_POLLS: u32 = 6;
/// Unanswered packets at which a packet waits for a picture before it goes in: libnvmpi queues input
/// in 10 buffers and blocks for good on the 11th packet while the hardware holds all 10. A packet
/// still in the hardware has not produced its picture, so at most `in_flight` buffers are held.
pub(crate) const INPUT_SLOTS: usize = 9;
/// Consecutive empty waits (each up to the session's wait timeout, 50 ms) with the input full after
/// which the hardware decoder counts as stalled.
pub(crate) const STALL_POLLS: u32 = 40;

/// An nvmpi hardware decoder for one H.264 or HEVC stream.
pub struct NvmpiDecoder {
    backend: Box<dyn DecBackend>,
    info: NalStreamInfo,
    name: &'static str,
    /// Where pictures are copied: sized for the stream's output rectangle.
    buf: PlaneBuf,
    size: (u32, u32),
    bits: u32,
    /// Whether the decoder has the stream's parameter sets (sent with the first sample after a reset).
    primed: bool,
    /// HEVC, after starting at a CRA / BLA picture: its RASL pictures (they refer to pictures before it)
    /// are not fed until the next random access point, as the software decoder drops them.
    skip_rasl: bool,
    /// Samples fed and not yet answered by a picture.
    in_flight: usize,
    /// Whether the session was given a packet since it opened or last restarted. One that was not
    /// needs no restart (a restart opens a new session, and the frame cache resets every new
    /// decoder before its first sample: the Tegra driver dislikes rapid open / close).
    fed: bool,
    /// Declared last: the session slot is given back only after the backend closed its session.
    _permit: Option<Permit>,
}

/// Whether libnvmpi takes the stream `info` describes (whatever the system has): 8-bit H.264 or 8-/10-bit
/// HEVC, 4:2:0, progressive, with a picture size the buffers can hold.
pub fn takes(info: &NalStreamInfo) -> std::result::Result<(), String> {
    let bits = info.bit_depth_luma;
    let depth_ok = match info.codec {
        NalCodec::H264 => bits == 8,
        NalCodec::Hevc => matches!(bits, 8 | 10),
    };
    if !depth_ok || info.chroma_format_idc != 1 || info.bit_depth_chroma != bits || info.interlaced {
        return Err(format!(
            "{} chroma format {} at {bits}/{}-bit is not taken",
            if info.interlaced { "interlaced" } else { "progressive" },
            info.chroma_format_idc,
            info.bit_depth_chroma
        ));
    }
    let (w, h) = info.size();
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("picture size {w}x{h} is not taken"));
    }
    plane_sizes((w, h), bits).map(|_| ())
}

/// Row stride and the luma and chroma plane sizes of a `size` picture at `bits`: both planes share the
/// stride of the chroma row (`ceil(w / 2)` CbCr pairs), which is never below the luma row.
fn plane_sizes(size: (u32, u32), bits: u32) -> std::result::Result<(usize, usize, usize), String> {
    let bps = if bits > 8 { 2usize } else { 1 };
    let (w, h) = (size.0 as usize, size.1 as usize);
    let stride = w.div_ceil(2).checked_mul(2).and_then(|n| n.checked_mul(bps)).ok_or("picture row size overflows")?;
    let luma = stride.checked_mul(h).ok_or("picture size overflows")?;
    let chroma = stride.checked_mul(h.div_ceil(2)).ok_or("picture size overflows")?;
    if luma.saturating_add(chroma) > MAX_FRAME_BYTES {
        return Err(format!("a {}x{} picture exceeds the buffer limit", size.0, size.1));
    }
    Ok((stride, luma, chroma))
}

fn zeroed(n: usize) -> std::result::Result<Vec<u8>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|e| format!("picture buffer allocation failed: {e}"))?;
    v.resize(n, 0);
    Ok(v)
}

impl NvmpiDecoder {
    /// A decoder for the stream `info` describes, or why nvmpi does not take it on this system: the
    /// stream is not one it decodes, the session cap is reached, or libnvmpi cannot open a session.
    pub fn new(info: NalStreamInfo) -> std::result::Result<Self, String> {
        takes(&info)?;
        let permit = cap::acquire(Kind::Decoder).ok_or("the nvmpi decoder session limit is reached")?;
        let (width, height) = info.size();
        let ten_bit = info.bit_depth_luma > 8;
        let backend = super::session_dec::HwDecoder::open(&DecParams { codec: info.codec, ten_bit, width, height })?;
        Self::with_backend(info, Box::new(backend), Some(permit))
    }

    /// A decoder over `backend` (a mock in tests); `permit` is held for as long as the decoder lives.
    pub(crate) fn with_backend(info: NalStreamInfo, backend: Box<dyn DecBackend>, permit: Option<Permit>) -> std::result::Result<Self, String> {
        takes(&info)?;
        let size = info.size();
        let bits = info.bit_depth_luma;
        let (stride, luma, chroma) = plane_sizes(size, bits)?;
        let name = match info.codec {
            NalCodec::H264 => "nvmpi H.264",
            NalCodec::Hevc => "nvmpi HEVC",
        };
        let buf = PlaneBuf { luma: zeroed(luma)?, chroma: zeroed(chroma)?, stride };
        Ok(Self { backend, info, name, buf, size, bits, primed: false, skip_rasl: false, in_flight: 0, fed: false, _permit: permit })
    }

    /// Start the pipeline over with a new session: the next packet brings the parameter sets again.
    /// Required after the end-of-stream marker (the session is finished) and on a seek. A session
    /// that has not been fed is left alone (see `fed`).
    fn restart(&mut self) {
        if self.fed {
            self.backend.reopen();
        }
        self.fed = false;
        self.primed = false;
        self.skip_rasl = false;
        self.in_flight = 0;
    }

    /// Wait for pictures (into `out`) until a packet can go in without libnvmpi blocking on a full
    /// input queue. A stream whose reorder depth keeps more pictures in the hardware than that raises
    /// the bound (its pictures come out only as packets go in). Errors when the hardware returns
    /// nothing for [`STALL_POLLS`] waits in a row.
    fn make_room(&mut self, out: &mut Vec<DecodedFrame>) -> Result<()> {
        let limit = INPUT_SLOTS.max(self.info.reorder.saturating_add(LAG_SLACK).saturating_add(1));
        let mut idle = 0;
        while self.in_flight >= limit {
            if self.poll(out, true)? {
                idle = 0;
                continue;
            }
            idle += 1;
            if idle >= STALL_POLLS {
                return Err(CodecError::Decode(format!("{}: no picture for {} packets, the hardware decoder has stalled", self.name, self.in_flight)));
            }
        }
        Ok(())
    }

    /// End of stream: send the marker (after making room for it: it takes an input buffer like any
    /// packet) and collect the pictures the session gives up, until it has answered every packet or
    /// stays empty for [`FLUSH_IDLE_POLLS`] waits. The session is finished afterwards.
    fn drain(&mut self, out: &mut Vec<DecodedFrame>) {
        if let Err(e) = self.make_room(out) {
            log::warn!("{e} at the end of the stream");
            return;
        }
        if let Err(code) = self.backend.put_eos() {
            log::warn!("{}: libnvmpi refused the end of stream ({code})", self.name);
            return;
        }
        let mut idle = 0;
        while self.in_flight > 0 && idle < FLUSH_IDLE_POLLS {
            match self.poll(out, true) {
                Ok(true) => idle = 0,
                Ok(false) => idle += 1,
                // a library that reports the end of the stream as a code ends the drain the same way
                Err(e) => {
                    log::info!("{e} after the end of the stream");
                    break;
                }
            }
        }
        if self.in_flight > 0 {
            // libnvmpi 4.6.5 drops the pictures held for reordering at the marker (jetson-nvmpi GitLab #16)
            log::info!("{}: {} pictures did not come out at the end of the stream", self.name, self.in_flight);
        }
    }

    /// Ask the session for one picture and, when there is one, append it to `out`. `Ok(true)`: a picture.
    fn poll(&mut self, out: &mut Vec<DecodedFrame>, wait: bool) -> Result<bool> {
        match self.backend.get_frame(&mut self.buf, wait) {
            DecOut::None => Ok(false),
            DecOut::Failed(code) => Err(CodecError::Decode(format!("{}: libnvmpi could not return a picture ({code})", self.name))),
            DecOut::Frame { width, height, pts } => {
                // a stream that changes size is one this decoder was not set up for: an error here is
                // the hybrid decoder's cue to continue in software
                if (width, height) != self.size {
                    return Err(CodecError::Decode(format!("{}: picture is {width}x{height}, the stream {}x{}", self.name, self.size.0, self.size.1)));
                }
                let geometry = Geometry { crop: (0, 0, width, height), bits: self.bits, color: self.info.color, par: self.info.par };
                let src = Biplanar { luma: &self.buf.luma, chroma: &self.buf.chroma, stride: self.buf.stride, rows: height as usize };
                let frame = to_frame(&src, &geometry).map_err(CodecError::Decode)?;
                out.try_reserve(1).map_err(|e| CodecError::Decode(format!("{}: output allocation failed: {e}", self.name)))?;
                out.push(DecodedFrame { pts, frame, draft: false });
                self.in_flight = self.in_flight.saturating_sub(1);
                Ok(true)
            }
        }
    }
}

impl VideoDecoder for NvmpiDecoder {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if self.info.codec == NalCodec::Hevc {
            let types = self.info.nal_types(sample);
            if self.info.is_irap(sample) {
                // IDR pictures (19, 20) have no RASL pictures
                self.skip_rasl = !self.primed && !types.iter().any(|t| matches!(t, 19 | 20));
            } else if self.skip_rasl && types.iter().any(|t| matches!(t, 8 | 9)) {
                return Ok(Vec::new());
            }
        }
        let data = to_annex_b(&self.info, sample, !self.primed).map_err(CodecError::Decode)?;
        // one access unit must fit one input buffer; the library would drop it with -3
        if data.len() > NVMPI_ENC_CHUNK_SIZE {
            return Err(CodecError::Decode(format!("{}: a {} byte access unit exceeds the input buffer", self.name, data.len())));
        }
        let mut out = Vec::new();
        self.make_room(&mut out)?;
        self.backend.put_packet(&data, pts).map_err(|code| CodecError::Decode(format!("{}: libnvmpi refused a packet ({code})", self.name)))?;
        self.primed = true;
        self.fed = true;
        self.in_flight = self.in_flight.saturating_add(1);
        while self.poll(&mut out, false)? {}
        // far behind the stream's reorder depth: one short wait for the hardware to catch up
        if out.is_empty() && self.in_flight > self.info.reorder.saturating_add(LAG_SLACK) && self.poll(&mut out, true)? {
            while self.poll(&mut out, false)? {}
        }
        Ok(out)
    }

    fn flush(&mut self) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        if self.fed {
            self.drain(&mut out);
        }
        self.restart();
        out
    }

    fn reset(&mut self) {
        self.restart();
    }

    fn name(&self) -> &str {
        self.name
    }

    fn is_random_access(&self, sample: &[u8]) -> Option<bool> {
        self.info.is_random_access(sample)
    }

    fn is_disposable(&self, sample: &[u8]) -> bool {
        self.info.is_disposable(sample)
    }
}

//! The libnvmpi decoder session: the only `unsafe` of the decoder. It owns one `nvmpictx`, closes it
//! exactly once on drop and is driven through [`DecBackend`] by [`super::dec::NvmpiDecoder`].

use std::ffi::{c_int, c_uint, c_ulong};
use std::ptr;

use super::backend::{DecBackend, DecOut, DecParams, PlaneBuf};
use super::ffi::*;
use super::loader::{self, Api};
use filmcraft_codecs::hw::NalCodec;

/// Decoded pictures the library buffers before the caller must read (its own default).
const FRAME_POOL: c_int = 5;
/// How long a blocking `get_frame` waits for a picture, in milliseconds: libnvmpi's shortest
/// (`[50, 5000]`; a value outside is replaced by its 500 ms default). Short on purpose: the decoder
/// logic waits only when the hardware lags behind, and a stall must stay small.
pub const WAIT_MS: c_int = 50;

/// One open libnvmpi decoder.
pub struct HwDecoder {
    api: &'static Api,
    /// The session; null after `drop` closed it, or when reopening it for a restart failed.
    ctx: *mut nvmpictx,
    /// The `NV_PIX_*` value the session was created for.
    pixel: c_int,
    /// What the session was created with, to open it again on a restart.
    params: DecParams,
}

// SAFETY: the `nvmpictx` is only reached through `&mut self`, so one thread at a time uses it
// (libnvmpi requires that: "all get_frame calls (and flush/close) must come from one thread at a
// time", and its own capture thread is internal). Nothing in it is tied to the creating thread.
unsafe impl Send for HwDecoder {}

impl HwDecoder {
    /// Open a decoder session, or say why libnvmpi would not (no library, no decoder device, device
    /// busy, stream type unsupported).
    pub fn open(p: &DecParams) -> Result<Self, String> {
        let api = loader::api()?;
        let (ctx, pixel) = create(api, p)?;
        Ok(Self { api, ctx, pixel, params: *p })
    }

    /// Close the session, once: the handle is nulled right after.
    fn close(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` is a live decoder handle owned by `self`; this is its single close (it is
        // nulled right after, so it cannot be used or closed again).
        let _ = unsafe { (self.api.decoder_close)(self.ctx) };
        self.ctx = ptr::null_mut();
    }
}

/// Create a libnvmpi decoder for `p`: the handle and the `NV_PIX_*` output format.
fn create(api: &Api, p: &DecParams) -> Result<(*mut nvmpictx, c_int), String> {
    let (coding, pixel) = match (p.codec, p.ten_bit) {
        (NalCodec::H264, false) => (NV_VIDEO_CodingH264, NV_PIX_NV12),
        (NalCodec::Hevc, false) => (NV_VIDEO_CodingHEVC, NV_PIX_NV12),
        (NalCodec::Hevc, true) => (NV_VIDEO_CodingHEVC, NV_PIX_P010),
        (NalCodec::H264, true) => return Err("10-bit H.264 is not taken".into()),
    };
    let mut param = nvDecParam {
        frame_pool_size: FRAME_POOL,
        codingType: coding,
        pixFormat: pixel,
        resized: nvSize { width: 0, height: 0 },
        // 0: the library's default (10 MiB); one access unit must fit
        chunk_size: 0,
        max_perf: 0,
        disable_dpb: 0,
        wait_timeout: WAIT_MS,
        width: p.width,
        height: p.height,
    };
    // SAFETY: `param` is a live, fully initialised `nvDecParam`; the library reads it once during
    // the call and keeps no reference to it. The returned handle is owned by the caller from here on.
    let ctx = unsafe { (api.create_decoder)(&mut param) };
    if ctx.is_null() {
        return Err("libnvmpi could not create a decoder (no decoder device, the device is busy, or the stream is not supported)".into());
    }
    Ok((ctx, pixel))
}

impl DecBackend for HwDecoder {
    fn put_packet(&mut self, annex_b: &[u8], pts: i64) -> Result<(), i32> {
        // An empty payload is the library's end-of-stream marker, which stops its capture loop at
        // once and loses the pictures still in flight: never forwarded.
        if annex_b.is_empty() || self.ctx.is_null() {
            return Err(NVMPI_ERR_INVAL);
        }
        let mut packet = nvPacket {
            flags: 0,
            payload_size: c_ulong::try_from(annex_b.len()).map_err(|_| NVMPI_ERR_INVAL)?,
            // the library only reads the payload (it memcpy's it into a V4L2 buffer)
            payload: annex_b.as_ptr().cast_mut(),
            // the timestamp comes back bit for bit (nvmpi.h pts contract), negative values included
            pts: pts as c_ulong,
            privData: ptr::null_mut(),
        };
        // SAFETY: `ctx` is a live decoder handle; `packet` and the `annex_b` bytes behind it are valid
        // for the whole call, which copies them and keeps neither.
        let rc = unsafe { (self.api.decoder_put_packet)(self.ctx, &mut packet) };
        if rc < 0 { Err(rc) } else { Ok(()) }
    }

    fn get_frame(&mut self, buf: &mut PlaneBuf, wait: bool) -> DecOut {
        let (Ok(stride), Ok(luma_len), Ok(chroma_len)) = (c_uint::try_from(buf.stride), c_ulong::try_from(buf.luma.len()), c_ulong::try_from(buf.chroma.len()))
        else {
            return DecOut::Failed(NVMPI_ERR_INVAL);
        };
        // get_frame2 needs a non-NULL plane with a non-zero capacity for each plane of the format
        if self.ctx.is_null() || luma_len == 0 || chroma_len == 0 || stride == 0 {
            return DecOut::Failed(NVMPI_ERR_INVAL);
        }
        let mut frame = nvFrame {
            flags: 0,
            payload_size: [luma_len, chroma_len, 0],
            payload: [buf.luma.as_mut_ptr(), buf.chroma.as_mut_ptr(), ptr::null_mut()],
            linesize: [stride, stride, 0],
            type_: self.pixel,
            width: 0,
            height: 0,
            timestamp: 0,
        };
        // SAFETY: `ctx` is a live decoder handle. `frame` points at the two planes of `buf`, which this
        // call borrows exclusively for its whole duration; the library writes at most `payload_size[p]`
        // bytes into plane `p` (nothing at all when the picture does not fit: `NVMPI_ERR_FRAME_SIZE`).
        let rc = unsafe { (self.api.decoder_get_frame2)(self.ctx, &mut frame, wait) };
        match rc {
            0 => DecOut::Frame { width: frame.width, height: frame.height, pts: frame.timestamp },
            -1 => DecOut::None,
            code => DecOut::Failed(code),
        }
    }

    fn flush(&mut self) {
        // A new session, not `nvmpi_decoder_flush`: on the Orin (L4T R36.4, libnvmpi 4.6.5) a flushed
        // session never signals the stream's resolution again, so it returns no picture, its input
        // buffers fill and the next `put_packet` blocks for good. If the new session cannot be opened
        // the handle stays null and every later call is refused (the hybrid decoder's cue to go on in
        // software).
        self.close();
        match create(self.api, &self.params) {
            Ok((ctx, pixel)) => {
                self.ctx = ctx;
                self.pixel = pixel;
            }
            Err(why) => log::warn!("nvmpi: the decoder session could not be reopened: {why}"),
        }
    }
}

impl Drop for HwDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

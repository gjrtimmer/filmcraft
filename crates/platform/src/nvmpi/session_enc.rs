//! The libnvmpi encoder session: the only `unsafe` of the encoder. It owns one `nvmpictx` and the
//! pool of packet buffers the library fills, and on drop ends the stream, drains it and closes the
//! session exactly once. Driven through [`EncBackend`] by [`super::enc::NvmpiEncoder`].

use std::ffi::{c_char, c_int};
use std::ptr;
use std::time::{Duration, Instant};

use super::backend::{EncBackend, EncOut, EncPacket, EncParams};
use super::ffi::*;
use super::loader::{self, Api};
use filmcraft_codecs::hw::NalCodec;

/// Packets in the pool: pictures the library may hold before the caller must read one. No B-frames
/// are asked for, so there is no encoder delay and a few are plenty.
const POOL: usize = 6;
/// V4L2 buffers per plane (the FFmpeg wrapper's default).
const CAPTURE_BUFFERS: c_int = 10;
/// The encoder speed / quality preset, "medium" (the FFmpeg wrapper's default).
const PRESET_MEDIUM: u32 = 3;
/// How long dropping a session waits for the library to hand back what is still in flight.
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// One pool packet: the header the library is handed by pointer, and the `NVMPI_ENC_CHUNK_SIZE`
/// buffer its payload points into (a frame above that size is dropped by the library).
struct Slot {
    header: *mut nvPacket,
    buf: Vec<u8>,
}

/// One open libnvmpi encoder.
pub struct HwEncoder {
    api: &'static Api,
    /// The session; null only after `drop` closed it.
    ctx: *mut nvmpictx,
    slots: Vec<Slot>,
    eos_sent: bool,
}

// SAFETY: the `nvmpictx` and the packet headers are only reached through `&mut self`, so one thread at
// a time uses them; the library's own dequeue thread works on its side of the packet pool, which this
// type never touches while the library owns a packet (between `qEmptyPacket` and `get_packet`).
unsafe impl Send for HwEncoder {}

/// Free the headers of `slots` (never handed to the library, or handed back and its session closed).
fn free_headers(slots: Vec<Slot>) {
    for slot in slots {
        // SAFETY: `header` came from `Box::into_raw` in `HwEncoder::open` and is freed here, once, at a
        // point where no library session can reach it.
        drop(unsafe { Box::from_raw(slot.header) });
    }
}

impl HwEncoder {
    /// Open an encoder session and fill its packet pool, or say why libnvmpi would not.
    pub fn open(p: &EncParams) -> Result<Self, String> {
        let api = loader::api()?;
        let coding = match p.codec {
            NalCodec::H264 => NV_VIDEO_CodingH264,
            NalCodec::Hevc => NV_VIDEO_CodingHEVC,
        };
        let mut slots = Vec::new();
        for _ in 0..POOL {
            let mut buf = Vec::new();
            buf.try_reserve_exact(NVMPI_ENC_CHUNK_SIZE).map_err(|e| format!("encoder packet buffer allocation failed: {e}"))?;
            buf.resize(NVMPI_ENC_CHUNK_SIZE, 0);
            // freed by `free_headers` (or leaked on purpose, see `Drop`); the payload points into `buf`,
            // whose heap block does not move when the `Slot` does
            let header = Box::into_raw(Box::new(nvPacket { flags: 0, payload_size: 0, payload: buf.as_mut_ptr(), pts: 0, privData: ptr::null_mut() }));
            slots.push(Slot { header, buf });
        }
        let flag = |on: bool| -> c_char { c_char::from(on) };
        let mut param = nvEncParam {
            width: p.width,
            height: p.height,
            // H.264: the FFmpeg-style profile id; HEVC: NVMPI_HEVC_PROFILE_MAIN
            profile: if p.codec == NalCodec::H264 { p.h264_profile } else { 1 },
            level: p.level,
            bitrate: p.bitrate_bps,
            peak_bitrate: p.peak_bitrate_bps,
            enableLossless: 0,
            mode_vbr: flag(p.vbr),
            // parameter sets repeat at every IDR: the first access unit carries them, which is where the
            // sample entry's SPS / PPS (VPS) are read from
            insert_spspps_idr: 1,
            insert_vui: 1,
            pixFormat: NV_PIX_YUV420,
            iframe_interval: p.keyint,
            idr_interval: p.keyint,
            fps_n: p.fps.0,
            fps_d: p.fps.1,
            capture_num: CAPTURE_BUFFERS,
            // no B-frames: pictures come out in the order they went in, with no encoder delay
            max_b_frames: 0,
            refs: 0,
            qmax: 0,
            qmin: 0,
            hw_preset_type: PRESET_MEDIUM,
            vbv_buffer_size: 0,
            codingType: coding,
            max_perf: 0,
            poc_type: 0,
            wait_timeout: 0,
            enable_cabac: 0,
            insert_aud: 0,
            use_dmabuf: 0,
            nonblocking: 0,
        };
        // SAFETY: `param` is a live, fully initialised `nvEncParam`; the library reads it once during the
        // call and keeps no reference to it.
        let ctx = unsafe { (api.create_encoder)(&mut param) };
        if ctx.is_null() {
            free_headers(slots);
            return Err("libnvmpi could not create an encoder (no encoder device, the device is busy, or the settings are not supported)".into());
        }
        for slot in &slots {
            // SAFETY: `ctx` is a live encoder handle and `header` a valid packet whose payload buffer is
            // owned by `slot` and outlives the session (see `Drop`). The library fills it later.
            unsafe { (api.encoder_q_empty_packet)(ctx, slot.header) };
        }
        Ok(Self { api, ctx, slots, eos_sent: false })
    }
}

impl EncBackend for HwEncoder {
    fn put_frame(&mut self, planes: [&[u8]; 3], strides: [u32; 3], pts: i64) -> i32 {
        if self.ctx.is_null() || self.eos_sent {
            return NVMPI_ERR_INVAL;
        }
        let size = |p: &[u8]| std::ffi::c_ulong::try_from(p.len()).unwrap_or(std::ffi::c_ulong::MAX);
        let mut frame = nvFrame {
            flags: 0,
            // the declared allocation of each plane: the library refuses a frame that needs more
            payload_size: [size(planes[0]), size(planes[1]), size(planes[2])],
            // the library only reads the planes (it copies them into a V4L2 buffer)
            payload: [planes[0].as_ptr().cast_mut(), planes[1].as_ptr().cast_mut(), planes[2].as_ptr().cast_mut()],
            linesize: strides,
            type_: NV_PIX_YUV420,
            width: 0,
            height: 0,
            timestamp: pts,
        };
        // SAFETY: `ctx` is a live encoder handle; `frame` and the three planes behind it are valid for the
        // whole call, which copies them and keeps none of them.
        unsafe { (self.api.encoder_put_frame)(self.ctx, &mut frame) }
    }

    fn put_eos(&mut self) -> i32 {
        if self.ctx.is_null() {
            return NVMPI_ERR_INVAL;
        }
        self.eos_sent = true;
        // SAFETY: `ctx` is a live encoder handle; a null frame is the documented end-of-stream marker.
        unsafe { (self.api.encoder_put_frame)(self.ctx, ptr::null_mut()) }
    }

    fn get_packet(&mut self, wait: bool) -> EncOut {
        if self.ctx.is_null() {
            return EncOut::Failed(NVMPI_ERR_INVAL);
        }
        let mut header: *mut nvPacket = ptr::null_mut();
        // SAFETY: `ctx` is a live encoder handle and `header` a writable out-pointer.
        let rc = unsafe { (self.api.encoder_get_packet)(self.ctx, &mut header, wait) };
        match rc {
            0 => {}
            -1 => return EncOut::None,
            -2 => return EncOut::Eos,
            code => return EncOut::Failed(code),
        }
        // only a packet of our own pool may be read or handed back
        let Some(slot) = self.slots.iter().find(|s| s.header == header) else { return EncOut::Failed(NVMPI_ERR_INVAL) };
        // SAFETY: the library handed this packet over (it is one of ours, checked above) and does not touch
        // it again until `qEmptyPacket` below.
        let (size, flags, pts) = unsafe { ((*header).payload_size, (*header).flags, (*header).pts) };
        let data = usize::try_from(size).ok().and_then(|n| slot.buf.get(..n)).map(<[u8]>::to_vec);
        // SAFETY: as above; the header is reset (as the FFmpeg wrapper does) and given back to the pool,
        // after which the library owns it again and this code does not read it.
        unsafe {
            (*header).payload_size = 0;
            (*header).flags = 0;
            (*header).pts = 0;
            (self.api.encoder_q_empty_packet)(self.ctx, header);
        }
        match data {
            // `flags` carries `AV_PKT_FLAG_KEY` (1) for a keyframe; `pts` is the submitted timestamp
            Some(data) => EncOut::Packet(EncPacket { data, key: flags & 1 != 0, pts: pts as i64 }),
            None => EncOut::Failed(NVMPI_ERR_INVAL),
        }
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        if self.ctx.is_null() {
            return;
        }
        // End the stream and read what is in flight (a finished export already did, and gets `Eos` at once).
        // Packets read here belong to an export that was cancelled or failed: nobody wants them.
        let finished = self.eos_sent;
        if !finished {
            let _ = self.put_eos();
        }
        let mut discarded = 0usize;
        let deadline = Instant::now() + DRAIN_LIMIT;
        while Instant::now() < deadline {
            match self.get_packet(true) {
                EncOut::Packet(_) => discarded += 1,
                EncOut::None => {}
                EncOut::Eos | EncOut::Failed(_) => break,
            }
        }
        if !finished || discarded > 0 {
            log::debug!("nvmpi encoder closed (finished: {finished}): {discarded} encoded picture(s) discarded");
        }
        // `close` discards (and warns about) every packet still queued in the library's pool, and ours are
        // all there (empty ones waiting, or handed back by `get_packet`): take them out first, empty pool
        // before filled, as the FFmpeg wrapper does. The headers are ours and are freed below, so nothing
        // is freed here; the encoder's dequeue thread has nothing in flight after end of stream.
        let mut header: *mut nvPacket = ptr::null_mut();
        // SAFETY: `ctx` is a live encoder handle and `header` a writable out-pointer; the pointers handed
        // out are our own pool headers, which this code never dereferences.
        while unsafe { (self.api.encoder_dq_empty_packet)(self.ctx, &mut header) } == 0 {}
        // SAFETY: as above; `get_packet(.., false)` of the library only hands out our own headers too.
        while unsafe { (self.api.encoder_get_packet)(self.ctx, &mut header, false) } == 0 {}
        // SAFETY: `ctx` is a live encoder handle owned by `self`; this is its single close, after which the
        // handle is nulled and never used again.
        let rc = unsafe { (self.api.encoder_close)(self.ctx) };
        self.ctx = ptr::null_mut();
        let slots = std::mem::take(&mut self.slots);
        if rc == 0 {
            free_headers(slots);
        } else {
            // close leaves the session (and its dequeue thread) alive when the thread did not stop in time:
            // that thread may still write into the packets, so they are leaked rather than freed under it
            log::warn!("libnvmpi did not close the encoder cleanly ({rc}): its packet pool is leaked");
            std::mem::forget(slots);
        }
    }
}

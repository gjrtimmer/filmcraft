//! The seam between this backend's logic (safe code, unit-tested against mock sessions on any
//! Linux) and libnvmpi (the `unsafe` sessions in `session_dec` / `session_enc`, which only work on
//! a Jetson). Everything here is plain data and the two session traits.

use filmcraft_codecs::hw::NalCodec;

/// What a decoder session is created with.
#[derive(Clone, Copy, Debug)]
pub struct DecParams {
    pub codec: NalCodec,
    /// 10-bit HEVC: P010 output, else NV12.
    pub ten_bit: bool,
    /// The stream's picture size (a hint, so the library can size its frame pool early).
    pub width: u32,
    pub height: u32,
}

/// Where a decoder session copies a picture: a luma plane and an interleaved chroma plane, both
/// `stride` bytes per row (NV12, or P010 with 16-bit samples).
pub struct PlaneBuf {
    pub luma: Vec<u8>,
    pub chroma: Vec<u8>,
    pub stride: usize,
}

/// The outcome of asking a decoder session for a picture.
#[derive(Debug, PartialEq, Eq)]
pub enum DecOut {
    /// A picture was copied into the buffer: its size and the timestamp the packet carried.
    Frame { width: u32, height: u32, pts: i64 },
    /// No picture is ready (non-blocking), or the wait timed out.
    None,
    /// The library refused (a libnvmpi error code); a held picture that did not fit comes here too.
    Failed(i32),
}

/// A libnvmpi decoder session. One thread at a time (`&mut self`); dropping it closes the session
/// exactly once.
pub trait DecBackend: Send {
    /// Queue one Annex B access unit (never an empty one: that is the library's end-of-stream
    /// marker). `Err` is the library's negative code.
    fn put_packet(&mut self, annex_b: &[u8], pts: i64) -> Result<(), i32>;
    /// Copy the next picture, in presentation order, into `buf`; `wait` blocks for up to the
    /// session's wait timeout.
    fn get_frame(&mut self, buf: &mut PlaneBuf, wait: bool) -> DecOut;
    /// Reset the pipeline for a seek: in-flight pictures are discarded and the next packet must
    /// bring the parameter sets again.
    fn flush(&mut self);
}

/// What an encoder session is created with. Frames go in as planar 4:2:0 (8-bit).
#[derive(Clone, Copy, Debug)]
pub struct EncParams {
    pub codec: NalCodec,
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub bitrate_bps: u32,
    pub peak_bitrate_bps: u32,
    /// VBR instead of CBR rate control.
    pub vbr: bool,
    /// Frames between IDR pictures.
    pub keyint: u32,
    /// H.264 `profile_idc` (66 / 77 / 100); ignored for HEVC (Main).
    pub h264_profile: u32,
    /// H.264 `level_idc` (41 = 4.1), 0 lets the encoder choose.
    pub level: u32,
}

/// One encoded access unit in Annex B form, as the library delivers it.
#[derive(Debug, PartialEq, Eq)]
pub struct EncPacket {
    pub data: Vec<u8>,
    /// The library's keyframe flag (`AV_PKT_FLAG_KEY`).
    pub key: bool,
    /// The timestamp the frame was submitted with.
    pub pts: i64,
}

/// The outcome of asking an encoder session for a packet.
#[derive(Debug, PartialEq, Eq)]
pub enum EncOut {
    Packet(EncPacket),
    /// Nothing ready (non-blocking, a timeout, or nothing in flight).
    None,
    /// End of stream: everything submitted has come out.
    Eos,
    /// The library refused (its negative code).
    Failed(i32),
}

/// A libnvmpi encoder session. One thread at a time (`&mut self`); dropping it ends the stream,
/// drains it and closes the session exactly once.
pub trait EncBackend: Send {
    /// Queue one picture: `planes` are Y, U, V with `strides` bytes per row. `0` is queued, the
    /// library's `EAGAIN` (-11) means not queued (drain packets, retry the same picture), any other
    /// value is an error.
    fn put_frame(&mut self, planes: [&[u8]; 3], strides: [u32; 3], pts: i64) -> i32;
    /// End of stream (never refused). `0` or the library's code.
    fn put_eos(&mut self) -> i32;
    /// The next encoded access unit; once end of stream was sent, blocks until one arrives or the
    /// stream is over ([`EncOut::Eos`]).
    fn get_packet(&mut self, wait: bool) -> EncOut;
}

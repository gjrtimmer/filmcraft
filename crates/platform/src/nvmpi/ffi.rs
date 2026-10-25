//! libnvmpi declarations transcribed from `include/nvmpi.h` of jetson-nvmpi v4.6.5 (the release the
//! Jetson container pins). Only what this backend calls: the CPU-copy decoder and encoder.
//! Linux LP64 only: C `unsigned long` and `time_t` are 64-bit. Enums are plain `c_int`s so a value
//! the library invents can never be an invalid Rust discriminant.
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

/// `NV_PIX_NV12`: Y plane + interleaved CbCr.
pub const NV_PIX_NV12: c_int = 0;
/// `NV_PIX_YUV420`: three planes.
pub const NV_PIX_YUV420: c_int = 1;
/// `NV_PIX_P010`: 10-bit 4:2:0, 16-bit little-endian samples with the 10 bits high.
pub const NV_PIX_P010: c_int = 2;
pub const NV_VIDEO_CodingH264: c_int = 1;
pub const NV_VIDEO_CodingHEVC: c_int = 6;

/// `NVMPI_CODEC_SUPPORTED`: `nvmpi_query_codec` says this process can use the codec.
pub const NVMPI_CODEC_SUPPORTED: c_int = 0;
/// `NVMPI_CODEC_UNSUPPORTED_HW`: the library can, this module or driver cannot.
pub const NVMPI_CODEC_UNSUPPORTED_HW: c_int = 1;
/// `NVMPI_CODEC_UNSUPPORTED_BUILD`: this libnvmpi build cannot, on any module.
pub const NVMPI_CODEC_UNSUPPORTED_BUILD: c_int = 2;

/// `NVMPI_ERR_EAGAIN`: the frame was not queued; drain packets and retry the same frame.
pub const NVMPI_ERR_EAGAIN: c_int = -11;
/// `NVMPI_ERR_INVAL`.
pub const NVMPI_ERR_INVAL: c_int = -22;
/// `NVMPI_ERR_FRAME_SIZE`: the decoded frame does not fit the caller's planes.
pub const NVMPI_ERR_FRAME_SIZE: c_int = -90;
/// `NVMPI_ENC_CHUNK_SIZE`: size of the buffer behind every encoder packet (a frame above it is dropped
/// by the library), and the default size of one decoder input buffer.
pub const NVMPI_ENC_CHUNK_SIZE: usize = 10 * 1024 * 1024;

/// `nvmpictx`: opaque handle, only ever passed back to the API family that made it.
#[repr(C)]
pub struct nvmpictx {
    _private: [u8; 0],
}

/// `nvPacket`.
#[repr(C)]
pub struct nvPacket {
    pub flags: c_ulong,
    pub payload_size: c_ulong,
    pub payload: *mut u8,
    pub pts: c_ulong,
    pub privData: *mut c_void,
}

/// `nvFrame` (`time_t` is an `i64` on LP64 Linux).
#[repr(C)]
pub struct nvFrame {
    pub flags: c_ulong,
    pub payload_size: [c_ulong; 3],
    pub payload: [*mut u8; 3],
    pub linesize: [c_uint; 3],
    pub type_: c_int,
    pub width: c_uint,
    pub height: c_uint,
    pub timestamp: i64,
}

/// `nvSize`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct nvSize {
    pub width: c_uint,
    pub height: c_uint,
}

/// `nvDecParam`.
#[repr(C)]
pub struct nvDecParam {
    pub frame_pool_size: c_int,
    pub codingType: c_int,
    pub pixFormat: c_int,
    pub resized: nvSize,
    pub chunk_size: c_uint,
    pub max_perf: c_int,
    pub disable_dpb: c_int,
    pub wait_timeout: c_int,
    pub width: c_uint,
    pub height: c_uint,
}

/// `nvEncParam`.
#[repr(C)]
pub struct nvEncParam {
    pub width: c_uint,
    pub height: c_uint,
    pub profile: c_uint,
    pub level: c_uint,
    pub bitrate: c_uint,
    pub peak_bitrate: c_uint,
    pub enableLossless: c_char,
    pub mode_vbr: c_char,
    pub insert_spspps_idr: c_char,
    pub insert_vui: c_char,
    pub pixFormat: c_int,
    pub iframe_interval: c_uint,
    pub idr_interval: c_uint,
    pub fps_n: c_uint,
    pub fps_d: c_uint,
    pub capture_num: c_int,
    pub max_b_frames: c_uint,
    pub refs: c_uint,
    pub qmax: c_uint,
    pub qmin: c_uint,
    pub hw_preset_type: c_uint,
    pub vbv_buffer_size: c_uint,
    pub codingType: c_int,
    pub max_perf: c_int,
    pub poc_type: c_uint,
    pub wait_timeout: c_uint,
    pub enable_cabac: c_char,
    pub insert_aud: c_char,
    pub use_dmabuf: c_int,
    pub nonblocking: c_int,
}

/// `nvmpi_create_decoder`.
pub type CreateDecoder = unsafe extern "C" fn(*mut nvDecParam) -> *mut nvmpictx;
/// `nvmpi_decoder_put_packet`.
pub type DecoderPutPacket = unsafe extern "C" fn(*mut nvmpictx, *mut nvPacket) -> c_int;
/// `nvmpi_decoder_get_frame2`.
pub type DecoderGetFrame2 = unsafe extern "C" fn(*mut nvmpictx, *mut nvFrame, bool) -> c_int;
/// `nvmpi_decoder_flush`.
pub type DecoderFlush = unsafe extern "C" fn(*mut nvmpictx) -> c_int;
/// `nvmpi_decoder_close`.
pub type DecoderClose = unsafe extern "C" fn(*mut nvmpictx) -> c_int;
/// `nvmpi_create_encoder`.
pub type CreateEncoder = unsafe extern "C" fn(*mut nvEncParam) -> *mut nvmpictx;
/// `nvmpi_encoder_put_frame` (a null frame is end of stream).
pub type EncoderPutFrame = unsafe extern "C" fn(*mut nvmpictx, *mut nvFrame) -> c_int;
/// `nvmpi_encoder_get_packet`.
pub type EncoderGetPacket = unsafe extern "C" fn(*mut nvmpictx, *mut *mut nvPacket, bool) -> c_int;
/// `nvmpi_encoder_dqEmptyPacket` (takes an empty packet back out of the pool; 0 while there is one).
pub type EncoderDqEmptyPacket = unsafe extern "C" fn(*mut nvmpictx, *mut *mut nvPacket) -> c_int;
/// `nvmpi_encoder_qEmptyPacket`.
pub type EncoderQEmptyPacket = unsafe extern "C" fn(*mut nvmpictx, *mut nvPacket);
/// `nvmpi_encoder_close`.
pub type EncoderClose = unsafe extern "C" fn(*mut nvmpictx) -> c_int;
/// `nvmpi_query_codec(type, is_encoder, pix_fmt)` (libnvmpi >= 4.10.0; optional, see the loader).
pub type QueryCodec = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;

//! LP64 layouts printed by a C compiler (clang, LP64) from `include/nvmpi.h` of jetson-nvmpi v4.6.5:
//! `sizeof`, `_Alignof` and `offsetof` of every field below and the constant values. Regenerate
//! with a C program that includes the header and prints those.
use super::ffi::*;
use std::mem::{align_of, offset_of, size_of};

#[test]
fn packet_and_frame_layouts_match_the_header() {
    assert_eq!((size_of::<nvPacket>(), align_of::<nvPacket>()), (40, 8));
    assert_eq!(offset_of!(nvPacket, flags), 0);
    assert_eq!(offset_of!(nvPacket, payload_size), 8);
    assert_eq!(offset_of!(nvPacket, payload), 16);
    assert_eq!(offset_of!(nvPacket, pts), 24);
    assert_eq!(offset_of!(nvPacket, privData), 32);
    assert_eq!((size_of::<nvFrame>(), align_of::<nvFrame>()), (88, 8));
    assert_eq!(offset_of!(nvFrame, flags), 0);
    assert_eq!(offset_of!(nvFrame, payload_size), 8);
    assert_eq!(offset_of!(nvFrame, payload), 32);
    assert_eq!(offset_of!(nvFrame, linesize), 56);
    assert_eq!(offset_of!(nvFrame, type_), 68);
    assert_eq!(offset_of!(nvFrame, width), 72);
    assert_eq!(offset_of!(nvFrame, height), 76);
    assert_eq!(offset_of!(nvFrame, timestamp), 80);
}

#[test]
fn decoder_parameters_match_the_header() {
    assert_eq!((size_of::<nvDecParam>(), align_of::<nvDecParam>()), (44, 4));
    assert_eq!(offset_of!(nvDecParam, frame_pool_size), 0);
    assert_eq!(offset_of!(nvDecParam, codingType), 4);
    assert_eq!(offset_of!(nvDecParam, pixFormat), 8);
    assert_eq!(offset_of!(nvDecParam, resized), 12);
    assert_eq!(offset_of!(nvDecParam, chunk_size), 20);
    assert_eq!(offset_of!(nvDecParam, max_perf), 24);
    assert_eq!(offset_of!(nvDecParam, disable_dpb), 28);
    assert_eq!(offset_of!(nvDecParam, wait_timeout), 32);
    assert_eq!(offset_of!(nvDecParam, width), 36);
    assert_eq!(offset_of!(nvDecParam, height), 40);
}

#[test]
fn encoder_parameters_match_the_header() {
    assert_eq!((size_of::<nvEncParam>(), align_of::<nvEncParam>()), (104, 4));
    let offsets = [
        (offset_of!(nvEncParam, width), 0),
        (offset_of!(nvEncParam, height), 4),
        (offset_of!(nvEncParam, profile), 8),
        (offset_of!(nvEncParam, level), 12),
        (offset_of!(nvEncParam, bitrate), 16),
        (offset_of!(nvEncParam, peak_bitrate), 20),
        (offset_of!(nvEncParam, enableLossless), 24),
        (offset_of!(nvEncParam, mode_vbr), 25),
        (offset_of!(nvEncParam, insert_spspps_idr), 26),
        (offset_of!(nvEncParam, insert_vui), 27),
        (offset_of!(nvEncParam, pixFormat), 28),
        (offset_of!(nvEncParam, iframe_interval), 32),
        (offset_of!(nvEncParam, idr_interval), 36),
        (offset_of!(nvEncParam, fps_n), 40),
        (offset_of!(nvEncParam, fps_d), 44),
        (offset_of!(nvEncParam, capture_num), 48),
        (offset_of!(nvEncParam, max_b_frames), 52),
        (offset_of!(nvEncParam, refs), 56),
        (offset_of!(nvEncParam, qmax), 60),
        (offset_of!(nvEncParam, qmin), 64),
        (offset_of!(nvEncParam, hw_preset_type), 68),
        (offset_of!(nvEncParam, vbv_buffer_size), 72),
        (offset_of!(nvEncParam, codingType), 76),
        (offset_of!(nvEncParam, max_perf), 80),
        (offset_of!(nvEncParam, poc_type), 84),
        (offset_of!(nvEncParam, wait_timeout), 88),
        (offset_of!(nvEncParam, enable_cabac), 92),
        (offset_of!(nvEncParam, insert_aud), 93),
        (offset_of!(nvEncParam, use_dmabuf), 96),
        (offset_of!(nvEncParam, nonblocking), 100),
    ];
    for (i, (got, want)) in offsets.into_iter().enumerate() {
        assert_eq!(got, want, "nvEncParam field {i}");
    }
}

#[test]
fn constants_match_the_header() {
    assert_eq!((NV_PIX_NV12, NV_PIX_YUV420, NV_PIX_P010), (0, 1, 2));
    assert_eq!((NV_VIDEO_CodingH264, NV_VIDEO_CodingHEVC), (1, 6));
    assert_eq!((NVMPI_ERR_EAGAIN, NVMPI_ERR_INVAL, NVMPI_ERR_FRAME_SIZE), (-11, -22, -90));
    assert_eq!(NVMPI_ENC_CHUNK_SIZE, 10_485_760);
    assert_eq!(size_of::<std::ffi::c_int>(), 4, "nvPixFormat / nvCodingType are 4-byte enums");
}

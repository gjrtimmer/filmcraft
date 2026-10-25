//! Tests of the codec probe against a stand-in for `nvmpi_query_codec`.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;

use filmcraft_codecs::hw::NalCodec;

use super::ffi::*;
use super::query::*;

fn fresh() -> Cache {
    Mutex::new(HashMap::new())
}

fn answer(code: Option<i32>) -> Result<(), String> {
    check_with(&fresh(), Key::decoder(NalCodec::Hevc, true), |_| code)
}

#[test]
fn a_supported_codec_is_tried() {
    assert_eq!(answer(Some(NVMPI_CODEC_SUPPORTED)), Ok(()));
}

#[test]
fn unsupported_hardware_is_declined_with_the_reason() {
    let why = answer(Some(NVMPI_CODEC_UNSUPPORTED_HW)).unwrap_err();
    assert!(why.contains("HEVC decoding (P010)") && why.contains("hardware"), "{why}");
}

#[test]
fn an_unsupported_build_is_declined_with_the_reason() {
    let why = answer(Some(NVMPI_CODEC_UNSUPPORTED_BUILD)).unwrap_err();
    assert!(why.contains("build"), "{why}");
}

#[test]
fn an_inconclusive_answer_or_an_error_code_is_tried_as_before() {
    for code in [-1, NVMPI_ERR_INVAL, 3, 99, i32::MIN] {
        assert_eq!(answer(Some(code)), Ok(()), "{code}");
    }
}

#[test]
fn a_missing_symbol_is_tried_as_before() {
    assert_eq!(answer(None), Ok(()));
}

#[test]
fn definite_answers_are_asked_once_and_inconclusive_ones_again() {
    let cache = fresh();
    let calls = Cell::new(0);
    let key = Key::encoder(NalCodec::H264);
    for _ in 0..3 {
        let r = check_with(&cache, key, |_| {
            calls.set(calls.get() + 1);
            Some(NVMPI_CODEC_UNSUPPORTED_HW)
        });
        assert!(r.is_err());
    }
    assert_eq!(calls.get(), 1, "cached");
    let other = Key::encoder(NalCodec::Hevc);
    for _ in 0..3 {
        let _ = check_with(&cache, other, |_| {
            calls.set(calls.get() + 1);
            Some(-1)
        });
    }
    assert_eq!(calls.get(), 4, "-1 is not cached");
}

#[test]
fn the_keys_are_the_sessions_this_backend_opens() {
    let k = Key::decoder(NalCodec::H264, false);
    assert_eq!((k.codec, k.is_encoder, k.pix), (NV_VIDEO_CodingH264, false, NV_PIX_NV12));
    let k = Key::decoder(NalCodec::Hevc, false);
    assert_eq!((k.codec, k.is_encoder, k.pix), (NV_VIDEO_CodingHEVC, false, NV_PIX_NV12));
    let k = Key::decoder(NalCodec::Hevc, true);
    assert_eq!((k.codec, k.is_encoder, k.pix), (NV_VIDEO_CodingHEVC, false, NV_PIX_P010));
    let k = Key::encoder(NalCodec::Hevc);
    assert_eq!((k.codec, k.is_encoder, k.pix), (NV_VIDEO_CodingHEVC, true, NV_PIX_YUV420));
}

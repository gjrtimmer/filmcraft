//! Asking libnvmpi whether it can run a codec before a session is opened (`nvmpi_query_codec`,
//! libnvmpi >= 4.10.0). A combination the library or the module cannot do is declined here, so the
//! software path takes it without a trial session on the Tegra driver (which dislikes rapid open /
//! close). Safe code: the one `unsafe` call is [`super::loader::Api::query_codec`].
//!
//! Only a definite "no" declines. An inconclusive answer (-1), an error code, a missing symbol (older
//! libnvmpi) and a missing library all mean "try as before": the session itself then reports. Definite
//! answers are cached here, so hot paths (a decoder per clip) do not repeat the FFI call; the library
//! caches them too, but does not cache -1, and neither do we.

use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::{LazyLock, Mutex, PoisonError};

use filmcraft_codecs::hw::NalCodec;

use super::ffi::*;

/// What `nvmpi_query_codec` answered, in this backend's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Supported,
    /// This libnvmpi build can, this module or driver cannot.
    UnsupportedHw,
    /// This libnvmpi build cannot, on any module.
    UnsupportedBuild,
    /// Inconclusive, an error code, or no answer at all: create the session as before.
    Unknown,
}

impl Verdict {
    /// The verdict for a raw `nvmpi_query_codec` result (`None`: the symbol is missing). Anything the
    /// header does not define, `-1` included, is [`Verdict::Unknown`].
    pub fn from_code(code: Option<c_int>) -> Self {
        match code {
            Some(NVMPI_CODEC_SUPPORTED) => Self::Supported,
            Some(NVMPI_CODEC_UNSUPPORTED_HW) => Self::UnsupportedHw,
            Some(NVMPI_CODEC_UNSUPPORTED_BUILD) => Self::UnsupportedBuild,
            _ => Self::Unknown,
        }
    }

    /// Whether the verdict is cached (only definite answers are).
    fn definite(self) -> bool {
        self != Self::Unknown
    }

    /// `Err` with the reason for a definite "no", `Ok` for everything else.
    pub fn allow(self, what: &str) -> Result<(), String> {
        match self {
            Self::UnsupportedHw => Err(format!("libnvmpi reports {what} unsupported by this module's hardware")),
            Self::UnsupportedBuild => Err(format!("libnvmpi reports {what} unsupported by this libnvmpi build")),
            Self::Supported | Self::Unknown => Ok(()),
        }
    }
}

/// A codec, direction and raw pixel layout, as `nvmpi_query_codec` takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub codec: c_int,
    pub is_encoder: bool,
    pub pix: c_int,
}

impl Key {
    /// The decoder session `session_dec` opens: NV12 output, or P010 for 10-bit HEVC.
    pub fn decoder(codec: NalCodec, ten_bit: bool) -> Self {
        match (codec, ten_bit) {
            (NalCodec::H264, _) => Self { codec: NV_VIDEO_CodingH264, is_encoder: false, pix: NV_PIX_NV12 },
            (NalCodec::Hevc, false) => Self { codec: NV_VIDEO_CodingHEVC, is_encoder: false, pix: NV_PIX_NV12 },
            (NalCodec::Hevc, true) => Self { codec: NV_VIDEO_CodingHEVC, is_encoder: false, pix: NV_PIX_P010 },
        }
    }

    /// The encoder session `session_enc` opens: planar 4:2:0 input.
    pub fn encoder(codec: NalCodec) -> Self {
        let codec = match codec {
            NalCodec::H264 => NV_VIDEO_CodingH264,
            NalCodec::Hevc => NV_VIDEO_CodingHEVC,
        };
        Self { codec, is_encoder: true, pix: NV_PIX_YUV420 }
    }

    fn describe(self) -> String {
        let codec = if self.codec == NV_VIDEO_CodingH264 {
            "H.264"
        } else if self.codec == NV_VIDEO_CodingHEVC {
            "HEVC"
        } else {
            "codec"
        };
        let pix = match self.pix {
            NV_PIX_NV12 => "NV12",
            NV_PIX_YUV420 => "YUV420",
            NV_PIX_P010 => "P010",
            _ => "?",
        };
        format!("{codec} {} ({pix})", if self.is_encoder { "encoding" } else { "decoding" })
    }
}

/// Definite answers by key.
pub type Cache = Mutex<HashMap<Key, Verdict>>;

/// `key`'s verdict: from `cache`, else from `ask` (not cached when inconclusive).
pub fn verdict_with(cache: &Cache, key: Key, ask: impl FnOnce(Key) -> Option<c_int>) -> Verdict {
    // the lock is held across `ask` on purpose: two threads asking for one key make one FFI call
    let mut known = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(v) = known.get(&key) {
        return *v;
    }
    let v = Verdict::from_code(ask(key));
    if v.definite() {
        known.insert(key, v);
    }
    v
}

/// Whether `key` may be tried: `Err` only for a definite "no".
pub fn check_with(cache: &Cache, key: Key, ask: impl FnOnce(Key) -> Option<c_int>) -> Result<(), String> {
    verdict_with(cache, key, ask).allow(&key.describe())
}

/// [`check_with`] against the loaded libnvmpi and the process-wide cache.
pub fn check(key: Key) -> Result<(), String> {
    static CACHE: LazyLock<Cache> = LazyLock::new(|| Mutex::new(HashMap::new()));
    check_with(&CACHE, key, |k| super::loader::api().ok().and_then(|api| api.query_codec(k.codec, k.is_encoder, k.pix)))
}

//! Helpers of `tests/nvmpi.rs`: telling the libnvmpi mock from a Jetson, and comparing hardware
//! pictures with the software decoder's within a tolerance (or exactly, see [`EXPECT_EXACT_ENV`]).
//!
//! The mock (jetson-nvmpi `mock/`, used by CI runners without a Jetson) stands in for the Tegra
//! V4L2 and NvBufSurface libraries under their real sonames, so the real libnvmpi runs on top of it.
//! It models the sessions (buffers, back-pressure, end of stream, resolution events), not the
//! codec: decoded pictures are gray and come out in decode order (`NVMPI_MOCK_DEC_REORDER` delays
//! them, as a decoder's reorder buffer does, but keeps that order), and encoded P pictures are
//! all-skip. Tests of hardware semantics skip those checks on the mock and say so
//! with `SKIPPED (mock): ...`, a wording distinct from `SKIPPED: no nvmpi`.
#![allow(dead_code)]

use std::sync::OnceLock;

use filmcraft_codecs::DecodedFrame;
use filmcraft_frame::PixelData;

/// The NvBufSurface library libnvmpi links (its soname on L4T and in the mock).
const NVBUFSURFACE: &str = "libnvbufsurface.so.1.0.0";
/// A test hook only the mock's NvBufSurface library exports.
const MOCK_SYMBOL: &[u8] = b"mock_nvbuf_register_export\0";

/// Whether the libnvmpi of this process runs on the jetson-nvmpi mock instead of a Jetson's
/// drivers. Decided by what is loaded, not by configuration, so a Jetson can never be taken for
/// the mock.
#[allow(unsafe_code)]
pub fn on_mock() -> bool {
    static MOCK: OnceLock<bool> = OnceLock::new();
    *MOCK.get_or_init(|| {
        if filmcraft_platform::nvmpi::probe().is_err() {
            return false;
        }
        // SAFETY: libnvmpi is loaded (the probe above) and links this library, so opening it again
        // only takes another reference and runs no initialiser; the symbol is looked up, never called.
        unsafe { libloading::Library::new(NVBUFSURFACE).is_ok_and(|lib| lib.get::<*const ()>(MOCK_SYMBOL).is_ok()) }
    })
}

/// Print why a check is skipped on the mock.
pub fn skipped_on_mock(why: &str) {
    eprintln!("SKIPPED (mock): {why}");
}

/// Set to `1` when the libnvmpi under test is 4.10.1 or newer (jetson-nvmpi #163: decoder surfaces keep
/// the stream's colorimetry and range, so decoded pictures are bit-exact with software decode). libnvmpi
/// has no version API and `nvmpi_query_codec` / `nvmpi_get_caps` do not tell 4.10.1 from 4.10.0, so CI
/// says so; unset, the PSNR tolerance of [`MIN_PSNR_DB`] applies.
pub const EXPECT_EXACT_ENV: &str = "FILMCRAFT_NVMPI_EXPECT_EXACT";

/// Whether `FILMCRAFT_NVMPI_EXPECT_EXACT=1` asks for exact pixels.
pub fn expect_exact() -> bool {
    std::env::var(EXPECT_EXACT_ENV).is_ok_and(|v| v.trim() == "1")
}

/// What [`assert_close`] measured, per plane Y / U / V.
pub struct Worst {
    /// Lowest PSNR in dB (99 on the mock).
    pub psnr: [f64; 3],
    /// Largest absolute sample difference (0 on the mock).
    pub diff: [u32; 3],
}

/// Lowest PSNR (dB) a hardware picture may have against the software decoder's, per plane. The
/// Tegra decoder output is not bit-exact today: libnvmpi's VIC copy colour-converts BT.709 and
/// full-range pictures (up to ~50 luma levels on coloured pixels; jetson-nvmpi #163 on GitHub,
/// gjrtimmer/jetson-nvmpi). The floor only rules out wrong or garbage pictures; that each picture
/// is the right one is checked against its neighbours (see [`assert_close`]).
pub const MIN_PSNR_DB: f64 = 20.0;

/// PSNR of two sample planes with `peak` as the largest value; 99 dB when identical.
fn psnr<T: Copy + Into<f64>>(a: &[T], b: &[T], peak: f64) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| ((*x).into() - (*y).into()).powi(2)).sum::<f64>() / a.len().max(1) as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (peak * peak / mse).log10() }
}

/// PSNR of plane `i` of two pictures of the same format and size.
fn plane_psnr(x: &DecodedFrame, y: &DecodedFrame, i: usize) -> f64 {
    match (&x.frame.data, &y.frame.data) {
        (PixelData::Yuv8 { planes: p, .. }, PixelData::Yuv8 { planes: q, .. }) => psnr(&p[i], &q[i], 255.0),
        (PixelData::Yuv16 { planes: p, bits, .. }, PixelData::Yuv16 { planes: q, .. }) => psnr(&p[i], &q[i], f64::from((1u32 << bits) - 1)),
        _ => panic!("pts {}: pixel formats differ", x.pts),
    }
}

/// Largest absolute sample difference in plane `i` of two pictures of the same format and size.
fn plane_max_diff(x: &DecodedFrame, y: &DecodedFrame, i: usize) -> u32 {
    fn max_diff<T: Copy + Into<u32>>(a: &[T], b: &[T]) -> u32 {
        a.iter().zip(b).map(|(p, q)| (*p).into().abs_diff((*q).into())).max().unwrap_or(0)
    }
    match (&x.frame.data, &y.frame.data) {
        (PixelData::Yuv8 { planes: p, .. }, PixelData::Yuv8 { planes: q, .. }) => max_diff(&p[i], &q[i]),
        (PixelData::Yuv16 { planes: p, .. }, PixelData::Yuv16 { planes: q, .. }) => max_diff(&p[i], &q[i]),
        _ => panic!("pts {}: pixel formats differ", x.pts),
    }
}

/// The pixel format of a picture: bit depth, chroma and plane sizes.
fn layout(f: &DecodedFrame) -> String {
    match &f.frame.data {
        PixelData::Yuv8 { planes, chroma, .. } => format!("8-bit {chroma:?} {:?}", planes.each_ref().map(|p| p.len())),
        PixelData::Yuv16 { planes, chroma, bits, .. } => format!("{bits}-bit {chroma:?} {:?}", planes.each_ref().map(|p| p.len())),
        _ => "not planar YUV".into(),
    }
}

/// Assert the hardware pictures `hw` are the software decoder's `sw`: the same timestamps in the
/// same (presentation) order, every one of them (libnvmpi 4.6.6 drains the end of the stream:
/// jetson-nvmpi #161 on GitHub), size, aspect, colour and pixel format, and on a Jetson pixels within [`MIN_PSNR_DB`] per plane,
/// each picture closer to its own software picture than to the neighbouring ones. On the mock (gray
/// pictures in decode order) the timestamps are compared as a set and the pixels not at all.
/// With `FILMCRAFT_NVMPI_EXPECT_EXACT=1` the pixels must also be identical (largest difference 0 in
/// every plane). Returns the worst PSNR and the largest difference of each plane (see [`Worst`]).
pub fn assert_close(what: &str, hw: &[DecodedFrame], sw: &[DecodedFrame], mock: bool) -> Worst {
    let mut hw: Vec<&DecodedFrame> = hw.iter().collect();
    if mock {
        hw.sort_by_key(|f| f.pts);
    }
    let (pa, pb): (Vec<i64>, Vec<i64>) = (hw.iter().map(|f| f.pts).collect(), sw.iter().map(|f| f.pts).collect());
    assert_eq!(pa, pb, "{what}: pts sequence");
    let exact = expect_exact();
    let mut worst = Worst { psnr: [99.0; 3], diff: [0; 3] };
    for (i, (x, y)) in hw.iter().zip(sw).enumerate() {
        let (fx, fy) = (&x.frame, &y.frame);
        assert_eq!((fx.width, fx.height, fx.par, fx.color), (fy.width, fy.height, fy.par, fy.color), "{what}: pts {} geometry / colour", x.pts);
        assert_eq!(layout(x), layout(y), "{what}: pts {} pixel format", x.pts);
        if mock {
            continue;
        }
        for plane in 0..3 {
            let db = plane_psnr(x, y, plane);
            let diff = plane_max_diff(x, y, plane);
            assert!(db >= MIN_PSNR_DB, "{what}: pts {} plane {plane} at {db:.1} dB against the software decoder", x.pts);
            assert!(
                !exact || diff == 0,
                "{what}: pts {} plane {plane} differs from the software decoder by up to {diff} ({EXPECT_EXACT_ENV}=1 wants exact pixels)",
                x.pts
            );
            worst.psnr[plane] = worst.psnr[plane].min(db);
            worst.diff[plane] = worst.diff[plane].max(diff);
        }
        let own = plane_psnr(x, y, 0);
        for n in [i.checked_sub(1), i.checked_add(1)].into_iter().flatten().filter_map(|n| sw.get(n)) {
            let other = plane_psnr(x, n, 0);
            assert!(own >= other, "{what}: pts {} is closer to software pts {} ({other:.1} dB) than to its own ({own:.1} dB)", x.pts, n.pts);
        }
    }
    worst
}

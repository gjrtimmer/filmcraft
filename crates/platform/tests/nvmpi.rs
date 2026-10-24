//! libnvmpi hardware decoding and encoding on a Jetson (Linux aarch64), against our software
//! decoders. Every test skips (and says so) when libnvmpi, a decoder / encoder device or ffmpeg (the
//! fixture generator) is missing, and the Tegra driver crashes on concurrent sessions, so the tests
//! of this binary run one at a time. Its own test binary: one test turns the process-wide Hardware
//! decoding switch off and on.
//!
//! On the jetson-nvmpi mock (CI without a Jetson) the session lifecycle runs for real (open, cap,
//! decline, end of stream, restart), while the checks of what only hardware does (presentation
//! order, pixels, encoded P pictures) are skipped with `SKIPPED (mock): ...` (see `nvmpi_common`).
//!
//! Never run it while a CI job uses the same Orin.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

#[path = "it/common/mod.rs"]
mod common;
mod nvmpi_common;

use std::sync::{Mutex, MutexGuard, PoisonError};

use common::*;
use filmcraft_codecs::VideoDecoder;
use filmcraft_export::{EncoderFrame, ExportSettings, Format, HardwareEncoding};
use filmcraft_frame::PixelData;
use filmcraft_time::FrameRate;
use nvmpi_common::{assert_close, on_mock, skipped_on_mock};

/// One test at a time: the Tegra V4L2 driver crashes on concurrent sessions.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The nvmpi decoder for `s`, or `None` (with the reason on stderr) when this system has none.
fn hardware(s: &Stream) -> Option<Box<dyn VideoDecoder>> {
    if let Err(why) = filmcraft_platform::nvmpi::probe() {
        eprintln!("SKIPPED: no nvmpi: {why}");
        return None;
    }
    match filmcraft_platform::nvmpi_factory(&s.entry) {
        Some(Ok(d)) => Some(d),
        Some(Err(e)) => panic!("factory error: {e}"),
        None => {
            eprintln!("SKIPPED: nvmpi declined {}", s.entry.codec.name());
            None
        }
    }
}

fn software(s: &Stream) -> Box<dyn VideoDecoder> {
    filmcraft_codecs::software_video_decoder(&s.entry).unwrap()
}

fn fixtures(ff: &std::path::Path) -> Vec<(String, Stream)> {
    ["h264_high.mp4", "hevc_main.mp4", "hevc_main10.mp4"].iter().filter_map(|n| Some((n.to_string(), read_stream(&named(ff, n)?)))).collect()
}

#[test]
fn decodes_the_pictures_of_the_software_decoder() {
    let _serial = serial();
    let ff = filmcraft_testkit::require_ffmpeg!();
    let mock = on_mock();
    if mock {
        skipped_on_mock("presentation order and pixels (the mock returns gray pictures in decode order); frames, timestamps and formats are checked");
    }
    for (name, s) in fixtures(&ff) {
        if mock && name.starts_with("hevc") {
            skipped_on_mock(&format!("{name} (the mock reads no HEVC picture size: its pictures do not fit and the decoder goes to software)"));
            continue;
        }
        let Some(mut hw) = hardware(&s) else { continue };
        let mut sw = software(&s);
        let a = decode_all(hw.as_mut(), &s.samples);
        assert!(a.len() >= 40, "{name}: {} pictures", a.len());
        let worst = assert_close(&name, &a, &decode_all(sw.as_mut(), &s.samples), mock);
        assert!(hw.name().starts_with("nvmpi"), "{name}: stayed in hardware ({})", hw.name());
        if !mock {
            eprintln!("{name}: worst luma PSNR against the software decoder {worst:.1} dB");
        }
        // reset + reseek to each later sync sample: the pictures from the seek point on must match
        // (pictures shown before an open-GOP I picture are concealed differently, RASL are dropped)
        for k in (1..s.samples.len()).filter(|&i| s.sync[i]).rev() {
            let end = (k + 17).min(s.samples.len());
            hw.reset();
            sw.reset();
            let start = s.samples[k].1;
            let mut a = decode_all(hw.as_mut(), &s.samples[k..end]);
            let mut b = decode_all(sw.as_mut(), &s.samples[k..end]);
            a.retain(|f| f.pts >= start);
            b.retain(|f| f.pts >= start);
            assert!(!a.is_empty(), "{name}: pictures after seeking to {k}");
            assert_close(&format!("{name} from sample {k}"), &a, &b, mock);
        }
    }
}

#[test]
fn declines_what_nvmpi_does_not_take_and_what_must_not_hold_a_session() {
    let _serial = serial();
    let ff = filmcraft_testkit::require_ffmpeg!();
    let src = "testsrc2=s=640x360:r=24:d=1";
    let cases: [(&str, Vec<&str>); 3] = [
        ("va_h264_interlaced.mp4", vec!["-c:v", "libx264", "-x264-params", "interlaced=1:keyint=12", "-pix_fmt", "yuv420p"]),
        ("va_h264_10bit.mp4", vec!["-c:v", "libx264", "-profile:v", "high10", "-pix_fmt", "yuv420p10le"]),
        ("va_hevc_422.mp4", vec!["-c:v", "libx265", "-tag:v", "hvc1", "-x265-params", "log-level=error:keyint=12", "-pix_fmt", "yuv422p10le"]),
    ];
    for (name, enc) in cases {
        let args: Vec<&str> = ["-f", "lavfi", "-i", src].into_iter().chain(enc).collect();
        let Some(path) = fixture(&ff, name, &args) else { continue };
        assert!(filmcraft_platform::nvmpi_factory(&read_stream(&path).entry).is_none(), "{name}: declined");
    }
    let Some(path) = named(&ff, "h264_high.mp4") else { return };
    let s = read_stream(&path);
    // background work (thumbnails) never opens a session, and neither does Hardware decoding Off
    assert!(filmcraft_media::cancel::with_background(true, || filmcraft_platform::nvmpi_factory(&s.entry)).is_none(), "background work declined");
    filmcraft_codecs::hw::set_hardware_decoding(false);
    let off = filmcraft_platform::nvmpi_factory(&s.entry).is_none();
    filmcraft_codecs::hw::set_hardware_decoding(true);
    assert!(off, "declined while hardware decoding is Off");
}

#[test]
fn the_session_cap_declines_the_decoder_over_the_limit_and_frees_the_slot_on_drop() {
    let _serial = serial();
    if std::env::var_os(filmcraft_platform::nvmpi::cap::MAX_SESSIONS_ENV).is_some() {
        eprintln!("SKIPPED: the decoder limit is overridden by the environment");
        return;
    }
    let ff = filmcraft_testkit::require_ffmpeg!();
    let Some(path) = named(&ff, "h264_high.mp4") else { return };
    let s = read_stream(&path);
    let Some(first) = hardware(&s) else { return };
    let second = hardware(&s).expect("a second decoder is within the default limit");
    assert!(filmcraft_platform::nvmpi_factory(&s.entry).is_none(), "the third decoder is over the limit and goes to software");
    drop(first);
    assert!(filmcraft_platform::nvmpi_factory(&s.entry).is_some(), "a closed session gives its slot back");
    drop(second);
}

#[test]
fn a_decoder_runs_again_after_end_of_stream_without_hanging() {
    let _serial = serial();
    let ff = filmcraft_testkit::require_ffmpeg!();
    let Some(path) = named(&ff, "h264_high.mp4") else { return };
    let s = read_stream(&path);
    let Some(mut hw) = hardware(&s) else { return };
    let mut sw: Vec<i64> = decode_all(software(&s).as_mut(), &s.samples).iter().map(|f| f.pts).collect();
    sw.sort_unstable();
    // `flush` ends the session with the end-of-stream marker and replaces it: every run decodes the
    // whole stream again, and none waits without bound on the library
    for run in 0..3 {
        let t = std::time::Instant::now();
        let mut pts: Vec<i64> = decode_all(hw.as_mut(), &s.samples).iter().map(|f| f.pts).collect();
        assert!(t.elapsed() < std::time::Duration::from_secs(30), "run {run} took {:?}", t.elapsed());
        pts.sort_unstable();
        // libnvmpi 4.6.6 drains the end of the stream: none of the last pictures is lost (jetson-nvmpi #161)
        assert_eq!(pts, sw, "run {run}: pictures");
    }
    assert!(hw.name().starts_with("nvmpi"), "stayed in hardware ({})", hw.name());
}

#[test]
fn one_encoder_at_a_time_and_a_closed_one_gives_its_slot_back() {
    let _serial = serial();
    if let Err(why) = filmcraft_platform::nvmpi::probe() {
        eprintln!("SKIPPED: no nvmpi: {why}");
        return;
    }
    let settings = |format| ExportSettings { format, hardware_encoding: HardwareEncoding::Auto, bitrate_kbps: 2000, ..Default::default() };
    let open = |format| filmcraft_platform::nvmpi::enc::factory(format, 640, 360, FrameRate::FPS_24, &settings(format));
    let Some(first) = open(Format::H264) else {
        eprintln!("SKIPPED: nvmpi declined H264");
        return;
    };
    let first = first.unwrap_or_else(|e| panic!("H264: {e}"));
    assert!(open(Format::H264).is_none(), "a second H.264 export goes to the software encoder");
    // H.265 has no software encoder: over the limit it is an error that says why
    assert!(matches!(open(Format::Hevc), Some(Err(_))), "a second H.265 export is an error");
    drop(first);
    assert!(matches!(open(Format::H264), Some(Ok(_))), "a closed encoder gives its slot back");
}

/// A moving gradient with a bright square, so every picture differs and edges exist.
fn picture(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut rgba = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let inside = x.abs_diff(40 + t * 6) < 24 && y.abs_diff(h / 2) < 24;
            let v = if inside { [250, 240, 30] } else { [(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y + t * 3) % 256) as u8] };
            rgba[(y * w + x) * 4..][..3].copy_from_slice(&v);
        }
    }
    rgba
}

fn luma_psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(x, y)| f64::from(x.abs_diff(*y)).powi(2)).sum::<f64>() / a.len().max(1) as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// Encode 72 pictures with nvmpi and read the stream back with our software decoder: every picture, in
/// order, close to the BT.709 conversion of what went in; keyframe first; no composition offsets.
fn round_trip(format: Format) {
    let _serial = serial();
    if let Err(why) = filmcraft_platform::nvmpi::probe() {
        eprintln!("SKIPPED: no nvmpi: {why}");
        return;
    }
    let (w, h, frames) = (1280usize, 720usize, 72usize);
    let rate = FrameRate::FPS_24;
    let settings = ExportSettings { format, hardware_encoding: HardwareEncoding::Auto, bitrate_kbps: 6000, keyframe_distance: Some(24), ..Default::default() };
    let Some(made) = filmcraft_platform::nvmpi::enc::factory(format, w as u32, h as u32, rate, &settings) else {
        eprintln!("SKIPPED: nvmpi declined {format:?}");
        return;
    };
    let mut enc = made.unwrap_or_else(|e| panic!("{format:?}: {e}"));
    let (mut packets, mut sources) = (Vec::new(), Vec::new());
    for t in 0..frames {
        let rgba = picture(w, h, t);
        let f = EncoderFrame { width: w as u32, height: h as u32, rgba: &rgba, hdr: None, index: t as u64 };
        packets.extend(enc.encode(&f).unwrap_or_else(|e| panic!("{format:?} picture {t}: {e}")));
        let (mut y, mut u, mut v) = (Vec::new(), Vec::new(), Vec::new());
        filmcraft_export::rgba_to_yuv420_8(&rgba, w, h, &mut y, &mut u, &mut v);
        sources.push(y);
    }
    packets.extend(enc.flush().unwrap());
    assert_eq!(packets.len(), frames, "{format:?}: one access unit per picture");
    assert!(packets[0].key, "{format:?}: the first picture is a keyframe");
    assert!(packets.iter().all(|p| p.composition_offset == 0 && p.duration == 1), "{format:?}: no reordering");
    let entry = enc.sample_entry();
    let mut dec = filmcraft_codecs::software_video_decoder(&entry).unwrap();
    let mut out = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        out.extend(dec.decode(&p.data, i as i64).unwrap_or_else(|e| panic!("{format:?} sample {i}: {e}")));
    }
    out.extend(dec.flush());
    assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), (0..frames as i64).collect::<Vec<_>>(), "{format:?}: pictures in order");
    // the mock codes P pictures as all-skip (they repeat the last keyframe); its keyframes carry the picture
    let mock = on_mock();
    if mock {
        skipped_on_mock(&format!("{format:?} P picture quality (the mock's P pictures repeat the keyframe); keyframes are checked"));
    }
    for ((f, src), p) in out.iter().zip(&sources).zip(&packets) {
        if mock && !p.key {
            continue;
        }
        let PixelData::Yuv8 { planes, .. } = &f.frame.data else { panic!("8-bit planes") };
        let db = luma_psnr(&planes[0], src);
        assert!(db > 30.0, "{format:?} picture {}: luma PSNR {db:.1} dB", f.pts);
    }
}

#[test]
fn h264_from_nvmpi_decodes_with_our_decoder() {
    round_trip(Format::H264);
}

#[test]
fn hevc_from_nvmpi_decodes_with_our_decoder() {
    round_trip(Format::Hevc);
}

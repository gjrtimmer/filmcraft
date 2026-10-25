//! The end of an encode against the mock session: finishing hands out every access unit still held by
//! the encoder (jetson-nvmpi puts the tail out only after the end-of-stream marker), none is lost.

use filmcraft_codecs::hw::NalCodec;
use filmcraft_export::VideoEncoder;

use super::enc_tests::{Mock, encoder, frame};

/// Encode `n` pictures through `mock` and finish: every picture's packet, in submission order.
fn encode_and_finish(mock: Mock, codec: NalCodec, n: u64) -> usize {
    // the mock's HEVC parameter sets are those of a 1280x720 stream, which the encoder checks
    let (w, h) = if codec == NalCodec::Hevc { (1280, 720) } else { (16, 16) };
    let mut e = encoder(mock, codec, w, h);
    let rgba = vec![60u8; (w * h * 4) as usize];
    let mut out = Vec::new();
    for i in 0..n {
        out.extend(e.encode(&frame(w, h, i, &rgba)).unwrap());
    }
    let during = out.len();
    out.extend(e.flush().unwrap());
    // the library's encoder delay (`latency`) keeps pictures back until the end of stream
    assert!(during <= out.len());
    assert_eq!(out.iter().filter(|p| p.key).count(), 1, "one keyframe: the first picture");
    out.len()
}

#[test]
fn finishing_drains_every_queued_packet_whatever_the_encoder_holds_back() {
    // B-frames are off, so the delay is the library's own queue: from none to all pictures held
    for latency in [0, 1, 3, 6, 100] {
        for codec in [NalCodec::H264, NalCodec::Hevc] {
            let (mock, log) = match codec {
                NalCodec::H264 => Mock::h264(latency),
                NalCodec::Hevc => Mock::hevc(latency),
            };
            let got = encode_and_finish(mock, codec, 12);
            assert_eq!(got, 12, "{codec:?} with {latency} pictures held back: packets == pictures submitted");
            let log = log.lock().unwrap();
            assert_eq!((log.frames.len(), log.eos), (12, 1), "{codec:?}: submitted pictures, end-of-stream markers");
        }
    }
}

#[test]
fn a_finished_encoder_has_nothing_left_to_read() {
    let (mock, _) = Mock::h264(100);
    let mut e = encoder(mock, NalCodec::H264, 16, 16);
    let rgba = vec![60u8; 16 * 16 * 4];
    for i in 0..5 {
        e.encode(&frame(16, 16, i, &rgba)).unwrap();
    }
    assert_eq!(e.flush().unwrap().len(), 4, "the first picture came out with the first batch, the other four at the end");
    assert!(e.flush().unwrap().is_empty(), "nothing is left after the end of the stream");
}

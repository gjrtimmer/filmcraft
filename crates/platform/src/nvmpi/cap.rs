//! The global session cap. The Tegra V4L2 driver crashes on concurrent sessions and on rapid open /
//! close (it segfaults instead of returning `EBUSY`), and FilmCraft opens many decoders (thumbnails,
//! background workers, probes): at most [`DEFAULT_DECODERS`] decoder sessions and [`MAX_ENCODERS`]
//! encoder session exist at a time, counted here. A [`Permit`] is held for the life of a session and
//! gives its slot back when dropped, on every path, including an open that fails after it.
//!
//! `FILMCRAFT_NVMPI_MAX_SESSIONS` overrides the decoder limit (the encoder limit stays 1).

use std::sync::atomic::{AtomicUsize, Ordering};

/// Decoder sessions at a time unless `FILMCRAFT_NVMPI_MAX_SESSIONS` says otherwise.
pub const DEFAULT_DECODERS: usize = 2;
/// Encoder sessions at a time.
pub const MAX_ENCODERS: usize = 1;
/// The decoder limit's environment variable.
pub const MAX_SESSIONS_ENV: &str = "FILMCRAFT_NVMPI_MAX_SESSIONS";
/// Largest decoder limit the variable can ask for.
const MAX_CONFIGURED: usize = 16;

static DECODERS: AtomicUsize = AtomicUsize::new(0);
static ENCODERS: AtomicUsize = AtomicUsize::new(0);

/// What a session is, which picks its counter and limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Decoder,
    Encoder,
}

/// One session slot, given back when dropped.
#[derive(Debug)]
pub struct Permit {
    counter: &'static AtomicUsize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        // saturating: a double release can never wrap the counter and lock every later session out
        update(self.counter, |n| Some(n.saturating_sub(1)));
    }
}

/// The decoder limit for a `FILMCRAFT_NVMPI_MAX_SESSIONS` value: a number from 0 (no hardware
/// decoders) to 16; anything else is the default.
pub fn decoder_limit(value: Option<&str>) -> usize {
    value.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n <= MAX_CONFIGURED).unwrap_or(DEFAULT_DECODERS)
}

/// Apply `f` to the counter atomically; false when `f` declined (returned `None`).
fn update(counter: &AtomicUsize, f: impl Fn(usize) -> Option<usize>) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = f(current) else { return false };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(seen) => current = seen,
        }
    }
}

fn try_take(counter: &'static AtomicUsize, limit: usize) -> Option<Permit> {
    // `then`, not `then_some`: a permit built for a refused take would give back a slot it never held
    update(counter, |n| (n < limit).then_some(n + 1)).then(|| Permit { counter })
}

/// A slot for a session of `kind`, or `None` when the cap is reached (the caller declines and the
/// software path takes the work).
pub fn acquire(kind: Kind) -> Option<Permit> {
    match kind {
        Kind::Decoder => try_take(&DECODERS, decoder_limit(std::env::var(MAX_SESSIONS_ENV).ok().as_deref())),
        Kind::Encoder => try_take(&ENCODERS, MAX_ENCODERS),
    }
}

/// Sessions of `kind` open right now (diagnostics and tests).
pub fn in_use(kind: Kind) -> usize {
    match kind {
        Kind::Decoder => DECODERS.load(Ordering::Acquire),
        Kind::Encoder => ENCODERS.load(Ordering::Acquire),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_comes_from_the_variable_or_the_default() {
        assert_eq!(decoder_limit(None), DEFAULT_DECODERS);
        assert_eq!(decoder_limit(Some("4")), 4);
        assert_eq!(decoder_limit(Some(" 1 ")), 1);
        assert_eq!(decoder_limit(Some("0")), 0);
        for bad in ["", "-1", "x", "17", "99999999999999999999", "2.5"] {
            assert_eq!(decoder_limit(Some(bad)), DEFAULT_DECODERS, "{bad:?}");
        }
    }

    #[test]
    fn permits_stop_at_the_limit_and_come_back_on_drop() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let a = try_take(&COUNT, 2).expect("first");
        let b = try_take(&COUNT, 2).expect("second");
        assert!(try_take(&COUNT, 2).is_none(), "the third is over the limit");
        drop(a);
        let c = try_take(&COUNT, 2).expect("a dropped permit frees its slot");
        assert!(try_take(&COUNT, 2).is_none());
        drop((b, c));
        assert_eq!(COUNT.load(Ordering::Acquire), 0);
        assert!(try_take(&COUNT, 0).is_none(), "a limit of zero takes nothing");
    }

    #[test]
    fn a_double_release_cannot_wrap_the_counter() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        drop(Permit { counter: &COUNT });
        assert_eq!(COUNT.load(Ordering::Acquire), 0);
    }

    #[test]
    fn decoders_and_encoders_are_counted_apart() {
        let before = (in_use(Kind::Decoder), in_use(Kind::Encoder));
        let enc = acquire(Kind::Encoder);
        // other tests may hold the one encoder slot: only a slot we got is checked
        if enc.is_some() {
            assert_eq!(in_use(Kind::Encoder), before.1 + 1);
            assert_eq!(in_use(Kind::Decoder), before.0, "an encoder takes no decoder slot");
            assert!(acquire(Kind::Encoder).is_none(), "one encoder at a time");
        }
        drop(enc);
    }
}

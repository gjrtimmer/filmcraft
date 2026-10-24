//! Run-time loading of libnvmpi (`libnvmpi.so.1`, then `libnvmpi.so`), as the other Linux backends
//! load theirs: building needs no libnvmpi headers or library, and a system without it simply has no
//! nvmpi backend. The library stays loaded for the life of the process, so the function pointers
//! never dangle.

use std::sync::OnceLock;

use super::ffi::*;

/// Library names tried in order: the versioned soname the deb installs, then the development link.
const NAMES: [&str; 2] = ["libnvmpi.so.1", "libnvmpi.so"];

/// The libnvmpi entry points this backend calls.
pub struct Api {
    /// The name the library was found under.
    pub name: &'static str,
    pub create_decoder: CreateDecoder,
    pub decoder_put_packet: DecoderPutPacket,
    pub decoder_get_frame2: DecoderGetFrame2,
    pub decoder_flush: DecoderFlush,
    pub decoder_close: DecoderClose,
    pub create_encoder: CreateEncoder,
    pub encoder_put_frame: EncoderPutFrame,
    pub encoder_get_packet: EncoderGetPacket,
    pub encoder_q_empty_packet: EncoderQEmptyPacket,
    pub encoder_close: EncoderClose,
    /// Keeps the library mapped; never dropped (the `Api` lives in a static).
    _library: libloading::Library,
}

/// The function pointer `name` (NUL-terminated) of `library`.
///
/// # Safety
/// `T` must be the symbol's real signature (see [`super::ffi`]).
unsafe fn symbol<T: Copy>(library: &libloading::Library, name: &'static [u8]) -> Result<T, String> {
    // SAFETY: the caller states that `T` is the signature of `name`; the pointer is copied out and
    // the library is kept loaded by the `Api` that stores it.
    unsafe { library.get::<T>(name).map(|s| *s) }.map_err(|e| e.to_string())
}

impl Api {
    fn open() -> Result<Self, String> {
        let mut last = String::from("no library name");
        for name in NAMES {
            // SAFETY: loading libnvmpi runs its initialisers, which only set up static state.
            match unsafe { libloading::Library::new(name) } {
                Ok(library) => return Self::resolve(name, library),
                Err(e) => last = format!("{name}: {e}"),
            }
        }
        Err(format!("no libnvmpi: {last}"))
    }

    fn resolve(name: &'static str, library: libloading::Library) -> Result<Self, String> {
        // SAFETY: every `T` below is the signature declared for that symbol in `nvmpi.h` (v4.x).
        unsafe {
            Ok(Self {
                name,
                create_decoder: symbol(&library, b"nvmpi_create_decoder\0")?,
                decoder_put_packet: symbol(&library, b"nvmpi_decoder_put_packet\0")?,
                // the capacity-checked copy (libnvmpi >= 4.6): an older library has no such symbol
                decoder_get_frame2: symbol(&library, b"nvmpi_decoder_get_frame2\0")?,
                decoder_flush: symbol(&library, b"nvmpi_decoder_flush\0")?,
                decoder_close: symbol(&library, b"nvmpi_decoder_close\0")?,
                create_encoder: symbol(&library, b"nvmpi_create_encoder\0")?,
                encoder_put_frame: symbol(&library, b"nvmpi_encoder_put_frame\0")?,
                encoder_get_packet: symbol(&library, b"nvmpi_encoder_get_packet\0")?,
                encoder_q_empty_packet: symbol(&library, b"nvmpi_encoder_qEmptyPacket\0")?,
                encoder_close: symbol(&library, b"nvmpi_encoder_close\0")?,
                _library: library,
            })
        }
    }
}

/// The loaded library, or why there is none. Loaded once; the answer is cached.
pub fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(Api::open).as_ref().map_err(Clone::clone)
}

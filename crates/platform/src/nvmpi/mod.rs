//! Hardware video decoding and encoding through libnvmpi on NVIDIA Jetson (Linux, aarch64): the
//! C library of jetson-nvmpi that wraps the Tegra V4L2 / NvBuffer multimedia API (`NVDEC` and
//! `NVENC` of the Orin family). The VA-API and NVDEC backends do not exist on Tegra, so this is its
//! hardware path. libnvmpi is loaded at run time ([`loader`]); without it there is no nvmpi
//! backend, never a failure to start.
//!
//! Safe logic and `unsafe` FFI are apart:
//!
//! - [`ffi`] (declarations), [`loader`], `session_dec` and `session_enc` (the sessions that call the
//!   library) are the only modules with `unsafe`;
//! - [`dec`] and [`enc`] drive a session through the [`backend`] traits and are safe code, tested
//!   against mock sessions on any Linux;
//! - [`cap`] is the global session cap (the Tegra driver crashes on concurrent sessions and on
//!   rapid open / close).
//!
//! `FILMCRAFT_NVMPI=0` turns the backend off. Pictures cross the library by CPU copy: there is no
//! zero-copy path to the GPU compositor.

pub mod backend;
pub mod cap;
pub mod dec;
pub mod ffi;
#[allow(unsafe_code)]
pub mod loader;
#[allow(unsafe_code)]
pub mod session_dec;

#[cfg(test)]
mod dec_tests;

#[cfg(test)]
mod abi_tests;

/// The environment variable that turns the backend off (`0`).
pub const DISABLE_ENV: &str = "FILMCRAFT_NVMPI";

/// Whether the environment leaves the backend on (anything but `FILMCRAFT_NVMPI=0`).
pub fn enabled() -> bool {
    enabled_for(std::env::var(DISABLE_ENV).ok().as_deref())
}

fn enabled_for(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

/// Whether this system can run the backend, and with which library: the environment allows it and
/// libnvmpi loads with every entry point. This opens no hardware session (a probe must not touch the
/// Tegra driver); a session the driver then refuses is declined by the factory that tried it.
pub fn probe() -> Result<&'static str, String> {
    if !enabled() {
        return Err(format!("turned off by {DISABLE_ENV}=0"));
    }
    loader::api().map(|api| api.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_zero_turns_the_backend_off() {
        assert!(enabled_for(None));
        for on in ["1", "", "auto", "yes"] {
            assert!(enabled_for(Some(on)), "{on:?}");
        }
        for off in ["0", " 0 "] {
            assert!(!enabled_for(Some(off)), "{off:?}");
        }
    }

    #[test]
    fn a_system_without_libnvmpi_is_reported_not_a_failure() {
        // this machine has no Tegra: either libnvmpi is missing (the usual case) or it loads
        match probe() {
            Ok(name) => assert!(name.starts_with("libnvmpi")),
            Err(why) => assert!(!why.is_empty()),
        }
    }
}

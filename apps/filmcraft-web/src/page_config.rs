//! The page config: an optional `./filmcraft-config.json` next to `index.html`, fetched once at
//! start-up (by `remote::load_config`, one fetch for every key). The keys are parsed by
//! [`crate::web_config`]; this module holds the result.

use std::cell::Cell;

use crate::web_config::{self, Startup};

thread_local! {
    static STARTUP: Cell<Option<Startup>> = const { Cell::new(None) };
}

/// The `startup` key (`None`: absent, invalid or no config file).
pub fn startup() -> Option<Startup> {
    STARTUP.with(Cell::get)
}

/// Apply the keys of a config file's bytes.
pub fn apply(bytes: &[u8]) {
    match web_config::parse_startup(bytes) {
        Ok(v) => STARTUP.with(|s| s.set(v)),
        Err(e) => log::warn!("filmcraft-config.json: {e}; ignored"),
    }
}

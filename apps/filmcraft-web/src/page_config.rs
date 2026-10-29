//! The page config: an optional `./filmcraft-config.json` next to `index.html`, fetched once at
//! start-up. The keys are parsed by [`crate::web_config`]; this module is the browser side.

use std::cell::Cell;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::web_config::{self, Startup};

/// Largest config file read (bytes).
const MAX_BYTES: u32 = 64 * 1024;

thread_local! {
    static STARTUP: Cell<Option<Startup>> = const { Cell::new(None) };
}

/// The `startup` key (`None`: absent, invalid or no config file).
pub fn startup() -> Option<Startup> {
    STARTUP.with(Cell::get)
}

/// Fetch and apply the config file; a missing or invalid file changes nothing.
pub async fn load() {
    match fetch().await {
        Ok(bytes) => apply(&bytes),
        Err(e) => log::info!("no page config: {e}"),
    }
}

/// Apply the keys of a config file's bytes.
pub fn apply(bytes: &[u8]) {
    match web_config::parse_startup(bytes) {
        Ok(v) => STARTUP.with(|s| s.set(v)),
        Err(e) => log::warn!("filmcraft-config.json: {e}; ignored"),
    }
}

async fn fetch() -> Result<Vec<u8>, String> {
    let win = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(win.fetch_with_str("filmcraft-config.json")).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp.dyn_into().map_err(|_| "not a Response".to_string())?;
    if resp.status() != 200 {
        return Err(format!("status {}", resp.status()));
    }
    let buf = JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
    let a = js_sys::Uint8Array::new(&buf);
    if a.length() > MAX_BYTES {
        return Err("config too large".into());
    }
    Ok(a.to_vec())
}

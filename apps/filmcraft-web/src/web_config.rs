//! The page-level keys of `filmcraft-config.json` that are not remote storage. Pure, so it runs
//! natively in `tests/it`.
//!
//! ```json
//! {"startup":"empty"}
//! ```

use serde_json::Value;

/// What the page opens when nothing was recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// The demo project (the default).
    Demo,
    /// An empty session.
    Empty,
}

/// The `startup` key. `Ok(None)` when it is absent; `Err` (to log as a warning, then ignore) for a
/// value other than `"empty"` or `"demo"`, or a file that is not a JSON object.
pub fn parse_startup(bytes: &[u8]) -> Result<Option<Startup>, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    match v.get("startup") {
        None => Ok(None),
        Some(Value::String(s)) if s == "empty" => Ok(Some(Startup::Empty)),
        Some(Value::String(s)) if s == "demo" => Ok(Some(Startup::Demo)),
        Some(_) => Err("startup must be \"empty\" or \"demo\"".into()),
    }
}

/// Whether the file has a `remote` key at all: a config with only other keys leaves remote
/// storage off without being an error.
pub fn has_remote(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).is_ok_and(|v| v.get("remote").is_some())
}

/// Whether to open the demo project: the URL flags win (`?empty`, then `?demo`), then the config.
pub fn open_demo(empty_flag: bool, demo_flag: bool, config: Option<Startup>) -> bool {
    if empty_flag {
        return false;
    }
    demo_flag || config != Some(Startup::Empty)
}

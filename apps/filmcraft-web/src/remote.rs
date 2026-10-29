//! Remote storage in the browser: files on a same-origin WebDAV server appear under mounted
//! virtual paths (`./filmcraft-config.json`, see [`crate::remote_config`]) and are read with
//! `fetch` (PROPFIND listings, `Range` GETs). The pure parts live in [`crate::remote_config`] and
//! [`crate::remote_dav`]; this module is the browser side: config load, listing and stat caches,
//! ranged reads for [`crate::fs`], and opening a project together with the sizes of its media.
//!
//! The engine's `Services` are synchronous, so everything they need is fetched ahead:
//! a directory listing starts on the first look and answers `WouldBlock` until it arrives, and a
//! project's media sizes are fetched before `file.open` runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use filmcraft_engine::media_browser::{DirEntry, Volume, VolumeKind};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::remote_config::{self, Config};
use crate::remote_dav::{self, Child};

/// A cached listing is shown for this long before it is refreshed (seconds).
const LIST_TTL_S: f64 = 15.0;
/// A failed listing is not retried for this long (seconds).
const ERROR_RETRY_S: f64 = 5.0;
/// Entries kept in the stat cache.
const STAT_MAX: usize = 200_000;
/// Media paths of one project whose sizes are fetched before it opens.
const MEDIA_MAX: usize = 10_000;
/// Largest config file read.
const CONFIG_MAX: usize = 64 * 1024;

/// What the server said about one path.
#[derive(Clone, Debug)]
pub struct Stat {
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<u64>,
    pub etag: Option<String>,
}

#[derive(Default)]
struct DirState {
    entries: Option<Vec<DirEntry>>,
    fetched: f64,
    loading: bool,
    error: Option<(String, f64)>,
}

#[derive(Default)]
struct State {
    cfg: Option<Config>,
    origin: String,
    stats: HashMap<String, Stat>,
    dirs: HashMap<String, DirState>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static T0: web_time::Instant = web_time::Instant::now();
}

fn now_s() -> f64 {
    T0.with(|t| t.elapsed().as_secs_f64())
}

/// Whether remote storage is configured.
pub fn enabled() -> bool {
    STATE.with(|s| s.borrow().cfg.is_some())
}

/// Whether `path` is on a remote mount.
pub fn is_remote(path: &str) -> bool {
    STATE.with(|s| s.borrow().cfg.as_ref().is_some_and(|c| c.mount_for(path).is_some()))
}

/// Each mount as a Network volume of the Media Browser.
pub fn volumes() -> Vec<Volume> {
    STATE.with(|s| {
        s.borrow().cfg.iter().flat_map(|c| c.mounts.iter()).map(|m| Volume { name: m.name.clone(), path: m.path.clone(), kind: VolumeKind::Network }).collect()
    })
}

/// The cached stat of a path (from a listing or a HEAD).
pub fn stat(path: &str) -> Option<Stat> {
    STATE.with(|s| s.borrow().stats.get(path.strip_suffix('/').unwrap_or(path)).cloned())
}

pub(crate) fn js_err(e: wasm_bindgen::JsValue) -> String {
    format!("{e:?}")
}

pub(crate) async fn fetch(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<web_sys::Response, String> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    // Same origin only: a redirect elsewhere fails instead of leaking the request.
    init.set_mode(web_sys::RequestMode::SameOrigin);
    init.set_cache(web_sys::RequestCache::NoCache);
    let h = web_sys::Headers::new().map_err(js_err)?;
    for (k, v) in headers {
        h.set(k, v).map_err(js_err)?;
    }
    init.set_headers(&h);
    if let Some(b) = body {
        init.set_body(&js_sys::Uint8Array::from(b));
    }
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let win = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(win.fetch_with_request(&req)).await.map_err(js_err)?;
    resp.dyn_into::<web_sys::Response>().map_err(|_| "fetch did not return a Response".to_string())
}

pub(crate) fn header(r: &web_sys::Response, name: &str) -> Option<String> {
    r.headers().get(name).ok().flatten()
}

/// The body, refused before reading when `Content-Length` exceeds `cap` and after when it does.
async fn read_body(r: &web_sys::Response, cap: usize) -> Result<Vec<u8>, String> {
    if header(r, "content-length").and_then(|h| remote_dav::parse_content_length(&h)).is_some_and(|n| n > cap as u64) {
        return Err("response too large".into());
    }
    let buf = JsFuture::from(r.array_buffer().map_err(js_err)?).await.map_err(js_err)?;
    let a = js_sys::Uint8Array::new(&buf);
    if a.length() as usize > cap {
        return Err("response too large".into());
    }
    Ok(a.to_vec())
}

pub(crate) fn cfg_and_origin() -> Result<(Config, String), String> {
    STATE.with(|s| {
        let s = s.borrow();
        s.cfg.clone().map(|c| (c, s.origin.clone())).ok_or_else(|| "remote storage is not configured".to_string())
    })
}

/// Read `./filmcraft-config.json` once for both of its parts: the page config
/// ([`crate::page_config::apply`], the `startup` key) and the remote mounts. Remote storage stays
/// off when the file is missing or invalid, or has no `remote` key.
pub async fn load_config() {
    let Some(loc) = web_sys::window().map(|w| w.location()) else { return };
    let (origin, path) = (loc.origin().unwrap_or_default(), loc.pathname().unwrap_or_default());
    let base_dir = path.get(..=path.rfind('/').unwrap_or(0)).unwrap_or("/").to_string();
    let bytes = match fetch("GET", "filmcraft-config.json", &[], None).await {
        Ok(r) if r.status() == 200 => read_body(&r, CONFIG_MAX).await,
        Ok(r) => Err(format!("status {}", r.status())),
        Err(e) => Err(e),
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => return log::info!("no page config: {e}"),
    };
    crate::page_config::apply(&bytes);
    if !crate::web_config::has_remote(&bytes) {
        return;
    }
    match remote_config::parse(&bytes, &origin, &base_dir) {
        Ok(cfg) => {
            log::info!("remote storage: {} mount(s)", cfg.mounts.len());
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.cfg = Some(cfg);
                s.origin = origin;
            });
        }
        Err(e) => log::info!("remote storage off: {e}"),
    }
}

/// PROPFIND `dir` (Depth 1) and map the answer to virtual paths.
async fn propfind(dir: &str) -> Result<Vec<Child>, String> {
    let (cfg, origin) = cfg_and_origin()?;
    let mount = cfg.mount_for(dir).ok_or_else(|| format!("{dir}: not on a remote mount"))?.clone();
    let url = cfg.url_for(&format!("{}/", dir.trim_end_matches('/')))?;
    let r = fetch("PROPFIND", &url, &[("Depth", "1"), ("Content-Type", "application/xml")], Some(remote_dav::PROPFIND_BODY.as_bytes())).await?;
    if r.status() != 207 {
        return Err(format!("{dir}: the server answered {} to a listing", r.status()));
    }
    let entries = remote_dav::parse_multistatus(&read_body(&r, remote_dav::MAX_XML_BYTES).await?)?;
    Ok(remote_dav::children_of(&cfg, &mount, dir, &origin, entries))
}

fn record_dir(dir: &str, res: Result<Vec<Child>, String>) {
    let now = now_s();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.dirs.entry(dir.to_string()).or_default();
        st.loading = false;
        match res {
            Ok(children) => {
                st.error = None;
                st.fetched = now;
                st.entries = Some(
                    children
                        .iter()
                        .map(|c| DirEntry { name: c.name.clone(), is_dir: c.entry.is_dir, size: c.entry.size, modified: c.entry.modified })
                        .collect(),
                );
                if s.stats.len() > STAT_MAX {
                    s.stats.clear();
                }
                for c in children {
                    let stat = Stat { size: c.entry.size.unwrap_or(0), is_dir: c.entry.is_dir, modified: c.entry.modified, etag: c.entry.etag };
                    s.stats.insert(c.path, stat);
                }
            }
            Err(e) => st.error = Some((e, now)),
        }
    });
}

fn start_listing(dir: String) {
    wasm_bindgen_futures::spawn_local(async move {
        let res = propfind(&dir).await;
        record_dir(&dir, res);
        crate::repaint();
    });
}

/// The Media Browser's listing of a directory under a mount; `None` for other paths. A listing
/// that is not here yet starts and answers `WouldBlock` ("Loading"); the browser retries on the
/// repaint that follows. An expired one is shown while it refreshes.
pub fn list(dir: &str) -> Option<io::Result<Vec<DirEntry>>> {
    let dir = dir.trim_end_matches('/').to_string();
    if !is_remote(&dir) {
        return None;
    }
    let now = now_s();
    let (result, start) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.dirs.entry(dir.clone()).or_default();
        let fresh = st.entries.is_some() && now - st.fetched < LIST_TTL_S;
        let blocked = st.loading || st.error.as_ref().is_some_and(|(_, at)| now - at < ERROR_RETRY_S);
        let start = !fresh && !blocked;
        if start {
            st.loading = true;
        }
        let result = match (&st.entries, &st.error) {
            (Some(e), _) => Ok(e.clone()),
            (None, Some((msg, at))) if now - at < ERROR_RETRY_S => Err(io::Error::other(msg.clone())),
            _ => Err(io::Error::new(io::ErrorKind::WouldBlock, "Loading…")),
        };
        (result, start)
    });
    if start {
        start_listing(dir);
    }
    Some(result)
}

/// A file's size: cached, else from its parent's listing, else `HEAD`.
pub async fn ensure_stat(path: &str) -> Result<Stat, String> {
    if let Some(s) = stat(path) {
        return Ok(s);
    }
    if let Some(parent) = remote_config::parent(path) {
        let res = propfind(parent).await;
        if let Err(e) = &res {
            log::info!("listing {parent}: {e}");
        }
        record_dir(parent, res);
        if let Some(s) = stat(path) {
            return Ok(s);
        }
    }
    let (cfg, _) = cfg_and_origin()?;
    let r = fetch("HEAD", &cfg.url_for(path)?, &[], None).await?;
    if r.status() != 200 {
        return Err(format!("{path}: the server answered {}", r.status()));
    }
    let size = header(&r, "content-length").and_then(|h| remote_dav::parse_content_length(&h)).ok_or_else(|| format!("{path}: the server gave no size"))?;
    let s = Stat { size, is_dir: false, modified: None, etag: header(&r, "etag") };
    STATE.with(|st| st.borrow_mut().stats.insert(path.to_string(), s.clone()));
    Ok(s)
}

/// Bytes `[start, start + len)` of a file of known `size`, as a validated `206` answer.
pub async fn fetch_range(path: &str, start: u64, len: u64, size: u64) -> Result<Vec<u8>, String> {
    let (cfg, _) = cfg_and_origin()?;
    let last = start.checked_add(len).and_then(|e| e.checked_sub(1)).ok_or("range overflows")?;
    let r = fetch("GET", &cfg.url_for(path)?, &[("Range", &format!("bytes={start}-{last}"))], None).await?;
    let expected = remote_dav::check_range_response(r.status(), header(&r, "content-range").as_deref(), start, len, size)?;
    if header(&r, "content-length").and_then(|h| remote_dav::parse_content_length(&h)).is_some_and(|n| n != expected) {
        return Err("Content-Length does not match the range".into());
    }
    let body = read_body(&r, usize::try_from(expected).map_err(|_| "range too large")?).await?;
    if body.len() as u64 != expected {
        return Err("short range body".into());
    }
    Ok(body)
}

/// The media (and proxy) file paths a project file refers to.
fn media_paths(bytes: &[u8]) -> Vec<String> {
    let Ok(loaded) = filmcraft_format::decode(bytes) else { return Vec::new() };
    let mut out = Vec::new();
    for it in loaded.project.items.values() {
        let Some(m) = it.as_media() else { continue };
        for r in std::iter::once(&m.media).chain(m.proxy.as_ref()) {
            if let filmcraft_engine::project::MediaRef::File { path } = r {
                out.push(path.clone());
            }
        }
    }
    out
}

/// Drop what is cached about `path` and its folder (after it was written): the next look asks again.
pub fn forget(path: &str) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.stats.remove(path);
        if let Some(p) = remote_config::parent(path) {
            s.dirs.remove(p);
        }
    });
}

/// Fetch the size of every remote file a project (its bytes) refers to, so the synchronous
/// `Services` find them when it opens. Missing ones stay missing: the engine reports them offline.
pub async fn prefetch_media(project: &[u8]) {
    for p in media_paths(project).into_iter().filter(|p| is_remote(p)).take(MEDIA_MAX) {
        if let Err(e) = ensure_stat(&p).await {
            log::info!("remote media {p}: {e}");
        }
    }
}

/// Open the project at a remote path: fetch it, fetch the sizes of its media, then `file.open`.
pub async fn open_project(path: &str) -> Result<Value, String> {
    let st = ensure_stat(path).await?;
    if st.is_dir || st.size > remote_dav::MAX_PROJECT_BYTES {
        return Err(format!("{path}: not a project file this size can open"));
    }
    let (cfg, _) = cfg_and_origin()?;
    let r = fetch("GET", &cfg.url_for(path)?, &[], None).await?;
    if r.status() != 200 {
        return Err(format!("{path}: the server answered {}", r.status()));
    }
    crate::remote_upload::note_opened(path, header(&r, "etag"));
    let bytes = read_body(&r, remote_dav::MAX_PROJECT_BYTES as usize).await?;
    prefetch_media(&bytes).await;
    crate::fs::put(path, bytes);
    let path = path.to_string();
    crate::import::on_ui(move |w, _| w.app.session.execute("file.open", json!({"path": path})).map_err(|e| e.to_string())).await
}

/// Route "open project by path" through the remote fetch (Media Browser, Open Recent, Home).
pub fn install_hooks(app: &mut filmcraft_ui_egui::FilmcraftApp) {
    if !enabled() {
        return;
    }
    let name = STATE.with(|s| s.borrow().cfg.as_ref().and_then(|c| c.mounts.first()).map(|m| m.name.clone())).unwrap_or_default();
    app.hooks.remote = Some(filmcraft_ui_egui::RemoteInfo { name });
    app.hooks.open_project = Some(Box::new(|path: &str| {
        let path = path.to_string();
        wasm_bindgen_futures::spawn_local(async move {
            let r = if is_remote(&path) {
                open_project(&path).await
            } else {
                let p = path.clone();
                crate::import::on_ui(move |w, _| w.app.session.execute("file.open", json!({"path": p})).map_err(|e| e.to_string())).await
            };
            if let Err(e) = r {
                crate::post(move |w, _| w.app.status(e));
            }
        });
    }));
}

//! Remote storage config and path mapping, including hostile inputs.

use crate::remote_config::{Config, parent, parse};

const ORIGIN: &str = "https://app.example";

fn cfg(json: &str) -> Result<Config, String> {
    parse(json.as_bytes(), ORIGIN, "/app/")
}

fn one() -> Config {
    cfg(r#"{"remote":{"mounts":[{"path":"/data","url":"dav/","name":"Server"}]}}"#).unwrap()
}

#[test]
fn relative_and_same_origin_urls_resolve_to_a_url_path() {
    assert_eq!(one().mounts[0].url_path, "/app/dav/");
    let c = cfg(r#"{"remote":{"mounts":[{"path":"/a","url":"https://APP.example/dav"},{"path":"/b","url":"/x/../y/"}]}}"#).unwrap();
    assert_eq!(c.mounts[0].url_path, "/dav/");
    assert_eq!(c.mounts[1].url_path, "/y/");
    assert_eq!(c.mounts[0].name, "a");
    assert!(!c.mounts[0].writable);
}

#[test]
fn foreign_or_odd_urls_are_refused() {
    for url in [
        "https://evil.example/dav/",
        "http://app.example/dav/",
        "//evil.example/dav/",
        "javascript:alert(1)",
        "data:text/plain,x",
        "../../../etc/",
        "dav/?x=1",
        "dav/#f",
        "dav\\x",
        "https://app.example.evil.example/dav/",
        "",
    ] {
        let j = format!(r#"{{"remote":{{"mounts":[{{"path":"/data","url":{}}}]}}}}"#, serde_json::json!(url));
        assert!(cfg(&j).is_err(), "{url} must be refused");
    }
}

#[test]
fn bad_configs_are_refused() {
    for j in [
        "",
        "not json",
        "{}",
        r#"{"remote":{"mounts":[]}}"#,
        r#"{"remote":{"mounts":[{"path":"/","url":"dav/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"data","url":"dav/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"/a/../b","url":"dav/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"/a//b","url":"dav/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"/files","url":"dav/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"/a","url":"dav/"},{"path":"/a/b","url":"dav2/"}]}}"#,
        r#"{"remote":{"mounts":[{"path":"/a","url":"dav/","name":""}]}}"#,
    ] {
        assert!(cfg(j).is_err(), "{j} must be refused");
    }
    let many = format!(r#"{{"remote":{{"mounts":[{}]}}}}"#, (0..17).map(|i| format!(r#"{{"path":"/m{i}","url":"d{i}/"}}"#)).collect::<Vec<_>>().join(","));
    assert!(cfg(&many).is_err());
    assert!(parse(&vec![b' '; 70_000], ORIGIN, "/").is_err());
}

#[test]
fn longest_prefix_mount_wins_and_segments_are_encoded() {
    let c = cfg(r#"{"remote":{"mounts":[{"path":"/data","url":"dav/"},{"path":"/other","url":"o/"}]}}"#).unwrap();
    assert_eq!(c.url_for("/data").unwrap(), "/app/dav/");
    assert_eq!(c.url_for("/data/My Clips/a#b.mp4").unwrap(), "/app/dav/My%20Clips/a%23b.mp4");
    assert_eq!(c.url_for("/data/dir/").unwrap(), "/app/dav/dir/");
    assert_eq!(c.url_for("/data/\u{e9}.mp4").unwrap(), "/app/dav/%C3%A9.mp4");
    assert_eq!(c.url_for("/other/x").unwrap(), "/app/o/x");
    assert!(c.url_for("/datax/a").is_err());
    assert!(c.url_for("/files/a").is_err());
}

#[test]
fn hostile_virtual_paths_never_map() {
    let c = one();
    for p in ["/data/../etc/passwd", "/data/a/../b", "/data//a", "/data/./a", "/data/a\\b", "/data/a\u{0}b", "/data/a\nb", "/data/a/..", "relative/x"] {
        assert!(c.url_for(p).is_err(), "{p:?} must not map");
    }
    let long = format!("/data/{}", "a/".repeat(200));
    assert!(c.url_for(&long).is_err());
    assert!(c.url_for(&format!("/data/{}", "a".repeat(5000))).is_err());
}

#[test]
fn hrefs_map_back_under_the_mount_only() {
    let c = one();
    let m = &c.mounts[0];
    let map = |h: &str| c.path_for_href(m, h, ORIGIN);
    assert_eq!(map("/app/dav/").as_deref(), Some("/data"));
    assert_eq!(map("/app/dav").as_deref(), Some("/data"));
    assert_eq!(map("/app/dav/My%20Clips/").as_deref(), Some("/data/My Clips"));
    assert_eq!(map("https://app.example/app/dav/a%23b.mp4").as_deref(), Some("/data/a#b.mp4"));
    assert_eq!(map("/app/dav/x.mp4?token=1").as_deref(), Some("/data/x.mp4"));
    for h in [
        "/other/x.mp4",
        "/app/davx/x.mp4",
        "/app/dav/../secret",
        "/app/dav/a/%2e%2e/b",
        "/app/dav/a%2Fb",
        "/app/dav/a%5Cb",
        "/app/dav/%00",
        "/app/dav//x",
        "/app/dav/%zz",
        "/app/dav/%ff",
        "https://evil.example/app/dav/x",
        "//evil.example/app/dav/x",
        "",
    ] {
        assert_eq!(map(h), None, "{h:?} must not map");
    }
}

#[test]
fn parent_of_paths() {
    assert_eq!(parent("/data/a/b.mp4"), Some("/data/a"));
    assert_eq!(parent("/data/a/"), Some("/data"));
    assert_eq!(parent("/data"), None);
}

//! Request building and answer handling of a save to a remote mount.

use crate::remote_config::{Config, parse};
use crate::remote_write::{
    Known, Outcome, Precondition, check_writable, classify_move, classify_put, delete_part, failure_message, head_target, move_part, part_path, precondition,
    put_part,
};

const ORIGIN: &str = "https://app.example";

fn cfg() -> Config {
    parse(br#"{"remote":{"mounts":[{"path":"/rw","url":"/dav/rw/","writable":true,"name":"Work"},{"path":"/ro","url":"/dav/ro/"}]}}"#, ORIGIN, "/").unwrap()
}

#[test]
fn part_file_is_beside_the_target() {
    assert_eq!(part_path("/rw/a/x.fcproj").unwrap(), "/rw/a/x.fcproj.part");
    assert!(part_path("/rw/a/").is_err());
    assert!(part_path("").is_err());
}

#[test]
fn only_writable_mounts_take_writes() {
    let c = cfg();
    assert!(check_writable(&c, "/rw/x.fcproj").is_ok());
    let e = check_writable(&c, "/ro/x.fcproj").unwrap_err();
    assert!(e.contains("read-only"), "{e}");
    assert!(check_writable(&c, "/elsewhere/x").is_err());
    assert!(put_part(&c, "/ro/x.fcproj").is_err());
    assert!(move_part(&c, ORIGIN, "/ro/x.fcproj", true).is_err());
    assert!(delete_part(&c, "/ro/x.fcproj").is_err());
}

#[test]
fn the_upload_goes_to_the_part_file_with_an_encoded_url() {
    let r = put_part(&cfg(), "/rw/My Films/a#b.fcproj").unwrap();
    assert_eq!((r.method, r.url.as_str()), ("PUT", "/dav/rw/My%20Films/a%23b.fcproj.part"));
    let h = head_target(&cfg(), "/rw/My Films/a#b.fcproj").unwrap();
    assert_eq!((h.method, h.url.as_str()), ("HEAD", "/dav/rw/My%20Films/a%23b.fcproj"));
}

#[test]
fn the_move_names_the_destination_and_overwrite() {
    let m = move_part(&cfg(), ORIGIN, "/rw/x.fcproj", true).unwrap();
    assert_eq!((m.method, m.url.as_str()), ("MOVE", "/dav/rw/x.fcproj.part"));
    assert!(m.headers.contains(&("Destination", "https://app.example/dav/rw/x.fcproj".to_string())));
    assert!(m.headers.contains(&("Overwrite", "T".to_string())));
    let m = move_part(&cfg(), ORIGIN, "/rw/x.fcproj", false).unwrap();
    assert!(m.headers.contains(&("Overwrite", "F".to_string())));
    // a conditional header on a MOVE would be tested against the .part file: never sent
    assert!(m.headers.iter().all(|(k, _)| !k.eq_ignore_ascii_case("if-match") && !k.eq_ignore_ascii_case("if-none-match")));
    let d = delete_part(&cfg(), "/rw/x.fcproj").unwrap();
    assert_eq!((d.method, d.url.as_str()), ("DELETE", "/dav/rw/x.fcproj.part"));
}

#[test]
fn hostile_paths_build_no_request() {
    for p in ["/rw/../ro/x", "/rw/a//b", "/rw/a\\b", "/rw/"] {
        assert!(put_part(&cfg(), p).is_err(), "{p}");
        assert!(move_part(&cfg(), ORIGIN, p, true).is_err(), "{p}");
    }
}

#[test]
fn a_changed_target_is_a_conflict_not_an_overwrite() {
    let was = Known::Etag("\"v1\"".into());
    assert_eq!(precondition(&was, 200, Some("\"v1\"")), Precondition::Proceed { overwrite: true });
    assert_eq!(precondition(&was, 200, Some("W/\"v1\"")), Precondition::Proceed { overwrite: true });
    assert_eq!(precondition(&was, 200, Some("\"v2\"")), Precondition::Conflict);
    // no ETag from the server now: it cannot be checked
    assert_eq!(precondition(&was, 200, None), Precondition::Proceed { overwrite: true });
    assert_eq!(precondition(&Known::NoEtag, 200, Some("\"x\"")), Precondition::Proceed { overwrite: true });
    // a file this session never saw must not exist
    assert_eq!(precondition(&Known::Unknown, 200, Some("\"x\"")), Precondition::Conflict);
    assert_eq!(precondition(&Known::Unknown, 404, None), Precondition::Proceed { overwrite: false });
    assert_eq!(precondition(&was, 404, None), Precondition::Proceed { overwrite: false });
    for s in [401, 403, 500, 503, 301] {
        assert!(matches!(precondition(&was, s, None), Precondition::Failed(_)), "{s}");
    }
}

#[test]
fn answers_are_classified_exactly() {
    for s in [200, 201, 204] {
        assert!(classify_put(s).is_ok());
    }
    for s in [202, 207, 400, 403, 409, 412, 413, 507] {
        assert!(classify_put(s).is_err(), "{s}");
    }
    assert_eq!(classify_move(201), Outcome::Saved);
    assert_eq!(classify_move(204), Outcome::Saved);
    assert_eq!(classify_move(412), Outcome::Conflict);
    for s in [200, 207, 403, 404, 409, 423, 500] {
        assert!(matches!(classify_move(s), Outcome::Failed(_)), "{s}");
    }
}

#[test]
fn failures_say_what_to_do() {
    assert_eq!(failure_message(&Outcome::Saved), None);
    assert!(failure_message(&Outcome::Conflict).unwrap().contains("Save As"));
    assert!(failure_message(&Outcome::Failed("x".into())).unwrap().contains("x"));
}

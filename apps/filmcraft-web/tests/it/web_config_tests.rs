//! The `startup` key of the web config and the rule for opening the demo.

use crate::web_config::{Startup, has_remote, open_demo, parse_startup};

#[test]
fn startup_values() {
    assert_eq!(parse_startup(br#"{"startup":"empty"}"#), Ok(Some(Startup::Empty)));
    assert_eq!(parse_startup(br#"{"startup":"demo"}"#), Ok(Some(Startup::Demo)));
    assert_eq!(parse_startup(br#"{"remote":{}}"#), Ok(None));
    assert_eq!(parse_startup(b"{}"), Ok(None));
}

#[test]
fn unknown_or_hostile_startup_is_an_error_to_ignore() {
    for j in [r#"{"startup":"Empty"}"#, r#"{"startup":""}"#, r#"{"startup":1}"#, r#"{"startup":null}"#, r#"{"startup":["empty"]}"#, "not json", ""] {
        assert!(parse_startup(j.as_bytes()).is_err(), "{j}");
    }
}

#[test]
fn a_config_with_only_startup_has_no_remote() {
    assert!(!has_remote(br#"{"startup":"empty"}"#));
    assert!(!has_remote(b"junk"));
    assert!(has_remote(br#"{"startup":"empty","remote":{"mounts":[]}}"#));
}

#[test]
fn demo_rule() {
    assert!(open_demo(false, false, None));
    assert!(open_demo(false, false, Some(Startup::Demo)));
    assert!(!open_demo(false, false, Some(Startup::Empty)));
    assert!(open_demo(false, true, Some(Startup::Empty)), "?demo forces it");
    assert!(!open_demo(true, false, None), "?empty still works");
    assert!(!open_demo(true, true, None), "?empty wins");
}

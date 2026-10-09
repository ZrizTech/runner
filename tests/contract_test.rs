//! Contract tests: the vendored schemas validate the vendored fixtures.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use std::fs;
use std::path::Path;
use zriz_runner::contract::{
    self, Error, ExchangeRequest, ExchangeResponse, Frame, Op, Result, Runner, Validator,
};

const FIXTURE_DIR: &str = "contract/fixtures/frames";

fn decode_any(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("decode fixture")
}

fn read_fixture(name: &str) -> Vec<u8> {
    fs::read(Path::new(FIXTURE_DIR).join(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[test]
fn round_trip() {
    struct Case {
        schema: &'static str,
    }
    let cases = [
        Case { schema: "frame" },
        Case { schema: "op" },
        Case { schema: "result" },
        Case { schema: "error" },
        Case {
            schema: "exchange-request",
        },
        Case {
            schema: "exchange-response",
        },
    ];

    for c in cases {
        let data = read_fixture(&format!("{}.valid-1.json", c.schema));
        let want = decode_any(&data);

        let got = match c.schema {
            "frame" => {
                let v: Frame = contract::decode(&data).expect("decode frame");
                serde_json::to_value(v).expect("encode")
            }
            "op" => {
                let v: Op = contract::decode(&data).expect("decode op");
                serde_json::to_value(v).expect("encode")
            }
            "result" => {
                let v: Result = contract::decode(&data).expect("decode result");
                serde_json::to_value(v).expect("encode")
            }
            "error" => {
                let v: Error = contract::decode(&data).expect("decode error");
                serde_json::to_value(v).expect("encode")
            }
            "exchange-request" => {
                let v: ExchangeRequest = contract::decode(&data).expect("decode exchange-request");
                serde_json::to_value(v).expect("encode")
            }
            "exchange-response" => {
                let v: ExchangeResponse =
                    contract::decode(&data).expect("decode exchange-response");
                serde_json::to_value(v).expect("encode")
            }
            other => panic!("unhandled schema {other}"),
        };

        assert_eq!(got, want, "round trip mismatch for {}", c.schema);
    }
}

#[test]
fn validator_checks_fixtures() {
    let v = Validator::new().expect("NewValidator");

    struct Case {
        name: &'static str,
        schema: &'static str,
        fixture: &'static str,
        want_err: bool,
    }
    let cases = [
        Case {
            name: "op valid",
            schema: "op",
            fixture: "op.valid-1.json",
            want_err: false,
        },
        Case {
            name: "op invalid",
            schema: "op",
            fixture: "op.invalid-1.json",
            want_err: true,
        },
        Case {
            name: "pipeline valid",
            schema: "pipeline",
            fixture: "pipeline.valid-1.json",
            want_err: false,
        },
        Case {
            name: "pipeline invalid",
            schema: "pipeline",
            fixture: "pipeline.invalid-2.json",
            want_err: true,
        },
    ];

    for c in cases {
        let data = read_fixture(c.fixture);
        let doc = decode_any(&data);
        let result = v.validate(c.schema, &doc);
        if c.want_err {
            assert!(result.is_err(), "case {}: want error", c.name);
        } else {
            assert!(result.is_ok(), "case {}: want ok, got {:?}", c.name, result);
        }
    }
}

#[test]
fn exchange_request_encodes_empty_frames_as_array() {
    let req = ExchangeRequest {
        v: 1,
        runner: Runner {
            runner_id: "r".to_string(),
            version: "1".to_string(),
            ops: vec![],
            resources: vec![],
            max_inflight: 1,
            env: String::new(),
        },
        inflight: 0,
        health: contract::Health {
            boot_id: "b-0123456789ab".to_string(),
            state: "ok".to_string(),
            browser: None,
            cli: None,
            worker: None,
            refused: 0,
        },
        frames: vec![],
    };
    let data = serde_json::to_value(&req).expect("marshal");
    assert_eq!(data["frames"], Value::Array(vec![]));
}

#[test]
fn deny_keys_contains_token() {
    let keys = contract::deny_keys().expect("DenyKeys");
    assert!(!keys.is_empty(), "DenyKeys = empty, want non-empty");
    assert!(
        keys.iter().any(|k| k == "token"),
        "DenyKeys = {keys:?}, want containing \"token\""
    );
}

#[test]
fn error_frame_fixtures_validate() {
    let v = Validator::new().expect("NewValidator");
    let mut seen = 0;
    for e in fs::read_dir(FIXTURE_DIR).expect("dir") {
        let name = e.expect("entry").file_name().into_string().expect("name");
        let Some(rest) = name.strip_prefix("error.") else {
            continue;
        };
        let doc = decode_any(&read_fixture(&name));
        let ok = v.validate("error", &doc).is_ok();
        assert_eq!(ok, rest.starts_with("valid"), "fixture {name}");
        seen += 1;
    }
    assert!(seen >= 6, "saw {seen} error fixtures");
}

#[test]
fn an_error_frame_has_details_and_no_message() {
    let v = Validator::new().expect("NewValidator");
    let e = Error::new(
        "op-1",
        "runner-at-capacity",
        serde_json::json!({"limit-name": "max-inflight", "limit": 8, "busy": 8, "waited-ms": 0}),
    );
    let doc = serde_json::to_value(&e).expect("encode");
    assert!(v.validate("error", &doc).is_ok(), "{doc}");
    assert!(doc.get("message").is_none());
    let bare = serde_json::to_value(Error::new("op-1", "spawn-failed", serde_json::json!({})))
        .expect("encode");
    assert_eq!(bare["details"], serde_json::json!({}));
    let mut with_message = doc.clone();
    with_message["message"] = serde_json::json!("x");
    assert!(v.validate("error", &with_message).is_err());
}

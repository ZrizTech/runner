//! The build id and the `User-Agent` header on exchange requests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::test_support::*;
use super::*;
use tokio_util::sync::CancellationToken;

async fn first_arrival(user_agent: Option<&str>) -> Arrival {
    let handler: Arc<dyn Handler> = Arc::new(FakeHandler::new(1));
    let cloud = ImmediateCloud::new(|_n, _req| (204, None)).await;
    let mut rx = cloud.take_receiver();
    let mut cfg = Config::new(cloud.base_url.clone(), TEST_TOKEN, "runner-1", handler);
    cfg.user_agent = user_agent.map(str::to_string);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let done = tokio::spawn(async move { run(cfg, cancel2).await });
    let first = wait_for(&mut rx, |a| a.n == 0).await;
    cancel.cancel();
    assert_eq!(done.await.expect("join"), Ok(()));
    first
}

#[tokio::test(flavor = "multi_thread")]
async fn user_agent_and_body_version_agree() {
    let a = first_arrival(None).await;
    assert_eq!(a.user_agent, format!("runner/{VERSION}"));
    assert_eq!(a.user_agent, USER_AGENT);
    assert_eq!(a.req.runner.version, VERSION);
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_user_agent_wins() {
    let a = first_arrival(Some("custom/0.2.1+64f6ef6a1b2c")).await;
    assert_eq!(a.user_agent, "custom/0.2.1+64f6ef6a1b2c");
}

/// `<digits 1-5>.<digits>.<digits>+` then 12 lowercase hex or `dev`.
fn is_build_id(s: &str) -> bool {
    let Some((ver, tail)) = s.split_once('+') else {
        return false;
    };
    let parts: Vec<&str> = ver.split('.').collect();
    let ver_ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    let tail_ok = tail == "dev"
        || (tail.len() == 12 && tail.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    ver_ok && tail_ok
}

#[test]
fn constants_match_the_contract_rules() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contract/log/lists.json"
    ))
    .expect("read lists.json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse lists.json");
    // The hand-written check below is only valid for these exact patterns.
    assert_eq!(
        v["build_rule"]["regex"],
        r"^[0-9]{1,5}\.[0-9]{1,5}\.[0-9]{1,5}\+([0-9a-f]{12}|dev)$"
    );
    // The client rule is `^(<names>)/` plus the build rule; `runner` must be a name.
    let client = v["client_rule"]["regex"].as_str().expect("client regex");
    let build = v["build_rule"]["regex"].as_str().expect("build regex");
    let tail = build.strip_prefix('^').expect("anchored build regex");
    let names = client
        .strip_prefix("^(")
        .and_then(|s| s.strip_suffix(&format!(")/{tail}")))
        .expect("client regex shape");
    assert!(names.split('|').any(|n| n == "runner"), "{client}");
    assert!(is_build_id(VERSION), "{VERSION}");
    let id = USER_AGENT.strip_prefix("runner/").expect("runner/ prefix");
    assert!(is_build_id(id), "{USER_AGENT}");
    assert!(USER_AGENT.len() <= 37);
    assert!(is_build_id("0.2.1+64f6ef6a1b2c") && !is_build_id("0.2.1+64F6ef6a1b2c"));
}

use crate::config::{Cloud, Config, Resource};
use crate::ops::tests::test_op;
use crate::ops::{Handler, NowFn, Options};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SID: &str = "sid-value-9f3a";

async fn handler(server: &MockServer, cookies: bool) -> (Handler, Arc<AtomicU64>) {
    let mut resources = HashMap::new();
    resources.insert(
        "web".to_string(),
        Resource {
            r#type: "http".to_string(),
            base_url: format!("{}/", server.uri()),
            cookies,
            ..Default::default()
        },
    );
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let c = clock.clone();
    let now: NowFn = Arc::new(move || UNIX_EPOCH + Duration::from_secs(c.load(Ordering::SeqCst)));
    let cfg = Config {
        cloud: Cloud::default(),
        evidence: "redacted".to_string(),
        resources,
        ..Default::default()
    };
    let opts = Options {
        now: Some(now),
        ..Default::default()
    };
    (Handler::new(cfg, opts).await.expect("handler"), clock)
}

async fn call(
    h: &Handler,
    run: &str,
    p: &str,
    headers: Option<(&str, &str)>,
) -> crate::contract::Result {
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String(p.to_string()));
    if let Some((k, v)) = headers {
        args.insert("headers".to_string(), serde_json::json!({ k: v }));
    }
    let project = vec![vec!["status".to_string()], vec!["body".to_string()]];
    let op = test_op("op", run, "http.request", "web", 2000, args, project);
    let (r, e) = h.handle(op).await;
    assert!(e.is_none(), "error: {e:?}");
    r.expect("result")
}

async fn cookie_of_last(server: &MockServer) -> Option<String> {
    let reqs = server.received_requests().await.expect("recorded");
    reqs.last()?
        .headers
        .get("cookie")
        .map(|v| v.to_str().unwrap_or("").to_string())
}

async fn mount_login(server: &MockServer, set_cookie: &str) {
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", set_cookie))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn cookie_sent_on_same_run_only() {
    let server = MockServer::start().await;
    mount_login(&server, &format!("sid={SID}; Path=/; HttpOnly")).await;
    let (h, _) = handler(&server, true).await;

    let login = call(&h, "run-a", "/login", None).await;
    assert!(!format!("{:?}", login.payload).contains(SID));
    call(&h, "run-a", "/me", None).await;
    assert_eq!(
        cookie_of_last(&server).await.as_deref(),
        Some(&*format!("sid={SID}"))
    );

    call(&h, "run-b", "/me", None).await;
    assert_eq!(cookie_of_last(&server).await, None);
}

#[tokio::test]
async fn step_cookie_header_wins() {
    let server = MockServer::start().await;
    mount_login(&server, &format!("sid={SID}; Path=/")).await;
    let (h, _) = handler(&server, true).await;
    call(&h, "run-a", "/login", None).await;
    call(&h, "run-a", "/me", Some(("Cookie", "own=1"))).await;
    assert_eq!(cookie_of_last(&server).await.as_deref(), Some("own=1"));
}

#[tokio::test]
async fn max_age_zero_and_past_expires_remove() {
    for gone in [
        "sid=; Path=/; Max-Age=0",
        "sid=x; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    ] {
        let server = MockServer::start().await;
        mount_login(&server, &format!("sid={SID}; Path=/")).await;
        Mock::given(method("GET"))
            .and(path("/logout"))
            .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", gone))
            .mount(&server)
            .await;
        let (h, _) = handler(&server, true).await;
        call(&h, "run-a", "/login", None).await;
        call(&h, "run-a", "/logout", None).await;
        call(&h, "run-a", "/me", None).await;
        assert_eq!(cookie_of_last(&server).await, None, "{gone}");
    }
}

#[tokio::test]
async fn cookie_set_on_redirect_hop_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("set-cookie", format!("hop={SID}; Path=/").as_str())
                .insert_header("location", "/me"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let (h, _) = handler(&server, true).await;
    call(&h, "run-a", "/start", None).await;
    // the redirected hop carried it
    assert_eq!(
        cookie_of_last(&server).await.as_deref(),
        Some(&*format!("hop={SID}"))
    );
    call(&h, "run-a", "/me", None).await;
    assert_eq!(
        cookie_of_last(&server).await.as_deref(),
        Some(&*format!("hop={SID}"))
    );
}

#[tokio::test]
async fn echoed_cookie_is_scrubbed() {
    let server = MockServer::start().await;
    mount_login(&server, &format!("sid={SID}; Path=/")).await;
    Mock::given(method("GET"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("you are {SID}")))
        .mount(&server)
        .await;
    let (h, _) = handler(&server, true).await;
    call(&h, "run-a", "/login", None).await;
    let r = call(&h, "run-a", "/echo", None).await;
    let text = format!("{:?}", r.payload);
    assert!(!text.contains(SID), "{text}");
    assert!(text.contains("[scrubbed]"));
    assert!(r.scrubbed >= 1);
}

#[tokio::test]
async fn idle_jar_is_evicted() {
    let server = MockServer::start().await;
    mount_login(&server, &format!("sid={SID}; Path=/")).await;
    let (h, clock) = handler(&server, true).await;
    call(&h, "run-a", "/login", None).await;
    clock.fetch_add(29 * 60, Ordering::SeqCst);
    call(&h, "run-a", "/me", None).await;
    assert!(cookie_of_last(&server).await.is_some());
    clock.fetch_add(31 * 60, Ordering::SeqCst);
    call(&h, "run-a", "/me", None).await;
    assert_eq!(cookie_of_last(&server).await, None);
}

#[tokio::test]
async fn resource_without_cookies_sends_none() {
    let server = MockServer::start().await;
    mount_login(&server, &format!("sid={SID}; Path=/")).await;
    let (h, _) = handler(&server, false).await;
    call(&h, "run-a", "/login", None).await;
    call(&h, "run-a", "/me", None).await;
    assert_eq!(cookie_of_last(&server).await, None);
}

async fn call_redirect(
    h: &Handler,
    run: &str,
    p: &str,
    redirect: Option<&str>,
    project: Vec<Vec<String>>,
) -> (
    Option<crate::contract::Result>,
    Option<crate::contract::Error>,
) {
    let mut args = HashMap::new();
    args.insert("path".to_string(), Value::String(p.to_string()));
    if let Some(r) = redirect {
        args.insert("redirect".to_string(), Value::String(r.to_string()));
    }
    let op = test_op("op", run, "http.request", "web", 2000, args, project);
    h.handle(op).await
}

async fn mount_redirect(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/go"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/me")
                .insert_header("set-cookie", format!("sid={SID}; Path=/; HttpOnly")),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn redirect_none_returns_3xx_and_stores_cookie() {
    let server = MockServer::start().await;
    mount_redirect(&server).await;
    let (h, _) = handler(&server, true).await;
    let project = vec![
        vec!["status".to_string()],
        vec!["headers".to_string(), "location".to_string()],
    ];
    let (r, e) = call_redirect(&h, "r1", "/go", Some("none"), project).await;
    assert!(e.is_none(), "error: {e:?}");
    let r = r.expect("result");
    assert_eq!(r.payload["status"], 302);
    assert_eq!(r.payload["headers"]["location"], "/me");
    let reqs = server.received_requests().await.expect("recorded");
    assert_eq!(reqs.len(), 1);
    call(&h, "r1", "/me", None).await;
    assert_eq!(
        cookie_of_last(&server).await.as_deref(),
        Some(format!("sid={SID}").as_str())
    );
}

#[tokio::test]
async fn redirect_none_200_unchanged_and_follow_follows() {
    let server = MockServer::start().await;
    mount_redirect(&server).await;
    let (h, _) = handler(&server, true).await;
    let project = vec![vec!["status".to_string()], vec!["body".to_string()]];
    let (r, e) = call_redirect(&h, "r2", "/me", Some("none"), project.clone()).await;
    assert!(e.is_none());
    let r = r.expect("result");
    assert_eq!(r.payload["status"], 200);
    assert_eq!(r.payload["body"], "hello");
    for mode in [None, Some("follow")] {
        let (r, e) = call_redirect(&h, "r3", "/go", mode, project.clone()).await;
        assert!(e.is_none());
        let r = r.expect("result");
        assert_eq!(r.payload["status"], 200);
        assert_eq!(r.payload["body"], "hello");
    }
}

#[tokio::test]
async fn redirect_bad_value_refused() {
    let server = MockServer::start().await;
    mount_redirect(&server).await;
    let (h, _) = handler(&server, false).await;
    for bad in ["manual", ""] {
        let (r, e) = call_redirect(&h, "r4", "/go", Some(bad), vec![]).await;
        assert!(r.is_none());
        assert_eq!(e.expect("error").reason, "runner-error");
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
}

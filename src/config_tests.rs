use super::*;
use std::collections::HashMap as StdHashMap;

fn lookup(env: StdHashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
    move |name: &str| env.get(name).map(|v| v.to_string())
}

fn write_config(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join("runner-config.json");
    std::fs::write(&path, body).expect("write config");
    path
}

const GOOD_CONFIG: &str = r#"{
  "cloud": {"url": "https://cloud.zriz.io", "token-env": "ZRIZ_TOKEN"},
  "evidence": "redacted",
  "resources": {
    "shop-api": {"type": "http", "base-url": "http://shop:9080/api"},
    "shop-db": {"type": "sql", "connection": "shop:${MYSQL_PASSWORD}@tcp(shop-mysql:3306)/shop", "read-only": true}
  }
}"#;

#[test]
fn missing_env_var_named_in_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, GOOD_CONFIG);
    let err = load(&path, &lookup(StdHashMap::new())).unwrap_err();
    assert!(err.to_string().contains("MYSQL_PASSWORD"), "err = {err}");
}

#[test]
fn good_config_loads_both_resources() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, GOOD_CONFIG);
    let mut env = StdHashMap::new();
    env.insert("MYSQL_PASSWORD", "hunter2");
    let cfg = load(&path, &lookup(env)).expect("load");
    assert_eq!(cfg.cloud.url, "https://cloud.zriz.io");
    assert_eq!(cfg.cloud.token_env, "ZRIZ_TOKEN");
    assert_eq!(cfg.resources.len(), 2);
    let db = &cfg.resources["shop-db"];
    assert_eq!(db.connection, "shop:hunter2@tcp(shop-mysql:3306)/shop");
    let api = &cfg.resources["shop-api"];
    assert_eq!(api.base_url, "http://shop:9080/api");
}

#[test]
fn unknown_resource_type() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = r#"{"cloud":{"url":"https://c","token-env":"T"},"resources":{"r":{"type":"ftp"}}}"#;
    let path = write_config(&dir, body);
    assert!(load(&path, &lookup(StdHashMap::new())).is_err());
}

#[test]
fn http_resource_without_scheme() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = r#"{"cloud":{"url":"https://c","token-env":"T"},"resources":{"r":{"type":"http","base-url":"shop:9080/api"}}}"#;
    let path = write_config(&dir, body);
    assert!(load(&path, &lookup(StdHashMap::new())).is_err());
}

#[test]
fn missing_cloud_url() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = r#"{"cloud":{"token-env":"T"},"resources":{}}"#;
    let path = write_config(&dir, body);
    assert!(load(&path, &lookup(StdHashMap::new())).is_err());
}

#[test]
fn missing_cloud_token_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = r#"{"cloud":{"url":"https://c"},"resources":{}}"#;
    let path = write_config(&dir, body);
    assert!(load(&path, &lookup(StdHashMap::new())).is_err());
}

#[test]
fn secrets_listing_the_token_env_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = r#"{"cloud":{"url":"https://c","token-env":"RUNNER_TOK"},"resources":{"r":{"type":"http","base-url":"http://h:1","secrets":["A","RUNNER_TOK"]}}}"#;
    let path = write_config(&dir, body);
    let err = load(&path, &lookup(StdHashMap::new())).unwrap_err();
    assert!(err.to_string().contains("RUNNER_TOK"), "err = {err}");
    assert!(err.to_string().contains("resource r"), "err = {err}");
}

fn cloud_cfg(url: &str) -> String {
    format!(r#"{{"cloud":{{"url":"{url}","token-env":"T"}},"resources":{{}}}}"#)
}

#[test]
fn cloud_url_scheme_rules() {
    let cases: &[(&str, bool, bool)] = &[
        ("http://example.com", false, false),
        ("https://example.com", true, false),
        ("http://localhost:8080", true, false),
        ("http://127.0.0.1:8080", true, false),
        ("http://[::1]:8080", true, false),
        ("http://host.docker.internal:8080", false, false),
        ("http://host.docker.internal:8080", true, true),
        ("ftp://example.com", false, true),
    ];
    for (url, ok, insecure) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(&dir, &cloud_cfg(url));
        let mut env = StdHashMap::new();
        if *insecure {
            env.insert("ZRIZ_RUNNER_INSECURE_CLOUD", "1");
        }
        let got = load(&path, &lookup(env));
        assert_eq!(got.is_ok(), *ok, "{url} insecure={insecure}: {got:?}");
        if let Err(e) = got {
            let m = e.to_string();
            assert!(m.contains("ZRIZ_RUNNER_INSECURE_CLOUD"), "{m}");
        }
    }
}

#[test]
fn cloud_url_error_hides_userinfo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(&dir, &cloud_cfg("http://user:pw123@example.com"));
    let err = load(&path, &lookup(StdHashMap::new())).unwrap_err();
    assert!(!err.to_string().contains("pw123"), "{err}");
}

#[test]
fn example_config_loads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.json");
    let mut env = StdHashMap::new();
    env.insert("SHOP_DB_PASSWORD", "pw");
    let cfg = load(&path, &lookup(env)).expect("load example");
    assert_eq!(cfg.worker_socket, "/run/zriz/worker.sock");
    let types: std::collections::BTreeSet<&str> =
        cfg.resources.values().map(|r| r.r#type.as_str()).collect();
    assert_eq!(
        types.into_iter().collect::<Vec<_>>(),
        ["browser", "cli", "http", "sql"]
    );
    assert!(cfg.resources["shop-db"].read_only);
}

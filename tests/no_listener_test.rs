//! "The runner only connects out; it opens no port." The non-test source
//! under `src/` must hold no listener, and `Cargo.toml` no server crate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Constructs that open a listening socket or start a server.
const LISTENERS: &[&str] = &[
    "TcpListener",
    "UnixListener",
    "UdpSocket",
    "UnixDatagram",
    "TcpSocket",
    ".bind(",
    ".listen(",
    "axum",
    "warp::",
    "tiny_http",
    "actix",
    "hyper::server",
    "hyper_util::server",
    "tonic::transport::Server",
];

/// Crates that exist to serve; none may be a dependency.
const SERVER_CRATES: &[&str] = &[
    "axum",
    "warp",
    "tiny_http",
    "actix-web",
    "rocket",
    "hyper",
    "tonic",
    "poem",
    "salvo",
];

fn rs_files(dir: &str) -> Vec<String> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let s = p.to_str().unwrap().to_string();
        if p.is_dir() {
            out.extend(rs_files(&s));
        } else if s.ends_with(".rs") && !s.ends_with("_tests.rs") && !s.ends_with("test_support.rs")
        {
            out.push(s);
        }
    }
    out
}

/// The non-test part of a file: everything before the first top-level `#[cfg(test)]`.
fn non_test(text: &str) -> &str {
    let at = text.find("\n#[cfg(test)]").map_or(text.len(), |i| i + 1);
    &text[..at]
}

fn hits(text: &str) -> Vec<&'static str> {
    let code = non_test(text);
    LISTENERS
        .iter()
        .copied()
        .filter(|p| code.contains(p))
        .collect()
}

#[test]
fn the_scan_sees_a_listener_and_skips_test_code() {
    let cases: &[(&str, &[&str])] = &[
        (
            "let l = tokio::net::TcpListener::bind(a).await;",
            &["TcpListener"],
        ),
        ("let s = sock.bind(addr);", &[".bind("]),
        ("use axum::Router;", &["axum"]),
        (
            "fn f() {}\n#[cfg(test)]\nmod t { use tokio::net::TcpListener; }",
            &[],
        ),
        ("let c = TcpStream::connect(a).await;", &[]),
    ];
    for (text, want) in cases {
        assert_eq!(&hits(text), want, "{text}");
    }
}

#[test]
fn the_runner_only_connects_out_and_opens_no_port() {
    let dir = format!("{}/src", env!("CARGO_MANIFEST_DIR"));
    let files = rs_files(&dir);
    assert!(
        files.len() > 20,
        "found {} files; the scan is broken",
        files.len()
    );
    for f in &files {
        let found = hits(&std::fs::read_to_string(f).unwrap());
        assert!(
            found.is_empty(),
            "{f} holds a listener construct: {found:?}"
        );
    }
}

#[test]
fn cargo_toml_has_no_server_crate() {
    let toml =
        std::fs::read_to_string(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut in_deps = false;
    let mut seen = 0;
    for line in toml.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_deps = l.contains("dependencies");
        } else if in_deps && l.contains('=') {
            let name = l.split('=').next().unwrap().trim();
            seen += 1;
            assert!(!SERVER_CRATES.contains(&name), "server crate: {name}");
        }
    }
    assert!(seen >= 15, "read {seen} dependencies; the parse is broken");
}

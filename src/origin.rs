//! Resolves an op's request path against a resource's base URL and refuses
//! anything that would leave the configured host.

use url::Url;

/// Errors resolving or checking a URL's origin against a resource's base.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OriginError {
    /// `path` is not path-absolute (or is protocol-relative).
    #[error("host not allowed: path {0:?} is not path-absolute")]
    NotPathAbsolute(String),
    /// `base_url` did not parse as a URL.
    #[error("host not allowed: invalid base url: {0}")]
    InvalidBaseUrl(String),
    /// The resolved origin differs from the base's origin.
    #[error("host not allowed: {0} is not {1}")]
    Mismatch(String, String),
}

/// A request's origin: scheme, host and port, filling in the scheme's
/// default port when the URL has none explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOrigin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

/// Returns `u`'s origin, filling in the scheme's default port when `u` has
/// none explicit.
pub fn of(u: &Url) -> ResolvedOrigin {
    ResolvedOrigin {
        scheme: u.scheme().to_string(),
        host: u.host_str().unwrap_or("").to_string(),
        port: port_of(u),
    }
}

fn port_of(u: &Url) -> u16 {
    if let Some(p) = u.port() {
        return p;
    }
    match u.scheme() {
        "http" => 80,
        "https" => 443,
        _ => 0,
    }
}

/// Resolves `path` against `base_url` and returns the resolved URL, or
/// [`OriginError`] if `path` is not path-absolute or the resolved origin
/// differs from `base_url`'s.
pub fn check(base_url: &str, path: &str) -> std::result::Result<Url, OriginError> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(OriginError::NotPathAbsolute(path.to_string()));
    }

    let mut base = Url::parse(base_url).map_err(|e| OriginError::InvalidBaseUrl(e.to_string()))?;
    if !base.path().ends_with('/') {
        let new_path = format!("{}/", base.path());
        base.set_path(&new_path);
    }

    // Join by parsing "." + path as a reference: joining a
    // relative reference "." + path against a base whose path ends in "/"
    // stays within that base directory.
    let rel = format!(".{path}");
    let resolved = base
        .join(&rel)
        .map_err(|e| OriginError::InvalidBaseUrl(e.to_string()))?;

    if of(&resolved) != of(&base) {
        return Err(OriginError::Mismatch(
            resolved.to_string(),
            base.to_string(),
        ));
    }
    Ok(resolved)
}

/// Returns [`OriginError::Mismatch`] if `target`'s origin differs from
/// `base_url`'s. Unlike [`check`], `target` is already a full URL — e.g. an
/// HTTP redirect's destination — rather than a path to resolve against the
/// base. Userinfo on either URL is ignored: only scheme, host and port
/// decide the origin.
pub fn check_origin(base_url: &str, target: &Url) -> std::result::Result<(), OriginError> {
    let base = Url::parse(base_url).map_err(|e| OriginError::InvalidBaseUrl(e.to_string()))?;
    if of(target) != of(&base) {
        return Err(OriginError::Mismatch(target.to_string(), base.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_table() {
        struct Case {
            name: &'static str,
            base: &'static str,
            path: &'static str,
            want_err: bool,
            want_url: &'static str,
        }
        let cases = vec![
            Case {
                name: "leading dot host",
                base: "http://shop:9080/api",
                path: ".evil.example/x",
                want_err: true,
                want_url: "",
            },
            Case {
                name: "protocol relative",
                base: "http://shop:9080/api",
                path: "//evil.example/x",
                want_err: true,
                want_url: "",
            },
            Case {
                name: "absolute url as path",
                base: "http://shop:9080/api",
                path: "http://evil.example/x",
                want_err: true,
                want_url: "",
            },
            Case {
                name: "query preserved",
                base: "http://shop:9080/api",
                path: "/health?x=1",
                want_err: false,
                want_url: "http://shop:9080/api/health?x=1",
            },
            Case {
                name: "base with trailing slash",
                base: "http://shop:9080/api/",
                path: "/orders/42",
                want_err: false,
                want_url: "http://shop:9080/api/orders/42",
            },
        ];

        for c in cases {
            let got = check(c.base, c.path);
            if c.want_err {
                assert!(got.is_err(), "case {}: want err", c.name);
                continue;
            }
            let got = got.unwrap_or_else(|e| panic!("case {}: unexpected error: {e}", c.name));
            assert_eq!(got.as_str(), c.want_url, "case {}", c.name);
        }
    }

    #[test]
    fn of_table() {
        struct Case {
            name: &'static str,
            raw: &'static str,
            want: ResolvedOrigin,
        }
        let cases = vec![
            Case {
                name: "https default port",
                raw: "https://a.example/x",
                want: ResolvedOrigin {
                    scheme: "https".into(),
                    host: "a.example".into(),
                    port: 443,
                },
            },
            Case {
                name: "http explicit port",
                raw: "http://a.example:8080",
                want: ResolvedOrigin {
                    scheme: "http".into(),
                    host: "a.example".into(),
                    port: 8080,
                },
            },
            Case {
                name: "ipv6 host",
                raw: "http://[::1]:9080/api",
                want: ResolvedOrigin {
                    scheme: "http".into(),
                    host: "[::1]".into(),
                    port: 9080,
                },
            },
            Case {
                name: "userinfo ignored",
                raw: "http://user:pw@a.example/x",
                want: ResolvedOrigin {
                    scheme: "http".into(),
                    host: "a.example".into(),
                    port: 80,
                },
            },
        ];
        for c in cases {
            let u = Url::parse(c.raw).unwrap_or_else(|e| panic!("parse {}: {e}", c.raw));
            let got = of(&u);
            assert_eq!(got, c.want, "case {}", c.name);
        }
    }

    #[test]
    fn check_origin_table() {
        struct Case {
            name: &'static str,
            base: &'static str,
            target: &'static str,
            want_err: bool,
        }
        let cases = vec![
            Case {
                name: "same origin",
                base: "http://shop:9080/api",
                target: "http://shop:9080/api/orders",
                want_err: false,
            },
            Case {
                name: "different host",
                base: "http://shop:9080/api",
                target: "http://evil.example:9080/api",
                want_err: true,
            },
            Case {
                name: "different port same host",
                base: "http://shop:9080/api",
                target: "http://shop:9999/api",
                want_err: true,
            },
            Case {
                name: "different scheme",
                base: "http://shop:9080/api",
                target: "https://shop:9080/api",
                want_err: true,
            },
            Case {
                name: "ipv6 base same origin",
                base: "http://[::1]:9080/api",
                target: "http://[::1]:9080/api/x",
                want_err: false,
            },
            Case {
                name: "ipv6 base different port",
                base: "http://[::1]:9080/api",
                target: "http://[::1]:9999/api/x",
                want_err: true,
            },
            Case {
                name: "userinfo base same origin ignores credentials",
                base: "http://user:pw@shop:9080/api",
                target: "http://shop:9080/api/x",
                want_err: false,
            },
            Case {
                name: "userinfo base different host still refused",
                base: "http://user:pw@shop:9080/api",
                target: "http://evil.example:9080/api",
                want_err: true,
            },
        ];
        for c in cases {
            let target = Url::parse(c.target).unwrap_or_else(|e| panic!("parse target: {e}"));
            let got = check_origin(c.base, &target);
            assert_eq!(got.is_err(), c.want_err, "case {}", c.name);
        }
    }
}

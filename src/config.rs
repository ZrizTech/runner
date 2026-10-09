//! Loads and validates the runner's JSON configuration file.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Where the runner polls for work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cloud {
    /// The cloud's exchange endpoint base URL.
    pub url: String,
    /// The name of the environment variable holding the runner's token.
    pub token_env: String,
}

/// Describes one HTTP or SQL target the runner may act against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resource {
    /// "http" or "sql".
    pub r#type: String,
    /// The resource's base URL, for an http resource.
    pub base_url: String,
    /// The resource's driver DSN, for a sql resource.
    pub connection: String,
    /// Whether raw SQL against this resource must be read-only.
    pub read_only: bool,
    /// Whether an http resource keeps a cookie jar per run.
    pub cookies: bool,
    /// Extra allowed origins, for a browser resource.
    pub origins: Vec<String>,
    /// Runner env names an op may substitute (`${NAME}`). `None` or empty =
    /// none, for every kind. sql takes no placeholders at all. Listing the
    /// runner token env name is a config error.
    pub secrets: Option<Vec<String>>,
    /// Browser: most live contexts in the worker.
    pub max_contexts: Option<u32>,
    /// Browser: idle time before a context is dropped.
    pub idle_ms: Option<u64>,
    /// Browser: default viewport (width, height).
    pub viewport: Option<(u32, u32)>,
    /// Cli: the declared commands, by name.
    pub commands: HashMap<String, crate::config_cli::CliCommand>,
    /// Cli: most live background handles per run.
    pub max_handles: Option<u32>,
}

/// The runner's parsed and validated configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub cloud: Cloud,
    pub evidence: String,
    pub resources: HashMap<String, Resource>,
    /// Unix socket of the worker sidecar; empty when there is none.
    pub worker_socket: String,
}

/// Errors loading or validating a runner config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config: read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("config: parse {0}: {1}")]
    Parse(String, serde_json::Error),
    #[error("missing environment variable {0} referenced in runner config")]
    MissingEnv(String),
    #[error("config: missing cloud.url")]
    MissingCloudUrl,
    #[error(
        "config: cloud.url must be https (http is allowed only for localhost, 127.0.0.1 or ::1, or when ZRIZ_RUNNER_INSECURE_CLOUD=1)"
    )]
    InsecureCloudUrl,
    #[error("config: missing cloud.token-env")]
    MissingCloudTokenEnv,
    #[error("config: resource {0}: base-url must have a scheme and host, got {1:?}")]
    BadBaseUrl(String, String),
    #[error("config: resource {0}: unknown type {1:?}")]
    UnknownType(String, String),
    #[error("config: resource {0}: type browser needs a top-level worker.socket")]
    BrowserNeedsWorker(String),
    #[error("config: resource {0}: bad browser setting: {1}")]
    BadBrowser(String, String),
    #[error("config: resource {0}: type cli needs a top-level worker.socket")]
    CliNeedsWorker(String),
    #[error("config: resource {0}: bad cli setting: {1}")]
    BadCli(String, String),
    #[error("config: resource {0}: secrets must not list the runner token env {1}")]
    TokenInSecrets(String, String),
}

#[derive(Deserialize, Default)]
struct RawCloud {
    #[serde(default)]
    url: String,
    #[serde(default, rename = "token-env")]
    token_env: String,
}

#[derive(Deserialize, Default)]
struct RawResource {
    #[serde(default)]
    r#type: String,
    #[serde(default, rename = "base-url")]
    base_url: String,
    #[serde(default)]
    connection: String,
    #[serde(default, rename = "read-only")]
    read_only: bool,
    #[serde(default)]
    cookies: bool,
    #[serde(default)]
    origins: Vec<String>,
    #[serde(default)]
    secrets: Option<Vec<String>>,
    #[serde(default, rename = "max-contexts")]
    max_contexts: Option<u32>,
    #[serde(default, rename = "idle-ms")]
    idle_ms: Option<u64>,
    #[serde(default)]
    viewport: Option<RawViewport>,
    #[serde(default)]
    commands: HashMap<String, crate::config_cli::RawCliCommand>,
    #[serde(default, rename = "max-handles")]
    max_handles: Option<u32>,
}

#[derive(Deserialize)]
struct RawViewport {
    width: u32,
    height: u32,
}

#[derive(Deserialize, Default)]
struct RawWorker {
    #[serde(default)]
    socket: String,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    worker: RawWorker,
    #[serde(default)]
    cloud: RawCloud,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    resources: HashMap<String, RawResource>,
}

/// Reads the config file at `path`, substitutes `${NAME}` placeholders
/// using `lookup`, and validates the result. `lookup` is injected so tests
/// can supply a map instead of the real process environment.
pub fn load(
    path: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Config, ConfigError> {
    let data = std::fs::read(path).map_err(|e| ConfigError::Read(path.display().to_string(), e))?;

    let raw: RawConfig = serde_json::from_slice(&data)
        .map_err(|e| ConfigError::Parse(path.display().to_string(), e))?;

    let cfg = substitute_config(raw, lookup)?;
    validate(&cfg)?;
    check_cloud_scheme(
        &cfg.cloud.url,
        lookup("ZRIZ_RUNNER_INSECURE_CLOUD").as_deref() == Some("1"),
    )?;
    Ok(cfg)
}

fn substitute_config(
    raw: RawConfig,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Config, ConfigError> {
    let cloud_url = substitute(&raw.cloud.url, lookup)?;
    let token_env = substitute(&raw.cloud.token_env, lookup)?;

    let mut resources = HashMap::with_capacity(raw.resources.len());
    for (id, r) in raw.resources {
        resources.insert(id, substitute_resource(r, lookup)?);
    }

    Ok(Config {
        cloud: Cloud {
            url: cloud_url,
            token_env,
        },
        evidence: raw.evidence,
        resources,
        worker_socket: substitute(&raw.worker.socket, lookup)?,
    })
}

fn substitute_resource(
    r: RawResource,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Resource, ConfigError> {
    let base_url = substitute(&r.base_url, lookup)?;
    let connection = substitute(&r.connection, lookup)?;
    let mut origins = Vec::with_capacity(r.origins.len());
    for o in &r.origins {
        origins.push(substitute(o, lookup)?);
    }
    Ok(Resource {
        origins,
        secrets: r.secrets,
        max_contexts: r.max_contexts,
        idle_ms: r.idle_ms,
        viewport: r.viewport.map(|v| (v.width, v.height)),
        commands: r.commands.into_iter().map(|(k, c)| (k, c.into())).collect(),
        max_handles: r.max_handles,
        r#type: r.r#type,
        base_url,
        connection,
        read_only: r.read_only,
        cookies: r.cookies,
    })
}

/// Replaces every `${NAME}` placeholder in `s` with `lookup(NAME)`, or
/// fails naming the first unresolved key.
fn substitute(
    s: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, ConfigError> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(name_len) = placeholder_at(s, i) {
            let name = &s[i + 2..i + 2 + name_len];
            match lookup(name) {
                Some(val) => out.push_str(&val),
                None => return Err(ConfigError::MissingEnv(name.to_string())),
            }
            i += 2 + name_len + 1; // "${" + name + "}"
        } else {
            let ch = s[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// If `s[i..]` starts a `${NAME}` placeholder (name matching
/// `[A-Za-z_][A-Za-z0-9_]*`), returns the byte length of NAME.
fn placeholder_at(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'$') || bytes.get(i + 1) != Some(&b'{') {
        return None;
    }
    let name_start = i + 2;
    let mut j = name_start;
    let first = *bytes.get(j)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    j += 1;
    while let Some(&b) = bytes.get(j) {
        if b.is_ascii_alphanumeric() || b == b'_' {
            j += 1;
        } else {
            break;
        }
    }
    if bytes.get(j) == Some(&b'}') {
        Some(j - name_start)
    } else {
        None
    }
}

/// The runner token travels to `cloud.url`, so it must be https. Plain http
/// is allowed for a loopback host, or when the insecure escape is set.
fn check_cloud_scheme(raw: &str, insecure_ok: bool) -> std::result::Result<(), ConfigError> {
    let Ok(u) = url::Url::parse(raw) else {
        return Err(ConfigError::InsecureCloudUrl);
    };
    match u.scheme() {
        "https" => Ok(()),
        "http" if insecure_ok => Ok(()),
        "http"
            if matches!(
                u.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            ) =>
        {
            Ok(())
        }
        _ => Err(ConfigError::InsecureCloudUrl),
    }
}

fn validate(cfg: &Config) -> std::result::Result<(), ConfigError> {
    if cfg.cloud.url.is_empty() {
        return Err(ConfigError::MissingCloudUrl);
    }
    if cfg.cloud.token_env.is_empty() {
        return Err(ConfigError::MissingCloudTokenEnv);
    }
    for (id, r) in &cfg.resources {
        validate_resource(id, r)?;
        if r.secrets
            .as_ref()
            .is_some_and(|l| l.contains(&cfg.cloud.token_env))
        {
            return Err(ConfigError::TokenInSecrets(
                id.clone(),
                cfg.cloud.token_env.clone(),
            ));
        }
        if r.r#type == "browser" && cfg.worker_socket.is_empty() {
            return Err(ConfigError::BrowserNeedsWorker(id.clone()));
        }
        if r.r#type == "cli" && cfg.worker_socket.is_empty() {
            return Err(ConfigError::CliNeedsWorker(id.clone()));
        }
    }
    Ok(())
}

fn validate_resource(id: &str, r: &Resource) -> std::result::Result<(), ConfigError> {
    match r.r#type.as_str() {
        "http" => match url::Url::parse(&r.base_url) {
            Ok(u) if u.host_str().is_some() && !u.scheme().is_empty() => Ok(()),
            _ => Err(ConfigError::BadBaseUrl(id.to_string(), r.base_url.clone())),
        },
        "browser" => validate_browser(id, r),
        "cli" => crate::config_cli::validate(id, r),
        "sql" => Ok(()), // Connection is a driver DSN; no further structural check here.
        other => Err(ConfigError::UnknownType(id.to_string(), other.to_string())),
    }?;
    Ok(())
}

/// A browser resource needs a bare-origin base URL and bare-origin extras.
fn validate_browser(id: &str, r: &Resource) -> std::result::Result<(), ConfigError> {
    let bad = |what: &str| ConfigError::BadBrowser(id.to_string(), what.to_string());
    let bare = |s: &str| {
        url::Url::parse(s).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && matches!(u.path(), "" | "/")
                && u.query().is_none()
                && u.fragment().is_none()
        })
    };
    if !bare(&r.base_url) {
        return Err(ConfigError::BadBaseUrl(id.to_string(), r.base_url.clone()));
    }
    if r.origins.len() > 15 || r.origins.iter().any(|o| !bare(o)) {
        return Err(bad("origins must be at most 15 bare http(s) origins"));
    }
    // The ranges of the worker (`worker-contract/worker-request.json`).
    if r.max_contexts.is_some_and(|n| !(1..=16).contains(&n)) {
        return Err(bad("max-contexts must be 1..16"));
    }
    if r.idle_ms.is_some_and(|n| !(1000..=3_600_000).contains(&n)) {
        return Err(bad("idle-ms must be 1000..3600000"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

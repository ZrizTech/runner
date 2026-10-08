//! The `cli` resource type of the runner config: declared commands with an
//! absolute binary path, an argv shape, fixed env and limits. Validated at
//! startup so a bad entry stops the runner instead of failing an op.

use crate::argshape;
use crate::config::{ConfigError, Resource};
use serde::Deserialize;
use std::collections::BTreeMap;

/// One declared command of a cli resource.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliCommand {
    /// Absolute path of the binary inside the worker image.
    pub path: String,
    /// Always the first argv tokens.
    pub argv_prefix: Vec<String>,
    /// Allowed argv tails: literals and `{name:class}` slots.
    pub shapes: Vec<Vec<String>>,
    /// Fixed child env.
    pub env: BTreeMap<String, String>,
    /// Env names an op may set (values may hold `${NAME}`).
    pub env_allow: Vec<String>,
    /// Default wait for run/wait/read.
    pub timeout_ms: u64,
    /// Hard kill for a background process.
    pub max_life_ms: u64,
    /// Per-stream output cap.
    pub max_output_bytes: u64,
}

#[derive(Deserialize, Default)]
pub struct RawCliCommand {
    #[serde(default)]
    path: String,
    #[serde(default, rename = "argv-prefix")]
    argv_prefix: Vec<String>,
    #[serde(default)]
    shapes: Vec<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default, rename = "env-allow")]
    env_allow: Vec<String>,
    #[serde(default, rename = "timeout-ms")]
    timeout_ms: u64,
    #[serde(default, rename = "max-life-ms")]
    max_life_ms: u64,
    #[serde(default, rename = "max-output-bytes")]
    max_output_bytes: u64,
}

impl From<RawCliCommand> for CliCommand {
    fn from(r: RawCliCommand) -> Self {
        Self {
            path: r.path,
            argv_prefix: r.argv_prefix,
            shapes: r.shapes,
            env: r.env,
            env_allow: r.env_allow,
            timeout_ms: r.timeout_ms,
            max_life_ms: r.max_life_ms,
            max_output_bytes: r.max_output_bytes,
        }
    }
}

fn name_ok(s: &str) -> bool {
    let mut it = s.bytes();
    it.next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && s.len() <= 64
        && it.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn cmd_name_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Checks one cli resource; the error names the resource and the setting,
/// never a value.
pub fn validate(id: &str, r: &Resource) -> Result<(), ConfigError> {
    let bad = |what: String| ConfigError::BadCli(id.to_string(), what);
    if r.commands.is_empty() {
        return Err(bad("commands is empty".into()));
    }
    if r.max_handles.is_some_and(|n| !(1..=8).contains(&n)) {
        return Err(bad("max-handles must be 1..8".into()));
    }
    if r.idle_ms.is_some_and(|n| !(1..=3_600_000).contains(&n)) {
        return Err(bad("idle-ms must be 1..3600000".into()));
    }
    let mut names: Vec<_> = r.commands.iter().collect();
    names.sort_by_key(|(k, _)| k.as_str());
    for (name, c) in names {
        let ctx = |w: &str| bad(format!("command {name}: {w}"));
        if !cmd_name_ok(name) {
            return Err(bad(format!(
                "command name {name:?} is not [A-Za-z0-9_-]{{1,64}}"
            )));
        }
        if !c.path.starts_with('/') || c.path.len() > 1024 || c.path.split('/').any(|s| s == "..") {
            return Err(ctx("path must be absolute, without .."));
        }
        if c.argv_prefix.len() > 16 {
            return Err(ctx("argv-prefix has more than 16 tokens"));
        }
        if c.shapes.is_empty() || !c.shapes.iter().all(|s| argshape::shape_valid(s)) {
            return Err(ctx(
                "shapes must be non-empty lists of literals or {name:class}",
            ));
        }
        if c.shapes.iter().any(|s| s.len() > 32) {
            return Err(ctx("a shape has more than 32 tokens"));
        }
        if c.env.len() > 32 || !c.env.keys().all(|k| name_ok(k)) {
            return Err(ctx("env: at most 32 valid names"));
        }
        if c.env_allow.len() > 16 || !c.env_allow.iter().all(|k| name_ok(k)) {
            return Err(ctx("env-allow: at most 16 valid names"));
        }
        if c.env_allow
            .iter()
            .any(|k| c.env.contains_key(k) || k == "HOME")
        {
            return Err(ctx("env-allow repeats a fixed env name or HOME"));
        }
        if !(1..=600_000).contains(&c.timeout_ms) {
            return Err(ctx("timeout-ms must be 1..600000"));
        }
        if !(1..=86_400_000).contains(&c.max_life_ms) {
            return Err(ctx("max-life-ms must be 1..86400000"));
        }
        if !(1..=1_048_576).contains(&c.max_output_bytes) {
            return Err(ctx("max-output-bytes must be 1..1048576"));
        }
    }
    Ok(())
}

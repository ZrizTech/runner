//! Placeholder substitution and capture, shared by the op kinds.

use super::*;

impl Handler {
    /// Substitutes `${NAME}` placeholders into `args` for `kind`, mapping any
    /// [`PlaceholderError`] to the contract error an op should return.
    /// `allow` lists the runner env names that may be substituted; an empty
    /// list means none. The run's vault is always searched first and always
    /// allowed. The runner token name is never allowed.
    pub(super) fn substitute_args(
        &self,
        op: &contract::Op,
        kind: &str,
        allow: &[String],
    ) -> std::result::Result<(Map<String, Value>, Vec<String>), ErrorFrame> {
        let map: Map<String, Value> = op
            .args
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let sub = self.with_lookup(op, allow, |l| placeholder::substitute(kind, &map, l))?;
        Ok((sub.args, sub.secrets))
    }

    /// Runs `f` with a lookup over the vault, then the env names in
    /// `allow` (never the runner token name). A name in neither the vault nor `allow` is
    /// `placeholder-not-found`; a listed name with no value is `secret-not-set`. Adds every vault value of the run and
    /// every value named in any resource's `secrets` to the returned
    /// secrets, so they are scrubbed from every op.
    pub(super) fn with_lookup(
        &self,
        op: &contract::Op,
        allow: &[String],
        f: impl FnOnce(
            &dyn Fn(&str) -> Option<String>,
        ) -> std::result::Result<placeholder::Substituted, PlaceholderError>,
    ) -> std::result::Result<placeholder::Substituted, ErrorFrame> {
        let now = (self.now)();
        // The first name with no value: (name, the resource lists it).
        let missing: std::cell::RefCell<Option<(String, bool)>> = std::cell::RefCell::new(None);
        let lookup = |name: &str| {
            if let Some(v) = self.vault.get(&op.run_id, name, now) {
                return Some(v);
            }
            let listed = name != self.cfg.cloud.token_env && allow.iter().any(|n| n == name);
            let value = if listed { (self.lookup)(name) } else { None };
            if value.is_none() {
                *missing.borrow_mut() = Some((name.to_string(), listed));
            }
            value
        };
        let mut sub = f(&lookup).map_err(|e| match (&e, missing.borrow().as_ref()) {
            (PlaceholderError::UnknownPlaceholder(_), Some((n, true))) => {
                new_error(&op.op_id, "secret-not-set", json!({"name": n}))
            }
            // In neither the vault nor the listed secrets: the cloud decides
            // which of the two is true (7.2).
            (PlaceholderError::UnknownPlaceholder(_), Some((n, false))) => new_error(
                &op.op_id,
                "placeholder-not-found",
                json!({"name": n, "resource": op.resource}),
            ),
            _ => placeholder_substitute_error(&op.op_id, &e),
        })?;
        sub.secrets.extend(self.vault.values(&op.run_id, now));
        sub.secrets.extend(self.listed_secrets.iter().cloned());
        Ok(sub)
    }

    /// Captures per the op's `capture` arg from `source`: stores them in the
    /// vault and adds them to `secrets`. Returns the parsed captures so the
    /// caller can mask their paths. Nothing is stored when any path is missing.
    pub(super) fn apply_capture(
        &self,
        op: &contract::Op,
        source: &Map<String, Value>,
        secrets: &mut Vec<String>,
    ) -> std::result::Result<capture::Captures, ErrorFrame> {
        let caps = capture::parse(&op.args).map_err(|_| capture_error(&op.op_id, None))?;
        if caps.is_empty() {
            return Ok(caps);
        }
        let got = capture::extract(source, &caps).map_err(|e| match e {
            capture::CaptureError::Missing(n) => capture_error(&op.op_id, Some(&n)),
            capture::CaptureError::Invalid => capture_error(&op.op_id, None),
        })?;
        self.vault.put(&op.run_id, &got, (self.now)());
        secrets.extend(got.into_iter().map(|(_, v)| v).filter(|v| !v.is_empty()));
        Ok(caps)
    }
}

/// The values of every env name in any resource's `secrets` list, in name
/// order, deduped. Unset and empty values are skipped, and so is the runner
/// token name.
pub(super) fn listed_secret_values(cfg: &Config, lookup: &Lookup) -> Vec<String> {
    let mut names: Vec<&str> = cfg
        .resources
        .values()
        .flat_map(|r| r.secrets.iter().flatten())
        .map(String::as_str)
        .filter(|n| *n != cfg.cloud.token_env)
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut values: Vec<String> = Vec::new();
    for v in names.into_iter().filter_map(|n| lookup(n)) {
        if !v.is_empty() && !values.contains(&v) {
            values.push(v);
        }
    }
    values
}

#[cfg(test)]
#[path = "listed_secrets_tests.rs"]
mod listed_secrets_tests;

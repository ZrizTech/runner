//! Per-run cookie jars for http resources that set `"cookies": true`.
//! A jar is keyed by (run-id, resource name) and lives in memory only. It
//! stays until the run-end notice ([`Jars::forget_run`]) or the end of the
//! process; at most [`MAX_JARS`] jars exist (the longest idle goes first,
//! with one WARN `state dropped`). Cookie values never leave this module
//! except as scrub-list entries and the outgoing `Cookie` header.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) const MAX_JARS: usize = 1000;

pub(crate) type JarKey = (String, String);

struct Cookie {
    name: String,
    value: String,
    path: String,
}

struct Jar {
    last_used: SystemTime,
    cookies: Vec<Cookie>,
}

#[derive(Default)]
pub(crate) struct Jars {
    inner: Mutex<HashMap<JarKey, Jar>>,
}

impl Jars {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().map_or(0, |m| m.len())
    }

    /// Drops every jar of `run_id` (the run ended).
    pub(crate) fn forget_run(&self, run_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.retain(|(run, _), _| run != run_id);
        }
    }

    /// The `Cookie` header value for a request to `url`, or `None` when the
    /// jar has nothing for that path.
    pub(crate) fn cookie_header(
        &self,
        key: &JarKey,
        url: &url::Url,
        now: SystemTime,
    ) -> Option<String> {
        let mut map = self.inner.lock().ok()?;
        let jar = map.get_mut(key)?;
        jar.last_used = now;
        let parts: Vec<String> = jar
            .cookies
            .iter()
            .filter(|c| path_matches(url.path(), &c.path))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// Applies `Set-Cookie` lines to the jar and returns every cookie value
    /// involved (before and after), for the op's scrub list.
    pub(crate) fn store(
        &self,
        key: &JarKey,
        set_cookies: &[String],
        now: SystemTime,
    ) -> Vec<String> {
        let Ok(mut map) = self.inner.lock() else {
            return Vec::new();
        };
        if !map.contains_key(key) {
            if set_cookies.is_empty() {
                return Vec::new();
            }
            if map.len() >= MAX_JARS
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, j)| j.last_used)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
                tracing::warn!(target: "runner.ops", run_id = %oldest.0, kind = "jar", "state dropped");
            }
            map.insert(
                key.clone(),
                Jar {
                    last_used: now,
                    cookies: Vec::new(),
                },
            );
        }
        let Some(jar) = map.get_mut(key) else {
            return Vec::new();
        };
        jar.last_used = now;
        let mut values: Vec<String> = jar.cookies.iter().map(|c| c.value.clone()).collect();
        for raw in set_cookies {
            if let Some((c, delete)) = parse_set_cookie(raw, now) {
                values.push(c.value.clone());
                jar.cookies
                    .retain(|o| !(o.name == c.name && o.path == c.path));
                if !delete {
                    jar.cookies.push(c);
                }
            }
        }
        values.extend(jar.cookies.iter().map(|c| c.value.clone()));
        values.retain(|v| !v.is_empty());
        values
    }
}

fn path_matches(req: &str, cookie: &str) -> bool {
    req == cookie
        || (req.starts_with(cookie)
            && (cookie.ends_with('/') || req[cookie.len()..].starts_with('/')))
}

/// Returns the cookie and whether this line deletes it (`Max-Age` <= 0 or
/// an `Expires` in the past).
fn parse_set_cookie(raw: &str, now: SystemTime) -> Option<(Cookie, bool)> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut path = "/".to_string();
    let mut delete = false;
    let mut max_age_seen = false;
    for attr in parts {
        let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "path" if v.starts_with('/') => path = v.to_string(),
            "max-age" => {
                if let Ok(n) = v.parse::<i64>() {
                    max_age_seen = true;
                    delete = n <= 0;
                }
            }
            "expires" if !max_age_seen => {
                if let Some(t) = parse_http_date(v) {
                    delete = t <= now;
                }
            }
            _ => {}
        }
    }
    let cookie = Cookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        path,
    };
    Some((cookie, delete))
}

/// Parses `Thu, 01 Jan 1970 00:00:00 GMT`.
fn parse_http_date(s: &str) -> Option<SystemTime> {
    let s = s.split_once(',').map_or(s, |(_, r)| r).trim();
    let mut it = s.split_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?.to_ascii_lowercase();
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| mon.starts_with(m))? as i64
        + 1;
    let mut year: i64 = it.next()?.parse().ok()?;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    let mut hms = it.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    // days from civil (Howard Hinnant)
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + m * 60 + sec;
    match u64::try_from(secs) {
        Ok(s) => Some(UNIX_EPOCH + Duration::from_secs(s)),
        Err(_) => Some(UNIX_EPOCH),
    }
}

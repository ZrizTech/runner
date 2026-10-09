//! Implements the http.request op kind: resolve the resource and args,
//! build and send the request, then shape the response into a Result.
//!
//! `reqwest::Client`'s redirect policy is fixed at build time and cannot be
//! swapped per request, so there is no seam to inject an unpoliced client.
//! Instead, [`Handler`]'s one internal client is always built with
//! `redirect::Policy::none()`, and this module runs its own redirect loop,
//! checking [`crate::origin::check_origin`] before ever following a hop —
//! the same fail-closed guarantee, made structural rather than
//! per-request.

use super::{Handler, capture, duration_ms, new_error, runner_error, scrub_payload, string_value};
use crate::config::Resource;
use crate::contract::{self, Error as ErrorFrame, Result as ResultFrame, Timing};
use crate::{origin, project};
use serde_json::{Map, Value, json};

/// Bounds how much of a response body is read; a response larger than this
/// is refused rather than buffered in full.
const MAX_HTTP_BODY_BYTES: usize = 1 << 20;

/// The most redirect hops one request follows.
const MAX_REDIRECTS: usize = 10;

struct PendingRequest {
    method: reqwest::Method,
    url: url::Url,
    headers: reqwest::header::HeaderMap,
    body: Option<Vec<u8>>,
}

enum SendErrorKind {
    HostNotAllowed,
    TooManyRedirects,
    TooLarge,
    /// The connect to the target failed (refused, reset, DNS, TLS).
    Connect,
    Runner,
}

impl SendErrorKind {
    fn frame(&self, op_id: &str) -> ErrorFrame {
        match self {
            Self::HostNotAllowed => new_error(op_id, "host-not-allowed", json!({})),
            Self::TooLarge => runner_error(op_id, "http-response-size"),
            Self::TooManyRedirects => runner_error(op_id, "http-redirects"),
            Self::Connect => new_error(op_id, "connection-error", json!({})),
            Self::Runner => runner_error(op_id, "http-client"),
        }
    }
}

impl Handler {
    pub(crate) async fn http_request(
        &self,
        op: &contract::Op,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let resource = match self.resource(&op.op_id, &op.resource, "http") {
            Ok(r) => r,
            Err(e) => return (None, Some(e)),
        };
        let (args, secrets) = match self.substitute_args(
            op,
            "http.request",
            resource.secrets.as_deref().unwrap_or(&[]),
        ) {
            Ok(v) => v,
            Err(e) => return (None, Some(e)),
        };
        let target = match resolve_http_target(&op.op_id, &resource, &args) {
            Ok(t) => t,
            Err(e) => return (None, Some(e)),
        };
        let pending = match build_pending_request(&op.op_id, target, &args) {
            Ok(p) => p,
            Err(e) => return (None, Some(e)),
        };

        let follow = match args.get("redirect") {
            None => true,
            Some(Value::String(s)) if s == "follow" => true,
            Some(Value::String(s)) if s == "none" => false,
            Some(_) => {
                return (None, Some(runner_error(&op.op_id, "http-request")));
            }
        };

        let start = (self.now)();
        let jar_key = resource
            .cookies
            .then(|| (op.run_id.clone(), op.resource.clone()));
        let mut secrets = secrets;
        let sent = self
            .send_with_redirects(
                pending,
                &resource.base_url,
                jar_key.as_ref(),
                follow,
                &mut secrets,
            )
            .await;
        let exec_ms = duration_ms((self.now)(), start);

        match sent {
            Ok((status, content_type, headers, body)) => self.shape_http_result(
                op,
                (status, &content_type, headers),
                body,
                &secrets,
                exec_ms,
            ),
            Err(kind) => (None, Some(kind.frame(&op.op_id))),
        }
    }

    /// Sends `pending`, following same-origin redirects only, up to
    /// [`MAX_REDIRECTS`] hops. The client itself never follows a redirect
    /// (`redirect::Policy::none()`), so every hop is inspected here before
    /// being re-issued.
    async fn send_with_redirects(
        &self,
        mut pending: PendingRequest,
        base_url: &str,
        jar_key: Option<&(String, String)>,
        follow: bool,
        secrets: &mut Vec<String>,
    ) -> std::result::Result<(u16, String, Map<String, Value>, Vec<u8>), SendErrorKind> {
        for hop in 0..=MAX_REDIRECTS {
            let mut headers = pending.headers.clone();
            if let Some(key) = jar_key
                && !headers.contains_key(reqwest::header::COOKIE)
                && let Some(line) = self.jars.cookie_header(key, &pending.url, (self.now)())
                && let Ok(v) = reqwest::header::HeaderValue::from_str(&line)
            {
                headers.insert(reqwest::header::COOKIE, v);
            }
            let mut builder = self
                .http_client
                .request(pending.method.clone(), pending.url.clone())
                .headers(headers);
            if let Some(b) = &pending.body {
                builder = builder.body(b.clone());
            }
            let resp = builder.send().await.map_err(|e| {
                if e.is_connect() {
                    SendErrorKind::Connect
                } else {
                    SendErrorKind::Runner
                }
            })?;
            let status = resp.status();
            if let Some(key) = jar_key {
                let set: Vec<String> = resp
                    .headers()
                    .get_all(reqwest::header::SET_COOKIE)
                    .iter()
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                    .collect();
                secrets.extend(self.jars.store(key, &set, (self.now)()));
            }

            if !follow || !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let headers = response_headers(resp.headers());
                let body = read_capped_body(resp).await?;
                return Ok((status.as_u16(), content_type, headers, body));
            }
            if hop == MAX_REDIRECTS {
                return Err(SendErrorKind::TooManyRedirects);
            }

            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(SendErrorKind::Runner)?;
            let next_url = pending
                .url
                .join(location)
                .map_err(|_| SendErrorKind::Runner)?;
            origin::check_origin(base_url, &next_url).map_err(|_| SendErrorKind::HostNotAllowed)?;

            let (method, body) =
                downgrade_method(&pending.method, status.as_u16(), pending.body.take());
            pending = PendingRequest {
                method,
                url: next_url,
                headers: pending.headers,
                body,
            };
        }
        Err(SendErrorKind::TooManyRedirects)
    }

    fn shape_http_result(
        &self,
        op: &contract::Op,
        (status, content_type, headers): (u16, &str, Map<String, Value>),
        body: Vec<u8>,
        secrets: &[String],
        exec_ms: i64,
    ) -> (Option<ResultFrame>, Option<ErrorFrame>) {
        let decoded_body = decode_http_body(&body, content_type);
        let status_str = if status >= 400 { "fail" } else { "pass" };
        let mut secrets = secrets.to_vec();
        let secrets = &mut secrets;

        let mut source = Map::new();
        source.insert("status".to_string(), Value::from(status));
        source.insert("body".to_string(), decoded_body.clone());
        source.insert("headers".to_string(), Value::Object(headers));
        let caps = match self.apply_capture(op, &source, secrets) {
            Ok(c) => c,
            Err(e) => return (None, Some(e)),
        };

        self.evidence.put(
            &op.run_id,
            crate::evidence::Entry {
                op_id: op.op_id.clone(),
                status: i32::from(status),
                body: decoded_body.clone(),
                secrets: secrets.to_vec(),
            },
            (self.now)(),
        );

        let projected = project::select_paths(&source, &op.project);

        let mut payload = Map::new();
        payload.insert("status".to_string(), Value::from(status));
        payload.insert(
            "body".to_string(),
            projected.get("body").cloned().unwrap_or(Value::Null),
        );
        payload.insert(
            "headers".to_string(),
            projected
                .get("headers")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        );

        capture::mask(&mut payload, &caps);

        let (scrubbed, count) = match scrub_payload(&payload, secrets) {
            Ok(v) => v,
            Err(_) => {
                return (None, Some(runner_error(&op.op_id, "response-encoding")));
            }
        };
        (
            Some(ResultFrame {
                op_id: op.op_id.clone(),
                status: status_str.to_string(),
                payload: scrubbed,
                scrubbed: count,
                timing: Timing { exec_ms },
            }),
            None,
        )
    }
}

/// Response headers as a JSON object for the projection source. Names are
/// lower-cased (as `http` stores them). Repeated headers are joined with
/// ", ", except `set-cookie`: a comma can sit inside a cookie (`Expires`),
/// so it stays a list, one entry per cookie, each with its value masked.
fn response_headers(map: &reqwest::header::HeaderMap) -> Map<String, Value> {
    let mut out = Map::new();
    for name in map.keys() {
        let values: Vec<String> = map
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        if name.as_str() == "set-cookie" {
            let masked = values.iter().map(|v| Value::String(mask_cookie(v)));
            out.insert(name.to_string(), Value::Array(masked.collect()));
        } else {
            out.insert(name.to_string(), Value::String(values.join(", ")));
        }
    }
    out
}

/// `name=value; Attr; ...` becomes `name=[cookie]; Attr; ...`.
fn mask_cookie(raw: &str) -> String {
    let (pair, attrs) = match raw.split_once(';') {
        Some((p, a)) => (p, Some(a)),
        None => (raw, None),
    };
    let name = pair.split_once('=').map_or(pair, |(n, _)| n).trim();
    match attrs {
        Some(a) => format!("{name}=[cookie];{a}"),
        None => format!("{name}=[cookie]"),
    }
}

/// Checks `path` resolves within the resource's origin, then layers
/// query-params on top of any query string already present in path: a key named in `query-params`
/// replaces every existing value for that key, and the final query string
/// is re-encoded with keys sorted.
fn resolve_http_target(
    op_id: &str,
    resource: &Resource,
    args: &Map<String, Value>,
) -> std::result::Result<url::Url, ErrorFrame> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let mut resolved = origin::check(&resource.base_url, path)
        .map_err(|_| new_error(op_id, "host-not-allowed", json!({})))?;

    if let Some(Value::Object(qp)) = args.get("query-params") {
        let mut values: Vec<(String, Vec<String>)> = Vec::new();
        for (k, v) in resolved.query_pairs() {
            if let Some(entry) = values.iter_mut().find(|(ek, _)| *ek == k) {
                entry.1.push(v.into_owned());
            } else {
                values.push((k.into_owned(), vec![v.into_owned()]));
            }
        }
        for (k, v) in qp {
            let sv = string_value(Some(v));
            if let Some(entry) = values.iter_mut().find(|(ek, _)| ek == k) {
                entry.1 = vec![sv];
            } else {
                values.push((k.clone(), vec![sv]));
            }
        }
        values.sort_by(|a, b| a.0.cmp(&b.0));

        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, vs) in &values {
            for v in vs {
                ser.append_pair(k, v);
            }
        }
        let encoded = ser.finish();
        resolved.set_query(if encoded.is_empty() {
            None
        } else {
            Some(&encoded)
        });
    }
    Ok(resolved)
}

fn build_pending_request(
    op_id: &str,
    target: url::Url,
    args: &Map<String, Value>,
) -> std::result::Result<PendingRequest, ErrorFrame> {
    let method_str = string_value(args.get("method"));
    let method = if method_str.is_empty() {
        reqwest::Method::GET
    } else {
        reqwest::Method::from_bytes(method_str.to_uppercase().as_bytes())
            .map_err(|_| runner_error(op_id, "http-request"))?
    };

    let (body, content_type) =
        http_request_body(args.get("body")).map_err(|_| runner_error(op_id, "http-request"))?;

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(Value::Object(hs)) = args.get("headers") {
        for (k, v) in hs {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| runner_error(op_id, "http-request"))?;
            if is_refused_header(name.as_str()) {
                return Err(runner_error(op_id, "http-request"));
            }
            let val = reqwest::header::HeaderValue::from_str(&string_value(Some(v)))
                .map_err(|_| runner_error(op_id, "http-request"))?;
            headers.insert(name, val);
        }
    }
    if content_type.is_some() && !headers.contains_key(reqwest::header::CONTENT_TYPE) {
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
    }
    Ok(PendingRequest {
        method,
        url: target,
        headers,
        body,
    })
}

/// Header names an op may not set: hop-by-hop and routing headers. The
/// transport owns them, and `host` would reroute a request past the origin check.
fn is_refused_header(name: &str) -> bool {
    const REFUSED: [&str; 10] = [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "upgrade",
        "te",
        "trailer",
        "proxy-authorization",
        "proxy-connection",
        "keep-alive",
    ];
    REFUSED.iter().any(|r| name.eq_ignore_ascii_case(r))
}

/// Turns an op's body arg into a request body: absent/null means no body, a
/// string is sent as-is, and anything else is JSON-encoded.
fn http_request_body(
    v: Option<&Value>,
) -> std::result::Result<(Option<Vec<u8>>, Option<String>), serde_json::Error> {
    match v {
        None | Some(Value::Null) => Ok((None, None)),
        Some(Value::String(s)) => Ok((Some(s.clone().into_bytes()), None)),
        Some(other) => Ok((
            Some(serde_json::to_vec(other)?),
            Some("application/json".to_string()),
        )),
    }
}

fn downgrade_method(
    method: &reqwest::Method,
    status: u16,
    body: Option<Vec<u8>>,
) -> (reqwest::Method, Option<Vec<u8>>) {
    if matches!(status, 301..=303) && *method != reqwest::Method::HEAD {
        (reqwest::Method::GET, None)
    } else {
        (method.clone(), body)
    }
}

async fn read_capped_body(
    mut resp: reqwest::Response,
) -> std::result::Result<Vec<u8>, SendErrorKind> {
    let mut buf = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(|_| SendErrorKind::Runner)?;
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_HTTP_BODY_BYTES {
            return Err(SendErrorKind::TooLarge);
        }
    }
    Ok(buf)
}

/// Parses `body` as JSON when `content_type` names it or the body's own
/// shape looks like JSON; otherwise it is kept as a plain string.
fn decode_http_body(body: &[u8], content_type: &str) -> Value {
    let trimmed = trim_ascii_whitespace(body);
    let looks_json = content_type.to_ascii_lowercase().contains("json")
        || matches!(trimmed.first(), Some(b'{') | Some(b'['));
    if looks_json && let Ok(v) = serde_json::from_slice::<Value>(trimmed) {
        return v;
    }
    Value::String(String::from_utf8_lossy(body).into_owned())
}

fn trim_ascii_whitespace(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end && b[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &b[start..end]
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "http_cookie_tests.rs"]
mod cookie_tests;

#[cfg(test)]
#[path = "http_capture_tests.rs"]
mod capture_tests;

//! Removes known secret values from text before it is stored or shown as
//! evidence.

use base64::Engine;
use serde_json::{Map, Value};

/// Replaces every occurrence of each non-blank secret in `text` with
/// `"[scrubbed]"`. For each secret it also scrubs the forms a value
/// commonly takes once it has passed through an encoding step: URL query-
/// and path-escaped, and base64 (standard padded, standard unpadded, and
/// URL-safe). Every form across every secret is deduped and replaced
/// longest-first, so a short form can never leave a fragment of a longer
/// one behind. The returned count is the total number of replacements made
/// across all forms, so a secret that appears once but is caught by two
/// different forms counts twice.
pub fn scrub(text: &str, secrets: &[String]) -> (String, usize) {
    let ordered = forms_of(secrets);

    let mut text = text.to_string();
    let mut count = 0;
    for form in ordered {
        let n = text.matches(form.as_str()).count();
        if n == 0 {
            continue;
        }
        text = text.replace(form.as_str(), "[scrubbed]");
        count += n;
    }
    (text, count)
}

/// Scrubs a JSON value tree: every string leaf and every object key goes
/// through [`scrub`], so a secret that JSON would escape (quote, backslash,
/// newline, tab) is still found, and scrubbing can never break the
/// structure. A number whose JSON text contains a secret (a PIN, a numeric
/// token) is replaced by the string `"[scrubbed]"`; other numbers, bools
/// and null are left alone. If two keys collide
/// after scrubbing, the first one wins. Returns the new value and the total
/// replacement count.
pub fn scrub_value(v: &Value, secrets: &[String]) -> (Value, usize) {
    match v {
        Value::String(s) => {
            let (t, n) = scrub(s, secrets);
            (Value::String(t), n)
        }
        Value::Array(items) => {
            let mut count = 0;
            let out = items
                .iter()
                .map(|x| {
                    let (y, n) = scrub_value(x, secrets);
                    count += n;
                    y
                })
                .collect();
            (Value::Array(out), count)
        }
        Value::Object(m) => {
            let mut count = 0;
            let mut out = Map::with_capacity(m.len());
            for (k, child) in m {
                let (key, kn) = scrub(k, secrets);
                let (val, vn) = scrub_value(child, secrets);
                count += kn + vn;
                out.entry(key).or_insert(val);
            }
            (Value::Object(out), count)
        }
        Value::Number(num) => {
            let text = num.to_string();
            let (t, n) = scrub(&text, secrets);
            if t == text {
                (v.clone(), 0)
            } else {
                (Value::String("[scrubbed]".to_string()), n)
            }
        }
        other => (other.clone(), 0),
    }
}

/// Collects every non-blank secret's raw value plus its URL- and
/// base64-encoded forms, deduped, longest first so `replace` in [`scrub`]
/// can never be defeated by a shorter form matching inside a longer one.
fn forms_of(secrets: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut forms: Vec<String> = Vec::new();
    let mut add = |s: String| {
        if s.is_empty() || seen.contains(&s) {
            return;
        }
        seen.insert(s.clone());
        forms.push(s);
    };

    for s in secrets {
        if s.trim().is_empty() {
            continue;
        }
        add(s.clone());
        add(url_query_escape(s));
        add(url_path_escape(s));
        let raw = s.as_bytes();
        add(base64::engine::general_purpose::STANDARD.encode(raw));
        add(base64::engine::general_purpose::STANDARD_NO_PAD.encode(raw));
        add(base64::engine::general_purpose::URL_SAFE.encode(raw));
    }

    forms.sort_by_key(|a| std::cmp::Reverse(a.len()));
    forms
}

/// Query-component escape: space becomes `+`, and everything
/// outside `[A-Za-z0-9-_.~]` is percent-encoded.
fn url_query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b == b' ' {
            out.push('+');
        } else if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Path-segment escape: space becomes `%20`, and the segment
/// set of "sub-delim" characters (including `&`) is left unescaped along
/// with `[A-Za-z0-9-_.~]`.
fn url_path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if is_unreserved(b) || is_path_safe(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// The path-segment escape leaves `$ & + : = @`
/// unescaped among the "reserved" set, but still escapes `/ ; , ?` since
/// those separate path segments.
fn is_path_safe(b: u8) -> bool {
    matches!(b, b'$' | b'&' | b'+' | b':' | b'=' | b'@')
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        text: &'static str,
        secrets: &'static [&'static str],
        want: &'static str,
        count: usize,
    }

    #[test]
    fn scrub_table() {
        let cases = vec![
            Case {
                name: "single occurrence",
                text: r#"{"user":{"token":"s3cret"}}"#,
                secrets: &["s3cret"],
                want: r#"{"user":{"token":"[scrubbed]"}}"#,
                count: 1,
            },
            Case {
                name: "two occurrences",
                text: "s3cret and s3cret again",
                secrets: &["s3cret"],
                want: "[scrubbed] and [scrubbed] again",
                count: 2,
            },
            Case {
                name: "longest secret first",
                text: "abc ab",
                secrets: &["ab", "abc"],
                want: "[scrubbed] [scrubbed]",
                count: 2,
            },
            Case {
                name: "blank secret ignored",
                text: "abc ab",
                secrets: &["", "  "],
                want: "abc ab",
                count: 0,
            },
            Case {
                name: "no secrets",
                text: "nothing to see",
                secrets: &[],
                want: "nothing to see",
                count: 0,
            },
            Case {
                name: "url query-encoded secret with spaces and ampersand also scrubbed",
                text: "q=s3+cret%26more raw=s3 cret&more",
                secrets: &["s3 cret&more"],
                want: "q=[scrubbed] raw=[scrubbed]",
                count: 2,
            },
            Case {
                name: "url path-escaped form also scrubbed",
                text: "p=s3%20cret&more raw=s3 cret&more",
                secrets: &["s3 cret&more"],
                want: "p=[scrubbed] raw=[scrubbed]",
                count: 2,
            },
            Case {
                name: "base64 standard form also scrubbed",
                text: "b64=czNjcmV0 raw=s3cret",
                secrets: &["s3cret"],
                want: "b64=[scrubbed] raw=[scrubbed]",
                count: 2,
            },
            Case {
                name: "base64 raw unpadded form also scrubbed distinctly from padded form",
                text: "std=czNjcmV0MQ== raw=czNjcmV0MQ plain=s3cret1",
                secrets: &["s3cret1"],
                want: "std=[scrubbed] raw=[scrubbed] plain=[scrubbed]",
                count: 3,
            },
            Case {
                name: "base64 url-safe form scrubbed distinctly from standard form",
                text: "std=Pj4tPj8/Xw== url=Pj4tPj8_Xw== raw=>>->??_",
                secrets: &[">>->??_"],
                want: "std=[scrubbed] url=[scrubbed] raw=[scrubbed]",
                count: 3,
            },
            Case {
                name: "one-character secret over-scrubs matching text, accepted cost",
                text: "a cat sat on a mat",
                secrets: &["a"],
                want: "[scrubbed] c[scrubbed]t s[scrubbed]t on [scrubbed] m[scrubbed]t",
                count: 5,
            },
            Case {
                name: "count is total replacements across all forms, not distinct secrets",
                text: "c3Vi and sub",
                secrets: &["sub"],
                want: "[scrubbed] and [scrubbed]",
                count: 2,
            },
        ];

        for c in cases {
            let secrets: Vec<String> = c.secrets.iter().map(|s| s.to_string()).collect();
            let (got, count) = scrub(c.text, &secrets);
            assert_eq!(got, c.want, "case {}", c.name);
            assert_eq!(count, c.count, "case {}", c.name);
        }
    }

    #[test]
    fn scrub_value_scrubs_numbers_containing_a_secret() {
        let secrets = vec!["48213907".to_string()];
        let v =
            serde_json::json!({"a": 48213907, "b": [1482139070], "c": 77, "d": true, "e": null});
        let (got, n) = scrub_value(&v, &secrets);
        assert_eq!(
            got,
            serde_json::json!({"a": "[scrubbed]", "b": ["[scrubbed]"], "c": 77, "d": true, "e": null})
        );
        assert_eq!(n, 2);
    }

    // Expected escapes for every byte 0x00-0x7f, plus a handful of UTF-8
    // strings. Pins `url_path_escape` / `url_query_escape`.
    include!("scrub_escape_golden.rs");

    #[test]
    fn url_path_escape_matches_go_byte_table() {
        for (b, want_path, _want_query) in BYTE_CASES {
            let s = String::from_utf8(vec![*b]).expect("ascii byte");
            assert_eq!(&url_path_escape(&s), want_path, "byte {b:#04x}");
        }
    }

    #[test]
    fn url_query_escape_matches_go_byte_table() {
        for (b, _want_path, want_query) in BYTE_CASES {
            let s = String::from_utf8(vec![*b]).expect("ascii byte");
            assert_eq!(&url_query_escape(&s), want_query, "byte {b:#04x}");
        }
    }

    #[test]
    fn url_escape_matches_go_utf8_table() {
        for (s, want_path, want_query) in UTF8_CASES {
            assert_eq!(&url_path_escape(s), want_path, "path escape of {s:?}");
            assert_eq!(&url_query_escape(s), want_query, "query escape of {s:?}");
        }
    }
}

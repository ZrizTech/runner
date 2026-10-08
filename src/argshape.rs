//! Argv shapes for cli commands. A shape is a list of tokens; a token is a
//! literal or `{name:class}`. Classes are fixed here, no regex crate. Every
//! class refuses a leading `-` (no flag injection) and `..`.

/// The class names a shape token may use.
const CLASSES: [&str; 4] = ["slug", "word", "int", "relpath"];

/// Longest `relpath` accepted.
const MAX_RELPATH: usize = 256;

enum Tok<'a> {
    Literal(&'a str),
    Class(&'a str),
}

/// Splits a token into a literal or a `{name:class}` slot. `None` when a
/// slot is malformed or names an unknown class.
fn parse_tok(t: &str) -> Option<Tok<'_>> {
    if t.is_empty() {
        return None;
    }
    if !t.starts_with('{') && !t.ends_with('}') {
        return Some(Tok::Literal(t));
    }
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    let (name, class) = inner.split_once(':')?;
    let name_ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    (name_ok && CLASSES.contains(&class)).then_some(Tok::Class(class))
}

/// True when every token of `shape` is a literal or a known slot.
pub fn shape_valid(shape: &[String]) -> bool {
    !shape.is_empty() && shape.iter().all(|t| parse_tok(t).is_some())
}

fn slug_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn word_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn int_ok(s: &str) -> bool {
    (1..=9).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

fn relpath_ok(s: &str) -> bool {
    let Some(stem) = s.strip_suffix(".json") else {
        return false;
    };
    s.len() <= MAX_RELPATH && !stem.is_empty() && stem.split('/').all(slug_ok)
}

/// True when `value` belongs to `class`.
pub fn class_ok(class: &str, value: &str) -> bool {
    if value.starts_with('-') {
        return false;
    }
    match class {
        "slug" => slug_ok(value),
        "word" => word_ok(value),
        "int" => int_ok(value),
        "relpath" => relpath_ok(value),
        _ => false,
    }
}

/// True when `argv` equals one of `shapes` token by token.
pub fn matches(shapes: &[Vec<String>], argv: &[String]) -> bool {
    shapes.iter().any(|shape| {
        shape.len() == argv.len()
            && shape.iter().zip(argv).all(|(t, a)| match parse_tok(t) {
                Some(Tok::Literal(l)) => l == a,
                Some(Tok::Class(c)) => class_ok(c, a),
                None => false,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classes_accept_and_refuse() {
        let table: [(&str, &[&str], &[&str]); 4] = [
            (
                "slug",
                &["a", "qa-1", "0", &"a".repeat(64)],
                &["", "A", "a_b", "a.b", "-a", "a/b", &"a".repeat(65)],
            ),
            (
                "word",
                &["a", "A_b.c-1", "v1.2", &"a".repeat(64)],
                &["", "..", "-x", "a b", "a/b", "a$", &"a".repeat(65)],
            ),
            (
                "int",
                &["0", "42", "123456789"],
                &["", "-1", "1.5", "a", "1234567890"],
            ),
            (
                "relpath",
                &["a.json", "qa/zriz/hello.json", "a-b/c1.json"],
                &[
                    "",
                    "a",
                    ".json",
                    "/a.json",
                    "../a.json",
                    "a/../b.json",
                    "a//b.json",
                    "a/.json",
                    "A.json",
                    "a.json.txt",
                    "a b.json",
                    "-a.json",
                ],
            ),
        ];
        for (class, good, bad) in table {
            for g in good {
                assert!(class_ok(class, g), "{class} should accept {g:?}");
            }
            for b in bad {
                assert!(!class_ok(class, b), "{class} should refuse {b:?}");
            }
        }
    }

    #[test]
    fn shapes_match_exactly() {
        let shapes = vec![
            s(&["version"]),
            s(&["init", "{dir:slug}"]),
            s(&["run", "{file:relpath}", "--local"]),
        ];
        assert!(matches(&shapes, &s(&["version"])));
        assert!(matches(&shapes, &s(&["init", "qa"])));
        assert!(matches(&shapes, &s(&["run", "qa/a.json", "--local"])));
        assert!(!matches(&shapes, &s(&[])));
        assert!(!matches(&shapes, &s(&["version", "x"])));
        assert!(!matches(&shapes, &s(&["init"])));
        assert!(!matches(&shapes, &s(&["init", "qa", "x"])));
        assert!(!matches(&shapes, &s(&["init", "/x"])));
        assert!(!matches(&shapes, &s(&["init", ".."])));
        assert!(!matches(&shapes, &s(&["run", "/etc/x.json", "--local"])));
        assert!(!matches(&shapes, &s(&["run", "../x.json", "--local"])));
        assert!(!matches(&shapes, &s(&["run", "qa/a.json"])));
        assert!(!matches(&shapes, &s(&["Version"])));
    }

    #[test]
    fn shape_tokens_validate() {
        assert!(shape_valid(&s(&["init", "{dir:slug}", "--x"])));
        for bad in [
            "{dir}",
            "{dir:regex}",
            "{:slug}",
            "{dir:slug",
            "dir:slug}",
            "",
        ] {
            assert!(!shape_valid(&s(&[bad])), "{bad:?}");
        }
        assert!(!shape_valid(&[]));
    }
}

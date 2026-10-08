//! Pre-checks raw SQL text so the runner never sends a write statement to a
//! resource configured as read-only.

/// Errors from [`check`], naming the offending keyword.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReadOnlyError {
    #[error("raw SQL query must not be empty")]
    Empty,
    #[error("raw SQL queries must be read-only, got: {0}")]
    NotReadOnly(String),
    #[error("raw SQL queries must not contain write operations, found: {0}")]
    WriteOperation(String),
}

const READ_PREFIXES: &[&str] = &["SELECT", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "WITH"];

/// Refused anywhere in a statement. This is fail closed: some of these
/// (SET, LOCK, HANDLER, CALL) can also appear as ordinary identifiers or
/// column names, so a legitimate read like "SELECT lock FROM t" is refused
/// too. That over-refusal is the accepted cost of never letting a real
/// write slip through.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE", "CREATE", "GRANT", "REVOKE", "INTO",
    "OUTFILE", "DUMPFILE", "LOAD", "HANDLER", "LOCK", "CALL", "SET", "RENAME",
];

/// Returns `Ok(())` when `sql` is a read-only statement, else an error
/// explaining why it was refused.
pub fn check(sql: &str) -> std::result::Result<(), ReadOnlyError> {
    if sql.trim().is_empty() {
        return Err(ReadOnlyError::Empty);
    }

    let stripped = strip_line_comments(&strip_plain_block_comments(sql));
    let normalized = stripped.trim().to_uppercase();

    let tokens: Vec<&str> = normalized
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();

    let first = tokens.first().copied().unwrap_or("");
    if !READ_PREFIXES.contains(&first) {
        return Err(ReadOnlyError::NotReadOnly(first.to_string()));
    }
    for t in &tokens {
        if WRITE_KEYWORDS.contains(t) {
            return Err(ReadOnlyError::WriteOperation((*t).to_string()));
        }
    }
    Ok(())
}

/// Strips `/* ... */` block comments, but leaves MySQL's executable
/// `/*! ... */` form in place (`/*!` is not a plain block comment opener),
/// so its keywords are still seen by the write-keyword scan. Also strips
/// the empty `/**/` form.
fn strip_plain_block_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            // Empty comment "/**/".
            if bytes.get(i + 2) == Some(&b'*') && bytes.get(i + 3) == Some(&b'/') {
                i += 4;
                continue;
            }
            // MySQL executable comment "/*!": not a plain block comment,
            // leave it (and its content) in place, code as-is.
            if bytes.get(i + 2) == Some(&b'!') {
                out.push(sql[i..].chars().next().unwrap_or('/'));
                i += 1;
                continue;
            }
            // Plain block comment: find the closing "*/".
            if let Some(end) = sql[i + 2..].find("*/") {
                i += 2 + end + 2;
                continue;
            }
            // Unterminated comment: there is no closing "*/" to match, so
            // leave the rest of the string as-is.
            out.push_str(&sql[i..]);
            break;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Strips `-- ...` line comments (to end of line).
fn strip_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '-' && sql[i..].starts_with("--") {
            if let Some(nl) = sql[i..].find('\n') {
                // Skip up to (not including) the newline; resume there.
                let resume = i + nl;
                while let Some(&(j, _)) = chars.peek() {
                    if j < resume {
                        chars.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
            break; // Comment runs to end of string.
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        sql: &'static str,
        want_err: &'static str, // substring expected in error, "" means Ok
    }

    #[test]
    fn check_table() {
        let cases = vec![
            Case {
                name: "select",
                sql: "SELECT 1",
                want_err: "",
            },
            Case {
                name: "lowercase select",
                sql: "select * from t",
                want_err: "",
            },
            Case {
                name: "with cte select",
                sql: "WITH x AS (SELECT 1) SELECT * FROM x",
                want_err: "",
            },
            Case {
                name: "insert",
                sql: "INSERT INTO t VALUES (1)",
                want_err: "INSERT",
            },
            Case {
                name: "statement injection",
                sql: "SELECT 1;INSERT INTO t VALUES(1)",
                want_err: "INSERT",
            },
            Case {
                name: "cte delete bypass",
                sql: "WITH x AS (SELECT 1) DELETE FROM t",
                want_err: "DELETE",
            },
            Case {
                name: "block comment stripped",
                sql: "/* DROP */ SELECT 1",
                want_err: "",
            },
            Case {
                name: "line comment stripped",
                sql: "-- DELETE\nSELECT 1",
                want_err: "",
            },
            Case {
                name: "empty",
                sql: "",
                want_err: "empty",
            },
            Case {
                name: "blank",
                sql: "   ",
                want_err: "empty",
            },
            Case {
                name: "mysql executable comment treated as code not stripped",
                sql: "/*! INSERT INTO t VALUES (1) */",
                want_err: "INSERT",
            },
            Case {
                name: "select into outfile refused",
                sql: "SELECT * FROM t INTO OUTFILE '/tmp/x'",
                want_err: "INTO",
            },
            Case {
                name: "select into dumpfile refused",
                sql: "SELECT * FROM t INTO DUMPFILE '/tmp/x'",
                want_err: "INTO",
            },
            Case {
                name: "load data refused",
                sql: "LOAD DATA INFILE '/tmp/x' INTO TABLE t",
                want_err: "LOAD",
            },
            Case {
                name: "handler refused",
                sql: "HANDLER t OPEN",
                want_err: "HANDLER",
            },
            Case {
                name: "lock tables refused",
                sql: "LOCK TABLES t READ",
                want_err: "LOCK",
            },
            Case {
                name: "call refused",
                sql: "CALL p()",
                want_err: "CALL",
            },
            Case {
                name: "set refused",
                sql: "SET @x = 1",
                want_err: "SET",
            },
            Case {
                name: "rename table refused",
                sql: "RENAME TABLE t TO u",
                want_err: "RENAME",
            },
            Case {
                name: "select lock column refused over-refusal accepted",
                sql: "SELECT lock FROM t",
                want_err: "LOCK",
            },
        ];

        for c in cases {
            let got = check(c.sql);
            if c.want_err.is_empty() {
                assert!(got.is_ok(), "case {}: {:?}", c.name, got);
                continue;
            }
            let err = got.unwrap_err();
            assert!(
                err.to_string().contains(c.want_err),
                "case {}: err = {}, want containing {}",
                c.name,
                err,
                c.want_err
            );
        }
    }
}

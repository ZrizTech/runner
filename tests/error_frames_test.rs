//! The error frames of the runner: each frame made in `src/` has a reason of
//! the contract list and only the `details` keys of its row in
//! `cause/reasons.json` (spec 11.3).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

fn json(path: &str) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("json")
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            sources(&p, out);
        } else {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if n.ends_with(".rs") && !n.ends_with("_tests.rs") && n != "test_support.rs" {
                out.push(p);
            }
        }
    }
}

/// The text between the parentheses that open at `open` (a `(`).
fn args_at(src: &str, open: usize) -> &str {
    let mut depth = 0;
    for (i, c) in src[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced call");
}

fn literals(s: &str) -> Vec<String> {
    s.split('"').skip(1).step_by(2).map(String::from).collect()
}

/// The string literals that are followed by a colon (the keys of a `json!`).
fn keys(s: &str) -> Vec<String> {
    let parts: Vec<&str> = s.split('"').collect();
    (1..parts.len().saturating_sub(1))
        .step_by(2)
        .filter(|&i| parts[i + 1].trim_start().starts_with(':'))
        .map(|i| parts[i].to_string())
        .collect()
}

#[test]
fn every_error_frame_in_src_has_a_true_reason_and_its_keys() {
    let reasons = json("contract/cause/reasons.json");
    let closed: BTreeSet<String> =
        json("contract/error.json")["properties"]["details"]["properties"]
            .as_object()
            .expect("props")
            .keys()
            .cloned()
            .collect();
    let list: Vec<String> = json("contract/error.json")["properties"]["reason"]["enum"]
        .as_array()
        .expect("enum")
        .iter()
        .map(|v| v.as_str().expect("str").to_string())
        .collect();
    let rows: HashMap<String, BTreeSet<String>> = reasons["reasons"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            let keys = r["details"].as_array().expect("details");
            let ks = keys.iter().map(|k| k.as_str().unwrap().to_string());
            (
                r["reason"].as_str().unwrap().to_string(),
                ks.filter(|k| closed.contains(k)).collect(),
            )
        })
        .collect();
    let wheres: Vec<String> = reasons["rules"]["where"]
        .as_array()
        .expect("where")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    let mut files = Vec::new();
    sources(Path::new("src"), &mut files);
    let mut seen = 0;
    for f in files {
        let src = fs::read_to_string(&f).expect("read");
        for (at, name) in src
            .match_indices("new_error(")
            .chain(src.match_indices("runner_error("))
            .chain(src.match_indices("ErrorFrame::new("))
            .chain(src.match_indices("Error::new("))
        {
            if src[..at].ends_with("fn ") {
                continue;
            }
            let a = args_at(&src, at + name.len() - 1);
            let lits = literals(a);
            let here = format!(
                "{} near {}",
                f.display(),
                &src[at..at + 40.min(src.len() - at)]
            );
            seen += 1;
            if name.starts_with("runner_error") {
                let w = lits.first().expect("a where word");
                assert!(
                    wheres.contains(w),
                    "where word {w} is not in rules.where: {here}"
                );
                continue;
            }
            let used = keys(a);
            // A reason that is not a literal (one table maps it): the keys
            // must be in the closed set at least.
            let reason = lits.iter().find(|l| list.contains(l));
            match reason {
                Some(r) => {
                    let row = &rows[r];
                    for k in &used {
                        assert!(row.contains(k), "key {k} is not in the row of {r}: {here}");
                    }
                    if r == "runner-error" {
                        let w = lits.iter().skip_while(|l| *l != "where").nth(1);
                        assert!(w.is_none_or(|w| wheres.contains(w)), "{here}");
                    }
                }
                None => {
                    for k in &used {
                        assert!(
                            closed.contains(k),
                            "key {k} is not in the closed set: {here}"
                        );
                    }
                }
            }
        }
    }
    assert!(seen > 30, "the scan saw only {seen} makers");
}

#[test]
fn the_reason_list_is_the_runner_and_worker_words_of_the_contract() {
    let reasons = json("contract/cause/reasons.json");
    let mut want: BTreeSet<String> = reasons["reasons"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|r| {
            r["by"]
                .as_array()
                .expect("by")
                .iter()
                .any(|b| b == "runner" || b == "worker")
        })
        .map(|r| r["reason"].as_str().unwrap().to_string())
        .collect();
    want.insert("placeholder-not-found".into());
    let got: BTreeSet<String> = json("contract/error.json")["properties"]["reason"]["enum"]
        .as_array()
        .expect("enum")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, want);
}

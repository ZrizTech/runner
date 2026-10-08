//! Trims op payloads down to the fields the cloud asked for, so a
//! resource's full response never has to leave the runner.

use serde_json::{Map, Value};

/// Caps how many rows [`select_columns`] returns, matching the evidence and
/// payload size limits the cloud expects from a single result.
const MAX_COLUMN_ROWS: usize = 100;

/// Returns a new map holding only the values at the given paths in `data`.
/// A digit path segment applied to a list is treated as an index: the kept
/// element stays at its original position, with null-padded gaps before it
/// and nothing kept after it. A path that is missing from `data`, or that
/// runs through a value that is not a map or list, is skipped silently.
pub fn select_paths(data: &Map<String, Value>, paths: &[Vec<String>]) -> Map<String, Value> {
    let mut result = Value::Object(Map::new());
    for path in paths {
        if path.is_empty() {
            continue;
        }
        let src = Value::Object(data.clone());
        if let Some(merged) = merge_path(&src, Some(&result), path) {
            result = merged;
        }
    }
    match result {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Copies the value at `path` from `src` into `dst`, building whatever maps
/// or lists are needed along the way, mirroring `src`'s own shape at each
/// step. Returns `None`, leaving the caller's `dst` untouched, when `path`
/// does not exist in `src`.
fn merge_path(src: &Value, dst: Option<&Value>, path: &[String]) -> Option<Value> {
    if path.is_empty() {
        return Some(src.clone());
    }
    let (seg, rest) = (&path[0], &path[1..]);
    match src {
        Value::Object(m) => merge_map_path(m, dst, seg, rest),
        Value::Array(items) => merge_list_path(items, dst, seg, rest),
        _ => None,
    }
}

fn merge_map_path(
    src: &Map<String, Value>,
    dst: Option<&Value>,
    seg: &str,
    rest: &[String],
) -> Option<Value> {
    let child = src.get(seg)?;
    let mut m = match dst {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let new_child = merge_path(child, m.get(seg), rest)?;
    m.insert(seg.to_string(), new_child);
    Some(Value::Object(m))
}

fn merge_list_path(
    src: &[Value],
    dst: Option<&Value>,
    seg: &str,
    rest: &[String],
) -> Option<Value> {
    let idx: usize = seg.parse().ok()?;
    if idx >= src.len() {
        return None;
    }
    let mut list = match dst {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    while list.len() <= idx {
        list.push(Value::Null);
    }
    let new_child = merge_path(&src[idx], Some(&list[idx]), rest)?;
    list[idx] = new_child;
    Some(Value::Array(list))
}

/// Keeps only the named columns from each row, matching column names
/// case-insensitively but using the spelling from `cols` as the output key.
/// At most 100 rows are returned.
pub fn select_columns(rows: &[Map<String, Value>], cols: &[String]) -> Vec<Map<String, Value>> {
    let limit = rows.len().min(MAX_COLUMN_ROWS);
    let mut result = Vec::with_capacity(limit);
    for row in &rows[..limit] {
        let mut out = Map::with_capacity(cols.len());
        for col in cols {
            if let Some(val) = lookup_column(row, col) {
                out.insert(col.clone(), val.clone());
            }
        }
        result.push(out);
    }
    result
}

fn lookup_column<'a>(row: &'a Map<String, Value>, col: &str) -> Option<&'a Value> {
    row.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(col))
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map_of(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    fn paths(segs: &[&[&str]]) -> Vec<Vec<String>> {
        segs.iter()
            .map(|p| p.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn keeps_only_selected_leaf_drops_siblings() {
        let data = map_of(json!({
            "status": 200,
            "body": {
                "user": {"id": 1, "email": "e"},
                "other": 1
            }
        }));
        let got = select_paths(&data, &paths(&[&["body", "user", "id"]]));
        let want = map_of(json!({"body": {"user": {"id": 1}}}));
        assert_eq!(got, want);
    }

    #[test]
    fn digit_segment_indexes_into_a_list_nil_padding_and_truncating() {
        let data = map_of(json!({
            "body": {"items": [{"sku": "A"}, {"sku": "B"}]}
        }));
        let got = select_paths(&data, &paths(&[&["body", "items", "1", "sku"]]));
        let want = map_of(json!({
            "body": {"items": [null, {"sku": "B"}]}
        }));
        assert_eq!(got, want);
    }

    #[test]
    fn missing_path_skipped_silently() {
        let data = map_of(json!({"status": 200}));
        let got = select_paths(&data, &paths(&[&["body", "user", "id"]]));
        assert_eq!(got, Map::new());
    }

    #[test]
    fn path_through_non_container_skipped_silently() {
        let data = map_of(json!({"status": 200}));
        let got = select_paths(&data, &paths(&[&["status", "code"]]));
        assert_eq!(got, Map::new());
    }

    #[test]
    fn empty_paths_yields_empty_map() {
        let data = map_of(json!({"status": 200}));
        let got = select_paths(&data, &[]);
        assert_eq!(got, Map::new());
    }

    #[test]
    fn select_columns_matches_case_insensitively_and_caps_at_100() {
        let rows = vec![
            map_of(json!({"id": 1, "email": "a@example.com", "name": "A"})),
            map_of(json!({"id": 2, "email": "b@example.com", "name": "B"})),
        ];
        let cols = vec!["id".to_string(), "EMAIL".to_string()];
        let got = select_columns(&rows, &cols);
        let want = vec![
            map_of(json!({"id": 1, "EMAIL": "a@example.com"})),
            map_of(json!({"id": 2, "EMAIL": "b@example.com"})),
        ];
        assert_eq!(got, want);

        let many: Vec<Map<String, Value>> = (0..150).map(|i| map_of(json!({"id": i}))).collect();
        let got_many = select_columns(&many, &["id".to_string()]);
        assert_eq!(got_many.len(), 100);
    }
}

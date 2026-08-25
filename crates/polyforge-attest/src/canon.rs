//! Canonical JSON: sorted object keys, compact separators, recursive.
//!
//! The conversion walks the whole [`serde_json::Value`] tree and rebuilds every
//! object through a [`BTreeMap`], so output ordering never depends on how the
//! input was constructed (including when the `preserve_order` feature of
//! `serde_json` is unified into the build).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Serializes `value` to canonical JSON text: recursively sorted object keys
/// and compact separators (``,`:`), no extra whitespace anywhere.
pub fn canonical_json(value: &Value) -> String {
    serde_json::to_string(&to_canonical(value)).expect("Value serialization cannot fail")
}

fn to_canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            let mut out = Map::new();
            for (key, item) in sorted {
                out.insert(key.clone(), to_canonical(item));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(to_canonical).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_object_keys_recursively() {
        let unsorted = r#"{"z":1,"a":{"y":2,"b":3}}"#;
        let v: Value = serde_json::from_str(unsorted).expect("parses");
        assert_eq!(canonical_json(&v), r#"{"a":{"b":3,"y":2},"z":1}"#);
    }

    #[test]
    fn emits_compact_separators() {
        let spaced = r#"{ "k" : [1, 2] }"#;
        let v: Value = serde_json::from_str(spaced).expect("parses");
        assert_eq!(canonical_json(&v), r#"{"k":[1,2]}"#);
    }

    #[test]
    fn leaves_scalars_and_arrays_in_order() {
        let v: Value = serde_json::from_str(r#"[3,1,{"b":2,"a":1}]"#).expect("parses");
        assert_eq!(canonical_json(&v), r#"[3,1,{"a":1,"b":2}]"#);
        assert_eq!(canonical_json(&Value::from(42)), "42");
    }
}

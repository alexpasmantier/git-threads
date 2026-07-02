//! Canonical JSON serialization (SPEC.md §6).
//!
//! Canonical bytes are what gets stored in git and what event IDs are hashed
//! from, so two implementations must produce identical bytes for the same
//! logical document: UTF-8, keys sorted lexicographically, no insignificant
//! whitespace, integers only.

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CanonicalError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("non-integer number at {path}: canonical JSON forbids floats (SPEC.md §6)")]
    Float { path: String },
}

/// Serialize `value` to canonical JSON bytes.
///
/// Key sorting comes from `serde_json`'s default `BTreeMap`-backed object
/// representation (this crate must never enable the `preserve_order`
/// feature), and compact output has no insignificant whitespace.
pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)?;
    reject_floats(&value, "$")?;
    Ok(serde_json::to_vec(&value)?)
}

fn reject_floats(value: &Value, path: &str) -> Result<(), CanonicalError> {
    match value {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => Err(CanonicalError::Float {
            path: path.to_string(),
        }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, v)| reject_floats(v, &format!("{path}[{i}]"))),
        Value::Object(map) => map
            .iter()
            .try_for_each(|(k, v)| reject_floats(v, &format!("{path}.{k}"))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_strips_whitespace() {
        let bytes = to_canonical_json(&json!({"zeta": 1, "alpha": {"b": 2, "a": 3}})).unwrap();
        assert_eq!(bytes, br#"{"alpha":{"a":3,"b":2},"zeta":1}"#);
    }

    #[test]
    fn rejects_floats_anywhere() {
        let err = to_canonical_json(&json!({"a": [1, {"b": 1.5}]})).unwrap_err();
        assert!(matches!(err, CanonicalError::Float { ref path } if path == "$.a[1].b"));
    }
}

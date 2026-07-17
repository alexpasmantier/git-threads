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
/// Object keys are sorted here, explicitly: canonical bytes must not depend
/// on `serde_json`'s map order, which the `preserve_order` feature changes —
/// and feature unification means any crate in a consumer's dependency graph
/// could enable it. Compact output has no insignificant whitespace; strings
/// take `serde_json`'s minimal escaping.
pub fn to_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write_canonical(&value, "$", &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, path: &str, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => {
            return Err(CanonicalError::Float { path: path.to_string() });
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, &format!("{path}[{i}]"), out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable(); // byte order (SPEC.md §6)
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_canonical(&map[key.as_str()], &format!("{path}.{key}"), out)?;
            }
            out.push(b'}');
        }
        scalar => serde_json::to_writer(&mut *out, scalar)?,
    }
    Ok(())
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

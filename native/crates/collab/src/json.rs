//! JSON values inside the shared document.
//!
//! Leaf JSON (metadata values, mime payloads, attachments) is stored as yrs
//! [`Any`], the representation Yjs/JupyterLab use for plain JSON. Two things
//! `Any` cannot carry are handled with *fidelity records* (see
//! [`crate::doc`]): object key order (`Any::Map` is a hash map) and integers
//! outside the 32-bit range (lib0 sends them as floats, so they come back as
//! `Number::Float`). Integers within ±2^31 and every float, including
//! integral ones like `1.0`, survive as-is.

use std::collections::HashMap;
use std::sync::Arc;

use herse_nb::{Map, Value};
use yrs::{Any, Number};

/// Largest integer lib0 sends as an integer (larger ones go out as floats).
const LIB0_INT_MAX: i64 = 0x7FFF_FFFF;

/// JSON -> `Any`.
pub(crate) fn to_any(v: &Value) -> Any {
    match v {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => Any::Number(if let Some(i) = n.as_i64() {
            Number::Int(i)
        } else {
            // u64 above i64::MAX or a float.
            Number::Float(n.as_f64().unwrap_or(0.0))
        }),
        Value::String(s) => Any::String(Arc::from(s.as_str())),
        Value::Array(a) => Any::Array(a.iter().map(to_any).collect::<Vec<_>>().into()),
        Value::Object(m) => Any::Map(Arc::new(m.iter().map(|(k, v)| (k.clone(), to_any(v))).collect())),
    }
}

/// `Any` -> JSON, object keys sorted (the deterministic default every peer
/// agrees on; fidelity records restore other orders).
pub(crate) fn from_any(a: &Any) -> Value {
    match a {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => Value::Bool(*b),
        Any::Number(Number::Int(i)) => Value::from(*i),
        Any::Number(Number::Float(f)) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Any::String(s) => Value::String(s.to_string()),
        // Not produced by Herse or Jupyter; keep the bytes as a number array.
        Any::Buffer(b) => Value::Array(b.iter().map(|&x| Value::from(x)).collect()),
        Any::Array(a) => Value::Array(a.iter().map(from_any).collect()),
        Any::Map(m) => Value::Object(map_from_any(m)),
    }
}

/// `Any::Map` contents -> JSON object with sorted keys.
pub(crate) fn map_from_any(m: &HashMap<String, Any>) -> Map {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort_unstable();
    keys.into_iter().map(|k| (k.clone(), from_any(&m[k]))).collect()
}

/// Whether storing `v` as `Any` and reading it back would change it: an
/// object whose keys are not sorted, or an integer lib0 would send as a float.
pub(crate) fn needs_record(v: &Value) -> bool {
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => !(-LIB0_INT_MAX..=LIB0_INT_MAX).contains(&i),
            None => n.is_u64(),
        },
        Value::Array(a) => a.iter().any(needs_record),
        Value::Object(m) => !keys_sorted(m) || m.values().any(needs_record),
        _ => false,
    }
}

pub(crate) fn keys_sorted(m: &Map) -> bool {
    let mut prev: Option<&str> = None;
    for k in m.keys() {
        if prev.is_some_and(|p| p > k.as_str()) {
            return false;
        }
        prev = Some(k);
    }
    true
}

/// Equality that ignores object key order and the int/float distinction
/// (`2` == `2.0`): what survives a trip through `Any` is equal to the
/// original under this relation.
pub(crate) fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x == y || x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| loose_eq(a, b)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| loose_eq(v, w)))
        }
        _ => a == b,
    }
}

/// Restore what a fidelity `record` remembers about `decoded`. If the record
/// still describes the value (loosely equal) the record wins outright;
/// otherwise objects take the record's key order for the keys they share
/// (recursively), so editing one metadata key keeps the others exact.
pub(crate) fn reconcile(decoded: Value, record: &Value) -> Value {
    if loose_eq(&decoded, record) {
        return record.clone();
    }
    match (decoded, record) {
        (Value::Object(mut d), Value::Object(r)) => {
            let mut out = Map::new();
            for (k, rv) in r {
                if let Some(dv) = d.shift_remove(k) {
                    out.insert(k.clone(), reconcile(dv, rv));
                }
            }
            out.extend(d);
            Value::Object(out)
        }
        (Value::Array(d), Value::Array(r)) if d.len() == r.len() => {
            Value::Array(d.into_iter().zip(r).map(|(d, r)| reconcile(d, r)).collect())
        }
        (d, _) => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Through the real lib0 encoding, the way a peer receives it.
    fn roundtrip(v: &Value) -> Value {
        use yrs::updates::decoder::Decode;
        use yrs::{Map as _, ReadTxn, Transact};
        let doc = yrs::Doc::new();
        let map = doc.get_or_insert_map("m");
        map.insert(&mut doc.transact_mut(), "v", to_any(v));
        let update = doc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        let other = yrs::Doc::new();
        let m2 = other.get_or_insert_map("m");
        other.transact_mut().apply_update(yrs::Update::decode_v1(&update).unwrap()).unwrap();
        let txn = other.transact();
        match m2.get(&txn, "v").unwrap() {
            yrs::Out::Any(a) => from_any(&a),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn small_ints_and_floats_survive_lib0() {
        let v = json!({"a": 1, "b": 1.0, "c": -2147483647, "d": 0.5, "e": [1, 2.0, null, true, "x"]});
        assert!(!needs_record(&v));
        assert_eq!(roundtrip(&v), v);
        // `1.0` stays a float: serialized text is identical.
        assert_eq!(serde_json::to_string(&roundtrip(&v)).unwrap(), serde_json::to_string(&v).unwrap());
    }

    #[test]
    fn big_ints_and_unsorted_keys_need_records() {
        let big = json!({"t": 1_697_040_000_000_i64});
        assert!(needs_record(&big));
        let back = roundtrip(&big);
        assert!(loose_eq(&back, &big));
        assert_ne!(serde_json::to_string(&back).unwrap(), serde_json::to_string(&big).unwrap());
        assert_eq!(reconcile(back, &big), big);

        let unsorted: Value = serde_json::from_str(r#"{"z": 1, "a": {"y": 2, "b": 3}}"#).unwrap();
        assert!(needs_record(&unsorted));
        let back = roundtrip(&unsorted);
        let fixed = reconcile(back, &unsorted);
        assert_eq!(serde_json::to_string(&fixed).unwrap(), r#"{"z":1,"a":{"y":2,"b":3}}"#);
    }

    #[test]
    fn reconcile_keeps_order_of_unchanged_keys() {
        let record: Value = serde_json::from_str(r#"{"z": 1, "m": 2, "a": 3}"#).unwrap();
        let decoded: Value = serde_json::from_str(r#"{"a": 30, "b": 4, "m": 2, "z": 1}"#).unwrap();
        let out = reconcile(decoded, &record);
        assert_eq!(serde_json::to_string(&out).unwrap(), r#"{"z":1,"m":2,"a":30,"b":4}"#);
    }
}

//! Lenient readers over a `serde_json::Value` (#1156).
//!
//! Three sensors read vendor JSON — Redfish (bmc), the Proxmox API (pve), the
//! podman socket (container) — through hand-written helpers rather than
//! through serde, because the shapes drift between releases in ways a derived
//! struct would turn into a silent `None`: a number that arrives as a string,
//! a flag that is `0` on one release and absent on the next, a reading that
//! moved into a nested `Reading` member. Each crate had its own copy of the
//! same six helpers with the same header comment, and the copies disagreed
//! *deliberately*: Redfish's `text` refuses a number and trims, Proxmox's
//! `text` accepts one and does not. That is exactly the kind of difference
//! that should be a **name**, not a crate boundary — so these come in a
//! `strict` and a `lenient` spelling, and each caller says which it means.
//!
//! Everything here returns `None`/`false`/empty for a shape it does not read;
//! nothing panics and nothing invents a value. Absent stays absent.

use serde_json::Value;

/// `v[key]`, treating JSON `null` as absent.
pub fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

/// A **string** member, trimmed, non-empty. A number is not a string here —
/// Redfish's `Name`/`Model`/`SerialNumber` are strings or nothing.
pub fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A string member as-is (non-empty), **or a number rendered as text** — the
/// Proxmox API serves an id as `140` on one release and `"140"` on the next.
pub fn text_lenient(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A **number** member. A numeric string is not a number here.
pub fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// A number member, **or a string that parses as one** — `"1.20"` in a
/// `loadavg` array, `"8G"`-free sizes served as decimal strings.
pub fn number_lenient(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(as_number_lenient)
}

/// [`number_lenient`] over a value rather than a member.
pub fn as_number_lenient(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A number from any of several member names, in preference order — the
/// alias table for a reading that moved between releases. Absent from all of
/// them stays absent.
pub fn first_number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| number(v, k))
}

/// An unsigned integer from any of several member names, in preference order.
pub fn first_uint(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_u64))
}

/// An embedded array member, or empty.
pub fn array(body: Option<&Value>, key: &str) -> Vec<Value> {
    body.and_then(|b| b.get(key))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A flag from a value that may be a bool, a number (`0` is false) or a
/// string the caller's `truthy` reads (`"1"`, `"true"`, `"yes"` — the set is
/// the API's, not this module's). Anything else is `false`.
pub fn flag_value(v: &Value, truthy: impl Fn(&str) -> bool) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => truthy(s),
        _ => false,
    }
}

/// [`flag_value`] on a member; an absent member is `false`.
pub fn flag(v: &Value, key: &str, truthy: impl Fn(&str) -> bool) -> bool {
    v.get(key).is_some_and(|x| flag_value(x, truthy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The strict/lenient split is the point: the same document reads
    /// differently, on purpose, and the name says which.
    #[test]
    fn strict_and_lenient_disagree_where_they_should() {
        let v = json!({"id": 140, "name": " x ", "empty": "", "n": "1.5", "z": null});
        assert_eq!(text(&v, "id"), None, "a number is not a strict string");
        assert_eq!(text_lenient(&v, "id").as_deref(), Some("140"));
        assert_eq!(text(&v, "name").as_deref(), Some("x"), "strict trims");
        assert_eq!(
            text_lenient(&v, "name").as_deref(),
            Some(" x "),
            "lenient keeps"
        );
        assert_eq!(text(&v, "empty"), None);
        assert_eq!(text_lenient(&v, "empty"), None);
        assert_eq!(
            number(&v, "n"),
            None,
            "a numeric string is not a strict number"
        );
        assert_eq!(number_lenient(&v, "n"), Some(1.5));
        assert_eq!(get(&v, "z"), None, "null is absent");
        assert_eq!(get(&v, "missing"), None);
    }

    #[test]
    fn alias_tables_take_the_first_present_name() {
        let v = json!({"LastPowerOutputWatts": 400.0, "PowerInputWatts": 450.0});
        assert_eq!(
            first_number(&v, &["PowerInputWatts", "LastPowerOutputWatts"]),
            Some(450.0)
        );
        assert_eq!(
            first_number(&v, &["Nope", "LastPowerOutputWatts"]),
            Some(400.0)
        );
        assert_eq!(first_number(&v, &["Nope"]), None);
        assert_eq!(first_uint(&json!({"a": 3}), &["b", "a"]), Some(3));
        assert_eq!(first_uint(&json!({"a": -3}), &["a"]), None);
    }

    #[test]
    fn a_flag_reads_every_spelling_and_invents_none() {
        let truthy = |s: &str| matches!(s, "1" | "true" | "yes");
        let v = json!({"b": true, "n": 1, "z": 0, "s": "yes", "no": "0", "o": {}});
        assert!(flag(&v, "b", truthy));
        assert!(flag(&v, "n", truthy));
        assert!(!flag(&v, "z", truthy));
        assert!(flag(&v, "s", truthy));
        assert!(!flag(&v, "no", truthy));
        assert!(!flag(&v, "o", truthy));
        assert!(!flag(&v, "absent", truthy));
    }

    #[test]
    fn an_array_member_or_nothing() {
        let body = json!({"Fans": [1, 2]});
        assert_eq!(array(Some(&body), "Fans").len(), 2);
        assert!(array(Some(&body), "PowerSupplies").is_empty());
        assert!(array(None, "Fans").is_empty());
    }
}

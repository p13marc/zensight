//! Overlaying policy fragments into one effective document (#938).
//!
//! Three rules, and each one is a decision that could have gone the other way.

use serde_json::{Map, Value};

/// The key a list of named objects merges on.
///
/// Every `@desired` payload that carries a list carries a list of *named*
/// things — expectations, log rules, threshold rules — and every one of them
/// spells the name `name` except log rules, which spell it `id`.
const NAME_KEYS: &[&str] = &["name", "id"];

/// Overlay `overlay` onto `base`.
///
/// - **Objects merge recursively.** A class that sets one field of
///   `thresholds` does not have to restate the rest.
/// - **`null` deletes.** There is no other way for a later class or a host
///   override to *remove* something an earlier one set; without it the only
///   escape from an inherited field is not to inherit, which means not using
///   the class.
/// - **A list of named objects merges by name; any other list replaces.**
///
/// # Why lists are not simply replaced
///
/// Replacement is the simpler rule and it was rejected. Every list here is a
/// set of independent rules, and the thing an operator wants from a class
/// hierarchy is "everything the base class watches, **plus** these" — with
/// replacement, `hypervisors` adding one expectation silently drops the
/// twelve that `all-hosts` contributed, and the loss is invisible: the
/// document is well-formed, the sensor accepts it, and twelve conditions stop
/// being watched.
///
/// # Why not simply concatenated
///
/// Because then a class could not *change* an inherited rule's threshold, only
/// add a second rule with the same name — and two rules sharing a name share
/// one alert key (RFC 11 §3.1), which is the collision `#849`'s validators
/// exist to refuse. Merging by name is the only rule that lets a class both
/// extend and adjust.
///
/// A list whose items are not all named objects replaces, because there is
/// nothing to merge on: `expect_status: [200, 204]` is one value, not a set of
/// things to accumulate.
pub fn overlay(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = b;
            for (k, v) in o {
                if v.is_null() {
                    out.remove(&k);
                    continue;
                }
                let merged = match out.remove(&k) {
                    Some(existing) => self::overlay(existing, v),
                    None => v,
                };
                out.insert(k, merged);
            }
            Value::Object(out)
        }
        (Value::Array(b), Value::Array(o)) => match name_key(&b).or_else(|| name_key(&o)) {
            Some(key) if all_named(&b, key) && all_named(&o, key) => {
                Value::Array(merge_named(b, o, key))
            }
            _ => Value::Array(o),
        },
        // A scalar, or a type change: the overlay wins. A type change is a
        // policy bug rather than a merge case, and `plan` catches it when the
        // merged document fails to deserialize.
        (_, o) => o,
    }
}

/// Which name key a list of objects uses, if it uses one consistently.
fn name_key(items: &[Value]) -> Option<&'static str> {
    NAME_KEYS
        .iter()
        .copied()
        .find(|k| !items.is_empty() && items.iter().all(|i| has_string(i, k)))
}

fn all_named(items: &[Value], key: &str) -> bool {
    items.iter().all(|i| has_string(i, key))
}

fn has_string(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_str).is_some()
}

/// Merge two named lists: base order preserved, overlay entries merged into
/// their namesakes, new entries appended in overlay order.
///
/// Order is preserved rather than sorted because it is the operator's, and a
/// diff of a rendered document should look like the file that produced it.
fn merge_named(base: Vec<Value>, overlay_items: Vec<Value>, key: &str) -> Vec<Value> {
    let name_of = |v: &Value| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();

    let mut out: Vec<Value> = Vec::with_capacity(base.len() + overlay_items.len());
    let mut by_name: std::collections::BTreeMap<String, usize> = Default::default();
    for item in base {
        by_name.insert(name_of(&item), out.len());
        out.push(item);
    }
    for item in overlay_items {
        let n = name_of(&item);
        // `null` inside a named list is not a deletion: an object with a name
        // is never null. Removing an inherited entry is done by overlaying it
        // with the field set to null, or by not inheriting the class.
        match by_name.get(&n) {
            Some(&i) => {
                let existing = std::mem::replace(&mut out[i], Value::Null);
                out[i] = self::overlay(existing, item);
            }
            None => {
                by_name.insert(n, out.len());
                out.push(item);
            }
        }
    }
    out
}

/// Serialize a document to **canonical** JSON: object keys sorted, no
/// incidental whitespace.
///
/// This is what makes "publish only on a real change" true rather than
/// approximately true. `serde_json::Map` preserves insertion order unless the
/// `preserve_order` feature is off — and the insertion order of a merged
/// document depends on which class contributed a key first, which depends on
/// the file. Two policies that mean the same thing would otherwise produce
/// different bytes, and every host in the fleet would take a write.
pub fn canonical(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: Map<String, Value> = m
                    .iter()
                    .map(|(k, val)| (k.clone(), sort(val)))
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .collect();
                Value::Object(sorted)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort(v)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_merge_recursively() {
        let out = overlay(
            json!({"a": {"x": 1, "y": 2}, "b": 1}),
            json!({"a": {"y": 3, "z": 4}}),
        );
        assert_eq!(out, json!({"a": {"x": 1, "y": 3, "z": 4}, "b": 1}));
    }

    /// Without this there is no way for a later class or a host override to
    /// remove an inherited field — the only escape would be not to use the
    /// class.
    #[test]
    fn null_deletes() {
        let out = overlay(json!({"a": 1, "b": 2}), json!({"b": null}));
        assert_eq!(out, json!({"a": 1}));
    }

    /// The rule that stops a class hierarchy from silently dropping rules.
    /// With list replacement, `hypervisors` adding one expectation would drop
    /// the twelve `all-hosts` contributed — a well-formed document the sensor
    /// accepts, with twelve conditions no longer watched.
    #[test]
    fn named_lists_merge_rather_than_replace() {
        let base = json!({"sockets": [
            {"name": "sshd", "listen": 22, "min": 1},
            {"name": "https", "listen": 443, "min": 1}
        ]});
        let over = json!({"sockets": [
            {"name": "https", "min": 2},
            {"name": "pve", "listen": 8006, "min": 1}
        ]});
        let out = overlay(base, over);
        let list = out["sockets"].as_array().unwrap();
        assert_eq!(list.len(), 3, "extended, not replaced");
        assert_eq!(list[0]["name"], "sshd", "base order is preserved");
        assert_eq!(list[1]["min"], 2, "the namesake was adjusted...");
        assert_eq!(list[1]["listen"], 443, "...without losing its other fields");
        assert_eq!(list[2]["name"], "pve", "and the new one is appended");
    }

    /// Log rules spell the name `id`, and must merge the same way.
    #[test]
    fn named_lists_also_key_on_id() {
        let out = overlay(
            json!({"rules": [{"id": "oom", "severity": "warning"}]}),
            json!({"rules": [{"id": "oom", "severity": "critical"}]}),
        );
        assert_eq!(out["rules"].as_array().unwrap().len(), 1);
        assert_eq!(out["rules"][0]["severity"], "critical");
    }

    /// A list of scalars is one value, not a set of things to accumulate.
    #[test]
    fn unnamed_lists_replace() {
        let out = overlay(
            json!({"expect_status": [200, 204]}),
            json!({"expect_status": [200]}),
        );
        assert_eq!(out, json!({"expect_status": [200]}));
    }

    /// Canonical bytes are what make "publish only on a change" true rather
    /// than approximately true.
    #[test]
    fn canonical_json_is_key_order_independent() {
        let a = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let b = json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(canonical(&a), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    /// Array order is NOT canonicalised: it is the operator's, and for a
    /// merged named list it is base-then-new, which a rendered document
    /// should show as written.
    #[test]
    fn canonical_json_does_not_reorder_arrays() {
        assert_eq!(canonical(&json!({"a": [3, 1, 2]})), r#"{"a":[3,1,2]}"#);
    }
}

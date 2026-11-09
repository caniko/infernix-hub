//! Versioned canonical payload hashing for frozen release actions.

use serde_json::Value;
use sha2::{Digest, Sha256};

pub const RELEASE_HASH_VERSION: &str = "hub-release-v1";

/// Canonical JSON: object keys sorted, no whitespace, arrays in order.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `hub-release-v1:<sha256>` over the canonical actions array.
pub fn release_hash(actions: &[contracts::ReleaseAction]) -> String {
    let v = serde_json::to_value(actions).expect("release actions serialize");
    let doc = serde_json::json!({ "v": RELEASE_HASH_VERSION, "actions": v });
    format!("{RELEASE_HASH_VERSION}:{}", sha256_hex(canonical_json(&doc).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use contracts::{MediaRef, ReleaseAction, TimingScope};
    use serde_json::json;

    fn action(text: &str) -> ReleaseAction {
        ReleaseAction {
            company_id: "c1".into(),
            account_id: "acct".into(),
            text: text.into(),
            media: vec![MediaRef { asset_id: uuid::Uuid::nil(), sha256: "ab".into(), mime: "image/png".into(), alt_text: "alt".into() }],
            platform_settings: json!({"b": 1, "a": [2, {"z": 1, "y": 2}]}),
            reply_to: None,
            timing: TimingScope {
                not_before: chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                not_after: chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            },
        }
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": {"d": [1, {"f": 0, "e": 1}], "c": "x"}})),
            r#"{"a":{"c":"x","d":[1,{"e":1,"f":0}]},"b":1}"#
        );
    }

    #[test]
    fn hash_is_stable_and_sensitive_to_every_field() {
        let a = action("hello");
        assert_eq!(release_hash(std::slice::from_ref(&a)), release_hash(&[action("hello")]));
        assert!(release_hash(std::slice::from_ref(&a)).starts_with("hub-release-v1:"));
        assert_ne!(release_hash(std::slice::from_ref(&a)), release_hash(&[action("hello!")]));
        let mut b = a.clone();
        b.media[0].alt_text = "other".into();
        assert_ne!(release_hash(std::slice::from_ref(&a)), release_hash(&[b]));
        let mut c = a.clone();
        c.timing.not_after = c.timing.not_before;
        assert_ne!(release_hash(&[a]), release_hash(&[c]));
    }
}

//! The shell's records as JSON (helixdesktop/spec
//! `contracts/agent-access/cli.md`, "Output"): integers and doubles become
//! numbers, strings and string lists stay, byte arrays become base64, and
//! keys the CLI does not know are passed through.

use std::collections::HashMap;

use serde_json::{Map, Value as Json};
use zbus::zvariant::{OwnedValue, Value};

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn value(v: &Value<'_>) -> Json {
    match v {
        Value::U8(n) => Json::from(*n),
        Value::Bool(b) => Json::from(*b),
        Value::I16(n) => Json::from(*n),
        Value::U16(n) => Json::from(*n),
        Value::I32(n) => Json::from(*n),
        Value::U32(n) => Json::from(*n),
        Value::I64(n) => Json::from(*n),
        Value::U64(n) => Json::from(*n),
        Value::F64(n) => serde_json::Number::from_f64(*n).map_or(Json::Null, Json::Number),
        Value::Str(s) => Json::from(s.as_str()),
        Value::Signature(s) => Json::from(s.to_string()),
        Value::ObjectPath(p) => Json::from(p.as_str()),
        Value::Value(inner) => value(inner),
        Value::Array(a) => {
            if a.element_signature() == "y" {
                let bytes: Vec<u8> = a.iter().filter_map(|b| if let Value::U8(b) = b { Some(*b) } else { None }).collect();
                Json::from(base64(&bytes))
            } else {
                Json::Array(a.iter().map(value).collect())
            }
        }
        Value::Dict(d) => {
            let mut out = Map::new();
            for (k, v) in d.iter() {
                let key = match k {
                    Value::Str(s) => s.to_string(),
                    other => value(other).to_string(),
                };
                out.insert(key, value(v));
            }
            Json::Object(out)
        }
        Value::Structure(s) => Json::Array(s.fields().iter().map(value).collect()),
        #[allow(unreachable_patterns)]
        _ => Json::Null,
    }
}

/// An `a{sv}` record.
pub fn record(map: &HashMap<String, OwnedValue>) -> Json {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    Json::Object(keys.into_iter().map(|k| (k.clone(), value(&map[k]))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn records() {
        let mut map: HashMap<String, OwnedValue> = HashMap::new();
        map.insert("id".into(), OwnedValue::from(7u32));
        map.insert("title".into(), Value::from("Notes").try_into().unwrap());
        map.insert("scale".into(), OwnedValue::from(1.5f64));
        map.insert("png".into(), Value::from(vec![1u8, 2, 3]).try_into().unwrap());
        map.insert("states".into(), Value::from(vec!["focused", "visible"]).try_into().unwrap());
        let json = record(&map);
        assert_eq!(json, serde_json::json!({"id": 7, "title": "Notes", "scale": 1.5, "png": "AQID", "states": ["focused", "visible"]}));
    }
}

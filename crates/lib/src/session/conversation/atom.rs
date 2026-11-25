//! The valid CRDT domain is a non-atomic Doc of immutable (hash, JSON bytes)
//! atoms. Arbitrary Doc mutation does not have these merge laws.
use eidetica::crdt::Doc;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

use super::{ConversationError, Result};

pub(super) fn decode_doc(bytes: &[u8]) -> Result<Doc> {
    // Doc's map deserializer otherwise accepts repeated child keys. Validate
    // the exact signed envelope before the native codec can hide a duplicate.
    let _: UniqueValue = serde_json::from_slice(bytes)?;
    use eidetica::crdt::Codec;
    Ok(Doc::decode(bytes)?)
}

pub(super) fn key(text: &str) -> String {
    format!("h{}", blake3::hash(text.as_bytes()).to_hex())
}

pub(super) fn atoms(doc: &Doc) -> Result<BTreeMap<String, String>> {
    // Doc::iter and len both hide tombstones. Its lossless serde envelope
    // includes them; compare that count so deletion of an absent key also fails.
    let envelope = serde_json::to_value(doc)?;
    let all_len = envelope
        .get("children")
        .and_then(Value::as_object)
        .ok_or_else(|| ConversationError::Integrity("unsupported Doc envelope".into()))?
        .len();
    if doc.is_atomic() || all_len != doc.len() {
        return Err(ConversationError::Integrity(
            "atomic Doc or tombstone".into(),
        ));
    }
    doc.iter()
        .map(|(hash, value)| {
            let text = value.as_text().ok_or_else(|| {
                ConversationError::Integrity("conversation atom is not text".into())
            })?;
            if hash != &key(text) {
                return Err(ConversationError::Integrity("atom digest mismatch".into()));
            }
            Ok((hash.clone(), text.into()))
        })
        .collect()
}

pub(super) fn encode<T: Serialize>(record: &T) -> Result<String> {
    Ok(canonical_json(serde_json::to_value(record)?)?)
}

/// serde_json's map feature selection is workspace-wide. Explicitly sort
/// recursively rather than relying on whether preserve_order is enabled.
pub(crate) fn canonical_json(value: Value) -> serde_json::Result<String> {
    fn ordered(value: Value) -> Value {
        match value {
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values.into_iter().collect();
                Value::Object(sorted.into_iter().map(|(k, v)| (k, ordered(v))).collect())
            }
            Value::Array(values) => Value::Array(values.into_iter().map(ordered).collect()),
            other => other,
        }
    }
    serde_json::to_string(&ordered(value))
}

pub(super) fn decode_v1<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let unique: UniqueValue = serde_json::from_str(text)?;
    let version = unique
        .0
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| ConversationError::Malformed("record version".into()))?;
    if version != 1 {
        return Err(ConversationError::Unsupported(format!("record v{version}")));
    }
    Ok(serde_json::from_value(unique.0)?)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    // Value's usual deserializer silently accepts duplicate object keys,
    // including keys hidden inside opaque provider observations.
    let unique: UniqueValue = serde_json::from_str(text)?;
    Ok(serde_json::from_value(unique.0)?)
}

struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite JSON number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<UniqueValue>()? {
                    values.push(v.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(key, value.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

//! Session settings v1: native structural fields, JSON at each semantic value.
//!
//! Conversion is an explicit, offline operation. Never lazily copy a retained
//! object into a concurrent patch: two such conversions would overwrite fields.

use super::EXTENSION_SETTINGS_STORE;
use anyhow::Context;
use eidetica::crdt::{Doc, doc::Value as NativeValue};
use eidetica::store::DocStore;
use eidetica::{Database, Transaction};
use serde_json::Value;

// Doc interprets dots as paths. Encode literal JSON keys (including empty keys)
// without constraining the settings API's existing key space.
fn field_key(key: &str) -> String {
    let mut encoded = String::from("k");
    use std::fmt::Write;
    for byte in key.bytes() {
        write!(encoded, "{byte:02x}").expect("String write");
    }
    encoded
}

fn decode_field_key(key: &str) -> anyhow::Result<String> {
    let hex = key
        .strip_prefix('k')
        .ok_or_else(|| anyhow::anyhow!("invalid settings v1 field"))?;
    anyhow::ensure!(
        hex.len().is_multiple_of(2) && hex.is_ascii(),
        "invalid settings v1 field"
    );
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(String::from_utf8(bytes)?)
}

fn decode(value: NativeValue) -> anyhow::Result<Value> {
    match value {
        NativeValue::Text(json) => Ok(serde_json::from_str(&json)?),
        NativeValue::Doc(fields) => {
            let mut object = serde_json::Map::new();
            for (key, value) in fields.iter() {
                let NativeValue::Text(json) = value else {
                    anyhow::bail!("settings v1 field must contain JSON text");
                };
                let key = decode_field_key(key)?;
                anyhow::ensure!(!object.contains_key(&key), "duplicate settings v1 field");
                object.insert(key, serde_json::from_str(json)?);
            }
            Ok(Value::Object(object))
        }
        _ => anyhow::bail!("extension settings must contain JSON text or a v1 document"),
    }
}

fn replacement(value: &Value) -> anyhow::Result<NativeValue> {
    if let Some(object) = value.as_object() {
        let mut fields = Doc::atomic();
        for (key, value) in object {
            fields.set(field_key(key), serde_json::to_string(value)?);
        }
        Ok(NativeValue::Doc(fields))
    } else {
        Ok(NativeValue::Text(serde_json::to_string(value)?))
    }
}

async fn stored(store: &DocStore, ext: &str) -> anyhow::Result<Option<NativeValue>> {
    match store.get(ext).await {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Absence or read failure gives defaults, preserving the lenient display API.
pub async fn read_settings(db: &Database, ext: &str) -> Value {
    read_settings_checked(db, ext)
        .await
        .unwrap_or_else(|_| serde_json::json!({}))
}

/// Absence gives defaults; corrupt/unavailable settings remain an error.
pub async fn read_settings_checked(db: &Database, ext: &str) -> anyhow::Result<Value> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    stored(&store, ext)
        .await?
        .map(decode)
        .transpose()
        .map(|v| v.unwrap_or_else(|| serde_json::json!({})))
}

/// Deliberate whole-value replacement, including removal of omitted fields.
pub async fn write_settings(db: &Database, ext: &str, value: Value) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await?
        .set(ext, replacement(&value)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Edit one coherent source snapshot; commit only the changed semantic fields.
/// `checked` retains the caller's strict versus lenient corruption policy.
/// A nested object or list is one value, not recursively merged JSON.
pub(crate) async fn update_settings<R>(
    db: &Database,
    ext: &str,
    checked: bool,
    edit: impl FnOnce(&mut Value) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let txn = db.new_transaction().await?;
    let (result, changed) = update_settings_in(&txn, ext, checked, edit).await?;
    if changed {
        txn.commit().await?;
    }
    Ok(result)
}

pub(super) async fn update_settings_in<R>(
    txn: &Transaction,
    ext: &str,
    checked: bool,
    edit: impl FnOnce(&mut Value) -> anyhow::Result<R>,
) -> anyhow::Result<(R, bool)> {
    let store = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    let raw = stored(&store, ext).await?;
    let retained_object = matches!(&raw, Some(NativeValue::Text(json)) if serde_json::from_str::<Value>(json).is_ok_and(|v| v.is_object()));
    anyhow::ensure!(
        !retained_object,
        "retained extension settings require offline conversion: /extensions migrate-settings --old-clients-stopped (Chaz >= 0.4.0 on every sharing client)"
    );
    let decoded = raw.map(decode).transpose();
    let corrupt = decoded.is_err();
    let before = match decoded {
        Ok(value) => value.unwrap_or_else(|| serde_json::json!({})),
        Err(error) if checked => return Err(error),
        Err(_) => serde_json::json!({}),
    };
    let mut after = before.clone();
    let result = edit(&mut after)?;
    if before == after {
        return Ok((result, false));
    }
    if corrupt {
        store.set(ext, replacement(&after)?).await?;
    } else if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        let mut delta = Doc::new();
        for (key, value) in after {
            if before.get(key) != Some(value) {
                delta.set(field_key(key), serde_json::to_string(value)?);
            }
        }
        for key in before.keys().filter(|key| !after.contains_key(*key)) {
            delta.set(field_key(key), NativeValue::Deleted);
        }
        if !delta.is_empty() {
            store.set_node(ext, delta).await?;
        }
    } else if before != after {
        store.set(ext, replacement(&after)?).await?;
    }
    Ok((result, true))
}

/// Offline retained conversion, all extensions in this session or none.
/// The operator must first stop/upgrade every old session-sharing client.
/// No startup, read or ordinary patch calls this function.
pub async fn migrate_settings(db: &Database) -> anyhow::Result<usize> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    let all = store.get_all().await?;
    let mut count = 0;
    for (ext, raw) in all.iter() {
        // Never turn corruption into defaults; identify the repair target.
        let value = decode(raw.clone())
            .with_context(|| format!("invalid settings for extension '{ext}'"))?;
        if matches!(raw, NativeValue::Text(_)) && value.is_object() {
            store.set(ext, replacement(&value)?).await?;
            count += 1;
        }
    }
    if count > 0 {
        txn.commit().await?;
    }
    Ok(count)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;

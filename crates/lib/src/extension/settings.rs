//! Native session settings. JSON is the host/API boundary, not the stored value.
//! Retained formats are read-only until explicit coordinated offline conversion.

use super::EXTENSION_SETTINGS_STORE;
use crate::native_doc::{WIDE_NUMBER as NUMBER, decode_field_key, field_key, unhex, wide_number};
use anyhow::Context;
use eidetica::crdt::{
    Doc,
    doc::{List, Value as NativeValue},
};
use eidetica::store::DocStore;
use eidetica::{Database, Store, Transaction};
use serde_json::Value;

const RETAINED_STORE: &str = "extension_settings";
// Reserved native keys cannot collide with application keys (which escape '~').
const ARRAY: &str = "~array";

pub(super) fn encode(value: &Value) -> NativeValue {
    match value {
        Value::Null => NativeValue::Null,
        Value::Bool(v) => NativeValue::Bool(*v),
        Value::String(v) => NativeValue::Text(v.clone()),
        Value::Number(v) => match v.as_i64() {
            Some(v) => NativeValue::Int(v),
            None => {
                // Native Value has only i64: From<f64> truncates and From<u64>
                // wraps. A tagged decimal preserves these unsupported numbers.
                NativeValue::Doc(wide_number(v))
            }
        },
        Value::Array(values) => {
            let mut list = List::new();
            for value in values {
                list.push(encode(value));
            }
            // A settings list is a complete semantic value, not positional edits.
            // List has no atomic replacement flag; the native Doc supplies it.
            let mut doc = Doc::atomic();
            doc.set(ARRAY, list);
            NativeValue::Doc(doc)
        }
        Value::Object(fields) => {
            let mut doc = Doc::atomic();
            for (key, value) in fields {
                doc.set(field_key(key), encode(value));
            }
            NativeValue::Doc(doc)
        }
    }
}

pub(super) fn decode(value: &NativeValue) -> anyhow::Result<Value> {
    Ok(match value {
        NativeValue::Null => Value::Null,
        NativeValue::Bool(v) => Value::Bool(*v),
        NativeValue::Int(v) => Value::Number((*v).into()),
        NativeValue::Text(v) => Value::String(v.clone()),
        NativeValue::Doc(doc) if doc.get(ARRAY).is_some() || doc.get(NUMBER).is_some() => {
            anyhow::ensure!(
                doc.is_atomic() && doc.len() == 1,
                "invalid settings value wrapper"
            );
            if let Some(NativeValue::List(list)) = doc.get(ARRAY) {
                Value::Array(list.iter().map(decode).collect::<Result<_, _>>()?)
            } else if let Some(NativeValue::Text(decimal)) = doc.get(NUMBER) {
                let number: serde_json::Number =
                    decimal.parse().context("invalid settings number")?;
                anyhow::ensure!(number.as_i64().is_none(), "noncanonical settings number");
                // The default JSON parser validates syntax but can round a
                // float's shortest decimal to a neighboring binary value.
                let number = if number.is_f64() {
                    serde_json::Number::from_f64(decimal.parse()?)
                        .context("invalid settings float")?
                } else {
                    number
                };
                Value::Number(number)
            } else {
                anyhow::bail!("invalid settings value wrapper");
            }
        }
        NativeValue::Doc(fields) => {
            let mut object = serde_json::Map::new();
            for (key, value) in fields.iter() {
                object.insert(decode_field_key(key)?, decode(value)?);
            }
            Value::Object(object)
        }
        _ => anyhow::bail!("invalid native settings value"),
    })
}

// Only the two historical formats: scalar JSON and PR216's hex/JSON-leaf Doc.
// Never write them or lazily convert them from an ordinary edit.
fn decode_retained(value: &NativeValue) -> anyhow::Result<Value> {
    match value {
        NativeValue::Text(json) => Ok(serde_json::from_str(json)?),
        NativeValue::Doc(fields) => {
            let mut object = serde_json::Map::new();
            for (key, value) in fields.iter() {
                let key = unhex(
                    key.strip_prefix('k')
                        .context("invalid retained settings key")?,
                )?;
                let NativeValue::Text(json) = value else {
                    anyhow::bail!("retained settings field must contain JSON text");
                };
                anyhow::ensure!(
                    !object.contains_key(&key),
                    "duplicate retained settings field"
                );
                object.insert(key, serde_json::from_str(json)?);
            }
            Ok(Value::Object(object))
        }
        _ => anyhow::bail!("invalid retained extension settings"),
    }
}

pub(super) async fn stored(store: &DocStore, field: &str) -> anyhow::Result<Option<NativeValue>> {
    match store.get(field).await {
        Ok(value) => Ok(Some(value)),
        // Backend EntryNotFound is an unreadable history, not an absent key.
        Err(error)
            if matches!(&error, eidetica::Error::Store(inner)
            if matches!(inner.as_ref(), eidetica::store::StoreError::KeyNotFound { store: name, key }
                if name == store.name() && key == field)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn read_in(txn: &Transaction, ext: &str) -> anyhow::Result<(Value, bool)> {
    let native = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    if let Some(value) = stored(&native, &field_key(ext)).await? {
        return Ok((decode(&value)?, false));
    }
    let retained = txn.get_store::<DocStore>(RETAINED_STORE).await?;
    // Historical extension names were path-interpreted by DocStore too.
    match stored(&retained, ext).await? {
        Some(value) => Ok((decode_retained(&value)?, true)),
        None => Ok((serde_json::json!({}), false)),
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
    Ok(read_in(&txn, ext).await?.0)
}

/// Deliberate whole-value replacement, including removal of omitted fields.
pub async fn write_settings(db: &Database, ext: &str, value: Value) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await?
        .set(field_key(ext), encode(&value))
        .await?;
    txn.commit().await?;
    Ok(())
}

/// Translate changed object paths into native structural deltas. Eidetica alone
/// composes history; this does not merge competing JSON snapshots in Chaz.
fn delta(before: &Value, after: &Value, recursive: bool) -> NativeValue {
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        let mut doc = Doc::new();
        for (key, value) in after {
            if before.get(key) != Some(value) {
                let value = match before.get(key) {
                    Some(old) if recursive => delta(old, value, true),
                    // Independent additions can share a previously absent parent.
                    // Only assignment/replacement APIs make that parent atomic.
                    None if recursive && value.is_object() => {
                        delta(&serde_json::json!({}), value, true)
                    }
                    _ => encode(value),
                };
                doc.set(field_key(key), value);
            }
        }
        for key in before.keys().filter(|key| !after.contains_key(*key)) {
            doc.set(field_key(key), NativeValue::Deleted);
        }
        // is_empty() hides tombstones; callers compare source values instead.
        NativeValue::Doc(doc)
    } else {
        encode(after)
    }
}

/// Edit a coherent snapshot; write only changed paths. Lists remain atomic.
pub(crate) async fn update_settings<R>(
    db: &Database,
    ext: &str,
    checked: bool,
    edit: impl FnOnce(&mut Value) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let txn = db.new_transaction().await?;
    let (result, changed) = update_settings_in(&txn, ext, checked, true, edit).await?;
    if changed {
        txn.commit().await?;
    }
    Ok(result)
}

/// Explicit top-level field assignment, not recursive object patching. A nested
/// value supplied by /extensions set or a turn patch replaces that whole field.
pub(crate) async fn patch_settings(
    db: &Database,
    ext: &str,
    checked: bool,
    patch: serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let (_, changed) = update_settings_in(&txn, ext, checked, false, |value| {
        if !checked && !value.is_object() {
            *value = serde_json::json!({});
        }
        value
            .as_object_mut()
            .context("extension settings must be an object")?
            .extend(patch);
        Ok(())
    })
    .await?;
    if changed {
        txn.commit().await?;
    }
    Ok(())
}

pub(super) async fn update_settings_in<R>(
    txn: &Transaction,
    ext: &str,
    checked: bool,
    recursive: bool,
    edit: impl FnOnce(&mut Value) -> anyhow::Result<R>,
) -> anyhow::Result<(R, bool)> {
    let decoded = read_in(txn, ext).await;
    let corrupt = decoded.is_err();
    let before = match decoded {
        Ok((_, true)) => anyhow::bail!(
            "retained extension settings require offline conversion: /extensions migrate-settings --old-clients-stopped (Chaz >= 0.4.0 on every sharing client)"
        ),
        Ok((value, false)) => value,
        Err(error) if checked => return Err(error),
        Err(_) => serde_json::json!({}),
    };
    let mut after = before.clone();
    let result = edit(&mut after)?;
    if before == after {
        return Ok((result, false));
    }
    let value = if corrupt {
        encode(&after)
    } else {
        delta(&before, &after, recursive)
    };
    txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE)
        .await?
        .set(field_key(ext), value)
        .await?;
    Ok((result, true))
}

/// Offline conversion of every retained override in this session or none.
/// All old session-sharing clients must be stopped/upgraded first. Old stores
/// remain as history; a native override takes precedence, never dual-written.
pub async fn migrate_settings(db: &Database) -> anyhow::Result<usize> {
    let txn = db.new_transaction().await?;
    let native = txn.get_store::<DocStore>(EXTENSION_SETTINGS_STORE).await?;
    let retained = txn
        .get_store::<DocStore>(RETAINED_STORE)
        .await?
        .get_all()
        .await?;
    let current = native.get_all().await?;
    for (key, value) in current.iter() {
        decode_field_key(key)?;
        decode(value).with_context(|| format!("invalid settings for extension '{key}'"))?;
    }
    let mut count = 0;
    for (ext, value) in retained.iter() {
        if current.get(field_key(ext)).is_some() {
            continue;
        }
        let value = decode_retained(value)
            .with_context(|| format!("invalid settings for extension '{ext}'"))?;
        native.set(field_key(ext), encode(&value)).await?;
        count += 1;
    }
    if count > 0 {
        txn.commit().await?;
    }
    Ok(count)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;

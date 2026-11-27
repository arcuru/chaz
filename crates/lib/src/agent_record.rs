//! Whole-record storage for Agent DB config and agent/bank display metadata.

use crate::native_doc::{WIDE_NUMBER, decode_field_key, field_key, wide_number};
use anyhow::{Context, ensure};
use eidetica::crdt::{Doc, doc::List, doc::Value};
use eidetica::store::{DocStore, StoreError};
use eidetica::{Database, Transaction};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value as Json};

pub(crate) const KEY: &str = "value";
const VERSION: i64 = 1;

/// A native record whose envelope this reader does not understand.
#[derive(Debug, thiserror::Error)]
#[error("missing or unsupported record version")]
pub(crate) struct UnsupportedVersion;

fn encode(value: Json) -> anyhow::Result<Value> {
    Ok(match value {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Bool(value),
        Json::String(value) => Value::Text(value),
        Json::Number(number) => match number.as_i64() {
            Some(value) => Value::Int(value),
            // The only wider numeric type in these records is usize.
            None if number.is_u64() => Value::Doc(wide_number(&number)),
            None => anyhow::bail!("unsupported number in agent or bank record"),
        },
        Json::Array(values) => {
            let mut list = List::new();
            for value in values {
                list.push(encode(value)?);
            }
            Value::List(list)
        }
        Json::Object(values) => {
            let mut doc = Doc::new();
            for (key, value) in values {
                doc.set(field_key(&key), encode(value)?);
            }
            Value::Doc(doc)
        }
    })
}

fn decode_fields(doc: &Doc) -> anyhow::Result<Map<String, Json>> {
    doc.iter()
        .map(|(key, value)| Ok((decode_field_key(key)?, decode(value)?)))
        .collect()
}

fn decode(value: &Value) -> anyhow::Result<Json> {
    Ok(match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int(value) => Json::Number((*value).into()),
        Value::Text(value) => Json::String(value.clone()),
        Value::List(list) => Json::Array(list.iter_all().map(decode).collect::<Result<_, _>>()?),
        Value::Doc(doc) => {
            if let Some(number) = doc.get(WIDE_NUMBER) {
                ensure!(doc.is_atomic() && doc.len() == 1, "invalid wide integer");
                let text = number
                    .as_text()
                    .context("expected a wide integer as text")?;
                let value: u64 = text.parse().context("invalid wide integer")?;
                ensure!(
                    i64::try_from(value).is_err() && value.to_string() == text,
                    "noncanonical wide integer"
                );
                Json::Number(value.into())
            } else {
                Json::Object(decode_fields(doc)?)
            }
        }
        Value::Deleted => anyhow::bail!("unexpected tombstone inside a complete list"),
    })
}

pub(crate) async fn read<T: DeserializeOwned + Default>(
    database: &Database,
    store_name: &str,
) -> anyhow::Result<T> {
    let txn = database.new_transaction().await?;
    read_in(&txn, store_name).await
}

/// One complete stored record, before its fields are read as any Rust type.
pub(crate) enum Stored {
    Text(String),
    Native(Map<String, Json>),
}

impl Stored {
    pub(crate) async fn read_in(
        txn: &Transaction,
        store_name: &str,
    ) -> anyhow::Result<Option<Self>> {
        let store = txn.get_store::<DocStore>(store_name).await?;
        match store.get(KEY).await {
            Ok(Value::Text(json)) => Ok(Some(Self::Text(json))),
            Ok(Value::Doc(record)) => {
                let decode_record = || -> anyhow::Result<Self> {
                    ensure!(record.is_atomic(), "record is not atomic");
                    ensure!(
                        record.get("version") == Some(&Value::Int(VERSION)),
                        UnsupportedVersion
                    );
                    ensure!(record.len() == 2, "invalid record envelope");
                    let fields = record
                        .get("fields")
                        .and_then(Value::as_doc)
                        .context("expected a document of record fields")?;
                    Ok(Self::Native(decode_fields(fields)?))
                };
                decode_record()
                    .map(Some)
                    .with_context(|| invalid_native(store_name))
            }
            Ok(other) => Err(eidetica::Error::from(StoreError::TypeMismatch {
                store: store_name.to_owned(),
                expected: "atomic Doc or legacy JSON Text".into(),
                actual: other.type_name().into(),
            })
            .into()),
            // A missing entry, broken history, or auth failure is not a missing key.
            Err(eidetica::Error::Store(error))
                if matches!(error.as_ref(), StoreError::KeyNotFound { store, key }
                    if store == store_name && key == KEY) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn decode<T: DeserializeOwned>(self, store_name: &str) -> anyhow::Result<T> {
        match self {
            Self::Text(json) => Ok(serde_json::from_str(&json)?),
            Self::Native(fields) => serde_json::from_value(Json::Object(fields))
                .with_context(|| invalid_native(store_name)),
        }
    }

    /// Identity of the stored logical record, including fields this reader
    /// does not know. `Doc` bytes are unsuitable: map order and list position
    /// IDs differ between equal writes. Changing this envelope needs a new
    /// `RecordRef` format.
    pub(crate) fn digest(&self) -> anyhow::Result<String> {
        let (kind, record) = match self {
            Self::Text(json) => ("text", json.as_str().into()),
            Self::Native(fields) => (
                "native",
                serde_json::json!({"version": VERSION, "fields": fields}),
            ),
        };
        let envelope = crate::session::canonical_json(serde_json::json!({
            "encoding": "chaz-agent-record",
            "version": 1,
            "kind": kind,
            "record": record,
        }))?;
        Ok(blake3::hash(envelope.as_bytes()).to_hex().to_string())
    }
}

fn invalid_native(store_name: &str) -> String {
    format!("invalid native {store_name} record")
}

pub(crate) async fn read_in<T: DeserializeOwned + Default>(
    txn: &Transaction,
    store_name: &str,
) -> anyhow::Result<T> {
    match Stored::read_in(txn, store_name).await? {
        Some(record) => record.decode(store_name),
        None => Ok(T::default()),
    }
}

pub(crate) async fn write<T: Serialize>(
    database: &Database,
    store_name: &str,
    value: &T,
) -> anyhow::Result<()> {
    let txn = database.new_transaction().await?;
    stage(&txn, store_name, value).await?;
    txn.commit().await?;
    Ok(())
}

pub(crate) async fn stage<T: Serialize>(
    txn: &Transaction,
    store_name: &str,
    value: &T,
) -> anyhow::Result<()> {
    let fields = serde_json::to_value(value)?;
    ensure!(fields.is_object(), "expected record fields");
    // Nothing inside this boundary is merged independently: no stale grant,
    // prompt source, or list member can survive a whole-record replacement.
    let mut record = Doc::atomic();
    record.set("version", VERSION);
    record.set("fields", encode(fields)?);
    txn.get_store::<DocStore>(store_name)
        .await?
        .set_node(KEY, record)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{AgentDb, AgentDbConfig, CONFIG_STORE};

    #[tokio::test]
    async fn concurrent_config_replacements_never_combine() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let agent = AgentDb::from_database(db.clone());
        agent.write_config(&AgentDbConfig::default()).await.unwrap();
        let base = db.snapshot().await.unwrap();
        let a: AgentDbConfig = serde_json::from_value(serde_json::json!({
            "system_prompt": "a",
            "system_prompt_files": ["a.md"],
            "prompt_sources": {"version": 1, "files": {"a.md": "source a"}},
            "tools": ["a", "b"],
            "presets": {"a": {}}
        }))
        .unwrap();
        let b: AgentDbConfig = serde_json::from_value(serde_json::json!({
            "system_prompt": "b",
            "system_prompt_files": ["b.md"],
            "prompt_sources": {"version": 1, "files": {"b.md": "source b"}},
            "tools": [],
            "presets": {"b": {}}
        }))
        .unwrap();
        let left = db.new_transaction_at(&base).await.unwrap();
        let right = db.new_transaction_at(&base).await.unwrap();
        stage(&left, CONFIG_STORE, &a).await.unwrap();
        stage(&right, CONFIG_STORE, &b).await.unwrap();
        let left_id = left.commit().await.unwrap();
        let right_id = right.commit().await.unwrap();
        let tips = db.snapshot().await.unwrap();
        assert_eq!(
            tips,
            [left_id, right_id].into(),
            "must exercise two sibling entries"
        );
        let observed = agent.read_config().await.unwrap();
        assert!(
            observed == a || observed == b,
            "mixed configuration: {observed:?}"
        );
    }

    #[tokio::test]
    async fn native_record_does_not_default_on_corruption() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let agent = AgentDb::from_database(db.clone());
        for version in [2i64, 1] {
            let txn = db.new_transaction().await.unwrap();
            let mut record = Doc::atomic();
            record.set("version", version);
            let mut fields = Doc::new();
            fields.set("autonomous", "not a bool");
            record.set("fields", fields);
            txn.get_store::<DocStore>(CONFIG_STORE)
                .await
                .unwrap()
                .set_node(KEY, record)
                .await
                .unwrap();
            txn.commit().await.unwrap();
            let error = format!("{:#}", agent.read_config().await.unwrap_err());
            let expected = if version == 1 {
                "expected a boolean"
            } else {
                "unsupported record version"
            };
            assert!(error.contains(expected), "{error}");
        }
    }

    async fn stored(db: &Database) -> Stored {
        let txn = db.new_transaction().await.unwrap();
        Stored::read_in(&txn, CONFIG_STORE).await.unwrap().unwrap()
    }

    async fn write_native(db: &Database, fields: &Json) -> (String, String) {
        write(db, CONFIG_STORE, fields).await.unwrap();
        let txn = db.new_transaction().await.unwrap();
        let store = txn.get_store::<DocStore>(CONFIG_STORE).await.unwrap();
        let raw = serde_json::to_string(&store.get(KEY).await.unwrap()).unwrap();
        (stored(db).await.digest().unwrap(), raw)
    }

    #[tokio::test]
    async fn digest_identifies_the_stored_logical_record() {
        let (_instance, _user, db) = crate::session::test_helpers::test_session_db().await;
        let fields = serde_json::json!({
            "system_prompt": "p",
            "tools": ["b", "a"],
            "max_context_tokens": u64::MAX,
            "presets": {"": {}, "a.b": {}, "~": {"model": null}},
            "not_known_to_this_reader": {"nested": [1, true, {}]},
        });
        // Golden: a change here needs a new reference format.
        let (golden, first) = write_native(&db, &fields).await;
        assert_eq!(
            golden,
            "25c9754c974a3c489df8cbebe420381e05a6767b5ef91db48dd35ed22ad88ac9"
        );
        let (again, second) = write_native(&db, &fields).await;
        assert_eq!(again, golden);
        assert_ne!(first, second, "stored Doc bytes are not canonical");

        for (pointer, value) in [
            ("/not_known_to_this_reader/nested/0", serde_json::json!(2)),
            ("/not_known_to_this_reader", serde_json::json!(null)),
            ("/max_context_tokens", serde_json::json!(i64::MAX)),
            ("/max_context_tokens", u64::MAX.to_string().into()),
            ("/tools", serde_json::json!(["a", "b"])),
            ("/presets/~0", serde_json::json!({})),
        ] {
            let mut changed = fields.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert_ne!(write_native(&db, &changed).await.0, golden, "{pointer}");
        }

        // Equal logical fields as legacy Text are a different record, and
        // Text is identified byte for byte.
        let text = crate::session::canonical_json(fields).unwrap();
        let mut digests = std::collections::BTreeSet::from([golden]);
        for text in [text.clone(), format!("{text}\n")] {
            let txn = db.new_transaction().await.unwrap();
            let store = txn.get_store::<DocStore>(CONFIG_STORE).await.unwrap();
            store.set_string(KEY, text).await.unwrap();
            txn.commit().await.unwrap();
            digests.insert(stored(&db).await.digest().unwrap());
        }
        assert_eq!(digests.len(), 3);
        assert_eq!(
            Stored::Text("{}".into()).digest().unwrap(),
            "25dda33b9c7d92c03555270472c796d034147833e52eeb616646226298ea14b9"
        );
    }
}

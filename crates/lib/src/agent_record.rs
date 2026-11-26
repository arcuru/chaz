//! Whole-record storage for Agent DB config and agent/bank display metadata.

use crate::native_doc::{WIDE_NUMBER, decode_field_key, field_key, wide_number};
use anyhow::{Context, ensure};
use eidetica::crdt::{Doc, doc::List, doc::Value};
use eidetica::store::{DocStore, StoreError};
use eidetica::{Database, Transaction};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value as Json};

const KEY: &str = "value";
const VERSION: i64 = 1;

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

pub(crate) async fn read_in<T: DeserializeOwned + Default>(
    txn: &Transaction,
    store_name: &str,
) -> anyhow::Result<T> {
    let store = txn.get_store::<DocStore>(store_name).await?;
    match store.get(KEY).await {
        Ok(Value::Text(json)) => Ok(serde_json::from_str(&json)?),
        Ok(Value::Doc(record)) => {
            let decode_record = || -> anyhow::Result<T> {
                ensure!(record.is_atomic(), "record is not atomic");
                ensure!(
                    record.get("version") == Some(&Value::Int(VERSION)),
                    "missing or unsupported record version"
                );
                ensure!(record.len() == 2, "invalid record envelope");
                let fields = record
                    .get("fields")
                    .and_then(Value::as_doc)
                    .context("expected a document of record fields")?;
                Ok(serde_json::from_value(Json::Object(decode_fields(
                    fields,
                )?))?)
            };
            decode_record().with_context(|| format!("invalid native {store_name} record"))
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
            Ok(T::default())
        }
        Err(error) => Err(error.into()),
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
}

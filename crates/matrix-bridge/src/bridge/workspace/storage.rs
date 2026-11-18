//! One coherent workspace snapshot per login, in the never-synced bridge peer DB.

use anyhow::{Context, bail, ensure};
use eidetica::Database;
use eidetica::crdt::{Doc, doc::Value};
use eidetica::store::{DocStore, StoreError};
use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};

use super::WorkspaceState;

pub(super) const STORE: &str = "matrix_workspace";
const VERSION: i64 = 1;

#[derive(Clone)]
pub(crate) struct WorkspaceStore {
    pub(super) db: Database,
    pub(super) key: String,
}

impl WorkspaceStore {
    pub(crate) fn new(db: Database, login_id: &str) -> Self {
        // Keep the existing login key; DocStore would treat dots as paths.
        let digest = Sha256::digest(login_id.as_bytes());
        let key: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        Self {
            db,
            key: format!("ws-{key}"),
        }
    }

    pub(super) async fn load(&self) -> anyhow::Result<Option<WorkspaceState>> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        let value = match store.get(&self.key).await {
            Ok(value) => value,
            Err(e)
                if matches!(&e, eidetica::Error::Store(err)
                if matches!(err.as_ref(), StoreError::KeyNotFound { .. })) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        match value {
            Value::Text(json) => serde_json::from_str(&json).map_err(Into::into),
            Value::Doc(record) => decode_record(&record),
            _ => bail!("unrecognized workspace record type; refusing to guess"),
        }
        .map(Some)
        .context("workspace record is malformed or unsupported; refusing to guess")
    }

    pub(super) async fn save(&self, state: &WorkspaceState) -> anyhow::Result<()> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        let Value::Doc(fields) = to_native(serde_json::to_value(state)?)? else {
            bail!("workspace state must be a record");
        };
        // Replacing only individual fields could mix progress, identities and
        // operation counters from different snapshots. The lists are inside
        // this atomic replacement; they are never independently merged.
        let mut record = Doc::atomic();
        record.set("version", VERSION);
        record.set("state", fields);
        store.set(&self.key, record).await?;
        txn.commit().await?;
        Ok(())
    }
}

fn decode_record(record: &Doc) -> anyhow::Result<WorkspaceState> {
    ensure!(record.is_atomic(), "workspace record is not atomic");
    ensure!(
        record.get("version") == Some(&Value::Int(VERSION)),
        "unsupported workspace record version"
    );
    let state = record.get("state").context("missing workspace state")?;
    let fields = state.as_doc().context("workspace state must be a record")?;
    // These were defaultable for the legacy JSON format. A native snapshot
    // must carry its room IDs, operation journal and counters explicitly.
    for name in [
        "space",
        "lobby",
        "chats",
        "operations",
        "next_op",
        "next_chat",
    ] {
        ensure!(fields.get(name).is_some(), "missing workspace field {name}");
    }
    Ok(serde_json::from_value(from_native(state)?)?)
}

// Serde's structured value is only an in-memory adapter for the typed state;
// the durable value contains native Doc, List and scalar nodes, not JSON text.
fn to_native(value: Json) -> anyhow::Result<Value> {
    Ok(match value {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Bool(value),
        Json::String(value) => Value::Text(value),
        Json::Array(items) => Value::List(
            items
                .into_iter()
                .map(to_native)
                .collect::<anyhow::Result<_>>()?,
        ),
        Json::Object(fields) => {
            let mut doc = Doc::new();
            for (name, value) in fields {
                let value = match (name.as_str(), value) {
                    // Step identifiers (e.g. "space.create") are literal data,
                    // not DocStore paths. Persist ordered (name, step) rows.
                    ("steps", Json::Object(steps)) => to_native(Json::Array(
                        steps
                            .into_iter()
                            .map(|(name, step)| Json::Array(vec![Json::String(name), step]))
                            .collect(),
                    ))?,
                    // The pin only has signed i64; keep the full legacy u64
                    // range in the two known counters without wrapping it.
                    ("next_op" | "next_chat", Json::Number(n)) if n.as_i64().is_none() => {
                        Value::Text(n.as_u64().context("invalid workspace counter")?.to_string())
                    }
                    (_, value) => to_native(value)?,
                };
                ensure!(!name.contains('.'), "unexpected dotted workspace field");
                doc.set(name, value);
            }
            Value::Doc(doc)
        }
        Json::Number(n) => Value::Int(n.as_i64().context("unsupported workspace number")?),
    })
}

fn from_native(value: &Value) -> anyhow::Result<Json> {
    Ok(match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Text(value) => Json::String(value.clone()),
        Value::Int(value) => (*value).into(),
        Value::List(items) => Json::Array(
            items
                .iter_all()
                .map(from_native)
                .collect::<anyhow::Result<_>>()?,
        ),
        Value::Doc(doc) => {
            let mut fields = Map::new();
            for (name, value) in doc.iter() {
                let value = match (name.as_str(), value) {
                    ("steps", value) => decode_steps(value)?,
                    ("next_op" | "next_chat", Value::Text(text)) => {
                        let n: u64 = text.parse().context("invalid workspace counter")?;
                        ensure!(
                            i64::try_from(n).is_err() && n.to_string() == *text,
                            "noncanonical workspace counter"
                        );
                        n.into()
                    }
                    (_, value) => from_native(value)?,
                };
                fields.insert(name.clone(), value);
            }
            Json::Object(fields)
        }
        Value::Deleted => bail!("workspace snapshot contains a tombstone"),
    })
}

fn decode_steps(value: &Value) -> anyhow::Result<Json> {
    let mut steps = Map::new();
    for row in value
        .as_list()
        .context("invalid workspace steps")?
        .iter_all()
    {
        let mut pair = row
            .as_list()
            .context("invalid workspace step row")?
            .iter_all();
        let (Some(Value::Text(name)), Some(step), None) = (pair.next(), pair.next(), pair.next())
        else {
            bail!("invalid workspace step row");
        };
        ensure!(
            steps.insert(name.clone(), from_native(step)?).is_none(),
            "duplicate workspace step"
        );
    }
    Ok(Json::Object(steps))
}

//! Native session settings. JSON is the host/API boundary, not the stored value.
//! Retained formats are read-only until explicit coordinated offline conversion.

use super::{EXTENSION_SETTINGS_STORE, Extension, ExtensionHub};
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

/// Accepted JSON types for public extension preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Boolean,
    NullableModel,
    Unsigned { min: u64, max: u64 },
}

impl SettingKind {
    pub fn validate(self, value: &Value) -> anyhow::Result<()> {
        match self {
            Self::Boolean => anyhow::ensure!(value.is_boolean(), "expected a boolean"),
            Self::NullableModel => anyhow::ensure!(
                value.is_null() || value.as_str().is_some_and(nonblank_line),
                "expected a nonblank single-line model identifier or null"
            ),
            Self::Unsigned { min, max } => anyhow::ensure!(
                value.as_u64().is_some_and(|n| (min..=max).contains(&n)),
                "expected an unsigned integer from {min} to {max}"
            ),
        }
        Ok(())
    }
}

fn nonblank_line(text: &str) -> bool {
    !text.trim().is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
}

/// Which layers may be edited. The declaration does not grant write access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingTargets {
    Agent,
    Session,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingField {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: SettingKind,
    pub targets: SettingTargets,
}

/// Validated registration snapshot, available even without an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSettingFields {
    fields: &'static [SettingField],
    defaults: serde_json::Map<String, Value>,
}

impl RegisteredSettingFields {
    pub(super) fn validate(ext: &dyn Extension) -> anyhow::Result<Option<Self>> {
        let fields = ext.setting_fields();
        if fields.is_empty() {
            return Ok(None);
        }
        let Value::Object(mut defaults) = ext.default_settings() else {
            anyhow::bail!("default_settings must be an object for declared fields");
        };
        let mut saved = serde_json::Map::new();
        for field in fields {
            let key = field.key;
            anyhow::ensure!(
                !key.is_empty() && !key.chars().any(|c| c.is_whitespace() || c.is_control()),
                "invalid setting key {key:?}"
            );
            anyhow::ensure!(!saved.contains_key(key), "duplicate setting key {key:?}");
            anyhow::ensure!(
                nonblank_line(field.label) && nonblank_line(field.help),
                "setting {key:?} needs a nonblank single-line label and help"
            );
            if let SettingKind::Unsigned { min, max } = field.kind {
                anyhow::ensure!(min <= max, "invalid unsigned bounds for setting {key:?}");
            }
            let value = defaults
                .remove(key)
                .with_context(|| format!("missing default for setting {key:?}"))?;
            field
                .kind
                .validate(&value)
                .with_context(|| format!("invalid default for setting {key:?}"))?;
            saved.insert(key.to_owned(), value);
        }
        Ok(Some(Self {
            fields,
            defaults: saved,
        }))
    }

    pub fn fields(&self) -> &'static [SettingField] {
        self.fields
    }

    pub fn field(&self, key: &str) -> Option<&'static SettingField> {
        self.fields.iter().find(|field| field.key == key)
    }

    /// `Some(Value::Null)` is a declared null default, not an absent field.
    pub fn default_value(&self, key: &str) -> Option<&Value> {
        self.defaults.get(key)
    }
}

pub enum SettingEdit {
    Set(Value),
    Reset,
}

/// The layer a declared field may be edited at, or a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingLayer {
    Session,
    Agent,
    Default,
}

impl SettingTargets {
    pub fn allows(self, layer: SettingLayer) -> bool {
        matches!(
            (self, layer),
            (Self::Both, SettingLayer::Session | SettingLayer::Agent)
                | (Self::Session, SettingLayer::Session)
                | (Self::Agent, SettingLayer::Agent)
        )
    }
}

/// One declared field for display. Only declared values are returned.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingRow {
    pub field: &'static SettingField,
    pub value: Value,
    pub source: SettingLayer,
    /// The edit target has its own value, so Reset is meaningful.
    pub present_at_target: bool,
    /// The declaration and current database view permit editing at the target.
    /// The actual commit rechecks authority.
    pub editable: bool,
}

/// Agent preferences live beside, never inside, the atomic `config["value"]`
/// record, so an old whole-record writer cannot erase them.
fn agent_key(ext: &str) -> String {
    format!("extension:{}", field_key(ext))
}

async fn read_agent_in(txn: &Transaction, ext: &str) -> anyhow::Result<Value> {
    let store = txn
        .get_store::<DocStore>(crate::agent_db::CONFIG_STORE)
        .await?;
    match stored(&store, &agent_key(ext)).await? {
        Some(value) => {
            let value = decode(&value)?;
            anyhow::ensure!(
                value.is_object(),
                "agent extension settings must be an object"
            );
            Ok(value)
        }
        None => Ok(serde_json::json!({})),
    }
}

/// Explicit agent preferences for one extension; absent gives an empty object.
pub async fn read_agent_settings(agent_db: &Database, ext: &str) -> anyhow::Result<Value> {
    let txn = agent_db.new_transaction().await?;
    read_agent_in(&txn, ext).await
}

/// First present value: session field, then agent preference, then default.
fn resolve(
    declaration: &RegisteredSettingFields,
    session: &Value,
    agent: &Value,
    field: &SettingField,
) -> (Value, SettingLayer) {
    if let Some(value) = session.get(field.key) {
        (value.clone(), SettingLayer::Session)
    } else if let Some(value) = agent
        .get(field.key)
        .filter(|_| field.targets.allows(SettingLayer::Agent))
    {
        (value.clone(), SettingLayer::Agent)
    } else {
        (
            declaration.defaults[field.key].clone(),
            SettingLayer::Default,
        )
    }
}

impl ExtensionHub {
    /// Edit one declared field on the already opened session. The caller binds
    /// the ID and session-key handle; commit enforces database authorization.
    /// This never falls back to the legacy raw setter.
    pub async fn edit_session_setting(
        &self,
        session_db_id: &str,
        session_db: &Database,
        extension: &str,
        key: &str,
        edit: SettingEdit,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            session_db.root_id().to_string() == session_db_id,
            "session settings target no longer matches the open session"
        );
        self.checked_field(extension, key, SettingLayer::Session, &edit)?;
        anyhow::ensure!(
            session_db.current_permission().await?.can_write(),
            "session settings are read-only"
        );
        match edit {
            SettingEdit::Set(value) => {
                patch_settings(
                    session_db,
                    extension,
                    true,
                    std::iter::once((key.to_owned(), value)).collect(),
                )
                .await
            }
            SettingEdit::Reset => {
                update_settings(session_db, extension, true, |value| {
                    value
                        .as_object_mut()
                        .context("extension settings must be an object")?
                        .remove(key);
                    Ok(())
                })
                .await
            }
        }
    }

    /// Edit one declared preference in the exact hosted Agent DB the caller
    /// opened with its held key. There is no session or display-name fallback.
    pub async fn edit_agent_setting(
        &self,
        agent_db_id: &str,
        agent_db: &Database,
        extension: &str,
        key: &str,
        edit: SettingEdit,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            agent_db.root_id().to_string() == agent_db_id,
            "agent settings target no longer matches the open agent"
        );
        self.checked_field(extension, key, SettingLayer::Agent, &edit)?;
        anyhow::ensure!(
            agent_db.current_permission().await?.can_write(),
            "agent settings are read-only"
        );
        let txn = agent_db.new_transaction().await?;
        let before = read_agent_in(&txn, extension).await?;
        let mut after = before.clone();
        let fields = after.as_object_mut().expect("checked object");
        match edit {
            SettingEdit::Set(value) => {
                fields.insert(key.to_owned(), value);
            }
            SettingEdit::Reset => {
                fields.remove(key);
            }
        }
        if before == after {
            return Ok(());
        }
        txn.get_store::<DocStore>(crate::agent_db::CONFIG_STORE)
            .await?
            .set(agent_key(extension), delta(&before, &after, false))
            .await?;
        txn.commit().await?;
        Ok(())
    }

    fn checked_field(
        &self,
        extension: &str,
        key: &str,
        layer: SettingLayer,
        edit: &SettingEdit,
    ) -> anyhow::Result<&'static SettingField> {
        let field = self
            .registered_setting_fields(extension)
            .and_then(|declaration| declaration.field(key))
            .with_context(|| format!("no declared field {extension}.{key}"))?;
        anyhow::ensure!(
            field.targets.allows(layer),
            "{extension}.{key} does not allow {} settings",
            match layer {
                SettingLayer::Agent => "agent",
                _ => "session overrides",
            }
        );
        if let SettingEdit::Set(value) = edit {
            field.kind.validate(value)?;
        }
        Ok(field)
    }

    /// Declared rows for display at `target`. A Peer view passes no session;
    /// a session view names the viewed agent's DB, if it has one. Any read
    /// failure is an error rather than an absent preference.
    pub async fn setting_rows(
        &self,
        extension: &str,
        target: SettingLayer,
        session_db: Option<&Database>,
        agent_db: Option<&Database>,
    ) -> anyhow::Result<Vec<SettingRow>> {
        let Some(declaration) = self.registered_setting_fields(extension) else {
            return Ok(Vec::new());
        };
        let (session, retained) = match session_db {
            Some(db) => read_in(&db.new_transaction().await?, extension).await?,
            None => (serde_json::json!({}), false),
        };
        anyhow::ensure!(session.is_object(), "extension settings must be an object");
        let agent = match agent_db {
            Some(db) => read_agent_settings(db, extension).await?,
            None => serde_json::json!({}),
        };
        let (at_target, writer) = match target {
            SettingLayer::Session => (&session, session_db.filter(|_| !retained)),
            SettingLayer::Agent => (&agent, agent_db),
            SettingLayer::Default => (&agent, None),
        };
        let writable = match writer {
            Some(db) => db.current_permission().await?.can_write(),
            None => false,
        };
        Ok(declaration
            .fields()
            .iter()
            .map(|field| {
                let (value, source) = resolve(declaration, &session, &agent, field);
                SettingRow {
                    field,
                    value,
                    source,
                    present_at_target: at_target.get(field.key).is_some(),
                    editable: writable && field.targets.allows(target),
                }
            })
            .collect())
    }

    /// The settings an extension sees while executing for `agent` in
    /// `session_db`: raw session fields, including private ones, with each
    /// absent declared key overlaid in memory. Nothing is written back.
    pub(crate) async fn effective_settings(
        &self,
        extension: &str,
        session_db: Option<&Database>,
        agent: Option<&crate::hosted_index::DbEntry>,
    ) -> anyhow::Result<Value> {
        let raw = match session_db {
            Some(db) => read_settings_checked(db, extension).await?,
            None => serde_json::json!({}),
        };
        let Some(declaration) = self.registered_setting_fields(extension) else {
            return Ok(raw);
        };
        let registry = self
            .session_registry
            .as_deref()
            .or_else(|| self.peer_handles.as_ref().map(|peer| &*peer.registry));
        let agent = agent_layer(registry, agent, extension).await?;
        overlay(declaration, raw, &agent)
    }
}

/// The turn owner's explicit preferences. A genuinely agentless context has
/// no agent layer; an expected agent that cannot be opened is an error.
pub(crate) async fn agent_layer(
    registry: Option<&crate::session::SessionRegistry>,
    agent: Option<&crate::hosted_index::DbEntry>,
    extension: &str,
) -> anyhow::Result<Value> {
    let Some(entry) = agent else {
        return Ok(serde_json::json!({}));
    };
    let adb = registry
        .context("agent settings are unavailable")?
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await?
        .context("agent settings are unavailable")?;
    read_agent_settings(adb.database(), extension).await
}

/// Fill each absent declared key in a raw session object, in memory only.
pub(crate) fn overlay(
    declaration: &RegisteredSettingFields,
    mut raw: Value,
    agent: &Value,
) -> anyhow::Result<Value> {
    let fields = raw
        .as_object_mut()
        .context("extension settings must be an object")?;
    for field in declaration.fields() {
        if !fields.contains_key(field.key) {
            let (value, _) = resolve(declaration, &Value::Null, agent, field);
            fields.insert(field.key.to_owned(), value);
        }
    }
    Ok(raw)
}

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

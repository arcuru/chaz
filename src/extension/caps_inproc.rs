// Step 4 of the cap refactor — pure addition. Nothing in the runtime
// hands these out yet; step 5 (hub refactor) populates `HostCaps` with
// them and wires `install_all` to drain the registration queues.
#![allow(dead_code)]

//! In-process backings for the host-only capability traits.
//!
//! These are the impls of [`SessionRead`], [`SessionWrite`],
//! [`Settings`], [`ToolRegistration`], and [`CommandRegistration`]
//! that chaz core provides today. Same trait shapes will fit
//! sandboxed extensions later — only the dispatch wiring changes.
//!
//! # Phasing
//!
//! Step 4 (this file) defines the impls in isolation.
//! Step 5 (hub refactor) constructs them and routes calls through:
//!
//! * **Per-session caps** (`InProcSessionRead`, `InProcSessionWrite`,
//!   `InProcSettings`) — one fresh instance per `(extension, session)`
//!   at handler-fire time, scoped to that session's
//!   `Arc<Mutex<Session>>` / `Database`.
//! * **Global caps** (`InProcToolRegistration`,
//!   `InProcCommandRegistration`) — one instance per extension, alive
//!   only across that extension's `install()` call. Registrations are
//!   buffered into pending queues; the hub drains them after install
//!   and routes each `(owner, registration)` pair through the existing
//!   owner-attribution path.
//!
//! # Entry-shape translation
//!
//! The cap's [`SessionEntryDraft`] / [`SessionEntryView`] are
//! deliberately abstract (a `kind` string plus a JSON value) so they
//! survive a sandbox boundary. Chaz's in-tree [`SessionEntry`] is
//! concrete (`sender`, `content`, `entry_type`, …). [`map_kind_to_type`]
//! / [`map_type_to_kind`] convert between the two and [`encode_data`] /
//! [`decode_data`] handle the data ↔ content marshaling. Entries chaz
//! itself wrote (with plain-string content) read back as `{"text":
//! content}` — round-trip-friendly but not lossless on the type tag.

use crate::extension::caps::{
    CapFuture, CommandDescriptor, CommandRegistration, EntryCursor, EntryId, SessionEntryDraft,
    SessionEntryView, SessionMeta as CapSessionMeta, SessionRead, SessionWrite, Settings,
    ToolRegistration,
};
use crate::extension::{ExtensionCommand, read_settings, write_settings};
use crate::session::{EntryType, Session, SessionEntry};
use crate::tool::{Tool, ToolDescriptor};
use chrono::Utc;
use eidetica::Database;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

// =========================================================================
// Per-session caps
// =========================================================================

/// In-process [`SessionRead`] for one session.
///
/// Reads off the session's in-memory entry list (which `Session::new`
/// loads from eidetica at session-open and `add_entry` keeps in sync).
/// `entries()` returns the full list — pagination via `since` is a
/// best-effort filter on the cap-level [`EntryCursor`] (an opaque
/// timestamp string), not on eidetica's storage cursors.
pub struct InProcSessionRead {
    session: Arc<Mutex<Session>>,
}

impl InProcSessionRead {
    pub fn new(session: Arc<Mutex<Session>>) -> Self {
        Self { session }
    }
}

impl SessionRead for InProcSessionRead {
    fn entries<'a>(&'a self, since: Option<EntryCursor>) -> CapFuture<'a, Vec<SessionEntryView>> {
        Box::pin(async move {
            let session = self.session.lock().await;
            let cutoff = since
                .as_ref()
                .and_then(|c| chrono::DateTime::parse_from_rfc3339(&c.0).ok());
            let out: Vec<SessionEntryView> = session
                .entries()
                .iter()
                .filter(|e| cutoff.is_none_or(|c| e.timestamp > c))
                .map(entry_to_view)
                .collect();
            Ok(out)
        })
    }

    fn meta<'a>(&'a self) -> CapFuture<'a, CapSessionMeta> {
        Box::pin(async move {
            let session = self.session.lock().await;
            let stored = session.read_meta().await;
            Ok(CapSessionMeta {
                session_id: session.conversation_id.0.clone(),
                agent_name: stored.agent_name,
                model: stored.model,
            })
        })
    }
}

/// In-process [`SessionWrite`] for one session.
///
/// Appends a new entry via `Session::add_entry`. The cap layer treats
/// the writer as anonymous; the in-proc impl tags the entry with the
/// owning extension's name so the audit trail records who wrote what.
pub struct InProcSessionWrite {
    session: Arc<Mutex<Session>>,
    owner: String,
}

impl InProcSessionWrite {
    pub fn new(session: Arc<Mutex<Session>>, owner: impl Into<String>) -> Self {
        Self {
            session,
            owner: owner.into(),
        }
    }
}

impl SessionWrite for InProcSessionWrite {
    fn append<'a>(&'a self, entry: SessionEntryDraft) -> CapFuture<'a, EntryId> {
        Box::pin(async move {
            let timestamp = Utc::now();
            let session_entry = SessionEntry {
                sender: self.owner.clone(),
                content: encode_data(&entry.data),
                timestamp,
                entry_type: map_kind_to_type(&entry.kind),
                metadata: None,
            };
            let mut session = self.session.lock().await;
            session.add_entry(session_entry).await;
            // Today's `Session::add_entry` doesn't surface eidetica's
            // entry id, so synthesize a stable opaque token from the
            // write coordinates. Consumers must treat it as opaque.
            Ok(EntryId(format!(
                "{}@{}",
                self.owner,
                timestamp.timestamp_nanos_opt().unwrap_or(0)
            )))
        })
    }
}

/// In-process [`Settings`] for one extension on one session.
///
/// Backed by the existing per-session per-extension settings store
/// (see `read_settings` / `write_settings` in `extension/mod.rs`).
/// `get` returns `None` for any missing key — the underlying helper
/// returns `json!({})` for "no overrides at all", which we translate
/// per-key.
pub struct InProcSettings {
    database: Database,
    extension: String,
}

impl InProcSettings {
    pub fn new(database: Database, extension: impl Into<String>) -> Self {
        Self {
            database,
            extension: extension.into(),
        }
    }
}

impl Settings for InProcSettings {
    fn get<'a>(&'a self, key: &'a str) -> CapFuture<'a, Option<Value>> {
        Box::pin(async move {
            let blob = read_settings(&self.database, &self.extension).await;
            Ok(blob.get(key).cloned())
        })
    }

    fn set<'a>(&'a self, key: &'a str, value: Value) -> CapFuture<'a, ()> {
        Box::pin(async move {
            let mut blob = read_settings(&self.database, &self.extension).await;
            if !blob.is_object() {
                blob = serde_json::json!({});
            }
            blob.as_object_mut()
                .expect("read_settings normalized to object above")
                .insert(key.into(), value);
            write_settings(&self.database, &self.extension, blob).await
        })
    }
}

// =========================================================================
// Global registration caps (install-time only)
// =========================================================================

/// One tool registration captured in the pending queue.
///
/// Hub drains this list after `install()` returns and routes each
/// entry through the existing owner-attributed tool registration path.
#[derive(Clone)]
pub struct PendingTool {
    pub owner: String,
    pub descriptor: ToolDescriptor,
    pub tool: Arc<dyn Tool>,
}

/// One slash-command registration captured in the pending queue.
pub struct PendingCommand {
    pub owner: String,
    pub descriptor: CommandDescriptor,
    pub command: Box<dyn ExtensionCommand>,
}

/// In-process [`ToolRegistration`] for one extension's install call.
///
/// Buffered: every `register` call pushes `(owner, descriptor, tool)`
/// onto the shared pending queue. The hub drains the queue after
/// `install()` returns, then routes each entry through
/// `ExtensionHub::register_tool` so attribution and collision handling
/// stay identical to the legacy path.
pub struct InProcToolRegistration {
    owner: String,
    pending: Arc<Mutex<Vec<PendingTool>>>,
}

impl InProcToolRegistration {
    pub fn new(owner: impl Into<String>, pending: Arc<Mutex<Vec<PendingTool>>>) -> Self {
        Self {
            owner: owner.into(),
            pending,
        }
    }
}

impl ToolRegistration for InProcToolRegistration {
    fn register<'a>(
        &'a self,
        descriptor: ToolDescriptor,
        tool: Arc<dyn Tool>,
    ) -> CapFuture<'a, ()> {
        Box::pin(async move {
            self.pending.lock().await.push(PendingTool {
                owner: self.owner.clone(),
                descriptor,
                tool,
            });
            Ok(())
        })
    }
}

/// In-process [`CommandRegistration`] for one extension's install call.
/// Same buffered pattern as [`InProcToolRegistration`].
pub struct InProcCommandRegistration {
    owner: String,
    pending: Arc<Mutex<Vec<PendingCommand>>>,
}

impl InProcCommandRegistration {
    pub fn new(owner: impl Into<String>, pending: Arc<Mutex<Vec<PendingCommand>>>) -> Self {
        Self {
            owner: owner.into(),
            pending,
        }
    }
}

impl CommandRegistration for InProcCommandRegistration {
    fn register<'a>(
        &'a self,
        descriptor: CommandDescriptor,
        command: Box<dyn ExtensionCommand>,
    ) -> CapFuture<'a, ()> {
        Box::pin(async move {
            self.pending.lock().await.push(PendingCommand {
                owner: self.owner.clone(),
                descriptor,
                command,
            });
            Ok(())
        })
    }
}

// =========================================================================
// Entry-shape translation
// =========================================================================

/// Cap-level entry `kind` strings → chaz [`EntryType`]. Unknown kinds
/// fall back to [`EntryType::Message`] (the most permissive in-context
/// flavor) so the system fails open rather than silently dropping the
/// write.
fn map_kind_to_type(kind: &str) -> EntryType {
    match kind {
        "message" => EntryType::Message,
        "directive" => EntryType::Directive,
        "tool_call" => EntryType::ToolCall,
        "tool_result" => EntryType::ToolResult,
        "ack" => EntryType::Ack,
        "error" => EntryType::Error,
        "summary" => EntryType::Summary,
        "persona_snapshot" => EntryType::PersonaSnapshot,
        _ => EntryType::Message,
    }
}

fn map_type_to_kind(t: &EntryType) -> &'static str {
    match t {
        EntryType::Message => "message",
        EntryType::Directive => "directive",
        EntryType::ToolCall => "tool_call",
        EntryType::ToolResult => "tool_result",
        EntryType::Ack => "ack",
        EntryType::Error => "error",
        EntryType::Summary => "summary",
        EntryType::PersonaSnapshot => "persona_snapshot",
    }
}

/// JSON values serialize to their string form for storage in
/// `SessionEntry.content`. Strings pass through unwrapped so chaz's
/// existing plain-string writes round-trip readably.
fn encode_data(data: &Value) -> String {
    match data {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Reverse of [`encode_data`]: if `content` parses as JSON, return the
/// parsed value; else wrap as `{"text": content}` so consumers always
/// get a `Value` regardless of who wrote the entry.
fn decode_data(content: &str) -> Value {
    serde_json::from_str(content).unwrap_or_else(|_| serde_json::json!({ "text": content }))
}

fn entry_to_view(entry: &SessionEntry) -> SessionEntryView {
    SessionEntryView {
        id: EntryId(format!(
            "{}@{}",
            entry.sender,
            entry.timestamp.timestamp_nanos_opt().unwrap_or(0)
        )),
        kind: map_type_to_kind(&entry.entry_type).into(),
        data: decode_data(&entry.content),
        timestamp: entry.timestamp,
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;

    async fn fixture_db() -> (Instance, Database) {
        let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, db)
    }

    async fn fixture_session() -> (Instance, Arc<Mutex<Session>>) {
        let (instance, db) = fixture_db().await;
        let session = Session::new(ConversationId("conv".into()), db).await;
        (instance, Arc::new(Mutex::new(session)))
    }

    // --- Translation helpers ----------------------------------------------

    #[test]
    fn map_kind_round_trips_for_every_known_type() {
        let kinds = [
            "message",
            "directive",
            "tool_call",
            "tool_result",
            "ack",
            "error",
            "summary",
            "persona_snapshot",
        ];
        for k in kinds {
            let t = map_kind_to_type(k);
            assert_eq!(map_type_to_kind(&t), k, "round-trip {k}");
        }
    }

    #[test]
    fn unknown_kind_falls_back_to_message() {
        assert!(matches!(map_kind_to_type("invented"), EntryType::Message));
    }

    #[test]
    fn encode_data_preserves_strings_unwrapped() {
        assert_eq!(encode_data(&serde_json::json!("hi")), "hi");
        assert_eq!(
            encode_data(&serde_json::json!({"k": 1})),
            r#"{"k":1}"#.to_string()
        );
    }

    #[test]
    fn decode_data_wraps_plain_strings() {
        assert_eq!(
            decode_data("hello, world"),
            serde_json::json!({"text": "hello, world"}),
        );
        assert_eq!(decode_data(r#"{"k":1}"#), serde_json::json!({"k": 1}));
    }

    // --- InProcSessionWrite + InProcSessionRead ---------------------------

    #[tokio::test]
    async fn write_then_read_round_trip_yields_view() {
        let (_inst, session) = fixture_session().await;
        let writer = InProcSessionWrite::new(session.clone(), "heartbeat");
        let id = writer
            .append(SessionEntryDraft {
                kind: "directive".into(),
                data: serde_json::json!({"task": "summarize overnight"}),
            })
            .await
            .unwrap();
        assert!(id.0.starts_with("heartbeat@"), "got id: {id:?}");

        let reader = InProcSessionRead::new(session.clone());
        let entries = reader.entries(None).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "directive");
        assert_eq!(
            entries[0].data,
            serde_json::json!({"task": "summarize overnight"})
        );
    }

    #[tokio::test]
    async fn read_filters_with_cursor() {
        let (_inst, session) = fixture_session().await;
        let writer = InProcSessionWrite::new(session.clone(), "heartbeat");
        writer
            .append(SessionEntryDraft {
                kind: "message".into(),
                data: serde_json::json!("first"),
            })
            .await
            .unwrap();

        // Cursor at *now* (after the first write) — second write is
        // strictly later by nanos; reader should see only it.
        let cursor = EntryCursor(Utc::now().to_rfc3339());

        // Force ordering even on coarse clocks.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;

        writer
            .append(SessionEntryDraft {
                kind: "message".into(),
                data: serde_json::json!("second"),
            })
            .await
            .unwrap();

        let reader = InProcSessionRead::new(session);
        let filtered = reader.entries(Some(cursor)).await.unwrap();
        assert_eq!(filtered.len(), 1, "expected only entries after cursor");
        assert_eq!(filtered[0].data, serde_json::json!({"text": "second"}));
    }

    #[tokio::test]
    async fn read_meta_returns_session_id() {
        let (_inst, session) = fixture_session().await;
        let reader = InProcSessionRead::new(session);
        let meta = reader.meta().await.unwrap();
        assert_eq!(meta.session_id, "conv");
    }

    // --- InProcSettings ---------------------------------------------------

    #[tokio::test]
    async fn settings_round_trip_via_cap() {
        let (_inst, db) = fixture_db().await;
        let cap = InProcSettings::new(db.clone(), "memory");
        assert_eq!(cap.get("max_results").await.unwrap(), None);
        cap.set("max_results", serde_json::json!(8)).await.unwrap();
        assert_eq!(
            cap.get("max_results").await.unwrap(),
            Some(serde_json::json!(8))
        );
    }

    #[tokio::test]
    async fn settings_isolate_by_extension_name() {
        let (_inst, db) = fixture_db().await;
        InProcSettings::new(db.clone(), "memory")
            .set("k", serde_json::json!(1))
            .await
            .unwrap();
        InProcSettings::new(db.clone(), "heartbeat")
            .set("k", serde_json::json!(2))
            .await
            .unwrap();

        let memory = InProcSettings::new(db.clone(), "memory")
            .get("k")
            .await
            .unwrap();
        let heartbeat = InProcSettings::new(db.clone(), "heartbeat")
            .get("k")
            .await
            .unwrap();
        assert_eq!(memory, Some(serde_json::json!(1)));
        assert_eq!(heartbeat, Some(serde_json::json!(2)));
    }

    #[tokio::test]
    async fn settings_overwrites_existing_key() {
        let (_inst, db) = fixture_db().await;
        let cap = InProcSettings::new(db, "ext");
        cap.set("k", serde_json::json!("first")).await.unwrap();
        cap.set("k", serde_json::json!("second")).await.unwrap();
        assert_eq!(
            cap.get("k").await.unwrap(),
            Some(serde_json::json!("second"))
        );
    }

    // --- InProcToolRegistration ------------------------------------------

    struct StubTool;
    impl Tool for StubTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "stub".into(),
                description: "stub tool".into(),
                parameters: serde_json::json!({}),
            }
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a crate::tool::ToolContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, crate::tool::ToolError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok("stubbed".to_string()) })
        }
    }

    #[tokio::test]
    async fn tool_registration_buffers_into_pending_queue() {
        let pending: Arc<Mutex<Vec<PendingTool>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = InProcToolRegistration::new("heartbeat", pending.clone());
        let tool: Arc<dyn Tool> = Arc::new(StubTool);
        cap.register(tool.descriptor(), tool.clone()).await.unwrap();
        cap.register(tool.descriptor(), tool.clone()).await.unwrap();

        let queue = pending.lock().await;
        assert_eq!(queue.len(), 2);
        assert!(queue.iter().all(|p| p.owner == "heartbeat"));
        assert!(queue.iter().all(|p| p.descriptor.name == "stub"));
    }

    #[tokio::test]
    async fn two_tool_caps_share_queue_with_distinct_owners() {
        let pending = Arc::new(Mutex::new(Vec::new()));
        let cap_a = InProcToolRegistration::new("alpha", pending.clone());
        let cap_b = InProcToolRegistration::new("beta", pending.clone());
        let tool: Arc<dyn Tool> = Arc::new(StubTool);
        cap_a
            .register(tool.descriptor(), tool.clone())
            .await
            .unwrap();
        cap_b
            .register(tool.descriptor(), tool.clone())
            .await
            .unwrap();

        let queue = pending.lock().await;
        let owners: Vec<&str> = queue.iter().map(|p| p.owner.as_str()).collect();
        assert_eq!(owners, vec!["alpha", "beta"]);
    }

    // --- InProcCommandRegistration ---------------------------------------

    struct StubCmd;
    impl ExtensionCommand for StubCmd {
        fn description(&self) -> &'static str {
            "stub command"
        }
        fn invoke<'a>(
            &'a self,
            _args: &'a str,
            _ctx: &'a crate::extension::HookContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = crate::extension::ExtensionCommandOutcome>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { crate::extension::ExtensionCommandOutcome::Text("stubbed".into()) })
        }
    }

    #[tokio::test]
    async fn command_registration_buffers_into_pending_queue() {
        let pending: Arc<Mutex<Vec<PendingCommand>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = InProcCommandRegistration::new("heartbeat", pending.clone());
        cap.register(
            CommandDescriptor {
                name: "wakeup".into(),
                description: "schedule a wakeup".into(),
            },
            Box::new(StubCmd),
        )
        .await
        .unwrap();

        let queue = pending.lock().await;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].owner, "heartbeat");
        assert_eq!(queue[0].descriptor.name, "wakeup");
    }
}

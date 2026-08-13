//! Bootstrap-queue handlers. Single transport-neutral
//! `/sharing` namespace covering the request queue across every resource
//! kind (agents, memory banks, sessions). Eidetica stores all bootstrap
//! requests in the one `_sync` DB regardless of what kind of tree they
//! target, so chaz exposes one unified queue.
//!
//! - `/sharing requests` — list pending bootstrap requests with the
//!   resource kind + display name resolved when the target DB is hosted
//!   on this peer.
//! - `/sharing approve <id>` — grants the requester's pubkey the
//!   permission they asked for. Owner can override only by rejecting and
//!   pre-seeding a different permission via `/agent invite`.
//! - `/sharing reject <id>` — marks the request rejected.
//! - `/sharing` — list every database this peer is currently sharing.

use eidetica::auth::types::Permission;

use super::{CommandContext, CommandOutcome};
use crate::agent_db::LoginRef;
use crate::security::approval_display::{pubkey_fingerprint, quarantine, quarantine_capped};

/// Display cap for a peer-supplied timestamp — long enough for RFC 3339, short
/// enough that padding it cannot push the rest of the line out of view.
const TIMESTAMP_CAP: usize = 32;

/// Render a short label for a target DB by walking the hosted indices.
/// Sessions don't appear in either index, so any tree_id we don't recognize
/// is labelled by its short id.
fn label_for_target(ctx: &CommandContext<'_>, tree_id: &eidetica::entry::ID) -> String {
    if let Some(entry) = ctx.server.agent_index().find_by_id(tree_id) {
        return format!("agent '{}'", entry.display_name);
    }
    if let Some(entry) = ctx.server.memory_bank_index().find_by_id(tree_id) {
        return format!("memory bank '{}'", entry.display_name);
    }
    let s = tree_id.to_string();
    let short = &s[..8.min(s.len())];
    format!("DB {short}…")
}

fn permission_name(p: &Permission) -> String {
    match p {
        Permission::Admin(n) => format!("admin({n})"),
        Permission::Write(n) => format!("write({n})"),
        Permission::Read => "read".to_string(),
    }
}

/// Everything one pending request contributes to the prompt, with the
/// attested and the claimed parts already separated. Split out from
/// [`sharing_requests`] because seeding a real bootstrap request needs live
/// two-peer sync, so this is where the rendering is actually testable.
struct RequestView<'a> {
    /// 1-based position in the printed list, which is also what
    /// `/sharing approve <index>` takes.
    index: usize,
    /// Locally assigned request ID, shortened.
    request_id_short: &'a str,
    /// Fingerprint of the key that signed the request — the attested anchor.
    fingerprint: &'a str,
    /// Locally resolved label for the target DB.
    target_label: &'a str,
    /// Permission being asked for, as recorded by the local sync layer.
    permission: &'a str,
    /// Timestamp as claimed by the requester.
    timestamp: &'a str,
    /// Login pointer the request asks the approver to register, if any.
    claimed: Option<&'a LoginRef>,
    /// Pointer already registered under the claimed identifier, if any.
    /// Approving replaces it.
    existing: Option<&'a LoginRef>,
}

/// Render one pending request.
///
/// The layout is deliberate: the fingerprint of the requesting key comes
/// first, because it is the only field the requester cannot choose, and
/// everything the requester *did* choose sits below it in an indented
/// `claimed:` block that is quarantined for display. A reader who stops after
/// the first line has still read something attested.
fn render_request(view: &RequestView<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("  [{}] {}", view.index, view.fingerprint));
    lines.push(format!(
        "       wants {} on {}",
        view.permission, view.target_label
    ));
    let ts = quarantine_capped(view.timestamp, TIMESTAMP_CAP);
    lines.push(format!(
        "       request {}, claimed time {}{}",
        view.request_id_short,
        ts,
        ts.note()
    ));

    // Approving a login request also writes its pointer into the agent DB,
    // replacing any existing pointer for the same identifier. Show what is
    // being claimed so that is a decision rather than a side effect.
    if let Some(login) = view.claimed {
        let kind = quarantine(&login.kind);
        let identifier = quarantine(&login.identifier);
        let bridge_db = quarantine(&login.bridge_db_id);
        lines.push("       claimed by the requester — unverified:".to_string());
        lines.push(format!("         kind:        {}{}", kind, kind.note()));
        lines.push(format!(
            "         identifier:  {}{}",
            identifier,
            identifier.note()
        ));
        lines.push(format!(
            "         bridge DB:   {}{}",
            bridge_db,
            bridge_db.note()
        ));

        if let Some(existing) = view.existing {
            let old_kind = quarantine(&existing.kind);
            let old_db = quarantine(&existing.bridge_db_id);
            lines.push(
                "       approving REPLACES the login already registered under that identifier:"
                    .to_string(),
            );
            lines.push(format!("         old: {} → bridge DB {}", old_kind, old_db));
            lines.push(format!("         new: {} → bridge DB {}", kind, bridge_db));
        }
    }
    lines
}

/// The pointer already registered under `identifier` on the target agent DB,
/// if this peer hosts that DB and holds a key for it. `None` covers every
/// case where nothing would be replaced *and* every case where we cannot
/// tell — the caller only uses it to add a warning, never to suppress one.
async fn existing_login_for(
    ctx: &CommandContext<'_>,
    tree_id: &eidetica::entry::ID,
    identifier: &str,
) -> Option<LoginRef> {
    let entry = ctx.server.agent_index().find_by_id(tree_id)?;
    let db = ctx
        .server
        .registry()
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .ok()??;
    db.find_login(identifier).await.ok()?
}

pub(super) async fn sharing_requests(ctx: &CommandContext<'_>) -> CommandOutcome {
    let requests = match ctx.server.registry().pending_bootstrap_requests().await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to list pending requests: {e}")),
    };
    if requests.is_empty() {
        return CommandOutcome::Text("No pending bootstrap requests.".to_string());
    }
    let mut lines: Vec<String> = Vec::with_capacity(requests.len() * 4 + 3);
    lines.push(format!("Pending bootstrap requests ({}):", requests.len()));
    lines.push(
        "Each entry opens with the SHA256 fingerprint of the requesting key; everything under \
         'claimed' is supplied by the requester."
            .to_string(),
    );
    for (i, (id, req)) in requests.iter().enumerate() {
        let claimed = req
            .metadata
            .as_ref()
            .and_then(crate::agent_db::LoginRef::from_metadata);
        // Look the existing pointer up by the raw identifier: the registry
        // dedups on the stored string, not on the quarantined rendering.
        let existing = match claimed.as_ref() {
            Some(login) => existing_login_for(ctx, &req.tree_id, &login.identifier).await,
            None => None,
        };
        lines.extend(render_request(&RequestView {
            index: i + 1,
            request_id_short: &id[..8.min(id.len())],
            fingerprint: &pubkey_fingerprint(&req.requesting_pubkey),
            target_label: &label_for_target(ctx, &req.tree_id),
            permission: &permission_name(&req.requested_permission),
            timestamp: &req.timestamp,
            claimed: claimed.as_ref(),
            existing: existing.as_ref(),
        }));
    }
    lines.push(
        "Approve with /sharing approve <index|prefix>, reject with /sharing reject <index|prefix>."
            .to_string(),
    );
    CommandOutcome::Text(lines.join("\n"))
}

/// Resolve a user-supplied prefix or 1-based index to an exact bootstrap
/// request ID. If `prefix` parses as a number, it's treated as an index
/// into the pending-request list. Otherwise it's matched as an ID prefix
/// (case-sensitive). Returns the full request ID on success or an error
/// string describing the failure.
async fn resolve_request_prefix(prefix: &str, ctx: &CommandContext<'_>) -> Result<String, String> {
    let requests = match ctx.server.registry().pending_bootstrap_requests().await {
        Ok(r) => r,
        Err(e) => return Err(format!("Failed to list pending requests: {e}")),
    };

    // Index-based lookup
    if let Ok(idx) = prefix.parse::<usize>() {
        if idx < 1 || idx > requests.len() {
            return Err(format!(
                "Index {} is out of range (1-{})",
                idx,
                requests.len()
            ));
        }
        return Ok(requests[idx - 1].0.clone());
    }

    // Prefix-based lookup
    let matches: Vec<&(String, eidetica::sync::BootstrapRequest)> = requests
        .iter()
        .filter(|(id, _)| id.starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(format!("no pending request matches prefix '{prefix}'")),
        1 => Ok(matches[0].0.clone()),
        _ => {
            let candidates: Vec<String> = matches
                .iter()
                .enumerate()
                .map(|(i, (id, _))| format!("  [{}] {}", i + 1, id))
                .collect();
            Err(format!(
                "ambiguous prefix '{prefix}' — {} candidates:\n{}",
                matches.len(),
                candidates.join("\n")
            ))
        }
    }
}

pub(super) async fn sharing_approve(args: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return CommandOutcome::Error("Usage: /sharing approve <index|prefix>".to_string());
    }
    let request_id = match resolve_request_prefix(trimmed, ctx).await {
        Ok(id) => id,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .approve_bootstrap_request(&request_id)
        .await
    {
        Ok((tree_id, req)) => CommandOutcome::Text(format!(
            "Approved {} for {} as {}. The requester must re-run their import to pull entries.",
            label_for_target(ctx, &tree_id),
            pubkey_fingerprint(&req.requesting_pubkey),
            permission_name(&req.requested_permission),
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to approve request: {e}")),
    }
}

pub(super) async fn sharing_reject(args: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return CommandOutcome::Error("Usage: /sharing reject <index|prefix>".to_string());
    }
    let request_id = match resolve_request_prefix(trimmed, ctx).await {
        Ok(id) => id,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .reject_bootstrap_request(&request_id)
        .await
    {
        Ok((tree_id, req)) => CommandOutcome::Text(format!(
            "Rejected request from {} for {}.",
            pubkey_fingerprint(&req.requesting_pubkey),
            label_for_target(ctx, &tree_id),
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to reject request: {e}")),
    }
}

/// List every database this peer is currently sharing (sync enabled).
/// Walks the user's tracked databases, classifies each by cross-referencing
/// with the hosted indices, and renders a table showing kind, display name
/// (when known), and DB root ID.
pub(super) async fn sharing_status(ctx: &CommandContext<'_>) -> CommandOutcome {
    let user = ctx.server.registry().user_lock().await;
    let tracked = match user.databases().await {
        Ok(dbs) => dbs,
        Err(e) => return CommandOutcome::Error(format!("Failed to list databases: {e}")),
    };
    let agent_index = ctx.server.agent_index();
    let bank_index = ctx.server.memory_bank_index();

    let shared: Vec<_> = tracked
        .into_iter()
        .filter(|tdb| tdb.sync_settings.sync_enabled)
        .collect();

    if shared.is_empty() {
        return CommandOutcome::Text("No databases are currently being shared.".to_string());
    }

    let mut lines: Vec<String> = Vec::with_capacity(shared.len() + 2);
    lines.push(format!("Sharing {} database(s):", shared.len()));
    lines.push(String::new());

    for tdb in &shared {
        let kind = if agent_index.find_by_id(&tdb.database_id).is_some() {
            "agent     "
        } else if bank_index.find_by_id(&tdb.database_id).is_some() {
            "bank      "
        } else {
            // Sessions are tracked by eidetica but not in either hosted index.
            // The registry's `sessions` store is the canonical session list.
            "session   "
        };
        let db_id = tdb.database_id.to_string();
        let short_id = &db_id[..8.min(db_id.len())];
        lines.push(format!("  {kind}  {short_id}…  {}", tdb.database_id));
    }

    // Append the sync server address so users know what address to put in tickets.
    if let Some(sync) = ctx.server.registry().instance().sync()
        && let Ok(addr) = sync.get_server_address().await
    {
        lines.push(String::new());
        lines.push(format!("Sync server address: {addr}"));
    }

    CommandOutcome::Text(lines.join("\n"))
}

#[cfg(test)]
mod render_tests {
    use super::{RequestView, render_request};
    use crate::agent_db::LoginRef;

    fn login(kind: &str, identifier: &str, bridge_db_id: &str) -> LoginRef {
        LoginRef {
            kind: kind.into(),
            identifier: identifier.into(),
            bridge_db_id: bridge_db_id.into(),
            peer_pubkey: None,
            sync_addresses: Vec::new(),
        }
    }

    fn view<'a>(
        claimed: Option<&'a LoginRef>,
        existing: Option<&'a LoginRef>,
        timestamp: &'a str,
    ) -> RequestView<'a> {
        RequestView {
            index: 1,
            request_id_short: "1a2b3c4d",
            fingerprint: "SHA256:0123456789abcdefghijklmnopqrstuvwxyzABCDEFG",
            target_label: "agent 'chaz'",
            permission: "write(bridge)",
            timestamp,
            claimed,
            existing,
        }
    }

    fn render(claimed: Option<&LoginRef>, existing: Option<&LoginRef>) -> String {
        render_request(&view(claimed, existing, "2026-08-13T00:00:00Z")).join("\n")
    }

    #[test]
    fn fingerprint_is_the_first_thing_on_the_first_line() {
        let out = render(None, None);
        let first = out.lines().next().unwrap();
        assert_eq!(
            first,
            "  [1] SHA256:0123456789abcdefghijklmnopqrstuvwxyzABCDEFG"
        );
        // The claimed block only appears when a login is actually claimed.
        assert!(!out.contains("claimed by the requester"), "got: {out}");
    }

    #[test]
    fn claimed_values_are_labelled_and_indented_below_the_fingerprint() {
        let l = login("matrix", "@chaz:example", "ab12cd34");
        let out = render(Some(&l), None);
        let claimed_at = out.find("claimed by the requester").expect("claimed block");
        let fp_at = out.find("SHA256:").expect("fingerprint");
        assert!(fp_at < claimed_at, "fingerprint must come first: {out}");
        assert!(out.contains("         identifier:  @chaz:example"), "{out}");
        assert!(out.contains("         kind:        matrix"), "{out}");
        assert!(out.contains("         bridge DB:   ab12cd34"), "{out}");
    }

    #[test]
    fn control_characters_never_reach_the_prompt() {
        let l = login("mat\u{0}rix", "@chaz\u{7}:example", "ab\u{1}12\u{7f}cd");
        let out = render(Some(&l), None);
        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "control character survived: {out:?}"
        );
        assert!(out.contains("kind:        matrix"), "{out}");
    }

    #[test]
    fn an_embedded_newline_cannot_forge_a_second_request_line() {
        let l = login(
            "matrix",
            "@chaz:example\n  [2] SHA256:trustme wants admin",
            "ab12",
        );
        let out = render(Some(&l), None);
        // Every rendered line for a single request is one of ours; a claimed
        // newline shows up escaped instead of breaking the line.
        assert!(out.contains("\\n  [2] SHA256:trustme"), "{out}");
        assert!(
            !out.lines().any(|line| line.starts_with("  [2] ")),
            "claim forged a request line: {out}"
        );
        assert!(out.contains("(escaped for display)"), "{out}");
    }

    #[test]
    fn escape_sequences_are_neutralized() {
        let l = login("matrix", "@chaz:example\u{1b}[2K\u{1b}]0;pwn\u{7}", "ab12");
        let out = render(Some(&l), None);
        assert!(!out.contains('\u{1b}'), "escape survived: {out:?}");
        assert!(out.contains('\u{fffd}'), "{out}");
    }

    #[test]
    fn bidi_overrides_are_neutralized() {
        let l = login("matrix", "@chaz\u{202e}elpmaxe:\u{202c}", "ab12");
        let out = render(Some(&l), None);
        assert!(!out.contains('\u{202e}'), "bidi override survived: {out:?}");
        assert!(!out.contains('\u{202c}'), "bidi pop survived: {out:?}");
    }

    #[test]
    fn over_length_claims_are_capped() {
        let long = format!("@{}:example", "a".repeat(200));
        let l = login("matrix", &long, &"b".repeat(300));
        let out = render(Some(&l), None);
        for line in out.lines() {
            assert!(
                line.chars().count() < 100,
                "line ran long ({}): {line}",
                line.chars().count()
            );
        }
        assert!(out.contains('…'), "{out}");
        assert!(out.contains("(truncated)"), "{out}");
    }

    #[test]
    fn a_hostile_timestamp_is_quarantined_too() {
        let out = render_request(&view(None, None, "2026\u{1b}[31m-08-13\nfake")).join("\n");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(out.contains("\\nfake"), "{out}");
    }

    #[test]
    fn replacing_an_existing_pointer_renders_an_old_new_diff() {
        let claimed = login("matrix", "@chaz:example", "newbridgedb");
        let existing = login("matrix", "@chaz:example", "oldbridgedb");
        let out = render(Some(&claimed), Some(&existing));
        assert!(out.contains("approving REPLACES"), "{out}");
        assert!(out.contains("old: matrix → bridge DB oldbridgedb"), "{out}");
        assert!(out.contains("new: matrix → bridge DB newbridgedb"), "{out}");
        let old_at = out.find("old: ").unwrap();
        let new_at = out.find("new: ").unwrap();
        assert!(old_at < new_at, "{out}");
    }

    #[test]
    fn a_first_registration_renders_no_replace_warning() {
        let claimed = login("matrix", "@chaz:example", "newbridgedb");
        let out = render(Some(&claimed), None);
        assert!(!out.contains("REPLACES"), "{out}");
        assert!(!out.contains("old: "), "{out}");
    }

    #[test]
    fn the_replace_diff_quarantines_the_stored_side_as_well() {
        let claimed = login("matrix", "@chaz:example", "newbridgedb");
        let existing = login("mat\u{1b}[31mrix", "@chaz:example", "old\u{202e}db");
        let out = render(Some(&claimed), Some(&existing));
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\u{202e}'), "{out:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Command, CommandContext, CommandOutcome, dispatch};
    use crate::agent::AgentRegistry;
    use crate::backends::BackendManager;
    use crate::hosted_index::HostedIndex;
    use crate::security::SecretStore;
    use crate::server::Server;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};
    use std::sync::Arc;

    /// Fixture matches `commands/agent.rs::tests::fixture` but takes a
    /// flag for whether to call `instance.enable_sync()` before opening
    /// the registry — Sharing tests need sync on; "sync not enabled"
    /// negative tests need it off.
    async fn fixture(
        sync_enabled: bool,
    ) -> (
        Instance,
        Arc<Server>,
        SecretStore,
        BackendManager,
        String,
        eidetica::Database,
    ) {
        let backend = InMemory::new();
        let (instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        if sync_enabled {
            instance.enable_sync().await.unwrap();
        }
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
                .await
                .unwrap(),
        );
        let chaz_peer = registry.chaz_peer().clone();
        let index = HostedIndex::empty("agent");
        let bank_index = HostedIndex::empty("bank");
        let tools = Arc::new(crate::tool::ToolRegistry::new());
        let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
        let security = crate::security::SecurityContext {
            leak_detector: crate::security::LeakDetector::new(
                crate::security::LeakPolicy::default(),
            ),
            auto_approved_tools: std::collections::HashSet::new(),
            approval_callback: None,
        };
        let secrets = SecretStore::new(chaz_peer).await;
        let backend_mgr = BackendManager::new(&None, secrets.clone());
        let server = Server::new(
            registry.clone(),
            agents,
            index,
            bank_index,
            crate::hosted_index::HostedIndex::empty("skill_bank"),
            tools,
            policies,
            security,
            std::collections::HashMap::new(),
            Default::default(),
            std::sync::Arc::new(crate::tool_host::NativeToolHost::new()),
            std::sync::Arc::new(crate::extension::ExtensionHub::new()),
            backend_mgr.clone(),
            std::sync::Arc::new(crate::mcp::McpRegistry::new()),
            true,
        );
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        (
            instance,
            server,
            secrets,
            backend_mgr,
            session_db_id,
            session_db,
        )
    }

    fn cmd_ctx<'a>(
        server: &'a Arc<Server>,
        secrets: &'a SecretStore,
        backend: &'a BackendManager,
        session_db_id: &'a str,
        session_db: &'a eidetica::Database,
    ) -> CommandContext<'a> {
        CommandContext {
            server,
            secrets,
            backend,
            session_db_id,
            session_db,
            current_agent: "chaz",
            session_name: None,
        }
    }

    #[tokio::test]
    async fn sharing_requests_empty_when_sync_enabled() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingRequests, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("No pending"), "got: {msg}");
            }
            other => panic!("expected Text, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_requests_errors_without_sync() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(false).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingRequests, &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("Sync not enabled"), "got: {msg}");
            }
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_approve_no_match_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingApprove("ghost-id".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(
                    msg.contains("no pending request matches prefix"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_reject_no_match_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingReject("ghost-id".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(
                    msg.contains("no pending request matches prefix"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_approve_blank_id_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingApprove("   ".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Usage"), "got: {msg}"),
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_reject_blank_id_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingReject("   ".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Usage"), "got: {msg}"),
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_approve_index_out_of_range_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        // No pending requests, so index 1 is out of range.
        match dispatch(Command::SharingApprove("1".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("out of range"), "got: {msg}");
            }
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_reject_index_out_of_range_errors() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingReject("1".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("out of range"), "got: {msg}");
            }
            other => panic!("expected Error, got {:?}", outcome_kind(&other)),
        }
    }

    /// Tiny outcome-kind helper — `CommandOutcome` doesn't impl Debug/Display
    /// directly, so panic messages need a hand-rolled label.
    fn outcome_kind(o: &CommandOutcome) -> &'static str {
        match o {
            CommandOutcome::Text(_) => "Text",
            CommandOutcome::Error(_) => "Error",
            CommandOutcome::SessionsList(_) => "SessionsList",
            CommandOutcome::SessionSwitched(_) => "SessionSwitched",
            CommandOutcome::Quit => "Quit",
        }
    }

    #[tokio::test]
    async fn sharing_status_empty_when_nothing_shared() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::SharingStatus, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(
                    msg.contains("No databases are currently being shared"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Text, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn sharing_status_shows_enabled() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Enable sync on the session DB — it should appear in the status output.
        let db_id = sdb.root_id().clone();
        ctx.server.registry().enable_sync_for(&db_id).await.unwrap();

        match dispatch(Command::SharingStatus, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("Sharing 1 database"), "got: {msg}");
                assert!(msg.contains("session"), "got: {msg}");
                assert!(msg.contains(&sid[..8]), "got: {msg}");
            }
            other => panic!("expected Text, got {:?}", outcome_kind(&other)),
        }
    }

    #[tokio::test]
    async fn session_unshare_disables_sync() {
        let (_i, server, secrets, backend, sid, sdb) = fixture(true).await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Enable sync so we have something to disable
        let db_id = sdb.root_id().clone();
        ctx.server.registry().enable_sync_for(&db_id).await.unwrap();
        assert!(
            ctx.server
                .registry()
                .is_sync_enabled_for(&db_id)
                .await
                .unwrap()
        );

        match dispatch(Command::SessionUnshare, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("no longer shared"), "got: {msg}");
            }
            other => panic!("expected Text, got {:?}", outcome_kind(&other)),
        }

        assert!(
            !ctx.server
                .registry()
                .is_sync_enabled_for(&db_id)
                .await
                .unwrap()
        );
    }
}

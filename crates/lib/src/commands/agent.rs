//! Living Agents handlers: session participation (attach/detach/list/host)
//! and lifecycle (new/share/import/hosted/delete).

use crate::session::Session;
use crate::types::ConversationId;

use super::{CoOwnerPermission, CommandContext, CommandOutcome, RehostScope};

// -----------------------------------------------------------------------------
// Shared: agent ref resolution
// -----------------------------------------------------------------------------

/// Resolve a user-supplied ref — either an agent display name or an eidetica
/// DB ID — to a `DbEntry`.
pub(super) async fn resolve_agent_ref(
    agent_ref: &str,
    ctx: &CommandContext<'_>,
) -> Result<crate::hosted_index::DbEntry, String> {
    let index = ctx.server.agent_index();
    if let Some(entry) = index.find_by_name(agent_ref) {
        return Ok(entry);
    }
    if let Ok(id) = eidetica::entry::ID::parse(agent_ref)
        && let Some(entry) = index.find_by_id(&id)
    {
        return Ok(entry);
    }
    Err(format!(
        "No hosted agent matches '{agent_ref}' (try a display name from /agents or an agent DB ID)"
    ))
}

// -----------------------------------------------------------------------------
// Participation
// -----------------------------------------------------------------------------

pub(super) async fn agent_add(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .attach_agent_to_session(ctx.session_db_id, &entry)
        .await
    {
        Ok(()) => CommandOutcome::Text(format!(
            "Attached agent '{}' to this session",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to attach agent: {e}")),
    }
}

pub(super) async fn agent_remove(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .detach_agent_from_session(ctx.session_db_id, &entry)
        .await
    {
        Ok(()) => CommandOutcome::Text(format!(
            "Detached agent '{}' from this session",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to detach agent: {e}")),
    }
}

pub(super) async fn agents_list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let meta = crate::session::read_meta_from_db(ctx.session_db).await;
    if meta.agents.is_empty() {
        // Pre-auto-attach session: name what actually routes so the user
        // isn't misled by "none attached" when messages clearly resolve.
        // `current_agent` mirrors the routing resolution chain.
        let routing = ctx.current_agent;
        let legacy = meta
            .agent_name
            .as_deref()
            .map(|n| format!(", legacy agent_name: {n}"))
            .unwrap_or_default();
        return CommandOutcome::Text(format!(
            "No Living Agents attached to this session.\n\
             Routing falls back to: {routing} (default){legacy}\n\
             \n\
             Run `/agent add {routing}` to make this explicit — required for\n\
             per-agent model overrides and other agent-scoped features."
        ));
    }
    let host = meta.host_agent_db_id.as_deref();
    let lines: Vec<String> = meta
        .agents
        .iter()
        .map(|a| {
            let marker = if host == Some(a.db_id.as_str()) {
                " *host*"
            } else {
                ""
            };
            format!("  {}{} ({})", a.display_name, marker, a.db_id)
        })
        .collect();
    CommandOutcome::Text(format!("Agents on this session:\n{}", lines.join("\n")))
}

/// `/agent room` — the multi-agent chat-room status surface (Gap 2).
/// Shows the attached roster, the designated host (flagging a dangling
/// host id if one somehow survives), and the burst-budget state so an
/// operator can see *why* an agent→agent chain stopped.
pub(super) async fn agent_room(ctx: &CommandContext<'_>) -> CommandOutcome {
    let meta = crate::session::read_meta_from_db(ctx.session_db).await;

    let mut out = String::from("Chat-room status for this session:\n");

    if meta.agents.is_empty() {
        out.push_str("  attached agents: (none — single-agent / legacy session)\n");
    } else {
        out.push_str("  attached agents:\n");
        for a in &meta.agents {
            out.push_str(&format!("    {} ({})\n", a.display_name, a.db_id));
        }
    }

    match meta.host_agent_db_id.as_deref() {
        None => out.push_str("  host: (none — turns use first-authorized order)\n"),
        Some(host_id) => match meta.agents.iter().find(|a| a.db_id == host_id) {
            Some(a) => out.push_str(&format!("  host: {}\n", a.display_name)),
            None => out.push_str(&format!(
                "  host: <dangling {host_id}> (not attached — will fall back; run /agent host to fix)\n"
            )),
        },
    }

    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let budget = ctx.server.agent_burst_budget();
    let burst = crate::session::trailing_agent_message_burst(session.entries(), |name| {
        ctx.server.agents().get(name).is_some()
    });
    out.push_str(&format!(
        "  agent→agent burst: {burst}/{budget}{}\n",
        if burst >= budget {
            " (exhausted — agent→agent wakes suppressed until a human or schedule speaks)"
        } else {
            ""
        }
    ));
    if meta.agents.len() < 2 {
        out.push_str("  note: agent→agent routing is inert until ≥2 agents are attached.\n");
    }

    CommandOutcome::Text(out)
}

pub(super) async fn agent_set_host(arg: Option<&str>, ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;

    match arg {
        None => {
            if let Err(e) = session.update_meta(|m| m.host_agent_db_id = None).await {
                return CommandOutcome::Error(format!("Failed to clear host agent: {e}"));
            }
            CommandOutcome::Text("Cleared host agent for this session".to_string())
        }
        Some(agent_ref) => {
            let entry = match resolve_agent_ref(agent_ref, ctx).await {
                Ok(e) => e,
                Err(msg) => return CommandOutcome::Error(msg),
            };

            // Host must be attached — catch the "set host on un-attached agent" footgun.
            let meta = crate::session::read_meta_from_db(ctx.session_db).await;
            let db_id = entry.db_id.to_string();
            if !meta.agents.iter().any(|a| a.db_id == db_id) {
                return CommandOutcome::Error(format!(
                    "Agent '{}' is not attached to this session. Attach it first with /agent add {}",
                    entry.display_name, agent_ref
                ));
            }

            let name = entry.display_name.clone();
            if let Err(e) = session
                .update_meta(move |m| m.host_agent_db_id = Some(db_id))
                .await
            {
                return CommandOutcome::Error(format!("Failed to set host agent: {e}"));
            }
            CommandOutcome::Text(format!("Set host agent to '{name}'"))
        }
    }
}

// -----------------------------------------------------------------------------
// Lifecycle: /agent new | share | import | hosted | delete
// -----------------------------------------------------------------------------

/// Supported `/agent new` and `/agent set` keys. Nested-structure fields
/// (`grants`, `presets`) intentionally omitted — edit yaml template or add
/// a dedicated command.
const SUPPORTED_AGENT_FIELDS: &str = "model, tools, autonomous, max_iterations, tool_profile, max_context_tokens, system_prompt, system_prompt_files";

/// Apply a single `key=value` override to an `AgentDbConfig`. Used by
/// `/agent new` (on a fresh config) and `/agent set` (on a DB-loaded one).
/// Unknown keys surface as user-facing errors so typos aren't silently dropped.
pub(super) fn apply_agent_field(
    cfg: &mut crate::agent_db::AgentDbConfig,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let comma_split = |v: &str| -> Vec<String> {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let parse_bool = |v: &str| -> Result<bool, String> {
        match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(format!("Invalid bool '{v}' (use true|false)")),
        }
    };

    match key {
        "model" => cfg.model = Some(value.to_string()),
        "tools" => cfg.tools = Some(comma_split(value)),
        "autonomous" => cfg.autonomous = parse_bool(value)?,
        "max_iterations" => {
            cfg.max_iterations = Some(
                value
                    .parse::<u32>()
                    .map_err(|e| format!("Invalid max_iterations '{value}': {e}"))?,
            );
        }
        "tool_profile" => cfg.tool_profile = Some(value.to_string()),
        "max_context_tokens" => {
            cfg.max_context_tokens = Some(
                value
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid max_context_tokens '{value}': {e}"))?,
            );
        }
        "system_prompt" => cfg.system_prompt = value.to_string(),
        "system_prompt_files" => {
            cfg.system_prompt_files = comma_split(value);
        }
        other => {
            return Err(format!(
                "Unknown agent field '{other}'. Supported: {SUPPORTED_AGENT_FIELDS}"
            ));
        }
    }
    Ok(())
}

fn apply_agent_new_overrides(
    cfg: &mut crate::agent_db::AgentDbConfig,
    overrides: &[(String, String)],
) -> Result<(), String> {
    for (key, value) in overrides {
        apply_agent_field(cfg, key, value)?;
    }
    Ok(())
}

pub(super) async fn agent_new(
    name: &str,
    overrides: &[(String, String)],
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let name = name.trim();
    if name.is_empty() {
        return CommandOutcome::Error("Agent name required".to_string());
    }

    // Reject duplicates at the registry level early — create_new_agent_db also
    // rejects at the DB-name level, but this catches in-memory collisions too.
    if ctx.server.agents().get(name).is_some() {
        return CommandOutcome::Error(format!("Agent '{name}' already registered"));
    }

    let mut cfg = crate::agent_db::AgentDbConfig::default();
    if let Err(msg) = apply_agent_new_overrides(&mut cfg, overrides) {
        return CommandOutcome::Error(msg);
    }
    let meta = crate::agent_db::AgentMeta {
        display_name: Some(name.to_string()),
        ..Default::default()
    };

    let (agent_db, pubkey) = match ctx
        .server
        .registry()
        .create_new_agent_db(name, &cfg, &meta)
        .await
    {
        Ok(p) => p,
        Err(e) => return CommandOutcome::Error(format!("Failed to create Agent DB: {e}")),
    };
    let db_id = agent_db.id();

    // Register in the peer-local agent index.
    ctx.server
        .agent_index()
        .register(crate::hosted_index::DbEntry {
            db_id: db_id.clone(),
            display_name: name.to_string(),
            pubkey: pubkey.clone(),
        });

    // Build a runtime Agent so the AgentRegistry can resolve it — makes the
    // agent spawnable + attachable by display name for the rest of this session.
    let runtime_agent = ctx.server.agents().build_from_db_config(name, &cfg);
    if let Err(e) = ctx.server.agents().register(runtime_agent) {
        return CommandOutcome::Error(format!(
            "Agent DB created + indexed but runtime registry rejected: {e}"
        ));
    }

    CommandOutcome::Text(format!(
        "Created Living Agent '{name}' (DB: {db_id}). Attach to a session with /agent add {name}."
    ))
}

pub(super) async fn agent_share(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let instance = ctx.server.registry().instance();
    if instance.sync().is_none() {
        return CommandOutcome::Error("Sync not enabled".to_string());
    }

    let ticket = match ctx.server.registry().share_for(&entry.db_id).await {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Failed to share agent DB: {e}")),
    };
    CommandOutcome::Text(format!(
        "Share this ticket to sync agent '{}' (DB {}):\n\n{ticket}",
        entry.display_name, entry.db_id
    ))
}

/// Disable sync on an agent DB so this peer stops serving it.
pub(super) async fn agent_unshare(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx.server.registry().disable_sync_for(&entry.db_id).await {
        Ok(()) => CommandOutcome::Text(format!(
            "Sync disabled for agent '{}' — it is no longer shared.",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to disable sync: {e}")),
    }
}

pub(super) async fn agent_import(
    ticket_str: &str,
    permission: CoOwnerPermission,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Invalid ticket: {e}")),
    };
    let db_id = ticket.database_id().clone();
    let eidetica_perm = map_coowner_permission(permission);

    // Bootstrap path: if the requester is preseeded (e.g. owner already
    // ran /agent invite), sync proceeds and we land in `Approved`. Otherwise
    // eidetica queues a pending request and the owner has to /sharing approve
    // before this peer can pull entries; we tell the user to re-run.
    match ctx
        .server
        .registry()
        .request_db_access(&ticket, eidetica_perm)
        .await
    {
        Ok(crate::session::BootstrapOutcome::Approved) => {}
        Ok(crate::session::BootstrapOutcome::Pending {
            request_id,
            message: _,
        }) => {
            return CommandOutcome::Text(format!(
                "Bootstrap request {request_id} pending the owner's approval. \
                 Re-run `/agent import <ticket>` after they run `/sharing approve {request_id}`."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Bootstrap failed: {e}")),
    }

    let agent_db = match ctx.server.registry().open_agent_db(&db_id, None).await {
        Ok(Some(db)) => db,
        Ok(None) => {
            return CommandOutcome::Error(format!(
                "Bootstrap reported success on agent DB {db_id} but this peer still holds no key. \
                 Likely an eidetica state mismatch — re-run the import to retry."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open synced agent DB: {e}")),
    };

    let meta = match agent_db.read_meta().await {
        Ok(m) => m,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent meta: {e}")),
    };
    let cfg = match agent_db.read_config().await {
        Ok(c) => c,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent config: {e}")),
    };
    let display_name = meta.display_name.clone().unwrap_or_else(|| {
        format!(
            "agent-{}",
            &db_id.to_string()[..8.min(db_id.to_string().len())]
        )
    });

    // Resolve the pubkey we hold for this DB — that's what `attach` writes
    // into session AuthSettings later. `open_agent_db` above already proved
    // a key exists; this second lookup is just to get the pubkey out.
    let pubkey =
        match ctx.server.registry().find_key_for_db(&db_id).await {
            Ok(Some(k)) => k,
            _ => return CommandOutcome::Error(
                "Expected a key for this DB (open_agent_db succeeded) but find_key returned None"
                    .to_string(),
            ),
        };

    ctx.server
        .agent_index()
        .register(crate::hosted_index::DbEntry {
            db_id: db_id.clone(),
            display_name: display_name.clone(),
            pubkey,
        });

    // Upsert into the runtime registry so re-importing a previously-seen
    // agent refreshes its config from the synced DB (model/tools/role may
    // have changed upstream since the last import).
    let runtime_agent = ctx
        .server
        .agents()
        .build_from_db_config(&display_name, &cfg);
    ctx.server.agents().upsert(runtime_agent);

    if let Err(e) = ctx.server.registry().enable_sync_for(&db_id).await {
        return CommandOutcome::Error(format!(
            "Imported agent '{display_name}' (DB {db_id}) but failed to enable ongoing sync: {e}"
        ));
    }

    CommandOutcome::Text(format!(
        "Imported agent '{display_name}' (DB {db_id}). Attach with /agent add {display_name}."
    ))
}

pub(super) async fn agent_hosted(ctx: &CommandContext<'_>) -> CommandOutcome {
    let entries = ctx.server.agent_index().list();
    if entries.is_empty() {
        return CommandOutcome::Text("No Living Agents hosted on this peer.".to_string());
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| format!("  {} ({})", e.display_name, e.db_id))
        .collect();
    CommandOutcome::Text(format!(
        "Living Agents hosted on this peer:\n{}",
        lines.join("\n")
    ))
}

pub(super) async fn agent_delete(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    // Refuse if the agent is still attached to any known session. Walking
    // every session is O(N) but agent-delete is a rare operation.
    let sessions = match ctx.server.registry().list_sessions().await {
        Ok(s) => s,
        Err(e) => return CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    };
    let db_id_str = entry.db_id.to_string();
    for idx in &sessions {
        let (_conv, sdb) = match ctx.server.registry().open_session(&idx.session_db_id).await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let meta = crate::session::read_meta_from_db(&sdb).await;
        if meta.agents.iter().any(|a| a.db_id == db_id_str) {
            return CommandOutcome::Error(format!(
                "Agent '{}' is still attached to session {}. Detach it first (/agent remove {}).",
                entry.display_name, idx.session_db_id, entry.display_name
            ));
        }
    }

    ctx.server.agent_index().unregister(&entry.db_id);
    ctx.server.agents().unregister(&entry.display_name);

    // Agent-owned schedules die with the agent DB; there is no session
    // routine sweep. A Pinned schedule whose owner is gone self-skips at
    // fire time (membership-at-fire check in fire_agent_schedule).
    CommandOutcome::Text(format!(
        "Deleted Living Agent '{}' (DB {} preserved for archive).",
        entry.display_name, entry.db_id
    ))
}

/// Edit one field on a Living Agent's DB config. Live hydration picks
/// up the change on the next message — no restart. We also upsert
/// the runtime `AgentRegistry` snapshot so the current session sees the
/// edit immediately (hydration rebuilds on message, upsert is belt-and-
/// suspenders for code paths that read registry state between messages).
pub(super) async fn agent_set(
    agent_ref: &str,
    field: &str,
    value: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let agent_db = match ctx
        .server
        .registry()
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
    {
        Ok(Some(db)) => db,
        Ok(None) => {
            return CommandOutcome::Error(format!(
                "This peer holds no key for agent '{}' — can't edit a read-only import",
                entry.display_name
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open agent DB: {e}")),
    };

    let mut cfg = match agent_db.read_config().await {
        Ok(c) => c,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent config: {e}")),
    };

    if let Err(msg) = apply_agent_field(&mut cfg, field, value) {
        return CommandOutcome::Error(msg);
    }

    // A prompt edit must refresh the blob pointer, else a stale
    // `system_prompt_ref` would mask the new inline text / files at hydration.
    // Other fields leave the ref untouched.
    if matches!(field, "system_prompt" | "system_prompt_files")
        && let Err(e) = ctx.server.refresh_prompt_ref(&mut cfg).await
    {
        return CommandOutcome::Error(format!("Failed to resolve system prompt: {e}"));
    }

    if let Err(e) = agent_db.write_config(&cfg).await {
        return CommandOutcome::Error(format!("Failed to write agent config: {e}"));
    }

    let runtime_agent = ctx
        .server
        .agents()
        .build_from_db_config(&entry.display_name, &cfg);
    ctx.server.agents().upsert(runtime_agent.clone());

    CommandOutcome::Text(format!(
        "Set {field}={value} on agent '{}' (takes effect next message)",
        entry.display_name
    ))
}

/// `/agent reload [ref]` — re-read the on-disk chaz yaml and re-run the
/// hash-gated reconcile, for one named agent or (with no ref) every agent.
/// This is the on-demand twin of the startup reconcile: yaml-declared fields
/// and the resolved system prompt refresh into each agent's DB, while a live
/// `/agent set` edit survives when the yaml block and prompt files are
/// unchanged. Changes take effect on the next message via live hydration.
pub(super) async fn agent_reload(
    agent_ref: Option<&str>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    match ctx.server.reload_config_for(agent_ref).await {
        Ok(report) => {
            if let Some(name) = agent_ref
                && report.considered == 0
            {
                return CommandOutcome::Error(format!(
                    "No agent '{name}' declared in the chaz config — nothing to reload"
                ));
            }
            match report.changed.as_slice() {
                [] => CommandOutcome::Text(
                    "Reload: every agent already matched its yaml — no change".to_string(),
                ),
                names => CommandOutcome::Text(format!(
                    "Reloaded from yaml: {} (effective next message)",
                    names.join(", ")
                )),
            }
        }
        Err(e) => CommandOutcome::Error(format!("Reload failed: {e}")),
    }
}

// -----------------------------------------------------------------------------
// Co-owned Agents: /pubkey + /agent invite + /agent revoke-peer
// -----------------------------------------------------------------------------

pub(super) async fn pubkey(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx.server.registry().default_pubkey().await {
        Ok(pk) => CommandOutcome::Text(pk.to_prefixed_string()),
        Err(e) => CommandOutcome::Error(format!("Failed to read default pubkey: {e}")),
    }
}

fn map_coowner_permission(p: CoOwnerPermission) -> eidetica::auth::types::Permission {
    match p {
        CoOwnerPermission::Admin => eidetica::auth::types::Permission::Admin(1),
        CoOwnerPermission::Write => eidetica::auth::types::Permission::Write(10),
        CoOwnerPermission::Read => eidetica::auth::types::Permission::Read,
    }
}

pub(super) async fn agent_invite(
    agent_ref: &str,
    pubkey_str: &str,
    permission: CoOwnerPermission,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let pk = match eidetica::auth::crypto::PublicKey::from_prefixed_string(pubkey_str) {
        Ok(k) => k,
        Err(e) => {
            return CommandOutcome::Error(format!(
                "Invalid pubkey '{pubkey_str}' — expected ed25519:base64… ({e})"
            ));
        }
    };

    // Inviting your own key is a no-op — the peer already holds a key on this DB.
    if pk == entry.pubkey {
        return CommandOutcome::Error(format!(
            "You already own agent '{}' on this peer — no invite needed",
            entry.display_name
        ));
    }

    let eidetica_perm = map_coowner_permission(permission);
    // Short-hash label keeps the AuthSettings key-id readable without bloating
    // the settings doc with the full 44-char base64 pubkey.
    let short = pubkey_str
        .strip_prefix("ed25519:")
        .unwrap_or(pubkey_str)
        .chars()
        .take(8)
        .collect::<String>();
    let key_label = format!("co-{}:{short}", permission_label(permission));

    if let Err(e) = ctx
        .server
        .registry()
        .grant_on_agent_db(&entry.db_id, &pk, &key_label, eidetica_perm)
        .await
    {
        return CommandOutcome::Error(format!("Failed to invite peer: {e}"));
    }

    CommandOutcome::Text(format!(
        "Invited {pubkey_str} as {permission:?} on agent '{}' (DB {}). Share the ticket with /agent share {}.",
        entry.display_name, entry.db_id, entry.display_name
    ))
}

fn permission_label(p: CoOwnerPermission) -> &'static str {
    match p {
        CoOwnerPermission::Admin => "admin",
        CoOwnerPermission::Write => "write",
        CoOwnerPermission::Read => "read",
    }
}

pub(super) async fn agent_revoke_peer(
    agent_ref: &str,
    pubkey_str: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let pk = match eidetica::auth::crypto::PublicKey::from_prefixed_string(pubkey_str) {
        Ok(k) => k,
        Err(e) => {
            return CommandOutcome::Error(format!(
                "Invalid pubkey '{pubkey_str}' — expected ed25519:base64… ({e})"
            ));
        }
    };

    // Revoking our own key on the agent's DB would orphan the agent locally
    // (no key → can't open the DB, can't undo). Use /agent delete for the
    // peer-local cleanup instead.
    if pk == entry.pubkey {
        return CommandOutcome::Error(format!(
            "That's this peer's own key for agent '{}' — use /agent delete to unregister locally",
            entry.display_name
        ));
    }

    if let Err(e) = ctx
        .server
        .registry()
        .revoke_on_agent_db(&entry.db_id, &pk)
        .await
    {
        return CommandOutcome::Error(format!("Failed to revoke peer: {e}"));
    }

    // Soft warning: identify sessions and agent-level state where the
    // revoked key was the home peer. The revoke succeeded — we don't
    // block it — but the affected sessions/agents will go silent on
    // their next wake until a surviving peer runs `/agent rehost`.
    let revoked_str = pk.to_string();
    let mut affected_sessions: Vec<String> = Vec::new();
    if let Ok(sessions) = ctx.server.registry().list_sessions().await {
        for s in sessions {
            let Ok((_conv, db)) = ctx.server.registry().open_session(&s.session_db_id).await else {
                continue;
            };
            let meta = crate::session::read_meta_from_db(&db).await;
            if meta.agents.iter().any(|a| {
                a.db_id == entry.db_id.to_string()
                    && a.home_pubkey.as_deref() == Some(revoked_str.as_str())
            }) {
                affected_sessions.push(s.session_db_id);
            }
        }
    }
    let agent_level_was_home = matches!(
        ctx.server.registry().open_agent_db(&entry.db_id, None).await,
        Ok(Some(adb)) if matches!(
            crate::db_kind::read_agent_home_pubkey(adb.database()).await,
            Some(p) if p == pk
        )
    );

    let mut body = format!(
        "Revoked {pubkey_str} from agent '{}'. They retain read access to history but cannot write.",
        entry.display_name
    );
    if !affected_sessions.is_empty() {
        body.push_str(&format!(
            "\n\nWARNING: revoked key was the home peer for {} session(s): {}. \
             Their next turn will be silent until you run `/agent rehost {}` from a surviving peer.",
            affected_sessions.len(),
            affected_sessions.join(", "),
            entry.display_name
        ));
    }
    if agent_level_was_home {
        body.push_str(&format!(
            "\n\nWARNING: revoked key was the agent-level home for '{}'. \
             Fresh schedule fires will be silent until you run `/agent rehost --agent {}` from a surviving peer.",
            entry.display_name, entry.display_name
        ));
    }
    CommandOutcome::Text(body)
}

/// `/agent rehost` — reassign the home peer for an agent in a session
/// (default scope) or globally (with `--agent`). `--clear` removes the
/// field. With no explicit pubkey, defaults to "rehost to me".
pub(super) async fn agent_rehost(
    agent_ref: &str,
    pubkey: Option<&str>,
    scope: RehostScope,
    clear: bool,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    if clear && pubkey.is_some() {
        return CommandOutcome::Error(
            "/agent rehost: cannot combine --clear with an explicit pubkey".to_string(),
        );
    }

    // Resolve the target pubkey (None = clear; Some = set to this peer's
    // local pubkey if no arg, else the parsed-and-authorized arg).
    let target_pk: Option<eidetica::auth::crypto::PublicKey> = if clear {
        None
    } else if let Some(s) = pubkey {
        let pk = match eidetica::auth::crypto::PublicKey::from_prefixed_string(s) {
            Ok(k) => k,
            Err(e) => {
                return CommandOutcome::Error(format!(
                    "Invalid pubkey '{s}' — expected ed25519:base64… ({e})"
                ));
            }
        };
        let agent_db = match ctx
            .server
            .registry()
            .open_agent_db(&entry.db_id, None)
            .await
        {
            Ok(Some(a)) => a,
            Ok(None) => {
                return CommandOutcome::Error(format!(
                    "This peer holds no key for agent '{}'",
                    entry.display_name
                ));
            }
            Err(e) => return CommandOutcome::Error(format!("Open agent DB failed: {e}")),
        };
        let settings = match agent_db.database().get_settings().await {
            Ok(s) => s,
            Err(e) => return CommandOutcome::Error(format!("Read settings failed: {e}")),
        };
        let active = matches!(
            settings.get_auth_key(&pk).await,
            Ok(auth) if auth.status() == &eidetica::auth::types::KeyStatus::Active
        );
        if !active {
            return CommandOutcome::Error(format!(
                "Target pubkey {s} is not authorized on agent '{}' — invite it first \
                 with /agent invite",
                entry.display_name
            ));
        }
        Some(pk)
    } else {
        Some(entry.pubkey.clone())
    };

    match scope {
        RehostScope::Session => {
            let target_str = target_pk.as_ref().map(|p| p.to_string());
            let agent_db_id = entry.db_id.to_string();
            let mut found = false;
            if let Err(e) = crate::session::update_meta_on_db(ctx.session_db, |m| {
                if let Some(a) = m.agents.iter_mut().find(|a| a.db_id == agent_db_id) {
                    a.home_pubkey = target_str.clone();
                    found = true;
                }
            })
            .await
            {
                return CommandOutcome::Error(format!("Failed to update session meta: {e}"));
            }
            if !found {
                return CommandOutcome::Error(format!(
                    "Agent '{}' is not attached to this session",
                    entry.display_name
                ));
            }
            // Reset the home-skip counter so a recently-stuck session that
            // just got rehosted doesn't WARN again on its next legit skip.
            ctx.server
                .reset_home_skip(ctx.session_db_id, &entry.display_name)
                .await;
            if clear {
                CommandOutcome::Text(format!(
                    "Cleared session-level home_pubkey for agent '{}'. \
                     WARNING: this re-introduces the multi-peer execution race \
                     on this session — two co-owning peers may now both respond.",
                    entry.display_name
                ))
            } else {
                CommandOutcome::Text(format!(
                    "Set session-level home_pubkey for agent '{}' to {}",
                    entry.display_name,
                    target_pk.as_ref().unwrap()
                ))
            }
        }
        RehostScope::Agent => {
            let agent_db = match ctx
                .server
                .registry()
                .open_agent_db(&entry.db_id, None)
                .await
            {
                Ok(Some(a)) => a,
                Ok(None) => {
                    return CommandOutcome::Error(format!(
                        "This peer holds no key for agent '{}'",
                        entry.display_name
                    ));
                }
                Err(e) => return CommandOutcome::Error(format!("Open agent DB failed: {e}")),
            };
            let result = if let Some(pk) = target_pk.as_ref() {
                crate::db_kind::write_agent_home_pubkey(agent_db.database(), pk).await
            } else {
                crate::db_kind::clear_agent_home_pubkey(agent_db.database()).await
            };
            if let Err(e) = result {
                return CommandOutcome::Error(format!("Failed to update agent meta: {e}"));
            }
            if clear {
                CommandOutcome::Text(format!(
                    "Cleared agent-level home_pubkey for '{}'. WARNING: this \
                     re-introduces the multi-peer race on Fresh schedule fires.",
                    entry.display_name
                ))
            } else {
                CommandOutcome::Text(format!(
                    "Set agent-level home_pubkey for '{}' to {}",
                    entry.display_name,
                    target_pk.as_ref().unwrap()
                ))
            }
        }
    }
}

/// `/agent home-status [ref]` — print agent-level and per-session
/// `home_pubkey` for one or all locally-hosted agents. Pubkeys that
/// match this peer's local key on the agent are tagged `← (me)`.
pub(super) async fn agent_home_status(
    agent_ref: Option<&str>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    use std::fmt::Write as _;

    let agents: Vec<crate::hosted_index::DbEntry> = match agent_ref {
        Some(r) => match resolve_agent_ref(r, ctx).await {
            Ok(e) => vec![e],
            Err(msg) => return CommandOutcome::Error(msg),
        },
        None => ctx.server.agent_index().list(),
    };
    if agents.is_empty() {
        return CommandOutcome::Text("No locally-hosted agents.".to_string());
    }

    let sessions = match ctx.server.registry().list_sessions().await {
        Ok(v) => v,
        Err(e) => return CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    };

    let mut out = String::new();
    for entry in &agents {
        let my_pk_str = entry.pubkey.to_string();
        let _ = writeln!(
            out,
            "agent: {} (db_id: {})",
            entry.display_name, entry.db_id
        );

        // Agent-level home_pubkey (Fresh-fire owner).
        let agent_level = match ctx
            .server
            .registry()
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
        {
            Ok(Some(adb)) => crate::db_kind::read_agent_home_pubkey(adb.database()).await,
            _ => None,
        };
        match agent_level {
            Some(pk) if pk.to_string() == my_pk_str => {
                let _ = writeln!(out, "  agent-level home: {pk} ← (me)");
            }
            Some(pk) => {
                let _ = writeln!(out, "  agent-level home: {pk}");
            }
            None => {
                let _ = writeln!(out, "  agent-level home: <unset — legacy, any keyholder>");
            }
        }

        // Per-session homes.
        let mut session_rows: Vec<String> = Vec::new();
        for s in &sessions {
            let Ok((_conv, db)) = ctx.server.registry().open_session(&s.session_db_id).await else {
                continue;
            };
            let meta = crate::session::read_meta_from_db(&db).await;
            let Some(ar) = meta
                .agents
                .iter()
                .find(|a| a.db_id == entry.db_id.to_string())
            else {
                continue;
            };
            let label = meta.name.as_deref().unwrap_or("");
            let row = match ar.home_pubkey.as_deref() {
                Some(home) if home == my_pk_str => {
                    format!("    {} {label:<30} {home} ← (me)", &s.session_db_id)
                }
                Some(home) => format!("    {} {label:<30} {home}", &s.session_db_id),
                None => format!(
                    "    {} {label:<30} <unset — legacy, any keyholder>",
                    &s.session_db_id
                ),
            };
            session_rows.push(row);
        }
        let _ = writeln!(out, "  sessions ({}):", session_rows.len());
        for row in session_rows {
            out.push_str(&row);
            out.push('\n');
        }
    }
    CommandOutcome::Text(out.trim_end().to_string())
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;

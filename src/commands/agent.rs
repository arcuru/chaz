//! Living Agents handlers: session participation (attach/detach/list/host)
//! and lifecycle (new/share/import/hosted/delete).

use crate::session::Session;
use crate::types::ConversationId;

use super::{CoOwnerPermission, CommandContext, CommandOutcome};

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
// Participation (Living Agents Stage 3d)
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
        let fallback = meta.agent_name.unwrap_or_else(|| "<default>".to_string());
        return CommandOutcome::Text(format!(
            "No Living Agents attached to this session. Legacy agent: {fallback}"
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
// Lifecycle (Living Agents Stage 6): /agent new | share | import | hosted | delete
// -----------------------------------------------------------------------------

/// Supported `/agent new` and `/agent set` keys. Nested-structure fields
/// (`grants`, `presets`) intentionally omitted — edit yaml template or add
/// a dedicated command.
///
/// Persona sub-fields use dotted keys: `persona.files` (comma-sep paths),
/// `persona.prompt` (inline text), `persona.description` (label),
/// `persona.clear` (any value — drops the persona).
const SUPPORTED_AGENT_FIELDS: &str = "role, model, tools, can_spawn, allowed_callers, autonomous, max_iterations, tool_profile, max_context_tokens, persona.files, persona.prompt, persona.description, persona.clear";

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
        "role" => cfg.role = Some(value.to_string()),
        "model" => cfg.model = Some(value.to_string()),
        "tools" => cfg.tools = Some(comma_split(value)),
        "can_spawn" => cfg.can_spawn = comma_split(value),
        "allowed_callers" => cfg.allowed_callers = comma_split(value),
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
        "persona.files" => {
            let files = comma_split(value);
            let mut p = cfg.persona.clone().unwrap_or_default();
            p.files = files;
            cfg.persona = Some(p);
        }
        "persona.prompt" => {
            let mut p = cfg.persona.clone().unwrap_or_default();
            p.prompt = if value.trim().is_empty() {
                None
            } else {
                Some(value.to_string())
            };
            cfg.persona = Some(p);
        }
        "persona.description" => {
            let mut p = cfg.persona.clone().unwrap_or_default();
            p.description = if value.trim().is_empty() {
                None
            } else {
                Some(value.to_string())
            };
            cfg.persona = Some(p);
        }
        "persona.clear" => {
            cfg.persona = None;
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

/// Edit one field on a Living Agent's DB config. Stage 8 live hydration
/// picks up the change on the next message — no restart. We also upsert
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

    let was_persona_edit = field.starts_with("persona.");
    if let Err(msg) = apply_agent_field(&mut cfg, field, value) {
        return CommandOutcome::Error(msg);
    }

    if let Err(e) = agent_db.write_config(&cfg).await {
        return CommandOutcome::Error(format!("Failed to write agent config: {e}"));
    }

    let runtime_agent = ctx
        .server
        .agents()
        .build_from_db_config(&entry.display_name, &cfg);
    ctx.server.agents().upsert(runtime_agent.clone());

    // If the operator edited the persona, immediately freeze the new
    // resolved prompt into this session as a snapshot. Other sessions
    // hosting this agent need an explicit `/agent persona bump` to pick
    // it up — same deterministic posture as file edits.
    let mut suffix = String::new();
    if was_persona_edit {
        match runtime_agent.persona {
            Some(persona) => match crate::session::write_persona_snapshot(
                ctx.session_db,
                &entry.display_name,
                &persona,
                crate::persona::SnapshotReason::Edit,
            )
            .await
            {
                Ok(_) => suffix.push_str(" + new PersonaSnapshot written"),
                Err(e) => {
                    suffix.push_str(&format!(
                        " (warning: snapshot write failed: {e}; bump manually)"
                    ));
                }
            },
            None => {
                // persona.clear left the persona empty — no snapshot to write.
            }
        }
    }

    CommandOutcome::Text(format!(
        "Set {field}={value} on agent '{}' (takes effect next message{suffix})",
        entry.display_name
    ))
}

// -----------------------------------------------------------------------------
// Persona inspection / refresh
// -----------------------------------------------------------------------------

/// `/agent persona show <ref>` — print the agent's current persona
/// definition (files + inline prompt) plus a summary of the most recent
/// `PersonaSnapshot` written into the active session.
pub(super) async fn agent_persona_show(
    agent_ref: &str,
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
                "This peer holds no key for agent '{}'",
                entry.display_name
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open agent DB: {e}")),
    };

    let cfg = match agent_db.read_config().await {
        Ok(c) => c,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent config: {e}")),
    };

    let mut out = format!("Persona for agent '{}':\n", entry.display_name);
    match &cfg.persona {
        None => {
            out.push_str("  (no persona set)\n");
            if let Some(role) = &cfg.role {
                out.push_str(&format!("  legacy role: {role}\n"));
            }
        }
        Some(p) => {
            if let Some(d) = &p.description {
                out.push_str(&format!("  description: {d}\n"));
            }
            if !p.files.is_empty() {
                out.push_str("  files:\n");
                for f in &p.files {
                    out.push_str(&format!("    - {f}\n"));
                }
            }
            if let Some(prompt) = &p.prompt {
                out.push_str(&format!(
                    "  inline prompt: ({} chars)\n    {}\n",
                    prompt.len(),
                    prompt.lines().take(3).collect::<Vec<_>>().join("\n    ")
                ));
            }
        }
    }

    // Latest snapshot summary on the active session.
    let session = crate::session::Session::new(
        crate::types::ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let entries = session.entries();
    match crate::context::latest_persona_snapshot(entries, &entry.display_name) {
        None => out.push_str("\nNo PersonaSnapshot yet on this session.\n"),
        Some(snap) => {
            out.push_str(&format!(
                "\nLatest snapshot ({}):\n  written_at: {}\n  reason: {:?}\n  sources: {}\n  text length: {} chars\n",
                entry.display_name,
                snap.written_at.to_rfc3339(),
                snap.reason,
                snap.resolved.sources.len(),
                snap.resolved.text.len(),
            ));
            for s in &snap.resolved.sources {
                out.push_str(&format!(
                    "    - {} ({} bytes, blake3:{})\n",
                    s.path,
                    s.bytes,
                    &s.hash_blake3[..16.min(s.hash_blake3.len())]
                ));
            }
        }
    }

    CommandOutcome::Text(out)
}

/// `/agent persona bump <ref>` — re-resolve the agent's persona files and
/// write a fresh `PersonaSnapshot` to the active session. Use after
/// editing source files (e.g. ~/AGENTS.md) so existing sessions pick up
/// the change. Without a bump, an in-flight session keeps the snapshot
/// from its last attach/edit indefinitely (deterministic by design).
pub(super) async fn agent_persona_bump(
    agent_ref: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    // Pull persona from the runtime registry — it's already hydrated
    // from the AgentDb at message-time, and falls back to the legacy
    // `role:` migration when persona is unset.
    let agent = match ctx.server.agents().get(&entry.display_name) {
        Some(a) => a,
        None => {
            return CommandOutcome::Error(format!(
                "Agent '{}' is not in the runtime registry",
                entry.display_name
            ));
        }
    };
    let persona = match agent.persona {
        Some(p) => p,
        None => {
            return CommandOutcome::Error(format!(
                "Agent '{}' has no persona set. Use `/agent set {} persona.files <paths>` or `persona.prompt <text>`.",
                entry.display_name, entry.display_name
            ));
        }
    };

    if let Err(e) = crate::session::write_persona_snapshot(
        ctx.session_db,
        &entry.display_name,
        &persona,
        crate::persona::SnapshotReason::Bump,
    )
    .await
    {
        return CommandOutcome::Error(format!(
            "Failed to bump persona snapshot: {e}. Source files may have moved or be unreadable."
        ));
    }

    CommandOutcome::Text(format!(
        "Bumped persona snapshot for agent '{}' on this session.",
        entry.display_name
    ))
}

// -----------------------------------------------------------------------------
// Co-owned Agents (Stage 10): /pubkey + /agent invite + /agent revoke-peer
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

    CommandOutcome::Text(format!(
        "Revoked {pubkey_str} from agent '{}'. They retain read access to history but cannot write.",
        entry.display_name
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Command, CommandContext, CommandOutcome, dispatch};
    use crate::agent::AgentRegistry;
    use crate::agent_db::find_agent_db;
    use crate::backends::BackendManager;
    use crate::hosted_index::HostedIndex;
    use crate::security::SecretStore;
    use crate::server::Server;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use std::sync::Arc;

    /// End-to-end fixture: Server + SessionRegistry + one open session +
    /// SecretStore/BackendManager suitable for running commands::dispatch.
    /// Returns (instance, server, registry, secrets, backend, session_db_id, session_db).
    async fn fixture() -> (
        Instance,
        Arc<Server>,
        Arc<crate::session::SessionRegistry>,
        SecretStore,
        BackendManager,
        String,
        eidetica::Database,
    ) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
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
            tools,
            policies,
            security,
            std::collections::HashMap::new(),
            Default::default(),
            std::sync::Arc::new(crate::tool_host::NativeToolHost::new()),
            std::sync::Arc::new(crate::extension::ExtensionHub::new()),
            backend_mgr.clone(),
        );
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        (
            instance,
            server,
            registry,
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
            config_roles: None,
            default_role: None,
        }
    }

    #[tokio::test]
    async fn agent_new_writes_overrides_into_db_and_registers() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![
                ("model".into(), "opus".into()),
                ("max_iterations".into(), "42".into()),
                ("tools".into(), "get_time,calculate".into()),
            ],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(_) => {}
            _ => panic!("expected Text outcome, got non-matching variant"),
        }

        // Runtime registry reflects the overrides.
        let agent = server.agents().get("alpha").expect("agent registered");
        assert_eq!(agent.default_model.as_deref(), Some("opus"));
        assert_eq!(agent.max_iterations, 42);
        assert_eq!(
            agent.allowed_tools.as_deref(),
            Some(&["get_time".to_string(), "calculate".to_string()][..])
        );

        // Persisted config in the AgentDb matches too.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.expect("DB exists");
        drop(user);
        let cfg = db.read_config().await.unwrap();
        assert_eq!(cfg.model.as_deref(), Some("opus"));
        assert_eq!(cfg.max_iterations, Some(42));
    }

    #[tokio::test]
    async fn agent_new_rejects_unknown_override() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![("bogus".into(), "x".into())],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Unknown"), "got {msg}"),
            _ => panic!("expected Error, got non-matching variant"),
        }
        // Agent should NOT be registered.
        assert!(server.agents().get("alpha").is_none());
    }

    #[tokio::test]
    async fn agent_hosted_lists_registered_agents() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Before any /agent new, the index is empty.
        match dispatch(Command::AgentHosted, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("No Living Agents"), "got {msg}"),
            _ => panic!("expected Text, got non-matching variant"),
        }

        // Create two agents and verify they both appear.
        for name in ["alpha", "beta"] {
            let _ = dispatch(
                Command::AgentNew {
                    name: name.to_string(),
                    overrides: vec![],
                },
                &ctx,
            )
            .await;
        }
        match dispatch(Command::AgentHosted, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("alpha"), "missing alpha in {msg}");
                assert!(msg.contains("beta"), "missing beta in {msg}");
            }
            _ => panic!("expected Text, got non-matching variant"),
        }
    }

    #[tokio::test]
    async fn agent_delete_removes_from_index_and_registry() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        assert!(server.agents().get("alpha").is_some());

        let result = dispatch(Command::AgentDelete("alpha".to_string()), &ctx).await;
        match result {
            CommandOutcome::Text(msg) => assert!(msg.contains("Deleted")),
            _ => panic!("expected Text, got non-matching variant"),
        }

        // Gone from runtime registry.
        assert!(server.agents().get("alpha").is_none());
        // Gone from agents index.
        assert!(server.agent_index().find_by_name("alpha").is_none());
        // But the DB is still present (preserved for archive).
        let user = registry.user_for_tests().await;
        assert!(find_agent_db(&user, "alpha").await.is_some());
    }

    #[tokio::test]
    async fn agent_delete_refuses_if_attached_to_session() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        dispatch(Command::AgentAdd("alpha".to_string()), &ctx).await;

        let result = dispatch(Command::AgentDelete("alpha".to_string()), &ctx).await;
        match result {
            CommandOutcome::Error(msg) => assert!(msg.contains("still attached"), "got {msg}"),
            _ => panic!("expected Error, got non-matching variant"),
        }
        // Still registered.
        assert!(server.agents().get("alpha").is_some());
    }

    // -------------------------------------------------------------------------
    // /agent new — extended field coverage (can_spawn / allowed_callers / autonomous)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn agent_new_accepts_can_spawn_allowed_callers_autonomous() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![
                ("can_spawn".into(), "beta,gamma".into()),
                ("allowed_callers".into(), "chaz".into()),
                ("autonomous".into(), "true".into()),
            ],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(_) => {}
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        let agent = server.agents().get("alpha").unwrap();
        assert_eq!(
            agent.can_spawn,
            vec!["beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(agent.allowed_callers, vec!["chaz".to_string()]);
        assert!(agent.autonomous);

        // And persisted to the DB.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
        drop(user);
        let cfg = db.read_config().await.unwrap();
        assert_eq!(cfg.can_spawn, vec!["beta".to_string(), "gamma".to_string()]);
        assert_eq!(cfg.allowed_callers, vec!["chaz".to_string()]);
        assert!(cfg.autonomous);
    }

    #[tokio::test]
    async fn agent_new_rejects_invalid_bool_for_autonomous() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![("autonomous".into(), "maybe".into())],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Invalid bool"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    // -------------------------------------------------------------------------
    // /agent set — edit a single field on an existing agent
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn agent_set_updates_db_and_registry() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Create with a baseline model.
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![("model".into(), "haiku".into())],
            },
            &ctx,
        )
        .await;

        // Edit one field.
        let cmd = Command::AgentSet {
            agent_ref: "alpha".to_string(),
            field: "model".to_string(),
            value: "opus".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("alpha"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        // Runtime registry reflects the new value.
        assert_eq!(
            server
                .agents()
                .get("alpha")
                .unwrap()
                .default_model
                .as_deref(),
            Some("opus")
        );

        // DB reflects it too — Stage 8 hydration will read this on next message.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
        drop(user);
        assert_eq!(
            db.read_config().await.unwrap().model.as_deref(),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn agent_set_rejects_unknown_field() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;

        let cmd = Command::AgentSet {
            agent_ref: "alpha".to_string(),
            field: "bogus".to_string(),
            value: "x".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Unknown"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn agent_set_missing_agent_errors() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentSet {
            agent_ref: "ghost".to_string(),
            field: "model".to_string(),
            value: "opus".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("No hosted agent"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    // -------------------------------------------------------------------------
    // Co-owned Agents (Stage 10): /pubkey + /agent invite + /agent revoke-peer
    // -------------------------------------------------------------------------

    use super::super::CoOwnerPermission;

    #[tokio::test]
    async fn pubkey_returns_peer_default_key() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::Pubkey, &ctx).await {
            CommandOutcome::Text(s) => assert!(s.starts_with("ed25519:"), "got {s}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }
    }

    async fn fresh_invitee_pubkey(
        registry: &crate::session::SessionRegistry,
    ) -> eidetica::auth::crypto::PublicKey {
        // Synthesize a second pubkey via the registry's ephemeral-key helper —
        // in real use this is a remote peer's pubkey, but for tests any valid
        // pubkey distinct from our default works.
        registry.new_ephemeral_key("invitee:test").await.unwrap()
    }

    #[tokio::test]
    async fn agent_invite_admin_adds_key_to_agent_db_auth() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;

        let invitee_pk = fresh_invitee_pubkey(&registry).await;
        let cmd = Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee_pk.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("Invited"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        let entry = server.agent_index().find_by_name("alpha").unwrap();
        let agent_db = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .unwrap()
            .unwrap();
        let auth = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_eq!(
            auth.permissions(),
            &eidetica::auth::types::Permission::Admin(1)
        );
        assert_eq!(auth.status(), &eidetica::auth::types::KeyStatus::Active);
    }

    #[tokio::test]
    async fn agent_invite_write_permission() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        let invitee_pk = fresh_invitee_pubkey(&registry).await;
        let _ = dispatch(
            Command::AgentInvite {
                agent_ref: "alpha".to_string(),
                pubkey: invitee_pk.to_prefixed_string(),
                permission: CoOwnerPermission::Write,
            },
            &ctx,
        )
        .await;
        let entry = server.agent_index().find_by_name("alpha").unwrap();
        let agent_db = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .unwrap()
            .unwrap();
        let auth = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_eq!(
            auth.permissions(),
            &eidetica::auth::types::Permission::Write(10)
        );
    }

    #[tokio::test]
    async fn agent_invite_read_permission() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        let invitee_pk = fresh_invitee_pubkey(&registry).await;
        let _ = dispatch(
            Command::AgentInvite {
                agent_ref: "alpha".to_string(),
                pubkey: invitee_pk.to_prefixed_string(),
                permission: CoOwnerPermission::Read,
            },
            &ctx,
        )
        .await;
        let entry = server.agent_index().find_by_name("alpha").unwrap();
        let agent_db = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .unwrap()
            .unwrap();
        let auth = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_eq!(auth.permissions(), &eidetica::auth::types::Permission::Read);
    }

    #[tokio::test]
    async fn agent_invite_rejects_own_pubkey() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        let own_pk = server.agent_index().find_by_name("alpha").unwrap().pubkey;
        match dispatch(
            Command::AgentInvite {
                agent_ref: "alpha".to_string(),
                pubkey: own_pk.to_prefixed_string(),
                permission: CoOwnerPermission::Admin,
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => assert!(msg.contains("already own"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn agent_invite_rejects_malformed_pubkey() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        match dispatch(
            Command::AgentInvite {
                agent_ref: "alpha".to_string(),
                pubkey: "not a pubkey".to_string(),
                permission: CoOwnerPermission::Admin,
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => assert!(msg.contains("Invalid pubkey"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn agent_invite_unknown_agent_errors() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(
            Command::AgentInvite {
                agent_ref: "ghost".to_string(),
                pubkey: "ed25519:AAAA".to_string(),
                permission: CoOwnerPermission::Admin,
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => assert!(msg.contains("No hosted agent"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn agent_revoke_peer_removes_key() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        let invitee_pk = fresh_invitee_pubkey(&registry).await;
        dispatch(
            Command::AgentInvite {
                agent_ref: "alpha".to_string(),
                pubkey: invitee_pk.to_prefixed_string(),
                permission: CoOwnerPermission::Admin,
            },
            &ctx,
        )
        .await;

        match dispatch(
            Command::AgentRevokePeer {
                agent_ref: "alpha".to_string(),
                pubkey: invitee_pk.to_prefixed_string(),
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Text(msg) => assert!(msg.contains("Revoked"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        let entry = server.agent_index().find_by_name("alpha").unwrap();
        let agent_db = registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await
            .unwrap()
            .unwrap();
        let auth_after = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_ne!(
            auth_after.status(),
            &eidetica::auth::types::KeyStatus::Active
        );
    }

    #[tokio::test]
    async fn agent_revoke_peer_refuses_own_key() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        let own_pk = server.agent_index().find_by_name("alpha").unwrap().pubkey;
        match dispatch(
            Command::AgentRevokePeer {
                agent_ref: "alpha".to_string(),
                pubkey: own_pk.to_prefixed_string(),
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("/agent delete"), "got {msg}")
            }
            _ => panic!("expected Error"),
        }
    }
}

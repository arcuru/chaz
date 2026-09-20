//! Session, Matrix-channel, scheduler, and LLM-config handlers.
//!
//! These are the "operate on the current session" commands: list, create,
//! switch, info, name, share/sync, compact, print, channel listing,
//! scheduler pointers, and per-session LLM config (model/role/backend).

use crate::session::{EntryType, Session, SessionIndex, SessionRegistry};
use crate::types::ConversationId;
use futures::{StreamExt, stream};

use super::{CommandContext, CommandOutcome, SessionInfo, SessionSwitch};

// -----------------------------------------------------------------------------
// Session CRUD
// -----------------------------------------------------------------------------

pub(super) async fn list_sessions(ctx: &CommandContext<'_>) -> CommandOutcome {
    match collect_session_infos(ctx.server.registry()).await {
        Ok(sessions) => CommandOutcome::SessionsList(sessions),
        Err(e) => CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    }
}

/// Build the complete session listing without requiring a current session.
/// One-shot `/sessions` uses this so a read-only catalog command does not
/// manufacture and attach a throwaway CLI session first.
pub async fn collect_session_infos(registry: &SessionRegistry) -> anyhow::Result<Vec<SessionInfo>> {
    let indices = registry.list_sessions().await?;
    let mut sessions: Vec<_> = stream::iter(indices)
        .map(|index| load_session_metadata(registry, index))
        .buffer_unordered(8)
        .collect()
        .await;

    sort_session_infos(&mut sessions);

    Ok(sessions)
}

/// Load mutable metadata for one picker row without constructing a `Session`
/// or reading its transcript. A failed open leaves the catalog row usable.
pub async fn load_session_metadata(registry: &SessionRegistry, index: SessionIndex) -> SessionInfo {
    let meta = match registry.open_session(&index.session_db_id).await {
        Ok((_conv_id, db)) => Some(crate::session::read_meta_from_db(&db).await),
        Err(_) => None,
    };
    SessionInfo {
        session_db_id: index.session_db_id,
        agent_name: meta.as_ref().and_then(|meta| meta.agent_name.clone()),
        name: index.name.or_else(|| meta.and_then(|meta| meta.name)),
        bridge: index.bridge,
        created_at: index.created_at,
        status: index.status,
        loaded: true,
    }
}

/// Order sessions for display: most-recently created first, with legacy
/// (`created_at = None`) sessions sorted to the end so fresh sessions are
/// always near the top. Rows patch in place by id and never reorder as lazy
/// metadata arrives.
pub fn sort_session_infos(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|a, b| match (a.created_at, b.created_at) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.session_db_id.cmp(&b.session_db_id),
    });
}

pub(super) async fn new_session(group: Option<&str>, ctx: &CommandContext<'_>) -> CommandOutcome {
    // Resolve the requested group before creating anything — a typo
    // should report the known groups, not strand an empty session.
    if let Some(name) = group
        && ctx.server.agent_group(name).is_none()
    {
        return CommandOutcome::Error(format!(
            "Unknown agent group '{name}'. {}",
            known_groups_hint(ctx)
        ));
    }
    let (conv_id, db) = match ctx.server.registry().create_session(Some("tui")).await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to create session: {e}")),
    };
    let session_db_id = db.root_id().to_string();
    // Mirror routing reality in `meta.agents` so `/agents` and the
    // per-agent model picker reflect the agent that will actually answer.
    let _ = ctx.server.auto_attach_agents(&session_db_id, group).await;
    let agent = ctx
        .server
        .registry()
        .resolve_agent(&session_db_id, None, ctx.server.agent_index())
        .await;
    CommandOutcome::SessionSwitched(Box::new(SessionSwitch {
        session_db_id,
        conv_id,
        db,
        agent_name: agent.name,
        session_name: None,
    }))
}

/// One-line tail for "unknown group" errors, naming what *is* configured.
fn known_groups_hint(ctx: &CommandContext<'_>) -> String {
    let names = ctx.server.agent_group_names();
    if names.is_empty() {
        "No agent groups are configured (set `agent_groups:` in the config).".to_string()
    } else {
        format!("Known groups: {}", names.join(", "))
    }
}

pub(super) async fn list_agent_groups(ctx: &CommandContext<'_>) -> CommandOutcome {
    let names = ctx.server.agent_group_names();
    if names.is_empty() {
        return CommandOutcome::Text(
            "No agent groups configured. Add an `agent_groups:` block to the config to \
             start sessions with a named roster (`/new <group>`)."
                .to_string(),
        );
    }
    let mut out = String::from("Agent groups (start one with `/new <group>`):\n");
    for name in names {
        let members = ctx.server.agent_group(&name).unwrap_or_default();
        let members = if members.is_empty() {
            "(empty — attaches no agents)".to_string()
        } else {
            members.join(", ")
        };
        out.push_str(&format!("  {name}: {members}\n"));
    }
    let defaults = ctx.server.default_agents();
    if !defaults.is_empty() {
        out.push_str(&format!("  (default): {}\n", defaults.join(", ")));
    }
    CommandOutcome::Text(out)
}

pub(super) async fn switch_session(identifier: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let (conv_id, db) = match ctx.server.registry().resolve_session(identifier).await {
        Ok(r) => r,
        Err(e) => return CommandOutcome::Error(format!("Failed to switch session: {e}")),
    };

    let session_db_id = db.root_id().to_string();
    let meta = crate::session::read_meta_from_db(&db).await;

    let agent = ctx
        .server
        .registry()
        .resolve_agent(&session_db_id, None, ctx.server.agent_index())
        .await;

    CommandOutcome::SessionSwitched(Box::new(SessionSwitch {
        session_db_id,
        conv_id,
        db,
        agent_name: agent.name,
        session_name: meta.name,
    }))
}

pub(super) async fn info(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let entries = session.entries();
    let msg_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Message)
        .count();
    let tool_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::ToolCall)
        .count();
    let directive_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Directive)
        .count();
    let error_count = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Error)
        .count();
    let name_line = match ctx.session_name {
        Some(n) => format!("\nName: {n}"),
        None => String::new(),
    };
    let channels = ctx
        .server
        .registry()
        .channels_for_session(ctx.session_db_id)
        .await
        .unwrap_or_default();
    let channels_line = if channels.is_empty() {
        String::new()
    } else {
        let rooms: Vec<String> = channels.into_iter().map(|(_t, _l, c)| c).collect();
        format!("\nMatrix rooms: {}", rooms.join(", "))
    };
    let usage_line = format_usage_summary(entries);
    CommandOutcome::Text(format!(
        "Session: {}{name_line}\nAgent: {}{channels_line}\nTotal entries: {}\nMessages: {msg_count} | Directives: {directive_count} | Tool calls: {tool_count} | Errors: {error_count}{usage_line}",
        ctx.session_db_id,
        ctx.current_agent,
        entries.len(),
    ))
}

/// Roll up `ResponseMetadata` across every entry in the session and render
/// it as one or two extra lines for `/info`. Returns the empty string when
/// no entries carry metadata (legacy sessions or sessions whose backend
/// didn't surface usage), so the output stays clean for those cases.
fn format_usage_summary(entries: &[crate::session::SessionEntry]) -> String {
    let mut calls = 0u32;
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut cached = 0u64;
    let mut cost: f64 = 0.0;
    let mut saw_cost = false;
    let mut models: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for entry in entries {
        let Some(m) = &entry.metadata else { continue };
        calls += 1;
        prompt += m.usage.prompt_tokens as u64;
        completion += m.usage.completion_tokens as u64;
        cached += m.usage.cached_tokens.unwrap_or(0) as u64;
        if let Some(c) = m.usage.cost_usd {
            cost += c;
            saw_cost = true;
        }
        if !m.model.is_empty() {
            *models.entry(m.model.clone()).or_insert(0) += 1;
        }
    }
    if calls == 0 {
        return String::new();
    }
    let cached_part = if cached > 0 {
        format!(" ({cached} cached)")
    } else {
        String::new()
    };
    let cost_part = if saw_cost {
        format!(" | ${cost:.4}")
    } else {
        String::new()
    };
    let mut out = format!(
        "\nLLM usage: {calls} call{} | {prompt} prompt + {completion} completion{cached_part}{cost_part}",
        if calls == 1 { "" } else { "s" }
    );
    if !models.is_empty() {
        let mut pairs: Vec<(String, u32)> = models.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let rendered: Vec<String> = pairs
            .into_iter()
            .map(|(name, n)| format!("{name} ({n})"))
            .collect();
        out.push_str(&format!("\nModels: {}", rendered.join(", ")));
    }
    out
}

pub(super) async fn list_costs(ctx: &CommandContext<'_>) -> CommandOutcome {
    let registry = ctx.server.registry();
    match crate::session::usage::collect_usage(registry, &Default::default()).await {
        Ok(rollup) => CommandOutcome::Text(crate::session::usage::render_text(&rollup)),
        Err(e) => CommandOutcome::Error(format!("Failed to collect usage: {e}")),
    }
}

pub(super) async fn name_session(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    if name.is_empty() {
        return CommandOutcome::Error("Usage: name <alias>".to_string());
    }
    match ctx
        .server
        .registry()
        .set_session_name(ctx.session_db_id, name.to_string())
        .await
    {
        Ok(()) => CommandOutcome::Text(format!("Session named '{name}'. Use it with join {name}.")),
        Err(e) => CommandOutcome::Error(format!("Failed to name session: {e}")),
    }
}

pub(super) async fn clear_session_name(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx
        .server
        .registry()
        .clear_session_name(ctx.session_db_id)
        .await
    {
        Ok(()) => CommandOutcome::Text("Session name cleared.".to_string()),
        Err(e) => CommandOutcome::Error(format!("Failed to clear name: {e}")),
    }
}

pub(super) async fn share(ctx: &CommandContext<'_>) -> CommandOutcome {
    let instance = ctx.server.registry().instance();
    if instance.sync().is_none() {
        return CommandOutcome::Error("Sync not enabled".to_string());
    }
    let db_id = ctx.session_db.root_id().clone();
    let ticket = match ctx.server.registry().share_for(&db_id).await {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Failed to share session: {e}")),
    };
    CommandOutcome::Text(format!(
        "Share this ticket to sync the current session:\n\n{ticket}"
    ))
}

pub(super) async fn interrupted(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx.server.interrupted_turns(ctx.session_db_id).await {
        Ok(turns) if turns.is_empty() => CommandOutcome::Text("No interrupted turns.".into()),
        Ok(turns) => CommandOutcome::Text(
            turns
                .into_iter()
                .map(|turn| {
                    format!(
                        "{}\tattempt {}\tinterrupted; retry explicitly with `/retry {}`",
                        turn.request_id, turn.attempt_id, turn.request_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Err(error) => {
            CommandOutcome::Error(format!("Failed to inspect interrupted turns: {error}"))
        }
    }
}

pub(super) async fn retry_interrupted(
    raw_request_id: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let request_id = crate::session::TurnRequestId::parse(raw_request_id);
    let interrupted = match ctx.server.interrupted_turns(ctx.session_db_id).await {
        Ok(turns) => turns,
        Err(error) => return CommandOutcome::Error(format!("Retry refused: {error}")),
    };
    let Some(target) = interrupted
        .into_iter()
        .find(|turn| turn.request_id == request_id)
    else {
        return CommandOutcome::Error(format!(
            "Retry refused: turn request {request_id} is not interrupted"
        ));
    };
    submit_command(
        crate::session::SessionCommand::Retry {
            target_request_id: request_id,
            expected_interrupted_attempt_id: target.attempt_id,
        },
        ctx,
    )
    .await
}

/// Disable sync on the current session so this peer stops serving it.
pub(super) async fn unshare(ctx: &CommandContext<'_>) -> CommandOutcome {
    let db_id = ctx.session_db.root_id().clone();
    match ctx.server.registry().disable_sync_for(&db_id).await {
        Ok(()) => CommandOutcome::Text(
            "Sync disabled for this session — it is no longer shared.".to_string(),
        ),
        Err(e) => CommandOutcome::Error(format!("Failed to disable sync: {e}")),
    }
}

pub(super) async fn sync_ticket(ticket_str: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Invalid ticket: {e}")),
    };
    let db_id = ticket.database_id().clone();
    // Sessions don't have a Read mode today (no read-only spectator UX), so
    // /sync always requests Write. If the requester's pubkey is preseeded
    // the sync proceeds; otherwise eidetica queues a bootstrap request.
    match ctx
        .server
        .registry()
        .request_db_access(&ticket, eidetica::auth::types::Permission::Write(10))
        .await
    {
        Ok(crate::session::BootstrapOutcome::Approved) => {}
        Ok(crate::session::BootstrapOutcome::Pending {
            request_id,
            message: _,
        }) => {
            return CommandOutcome::Text(format!(
                "Bootstrap request {request_id} pending the owner's approval. \
                 Re-run `/sync <ticket>` after they run `/sharing approve {request_id}`."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Bootstrap failed: {e}")),
    }
    if let Err(e) = ctx.server.registry().enable_sync_for(&db_id).await {
        return CommandOutcome::Error(format!(
            "Synced {db_id} but failed to enable ongoing sync: {e}"
        ));
    }
    CommandOutcome::Text(format!("Synced database {db_id}. Use sessions to find it."))
}

pub(super) async fn compact(ctx: &CommandContext<'_>) -> CommandOutcome {
    let source_snapshot = match ctx.session_db.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return CommandOutcome::Error(format!("Failed to snapshot session: {error}"));
        }
    };
    submit_command(
        crate::session::SessionCommand::Compact { source_snapshot },
        ctx,
    )
    .await
}

async fn submit_command(
    command: crate::session::SessionCommand,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let command_id = crate::session::TurnRequestId::parse(uuid::Uuid::new_v4().to_string());
    let request = crate::session::SessionCommandRequest {
        command_id: command_id.clone(),
        sender: ctx.current_agent.to_string(),
        created_at: chrono::Utc::now(),
        command,
    };
    let observer = match ctx.server.observe_session_command(ctx.session_db_id).await {
        Ok(observer) => observer,
        Err(error) => {
            return CommandOutcome::Error(format!(
                "Command {command_id} was not submitted: {error}"
            ));
        }
    };
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    if let Err(error) = session.submit_command(request).await {
        return CommandOutcome::Error(format!("Command {command_id} was not submitted: {error}"));
    }
    let waited = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        observer.wait(&command_id),
    )
    .await;
    match waited {
        Ok(Ok(result)) => match result.outcome {
            crate::session::SessionCommandOutcome::Compact { summary } => {
                CommandOutcome::Text(format!(
                    "Session compacted by command {command_id}. Summary ({} chars) persisted.",
                    summary.len()
                ))
            }
            crate::session::SessionCommandOutcome::RetryAccepted { target_attempt_id } => {
                CommandOutcome::Text(format!(
                    "Retry command {command_id} accepted as attempt {target_attempt_id}."
                ))
            }
            crate::session::SessionCommandOutcome::Rejected { message }
            | crate::session::SessionCommandOutcome::Failed { message } => {
                CommandOutcome::Error(format!("Command {command_id} failed: {message}"))
            }
        },
        Ok(Err(error)) => CommandOutcome::Error(format!("Command {command_id}: {error}")),
        Err(_) => CommandOutcome::Error(format!(
            "Command {command_id} remains durable but did not finish within 30 seconds"
        )),
    }
}

pub(super) async fn print_transcript(ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let mut buf = String::new();
    for entry in session.entries() {
        let label: &str = if entry.sender == ctx.current_agent {
            "assistant"
        } else {
            entry.sender.as_str()
        };
        let type_label = match entry.entry_type {
            EntryType::Directive => " [directive]",
            EntryType::Summary => " [summary]",
            EntryType::Error => " [error]",
            _ => "",
        };
        if matches!(
            entry.entry_type,
            EntryType::Message | EntryType::Directive | EntryType::Summary | EntryType::Error
        ) {
            buf.push_str(&format!("{label}{type_label}: {}\n", entry.content));
        }
    }
    if buf.is_empty() {
        CommandOutcome::Text("(empty)".to_string())
    } else {
        CommandOutcome::Text(buf)
    }
}

// -----------------------------------------------------------------------------
// Matrix channels (listing is transport-neutral; attach/detach are per-bridge)
// -----------------------------------------------------------------------------

pub(super) async fn list_channels(ctx: &CommandContext<'_>) -> CommandOutcome {
    match ctx
        .server
        .registry()
        .channels_for_session(ctx.session_db_id)
        .await
    {
        Ok(channels) if channels.is_empty() => {
            CommandOutcome::Text("No Matrix rooms attached to this session.".to_string())
        }
        Ok(channels) => {
            let rooms: Vec<String> = channels.into_iter().map(|(_t, _l, c)| c).collect();
            CommandOutcome::Text(format!(
                "Matrix rooms attached to this session:\n  {}",
                rooms.join("\n  ")
            ))
        }
        Err(e) => CommandOutcome::Error(format!("Failed to list channels: {e}")),
    }
}

// -----------------------------------------------------------------------------
// LLM config (per-session)
// -----------------------------------------------------------------------------

pub(super) async fn model(arg: Option<String>, ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    match arg {
        None => {
            // Mirror the runtime: per-agent override → per-session pin →
            // agent default → backend default. Matches what `runtime::execute`
            // routes to so the display agrees with reality.
            let meta = session.read_meta().await;
            let agent_default = ctx
                .server
                .agents()
                .get(ctx.current_agent)
                .and_then(|a| a.default_model.clone());
            let per_agent = meta.agent_models.get(ctx.current_agent).cloned();
            let effective = per_agent
                .clone()
                .or_else(|| meta.model.clone())
                .or(agent_default);
            let resolved = ctx.backend.resolve_model_name(effective.as_deref());
            let current = if resolved.is_empty() {
                "unknown".to_string()
            } else {
                resolved
            };
            let source = if per_agent.is_some() {
                format!(" (per-agent override on {})", ctx.current_agent)
            } else if meta.model.is_some() {
                " (session pin)".to_string()
            } else if effective.is_some() {
                " (agent default)".to_string()
            } else {
                " (backend default)".to_string()
            };
            let mut msg = format!("Current Model: {current}{source}");
            if let Some(pin) = &meta.model {
                msg.push_str(&format!("\nSession pin: {pin}"));
            }
            if !meta.agent_models.is_empty() {
                msg.push_str("\nPer-agent overrides:");
                let mut entries: Vec<(&String, &String)> = meta.agent_models.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (agent, model_id) in entries {
                    msg.push_str(&format!("\n  {agent}: {model_id}"));
                }
            }
            msg.push_str(&format!(
                "\n\nKnown Backends:\n{}",
                ctx.backend.list_known_backends().join("\n")
            ));
            msg.push_str("\n\nKnown Models:\n");
            msg.push_str(&ctx.backend.list_known_models().join("\n"));
            CommandOutcome::Text(msg)
        }
        Some(m) => {
            let note = if ctx.backend.is_known_model(&m) {
                format!("Model set to \"{m}\"")
            } else {
                match ctx.backend.validate_model(&m) {
                    Ok(()) => format!(
                        "Model set to \"{m}\" (not in known list — verify your backend supports it)"
                    ),
                    Err(e) => return CommandOutcome::Error(e),
                }
            };
            let m_clone = m.clone();
            if let Err(e) = session.update_meta(|meta| meta.model = Some(m_clone)).await {
                return CommandOutcome::Error(format!("Failed to set model: {e}"));
            }
            CommandOutcome::Text(note)
        }
    }
}

/// Set or clear a per-agent model override for the current session.
/// `Some(id)` pins that agent to the given model; `None` clears the
/// override so the agent falls back to the session pin / its own default.
/// `agent` is matched case-sensitively against `AgentRef.display_name`
/// on the session meta; agents that aren't currently attached produce a
/// warning but the override is written anyway (so you can pre-pin before
/// attaching).
pub(super) async fn agent_model(
    agent: &str,
    model_arg: Option<String>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    if agent.is_empty() {
        return CommandOutcome::Error(
            "Agent name required. Usage: /model <agent> <model-id> | /model <agent> clear".into(),
        );
    }
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let meta = session.read_meta().await;
    let known_agent = meta.agents.iter().any(|a| a.display_name == agent);

    match model_arg {
        Some(m) => {
            // Validate the model before writing, same surface as `/model <id>`.
            let note = if ctx.backend.is_known_model(&m) {
                format!("Model for {agent} set to \"{m}\"")
            } else {
                match ctx.backend.validate_model(&m) {
                    Ok(()) => format!(
                        "Model for {agent} set to \"{m}\" (not in known list — verify your backend supports it)"
                    ),
                    Err(e) => return CommandOutcome::Error(e),
                }
            };
            let agent_owned = agent.to_string();
            let m_owned = m.clone();
            if let Err(e) = session
                .update_meta(|meta| {
                    meta.agent_models.insert(agent_owned, m_owned);
                })
                .await
            {
                return CommandOutcome::Error(format!("Failed to set per-agent model: {e}"));
            }
            let suffix = if known_agent {
                String::new()
            } else {
                format!(
                    " (note: {agent} is not currently attached to this session — override saved anyway)"
                )
            };
            CommandOutcome::Text(format!("{note}{suffix}"))
        }
        None => {
            let agent_owned = agent.to_string();
            let mut had_override = false;
            if let Err(e) = session
                .update_meta(|meta| {
                    had_override = meta.agent_models.remove(&agent_owned).is_some();
                })
                .await
            {
                return CommandOutcome::Error(format!("Failed to clear per-agent model: {e}"));
            }
            if had_override {
                CommandOutcome::Text(format!("Cleared per-agent model override for {agent}"))
            } else {
                CommandOutcome::Text(format!("No per-agent override was set for {agent}"))
            }
        }
    }
}

pub(super) async fn role(
    arg: Option<(String, Option<String>)>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    match arg {
        None => {
            let meta = session.read_meta().await;
            let current_role = meta.role_name.unwrap_or_else(|| "none".to_string());
            let role_prompt = meta.role_prompt.as_deref().unwrap_or("(none)");
            let msg = format!(
                "Current Role: {current_role}\nPrompt: {role_prompt}\n\n\
                 Roles are deprecated. Use per-agent system_prompt: /agent set <name> system_prompt <text>"
            );
            CommandOutcome::Text(msg)
        }
        Some((name, prompt)) => {
            let name_clone = name.clone();
            let prompt_clone = prompt.clone();
            if let Err(e) = session
                .update_meta(|meta| {
                    meta.role_name = Some(name_clone);
                    if let Some(p) = prompt_clone {
                        meta.role_prompt = Some(p);
                    }
                })
                .await
            {
                return CommandOutcome::Error(format!("Failed to set role: {e}"));
            }
            CommandOutcome::Text(format!("Role set to \"{name}\""))
        }
    }
}

pub(super) async fn set_backend(
    name: &str,
    url: &str,
    api_key: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let ref_id = format!("session:{}:{name}", ctx.session_db_id);
    ctx.secrets
        .insert(ref_id.clone(), api_key.to_string())
        .await;
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;
    let name_owned = name.to_string();
    let url_owned = url.to_string();
    let ref_id_clone = ref_id.clone();
    if let Err(e) = session
        .update_meta(|meta| {
            meta.backend_name = Some(name_owned);
            meta.backend_url = Some(url_owned);
            meta.backend_key_ref = Some(ref_id_clone);
        })
        .await
    {
        return CommandOutcome::Error(format!("Failed to set backend: {e}"));
    }
    CommandOutcome::Text(format!("Successfully added backend {name}"))
}

pub(super) async fn list_backends(ctx: &CommandContext<'_>) -> CommandOutcome {
    let msg = format!(
        "Known Backends:\n{}\n\nKnown Models:\n{}",
        ctx.backend.list_known_backends().join("\n"),
        ctx.backend.list_known_models().join("\n")
    );
    CommandOutcome::Text(msg)
}

#[cfg(test)]
mod listing_tests {
    use super::*;
    use crate::session::{EntryType, Session, SessionEntry};
    use crate::test_support::fresh_session_registry;

    #[tokio::test]
    async fn listing_is_metadata_only_regardless_of_transcript_size() {
        let (instance, registry) = fresh_session_registry().await;
        let (conv, db) = registry.create_session(Some("cli")).await.unwrap();
        let id = db.root_id().to_string();
        registry
            .set_session_name(&id, "large-session".into())
            .await
            .unwrap();

        let mut session = Session::new(conv, db).await;
        for n in 0..128 {
            session
                .add_entry(SessionEntry {
                    sender: "user".into(),
                    content: format!("transcript payload {n}"),
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                    routing: None,
                })
                .await
                .unwrap();
        }

        let engine = instance.backend().local_engine().unwrap();
        let memory = engine
            .as_any()
            .downcast_ref::<eidetica::backend::database::InMemory>()
            .unwrap();
        let root = eidetica::entry::ID::parse(&id).unwrap();
        instance
            .backend()
            .clear_derived_store_state()
            .await
            .unwrap();
        assert_eq!(memory.store_state_record_count(&root, "entries"), 0);
        let rows = collect_session_infos(&registry).await.unwrap();

        let row = rows.iter().find(|row| row.session_db_id == id).unwrap();
        assert_eq!(row.name.as_deref(), Some("large-session"));
        assert_eq!(
            memory.store_state_record_count(&root, "entries"),
            0,
            "listing must not materialize transcript rows"
        );

        let (_conv, db) = registry.open_session(&id).await.unwrap();
        let loaded = Session::new(crate::types::ConversationId(id), db).await;
        assert_eq!(loaded.entries().len(), 128);
        assert_eq!(
            memory.store_state_record_count(loaded.database().root_id(), "entries"),
            128,
            "negative control: constructing Session materializes transcript rows"
        );
    }
}

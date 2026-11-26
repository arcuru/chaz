//! Native scheduling evidence check for the disposable Matrix E2E fixture.
//! Open a SQLite backup, never the running daemon's embedded backend or cache.
use anyhow::{Context, ensure};
use chaz_core::agent_db::{ScheduleTarget, find_agent_db};
use chaz_core::session::{EntryType, Session, outcome::TurnOutcome};
use chaz_core::types::ConversationId;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    connection: String,
    #[arg(long)]
    schedule: String,
    #[arg(long)]
    prompt: String,
    #[arg(long)]
    final_text: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let instance = eidetica::Instance::connect(&args.connection).await?;
    let user = instance.login_account("chaz", "test-password").await?;
    let (agent, key) = find_agent_db(&user, "chaz")
        .await
        .context("fixture Agent missing")?;
    let registry = chaz_core::session::SessionRegistry::new(
        instance,
        user,
        std::sync::Arc::new(chaz_core::agent::AgentRegistry::default()),
    )
    .await?;
    let agent = registry
        .open_scheduling_agent_db(&agent.id(), Some(&key))
        .await?
        .context("selected scheduling Agent unavailable")?;
    let definition = agent
        .find_schedule(&args.schedule)
        .await?
        .context("definition missing")?;
    let ScheduleTarget::Pinned { session_db_id } = &definition.target else {
        anyhow::bail!("fixture needs a Pinned schedule");
    };
    ensure!(
        definition.prompt == args.prompt
            && definition.enabled
            && definition.fire_count == 0
            && definition.max_fires == Some(1),
        "definition was changed by firing"
    );
    let effective = agent
        .effective_schedule(&args.schedule)
        .await?
        .context("effective schedule missing")?;
    ensure!(
        !effective.enabled && effective.fire_count == 1,
        "last slot was not consumed"
    );
    let fires: Vec<_> = agent
        .list_schedule_fires()
        .await?
        .into_iter()
        .filter(|fire| fire.schedule_id == args.schedule)
        .collect();
    ensure!(
        fires.len() == 1,
        "expected exactly one handoff, got {}",
        fires.len()
    );
    let fire = &fires[0];
    ensure!(
        !fire.fresh
            && &fire.session_db_id == session_db_id
            && fire.lifetime.as_deref() == Some(definition.lifetime_key().as_str()),
        "handoff target/lifetime differs"
    );
    let db = registry
        .user_lock()
        .await
        .open_database(&eidetica::entry::ID::parse(session_db_id)?)
        .await?;
    let session = Session::try_new(ConversationId(session_db_id.clone()), db).await?;
    let rows = agent.schedule_occurrences().await?;
    let row = rows
        .iter()
        .find(|row| row.reference.schedule_id == args.schedule)
        .context("timed occurrence missing")?;
    ensure!(
        rows.iter()
            .filter(|row| row.reference.schedule_id == args.schedule)
            .count()
            == 1,
        "duplicate timed occurrence"
    );
    ensure!(
        row.clock.is_some()
            && row.reference.delivery_id == *fire.firing_id.as_ref().context("delivery missing")?,
        "handoff is not the timed lifecycle/common path"
    );
    let evidence = session
        .schedule_delivery_evidence(&row.reference)
        .await?
        .context("delivery receipt missing")?;
    let attempt_id = &evidence.input.attempt_id;
    let request_id = &evidence.input.request_id;
    ensure!(
        fire.attempt_id.is_none()
            && fire.request_id.as_ref() == Some(&evidence.input.anchor.request_id),
        "handoff incorrectly inferred Started or changed the inert Ack"
    );
    ensure!(
        matches!(
            evidence.input.state,
            chaz_core::session::steering::JobInputState::Included { .. }
        ),
        "scheduled private input not Included"
    );
    let completion = session
        .turn_completion(attempt_id)
        .await?
        .context("terminal outcome missing")?;
    ensure!(
        completion.request_id == *request_id && completion.outcome == TurnOutcome::Success,
        "handoff has no successful terminal execution"
    );
    let finals = session
        .entries()
        .iter()
        .filter(|entry| {
            entry.entry_type == EntryType::Message
                && entry.sender == "chaz"
                && entry.content == args.final_text
                && entry.routing.is_none()
                && entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.turn_attempt_id.as_ref())
                    == Some(attempt_id)
        })
        .count();
    ensure!(
        finals == 1,
        "expected one matching local-only final, got {finals}"
    );
    println!(
        "{}",
        serde_json::json!({"handoffs": fires.len(), "local_finals": finals,
        "request_id": request_id, "attempt_id": attempt_id, "outcome": completion.outcome,
        "capacity_used": effective.fire_count})
    );
    Ok(())
}

//! The pinned fixture has one live transaction per execution. It has no
//! reopen-to-execute path, durable dispatch lease, or external effect grant.
use super::*;
use crate::{agent_db::AgentDb, session::RecordRef};
use eidetica::entry::ID;

pub(super) struct PinnedTurn {
    agent: AgentDb,
    config: RecordRef,
    pub(super) model: Option<Execution>,
    pub(super) selection: Option<crate::session::CallInput>,
    pub(super) tool: Option<(u64, usize, Execution)>,
    pub(super) terminal: Option<ID>,
}

impl PinnedTurn {
    pub(super) async fn require_config_access(&self) -> anyhow::Result<()> {
        self.agent.read_config_ref(&self.config).await?;
        Ok(())
    }
}

pub(super) struct Execution {
    pub(super) at: Snapshot,
    pub(super) txn: Transaction,
}

impl Execution {
    pub(super) async fn current(db: &Database) -> anyhow::Result<Self> {
        // This verified-history read runs inside both the model and tool
        // runtime futures, which already have substantial debug frames.
        let at = Box::pin(Session::history(db)).await?.snapshot().clone();
        let txn = db.new_transaction_at(&at).await?;
        Ok(Self { at, txn })
    }
}

impl TurnRecorder {
    pub(in crate::session::turn_recorder) async fn fresh_pinned_context(
        &self,
        fenced: &Mutex<FencedTurn>,
        messages: &[RuntimeMessage],
        sources: &[ContextSource],
    ) -> anyhow::Result<Option<crate::runtime::CallContext>> {
        if fenced.lock().await.pinned.is_none() {
            return Ok(None);
        }
        let session = self.write_session().await.map_err(anyhow::Error::msg)?;
        let mut state = fenced.lock().await;
        state.require_active(&session, &self.attempt).await?;
        let pin = state.pinned.as_mut().context("missing pinned turn")?;
        ensure!(
            pin.model.is_none() && pin.tool.is_none(),
            "execution already in progress"
        );
        let config = pin.agent.read_config_ref(&pin.config).await?;
        ensure!(
            config.prompt_sources.is_none()
                && config.system_prompt_ref.is_none()
                && config.system_prompt_files.is_empty(),
            "fixture only reads inline pinned instructions"
        );
        let db = session.database();
        let exec = Box::pin(Execution::current(db)).await?;
        let call = self.call_id(0);
        let staged = Box::pin(Staging::at(db, &exec.at, &exec.txn, &call.branch)).await?;
        let mut fresh = crate::runtime::CallContext {
            messages: Vec::new(),
            sources: Vec::new(),
        };
        if !config.system_prompt.is_empty() {
            fresh
                .messages
                .push(RuntimeMessage::System(config.system_prompt));
            fresh.sources.push(ContextSource::Instructions);
        }
        // The one inline prompt comes from the pinned Agent revision, not
        // from caller-selected latest config. Preserve actual hook injections.
        for (message, source) in messages.iter().zip(sources) {
            if *source == ContextSource::TurnInjection {
                fresh.messages.push(message.clone());
                fresh.sources.push(source.clone());
            }
        }
        for group in staged.context()?.groups {
            let HistoryBody::Recorded { messages, .. } = group.body else {
                anyhow::bail!("pinned fixture requires signed context");
            };
            for (message, id) in messages {
                fresh.messages.push(message);
                fresh
                    .sources
                    .push(ContextSource::ConversationRepresentation { id });
            }
        }
        pin.model = Some(exec);
        Ok(Some(fresh))
    }

    pub(in crate::session::turn_recorder) async fn begin_pinned_tool(
        &self,
        fenced: &Mutex<FencedTurn>,
        sequence: u64,
        index: usize,
        name: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<()> {
        if fenced.lock().await.pinned.is_none() {
            return Ok(());
        }
        let session = self.write_session().await.map_err(anyhow::Error::msg)?;
        let mut state = fenced.lock().await;
        state.require_active(&session, &self.attempt).await?;
        let pin = state.pinned.as_ref().context("missing pinned turn")?;
        ensure!(
            pin.tool.is_none() && pin.model.is_none(),
            "execution already in progress"
        );
        pin.agent.read_config_ref(&pin.config).await?;
        let db = session.database();
        let exec = Box::pin(Execution::current(db)).await?;
        let response = state
            .responses
            .get(&sequence)
            .context("tool has no native response")?;
        let id = self.call_id(sequence);
        let staged = Box::pin(Staging::at(db, &exec.at, &exec.txn, &id.branch)).await?;
        let Payload::NativeModelResponse { response } = staged.payload(response)? else {
            anyhow::bail!("tool has no native call");
        };
        let call = response
            .tool_calls
            .get(index)
            .context("invalid tool occurrence")?;
        ensure!(
            call.name == name
                && serde_json::from_str::<serde_json::Value>(&call.arguments)? == *args,
            "fixture cannot execute altered tool arguments"
        );
        state.pinned.as_mut().unwrap().tool = Some((sequence, index, exec));
        Ok(())
    }
}

#[cfg(test)]
impl Session {
    /// Creation/Agent join and explicit update are the only pin writes. The
    /// fixture accepts one already authorized Agent handle, not a revision
    /// chosen by the caller of a turn.
    pub(in crate::session) async fn fixture_pin_agent(
        session: &Arc<Mutex<Self>>,
        agent: &AgentDb,
        update: bool,
    ) -> anyhow::Result<ID> {
        let mut session = session.lock().await;
        session.refresh_writer().await?;
        let db = session.database();
        ensure!(
            db.current_permission().await?.can_write(),
            "no current Session write permission"
        );
        let read = Self::history(db).await?;
        let root = Snapshot::from([db.root_id().clone()]);
        let at = read.snapshot();
        let txn = db.new_transaction_at(at).await?;
        let (_, config) = agent
            .read_config_bound(&agent.database().snapshot().await?)
            .await?;
        if at == &root {
            ensure!(
                !update && read.entries().await?.is_empty(),
                "expected fresh Session for first pin"
            );
            stage_pinned_descriptor(&txn, db, at).await?;
        } else {
            let previous = read.agent_pin(&agent.id()).await?;
            ensure!(
                update == previous.is_some(),
                "pin join/update does not match membership"
            );
            if let Some((id, current)) = previous.filter(|(_, prior)| *prior == &config) {
                debug_assert_eq!(current, &config);
                return Ok(id.clone());
            }
        }
        crate::session::conversation::pins::stage(&txn, config, update).await?;
        Ok(txn.commit().await?)
    }

    pub(in crate::session) async fn prepare_pinned_fixture(
        session: &Arc<Mutex<Self>>,
        agent: &AgentDb,
        entry: SessionEntry,
    ) -> anyhow::Result<PreparedPinnedInput> {
        let session = session.lock().await;
        let db = session.database();
        let exec = Execution::current(db).await?;
        let staged = Staging::at(db, &exec.at, &exec.txn, "fixture").await?;
        Ok(staged.prepare_pinned_input(entry, &agent.id())?)
    }

    /// Only a first successful local acceptance returns an executor. Lost
    /// acknowledgement is readback evidence, never permission to dispatch.
    pub(in crate::session) async fn start_pinned_fixture(
        session: Arc<Mutex<Self>>,
        prepared: &PreparedPinnedInput,
        agent: AgentDb,
        display: String,
        cancel: TurnCancel,
    ) -> anyhow::Result<(ID, TurnRecorder)> {
        let mut guard = session.lock().await;
        guard.refresh_writer().await?;
        let db = guard.database();
        ensure!(
            db.current_permission().await?.can_write() && !cancel.is_cancelled(),
            "cannot accept input without live writer"
        );
        let current = Self::history(db).await?;
        ensure!(
            prepared.readback(db, current.snapshot()).await?.is_none(),
            "input already accepted; cannot dispatch again"
        );
        let at = &prepared.op.observed.frontier;
        let original = Self::history_at(db, at).await?;
        let (pin, config) = original
            .agent_pin(&agent.id())
            .await?
            .context("no pinned Agent")?;
        ensure!(
            Some(pin) == prepared.accepted.pin.as_ref(),
            "input refers to a different Agent pin"
        );
        // A new input must also refuse an ambiguous live pin. A unique new
        // update is allowed to be a sibling of an already prepared input.
        current
            .agent_pin(&agent.id())
            .await?
            .context("no current Agent membership")?;
        agent.read_config_ref(config).await?;
        let config = config.clone();
        ensure!(
            prepared.reserve_once(),
            "input acceptance already attempted; inspect readback, do not dispatch again"
        );
        let txn = db.new_transaction_at(at).await?;
        let staged = Staging::at(db, at, &txn, &prepared.accepted.branch).await?;
        prepared.stage(&staged).await?;
        let accepted = txn.commit().await?;
        drop(guard);
        let mut recorder = Self::turn_recorder(
            session,
            &prepared.attempt,
            TurnCallIdentity {
                agent: display,
                requested_model: String::new(),
                execution_id: Some(prepared.accepted.branch.clone()),
            },
            Default::default(),
        );
        recorder.fenced = Some(Mutex::new(FencedTurn {
            legacy: None,
            pinned: Some(PinnedTurn {
                agent,
                config,
                model: None,
                selection: None,
                tool: None,
                terminal: None,
            }),
            compaction: None,
            cancel: Some(cancel),
            input: prepared.accepted.representation(),
            next_model: 0,
            pending: None,
            responses: Default::default(),
            additions: Default::default(),
            tools: Vec::new(),
            exchanges: Default::default(),
            terminal: None,
            fail: None,
        }));
        Ok((accepted, recorder))
    }
}

//! Worker inspection uses only read callbacks and snapshots, never registration.
use super::*;

pub(super) async fn switch_session(identifier: &str, server: &Server) -> CommandOutcome {
    let result = async {
        let id = server
            .registry()
            .find_by_name(identifier)
            .await?
            .unwrap_or_else(|| identifier.to_owned());
        let (conv_id, db) = server.registry().open_session_for_inspection(&id).await?;
        let meta = chaz_core::session::try_read_meta_from_db(&db).await?;
        let agent = server
            .registry()
            .resolve_agent(&id, None, server.agent_index())
            .await;
        Ok::<_, anyhow::Error>(CommandOutcome::SessionSwitched(Box::new(
            commands::SessionSwitch {
                session_db_id: id,
                conv_id,
                db,
                agent_name: agent.name.clone(),
                session_name: meta.name,
            },
        )))
    }
    .await;
    result.unwrap_or_else(|e| CommandOutcome::Error(format!("Failed to switch session: {e}")))
}

pub(super) async fn open_tab(
    server: &Server,
    backend: &BackendManager,
    id: &str,
    tx: mpsc::Sender<String>,
) -> Result<Tab, String> {
    let (_, db) = server
        .registry()
        .open_worker_session(id)
        .await
        .map_err(|e| e.to_string())?;
    // Install before reading, closing the read/subscribe gap. The existing job
    // callback is nonexecuting and requires the same native materialization floor.
    let observation = jobs::observe(&db, tx).await.map_err(|e| e.to_string())?;
    let mut tab = build_tab(server, backend, db, id.to_owned()).await?;
    tab.live.worker_observer = true;
    tab.live.observation = Some(observation);
    Ok(tab)
}

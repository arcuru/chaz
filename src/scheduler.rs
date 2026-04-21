//! Cron-driven scheduled task execution.
//!
//! The scheduler is a background task that writes Directive entries to sessions
//! on a cron schedule. The existing server callback machinery handles agent
//! execution — the scheduler just provides the trigger.
//!
//! Schedule state (last_run) is persisted in the central eidetica database
//! so that restarts don't cause duplicate or missed runs.

use crate::backends::BackendManager;
use crate::config::ScheduleConfig;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use chrono::{DateTime, Utc};
use cron::Schedule;
use eidetica::store::Table;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Persisted schedule state in eidetica central DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduleState {
    name: String,
    last_run: String, // ISO 8601
}

/// A parsed schedule ready for execution.
struct ScheduleRecord {
    name: String,
    /// Session identifier — session name or eidetica DB root ID (resolved at fire time)
    session_id: String,
    task: String,
    cron: Schedule,
    enabled: bool,
    last_run: Option<DateTime<Utc>>,
}

/// Cron-driven scheduler that writes Directive entries to sessions.
pub struct Scheduler {
    records: Arc<Mutex<Vec<ScheduleRecord>>>,
    server: Arc<Server>,
    backend: BackendManager,
    central_db: Database,
}

impl Scheduler {
    /// Create a new scheduler from config.
    ///
    /// Parses cron expressions, validates configs, and loads persisted last_run
    /// times from the central eidetica database. Invalid schedules are logged
    /// and skipped.
    pub async fn new(
        configs: Vec<ScheduleConfig>,
        server: Arc<Server>,
        backend: BackendManager,
        central_db: Database,
    ) -> Self {
        let mut records = Vec::new();

        // Load persisted state
        let persisted = load_schedule_states(&central_db).await;

        for cfg in configs {
            if !cfg.enabled {
                info!(schedule = %cfg.name, "Schedule disabled, skipping");
                continue;
            }

            match Schedule::from_str(&cfg.cron) {
                Ok(cron) => {
                    let last_run = persisted
                        .iter()
                        .find(|s| s.name == cfg.name)
                        .and_then(|s| DateTime::parse_from_rfc3339(&s.last_run).ok())
                        .map(|dt| dt.with_timezone(&Utc));

                    info!(
                        schedule = %cfg.name,
                        session = %cfg.session,
                        cron = %cfg.cron,
                        last_run = ?last_run,
                        "Schedule registered"
                    );
                    records.push(ScheduleRecord {
                        name: cfg.name,
                        session_id: cfg.session,
                        task: cfg.task,
                        cron,
                        enabled: true,
                        last_run,
                    });
                }
                Err(e) => {
                    error!(
                        schedule = %cfg.name,
                        cron = %cfg.cron,
                        "Invalid cron expression: {e}"
                    );
                }
            }
        }

        Self {
            records: Arc::new(Mutex::new(records)),
            server,
            backend,
            central_db,
        }
    }

    /// Start the scheduler as a background tokio task.
    pub fn start(self: &Arc<Self>) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            scheduler.run_loop().await;
        });
    }

    /// List all schedules with their current status.
    pub async fn list(&self) -> Vec<ScheduleInfo> {
        let records = self.records.lock().await;
        records
            .iter()
            .map(|r| {
                let next_run = next_run_after(&r.cron, r.last_run);
                ScheduleInfo {
                    name: r.name.clone(),
                    session: r.session_id.clone(),
                    task: r.task.clone(),
                    enabled: r.enabled,
                    last_run: r.last_run,
                    next_run,
                }
            })
            .collect()
    }

    /// Trigger a named schedule immediately, regardless of cron timing.
    pub async fn trigger(&self, name: &str) -> anyhow::Result<()> {
        let task = {
            let records = self.records.lock().await;
            let record = records
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow::anyhow!("Unknown schedule: '{name}'"))?;
            (record.session_id.clone(), record.task.clone())
        };

        self.fire_schedule(name, &task.0, &task.1).await?;
        self.record_last_run(name).await;

        Ok(())
    }

    /// Main scheduler loop. Sleeps until the next scheduled event, then fires it.
    async fn run_loop(&self) {
        loop {
            let sleep_duration = {
                let records = self.records.lock().await;
                self.next_sleep(&records)
            };

            match sleep_duration {
                Some(duration) => {
                    tokio::time::sleep(duration).await;
                    self.check_and_fire().await;
                }
                None => {
                    // No enabled schedules — sleep a while and re-check
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                }
            }
        }
    }

    /// Calculate how long to sleep until the next scheduled event.
    fn next_sleep(&self, records: &[ScheduleRecord]) -> Option<tokio::time::Duration> {
        let now = Utc::now();
        records
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| next_run_after(&r.cron, r.last_run))
            .min()
            .map(|next| {
                let delta = next - now;
                // Clamp to at least 1 second to avoid busy-looping
                tokio::time::Duration::from_secs(delta.num_seconds().max(1) as u64)
            })
    }

    /// Check all schedules and fire any that are due.
    async fn check_and_fire(&self) {
        let now = Utc::now();
        let due: Vec<(String, String, String)> = {
            let records = self.records.lock().await;
            records
                .iter()
                .filter(|r| r.enabled)
                .filter(|r| {
                    // A schedule is due if its next run time (after last_run) is at or before now
                    next_run_after(&r.cron, r.last_run)
                        .map(|next| next <= now)
                        .unwrap_or(false)
                })
                .map(|r| (r.name.clone(), r.session_id.clone(), r.task.clone()))
                .collect()
        };

        for (name, session, task) in due {
            if let Err(e) = self.fire_schedule(&name, &session, &task).await {
                error!(schedule = %name, "Failed to fire schedule: {e}");
            }
            self.record_last_run(&name).await;
        }
    }

    /// Update last_run in memory and persist to eidetica.
    async fn record_last_run(&self, name: &str) {
        let now = Utc::now();

        // Update in-memory
        {
            let mut records = self.records.lock().await;
            if let Some(record) = records.iter_mut().find(|r| r.name == name) {
                record.last_run = Some(now);
            }
        }

        // Persist to eidetica
        if let Err(e) = save_schedule_state(&self.central_db, name, now).await {
            warn!(schedule = %name, "Failed to persist last_run: {e}");
        }
    }

    /// Fire a single schedule: write a Directive to the target session.
    async fn fire_schedule(&self, name: &str, session_id: &str, task: &str) -> anyhow::Result<()> {
        info!(schedule = %name, session = %session_id, "Firing scheduled task");

        // Resolve the session identifier (name or DB ID)
        let (conversation_id, session_db) =
            self.server.registry().resolve_session(session_id).await?;

        // Ensure the server is watching this session. No approval channel —
        // scheduled runs are autonomous.
        self.server
            .register_session(&session_db, self.backend.clone(), None, None)
            .await?;

        // Write the directive
        let mut session = Session::new(conversation_id, session_db).await;
        let directive_content = format!(
            "Scheduled task '{}' triggered at {}.\n\n{}",
            name,
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            task
        );

        session
            .add_entry(SessionEntry {
                sender: "scheduler".to_string(),
                content: directive_content,
                timestamp: Utc::now(),
                entry_type: EntryType::Directive,
            })
            .await;

        Ok(())
    }
}

/// Compute the next run time for a cron schedule after the given last_run.
/// If last_run is None, returns the next upcoming time from now.
fn next_run_after(cron: &Schedule, last_run: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match last_run {
        Some(lr) => cron.after(&lr).next(),
        None => cron.upcoming(Utc).next(),
    }
}

/// Load all persisted schedule states from the central DB.
async fn load_schedule_states(db: &Database) -> Vec<ScheduleState> {
    let Ok(txn) = db.new_transaction().await else {
        return Vec::new();
    };
    let Ok(store) = txn.get_store::<Table<ScheduleState>>("schedules").await else {
        return Vec::new();
    };
    match store.search(|_| true).await {
        Ok(results) => results.into_iter().map(|(_, s)| s).collect(),
        Err(_) => Vec::new(),
    }
}

/// Persist a schedule's last_run to the central DB.
async fn save_schedule_state(
    db: &Database,
    name: &str,
    last_run: DateTime<Utc>,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<Table<ScheduleState>>("schedules").await?;

    // Update existing or insert new
    let existing = store.search(|s| s.name == name).await?;
    let state = ScheduleState {
        name: name.to_string(),
        last_run: last_run.to_rfc3339(),
    };

    if let Some((key, _)) = existing.into_iter().next() {
        store.set(&key, state).await?;
    } else {
        store.insert(state).await?;
    }

    txn.commit().await?;
    Ok(())
}

/// Public schedule status info for TUI display.
pub struct ScheduleInfo {
    pub name: String,
    pub session: String,
    pub task: String,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;

    async fn test_db() -> (Instance, Database) {
        let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = eidetica::crdt::Doc::new();
        s.set("name", "central");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, db)
    }

    // ================================================================
    // next_run_after
    // ================================================================

    #[test]
    fn next_run_after_respects_last_run() {
        // Fire at every minute
        let cron = Schedule::from_str("0 * * * * *").unwrap();
        let last_run = Utc.with_ymd_and_hms(2026, 4, 21, 10, 0, 0).unwrap();
        let next = next_run_after(&cron, Some(last_run)).unwrap();
        // Cron "0 * * * * *" fires at second 0 each minute; next after 10:00:00
        // should be 10:01:00.
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 4, 21, 10, 1, 0).unwrap());
    }

    #[test]
    fn next_run_after_without_last_run_uses_now() {
        // Daily at 03:00
        let cron = Schedule::from_str("0 0 3 * * *").unwrap();
        let next = next_run_after(&cron, None).unwrap();
        // Always in the future
        assert!(next > Utc::now());
    }

    #[test]
    fn next_run_after_returns_none_for_exhausted_cron() {
        // A one-shot cron in the past — `after(last_run)` still has future
        // occurrences unless the cron uses a specific year. Use a year in the
        // past to force exhaustion.
        let cron = Schedule::from_str("0 0 0 1 1 * 2020").unwrap();
        let last_run = Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).unwrap();
        assert!(next_run_after(&cron, Some(last_run)).is_none());
    }

    // ================================================================
    // save_schedule_state + load_schedule_states round-trip
    // ================================================================

    #[tokio::test]
    async fn save_and_load_single_schedule() {
        let (_instance, db) = test_db().await;
        let now = Utc.with_ymd_and_hms(2026, 4, 21, 12, 0, 0).unwrap();
        save_schedule_state(&db, "daily_ping", now).await.unwrap();

        let states = load_schedule_states(&db).await;
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].name, "daily_ping");
        let parsed = DateTime::parse_from_rfc3339(&states[0].last_run)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parsed, now);
    }

    #[tokio::test]
    async fn save_updates_existing_schedule() {
        let (_instance, db) = test_db().await;
        let t1 = Utc.with_ymd_and_hms(2026, 4, 21, 10, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 4, 21, 11, 0, 0).unwrap();

        save_schedule_state(&db, "daily_ping", t1).await.unwrap();
        save_schedule_state(&db, "daily_ping", t2).await.unwrap();

        let states = load_schedule_states(&db).await;
        // Only one entry; it was updated, not duplicated.
        assert_eq!(states.len(), 1);
        let parsed = DateTime::parse_from_rfc3339(&states[0].last_run)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parsed, t2);
    }

    #[tokio::test]
    async fn save_multiple_schedules_independent() {
        let (_instance, db) = test_db().await;
        let t1 = Utc.with_ymd_and_hms(2026, 4, 21, 10, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 4, 21, 11, 0, 0).unwrap();
        save_schedule_state(&db, "job_a", t1).await.unwrap();
        save_schedule_state(&db, "job_b", t2).await.unwrap();

        let states = load_schedule_states(&db).await;
        assert_eq!(states.len(), 2);
        let names: Vec<&str> = states.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"job_a"));
        assert!(names.contains(&"job_b"));
    }

    #[tokio::test]
    async fn load_on_empty_db_returns_empty() {
        let (_instance, db) = test_db().await;
        assert!(load_schedule_states(&db).await.is_empty());
    }
}

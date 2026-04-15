//! Cron-driven scheduled task execution.
//!
//! The scheduler is a background task that writes Directive entries to sessions
//! on a cron schedule. The existing server callback machinery handles agent
//! execution — the scheduler just provides the trigger.

use crate::backends::BackendManager;
use crate::config::ScheduleConfig;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use chrono::Utc;
use cron::Schedule;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

/// A parsed schedule ready for execution.
struct ScheduleRecord {
    name: String,
    session_db_id: String,
    task: String,
    cron: Schedule,
    enabled: bool,
    last_run: Option<chrono::DateTime<Utc>>,
}

/// Cron-driven scheduler that writes Directive entries to sessions.
pub struct Scheduler {
    records: Arc<Mutex<Vec<ScheduleRecord>>>,
    server: Arc<Server>,
    backend: BackendManager,
}

impl Scheduler {
    /// Create a new scheduler from config.
    ///
    /// Parses cron expressions and validates schedule configs. Invalid schedules
    /// are logged and skipped.
    pub fn new(
        configs: Vec<ScheduleConfig>,
        server: Arc<Server>,
        backend: BackendManager,
    ) -> Self {
        let mut records = Vec::new();

        for cfg in configs {
            if !cfg.enabled {
                info!(schedule = %cfg.name, "Schedule disabled, skipping");
                continue;
            }

            match Schedule::from_str(&cfg.cron) {
                Ok(cron) => {
                    info!(
                        schedule = %cfg.name,
                        session = %cfg.session,
                        cron = %cfg.cron,
                        "Schedule registered"
                    );
                    records.push(ScheduleRecord {
                        name: cfg.name,
                        session_db_id: cfg.session,
                        task: cfg.task,
                        cron,
                        enabled: true,
                        last_run: None,
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
                let next_run = r.cron.upcoming(Utc).next();
                ScheduleInfo {
                    name: r.name.clone(),
                    session: r.session_db_id.clone(),
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
            (record.session_db_id.clone(), record.task.clone())
        };

        self.fire_schedule(name, &task.0, &task.1).await?;

        // Update last_run
        let mut records = self.records.lock().await;
        if let Some(record) = records.iter_mut().find(|r| r.name == name) {
            record.last_run = Some(Utc::now());
        }

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
            .filter_map(|r| r.cron.upcoming(Utc).next())
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
                    // Check if the next scheduled time is now or in the past
                    // (i.e., we woke up at or after the scheduled time)
                    r.cron
                        .upcoming(Utc)
                        .next()
                        .map(|next| next <= now + chrono::Duration::seconds(2))
                        .unwrap_or(false)
                })
                .map(|r| {
                    (
                        r.name.clone(),
                        r.session_db_id.clone(),
                        r.task.clone(),
                    )
                })
                .collect()
        };

        for (name, session, task) in due {
            if let Err(e) = self.fire_schedule(&name, &session, &task).await {
                error!(schedule = %name, "Failed to fire schedule: {e}");
            }

            // Update last_run
            let mut records = self.records.lock().await;
            if let Some(record) = records.iter_mut().find(|r| r.name == name) {
                record.last_run = Some(Utc::now());
            }
        }
    }

    /// Fire a single schedule: write a Directive to the target session.
    async fn fire_schedule(
        &self,
        name: &str,
        session_db_id: &str,
        task: &str,
    ) -> anyhow::Result<()> {
        info!(schedule = %name, db_id = %session_db_id, "Firing scheduled task");

        // Open the session by its eidetica DB root ID
        let (transport_id, conversation_id, session_db) = self
            .server
            .registry()
            .open_session_by_db_id(session_db_id)
            .await?;

        // Ensure the server is watching this session
        self.server
            .register_session(
                &transport_id,
                &session_db,
                self.backend.clone(),
                None,
                None, // No approval channel — scheduled runs are autonomous
            )
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

/// Public schedule status info for TUI display.
pub struct ScheduleInfo {
    pub name: String,
    pub session: String,
    pub task: String,
    pub enabled: bool,
    pub last_run: Option<chrono::DateTime<Utc>>,
    pub next_run: Option<chrono::DateTime<Utc>>,
}

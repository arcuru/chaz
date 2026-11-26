//! One local mutation boundary for the Agent's selected scheduling ledger and
//! definitions. Reopened handles share it; neither a handle-local lock nor a
//! Server-only accounting lock serializes operator edits with admission.
use super::lifecycle::AccountingCache;
use eidetica::entry::ID;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub(crate) type ScheduleLock = Arc<tokio::sync::Mutex<AccountingCache>>;

pub(crate) fn for_agent(id: &ID) -> ScheduleLock {
    static LOCKS: OnceLock<Mutex<HashMap<ID, Weak<tokio::sync::Mutex<AccountingCache>>>>> =
        OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap();
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
        return lock;
    }
    // All local schedule mutations share one operational DB. Serialize at that
    // boundary, not across execution, target readiness or runtime capacity.
    let lock = Arc::new(tokio::sync::Mutex::new(AccountingCache::default()));
    locks.insert(id.clone(), Arc::downgrade(&lock));
    lock
}

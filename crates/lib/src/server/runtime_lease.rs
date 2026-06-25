//! Runtime-ownership lease — the single-owner guard behind `claim_runtime`.
//!
//! `register_session` historically did two unrelated jobs: **transport**
//! (subscribe a client to a session's I/O) and **runtime** (fire the
//! `session_start` hook, drive the routine engine, publish extension status).
//! A pure transport client (a Matrix/Discord bridge) that called it silently
//! took on the runtime duties too — two clients running the runtime meant
//! doubled side effects (two agent replies, two scheduler fires) with nothing
//! to dedup, because the duplication is in the *side effect*, not the data.
//!
//! "Who runs the agent" is leader election and needs a real owner. This module
//! is that owner's seam: a [`RuntimeLease`] trait with an in-process
//! [`LocalRuntimeLease`] implementation. Today it is a per-session
//! `Mutex<HashMap<session_id, OwnerId>>` — claim-once, single process. The
//! trait is the interface a real, daemon-arbitrated eidetica lease drops into
//! later (Track D) with no caller changes: `claim_runtime` keeps calling
//! `try_claim`; only the impl behind the `Arc<dyn RuntimeLease>` changes.

use std::collections::HashMap;
use std::sync::Mutex;

/// Identifies a runtime owner. In-process this is a generated token; a future
/// eidetica lease maps it to a `(pubkey, fence term)` pair.
pub type OwnerId = String;

/// Why a [`RuntimeLease::try_claim`] was refused.
#[derive(Debug, Clone)]
pub enum LeaseError {
    /// Another owner already holds the runtime claim for this session.
    Held(OwnerId),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Held(owner) => write!(f, "runtime already claimed by owner {owner}"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// Single-owner runtime lease. One owner per session: the first claimant wins;
/// later claimants from a *different* owner are refused until the holder
/// releases. Re-claiming with the same `OwnerId` is idempotent.
pub trait RuntimeLease: Send + Sync {
    /// Try to claim runtime ownership of `session_id` for `owner`. `Ok(())`
    /// when the claim is granted (the slot was free, or already held by the
    /// same `owner`); `Err(LeaseError::Held)` when a *different* owner holds it.
    fn try_claim(&self, session_id: &str, owner: &str) -> Result<(), LeaseError>;

    /// Release `session_id`'s claim. Idempotent — releasing an unclaimed
    /// session is a no-op. In-process this clears the slot unconditionally;
    /// the runtime owns its own sessions single-process, so there is no
    /// cross-owner release to police yet.
    fn release(&self, session_id: &str);

    /// The current owner of `session_id`, if any.
    fn owner_of(&self, session_id: &str) -> Option<OwnerId>;
}

/// In-process [`RuntimeLease`]: a per-session `Mutex<HashMap<_, OwnerId>>`.
/// Ships single-process with zero eidetica dependency.
#[derive(Default)]
pub struct LocalRuntimeLease {
    owners: Mutex<HashMap<String, OwnerId>>,
}

impl LocalRuntimeLease {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RuntimeLease for LocalRuntimeLease {
    fn try_claim(&self, session_id: &str, owner: &str) -> Result<(), LeaseError> {
        let mut owners = self.owners.lock().expect("runtime lease poisoned");
        match owners.get(session_id) {
            Some(existing) if existing == owner => Ok(()),
            Some(existing) => Err(LeaseError::Held(existing.clone())),
            None => {
                owners.insert(session_id.to_string(), owner.to_string());
                Ok(())
            }
        }
    }

    fn release(&self, session_id: &str) {
        self.owners
            .lock()
            .expect("runtime lease poisoned")
            .remove(session_id);
    }

    fn owner_of(&self, session_id: &str) -> Option<OwnerId> {
        self.owners
            .lock()
            .expect("runtime lease poisoned")
            .get(session_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_claim_wins_second_owner_refused() {
        let lease = LocalRuntimeLease::new();
        assert!(lease.try_claim("s1", "owner-a").is_ok());
        match lease.try_claim("s1", "owner-b") {
            Err(LeaseError::Held(h)) => assert_eq!(h, "owner-a"),
            other => panic!("expected Held(owner-a), got {other:?}"),
        }
    }

    #[test]
    fn same_owner_reclaim_is_idempotent() {
        let lease = LocalRuntimeLease::new();
        assert!(lease.try_claim("s1", "owner-a").is_ok());
        assert!(lease.try_claim("s1", "owner-a").is_ok());
        assert_eq!(lease.owner_of("s1").as_deref(), Some("owner-a"));
    }

    #[test]
    fn release_frees_the_slot() {
        let lease = LocalRuntimeLease::new();
        assert!(lease.try_claim("s1", "owner-a").is_ok());
        lease.release("s1");
        assert_eq!(lease.owner_of("s1"), None);
        // A different owner can now claim.
        assert!(lease.try_claim("s1", "owner-b").is_ok());
    }

    #[test]
    fn distinct_sessions_are_independent() {
        let lease = LocalRuntimeLease::new();
        assert!(lease.try_claim("s1", "owner-a").is_ok());
        assert!(lease.try_claim("s2", "owner-b").is_ok());
        assert_eq!(lease.owner_of("s1").as_deref(), Some("owner-a"));
        assert_eq!(lease.owner_of("s2").as_deref(), Some("owner-b"));
    }
}

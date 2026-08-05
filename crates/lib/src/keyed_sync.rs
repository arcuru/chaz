//! Periodic sync of tracked databases under **their own** recorded key.
//!
//! # Why this exists
//!
//! Eidetica's background sync engine signs every sync request with the
//! *instance device key*. A database obtained by bootstrapping with a named
//! *user* key is authorized for that user key and nothing else, so the owning
//! peer refuses the pull:
//!
//! ```text
//! requester: Permission denied: key <device key> is not authorized to read <db>
//! owner:     Incremental sync request rejected: caller is not authorized to read this database
//! ```
//!
//! The correct key is recorded — `TrackedDatabase::key_id` holds exactly the
//! key that was bootstrapped and approved — but the sync layer's user↔database
//! link carries only a user uuid, so the background engine has nothing to look
//! up and passes `None` (device key) for every tree. The explicit API,
//! `Sync::sync_tree_with_peer_as`, takes the key and documents this exact case.
//!
//! This module closes the gap from the outside: walk the tracked databases,
//! read the key each one recorded, and drive the explicit API with it. It is a
//! reconciler, not a replacement — the background engine keeps running and
//! keeps handling device-key databases, which we skip.
//!
//! # Cost of the workaround
//!
//! Latency, not correctness. Push-on-commit is part of the same broken path, so
//! a bridge's inbound message reaches the daemon on the next tick rather than
//! immediately. [`DEFAULT_INTERVAL`] trades chattiness against that delay.
//!
//! # Removing it
//!
//! Delete this module and its one call site in [`crate::server::build`] once
//! eidetica's background engine signs with the tracked key. Nothing else
//! depends on it, and no configuration schema mentions it — the interval knob
//! is an environment variable precisely so the removal is a clean delete.

use std::sync::Arc;
use std::time::Duration;

use eidetica::auth::crypto::PublicKey;
use tracing::{debug, info, warn};

use crate::session::SessionRegistry;

/// How often to reconcile. Short enough that a bridged message round-trip
/// still feels immediate, since on-commit push is broken along with the rest.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// Override for [`DEFAULT_INTERVAL`], in seconds. Deliberately an environment
/// variable rather than a config field: this whole module is temporary, and
/// keeping it out of the config schema means removing it breaks nobody's yaml.
pub const INTERVAL_ENV: &str = "CHAZ_KEYED_SYNC_INTERVAL_SECS";

/// Resolve the reconcile interval, falling back to [`DEFAULT_INTERVAL`] when
/// [`INTERVAL_ENV`] is unset or unparseable. Zero disables the reconciler.
fn resolve_interval() -> Option<Duration> {
    let Ok(raw) = std::env::var(INTERVAL_ENV) else {
        return Some(DEFAULT_INTERVAL);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(e) => {
            warn!(
                value = %raw,
                "{INTERVAL_ENV} is not a whole number of seconds ({e}); using the default"
            );
            Some(DEFAULT_INTERVAL)
        }
    }
}

/// Spawn the reconciler. No-op when sync is disabled on this peer or when
/// [`INTERVAL_ENV`] is set to `0`.
pub fn spawn(registry: Arc<SessionRegistry>) {
    if registry.instance().sync().is_none() {
        return;
    }
    let Some(interval) = resolve_interval() else {
        info!("Keyed sync reconciler disabled by {INTERVAL_ENV}=0");
        return;
    };
    info!(
        interval_secs = interval.as_secs(),
        "Keyed sync reconciler started (works around device-key-signed background sync)"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            reconcile_once(&registry).await;
        }
    });
}

/// One reconcile pass. Never returns an error: a peer being unreachable is the
/// normal case for a bridge that has been redeployed, and the next tick retries.
pub async fn reconcile_once(registry: &SessionRegistry) {
    let Some(sync) = registry.instance().sync() else {
        return;
    };
    // A database tracked under the device key is exactly the case the background
    // engine already gets right; syncing it again here would only double the
    // traffic.
    let device_pubkey = sync.get_device_pubkey().ok();

    let tracked = match registry.tracked_databases().await {
        Ok(t) => t,
        Err(e) => {
            debug!("keyed sync: listing tracked databases failed: {e}");
            return;
        }
    };

    // A pass that syncs nothing looks identical to a pass that syncs everything
    // if only failures are logged. Count what happened so an operator can tell
    // "converging" from "no work found", which are the two states that matter.
    let considered = tracked.len();
    let mut sync_disabled = 0usize;
    let mut device_keyed = 0usize;
    let mut no_peers = 0usize;
    let mut attempted = 0usize;
    let mut failed = 0usize;

    for db in tracked {
        if !db.sync_settings.sync_enabled {
            sync_disabled += 1;
            continue;
        }
        if device_pubkey.as_ref() == Some(&db.key_id) {
            device_keyed += 1;
            continue;
        }
        let signing_key = match registry.signing_key_for(&db.key_id).await {
            Ok(k) => k,
            Err(e) => {
                // Tracked under a key this peer doesn't hold the secret for —
                // nothing we can sign as. Not our problem to fix here.
                debug!(db_id = %db.database_id, "keyed sync: no signing key: {e}");
                continue;
            }
        };
        let peers = match sync.get_tree_peers(&db.database_id).await {
            Ok(p) => p,
            Err(e) => {
                debug!(db_id = %db.database_id, "keyed sync: get_tree_peers failed: {e}");
                continue;
            }
        };
        if peers.is_empty() {
            no_peers += 1;
            continue;
        }
        for peer in &peers {
            let peer_key: &PublicKey = peer.public_key();
            attempted += 1;
            if let Err(e) = sync
                .sync_tree_with_peer_as(peer_key, &db.database_id, Some(&signing_key))
                .await
            {
                failed += 1;
                debug!(
                    db_id = %db.database_id,
                    peer = %peer,
                    "keyed sync: sync failed: {e}"
                );
            }
        }
    }

    debug!(
        considered,
        sync_disabled,
        device_keyed,
        no_peers,
        attempted,
        failed,
        succeeded = attempted - failed,
        "keyed sync pass complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer with sync switched off must be a no-op, not a panic — `--print`
    /// runs and any sync-less embedding hit this path every tick.
    #[tokio::test]
    async fn reconcile_is_a_noop_without_sync() {
        let (_instance, registry) = crate::session::test_helpers::make_registry().await;
        reconcile_once(&registry).await;
    }

    /// With sync enabled but nothing tracked for sync and no peers, a pass must
    /// still complete cleanly — this is the steady state on a fresh daemon.
    #[tokio::test]
    async fn reconcile_handles_a_peer_with_no_synced_databases() {
        let (_instance, registry) = crate::session::test_helpers::make_registry_with_sync().await;
        reconcile_once(&registry).await;
    }
}

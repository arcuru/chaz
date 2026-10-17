//! A forwarding Eidetica backend that records which database trees each
//! operation touches, so a test can prove an operation's reads stay bounded
//! as unrelated databases accumulate.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eidetica::backend::{
    BackendImpl, InstanceMetadata, InstanceSecrets, Reachability, RecordMutations, RecordPage,
    RecordRange, RecordView, StagingToken, StoreStateRequest, VerificationStatus,
};
use eidetica::entry::{Entry, ID};
use eidetica::{Result, Snapshot};

/// Shared probe: operation count and touched tree IDs while recording.
#[derive(Clone, Default)]
pub(crate) struct BackendProbe {
    recording: Arc<AtomicBool>,
    operations: Arc<AtomicUsize>,
    trees: Arc<Mutex<BTreeSet<ID>>>,
    methods: Arc<Mutex<BTreeMap<&'static str, usize>>>,
    fail_get: Arc<Mutex<Option<ID>>>,
    fail_put: Arc<Mutex<Option<(ID, usize)>>>,
}

impl BackendProbe {
    /// Start a fresh recording window.
    pub(crate) fn start(&self) {
        self.operations.store(0, Ordering::SeqCst);
        self.trees.lock().unwrap().clear();
        self.methods.lock().unwrap().clear();
        self.recording.store(true, Ordering::SeqCst);
    }

    /// End the window, returning (operation count, touched tree IDs).
    pub(crate) fn stop(&self) -> (usize, BTreeSet<ID>) {
        self.recording.store(false, Ordering::SeqCst);
        (
            self.operations.load(Ordering::SeqCst),
            self.trees.lock().unwrap().clone(),
        )
    }

    /// Per-method operation counts in the last window.
    pub(crate) fn methods(&self) -> BTreeMap<&'static str, usize> {
        self.methods.lock().unwrap().clone()
    }

    /// Fail reads of one entry until cleared.
    pub(crate) fn fail_get(&self, id: Option<ID>) {
        *self.fail_get.lock().unwrap() = id;
    }

    /// Fail one write to a tree after `skip` matching writes succeed.
    pub(crate) fn fail_put(&self, tree: ID, skip: usize) {
        *self.fail_put.lock().unwrap() = Some((tree, skip));
    }

    fn op(&self, method: &'static str) {
        if self.recording.load(Ordering::SeqCst) {
            self.operations.fetch_add(1, Ordering::SeqCst);
            *self.methods.lock().unwrap().entry(method).or_default() += 1;
        }
    }

    fn tree(&self, method: &'static str, id: &ID) {
        self.op(method);
        if self.recording.load(Ordering::SeqCst) {
            self.trees.lock().unwrap().insert(id.clone());
        }
    }

    fn entry(&self, entry: &Entry) {
        if self.recording.load(Ordering::SeqCst) {
            let root = entry.root().unwrap_or_else(|| entry.id());
            self.trees.lock().unwrap().insert(root);
        }
    }
}

/// Forwards every [`BackendImpl`] method to `inner`, recording on `probe`.
pub(crate) struct CountingBackend<B> {
    inner: B,
    probe: BackendProbe,
}

impl<B> CountingBackend<B> {
    pub(crate) fn new(inner: B, probe: BackendProbe) -> Self {
        Self { inner, probe }
    }
}

#[async_trait]
impl<B: BackendImpl> BackendImpl for CountingBackend<B> {
    async fn resolve_store_state(&self, request: &StoreStateRequest) -> Result<Option<RecordView>> {
        self.probe.tree("resolve_store_state", &request.database);
        self.inner.resolve_store_state(request).await
    }

    async fn begin_store_state_staging(&self, request: StoreStateRequest) -> Result<StagingToken> {
        self.probe
            .tree("begin_store_state_staging", &request.database);
        self.inner.begin_store_state_staging(request).await
    }

    async fn stage_store_state_records(
        &self,
        token: &StagingToken,
        records: RecordMutations,
    ) -> Result<()> {
        self.probe.op("stage_store_state_records");
        self.inner.stage_store_state_records(token, records).await
    }

    async fn publish_store_state(&self, token: StagingToken) -> Result<RecordView> {
        self.probe.op("publish_store_state");
        self.inner.publish_store_state(token).await
    }

    async fn abort_store_state(&self, token: StagingToken) -> Result<()> {
        self.probe.op("abort_store_state");
        self.inner.abort_store_state(token).await
    }

    async fn store_state_record_get(
        &self,
        view: &RecordView,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.probe.op("store_state_record_get");
        self.inner.store_state_record_get(view, key).await
    }

    async fn store_state_record_scan(
        &self,
        view: &RecordView,
        range: &RecordRange,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<RecordPage> {
        self.probe.op("store_state_record_scan");
        self.inner
            .store_state_record_scan(view, range, after, limit)
            .await
    }

    async fn clear_derived_store_state(&self) -> Result<()> {
        self.probe.op("clear_derived_store_state");
        self.inner.clear_derived_store_state().await
    }

    async fn reset_local_verification(&self) -> Result<()> {
        self.probe.op("reset_local_verification");
        self.inner.reset_local_verification().await
    }

    async fn get(&self, id: &ID) -> Result<Entry> {
        self.probe.op("get");
        if self.probe.fail_get.lock().unwrap().as_ref() == Some(id) {
            return Err(eidetica::backend::BackendError::StateInconsistency {
                reason: "injected root read failure".into(),
            }
            .into());
        }
        let entry = self.inner.get(id).await?;
        self.probe.entry(&entry);
        Ok(entry)
    }

    async fn get_verification_status(&self, id: &ID) -> Result<VerificationStatus> {
        self.probe.op("get_verification_status");
        self.inner.get_verification_status(id).await
    }

    async fn put(&self, entry: Entry) -> Result<()> {
        self.probe.op("put");
        self.probe.entry(&entry);
        {
            let mut failure = self.probe.fail_put.lock().unwrap();
            if let Some((tree, skip)) = failure.as_mut()
                && entry.root().as_ref() == Some(tree)
            {
                if *skip == 0 {
                    *failure = None;
                    return Err(eidetica::backend::BackendError::StateInconsistency {
                        reason: "injected root write failure".into(),
                    }
                    .into());
                }
                *skip -= 1;
            }
        }
        self.inner.put(entry).await
    }

    async fn update_verification_status(
        &self,
        id: &ID,
        verification_status: VerificationStatus,
    ) -> Result<()> {
        self.probe.op("update_verification_status");
        self.inner
            .update_verification_status(id, verification_status)
            .await
    }

    async fn get_entries_by_verification_status(
        &self,
        status: VerificationStatus,
    ) -> Result<Vec<ID>> {
        self.probe.op("get_entries_by_verification_status");
        self.inner.get_entries_by_verification_status(status).await
    }

    async fn snapshot(&self, tree: &ID) -> Result<Snapshot> {
        self.probe.tree("snapshot", tree);
        self.inner.snapshot(tree).await
    }

    async fn store_snapshot(&self, tree: &ID, store: &str) -> Result<Snapshot> {
        self.probe.tree("store_snapshot", tree);
        self.inner.store_snapshot(tree, store).await
    }

    async fn store_snapshot_at(
        &self,
        tree: &ID,
        store: &str,
        main_snapshot: &Snapshot,
    ) -> Result<Snapshot> {
        self.probe.tree("store_snapshot_at", tree);
        self.inner
            .store_snapshot_at(tree, store, main_snapshot)
            .await
    }

    async fn all_roots(&self) -> Result<Vec<ID>> {
        self.probe.op("all_roots");
        self.inner.all_roots().await
    }

    async fn find_merge_base(
        &self,
        tree: &ID,
        store: &str,
        entry_ids: &[ID],
    ) -> Result<Option<ID>> {
        self.probe.tree("find_merge_base", tree);
        self.inner.find_merge_base(tree, store, entry_ids).await
    }

    fn as_any(&self) -> &dyn Any {
        // Keep downcasts to the concrete backend (e.g. InMemory persistence)
        // working through the wrapper.
        self.inner.as_any()
    }

    async fn get_tree(&self, tree: &ID) -> Result<Vec<Entry>> {
        self.probe.tree("get_tree", tree);
        self.inner.get_tree(tree).await
    }

    async fn get_store(&self, tree: &ID, store: &str) -> Result<Vec<Entry>> {
        self.probe.tree("get_store", tree);
        self.inner.get_store(tree, store).await
    }

    async fn get_tree_from_tips(&self, tree: &ID, tips: &[ID]) -> Result<Vec<Entry>> {
        self.probe.tree("get_tree_from_tips", tree);
        self.inner.get_tree_from_tips(tree, tips).await
    }

    async fn check_targets_reachable_from(
        &self,
        tree: &ID,
        from: &[ID],
        targets: &[ID],
    ) -> Result<Reachability> {
        self.probe.tree("check_targets_reachable_from", tree);
        self.inner
            .check_targets_reachable_from(tree, from, targets)
            .await
    }

    async fn store_at(&self, tree: &ID, store: &str, snapshot: &Snapshot) -> Result<Vec<Entry>> {
        self.probe.tree("store_at", tree);
        self.inner.store_at(tree, store, snapshot).await
    }

    async fn get_sorted_store_parents(
        &self,
        tree_id: &ID,
        entry_id: &ID,
        store: &str,
    ) -> Result<Vec<ID>> {
        self.probe.tree("get_sorted_store_parents", tree_id);
        self.inner
            .get_sorted_store_parents(tree_id, entry_id, store)
            .await
    }

    async fn get_path_from_to(
        &self,
        tree_id: &ID,
        store: &str,
        from_id: Option<&ID>,
        to_ids: &[ID],
    ) -> Result<Vec<ID>> {
        self.probe.tree("get_path_from_to", tree_id);
        self.inner
            .get_path_from_to(tree_id, store, from_id, to_ids)
            .await
    }

    async fn get_instance_metadata(&self) -> Result<Option<InstanceMetadata>> {
        self.probe.op("get_instance_metadata");
        self.inner.get_instance_metadata().await
    }

    async fn set_instance_metadata(&self, metadata: &InstanceMetadata) -> Result<()> {
        self.probe.op("set_instance_metadata");
        self.inner.set_instance_metadata(metadata).await
    }

    async fn get_instance_secrets(&self) -> Result<Option<InstanceSecrets>> {
        self.probe.op("get_instance_secrets");
        self.inner.get_instance_secrets().await
    }

    async fn set_instance_secrets(&self, secrets: &InstanceSecrets) -> Result<()> {
        self.probe.op("set_instance_secrets");
        self.inner.set_instance_secrets(secrets).await
    }
}

//! An in-memory backend that counts raw Entry reads.
//!
//! Every method delegates to the native `InMemory` backend unchanged, including
//! the methods with trait defaults, so the fixture keeps native cached-state and
//! traversal behavior. Only `get` is observed: on a service connection each one
//! is a separate `GetEntry` round trip, so read paths must not scale it with
//! session history.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eidetica::{
    AccountSession, Instance, NewAccount,
    backend::{
        BackendImpl, InstanceMetadata, InstanceSecrets, Reachability, RecordMutations, RecordPage,
        RecordRange, RecordView, StagingToken, StoreStateRequest, VerificationStatus,
        database::InMemory,
    },
    entry::{Entry, ID},
    snapshot::Snapshot,
};

#[derive(Clone, Default)]
pub(crate) struct EntryReads(Arc<AtomicUsize>);

impl EntryReads {
    pub(crate) fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct Counting {
    inner: InMemory,
    reads: EntryReads,
}

/// Fresh one-user instance whose backend counts raw Entry reads.
pub(crate) async fn counting_eidetica() -> (Instance, AccountSession, EntryReads) {
    let reads = EntryReads::default();
    let (instance, user) = Instance::create_backend(
        Box::new(Counting {
            inner: InMemory::new(),
            reads: reads.clone(),
        }),
        NewAccount::passwordless("test"),
    )
    .await
    .unwrap();
    (instance, user, reads)
}

#[async_trait::async_trait]
impl BackendImpl for Counting {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn get(&self, id: &ID) -> eidetica::Result<Entry> {
        self.reads.0.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }
    async fn get_verification_status(&self, id: &ID) -> eidetica::Result<VerificationStatus> {
        self.inner.get_verification_status(id).await
    }
    async fn put(&self, entry: Entry) -> eidetica::Result<()> {
        self.inner.put(entry).await
    }
    async fn update_verification_status(
        &self,
        id: &ID,
        status: VerificationStatus,
    ) -> eidetica::Result<()> {
        self.inner.update_verification_status(id, status).await
    }
    async fn get_entries_by_verification_status(
        &self,
        status: VerificationStatus,
    ) -> eidetica::Result<Vec<ID>> {
        self.inner.get_entries_by_verification_status(status).await
    }
    async fn snapshot(&self, tree: &ID) -> eidetica::Result<Snapshot> {
        self.inner.snapshot(tree).await
    }
    async fn store_snapshot(&self, tree: &ID, store: &str) -> eidetica::Result<Snapshot> {
        self.inner.store_snapshot(tree, store).await
    }
    async fn store_snapshot_at(
        &self,
        tree: &ID,
        store: &str,
        snapshot: &Snapshot,
    ) -> eidetica::Result<Snapshot> {
        self.inner.store_snapshot_at(tree, store, snapshot).await
    }
    async fn all_roots(&self) -> eidetica::Result<Vec<ID>> {
        self.inner.all_roots().await
    }
    async fn find_merge_base(
        &self,
        tree: &ID,
        store: &str,
        entry_ids: &[ID],
    ) -> eidetica::Result<Option<ID>> {
        self.inner.find_merge_base(tree, store, entry_ids).await
    }
    async fn get_tree(&self, tree: &ID) -> eidetica::Result<Vec<Entry>> {
        self.inner.get_tree(tree).await
    }
    async fn get_store(&self, tree: &ID, store: &str) -> eidetica::Result<Vec<Entry>> {
        self.inner.get_store(tree, store).await
    }
    async fn get_tree_from_tips(&self, tree: &ID, tips: &[ID]) -> eidetica::Result<Vec<Entry>> {
        self.inner.get_tree_from_tips(tree, tips).await
    }
    async fn check_targets_reachable_from(
        &self,
        tree: &ID,
        from: &[ID],
        targets: &[ID],
    ) -> eidetica::Result<Reachability> {
        self.inner
            .check_targets_reachable_from(tree, from, targets)
            .await
    }
    async fn store_at(
        &self,
        tree: &ID,
        store: &str,
        snapshot: &Snapshot,
    ) -> eidetica::Result<Vec<Entry>> {
        self.inner.store_at(tree, store, snapshot).await
    }
    async fn get_sorted_store_parents(
        &self,
        tree: &ID,
        id: &ID,
        store: &str,
    ) -> eidetica::Result<Vec<ID>> {
        self.inner.get_sorted_store_parents(tree, id, store).await
    }
    async fn get_path_from_to(
        &self,
        tree: &ID,
        store: &str,
        from: Option<&ID>,
        to: &[ID],
    ) -> eidetica::Result<Vec<ID>> {
        self.inner.get_path_from_to(tree, store, from, to).await
    }
    async fn get_instance_metadata(&self) -> eidetica::Result<Option<InstanceMetadata>> {
        self.inner.get_instance_metadata().await
    }
    async fn set_instance_metadata(&self, metadata: &InstanceMetadata) -> eidetica::Result<()> {
        self.inner.set_instance_metadata(metadata).await
    }
    async fn get_instance_secrets(&self) -> eidetica::Result<Option<InstanceSecrets>> {
        self.inner.get_instance_secrets().await
    }
    async fn set_instance_secrets(&self, secrets: &InstanceSecrets) -> eidetica::Result<()> {
        self.inner.set_instance_secrets(secrets).await
    }
    async fn resolve_store_state(
        &self,
        request: &StoreStateRequest,
    ) -> eidetica::Result<Option<RecordView>> {
        self.inner.resolve_store_state(request).await
    }
    async fn begin_store_state_staging(
        &self,
        request: StoreStateRequest,
    ) -> eidetica::Result<StagingToken> {
        self.inner.begin_store_state_staging(request).await
    }
    async fn stage_store_state_records(
        &self,
        token: &StagingToken,
        mutations: RecordMutations,
    ) -> eidetica::Result<()> {
        self.inner.stage_store_state_records(token, mutations).await
    }
    async fn publish_store_state(&self, token: StagingToken) -> eidetica::Result<RecordView> {
        self.inner.publish_store_state(token).await
    }
    async fn abort_store_state(&self, token: StagingToken) -> eidetica::Result<()> {
        self.inner.abort_store_state(token).await
    }
    async fn store_state_record_get(
        &self,
        view: &RecordView,
        key: &[u8],
    ) -> eidetica::Result<Option<Vec<u8>>> {
        self.inner.store_state_record_get(view, key).await
    }
    async fn store_state_record_scan(
        &self,
        view: &RecordView,
        range: &RecordRange,
        after: Option<&[u8]>,
        limit: usize,
    ) -> eidetica::Result<RecordPage> {
        self.inner
            .store_state_record_scan(view, range, after, limit)
            .await
    }
    async fn clear_derived_store_state(&self) -> eidetica::Result<()> {
        self.inner.clear_derived_store_state().await
    }
    async fn reset_local_verification(&self) -> eidetica::Result<()> {
        self.inner.reset_local_verification().await
    }
}

//! Shared InMemory backend wrapper for eidetica persistence.
//!
//! Wraps Arc<InMemory> in a BackendImpl so we can pass ownership to
//! eidetica's Instance::open() while keeping a reference for save_to_file().

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use eidetica::backend::database::InMemory;
use eidetica::backend::{BackendImpl, InstanceMetadata, InstanceSecrets, VerificationStatus};
use eidetica::entry::{Entry, ID};
use eidetica::Result;

/// A BackendImpl wrapper that delegates to a shared Arc<InMemory>.
pub struct SharedBackend(Arc<InMemory>);

impl SharedBackend {
    /// Load from file or create fresh, returning both the backend for eidetica
    /// and a handle for later persistence.
    pub async fn load_or_create(path: Option<&Path>) -> (Box<dyn BackendImpl>, Option<SaveHandle>) {
        let inner = match path {
            Some(p) => InMemory::load_from_file(p).await.unwrap_or_default(),
            None => InMemory::new(),
        };
        let arc = Arc::new(inner);
        let handle = path.map(|p| SaveHandle {
            backend: arc.clone(),
            path: p.to_owned(),
        });
        (Box::new(SharedBackend(arc)), handle)
    }
}

/// Handle for saving the InMemory state to disk.
pub struct SaveHandle {
    backend: Arc<InMemory>,
    path: std::path::PathBuf,
}

impl SaveHandle {
    pub async fn save(&self) -> eidetica::Result<()> {
        self.backend.save_to_file(&self.path).await
    }
}

#[async_trait]
impl BackendImpl for SharedBackend {
    async fn get(&self, id: &ID) -> Result<Entry> {
        self.0.get(id).await
    }
    async fn get_verification_status(&self, id: &ID) -> Result<VerificationStatus> {
        self.0.get_verification_status(id).await
    }
    async fn put(&self, vs: VerificationStatus, entry: Entry) -> Result<()> {
        self.0.put(vs, entry).await
    }
    async fn update_verification_status(&self, id: &ID, vs: VerificationStatus) -> Result<()> {
        self.0.update_verification_status(id, vs).await
    }
    async fn get_entries_by_verification_status(&self, s: VerificationStatus) -> Result<Vec<ID>> {
        self.0.get_entries_by_verification_status(s).await
    }
    async fn get_tips(&self, tree: &ID) -> Result<Vec<ID>> {
        self.0.get_tips(tree).await
    }
    async fn get_store_tips(&self, tree: &ID, store: &str) -> Result<Vec<ID>> {
        self.0.get_store_tips(tree, store).await
    }
    async fn get_store_tips_up_to_entries(
        &self,
        tree: &ID,
        store: &str,
        main_entries: &[ID],
    ) -> Result<Vec<ID>> {
        self.0
            .get_store_tips_up_to_entries(tree, store, main_entries)
            .await
    }
    async fn all_roots(&self) -> Result<Vec<ID>> {
        self.0.all_roots().await
    }
    async fn find_merge_base(&self, tree: &ID, store: &str, ids: &[ID]) -> Result<ID> {
        self.0.find_merge_base(tree, store, ids).await
    }
    async fn collect_root_to_target(&self, tree: &ID, store: &str, target: &ID) -> Result<Vec<ID>> {
        self.0.collect_root_to_target(tree, store, target).await
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    async fn get_tree(&self, tree: &ID) -> Result<Vec<Entry>> {
        self.0.get_tree(tree).await
    }
    async fn get_store(&self, tree: &ID, store: &str) -> Result<Vec<Entry>> {
        self.0.get_store(tree, store).await
    }
    async fn get_tree_from_tips(&self, tree: &ID, tips: &[ID]) -> Result<Vec<Entry>> {
        self.0.get_tree_from_tips(tree, tips).await
    }
    async fn get_store_from_tips(
        &self,
        tree: &ID,
        store: &str,
        tips: &[ID],
    ) -> Result<Vec<Entry>> {
        self.0.get_store_from_tips(tree, store, tips).await
    }
    async fn get_cached_crdt_state(&self, id: &ID, store: &str) -> Result<Option<String>> {
        self.0.get_cached_crdt_state(id, store).await
    }
    async fn cache_crdt_state(&self, id: &ID, store: &str, state: String) -> Result<()> {
        self.0.cache_crdt_state(id, store, state).await
    }
    async fn clear_crdt_cache(&self) -> Result<()> {
        self.0.clear_crdt_cache().await
    }
    async fn get_sorted_store_parents(
        &self,
        tree_id: &ID,
        entry_id: &ID,
        store: &str,
    ) -> Result<Vec<ID>> {
        self.0
            .get_sorted_store_parents(tree_id, entry_id, store)
            .await
    }
    async fn get_path_from_to(
        &self,
        tree_id: &ID,
        store: &str,
        from_id: &ID,
        to_ids: &[ID],
    ) -> Result<Vec<ID>> {
        self.0
            .get_path_from_to(tree_id, store, from_id, to_ids)
            .await
    }
    async fn get_instance_metadata(&self) -> Result<Option<InstanceMetadata>> {
        self.0.get_instance_metadata().await
    }
    async fn set_instance_metadata(&self, metadata: &InstanceMetadata) -> Result<()> {
        self.0.set_instance_metadata(metadata).await
    }
    async fn get_instance_secrets(&self) -> Result<Option<InstanceSecrets>> {
        self.0.get_instance_secrets().await
    }
    async fn set_instance_secrets(&self, secrets: &InstanceSecrets) -> Result<()> {
        self.0.set_instance_secrets(secrets).await
    }
}

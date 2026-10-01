//! Process ownership of a Matrix login and its encrypted store.
use anyhow::{Context, ensure};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Never unlink a lock file: waiters must continue to name the same inode.
pub(crate) struct StoreLock {
    _files: Vec<File>,
    stores: Vec<PathBuf>,
    pub(crate) root: PathBuf,
}

pub(crate) fn no_symlinks(path: &Path) -> anyhow::Result<()> {
    let path = std::path::absolute(path)?;
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part);
        if let Ok(meta) = std::fs::symlink_metadata(&prefix) {
            ensure!(
                !meta.file_type().is_symlink(),
                "symlink in private state path; refusing access"
            );
        }
    }
    Ok(())
}

pub(crate) fn private_dir(path: &Path) -> anyhow::Result<()> {
    no_symlinks(path)?;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn lock(path: &Path) -> anyhow::Result<File> {
    no_symlinks(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "store lock is not a regular file"
    );
    file.try_lock()
        .context("Matrix store is in use; stop the bridge and other maintenance commands")?;
    Ok(file)
}

impl StoreLock {
    pub(crate) fn acquire(root: &Path) -> anyhow::Result<Self> {
        private_dir(root)?;
        let root = root.canonicalize()?;
        Ok(Self {
            _files: vec![lock(&root.join("store.lock"))?],
            stores: Vec::new(),
            root,
        })
    }

    pub(crate) fn lock_store(&mut self, store: &Path) -> anyhow::Result<()> {
        no_symlinks(store)?;
        let store = std::path::absolute(store)?;
        ensure!(
            !store
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
            "parent traversal in Matrix store path; refusing access"
        );
        ensure!(
            store.starts_with(&self.root),
            "Matrix store lies outside this login's private directory; move the session and store together"
        );
        private_dir(&store)?;
        for entry in std::fs::read_dir(&store)? {
            ensure!(
                entry?.file_type()?.is_file(),
                "nonregular entry in Matrix store; refusing unsafe store access"
            );
        }
        let store = store.canonicalize()?;
        if self.stores.contains(&store) {
            return Ok(());
        }
        self._files.push(lock(&store.join("ownership.lock"))?);
        self.stores.push(store);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contention_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let first = StoreLock::acquire(dir.path()).unwrap();
        assert!(StoreLock::acquire(dir.path()).is_err());
        drop(first);
        assert!(StoreLock::acquire(dir.path()).is_ok());
    }
    #[test]
    fn a_commit_pointer_already_on_the_pending_store_can_relock_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut lease = StoreLock::acquire(dir.path()).unwrap();
        let store = dir.path().join("store");
        lease.lock_store(&store).unwrap();
        lease.lock_store(&store).unwrap();
        assert!(lock(&store.join("ownership.lock")).is_err());
    }
    #[test]
    fn a_symlinked_crypto_database_cannot_bypass_store_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.sqlite3");
        std::fs::write(&other, "fixture").unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        std::os::unix::fs::symlink(other, store.join("matrix-sdk-crypto.sqlite3")).unwrap();
        let mut lease = StoreLock::acquire(dir.path()).unwrap();
        assert!(lease.lock_store(&store).is_err());
    }
    #[test]
    fn rejects_symlink_locks_and_escaping_stores() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("elsewhere", dir.path().join("store.lock")).unwrap();
        assert!(StoreLock::acquire(dir.path()).is_err());
        std::fs::remove_file(dir.path().join("store.lock")).unwrap();
        let mut lease = StoreLock::acquire(dir.path()).unwrap();
        assert!(
            lease
                .lock_store(Path::new("/tmp/outside-matrix-store"))
                .is_err()
        );
    }
}

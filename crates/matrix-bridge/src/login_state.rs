//! Per-login Matrix state directories: naming, ownership markers, and
//! in-place adoption of directories written under the legacy name.
//!
//! Every configured login owns one directory under `<state_dir>/matrix/`. It
//! holds the `session` file, the encrypted crypto `store`, the store lock, and
//! any pending identity-transition journal. Two logins sharing one would share
//! a device and its keys, so the name must be injective over raw login ids.
//!
//! - **New name** (v1): `<readable>-<tag>`, where `readable` is the legacy
//!   sanitized id lowercased and cut to 48 characters, and `tag` is the first
//!   32 hex characters of `SHA-256("chaz-matrix-login-state-v1\0" ‖ raw id)`.
//!   The tag is what makes names distinct; the prefix only helps operators.
//! - **Legacy name**: [`legacy_dir_name`], the old lossy mapping. It is kept
//!   only to find directories written by earlier releases.
//! - **Marker**: `login-id`, a private regular file whose content is
//!   `chaz-matrix-login-state-v1\n<raw id>`. It records which raw login id
//!   owns a directory, so a legacy name shared by several ids, or a hash tag
//!   collision, refuses instead of aliasing two identities.
//!
//! Resolution ([`resolve_logins`]) is read-only and covers every configured
//! login at once. Any refusal stops the whole bridge before anything is
//! written. Directories are never renamed, moved, copied, or deleted: an
//! unambiguous legacy directory is adopted where it is, so the absolute store
//! paths recorded in its `session` and `pending-reset.json` stay valid.
//! [`claim`] writes the marker later, while the caller holds the store lock.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};

use crate::bridge::client::{FullSession, expand_tilde};
use crate::store_lock::no_symlinks;

/// File name of the ownership marker inside a login directory.
pub const MARKER_FILE: &str = "login-id";
const MARKER_HEADER: &str = "chaz-matrix-login-state-v1\n";
const TAG_DOMAIN: &[u8] = b"chaz-matrix-login-state-v1\0";
const PREFIX_MAX: usize = 48;
const TAG_HEX: usize = 32;
/// Longest file name a filesystem accepts; no legacy directory exceeds it.
const NAME_MAX: usize = 255;
/// Bounds the bytes read from a marker or session during resolution.
const READ_LIMIT: u64 = 1 << 20;

/// The lossy legacy directory name: every char outside `[A-Za-z0-9_-]`
/// becomes `_`. Distinct ids can share it, so it is only used to find
/// directories that earlier releases created.
pub fn legacy_dir_name(login_id: &str) -> String {
    login_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The v1 directory name for a raw login id. The id is hashed byte-for-byte:
/// no trimming, Unicode normalisation, or case folding. Lowercasing applies
/// only to the readable prefix, so names stay distinct on case-insensitive
/// filesystems. Always 34..=81 bytes of `[a-z0-9_-]`.
pub fn login_dir_name(login_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(TAG_DOMAIN);
    hasher.update(login_id.as_bytes());
    let digest = hasher.finalize();
    let tag: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut prefix = legacy_dir_name(login_id).to_ascii_lowercase();
    // The legacy mapping is ASCII-only, so a byte cut is a char cut.
    prefix.truncate(PREFIX_MAX);
    format!("{prefix}-{}", &tag[..TAG_HEX])
}

/// The bridge's base state directory, resolved identically by the runtime
/// and by `keys` maintenance: the configured `state_dir` with a leading `~/`
/// expanded, else the platform state dir's `chaz-matrix`.
pub fn state_base(state_dir: Option<&str>) -> anyhow::Result<PathBuf> {
    match state_dir {
        Some(path) => Ok(PathBuf::from(expand_tilde(path))),
        None => dirs::state_dir()
            .map(|dir| dir.join("chaz-matrix"))
            .context("could not determine a state directory"),
    }
}

/// One configured login, as the resolver sees it. The account and homeserver
/// disambiguate a legacy directory that several raw ids map to.
#[derive(Debug, Clone, Copy)]
pub struct Claimant<'a> {
    pub id: &'a str,
    pub username: &'a str,
    pub homeserver_url: &'a str,
}

/// How one login's state directory resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The v1-named directory: already marked for this id, or created on
    /// first use.
    New(PathBuf),
    /// A legacy-named directory that belongs to this id, used in place.
    Adopted(PathBuf),
    /// No safe directory; the reason names paths and ids, never secrets.
    Refused(String),
}

impl Resolution {
    /// The directory to use, or `None` for a refusal.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Resolution::New(path) | Resolution::Adopted(path) => Some(path),
            Resolution::Refused(_) => None,
        }
    }
}

/// Resolve every configured login's state directory under `base`, or refuse
/// the whole set. Refusal writes nothing; the error lists each blocked login
/// with its repair hint. Results are in the order of `logins`.
pub fn resolve_logins(base: &Path, logins: &[Claimant<'_>]) -> anyhow::Result<Vec<Resolution>> {
    let resolved: Vec<Resolution> = logins
        .iter()
        .map(|login| resolve_one(base, login, logins))
        .collect();
    let mut refusals: Vec<String> = logins
        .iter()
        .zip(&resolved)
        .filter_map(|(login, resolution)| match resolution {
            Resolution::Refused(reason) => Some(format!("login {:?}: {reason}", login.id)),
            _ => None,
        })
        .collect();
    let mut owners: HashMap<&Path, &str> = HashMap::new();
    for (login, resolution) in logins.iter().zip(&resolved) {
        if let Some(path) = resolution.path()
            && let Some(other) = owners.insert(path, login.id)
        {
            refusals.push(format!(
                "logins {other:?} and {:?} both resolve to {}",
                login.id,
                path.display()
            ));
        }
    }
    if !refusals.is_empty() {
        bail!(
            "refusing to start any Matrix login; per-login state is ambiguous or unsafe. \
             Nothing was written, moved or regenerated.\n  {}",
            refusals.join("\n  ")
        );
    }
    Ok(resolved)
}

fn resolve_one(base: &Path, login: &Claimant<'_>, all: &[Claimant<'_>]) -> Resolution {
    match try_resolve(base, login, all) {
        Ok(resolution) => resolution,
        Err(error) => Resolution::Refused(format!("{error:#}")),
    }
}

/// What a directory entry is, examined without following symlinks.
enum Entry {
    Absent,
    Dir,
}

fn entry(path: &Path) -> anyhow::Result<Entry> {
    no_symlinks(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Entry::Dir),
        Ok(_) => bail!("{} is not a directory", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Entry::Absent),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// Read a small private file without following a symlink. `None` if absent.
fn read_nofollow(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot open {}", path.display()));
        }
    };
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    ensure!(
        meta.len() <= READ_LIMIT,
        "{} is unexpectedly large",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(READ_LIMIT).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// The raw id recorded in `dir`'s marker, if it has one.
fn marker_owner(dir: &Path) -> anyhow::Result<Option<String>> {
    let path = dir.join(MARKER_FILE);
    let Some(bytes) = read_nofollow(&path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .ok()
        .and_then(|text| text.strip_prefix(MARKER_HEADER).map(str::to_string));
    match text {
        Some(id) => Ok(Some(id)),
        None => bail!(
            "{} is not a v1 login marker; restore it or move the directory aside by hand",
            path.display()
        ),
    }
}

/// The account and homeserver saved in `dir/session`, if one exists. The
/// file's contents never appear in errors: it holds the store passphrase.
fn saved_account(dir: &Path) -> anyhow::Result<Option<(String, String)>> {
    let path = dir.join("session");
    let Some(bytes) = read_nofollow(&path)? else {
        return Ok(None);
    };
    let full: FullSession = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is not a readable Matrix session; contents suppressed",
            path.display()
        )
    })?;
    Ok(Some((
        full.user_session.meta.user_id.to_string(),
        full.client_session.homeserver,
    )))
}

/// Whether a `v1`-named directory holds nothing but a store lock: what
/// [`claim`] leaves if the process stops between creating it and writing the
/// marker. Anything else in an unmarked new-name directory is unexplained.
fn only_lock(dir: &Path) -> anyhow::Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        if entry?.file_name() != "store.lock" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn try_resolve(
    base: &Path,
    login: &Claimant<'_>,
    all: &[Claimant<'_>],
) -> anyhow::Result<Resolution> {
    ensure!(
        !login.id.is_empty(),
        "empty login id; its legacy directory would be {}/matrix itself, which holds every login",
        base.display()
    );
    let matrix = base.join("matrix");
    let new = matrix.join(login_dir_name(login.id));
    let legacy_name = legacy_dir_name(login.id);
    let legacy = matrix.join(&legacy_name);

    // 1. The v1 directory exists: it must be this id's.
    if matches!(entry(&new)?, Entry::Dir) {
        match marker_owner(&new)? {
            Some(owner) if owner == login.id => {}
            Some(_) => bail!(
                "{} is marked for a different login id; refusing to share it",
                new.display()
            ),
            None if only_lock(&new)? => {}
            None => bail!(
                "{} has state but no login marker; add the marker by hand only if it belongs to this login",
                new.display()
            ),
        }
        if legacy != new && matches!(entry(&legacy), Ok(Entry::Dir)) {
            tracing::info!(
                login = login.id,
                ignored = %legacy.display(),
                "Using the v1 login directory; the legacy-named directory is left untouched"
            );
        }
        return Ok(Resolution::New(new));
    }

    // 2. No legacy directory either: a fresh login. A legacy name longer
    // than NAME_MAX could never have been created, so it is not inspected.
    if legacy_name.len() > NAME_MAX || matches!(entry(&legacy)?, Entry::Absent) {
        return Ok(Resolution::New(new));
    }

    // 3. A marked legacy directory belongs to exactly one id.
    if let Some(owner) = marker_owner(&legacy)? {
        return Ok(if owner == login.id {
            Resolution::Adopted(legacy)
        } else {
            Resolution::New(new)
        });
    }

    // 4–6. Unmarked: every configured login whose legacy name could reach
    // this directory may own it. Names are compared ASCII-case-insensitively
    // because a case-insensitive filesystem maps `_Bot_hs` and `_bot_hs` to
    // one directory; on a case-sensitive one this only adds claimants, which
    // can make resolution stricter but never looser.
    let claimants: Vec<&Claimant<'_>> = all
        .iter()
        .filter(|other| legacy_dir_name(other.id).eq_ignore_ascii_case(&legacy_name))
        .collect();
    let session = saved_account(&legacy)?;
    let matching: Vec<&Claimant<'_>> = match &session {
        Some((user, homeserver)) => claimants
            .iter()
            .copied()
            .filter(|c| c.username == user && c.homeserver_url == homeserver)
            .collect(),
        None => Vec::new(),
    };
    let names = || {
        claimants
            .iter()
            .map(|c| format!("{:?}", c.id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (claimants.len(), &session, matching.as_slice()) {
        // 4. A single claimant: adopt unless the saved account disagrees.
        (1, None, _) => Ok(Resolution::Adopted(legacy)),
        (1, Some(_), [only]) if only.id == login.id => Ok(Resolution::Adopted(legacy)),
        (1, Some(_), _) => bail!(
            "saved Matrix account/homeserver in {} differs from this login's configuration; refusing store access",
            legacy.display()
        ),
        // 5. Several claimants, one saved account match: it adopts; the
        // others could never have started here, so they own no state in it.
        (_, Some(_), [only]) => Ok(if only.id == login.id {
            Resolution::Adopted(legacy)
        } else {
            Resolution::New(new)
        }),
        // 6. Ambiguous.
        (_, None, _) => bail!(
            "legacy directory {} has no saved session to tell its claimants apart ({}); \
             write a {MARKER_FILE} marker naming the owner, or remove the extra logins",
            legacy.display(),
            names()
        ),
        (_, Some(_), _) => bail!(
            "legacy directory {} matches {} claimants' account/homeserver ({}); \
             write a {MARKER_FILE} marker naming the owner, or remove the duplicate logins",
            legacy.display(),
            matching.len(),
            names()
        ),
    }
}

/// Record `login_id` as the owner of `root`, or confirm it already is. Call
/// only while holding `root`'s store lock, after any saved session has been
/// checked against the configuration. A marker for another id refuses.
pub(crate) fn claim(root: &Path, login_id: &str) -> anyhow::Result<()> {
    ensure!(!login_id.is_empty(), "empty login id; refusing to mark");
    if let Some(owner) = marker_owner(root)? {
        ensure!(
            owner == login_id,
            "{} is marked for a different login id; refusing store access",
            root.display()
        );
        return Ok(());
    }
    let path = root.join(MARKER_FILE);
    no_symlinks(&path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(MARKER_HEADER.as_bytes())?;
    file.write_all(login_id.as_bytes())?;
    file.sync_all()?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};
    use std::os::unix::fs::{MetadataExt, symlink};

    // Golden v1 tags, computed independently of this code with
    // `printf 'chaz-matrix-login-state-v1\0%s' "$id" | sha256sum | cut -c1-32`.
    const GOLDEN: &[(&str, &str)] = &[
        (
            "@chaz:example.org",
            "_chaz_example_org-9987cde0ec531db5dfeac4f4608f32a3",
        ),
        ("@Bot:hs", "_bot_hs-c166b9cf34168cd3f9e8a051586a74b6"),
        ("@bot:hs", "_bot_hs-fb12297d55af99ea03378a606459fb95"),
        ("@böt:hs", "_b_t_hs-a78ab721aa85890314e3b7dcbefa2dce"),
    ];

    #[test]
    fn golden_v1_names_are_pinned() {
        for (id, name) in GOLDEN {
            assert_eq!(login_dir_name(id), *name, "{id:?}");
        }
    }

    fn collision_ids() -> Vec<String> {
        let long = format!("@{}:hs", "a".repeat(252));
        [
            "@chaz:example.org",
            "@chaz_example.org",
            "@chaz:example_org",
            "_chaz_example_org",
            "@a.b:hs",
            "@a_b:hs",
            "@böt:hs",
            "@bát:hs",
            "@bot:hs\0",
            "@bot:hs/",
            "@bot:hs",
            "@Bot:hs",
            "@BOT:HS",
            " @bot:hs",
            "@bot:hs ",
            "@bot:hs\n",
            "e\u{301}",
            "\u{e9}",
            ".",
            "..",
            "/",
            "-",
            long.as_str(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn names_are_distinct_exactly_and_case_folded_and_bounded() {
        let ids = collision_ids();
        let mut exact = HashSet::new();
        let mut folded = HashSet::new();
        let mut legacy = HashSet::new();
        for id in &ids {
            let name = login_dir_name(id);
            assert!(name.len() <= 255 && name.len() > TAG_HEX, "{name}");
            assert!(!matches!(name.as_str(), "" | "." | ".."));
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'_'),
                "{name}"
            );
            assert_eq!(name, login_dir_name(id), "stable within a process");
            assert!(exact.insert(name.clone()), "exact collision for {id:?}");
            assert!(
                folded.insert(name.to_ascii_lowercase()),
                "case-folded collision for {id:?}"
            );
            legacy.insert(legacy_dir_name(id).to_ascii_lowercase());
        }
        // Negative control: the legacy mapping collides heavily on this
        // table (12 of 23 ids share a case-folded legacy name).
        assert_eq!(ids.len() - legacy.len(), 12, "{legacy:?}");
    }

    /// SHA-256 of every file under `dir`, keyed by relative path, plus the
    /// set of directories, so a test can assert nothing moved or changed.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, String> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let rel = path.strip_prefix(dir).unwrap().to_path_buf();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    out.insert(rel, format!("dir {:o}", meta.mode() & 0o7777));
                    stack.push(path);
                } else if meta.is_symlink() {
                    out.insert(
                        rel,
                        format!("link {}", std::fs::read_link(&path).unwrap().display()),
                    );
                } else {
                    let digest = Sha256::digest(std::fs::read(&path).unwrap());
                    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                    out.insert(rel, format!("file {:o} {hex}", meta.mode() & 0o7777));
                }
            }
        }
        out
    }

    fn claimant<'a>(id: &'a str, user: &'a str) -> Claimant<'a> {
        Claimant {
            id,
            username: user,
            homeserver_url: "https://hs",
        }
    }

    /// A legacy login directory with a session for `user`, a store, and a
    /// pending reset journal whose bytes the tests hash.
    fn legacy_dir(base: &Path, name: &str, user: Option<&str>) -> PathBuf {
        let dir = base.join("matrix").join(name);
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::fs::write(
            dir.join("store/matrix-sdk-crypto.sqlite3"),
            b"crypto fixture",
        )
        .unwrap();
        std::fs::write(dir.join("pending-reset.json"), b"{\"journal\":\"fixture\"}").unwrap();
        if let Some(user) = user {
            let session = serde_json::json!({
                "client_session": {
                    "homeserver": "https://hs",
                    "db_path": dir.join("store"),
                    "passphrase": "fixture-passphrase-never-logged",
                },
                "user_session": {"user_id": user, "device_id": "DEV", "access_token": "fixture-token"},
            });
            std::fs::write(dir.join("session"), session.to_string()).unwrap();
        }
        dir
    }

    fn refused(base: &Path, logins: &[Claimant<'_>]) -> String {
        let before = snapshot(base);
        let error = resolve_logins(base, logins).unwrap_err().to_string();
        assert_eq!(snapshot(base), before, "refusal must write nothing");
        for secret in ["fixture-passphrase-never-logged", "fixture-token"] {
            assert!(!error.contains(secret), "{error}");
        }
        error
    }

    /// Resolve without writing, asserting no byte or rename under `base`.
    fn resolve(base: &Path, logins: &[Claimant<'_>]) -> Vec<Resolution> {
        let before = snapshot(base);
        let resolved = resolve_logins(base, logins).unwrap();
        assert_eq!(snapshot(base), before, "resolution is read-only");
        resolved
    }

    #[test]
    fn fresh_login_gets_the_new_name_and_claim_marks_it_privately() {
        let base = tempfile::tempdir().unwrap();
        let logins = [claimant("@a:hs", "@a:hs")];
        let resolved = resolve(base.path(), &logins);
        let new = base.path().join("matrix").join(login_dir_name("@a:hs"));
        assert_eq!(resolved, vec![Resolution::New(new.clone())]);
        assert!(!new.exists());

        let lease = crate::store_lock::StoreLock::acquire(&new).unwrap();
        claim(&lease.root, "@a:hs").unwrap();
        let marker = new.join(MARKER_FILE);
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            b"chaz-matrix-login-state-v1\n@a:hs"
        );
        assert_eq!(
            std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o700
        );
        claim(&lease.root, "@a:hs").unwrap();
        assert!(claim(&lease.root, "@b:hs").is_err());
        drop(lease);
        assert_eq!(resolve(base.path(), &logins), vec![Resolution::New(new)]);
    }

    #[test]
    fn single_claimant_adopts_legacy_in_place() {
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_hs", Some("@a:hs"));
        let before = snapshot(&legacy);
        let logins = [claimant("@a:hs", "@a:hs")];
        assert_eq!(
            resolve(base.path(), &logins),
            vec![Resolution::Adopted(legacy.clone())]
        );
        claim(&legacy, "@a:hs").unwrap();
        let mut after = snapshot(&legacy);
        assert!(after.remove(Path::new(MARKER_FILE)).is_some());
        assert_eq!(after, before, "adoption changes nothing but the marker");
        // Marked: still adopted, and the new name is never created.
        assert_eq!(
            resolve(base.path(), &logins),
            vec![Resolution::Adopted(legacy)]
        );
        assert!(
            !base
                .path()
                .join("matrix")
                .join(login_dir_name("@a:hs"))
                .exists()
        );
    }

    #[test]
    fn single_claimant_without_session_adopts() {
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_hs", None);
        assert_eq!(
            resolve(base.path(), &[claimant("@a:hs", "@a:hs")]),
            vec![Resolution::Adopted(legacy)]
        );
    }

    #[test]
    fn single_claimant_with_mismatched_session_refuses() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_hs", Some("@other:hs"));
        let error = refused(base.path(), &[claimant("@a:hs", "@a:hs")]);
        assert!(error.contains("differs"), "{error}");
        // Same account on another homeserver is a mismatch too.
        let logins = [Claimant {
            id: "@other:hs",
            username: "@other:hs",
            homeserver_url: "https://elsewhere",
        }];
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_other_hs", Some("@other:hs"));
        refused(base.path(), &logins);
    }

    #[test]
    fn different_account_claimants_split() {
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_b_hs", Some("@a.b:hs"));
        let logins = [
            claimant("@a_b:hs", "@a_b:hs"),
            claimant("@a.b:hs", "@a.b:hs"),
        ];
        let resolved = resolve(base.path(), &logins);
        assert_eq!(
            resolved,
            vec![
                Resolution::New(base.path().join("matrix").join(login_dir_name("@a_b:hs"))),
                Resolution::Adopted(legacy),
            ]
        );
    }

    #[test]
    fn same_account_claimants_refuse() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_b_hs", Some("@a:hs"));
        let logins = [claimant("@a.b:hs", "@a:hs"), claimant("@a_b:hs", "@a:hs")];
        let error = refused(base.path(), &logins);
        assert!(error.contains("2 claimants"), "{error}");
        assert!(error.contains(MARKER_FILE), "{error}");
    }

    #[test]
    fn claimants_without_session_refuse() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_b_hs", None);
        let logins = [
            claimant("@a.b:hs", "@a.b:hs"),
            claimant("@a_b:hs", "@a_b:hs"),
        ];
        let error = refused(base.path(), &logins);
        assert!(error.contains("no saved session"), "{error}");
        // A single unambiguous login elsewhere does not start either.
        let logins = [
            claimant("@a.b:hs", "@a.b:hs"),
            claimant("@a_b:hs", "@a_b:hs"),
            claimant("@c:hs", "@c:hs"),
        ];
        refused(base.path(), &logins);
    }

    #[test]
    fn no_claimant_matching_session_refuses() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_b_hs", Some("@z:hs"));
        let logins = [
            claimant("@a.b:hs", "@a.b:hs"),
            claimant("@a_b:hs", "@a_b:hs"),
        ];
        let error = refused(base.path(), &logins);
        assert!(error.contains("0 claimants"), "{error}");
    }

    #[test]
    fn marker_decides_a_legacy_directory() {
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_b_hs", None);
        std::fs::write(
            legacy.join(MARKER_FILE),
            "chaz-matrix-login-state-v1\n@a_b:hs",
        )
        .unwrap();
        let logins = [claimant("@a.b:hs", "@a:hs"), claimant("@a_b:hs", "@a:hs")];
        assert_eq!(
            resolve(base.path(), &logins),
            vec![
                Resolution::New(base.path().join("matrix").join(login_dir_name("@a.b:hs"))),
                Resolution::Adopted(legacy.clone()),
            ]
        );
        // Raw bytes: a case or whitespace variant is another identity.
        std::fs::write(
            legacy.join(MARKER_FILE),
            "chaz-matrix-login-state-v1\n@A_b:hs",
        )
        .unwrap();
        assert!(matches!(
            resolve(base.path(), &[claimant("@a_b:hs", "@a:hs")])[0],
            Resolution::New(_)
        ));
        std::fs::write(legacy.join(MARKER_FILE), "@a_b:hs").unwrap();
        let error = refused(base.path(), &[claimant("@a_b:hs", "@a:hs")]);
        assert!(error.contains("not a v1 login marker"), "{error}");
    }

    #[test]
    fn new_and_legacy_both_present_uses_new_and_leaves_legacy() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_hs", Some("@a:hs"));
        let new = base.path().join("matrix").join(login_dir_name("@a:hs"));
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join(MARKER_FILE), "chaz-matrix-login-state-v1\n@a:hs").unwrap();
        assert_eq!(
            resolve(base.path(), &[claimant("@a:hs", "@a:hs")]),
            vec![Resolution::New(new)]
        );
    }

    #[test]
    fn new_name_marker_mismatch_or_unexplained_state_refuses() {
        let base = tempfile::tempdir().unwrap();
        let new = base.path().join("matrix").join(login_dir_name("@a:hs"));
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join(MARKER_FILE), "chaz-matrix-login-state-v1\n@b:hs").unwrap();
        let error = refused(base.path(), &[claimant("@a:hs", "@a:hs")]);
        assert!(error.contains("different login id"), "{error}");
        std::fs::remove_file(new.join(MARKER_FILE)).unwrap();
        std::fs::write(new.join("session"), b"{}").unwrap();
        let error = refused(base.path(), &[claimant("@a:hs", "@a:hs")]);
        assert!(error.contains("no login marker"), "{error}");
        // An interrupted first claim leaves only the lock: still usable.
        std::fs::remove_file(new.join("session")).unwrap();
        std::fs::write(new.join("store.lock"), b"").unwrap();
        assert_eq!(
            resolve(base.path(), &[claimant("@a:hs", "@a:hs")]),
            vec![Resolution::New(new)]
        );
    }

    #[test]
    fn id_too_long_for_a_legacy_name_resolves_to_the_new_name() {
        // No earlier release could have created a legacy directory whose name
        // exceeds NAME_MAX, so such an id is simply a fresh login.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("matrix")).unwrap();
        let long = format!("@{}:hs", "a".repeat(300));
        let logins = [claimant(&long, &long), claimant("@a:hs", "@a:hs")];
        let resolved = resolve(base.path(), &logins);
        assert_eq!(
            resolved[0],
            Resolution::New(base.path().join("matrix").join(login_dir_name(&long)))
        );
    }

    #[test]
    fn empty_id_never_adopts_the_parent() {
        let base = tempfile::tempdir().unwrap();
        legacy_dir(base.path(), "_a_hs", Some("@a:hs"));
        let error = refused(base.path(), &[claimant("", "@a:hs")]);
        assert!(error.contains("empty login id"), "{error}");
    }

    #[test]
    fn case_variant_legacy_names_are_claimants_of_one_directory() {
        // On a case-insensitive filesystem `_Bot_hs` *is* `_bot_hs`, so a
        // directory found under either name is claimed by both case
        // variants and must be disambiguated by the saved account. Here
        // `@Bot:hs` saved its session through the lowercase name.
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_bot_hs", Some("@Bot:hs"));
        let ids = [
            claimant("@bot:hs", "@bot:hs"),
            claimant("@Bot:hs", "@Bot:hs"),
        ];
        let resolved = resolve(base.path(), &ids);
        // `@bot:hs` is not the saved account, so it gets a fresh directory
        // rather than refusing as a lone mismatched claimant would. On this
        // case-sensitive test filesystem `_Bot_hs` does not exist, so
        // `@Bot:hs` is fresh too; a case-insensitive one would adopt.
        assert_eq!(
            resolved,
            vec![
                Resolution::New(base.path().join("matrix").join(login_dir_name("@bot:hs"))),
                Resolution::New(base.path().join("matrix").join(login_dir_name("@Bot:hs"))),
            ]
        );
        // Negative control: without its case variant configured, the lone
        // claimant refuses the mismatched session.
        refused(base.path(), &ids[..1]);
        // Without a session the variants cannot be told apart.
        std::fs::remove_file(legacy.join("session")).unwrap();
        let error = refused(base.path(), &ids);
        assert!(error.contains("no saved session"), "{error}");
        // The new names never alias, even case-folded.
        assert_ne!(
            login_dir_name("@bot:hs").to_ascii_lowercase(),
            login_dir_name("@Bot:hs").to_ascii_lowercase()
        );
    }

    #[test]
    fn symlinks_and_non_directories_refuse() {
        let base = tempfile::tempdir().unwrap();
        let real = legacy_dir(base.path(), "real", Some("@a:hs"));
        symlink(&real, base.path().join("matrix/_a_hs")).unwrap();
        let error = refused(base.path(), &[claimant("@a:hs", "@a:hs")]);
        assert!(error.contains("symlink"), "{error}");

        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("matrix")).unwrap();
        std::fs::write(base.path().join("matrix/_a_hs"), b"not a dir").unwrap();
        refused(base.path(), &[claimant("@a:hs", "@a:hs")]);

        // A symlinked marker or session is not followed.
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_hs", None);
        symlink(base.path().join("elsewhere"), legacy.join(MARKER_FILE)).unwrap();
        refused(base.path(), &[claimant("@a:hs", "@a:hs")]);
        std::fs::remove_file(legacy.join(MARKER_FILE)).unwrap();
        symlink(base.path().join("elsewhere"), legacy.join("session")).unwrap();
        refused(base.path(), &[claimant("@a:hs", "@a:hs")]);

        // A symlinked state base is refused as well.
        let outer = tempfile::tempdir().unwrap();
        let target = outer.path().join("target");
        legacy_dir(&target, "_a_hs", Some("@a:hs"));
        symlink(&target, outer.path().join("link")).unwrap();
        let error = refused(&outer.path().join("link"), &[claimant("@a:hs", "@a:hs")]);
        assert!(error.contains("symlink"), "{error}");
    }

    #[test]
    fn claim_refuses_symlinked_markers() {
        let dir = tempfile::tempdir().unwrap();
        symlink(dir.path().join("elsewhere"), dir.path().join(MARKER_FILE)).unwrap();
        assert!(claim(dir.path(), "@a:hs").is_err());
        assert!(!dir.path().join("elsewhere").exists());
    }

    #[test]
    fn unreadable_session_is_suppressed() {
        let base = tempfile::tempdir().unwrap();
        let legacy = legacy_dir(base.path(), "_a_b_hs", None);
        std::fs::write(legacy.join("session"), b"{\"passphrase\":\"fixture-token\"").unwrap();
        let logins = [
            claimant("@a.b:hs", "@a.b:hs"),
            claimant("@a_b:hs", "@a_b:hs"),
        ];
        let error = refused(base.path(), &logins);
        assert!(error.contains("contents suppressed"), "{error}");
    }

    #[test]
    fn state_base_expands_tilde_once_for_every_caller() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            state_base(Some("~/chaz-state")).unwrap(),
            home.join("chaz-state")
        );
        assert_eq!(
            state_base(Some("/var/lib/chaz")).unwrap(),
            PathBuf::from("/var/lib/chaz")
        );
        assert_eq!(
            state_base(None).unwrap(),
            dirs::state_dir().unwrap().join("chaz-matrix")
        );
    }
}

//! Persona — an agent's system prompt, composed from optional file
//! includes plus an optional inline string.
//!
//! Personas replace the legacy `role:` indirection. Where roles were
//! named, reusable templates a chat-bot could swap into, personas are
//! per-agent: a Living Agent owns its persona via `AgentDbConfig.persona`
//! and the resolved text travels with the agent via eidetica sync.
//!
//! Resolution = read each file, hash its bytes, concatenate the contents
//! followed by the inline `prompt`. The hashes are recorded in
//! [`ResolvedPersona::sources`] so a session snapshot has a self-contained
//! audit trail of which files (and which versions of them) shaped the
//! agent's instructions at that point in time.
//!
//! Snapshots are written into the session DB as `EntryType::PersonaSnapshot`
//! entries. Once a snapshot exists, ContextBuilder treats it as authoritative
//! for the system prompt — disk edits do not silently mutate ongoing
//! sessions, the operator must run `/agent persona bump <ref>` to write a
//! new snapshot.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Persona definition: zero or more file includes plus an optional inline
/// prompt. The inline prompt is appended after file content (separated by
/// a blank line) so an operator can layer overrides on top of a shared
/// base file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Persona {
    /// Short label surfaced in `/agent persona show`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// File paths whose contents are concatenated into the system prompt.
    /// Supports `~`/`~/...` expansion. Relative paths resolve against the
    /// `base_dir` passed to [`Persona::resolve`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Inline prompt text appended after file content. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Hash + size record of a persona source file at resolution time. Lives
/// inside [`ResolvedPersona::sources`] so a session snapshot can be
/// audited later: "which files contributed to this agent's prompt on
/// 2026-05-07, and what was in them?"
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaSource {
    /// Absolute path resolved at snapshot time.
    pub path: String,
    /// Size of the file's content in bytes.
    pub bytes: u64,
    /// blake3 hex digest of the file's content (lowercase, 64 chars).
    pub hash_blake3: String,
}

/// Resolved persona — the literal text fed to the LLM as the system
/// message, plus the source manifest used to build it. This is what gets
/// snapshotted into a session DB.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedPersona {
    /// The full system-prompt string.
    pub text: String,
    /// One entry per file include, in resolution order. Empty when the
    /// persona has no `files:` (inline-prompt-only persona).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PersonaSource>,
}

impl Persona {
    /// Resolve the persona: read each file (relative paths against
    /// `base_dir`, `~` expanded against `$HOME`), hash its contents, and
    /// concatenate. Inline `prompt` is appended last.
    ///
    /// Returns an error on the first file that fails to read so a missing
    /// AGENTS.md doesn't silently produce an empty system prompt.
    pub fn resolve(&self, base_dir: &Path) -> anyhow::Result<ResolvedPersona> {
        let mut text = String::new();
        let mut sources = Vec::with_capacity(self.files.len());

        for raw in &self.files {
            let path = expand_path(raw, base_dir)?;
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("Failed to read persona file {}: {e}", path.display())
            })?;
            let hash = blake3::hash(content.as_bytes());
            sources.push(PersonaSource {
                path: path.to_string_lossy().into_owned(),
                bytes: content.len() as u64,
                hash_blake3: hash.to_hex().to_string(),
            });
            append_with_separator(&mut text, content.trim_end_matches('\n'));
        }

        if let Some(p) = &self.prompt {
            append_with_separator(&mut text, p.trim_end_matches('\n'));
        }

        Ok(ResolvedPersona { text, sources })
    }

    /// True when the persona has no file includes and no inline prompt.
    /// Such a persona resolves to the empty string — useful for
    /// stateless agents whose role is fully defined by their tool set.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.prompt.as_deref().unwrap_or("").is_empty()
    }
}

/// Audit-only entry written to a session's `entries` table whenever an
/// agent's persona changes (initial attach, `/agent persona bump`,
/// `/agent set <ref> persona.*`). The most recent snapshot for a given
/// agent is what ContextBuilder injects as the system message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaSnapshotPayload {
    /// Display name of the agent this snapshot belongs to. Mirrored from
    /// `SessionEntry.sender` so a snapshot is self-describing if read
    /// without surrounding context.
    pub agent: String,
    /// The resolved prompt text + source manifest at the time of writing.
    pub resolved: ResolvedPersona,
    /// What triggered this snapshot — used for audit log readability.
    pub reason: SnapshotReason,
    /// Wall-clock time of the snapshot write. Independent of the
    /// `SessionEntry.timestamp` so the payload can be inspected on its own.
    pub written_at: DateTime<Utc>,
}

/// Why a `PersonaSnapshot` was written. Audit-only; drives no behavior.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    /// First snapshot for this agent on this session. Written lazily
    /// the first time the session needs a system prompt and finds no
    /// snapshot — covers fresh sessions, sessions attached without an
    /// explicit `/agent add`, and legacy sessions that predate the
    /// persona feature.
    Initial,
    /// Operator ran `/agent persona bump <ref>` after editing source files.
    Bump,
    /// `/agent set <ref> persona.*` rewrote the persona definition.
    Edit,
}

/// Expand `~`, `~/...`, and resolve relative paths against `base_dir`.
fn expand_path(raw: &str, base_dir: &Path) -> anyhow::Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Persona file path is empty");
    }
    if trimmed == "~" {
        return dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("$HOME is unset; cannot expand '~'"));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("$HOME is unset; cannot expand '~/'"))?;
        return Ok(home.join(rest));
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(base_dir.join(p))
    }
}

/// Append `chunk` to `text`, separating it from existing content with
/// exactly one blank line. Skips appending if `chunk` is empty.
fn append_with_separator(text: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if !text.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.ends_with("\n\n") {
            text.push('\n');
        }
    }
    text.push_str(chunk);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn empty_persona_is_empty() {
        let p = Persona::default();
        assert!(p.is_empty());
        let resolved = p.resolve(Path::new("/")).unwrap();
        assert_eq!(resolved.text, "");
        assert!(resolved.sources.is_empty());
    }

    #[test]
    fn inline_only_persona_skips_sources() {
        let p = Persona {
            prompt: Some("be terse".to_string()),
            ..Default::default()
        };
        let resolved = p.resolve(Path::new("/")).unwrap();
        assert_eq!(resolved.text, "be terse");
        assert!(resolved.sources.is_empty());
    }

    #[test]
    fn file_concat_records_hashes_and_separates_with_blank_line() {
        let tmp = TempDir::new().unwrap();
        let a = write_file(tmp.path(), "a.md", "alpha line\n");
        let b = write_file(tmp.path(), "b.md", "beta line\n");

        let p = Persona {
            files: vec![
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            prompt: Some("trailing override".to_string()),
            ..Default::default()
        };
        let resolved = p.resolve(tmp.path()).unwrap();
        assert_eq!(
            resolved.text,
            "alpha line\n\nbeta line\n\ntrailing override"
        );
        assert_eq!(resolved.sources.len(), 2);
        // Hashes are deterministic blake3 of the literal file bytes.
        assert_eq!(
            resolved.sources[0].hash_blake3,
            blake3::hash(b"alpha line\n").to_hex().to_string()
        );
        assert_eq!(resolved.sources[0].bytes, "alpha line\n".len() as u64);
    }

    #[test]
    fn missing_file_is_a_hard_error() {
        let tmp = TempDir::new().unwrap();
        let p = Persona {
            files: vec![tmp.path().join("nope.md").to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert!(p.resolve(tmp.path()).is_err());
    }

    #[test]
    fn relative_path_resolves_against_base_dir() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "rel.md", "relative\n");
        let p = Persona {
            files: vec!["rel.md".to_string()],
            ..Default::default()
        };
        let resolved = p.resolve(tmp.path()).unwrap();
        assert_eq!(resolved.text, "relative");
    }

    #[test]
    fn tilde_expands_to_home() {
        // Pick a path we know exists under $HOME.
        let home = dirs::home_dir().unwrap();
        let probe = home.join(".persona-tilde-probe");
        fs::write(&probe, "tilde works\n").unwrap();
        let p = Persona {
            files: vec!["~/.persona-tilde-probe".to_string()],
            ..Default::default()
        };
        let resolved = p.resolve(Path::new("/")).unwrap();
        fs::remove_file(&probe).ok();
        assert_eq!(resolved.text, "tilde works");
    }

    #[test]
    fn append_with_separator_handles_edge_cases() {
        let mut s = String::new();
        append_with_separator(&mut s, "");
        assert_eq!(s, "");
        append_with_separator(&mut s, "first");
        assert_eq!(s, "first");
        append_with_separator(&mut s, "second");
        assert_eq!(s, "first\n\nsecond");
        // Idempotent on existing trailing newline.
        let mut t = String::from("alpha\n");
        append_with_separator(&mut t, "beta");
        assert_eq!(t, "alpha\n\nbeta");
    }

    #[test]
    fn snapshot_payload_round_trips_through_json() {
        let payload = PersonaSnapshotPayload {
            agent: "ava".to_string(),
            resolved: ResolvedPersona {
                text: "hello".to_string(),
                sources: vec![PersonaSource {
                    path: "/tmp/foo.md".to_string(),
                    bytes: 5,
                    hash_blake3: "deadbeef".repeat(8),
                }],
            },
            reason: SnapshotReason::Initial,
            written_at: Utc::now(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: PersonaSnapshotPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
    }
}

# Skills & Prompts

> **Status: Design** — replaces the legacy `role` system and the transitional
> `persona` snapshots. Skills become a built-in extension; persona fields
> collapse into Agent config. Roles/routing layer removed entirely.
>
> Reference: IronClaw's `ironclaw_skills` crate, pi's `.pi/skills/` +
> `.pi/prompts/`, Claude Code's `CLAUDE.md` / custom commands.

## Summary

**What dies:**

- `src/role.rs` — removed. The one-release migration window closes.
- `src/persona.rs` — removed. `Persona`, `ResolvedPersona`, `PersonaSnapshot`,
  `PersonaSnapshotPayload`, `SnapshotReason` all gone.
- `migrate_role_to_persona()` — removed.
- All `default_role` / `role:` fields on `Agent`, `AgentDbConfig`, `AgentConfig`.

**What replaces them:**

- **Agent fields** — `system_prompt: String`, `system_prompt_files: Vec<PathBuf>`.
  These live in `AgentDbConfig` and the runtime `Agent` struct. File resolution
  happens at agent construction time — files are read once and the concatenated
  text is what `system_prompt` holds. No per-session snapshot layer.
- **`skills` extension** — a built-in extension that scans skill directories at
  install time, holds a `SkillRegistry` in memory, provides `skill_list` /
  `skill_search` tools, and injects matched skill bodies into the system prompt
  via a new `PromptAugmentation` capability.
- **`PromptAugmentation` cap** — a new extension capability that lets extensions
  append text to the system prompt. The extension hub calls every provider at
  context assembly time. Per-session extension filtering gates participation.

**What stays:**

- `SystemPromptSnapshot` entry type — renamed from `PersonaSnapshot`, now just
  records the final assembled prompt text + a reason. But the snapshot is
  _observational_ (audit-only), not _authoritative_. ContextBuilder always
  assembles fresh from Agent + skills; it doesn't consult snapshots.

## Problem

Today chaz has three tangled layers for what should be one thing:

1. **`RoleDetails`** (`role.rs`) — named static prompt templates. Deprecated but
   still wired: `Agent` carries `default_role`, `AgentDbConfig` carries `role`,
   `Agent::from_*_config()` calls `migrate_role_to_persona()`, ContextBuilder
   falls back to legacy role prompt when no snapshot exists. This is dead code
   walking on a one-release grace period.

2. **`Persona`** (`persona.rs`) — per-agent file includes + inline prompt.
   Resolved to text + file hashes at snapshot time. Snapshotted into session DB
   as `PersonaSnapshot` entries. ContextBuilder treats the latest snapshot as
   authoritative — disk edits to persona source files don't silently mutate
   running sessions. This snapshot-as-authoritative design means every persona
   edit requires a `bump` command and a new session entry.

3. **No skills** — every instruction lives in the agent's persona or role. There
   is no way to load contextual task guidance (e.g. "how to use Nix" only when
   the conversation touches Nix), no trigger matching, no skill catalog.

The ecosystem has converged on a cleaner model:

| Concept               | Role in chaz today              | Role after                                           |
| --------------------- | ------------------------------- | ---------------------------------------------------- |
| Agent identity        | Persona (files + inline prompt) | `Agent.system_prompt` + `system_prompt_files`        |
| Reusable templates    | `RoleDetails`                   | Skills (contextual, trigger-matched)                 |
| Parameterized prompts | None                            | Future: prompt templates with `{{var}}` substitution |
| Audit trail           | PersonaSnapshot (authoritative) | `SystemPromptSnapshot` (observational only)          |

## Model

### Agent fields

The `Agent` struct and `AgentDbConfig` lose `persona`, `default_role`/`role`,
and gain:

```rust
/// The agent's system prompt — who it is, what it does, permanent constraints.
/// This is the text fed to the LLM as the first message.
pub system_prompt: String,

/// Optional files whose content is concatenated into `system_prompt` at agent
/// construction time. File paths are resolved relative to chaz's config
/// directory and expanded for `~`.
///
/// These are read once at construction, never per-turn. To change the prompt,
/// edit files and run `/agent reload <ref>` (which re-reads files and
/// updates the agent's in-memory config + AgentDbConfig).
pub system_prompt_files: Vec<PathBuf>,
```

`Agent::build()` or `Agent::from_db_config()` resolves files → reads → BLAKE3
hashes them → concatenates into `system_prompt`. The runtime Agent carries
the resolved text; disk edits don't silently take effect — the operator must
run `/agent reload` (or restart chaz).

Config schema (`agents:` in yaml):

```yaml
agents:
  - name: ava
    system_prompt: "You are Ava, Patrick's assistant."
    system_prompt_files:
      - ~/AGENTS.md
      - ~/brain/ava/SOUL.md
    # ... model, tools, etc.
```

### Skills extension

A single built-in extension — `skills` in `src/extensions/skills.rs` — that
manages the skill catalog and hooks into context assembly.

#### Skill format

SKILL.md convention: YAML frontmatter + Markdown body.

```markdown
---
name: nix
description: Nix and NixOS package management, configuration, and troubleshooting
triggers:
  - nix
  - nixos
  - nixpkgs
  - flake
  - home-manager
requires_tools: []
---

# Nix skill

Guidelines for working with Nix:

- Use `nix develop .#` not `nix-shell`
- Prefer `home-manager switch` over manual edits
- ...
```

- `name` — unique identifier within the skill catalog
- `description` — one-line summary shown in `skill_list`
- `triggers` — keyword list for deterministic prefiltering (see below)
- `requires_tools` — optional; skill is suppressed when required tools aren't available
- Body — markdown instructions injected into the system prompt

Maximum file size: 64 KiB (IronClaw convention).

#### Discovery paths

Scanned at extension install time, from highest to lowest priority:

1. **Project-local**: `.chaz/skills/` — relative to the session's working
   directory (or the TUI's cwd). For project-specific guidance.
2. **User-global**: `~/.config/chaz/skills/` — available to all agents on this peer.

Duplicate names: project-local wins (shadowing user-global).

#### Trigger matching (prefiltering)

Deterministic, not LLM-driven. At context assembly time, the extension receives
the agent's current turn context (the last N user messages, or the session's
recent entries). For each skill, prefiltering scores trigger matches:

1. Extract all non-common words from recent user messages (stoplist-filtered)
2. For each skill, count trigger matches against extracted words
3. Skills with ≥1 match are "active" for this turn
4. All active skill bodies are concatenated and appended to the system prompt

This is cheap (string matching, no LLM call) and predictable (operator knows
exactly which keywords activate which skill). It deliberately avoids
embedding-based or LLM-based selection to prevent circular manipulation.

#### Trust tiers

Two tiers, matching IronClaw's model:

| Tier        | Location                                   | Trust             | Tool access           |
| ----------- | ------------------------------------------ | ----------------- | --------------------- |
| `trusted`   | `.chaz/skills/`, `~/.config/chaz/skills/`  | Operator-placed   | Full (no restriction) |
| `installed` | Future: `~/.config/chaz/skills/installed/` | Registry download | Read-only tools only  |

v1 is `trusted` only. `installed` depends on a skill registry (future work).

The effective tool ceiling for a turn is `min(agent's tool set, lowest-trust active skill's tool ceiling)` — a single `installed` skill drops the turn to read-only. This prevents privilege escalation through skill mixing.

#### Built-in tools

| Tool           | Risk | Description                                                              |
| -------------- | ---- | ------------------------------------------------------------------------ |
| `skill_list`   | Low  | List loaded skills with name, description, trigger count, trust tier     |
| `skill_search` | Low  | Full-text search across skill names + descriptions + trigger lists       |
| `skill_show`   | Low  | Display the full body of a named skill (for the agent to read on-demand) |

`skill_install` and `skill_remove` are deferred to v2 (registry integration).

#### Per-session filtering

The `skills` extension participates in the standard per-session extension
filtering. A session can disable `skills` via `/extension disable skills`,
suppressing all skill injection for that session's turns.

### PromptAugmentation capability

A new extension capability that lets extensions inject text into the system
prompt before each LLM call.

```rust
/// Capability: append text to the agent's system prompt during context assembly.
///
/// The hub calls every installed extension that provides this cap, collects
/// results, and concatenates them after the agent's system prompt (separated
/// by newlines). Per-session extension filtering gates participation.
#[async_trait]
pub trait PromptAugmentation: Send + Sync {
    /// Return additional text to append to the system prompt, or `None` if
    /// this extension has nothing to add for this turn.
    ///
    /// Called once per turn, before the LLM call. The extension receives:
    /// - `agent` — the agent that will process this turn
    /// - `session_entries` — recent entries from the session (last ~10 messages)
    /// - `session_meta` — session-level metadata (participants, host, etc.)
    async fn augment_system_prompt(
        &self,
        agent: &Agent,
        session_entries: &[SessionEntry],
        session_meta: &SessionMeta,
    ) -> Option<String>;
}
```

The `Capability` enum gains `PromptAugmentation(Arc<dyn PromptAugmentation>)`.
`ExtensionCaps` has `prompt_augmentation: Vec<Arc<dyn PromptAugmentation>>`.

The hub collects all providers, calls each, concatenates non-empty results.

### Context assembly flow

The new assembly order in `ContextBuilder::build()`:

```
1. Agent.system_prompt                              (who I am)
2. Agent.system_prompt_files (already concatenated)  (resolved at construction)
3. ── blank line ──
4. hub.augment_system_prompt(agent, entries, meta)   (skills + any other extensions)
   └── skills extension: active skill bodies, one per matched skill
   └── future extensions: memory surfacing, todo reminders, etc.
5. Optional multi-agent room note                     (existing behavior)
→ RuntimeMessage::System(text)
```

The assembled system prompt is recorded as a `SystemPromptSnapshot` entry in
the session for audit purposes, but ContextBuilder does NOT look up past
snapshots — it always assembles fresh.

### SystemPromptSnapshot (audit-only)

```rust
/// Observational record of the system prompt assembled for a turn.
/// Written once per turn for audit purposes. ContextBuilder does not
/// consult past snapshots — it always assembles fresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptSnapshotPayload {
    /// Which agent this prompt was for
    pub agent_name: String,
    /// The full assembled text fed to the LLM as system message
    pub text: String,
    /// Which extensions contributed (and their versions/hashes)
    pub contributors: Vec<PromptContributor>,
    /// When this snapshot was taken
    pub at: DateTime<Utc>,
    /// Why: InitialAttach, Reload, Edit, Bump (informational only)
    pub reason: SnapshotReason,
}
```

This replaces the current `PersonaSnapshotPayload`. The `reason` field is
retained for audit filtering ("show me all bumps since last week") but
drives no behavior.

### /agent reload command

New shared command replacing `/agent persona bump`:

```
/agent reload <ref>
```

Re-reads `system_prompt_files` from disk, re-hashes, updates the agent's
in-memory `system_prompt` + persists to `AgentDbConfig`. Writes a
`SystemPromptSnapshot` with `reason: Reload`. Unlike the old bump, this
updates the authoritative agent config — there is no snapshot-authoritative
layer to bypass.

### Migration

No on-disk migration. The legacy `role:` fields have been warning on startup
since the persona transition; this closes the window.

1. Remove `src/role.rs` entirely.
2. Remove `src/persona.rs` entirely.
3. Remove `migrate_role_to_persona()` from `src/agent.rs`.
4. Drop `persona`, `default_role`, `role` from `Agent`, `AgentConfig`,
   `AgentDbConfig`.
5. Add `system_prompt`, `system_prompt_files` to all three.
6. Remove `PersonaSnapshotPayload` entry type; add `SystemPromptSnapshotPayload`.
7. Remove `/agent persona` commands; add `/agent reload`.
8. Remove legacy startup warnings for `role:` config usage.
9. Add `PromptAugmentation` capability to `src/extension/caps.rs`.
10. Create `src/extensions/skills.rs` with `SkillRegistry` + `PromptAugmentation` impl.
11. Update `ContextBuilder::build()` to call hub for augmentations.
12. Register `skills` extension in `src/main.rs` builtins list.

Config migration for operators: replace `persona:` + `role:` in agent configs:

```yaml
# Before
agents:
  - name: ava
    persona:
      files: [~/AGENTS.md, ~/brain/ava/SOUL.md]
      prompt: "You are Ava."
    role: assistant

# After
agents:
  - name: ava
    system_prompt: "You are Ava."
    system_prompt_files:
      - ~/AGENTS.md
      - ~/brain/ava/SOUL.md
```

The `role:` name had no semantic value (it was just a template key, not
routing-affecting) — it disappears without replacement.

### V2: Eidetica-backed skill libraries

Deferred but specced so the extension model accommodates it:

- `SkillLibraryDb` — an eidetica DB kind holding many skills in a Table.
  `meta.kind = "skill_library"`. Each row is a serialized `SkillManifest` +
  `prompt_content`.
- Agent's `skills` config gains `SkillSource::Library { db_id, name }` — a
  reference to a synced library.
- `skill_library_<name>` becomes a separate extension (one per library), same
  pattern as `mcp-<server_name>`. Each library extension provides its own
  `PromptAugmentation` implementation that queries the library DB.
- `skill_install` / `skill_remove` tools copy between folders and library DBs.
- Libraries can be shared/synced via AuthSettings, exactly like memory banks.

The v1 folders-only model is a strict subset — adding libraries later adds
extensions, not new abstractions.

## Implementation Touch Points

| File                       | Change                                                                                                                                                       |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `src/role.rs`              | **Delete**                                                                                                                                                   |
| `src/persona.rs`           | **Delete**                                                                                                                                                   |
| `src/agent.rs`             | Drop `persona`, `default_role`; add `system_prompt`, `system_prompt_files`; drop `migrate_role_to_persona()`                                                 |
| `src/agent_db.rs`          | `AgentDbConfig` loses `persona`, `role`; gains `system_prompt`, `system_prompt_files`                                                                        |
| `src/config.rs`            | `AgentConfig` schema: same field swap                                                                                                                        |
| `src/context.rs`           | `ContextBuilder::build()`: drop snapshot lookup, add hub augmentation call                                                                                   |
| `src/extension/caps.rs`    | Add `PromptAugmentation` trait + `Capability::PromptAugmentation` variant                                                                                    |
| `src/extension/mod.rs`     | `ExtensionHub::augment_system_prompt()` — iterates providers, concatenates                                                                                   |
| `src/extensions/skills.rs` | **New** — `SkillRegistry`, `SkillManifest`, SKILL.md parser, trigger prefiltering, `skill_list`/`skill_search`/`skill_show` tools, `PromptAugmentation` impl |
| `src/extensions/mod.rs`    | Add `pub mod skills;`                                                                                                                                        |
| `src/main.rs`              | Register `skills` in builtins list                                                                                                                           |
| `src/server.rs`            | `write_persona_snapshot()` → `write_system_prompt_snapshot()`; snapshot writes on initial attach + reload, not on first LLM call                             |
| `src/session/agents.rs`    | Same snapshot rename; `/agent persona` commands removed                                                                                                      |
| `src/commands/agent.rs`    | Remove `persona` sub-commands; add `/agent reload <ref>`                                                                                                     |
| `src/types.rs`             | Entry type: `PersonaSnapshot` → `SystemPromptSnapshot`; `SnapshotReason` stays                                                                               |
| `src/defaults.rs`          | Built-in agent defs: `persona` → `system_prompt` + `system_prompt_files`                                                                                     |
| `docs/src/`                | User guide: skills directory, SKILL.md format, `/agent reload`; architecture: PromptAugmentation cap                                                         |

## Testing

- Unit: SKILL.md parser (valid frontmatter, missing fields, oversized files)
- Unit: trigger prefiltering (empty context, all matches, no matches, stoplist filtering)
- Unit: trust tier tool ceiling (trusted + installed active = read-only)
- Unit: per-session extension filtering (skills disabled → no injection)
- Unit: `PromptAugmentation` hub collection (empty, one provider, multiple providers, per-session filter)
- Unit: `Agent::build()` file resolution (missing file, multiple files, empty files)
- Unit: `SystemPromptSnapshot` round-trip
- Integration: `/agent reload` with file edit → new system prompt on next turn
- Integration: skill triggers match in TUI session → skill body appears in assembled prompt
- Integration: old config with `persona:` / `role:` → clear startup error with migration instructions

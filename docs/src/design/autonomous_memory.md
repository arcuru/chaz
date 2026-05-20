# Autonomous Memory Extension

> **Status: Design** — extends the existing `memory` extension with autonomous
> surfacing (PromptAugmentation), a `MemoryAccess` cap impl, and config-driven
> bank attachment. All storage stays in eidetica (AgentDb + MemoryBankDb).
>
> References: pi's `memories/` pipeline (extraction → consolidation → injection),
> chaz's existing `MemoryAccess` cap trait, the `PromptAugmentation` pattern
> from the `skills` extension.

## Summary

**What exists:**

- `src/extensions/memory.rs` — registers `remember`, `recall`, `list_memory_banks` tools
- `src/agent_db.rs` — per-agent `memory` store (Table<MemoryEntry>)
- `src/memory_bank_db.rs` — standalone shared memory bank DBs
- `src/commands/memory.rs` — `/memory new|list|delete|grant|revoke|share|import`
- `src/extension/caps.rs` — `MemoryAccess` trait (defined, no impl), `PromptAugmentation` trait
- BM25 + cosine hybrid search in `src/tools/memory.rs::search_memory()`

**What this adds:**

- **`PromptAugmentation`** — at context assembly, search agent memory + attached
  banks for facts relevant to recent messages, inject into system prompt
- **`MemoryAccess`** cap impl — thin wrapper over the existing eidetica-backed
  search/write paths, consumable by other extensions
- **`default_memory_banks`** — agent config field listing banks to attach at
  agent startup (declarative, not imperative)
- **`/memory attach` / `/memory detach`** — per-session bank attachment for
  shared context (e.g. "attach the `ava-facts` bank to this session")

**What's deferred (v2):**

- Autonomous session transcript extraction (OMP's phase 1: read past sessions →
  extract durable knowledge)
- Cross-session memory consolidation (OMP's phase 2: merge per-session
  extractions into a curated long-term document)
- These require LLM-driven summarization and a job pipeline; v1 focuses on
  surfacing what agents already store with `remember`.

## Problem

Today the `memory` extension provides manual tools (`remember`/`recall`) but
does nothing autonomous. An agent must explicitly remember facts and explicitly
recall them. There's no:

1. **Autonomous surfacing** — relevant memories don't appear in the system
   prompt unless the agent calls `recall`. The agent's own stored knowledge
   sits unused.
2. **Cross-agent shared context** — memory banks exist (`/memory grant` works)
   but there's no config-driven attachment. Banks are manually attached per
   session.
3. **Capability surface for other extensions** — the `MemoryAccess` trait exists
   but has no provider. Other extensions can't consume memory without
   duplicating the DB access glue.
4. **No memory pipeline** — sessions don't learn from each other. Every session
   starts cold; the only bridge is what agents manually remember.

The `skills` extension already demonstrates the pattern: an extension provides
`PromptAugmentation`, the hub calls it at context assembly, and the extension
decides what text to inject. Memory surfacing is the same shape — search for
relevant facts, inject them.

## Model

### Context assembly order

```
1. Agent.system_prompt                                    (who I am)
2. Agent.system_prompt_files                              (resolved at construction)
3. ── blank line ──
4. hub.augment_system_prompt(agent, entries, meta)         (skills + memory + future)
   ├── skills: matched skill bodies
   ├── memory: surfaced relevant facts                    ← NEW
   └── future: todo reminders, etc.
5. Optional multi-agent room note
→ RuntimeMessage::System(text)
```

### PromptAugmentation: memory surfacing

The `MemoryPromptAugmentation` provider:

1. Receives `agent_name` and `recent_message_text` (the last ~5 user/assistant
   messages as text)
2. Extract meaningful query terms from those messages (stoplist-filtered,
   same tokenizer as `recall`)
3. Search the agent's own memory store (always active)
4. Search each attached bank (from `memory_banks` store)
5. Combine results with RRF (k=60), take top 5
6. Format as a compact block:

```
## Relevant Memories

- [key]: value (from: self)
- [key]: value (from: bank:ava-facts)
...
```

Returns `None` when no relevant memories found (common case — keeps the prompt
clean).

### MemoryAccess cap impl

The extension publishes `CapProvider::MemoryAccess(Arc<MemoryAccessImpl>)`.
The impl delegates to the existing `search_memory` and `write_memory_entry`
functions in `src/tools/memory.rs` — no duplication.

```rust
impl MemoryAccess for MemoryAccessImpl {
    fn search(&self, query: &str, scope: MemoryScope) -> CapFuture<Vec<MemoryHit>>;
    fn remember(&self, key: &str, value: &str, scope: MemoryScope) -> CapFuture<()>;
}
```

`MemoryScope::Agent` → agent's own `AgentDb::memory`.
`MemoryScope::Bank { name }` → resolve bank name via `HostedIndex`, search
`MemoryBankDb::memory`.

This makes memory consumable by other extensions (e.g. a future `todo`
extension that stores checklist state in a shared bank without writing
its own DB glue).

### Config: default_memory_banks

New field on `AgentDbConfig` (and yaml `agents:`):

```yaml
agents:
  - name: ava
    system_prompt: "You are Ava..."
    default_memory_banks:
      - ava-facts
      - chaz-conventions
```

At agent DB bootstrap / reload:

1. For each bank name in `default_memory_banks`:
2. Find the bank DB via `HostedIndex` (or `find_memory_bank`)
3. If the agent doesn't already have a `MemoryBankRef` to it:
   - Grant `Write` auth on the bank DB to the agent's pubkey
   - Write a `MemoryBankRef { db_id, name, permission: Write }` into the
     agent's `memory_banks` store
4. Same pattern as `memory_grant` in `src/commands/memory.rs`

This is idempotent — re-running bootstrap on an existing agent DB skips
already-attached banks. Removing a bank from `default_memory_banks` does
NOT auto-detach (detach is explicit to avoid data loss).

### /memory attach and /memory detach

Session-scoped commands for shared context:

```
/memory attach <bank_name>
```

Adds the bank to the current session's active bank set (stored in
`session.extension_settings["memory"].attached_banks`). The bank must
already be granted to the agent running this session.

```
/memory detach <bank_name>
```

Removes the bank from the session's active set. Does not revoke the
grant — the bank remains accessible via `remember bank=<name>` and
future sessions.

Session-scoped attachment is lighter weight than the global
`default_memory_banks` — it's for temporary context sharing ("add the
project conventions bank while we're working on this feature").

### Extension structure

```
src/extensions/memory.rs  (modified)
├── MemoryExtension
│   ├── fields: Arc<SessionRegistry>, HostedIndex, Option<Arc<dyn Embedder>>
│   ├── manifest: provides [MemoryAccess, PromptAugmentation]
│   │             requires [ToolRegistration, CommandRegistration]
│   └── install: registers tools, commands, returns handlers
├── MemoryPromptAugmentation  (new)
│   └── impl PromptAugmentation
├── MemoryAccessImpl          (new)
│   └── impl MemoryAccess
├── MemoryAttachTool          (new)
│   └── Tool: /memory attach <bank>
├── MemoryDetachTool          (new)
│   └── Tool: /memory detach <bank>
└── (existing Remember/Recall/ListMemoryBanks stay)
```

### What we do NOT do (v2 territory)

OMP's memory pipeline has three phases we're explicitly deferring:

1. **Per-session extraction** — reading past session transcripts with an LLM
   to extract durable knowledge (technical decisions, resolved failures,
   recurring workflows). This is expensive (one LLM call per past session)
   and needs a job queue.

2. **Cross-session consolidation** — merging per-session extractions into a
   curated `MEMORY.md` document and generated skill playbooks. This is the
   "what did we learn across all sessions" step.

3. **`memory://` URLs** — internal URL protocol for reading memory artifacts.
   Chaz doesn't have an internal-URL framework yet.

v1 gives agents the ability to surface what they already store. v2 adds the
pipeline that populates memory automatically from session history.

### Configuration reference

```yaml
# In config.yaml or agent DB config:
agents:
  - name: ava
    system_prompt: "..."
    default_memory_banks:
      - ava-facts # auto-attached at bootstrap
      - project-conventions
# Per-session attachment (via slash commands, not config):
# /memory attach ava-facts
# /memory detach ava-facts
```

### Trust model

Memory surfacing via `PromptAugmentation` is read-only — it searches
existing memory stores but never writes. The write path stays with the
existing `remember` tool and the `MemoryAccess::remember` cap method
(both gated by eidetica AuthSettings on the target DB).

Bank attachment (`default_memory_banks` + `/memory attach`) grants Write
by default (matching the existing `/memory grant` convention). Session-scoped
attachment only affects which banks are searched for surfacing — the
underlying grant is per-agent, not per-session.

## Implementation plan

1. **Add `default_memory_banks` to `AgentDbConfig`** — new field, serialized to
   agent DB `config` store. Bootstrap reads it, grants+attaches banks
   idempotently.

2. **Add `MemoryPromptAugmentation`** — searches agent memory + attached banks,
   returns top-5 formatted block. Registers via `build_providers()`.

3. **Add `MemoryAccessImpl`** — thin delegate to existing `search_memory` /
   `write_memory_entry`. Registers via `build_providers()`.

4. **Update manifest** — `provides_capabilities` gains `MemoryAccess` and
   `PromptAugmentation`. `required_capabilities` gains `CommandRegistration`.

5. **Add `/memory attach` and `/memory detach`** — session-scoped bank
   attachment stored in `extension_settings["memory"].attached_banks`.

6. **Tests** — unit tests for surfacing relevance ranking, integration test
   for the full context assembly pipeline with memory injection.

## Open questions

- **Embedder dependency**: memory surfacing quality depends heavily on the
  embedder. Without one, it falls back to BM25-only search which is keyword-
  based and less semantic. Is BM25-only good enough for v1, or should we
  require an embedder config?
- **Token budget**: surfacing 5 memory entries could be ~500 tokens. Should
  this be configurable per-agent? (Default: top 5, max 200 chars per entry.)
- **Surfacing frequency**: every turn, or only when the agent hasn't called
  `recall` recently? The OMP model injects once at session start. Chaz's
  per-turn assembly means surfacing runs every turn.

# Model Catalog Cache

> **Status: Complete** — per-machine cache of live backend model catalogs,
> backed by the `chaz_peer` eidetica DB. Powers the TUI model picker (full
> live catalog without a per-open network round-trip) and the runtime's
> zero-config context-window budgeting.

## Summary

`ModelCatalogCache` (`crates/lib/src/model_catalog_cache.rs`) persists the
result of a backend's live `/models` fetch so two consumers can read it back
without hitting the network:

1. **The TUI model picker** — renders the full OpenRouter-style catalog
   (hundreds of models, with pricing and modality badges).
2. **The runtime context budget** — `Server::warm_catalog_windows` seeds
   `BackendManager`'s context-window overlay from the cache at startup, so a
   model's real window bounds the per-turn budget *without* anyone declaring
   `context_window:` in YAML. See
   [Bounding the budget by the window](#bounding-the-budget-by-the-window).

The cache lives in `chaz_peer`, which is an **append-only eidetica DB**: every
write is retained in history forever. That property drives the storage-format
decisions below — the goal is to store the smallest blob that still serves
both consumers, and to write it as rarely as correctness allows.

## Storage layout

- **DB:** `chaz_peer` (per-machine peer DB).
- **Store:** one `DocStore` named `model_catalog`.
- **Key:** the backend set's canonical key, `backends-v2:{sorted,backend,names}`
  (`model_catalog_cache::cache_key`). One key holds that backend-set's *entire*
  catalog as a single value, so a refetch logically overwrites the prior entry
  rather than spraying one key per model. The `v2` prefix lets the shape evolve
  without colliding with older entries; changing the configured backends yields
  a different key (and thus a clean cache slot) instead of serving a stale
  config's catalog. Both the writer (picker) and the reader (runtime warm)
  derive the key through the same function, so they cannot drift.
- **Value:** a JSON-encoded `CachedCatalog`:

  ```jsonc
  {
    "fetched_at": "2026-06-07T12:00:00Z",   // RFC3339; freshness/TTL anchor
    "models": [ /* Vec<ModelInfo> */ ]
  }
  ```

## What `ModelInfo` serializes to

`ModelInfo` (`crates/lib/src/backends.rs`) is the single shape used end-to-end:
it is the runtime display type, the picker row, **and** the persisted catalog
entry — there is no parallel "cached" struct to keep in sync. (An earlier
`CachedModel` duplicate was deleted once `ModelInfo` grew serde derives.)

```rust
pub struct ModelInfo {
    pub id: String,                          // always written — the model name / key
    pub price_input: Option<f64>,            // USD per 1M tokens
    pub price_output: Option<f64>,
    pub price_cache_read: Option<f64>,
    pub input_modalities: Vec<String>,       // "text", "image", ...
    pub output_modalities: Vec<String>,
    pub context_window: Option<u32>,         // tokens, when the provider reports it
}
```

Every field except `id` is annotated:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]   // optionals
#[serde(default, skip_serializing_if = "Vec::is_empty")]     // lists
```

- **`skip_serializing_if`** keeps absent values *out of the stored JSON
  entirely*. A model that only reports an id and a context window serializes to
  `{"id":"…","context_window":200000}` — not a row padded with six `null`s.
  Because the cache is append-only, every byte we omit is a byte that does *not*
  accumulate in history forever. Across a few-hundred-model catalog this is the
  difference between a lean blob and one carrying mostly nulls.
- **`#[serde(default)]`** is the read-side counterpart: entries written by an
  older shape (or by a provider that omits a field) deserialize cleanly, with
  missing fields falling back to `None` / empty. The two annotations are a
  matched pair — skip on write, default on read — so the format can shrink or
  grow fields without a migration.

A model with **no** declared window simply has no `context_window` key; on read
it becomes `None`, and the runtime falls back to the static budget for it.

## Churn discipline — why writes are rare

Append-only history means a *write* is the cost, not the live size. The cache
is therefore written only when it must be:

- **TTL-gated reads.** `spawn_catalog_load` checks `CachedCatalog::is_fresh`
  (TTL `MODEL_CATALOG_TTL`, currently 24h) and serves the cached value without
  refetching or rewriting when fresh. A normal session that opens the picker
  repeatedly writes **zero** new entries.
- **Writes only on miss / stale / explicit refresh.** A new entry is appended
  only on a cache miss, a TTL expiry, or a user-triggered force-refresh. So the
  steady state is roughly one write per backend-set per day, regardless of how
  often the catalog is *read*.
- **Single key, whole catalog.** One value per backend set (not per model)
  means a refetch is one appended entry, not hundreds of keyed writes.

### Deliberate non-goals

- **No history pruning.** eidetica retains history by design; we do not attempt
  to compact the `model_catalog` store. The combination of small per-entry
  blobs and ~daily write cadence keeps growth modest, so pruning would add
  complexity for little gain. If a future audit shows the store growing
  faster than expected, revisit here first.
- **Pricing is the volatile field, not the window.** Prices change far more
  often than context windows. We still cache pricing (the picker needs it), but
  it is the field most likely to make a stored entry "wrong" before its TTL
  elapses — a known and accepted staleness, surfaced to the user as catalog
  data, never used to make a silent runtime decision.

## Bounding the budget by the window

The runtime reads only one thing out of this cache: context windows.

- `ModelCatalogCache::context_windows(&backend)` reads the catalog for the
  backend's `cache_key` and returns `{ model_id -> window }` for every model
  that declares one. Freshness is deliberately **ignored** here: a context
  window is a near-static property, and a slightly-stale window still beats the
  model-blind static default it replaces.
- `Server::warm_catalog_windows` runs once at startup (in `main.rs`, right after
  the multi-agent tuning) and pushes that map into the default backend via
  `BackendManager::set_catalog_windows`. The overlay lives behind a shared
  `Arc`, so the update also reaches every per-session worker backend cloned from
  the default at `register_session`.
- `BackendManager::context_window` then resolves in two tiers:
  1. a YAML-declared `context_window:` (explicit operator intent — wins);
  2. the catalog overlay (the zero-config path).

  `None` only when neither tier knows the model, in which case the budget falls
  back to the configured `max_context_tokens`. The clamp itself lives in
  `server::clamp_budget_to_window`: a known window is the budget ceiling that a
  model-blind static default must not cap, while an explicit per-agent cap may
  still lower it.

A fresh machine with no catalog yet fetched is a clean no-op: the overlay stays
empty and the runtime behaves exactly as before until the picker populates the
cache; the next start warms from it.

## Known limitation / follow-up

The startup warm reads the cache **once**. A live `/models` refetch triggered
from the picker mid-session updates the persisted cache but not the running
server's overlay — the picker builds its own `BackendManager`, which does not
share the server's `Arc`. The new windows take effect on the next start. Live
propagation (the picker calling back into `Server::warm_catalog_windows` after a
successful fetch) is intentionally deferred: window changes are rare and a
restart suffices, so the coupling isn't yet worth it.

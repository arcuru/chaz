# Model Info Store

> **Status: Complete** — per-machine store of pulled `ModelInfo` for the models
> actually **in use**, backed by the `chaz_peer` eidetica DB. Feeds the
> runtime's zero-config context-window budgeting. The TUI model picker pulls the
> full live catalog into memory only — it is never persisted.

## Summary

There are two different needs around model metadata, and they are served
differently on purpose:

1. **Browsing** (the TUI picker) wants the *whole* provider catalog — hundreds
   of models with pricing and modality badges — but only transiently, while the
   picker is open. This is pulled live from the backend's `/models` endpoint and
   cached **in memory** for the session (`App::session_catalog`). It is never
   written to disk.
2. **Budgeting** (the runtime) wants the context window for the handful of
   models a peer actually *uses*, available at startup without a network call.
   This — and only this — is persisted, in `ModelInfoStore`.

Splitting them this way keeps the durable footprint tiny and bounded by usage,
which matters because `chaz_peer` is an **append-only** eidetica DB (SQLite
under the hood) with no entry compaction: anything written there is retained
forever. A full-catalog blob rewritten on every refresh would accrete
indefinitely; an in-use-only map rewritten only when a genuinely new model
enters use does not.

## What's persisted, and where

- **DB:** `chaz_peer` (per-machine; not the synced `chaz_group`).
- **Store:** one `DocStore` named `model_info`.
- **Key:** a single fixed key, `in_use`.
- **Value:** a JSON `BTreeMap<model_id, ModelInfo>` — the set of models in use.
  `BTreeMap` so the serialization is order-stable, which makes the
  no-write-on-unchanged check below exact.

`ModelInfoStore` (`crates/lib/src/model_info_store.rs`) exposes:

- `all() -> BTreeMap<id, ModelInfo>` — read the whole in-use set.
- `context_windows() -> {id -> window}` — the windows the runtime warms from.
- `put(&ModelInfo)` — upsert one model. **No-ops when the stored entry already
  equals the input**, so re-using an already-recorded model appends nothing to
  the append-only DB. This is the core churn guard.

### `ModelInfo` serialization

`ModelInfo` (`crates/lib/src/backends.rs`) is the single shape used end-to-end —
runtime display type, picker row, and the persisted value here. Every field
except `id` pairs `#[serde(default)]` (older/sparser entries load cleanly) with
`skip_serializing_if` (absent values aren't written at all), so a stored entry
carries only the fields a model actually has. With the in-use set already tiny,
this keeps each persisted map about as small as it can be.

## How the store gets populated

A model lands in the store via **two** triggers (both, by design):

1. **On switch** — when you pick a model in the TUI picker,
   `dispatch_model_selection` hands the merged `ModelInfo` to
   `Server::cache_model_info`, which slots its window into the live overlay and
   `put`s it into the store.
2. **On first runtime use** — when the runtime is about to budget a turn for a
   model whose window it doesn't yet know, `spawn_model_window_fetch` (via
   `Server::ensure_model_window_cached`) fetches that model's info from the
   live catalog **in the background**, slots the window into the overlay so the
   *next* turn is window-aware, and `put`s it. Non-blocking — the current turn
   proceeds model-blind. Deduped per model id via an in-flight set, so a burst
   of turns on a new model triggers exactly one fetch.

Trigger 1 covers models you explicitly select; trigger 2 covers models you use
without ever opening the picker (e.g. a YAML-configured agent default). Together
they make the store self-populating from real usage, with no `context_window:`
ever required in config.

## How the runtime uses it

- At startup, `Server::warm_model_windows` reads `context_windows()` into the
  default backend's overlay (`BackendManager::set_model_windows`). The overlay
  lives behind a shared `Arc`, so it also reaches every per-session worker
  backend cloned from the default.
- `BackendManager::context_window` resolves in two tiers: a YAML-declared
  `context_window:` (explicit operator intent — wins), then the overlay (the
  zero-config path). `None` only when neither knows the model, in which case the
  budget falls back to the configured `max_context_tokens`.
- The clamp lives in `server::clamp_budget_to_window`: a known window is the
  budget ceiling a model-blind static default must not cap, while an explicit
  per-agent cap may still lower it.
- The same budget surfaces in the TUI status bar as `ctx N%`.
  `Server::effective_context_budget` returns the concrete denominator and
  resolves windows through the server's **own** `default_backend` (the one that
  gets warmed and updated) — not a caller's freshly-built manager, whose overlay
  would be empty.

A fresh machine that has never used or picked a model is a clean no-op: the
overlay stays empty and the runtime uses the static budget until the first use
or selection populates the store; the next startup warms from it.

## The in-memory picker catalog

The browse catalog (`App::session_catalog`) is pulled live on first open and
reused for the rest of the session (instant reopen, no re-fetch). The picker's
refresh binding clears it to force a re-pull. It is gone on restart, where the
next open re-fetches. Nothing about browsing touches the store — only selecting
or using a model does.

The tradeoff is deliberate: we give up an instant/offline picker across restarts
(one `/models` HTTP call on first open per session) in exchange for never
persisting hundreds of disposable models into an append-only DB.

## Why `chaz_peer`, given the append-only constraint

eidetica (0.2.0, SQLite-backed) exposes no entry-level compaction or GC, so the
DB file only ever grows. We accept persisting the in-use set there anyway,
deliberately:

- The set is tiny (distinct models used) and `put` writes nothing on unchanged
  entries, so growth is negligible — and `chaz_peer` is machine-local, so
  nothing replicates across devices.
- Keeping one storage story — everything in eidetica, no second file-IO path
  with its own atomic-write/XDG-path/error handling — is worth more than the
  few bytes a `$XDG_CACHE_HOME` file would save.
- The real fix is upstream: the planned eidetica direction is **content-addressed
  blob storage** (store a large value once, reference it by hash), at which
  point even a re-fetched-but-unchanged value dedupes to its existing hash and
  stops accreting. When that lands, this rides it for free.

If that upstream story stalls and the store grows faster than expected, the
fallback is a `$XDG_CACHE_HOME/chaz/` file — `ModelInfoStore`'s surface
(`all`/`put`/`context_windows`) is the same either way, so only the backend
swaps.

# Editing extension settings

An extension can use its settings document for more than configuration. For example, `session_naming` reads `model` but also records `attempted` before making a request. Turning every key in that document into a form control would make a retry marker look like a preference. It would also leave nothing to edit on a fresh session.

This design adds a field editor to the existing [scoped Settings pages](../user_guide/tui.md#settings). It does not change how extensions are installed, activated or granted capabilities.

## Which fields appear?

Give `Extension` a provided `setting_fields()` method returning an empty slice by default. Consult the **registered declaration**, including in a command-only client; do not instantiate an extension to discover its fields. Each entry describes one public preference:

- A stable, literal top-level key, unique within the extension; a short label and one-line help. The pair `(registered extension name, key)` is the identity, never the label, translated text or list index.
- A value kind with its constraints. The first controls need only a boolean, a nullable model identifier and an unsigned integer with inclusive bounds. Model identifiers use the ordinary configured route syntax; choosing a model does not test the route. A missing key and a present JSON `null` are different.
- The default comes from that key in the extension's `default_settings()` object. Validate that it exists and fits the kind. Reject empty/duplicate keys and inconsistent declarations at registration in both client and executor paths. Do not infer declarations from defaults.
- The allowed edit targets: agent default, session override, or both. A declaration permits proposing an edit, not performing it. A field without write access is inspect-only. No declaration means no generic control, even if a value is stored. No executable callbacks, secret fields, nested path syntax or arbitrary JSON editor.

| First fields              | Kind and default                                                  | Meaning                                                                                               |
| ------------------------- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `session_naming.enabled`  | Boolean, `false`                                                  | Permit the existing automatic naming hook on an eligible turn.                                        |
| `session_naming.model`    | Nonblank single-line model ID, or `null` (default)                | `null` selects the turn model, even if an agent preference names a different one.                     |
| `context_prune.min_bytes` | Unsigned byte count fitting the existing `usize` setting, `16384` | Minimum result size to consider for substitution; `0` is a value. This does not enable the projector. |

All three allow both targets. The second extension exercises a different type and runtime consumer without introducing another feature. The one-attempt marker, usage, status, attached resources and credentials are not fields. **Generate now** belongs to a separate session command/action; it must not be represented by a boolean or run on open, save or reset. Further naming policy can add descriptors when the extension actually implements it.

## Where does a value come from?

For each declared key independently, take the first **present** value:

1. The explicit field in the selected session's `extension_settings_v2` document (or the existing read-compatible retained value).
2. The preference in the **owning agent's** Agent DB, if the field admits agent configuration.
3. The extension default.

A session pin applies to the whole session, as `/extensions set` does today. In a shared session, Alpha and Beta can see different inherited models; a session model pin wins for both. Reset that pin and each sees its own model again. Do not store the effective object on create, import, attach, open or first read. Resolve for the actual turn owner, not the display name of the host, first roster row, or the last agent the frontend inspected. A running turn need not restart; the next use reads current settings. No fan-out writes to existing sessions.

Keep session pins in the [native v2 format](../architecture/extensions.md#per-session-settings). Store only explicit agent preferences in the Agent DB's existing `config` DocStore: one sibling key `extension:` followed by `field_key(extension_name)`, holding a native document of fields using the same key/value encoding and delta semantics as session settings. For example, `config["extension:session_naming"].model` is separate from `config["value"]`. The latter is an atomic agent definition: putting an unrecognized map inside it would let an older config writer erase preferences on an unrelated edit. Do not serialize a second copy into that record, the peer DB, root YAML or managed folder. A folder reload replaces its existing editable document; it neither imports nor discards these preferences. An Agent DB import or sync carries the agent's explicit choices, not a snapshot of any session.

Older extensions need no changes; readers of the old config record and folder continue to see the same bytes. Older executors still use session settings only: upgrade executors before relying on agent defaults, and expect old behavior on binary rollback. A session pin retains its wire meaning across that boundary. Do not change the native-v2 minimum client version, write the retained store, convert on read, or invent a new migration. Test an ordinary old-shape config write with a populated sibling preference; testing only an old record with the new reader misses the data-loss direction.

## Shared host operations

Keep the distinction between **read for display**, **read for execution**, and **edit one explicit field** in `chaz-core`:

- **List/read:** Given a bound target and viewing agent, return declared field rows (key, display metadata, effective value and source, presence at the edit target, allowed operations). For Peer there is no session; for Session, label the viewed agent by identity. Return only declared values, not either backing document. A failed read is unavailable, not an absent preference. Sanitize display text, withhold raw diagnostics and do not instantiate a backend to populate a form.
- **Resolve for execution:** Use the same precedence function. Retain unknown/session-private members for the extension's own raw-state operations; overlay only declared keys for its effective read, in memory. Leave raw inspection and scoped runtime patches raw. In particular, `TurnCompleteContext::settings` for naming must see the current marker **and** effective preferences; `update_settings` still patches only the session fields supplied by the hook. Supply the resolved pruner field at the projection call. Do not pass an Agent DB handle to an endpoint or weaken revocation, projection or activation checks. With no declarations use the existing read path. A genuinely agentless context has no agent layer; failing to open an expected agent is an error, not an invitation to substitute another one.
- **Edit:** The input is `(target, extension, field, Set(typed_value) | Reset)`. For Peer bind the exact hosted Agent DB ID and its held key as for local agent configuration; there is no current-session fallback. For Session bind the open session ID and its session key. Resolve the viewed agent through checked authorized membership (`authorized_hosted_agents`), not the roster label. Selecting an agent for a preview never changes the edit target. Recheck identity, registration, field, allowed target, value and write authority on apply; database authorization is final. A session writer does not get an agent writer, and a remote room must not gain a new peer administration command. No write on a read-only, stale, ambiguous or failed target.
- **Write one key in a checked source-bound transaction.** Reuse the settings codec, structural delta and tombstone logic; do not save the form snapshot or replace the extension document. `Set(false)`, `Set(0)` and, for the model, `Set(null)` are explicit pins, even if equal to the inherited value. `Reset` removes only this layer's key; it does not set `null`, reset the extension, clear a retry marker or change another agent/session. A repeated set/reset needs no commit. Preserve unknown siblings and existing same-path conflict semantics. Refuse corrupt/unreadable documents and respect the existing refusal to edit retained settings before offline conversion.

TUI and command clients call the same typed mutation, not separate validators or a slash string assembled from display text. Route the existing session `/extensions set` through it for declared fields; add `/extensions reset <extension> <field>` for a declared session field. Keep `/extensions settings` as raw session inspection, and preserve the existing explicit, session-only raw setter for undeclared keys and descriptorless extensions (including the documented `attempted false` repair). The typed editor never falls back to that setter. Agent-target operations are local operator operations; do not teach session/bridge commands to choose arbitrary Agent DBs.

## Keyboard flow

Use the Session Hub, not a resource sidebar. From the hub, `s` opens Peer Settings even with zero conversations. On **Extensions** keep the current registration catalog and add `e Edit agent settings`: pick a hosted agent, then edit its defaults. From chat, `Ctrl+S` → **Extensions** (append after Sharing, leaving keys 1–6 intact) lists authorized agent views for that session. `Enter` on a visible agent opens the same field sheet, with **Session override** as its locked target. No editor for a missing target. Keep registered-but-inactive distinct from disabled fields: editing preferences never activates an extension.

Example field sheet, illustrating the session path rather than a screenshot:

```text
Session Settings / work / Extensions
Session overrides (all agents)    Viewing as: Alpha

  session_naming
>   Model                    demo:small          Agent: Alpha
    Automatic naming         off                 Session
  context_prune
    Minimum result bytes     16384               Extension default

Model used for naming. Use turn model to pin the turn's own model.
Up/Down select   Enter edit   Esc back
```

Only offer Reset where the selected layer has a value. `Enter` opens a typed edit prompt: select `On`/`Off`, enter a model ID or choose `Use turn model`, or enter decimal bytes. Confirm with `Enter`; `Esc` discards the draft. An invalid value leaves the prompt open with the constraint, not a success message. Confirm `r` with the target visible (e.g. `Remove session override for Automatic naming?`), then reread. Resetting a peer value instead says `Remove Alpha's default?` and falls back to the extension. After a save or reset, keep the same field selected and show its new source. A write failure leaves the choice unsaved; never show inheritance from an unreadable source.

```text
Peer Settings / Extensions
Agent defaults: Beta
> Model   (turn model)   Extension default
Enter edit    Esc back to agent picker
```

The scoped page keeps Category → List → Content focus and normal reader scrolling. The temporary field sheet owns its keys and pointer hits like a Settings picker: only visible, selectable fields get actions; headings do not. In compact mode keep scope, field and source reachable without the category rail. For example at 40×8:

```text
Session: work / Extensions
Session pin (all) · view: Alpha
> Automatic naming
  off · Session override

Enable on the next eligible turn.
Enter edit  r reset  Esc back
```

Below the supported floor swallow editing keys except safe cancellation. Approvals keep first claim on input and preserve drafts. `Esc` unwinds prompt → sheet → caller, returning through the agent picker when used. Reuse the Settings return target; don't change chat, draft, cursor, or scroll. Load snapshots off the render/input path, keyed by connected server generation, DB ID and tab incarnation where applicable. Retire stale loads/results on navigation or reconnect, and never retry a write against a different target.

## Behavior to verify

1. Start with two agents and two existing sessions. Set different naming models for the agents. Read back through the session editor and naming's runtime path without inference. Override one session's model with `null`, set its `enabled` to `false`, then change the first agent's choices. The untouched session follows the change; pinned fields do not. Reset only `model` on the shared session: each agent must now resolve its own model. Opening or importing the agent must not seed session pins.
2. Edit `context_prune.min_bytes` to zero and inspect the value received by the projector. Verify a negative or overflowing input fails in both the form and command paths. Check a reset exposes the agent value; an unrelated stored key survives. Exercise concurrent writes to different native fields and reject unauthorized or unconverted writes without changing history.
3. Exercise both Settings entry points with real key handling and rendered frames, including compact layout, cancel, validation failure, stale selection, and a read-only target. A descriptorless extension stays in the catalog with no edit control. Verify a runtime marker neither appears nor disappears when another field is saved. No model call, live account or full-interface redesign is required.

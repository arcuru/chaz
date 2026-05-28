# Skills

A **skill** is a named, reusable chunk of prompt content — instructions, conventions, or domain knowledge — that an agent can pull into its working context on demand. Skills are how you give chaz agents "I know how to do X" capabilities without baking everything into every agent's system prompt.

Chaz uses a **progressive-disclosure** model: every turn injects a short catalog of available skills (name + one-line description) into the system prompt; the LLM scans it, decides which skill is relevant, and calls the `skill_show` tool to load the full body. This keeps the system prompt small while making a large library reachable.

The skills extension also wraps **skill banks** — standalone synced databases of skills that can be shared between peers, similar to memory banks.

## The Four Sources

Skills are composed at turn time from four sources, in priority order. The first definition of a name wins; later sources don't override earlier ones.

| Order | Source                    | Backing                                                            | Lifetime / Scope                                  |
| ----- | ------------------------- | ------------------------------------------------------------------ | ------------------------------------------------- |
| 1     | **Disk** (project-local)  | `.chaz/skills/*.md` relative to cwd                                | Highest priority; intended for repo-scoped skills |
| 2     | **Disk** (user-global)    | `~/.config/chaz/skills/*.md`                                       | Available to every agent on this peer             |
| 3     | **Agent-private**         | `AgentDb.skills` (Table\<Skill\> on the agent's DB)                | Travels with the agent via eidetica sync          |
| 4     | **Granted skill bank**    | `SkillBankDb` granted to the agent via `/skills grant`             | Persistent grant; agent carries the ref in its DB |
| 5     | **Session-attached bank** | `SkillBankDb` attached to the current session via `/skills attach` | Transient; lasts only as long as the session      |

Disk paths are scanned at extension install time (process startup). DB-backed sources are read at every turn — write a new skill to a granted bank from another peer, sync settles, the next turn sees it.

## The SKILL.md format

Disk skills are Markdown files with YAML frontmatter. One skill per file. Max size 64 KiB.

```markdown
---
name: nix
description: Nix and NixOS package management, configuration, and troubleshooting
triggers:
  - nix
  - nixos
  - flake
---

# Nix skill

Guidelines for working with Nix on this machine:

- Use `nix develop .#` (the trailing `.#` is required because eidetica's flake also lives here).
- Prefer `home-manager switch` over manual `~/.config` edits.
- Never call `nix-shell` — it pulls in legacy channels.
```

Required frontmatter:

| Field         | Type          | Required | Notes                                                            |
| ------------- | ------------- | -------- | ---------------------------------------------------------------- |
| `name`        | `String`      | yes      | Unique identifier; how `skill_show` resolves the body            |
| `description` | `String`      | no       | One-liner shown in the in-prompt catalog and `skill_list` output |
| `triggers`    | `Vec<String>` | no       | Keyword hints, surfaced by `skill_list` (informational in v1)    |

The Markdown body after the closing `---` is the full instruction text loaded by `skill_show`.

Skills in DB-backed sources (`AgentDb.skills`, skill banks) carry the same `name` / `description` / `body` plus a `timestamp` and free-form `tags`. There's no markdown-frontmatter parsing step for those — they're written directly into the eidetica table.

## What the LLM sees

Every turn, the per-session `PromptAugmentation` from the `skills` extension appends a block like this to the system prompt:

```
## Available skills
Each line is `name — description`. To use a skill, call the `skill_show` tool with the skill's `name` to load its full instructions.

- **nix** — Nix and NixOS package management, configuration, and troubleshooting
- **postgres-debug** — Common postgres failure modes and triage steps
- **release-checklist** — Steps for cutting a chaz release
```

No bodies. No keyword scoring. The LLM picks based on the descriptions and calls `skill_show` to load whatever it wants.

### Tools

These are registered with the runtime so the LLM can invoke them like any other tool. All three are low-risk (read-only catalog access).

| Tool           | What it does                                                           |
| -------------- | ---------------------------------------------------------------------- |
| `skill_list`   | List loaded **disk** skills with name, description, and triggers       |
| `skill_search` | Substring-search disk skills by name, description, or trigger keyword  |
| `skill_show`   | Resolve a name against all four sources and return the full skill body |

`skill_list` and `skill_search` currently only see disk skills; `skill_show` walks all four sources in the priority order above. (This asymmetry is a known v1 limitation — bodies are reachable from anywhere; the listing tools haven't been unified yet.)

## The `/skills` commands

Every transport uses the same surface. TUI: `/skills <sub>`. Matrix: `!chaz skills <sub>`.

`/skills` manages **skill banks** — the eidetica-backed source kind (#4 and #5 in the four-sources table). Disk skills (#1, #2) are filesystem-only; no command authors them. Agent-private skills (#3) are written directly to the agent DB in v1 (no slash surface yet).

| Command                                        | What                                                                                                                       |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `/skills` or `/skills list`                    | List skill banks this peer hosts                                                                                           |
| `/skills new <name> [description]`             | Create a new skill bank on this peer                                                                                       |
| `/skills delete <bank>`                        | Unregister locally (the DB is preserved for archive)                                                                       |
| `/skills grant <bank> <agent> <read\|write>`   | Authorize an agent's key on the bank and write a `SkillBankRef` into the agent's DB so the bank appears in its source list |
| `/skills revoke <bank> <agent>`                | Reverse `grant` — removes the auth key and the agent's ref                                                                 |
| `/skills share <bank>`                         | Generate a `DatabaseTicket` URL for the bank so another peer can `/skills import` it                                       |
| `/skills unshare <bank>`                       | Stop sharing — disables sync for the bank on this peer; doesn't revoke keys already held                                   |
| `/skills import <ticket> [admin\|write\|read]` | Request access to a shared bank via the bootstrap workflow. Default `write`. Returns the new bank name on success.         |
| `/skills attach <bank\|db_id\|ticket>`         | Attach a bank to the current session (transient, source #5). If passed a ticket, imports it first.                         |
| `/skills detach <bank>`                        | Detach a bank from the current session                                                                                     |

Permission tokens for `grant`: `read` lets the agent see and load the bank's skills; `write` additionally lets it add or edit them (writes still pending a tool — see below).

## What's not in v1 yet

- **No `/skills add` slash command** for writing skills _into_ a bank. The bank machinery and grants are wired end-to-end, but adding new skill rows requires direct eidetica writes from code (or a future tool). Disk SKILL.md files are the only end-user authoring path in v1.
- **No `skill_install` / `skill_remove`** registry-style tools. Deferred along with the broader skill registry concept (see [`design/skills_and_prompts.md`](../design/skills_and_prompts.md)).
- **No trust-tier ceiling.** The design called for `installed`-tier skills to drop the turn's tool ceiling to read-only; v1 has only `trusted` (operator-placed) skills, so the ceiling logic is a no-op.
- **No live reload of disk skills.** Disk paths are scanned at extension install (process startup). Add or edit a SKILL.md → restart chaz to pick it up.

## End-to-end walkthroughs

### Walkthrough 1: a disk skill

The fast path for adding a skill to a single peer. Two steps: drop a file, restart.

1. **Write a SKILL.md.** User-global location:

   ```bash
   mkdir -p ~/.config/chaz/skills
   cat > ~/.config/chaz/skills/release.md <<'EOF'
   ---
   name: release
   description: Cutting a chaz release — bump version, tag, push.
   triggers: [release, tag, bump]
   ---

   ## Cutting a chaz release

   1. Bump `version` in `Cargo.toml`.
   2. Run `just ci` — must be green.
   3. `git tag v$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)`.
   4. `git push --tags`.
   EOF
   ```

2. **Restart chaz.** On startup you'll see:

   ```text
   INFO Skills loaded count=1
   INFO Skills extension installed count=1
   ```

3. **Send a message.** The catalog appears in the next turn's system prompt:

   ```
   ## Available skills
   - **release** — Cutting a chaz release — bump version, tag, push.
   ```

4. **The LLM picks it up.** When you ask "how do I cut a release?", the agent calls `skill_show` with `name: "release"` and gets the full body back to act on.

### Walkthrough 2: a shared skill bank across peers

The collaborative path: one peer owns the bank, others sync it and grant it to their agents. Two TUIs against separate state dirs.

1. **Peer A creates the bank:**

   ```text
   > /skills new ops "Runbooks for the prod stack"
   Created skill bank 'ops' (DB sha256:abc…). Grant it to an agent with /skills grant.
   ```

2. **Peer A grants their local agent:**

   ```text
   > /skills grant ops sre-agent write
   Granted agent 'sre-agent' Write access to skill bank 'ops'
   ```

   _(v1 caveat: this gives the agent permission to write, but there's no slash command to actually add skills into the bank yet. For now, the bank is useful as a sharing container; populate it from code or via direct eidetica writes until the authoring tool lands.)_

3. **Peer A shares it:**

   ```text
   > /skills share ops
   Share this ticket to sync skill bank 'ops' (DB sha256:abc…):

   eidetica:?db=sha256:abc…&pr=…
   ```

4. **Peer B imports:**

   ```text
   > /skills import eidetica:?db=sha256:abc…&pr=… write
   Imported skill bank 'ops' (DB sha256:abc…). Grant it to agents with /skills grant ops <agent> <read|write>.
   ```

   (If Peer A didn't preseed Peer B's pubkey, this prints a bootstrap-request ID; Peer A runs `/sharing approve <id>` and Peer B re-runs the import — same shape as `/agent import` / `/memory import`.)

5. **Peer B grants their own agent:**

   ```text
   > /skills grant ops oncall-agent read
   Granted agent 'oncall-agent' Read access to skill bank 'ops'
   ```

6. **From either side**, the `ops` bank now appears in the agent's source list at every turn. Any skill row in the bank (once authoring exists or is written from code) flows into the catalog injected into the system prompt for both agents.

### Walkthrough 3: transient session attach

When you want a bank's skills for just this conversation, without persisting a grant on the agent's DB:

```text
> /skills attach ops
Attached skill bank 'ops' to this session. Its skills will be surfaced in context.

> /skills detach ops
Detached skill bank 'ops' from this session.
```

`/skills attach` also accepts a ticket directly — it'll import the bank (creating it locally) and then attach in one step. Useful for ad-hoc spectating.

## Configuration

Auto-attach skill banks at agent bootstrap:

```yaml
agents:
  - name: sre-agent
    system_prompt: "You triage prod issues."
    default_skill_banks:
      - ops
      - postgres-runbook
```

Each named bank must already exist on this peer (created with `/skills new` or imported). Missing names are logged at WARN and skipped — a typo in config doesn't fail startup.

## See also

- [Memory](./memory.md) — the same shape (per-agent stash + standalone shareable banks + grants) applied to factual recall instead of prompt fragments.
- [Agents → System Prompts](./agents.md#system-prompts) — how a skill's body interacts with the agent's base prompt.
- [Extensions](./extensions.md) — the `skills` extension's place in the broader extension framework.
- [`design/skills_and_prompts.md`](../design/skills_and_prompts.md) — design rationale, the four-source composition decision, and what was deferred from v1.

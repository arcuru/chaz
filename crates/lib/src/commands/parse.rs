//! Transport-neutral slash-command parsing.
//!
//! Every frontend (TUI, Matrix, future HTTP/Discord/…) recognizes the same
//! `/command` vocabulary and builds the same [`Command`]. Historically each
//! gateway re-encoded that vocabulary: the TUI in `parse_chat_line`, Matrix in
//! ~20 `register_text_command` closures. [`parse`] is the single source of that
//! grammar.
//!
//! ## What lives here vs. in the view
//!
//! `parse` owns only the **shared** vocabulary — the verbs that map to a
//! [`Command`] and dispatch identically everywhere. It is pure: no `Server`, no
//! eidetica, no transport. Bad arguments come back as [`Parsed::Usage`] (a
//! string the view renders however it likes) rather than a side-effecting
//! error.
//!
//! **View-local commands stay in the view.** A frontend checks its own command
//! table *first*, then falls through to `parse`. This is how the TUI keeps
//! `/sessions` → open an interactive picker, `/settings`, `/clear`, `/debug`,
//! etc. — verbs that either have no [`Command`] equivalent or mean something
//! richer in that particular view. Where a view-local verb collides with a
//! shared one (e.g. `sessions`: TUI picker vs. core [`Command::ListSessions`]),
//! the view's first-look table shadows the shared mapping for that view only.
//!
//! So the contract for a frontend is:
//! ```ignore
//! if let Some(action) = view.local(input) { return action; } // per-view
//! match commands::parse(input) {
//!     Parsed::Command(cmd) => dispatch(cmd, ctx).await,       // shared
//!     Parsed::Usage(msg)   => show(msg),
//!     Parsed::NotCommand   => session.write(input),           // plain text
//! }
//! ```

use super::{
    CoOwnerPermission, Command, ExtensionsAction, RehostScope, parse_permission_token,
    split_ext_scope,
};

/// Result of parsing one input line against the shared command grammar.
pub enum Parsed {
    /// A recognized built-in command, or an unknown `/foo args` routed to the
    /// extension dispatcher as [`Command::Extension`]. Hand it to `dispatch`.
    Command(Command),
    /// A recognized command verb with missing/invalid arguments. The payload is
    /// a usage hint; the view renders it (TUI status line, Matrix notice, …).
    Usage(String),
    /// No leading `/` — not a command. The caller treats the line as a plain
    /// message to write to the session.
    NotCommand,
}

/// Parse one line of user input against the shared `/command` grammar.
///
/// `input` is the raw line for a `/`-sigil frontend (the TUI passes `/compact`,
/// `/agent add foo`, …). Frontends that use a different sigil (Matrix's
/// `!chaz `) normalize to a leading `/` before calling.
///
/// Pure and synchronous — safe to unit-test without a runtime.
pub fn parse(input: &str) -> Parsed {
    // No normalization of `input` itself: the argument-bearing arms below match
    // a literal trailing space (`strip_prefix("/agent add ")`), and per-arg
    // `.trim()` handles the rest. This preserves the exact pre-extraction
    // behavior of the frontends — e.g. `/compact ` (trailing space) falls
    // through to the extension lookup, just as it did before.
    let text = input;

    // Exact-match verbs with no arguments. View-local verbs (`/models`,
    // `/settings`, `/clear`, `/debug`, `/expand`, `/raw`, `/help`, `/?`) are
    // deliberately absent — a frontend intercepts those before calling here.
    match text {
        "/quit" | "/exit" | "/q" => return Parsed::Command(Command::Quit),
        "/sessions" | "/s" => return Parsed::Command(Command::ListSessions),
        "/share" => return Parsed::Command(Command::Share),
        "/unshare" => return Parsed::Command(Command::SessionUnshare),
        "/compact" => return Parsed::Command(Command::Compact),
        "/info" => return Parsed::Command(Command::Info),
        "/costs" => return Parsed::Command(Command::ListCosts),
        "/print" => return Parsed::Command(Command::Print),
        "/backends" => return Parsed::Command(Command::ListBackends),
        "/new" => return Parsed::Command(Command::NewSession),
        // No-arg `/name` (and `/rename`) clears the alias.
        "/name" | "/rename" => return Parsed::Command(Command::ClearSessionName),
        "/role" => return Parsed::Command(Command::Role(None)),
        "/model" => return Parsed::Command(Command::Model(None)),
        "/channels" => return Parsed::Command(Command::ListChannels),
        "/agents" => return Parsed::Command(Command::AgentsList),
        "/pubkey" => return Parsed::Command(Command::Pubkey),
        _ => {}
    }

    // --- Living Agents: per-session participation + lifecycle ---
    if let Some(arg) = text.strip_prefix("/agent add ") {
        let r = arg.trim();
        return if r.is_empty() {
            Parsed::Usage("Usage: /agent add <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::AgentAdd(r.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/agent remove ") {
        let r = arg.trim();
        return if r.is_empty() {
            Parsed::Usage("Usage: /agent remove <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::AgentRemove(r.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/agent new ") {
        let mut parts = arg.split_whitespace();
        let name = match parts.next() {
            Some(n) => n.to_string(),
            None => return Parsed::Usage("Usage: /agent new <name> [k=v...]".to_string()),
        };
        let mut overrides: Vec<(String, String)> = Vec::new();
        for tok in parts {
            match tok.split_once('=') {
                Some((k, v)) if !k.is_empty() => overrides.push((k.to_string(), v.to_string())),
                _ => {
                    return Parsed::Usage(format!(
                        "Invalid /agent new override '{tok}' — use key=value"
                    ));
                }
            }
        }
        return Parsed::Command(Command::AgentNew { name, overrides });
    }
    if let Some(arg) = text.strip_prefix("/agent share ") {
        let r = arg.trim();
        return if r.is_empty() {
            Parsed::Usage("Usage: /agent share <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::AgentShare(r.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/agent unshare ") {
        let r = arg.trim();
        return if r.is_empty() {
            Parsed::Usage("Usage: /agent unshare <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::AgentUnshare(r.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/agent import ") {
        let mut parts = arg.trim().splitn(2, char::is_whitespace);
        let ticket = parts.next().unwrap_or("").trim().to_string();
        let perm_tok = parts.next().unwrap_or("").trim();
        if ticket.is_empty() {
            return Parsed::Usage("Usage: /agent import <ticket> [admin|write|read]".to_string());
        }
        // `/agent import` defaults to Write (co-ownership with edit rights);
        // Admin and Read are explicit opt-ins.
        let permission = match perm_tok {
            "" => CoOwnerPermission::Write,
            other => match parse_permission_token(other) {
                Some(p) => p,
                None => {
                    return Parsed::Usage(format!(
                        "Unknown permission '{other}' — use admin, write, or read (default: write)"
                    ));
                }
            },
        };
        return Parsed::Command(Command::AgentImport { ticket, permission });
    }
    if let Some(arg) = text.strip_prefix("/agent delete ") {
        let r = arg.trim();
        return if r.is_empty() {
            Parsed::Usage("Usage: /agent delete <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::AgentDelete(r.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/agent invite ") {
        let mut parts = arg.trim().splitn(3, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let pubkey = parts.next().unwrap_or("").trim();
        let perm_tok = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || pubkey.is_empty() {
            return Parsed::Usage(
                "Usage: /agent invite <ref> <pubkey> [admin|write|read]".to_string(),
            );
        }
        // `/agent invite` defaults to Admin.
        let permission = match parse_permission_token(perm_tok) {
            Some(p) => p,
            None => {
                return Parsed::Usage(format!(
                    "Unknown permission '{perm_tok}' — use admin, write, or read (default: admin)"
                ));
            }
        };
        return Parsed::Command(Command::AgentInvite {
            agent_ref: agent_ref.to_string(),
            pubkey: pubkey.to_string(),
            permission,
        });
    }
    if let Some(arg) = text.strip_prefix("/agent revoke-peer ") {
        let mut parts = arg.trim().splitn(2, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let pubkey = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || pubkey.is_empty() {
            return Parsed::Usage("Usage: /agent revoke-peer <ref> <pubkey>".to_string());
        }
        return Parsed::Command(Command::AgentRevokePeer {
            agent_ref: agent_ref.to_string(),
            pubkey: pubkey.to_string(),
        });
    }
    if let Some(arg) = text.strip_prefix("/agent home-status") {
        let trimmed = arg.trim();
        let agent_ref = (!trimmed.is_empty()).then(|| trimmed.to_string());
        return Parsed::Command(Command::AgentHomeStatus(agent_ref));
    }
    if let Some(arg) = text.strip_prefix("/agent rehost ") {
        // Flags (--agent, --clear) in any order, then `<ref> [pubkey]`.
        let mut scope = RehostScope::Session;
        let mut clear = false;
        let mut positional: Vec<&str> = Vec::new();
        for tok in arg.split_whitespace() {
            match tok {
                "--agent" => scope = RehostScope::Agent,
                "--clear" => clear = true,
                _ => positional.push(tok),
            }
        }
        let agent_ref = positional.first().copied().unwrap_or("").trim();
        let pubkey = positional.get(1).copied().map(str::to_string);
        if agent_ref.is_empty() {
            return Parsed::Usage(
                "Usage: /agent rehost [--agent] [--clear] <ref> [pubkey]".to_string(),
            );
        }
        if clear && pubkey.is_some() {
            return Parsed::Usage(
                "/agent rehost: --clear cannot be combined with an explicit pubkey".to_string(),
            );
        }
        return Parsed::Command(Command::AgentRehost {
            agent_ref: agent_ref.to_string(),
            pubkey,
            scope,
            clear,
        });
    }
    if let Some(arg) = text.strip_prefix("/agent set ") {
        let mut parts = arg.trim().splitn(3, char::is_whitespace);
        let agent_ref = parts.next().unwrap_or("").trim();
        let field = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if agent_ref.is_empty() || field.is_empty() || value.is_empty() {
            return Parsed::Usage("Usage: /agent set <name|db_id> <field> <value>".to_string());
        }
        return Parsed::Command(Command::AgentSet {
            agent_ref: agent_ref.to_string(),
            field: field.to_string(),
            value: value.to_string(),
        });
    }
    if text == "/agent reload" {
        return Parsed::Command(Command::AgentReload(None));
    }
    if let Some(arg) = text.strip_prefix("/agent reload ") {
        let r = arg.trim();
        return Parsed::Command(Command::AgentReload((!r.is_empty()).then(|| r.to_string())));
    }
    if text == "/agent hosted" {
        return Parsed::Command(Command::AgentHosted);
    }
    if let Some(arg) = text.strip_prefix("/agent host ") {
        let r = arg.trim();
        return Parsed::Command(Command::AgentSetHost(
            (!r.is_empty()).then(|| r.to_string()),
        ));
    }
    if text == "/agent host" {
        return Parsed::Command(Command::AgentSetHost(None));
    }
    if let Some(arg) = text.strip_prefix("/agent burst ") {
        let s = arg.trim();
        return match s.parse::<usize>() {
            Ok(n) if n >= 1 => Parsed::Command(Command::AgentSetBurst(Some(n))),
            Ok(_) => Parsed::Usage("Burst budget must be ≥ 1".to_string()),
            Err(_) => Parsed::Usage(format!(
                "Invalid burst budget '{s}' — expected a positive integer"
            )),
        };
    }
    if text == "/agent burst" {
        return Parsed::Command(Command::AgentSetBurst(None));
    }
    if text == "/agent" || text == "/agent list" {
        return Parsed::Command(Command::AgentsList);
    }
    if text == "/agent room" {
        return Parsed::Command(Command::AgentRoom);
    }

    // --- Bootstrap-queue surface ---
    if text == "/sharing" || text == "/sharing status" {
        return Parsed::Command(Command::SharingStatus);
    }
    if text == "/sharing requests" || text == "/sharing list" {
        return Parsed::Command(Command::SharingRequests);
    }
    if let Some(arg) = text.strip_prefix("/sharing approve ") {
        let id = arg.trim();
        return if id.is_empty() {
            Parsed::Usage("Usage: /sharing approve <request_id>".to_string())
        } else {
            Parsed::Command(Command::SharingApprove(id.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/sharing reject ") {
        let id = arg.trim();
        return if id.is_empty() {
            Parsed::Usage("Usage: /sharing reject <request_id>".to_string())
        } else {
            Parsed::Command(Command::SharingReject(id.to_string()))
        };
    }

    // --- /extensions: per-session framework control ---
    if text == "/extensions" || text == "/extensions list" {
        return Parsed::Command(Command::Extensions(ExtensionsAction::List));
    }
    if let Some(arg) = text.strip_prefix("/extensions add ") {
        let (name, scope) = split_ext_scope(arg);
        return if name.is_empty() {
            Parsed::Usage("Usage: /extensions add <name> [agent]".to_string())
        } else {
            Parsed::Command(Command::Extensions(ExtensionsAction::Add(name, scope)))
        };
    }
    if let Some(arg) = text.strip_prefix("/extensions remove ") {
        let (name, scope) = split_ext_scope(arg);
        return if name.is_empty() {
            Parsed::Usage("Usage: /extensions remove <name> [agent]".to_string())
        } else {
            Parsed::Command(Command::Extensions(ExtensionsAction::Remove(name, scope)))
        };
    }
    if let Some(arg) = text.strip_prefix("/extensions settings ") {
        let name = arg.trim();
        return if name.is_empty() {
            Parsed::Usage("Usage: /extensions settings <name>".to_string())
        } else {
            Parsed::Command(Command::Extensions(ExtensionsAction::Settings(
                name.to_string(),
            )))
        };
    }
    if let Some(arg) = text.strip_prefix("/extensions set ") {
        let mut parts = arg.trim().splitn(3, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim();
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if name.is_empty() || key.is_empty() || value.is_empty() {
            return Parsed::Usage("Usage: /extensions set <name> <key> <value>".to_string());
        }
        return Parsed::Command(Command::Extensions(ExtensionsAction::Set {
            name: name.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        }));
    }

    // --- Session ops with arguments ---
    if let Some(arg) = text.strip_prefix("/join ") {
        let id = arg.trim();
        return if id.is_empty() {
            Parsed::Usage("Usage: /join <name|db_id>".to_string())
        } else {
            Parsed::Command(Command::SwitchSession(id.to_string()))
        };
    }
    if let Some(arg) = text
        .strip_prefix("/name ")
        .or_else(|| text.strip_prefix("/rename "))
    {
        let name = arg.trim();
        // Trailing-whitespace-only arg falls back to the clear semantics of
        // the no-arg form rather than erroring.
        return if name.is_empty() {
            Parsed::Command(Command::ClearSessionName)
        } else {
            Parsed::Command(Command::NameSession(name.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/sync ") {
        let ticket = arg.trim();
        return if ticket.is_empty() {
            Parsed::Usage("Usage: /sync <ticket>".to_string())
        } else {
            Parsed::Command(Command::Sync(ticket.to_string()))
        };
    }
    if let Some(arg) = text.strip_prefix("/model ") {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            // `/model ` with no id behaves like bare `/model` (show/clear).
            return Parsed::Command(Command::Model(None));
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        return match tokens.as_slice() {
            // Single token — session-wide pin.
            [id] => Parsed::Command(Command::Model(Some((*id).to_string()))),
            // Two tokens — per-agent override; `clear` wipes it.
            [agent, second] => {
                let model = (!second.eq_ignore_ascii_case("clear")).then(|| (*second).to_string());
                Parsed::Command(Command::AgentModel {
                    agent: (*agent).to_string(),
                    model,
                })
            }
            _ => Parsed::Usage("Usage: /model [<id> | <agent> <id> | <agent> clear]".to_string()),
        };
    }
    if let Some(arg) = text.strip_prefix("/role ") {
        let mut parts = arg.trim().splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim().to_string();
        let prompt = parts.next().map(|s| s.trim().to_string());
        return if name.is_empty() {
            Parsed::Command(Command::Role(None))
        } else {
            Parsed::Command(Command::Role(Some((name, prompt))))
        };
    }
    if let Some(arg) = text.strip_prefix("/backend ") {
        let mut parts = arg.split_whitespace();
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(name), Some(url), Some(key)) => Parsed::Command(Command::SetBackend {
                name: name.to_string(),
                url: url.to_string(),
                api_key: key.to_string(),
            }),
            _ => Parsed::Usage("Usage: /backend <name> <api_base> <api_key>".to_string()),
        };
    }

    // Unknown `/foo [args]` → extension command dispatch. `dispatch` returns a
    // `CommandOutcome::Error` if no extension registered the name.
    if let Some(stripped) = text.strip_prefix('/') {
        let (name, args) = match stripped.split_once(char::is_whitespace) {
            Some((n, a)) => (n.to_string(), a.trim().to_string()),
            None => (stripped.to_string(), String::new()),
        };
        return if name.is_empty() {
            Parsed::Usage("Empty command".to_string())
        } else {
            Parsed::Command(Command::Extension { name, args })
        };
    }

    Parsed::NotCommand
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert a line parses to a `Command` and hand it to a matcher.
    fn cmd(input: &str) -> Command {
        match parse(input) {
            Parsed::Command(c) => c,
            Parsed::Usage(u) => panic!("expected Command for {input:?}, got Usage({u:?})"),
            Parsed::NotCommand => panic!("expected Command for {input:?}, got NotCommand"),
        }
    }

    fn usage(input: &str) -> String {
        match parse(input) {
            Parsed::Usage(u) => u,
            other => panic!(
                "expected Usage for {input:?}, got {}",
                match other {
                    Parsed::Command(_) => "Command",
                    Parsed::NotCommand => "NotCommand",
                    Parsed::Usage(_) => unreachable!(),
                }
            ),
        }
    }

    #[test]
    fn plain_text_is_not_a_command() {
        assert!(matches!(parse("hello there"), Parsed::NotCommand));
        assert!(matches!(parse("what is 2 + 2?"), Parsed::NotCommand));
        // A leading slash mid-sentence does not count — only a leading sigil.
        assert!(matches!(parse("use the /compact idea"), Parsed::NotCommand));
    }

    #[test]
    fn simple_verbs() {
        assert!(matches!(cmd("/compact"), Command::Compact));
        assert!(matches!(cmd("/info"), Command::Info));
        assert!(matches!(cmd("/quit"), Command::Quit));
        assert!(matches!(cmd("/exit"), Command::Quit));
        assert!(matches!(cmd("/q"), Command::Quit));
        assert!(matches!(cmd("/sessions"), Command::ListSessions));
        assert!(matches!(cmd("/s"), Command::ListSessions));
        assert!(matches!(cmd("/print"), Command::Print));
        assert!(matches!(cmd("/costs"), Command::ListCosts));
        assert!(matches!(cmd("/pubkey"), Command::Pubkey));
    }

    #[test]
    fn name_set_and_clear() {
        assert!(matches!(cmd("/name"), Command::ClearSessionName));
        assert!(matches!(cmd("/rename"), Command::ClearSessionName));
        // Trailing-space-only arg clears, matching the no-arg form.
        assert!(matches!(cmd("/name   "), Command::ClearSessionName));
        match cmd("/name my session") {
            Command::NameSession(n) => assert_eq!(n, "my session"),
            other => panic!("expected NameSession, got {other:?}"),
        }
        match cmd("/rename foo") {
            Command::NameSession(n) => assert_eq!(n, "foo"),
            other => panic!("expected NameSession, got {other:?}"),
        }
    }

    #[test]
    fn model_pin_and_per_agent_override() {
        assert!(matches!(cmd("/model"), Command::Model(None)));
        assert!(matches!(cmd("/model "), Command::Model(None)));
        match cmd("/model deepseek/deepseek-v4-pro") {
            Command::Model(Some(id)) => assert_eq!(id, "deepseek/deepseek-v4-pro"),
            other => panic!("expected Model(Some), got {other:?}"),
        }
        match cmd("/model ava gpt-4o") {
            Command::AgentModel { agent, model } => {
                assert_eq!(agent, "ava");
                assert_eq!(model.as_deref(), Some("gpt-4o"));
            }
            other => panic!("expected AgentModel, got {other:?}"),
        }
        // `clear` (any case) as the second token wipes the override.
        match cmd("/model ava CLEAR") {
            Command::AgentModel { agent, model } => {
                assert_eq!(agent, "ava");
                assert!(model.is_none());
            }
            other => panic!("expected AgentModel clear, got {other:?}"),
        }
        assert!(usage("/model a b c").starts_with("Usage: /model"));
    }

    #[test]
    fn agent_subcommands() {
        match cmd("/agent add researcher") {
            Command::AgentAdd(r) => assert_eq!(r, "researcher"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(cmd("/agent"), Command::AgentsList));
        assert!(matches!(cmd("/agent list"), Command::AgentsList));
        assert!(matches!(cmd("/agent room"), Command::AgentRoom));
        assert!(matches!(cmd("/agent hosted"), Command::AgentHosted));
        assert!(matches!(cmd("/agent host"), Command::AgentSetHost(None)));
        match cmd("/agent host ava") {
            Command::AgentSetHost(Some(r)) => assert_eq!(r, "ava"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(cmd("/agent reload"), Command::AgentReload(None)));
        match cmd("/agent reload ava") {
            Command::AgentReload(Some(r)) => assert_eq!(r, "ava"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(cmd("/agent burst"), Command::AgentSetBurst(None)));
        assert!(matches!(
            cmd("/agent burst 3"),
            Command::AgentSetBurst(Some(3))
        ));
        assert!(usage("/agent burst 0").contains("≥ 1"));
        assert!(usage("/agent burst nope").contains("positive integer"));
        // `/agent add` (no trailing arg) misses the `add ` arm and falls to the
        // extension fallback — only `/agent add ` (with a space) yields a usage.
        assert!(usage("/agent add ").starts_with("Usage: /agent add"));
    }

    #[test]
    fn agent_new_overrides() {
        match cmd("/agent new scribe role=writer model=gpt-4o") {
            Command::AgentNew { name, overrides } => {
                assert_eq!(name, "scribe");
                assert_eq!(
                    overrides,
                    vec![
                        ("role".to_string(), "writer".to_string()),
                        ("model".to_string(), "gpt-4o".to_string()),
                    ]
                );
            }
            other => panic!("got {other:?}"),
        }
        assert!(usage("/agent new scribe role").contains("key=value"));
    }

    #[test]
    fn agent_rehost_flags() {
        match cmd("/agent rehost --agent ava") {
            Command::AgentRehost {
                agent_ref,
                pubkey,
                scope,
                clear,
            } => {
                assert_eq!(agent_ref, "ava");
                assert!(pubkey.is_none());
                assert_eq!(scope, RehostScope::Agent);
                assert!(!clear);
            }
            other => panic!("got {other:?}"),
        }
        assert!(usage("/agent rehost --clear ava deadbeef").contains("--clear cannot"));
        assert!(usage("/agent rehost --agent").starts_with("Usage:"));
    }

    #[test]
    fn agent_invite_and_import_permission_defaults() {
        // invite defaults to Admin
        match cmd("/agent invite ava deadbeef") {
            Command::AgentInvite { permission, .. } => {
                assert_eq!(permission, CoOwnerPermission::Admin)
            }
            other => panic!("got {other:?}"),
        }
        // import defaults to Write
        match cmd("/agent import ticket://x") {
            Command::AgentImport { permission, .. } => {
                assert_eq!(permission, CoOwnerPermission::Write)
            }
            other => panic!("got {other:?}"),
        }
        match cmd("/agent import ticket://x read") {
            Command::AgentImport { permission, .. } => {
                assert_eq!(permission, CoOwnerPermission::Read)
            }
            other => panic!("got {other:?}"),
        }
        assert!(usage("/agent invite ava").contains("pubkey"));
        assert!(usage("/agent import ticket://x bogus").contains("Unknown permission"));
    }

    #[test]
    fn sharing_and_extensions() {
        assert!(matches!(cmd("/sharing"), Command::SharingStatus));
        assert!(matches!(cmd("/sharing status"), Command::SharingStatus));
        assert!(matches!(cmd("/sharing requests"), Command::SharingRequests));
        assert!(matches!(cmd("/sharing list"), Command::SharingRequests));
        match cmd("/sharing approve req-1") {
            Command::SharingApprove(id) => assert_eq!(id, "req-1"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            cmd("/extensions"),
            Command::Extensions(ExtensionsAction::List)
        ));
        assert!(matches!(
            cmd("/extensions list"),
            Command::Extensions(ExtensionsAction::List)
        ));
        assert!(usage("/extensions add ").starts_with("Usage: /extensions add"));
    }

    #[test]
    fn unknown_slash_routes_to_extension() {
        match cmd("/memory list") {
            Command::Extension { name, args } => {
                assert_eq!(name, "memory");
                assert_eq!(args, "list");
            }
            other => panic!("got {other:?}"),
        }
        match cmd("/weather") {
            Command::Extension { name, args } => {
                assert_eq!(name, "weather");
                assert_eq!(args, "");
            }
            other => panic!("got {other:?}"),
        }
        assert_eq!(usage("/"), "Empty command");
    }

    #[test]
    fn backend_and_join_and_sync() {
        match cmd("/backend openrouter https://x.ai/v1 sk-123") {
            Command::SetBackend { name, url, api_key } => {
                assert_eq!(name, "openrouter");
                assert_eq!(url, "https://x.ai/v1");
                assert_eq!(api_key, "sk-123");
            }
            other => panic!("got {other:?}"),
        }
        assert!(usage("/backend openrouter").starts_with("Usage: /backend"));
        match cmd("/join sess-7") {
            Command::SwitchSession(id) => assert_eq!(id, "sess-7"),
            other => panic!("got {other:?}"),
        }
        assert!(
            usage("/sync ").starts_with("Usage: /sync")
                || matches!(parse("/sync "), Parsed::Usage(_))
        );
        match cmd("/sync ticket://abc") {
            Command::Sync(t) => assert_eq!(t, "ticket://abc"),
            other => panic!("got {other:?}"),
        }
    }
}

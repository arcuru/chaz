//! `chaz-matrix rooms` — list or leave every joined Matrix room for a bridge
//! login.
//!
//! A maintenance path, not part of the bridge's steady-state runtime. It signs
//! in with a fresh throwaway device — never the bridge's own persisted session
//! file or device — lists the account's joined rooms, and in `--execute` mode
//! leaves them one by one before logging the throwaway device back out. The
//! bridge's own session is untouched.
//!
//! Plain `leave-all` is only a *membership* reset: pending invitations survive
//! it and left rooms keep their history. `--reset` upgrades an execute run (or
//! its dry-run preview) to a true reset — it also enumerates and rejects every
//! pending invite (leaving an invited-but-not-joined room rejects the invite
//! per spec) and forgets each room after leaving it, which drops the account's
//! history access. The bridge's invite auto-joiner will rejoin any invite it
//! sees, so the bridge must be stopped for the duration of a reset run.
//!
//! This path must stay crypto-free. The throwaway login shares the bridge's
//! E2EE-capable client builder, but its crypto store lives in the throwaway
//! state dir and nothing here ever publishes key material: requests go through
//! raw `Client::send` rather than an SDK sync (which would upload device and
//! one-time keys), and cross-signing is never bootstrapped. That is what keeps
//! the ephemeral device invisible to other clients' device lists. Do not add an
//! SDK sync, key upload, or cross-signing here.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::bridge::{Login, MatrixClient};
use crate::config::{MatrixBridgeConfig, MatrixLoginConfig};
use crate::credentials::MatrixCredentials;

use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::api::client::membership::forget_room::v3 as forget_room;
use matrix_sdk::ruma::api::client::membership::joined_rooms::v3 as joined_rooms;
use matrix_sdk::ruma::api::client::membership::leave_room::v3 as leave_room;
use matrix_sdk::ruma::api::client::sync::sync_events::v3 as sync_events;
use matrix_sdk::ruma::presence::PresenceState;
use tokio::time::sleep;

/// Whether `rooms` only reports or actually changes membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// List joined rooms (and, with `reset`, pending invites) and mutate
    /// nothing.
    DryRun,
    /// Leave every joined room — and, with `reset`, reject every pending
    /// invite and forget each room afterwards.
    Execute,
}

/// Tuning for one reset run. Durations are injectable so the decision core can
/// be exercised with zero-length sleeps in tests.
#[derive(Debug, Clone, Copy)]
struct EngineConfig {
    mode: Mode,
    /// True-reset scope: reject pending invites and forget rooms after
    /// leaving them, on top of the plain leave-all behavior.
    reset: bool,
    /// Leave attempts per room before giving up.
    retries: u32,
    /// Base backoff between leave attempts; doubles each attempt (1s, 2s, 4s…).
    retry_backoff: Duration,
    /// Delay between rooms.
    inter_room_delay: Duration,
}

/// The Matrix calls a reset needs, split from the decision/reporting core so
/// the latter is testable without a homeserver. Room ids are plain strings to
/// keep fakes trivial; the real implementation converts to and from ruma types.
trait RoomEndpoint {
    async fn joined_rooms(&mut self) -> anyhow::Result<Vec<String>>;
    /// Rooms the account has been invited to but not joined. Rejecting an
    /// invite is `leave_room` on one of these, per spec.
    async fn invited_rooms(&mut self) -> anyhow::Result<Vec<String>>;
    async fn leave_room(&mut self, room_id: &str) -> anyhow::Result<()>;
    /// Drop the account's history access to an already-left room. Valid only
    /// after the room has been left (or its invite rejected).
    async fn forget_room(&mut self, room_id: &str) -> anyhow::Result<()>;
}

/// The real endpoint, backed by a logged-in [`matrix_sdk::Client`].
struct ClientEndpoint {
    client: matrix_sdk::Client,
}

impl RoomEndpoint for ClientEndpoint {
    async fn joined_rooms(&mut self) -> anyhow::Result<Vec<String>> {
        let resp = self.client.send(joined_rooms::Request::new()).await?;
        Ok(resp
            .joined_rooms
            .into_iter()
            .map(|id| id.to_string())
            .collect())
    }

    async fn invited_rooms(&mut self) -> anyhow::Result<Vec<String>> {
        // There is no dedicated "list invites" endpoint; an initial /sync with
        // a zero-length timeline is the cheapest read that surfaces the
        // invite section. Presence stays offline so a maintenance run — taken
        // while the bridge is stopped — does not flip the account online.
        let mut filter = FilterDefinition::default();
        filter.room.timeline.limit = Some(0u32.into());
        let mut request = sync_events::Request::new();
        request.filter = Some(filter.into());
        request.set_presence = PresenceState::Offline;
        let response = self.client.send(request).await?;
        Ok(response
            .rooms
            .invite
            .into_keys()
            .map(|id| id.to_string())
            .collect())
    }

    async fn leave_room(&mut self, room_id: &str) -> anyhow::Result<()> {
        let id: OwnedRoomId = matrix_sdk::ruma::RoomId::parse(room_id)?;
        self.client.send(leave_room::Request::new(id)).await?;
        Ok(())
    }

    async fn forget_room(&mut self, room_id: &str) -> anyhow::Result<()> {
        let id: OwnedRoomId = matrix_sdk::ruma::RoomId::parse(room_id)?;
        self.client.send(forget_room::Request::new(id)).await?;
        Ok(())
    }
}

/// Base backoff between leave attempts for a real run: 1s, then 2s, 4s, …
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Run the `rooms` subcommand against a bridge config: select one login, sign
/// in with a throwaway device, enumerate (and in execute mode leave) the joined
/// rooms — plus, in reset mode, reject the pending invites and forget each
/// room after leaving it — and log the throwaway device back out. Returns the
/// process exit code.
///
/// All human-facing output goes to stdout; tracing stays on stderr. The
/// password is never written to either.
pub async fn run_rooms(
    config_path: &Path,
    bridge_cfg: &MatrixBridgeConfig,
    selected_login: Option<&str>,
    execute: bool,
    reset: bool,
    retries: u32,
    delay_ms: u64,
) -> u8 {
    let mut out = std::io::stdout();

    // Choose the login to operate on: the only one, or the one named by --login.
    let login = match select_login(bridge_cfg, selected_login, &mut out) {
        Ok(login) => login,
        Err(code) => return code,
    };

    // Resolve credentials. The utility needs a resolvable password; a `${ENV}`
    // reference that cannot be expanded, or no password at all, is a setup
    // failure rather than something to act around.
    let (login_id, creds) = match login.to_credentials() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                out,
                "cannot resolve credentials for login {}: {e}",
                login.login.login_id()
            );
            return 2;
        }
    };
    if creds.password.is_none() {
        let _ = writeln!(
            out,
            "login {login_id} has no resolvable password; the rooms command needs one"
        );
        return 2;
    }

    // Fresh login with a throwaway state dir — never the bridge's own, whose
    // session file and device must stay untouched.
    let temp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(out, "could not create a throwaway state dir: {e}");
            return 2;
        }
    };
    let state_dir = temp.path().to_string_lossy().into_owned();
    let mc = match MatrixClient::login(
        &Login {
            homeserver_url: creds.homeserver_url.clone(),
            username: creds.username.clone(),
            password: creds.password.clone(),
        },
        Some(&state_dir),
        "chaz-matrix-rooms",
        None,
    )
    .await
    {
        Ok(mc) => mc,
        Err(e) => {
            let _ = writeln!(out, "login failed for {login_id}: {e}");
            return 2;
        }
    };

    // Never act on the wrong account: the authenticated MXID must match the
    // configured username. On mismatch, log the throwaway device out and stop.
    let authed = mc.client().user_id().map(|u| u.to_string());
    match authed {
        Some(id) if id == creds.username => {}
        Some(id) => {
            let _ = writeln!(
                out,
                "authenticated as {id} but configured for {}; refusing to act",
                creds.username
            );
            let _ = mc.client().logout().await;
            return 2;
        }
        None => {
            let _ = writeln!(out, "authenticated client has no user id; refusing to act");
            let _ = mc.client().logout().await;
            return 2;
        }
    }

    let mut endpoint = ClientEndpoint {
        client: mc.client().clone(),
    };
    let cfg = EngineConfig {
        mode: if execute { Mode::Execute } else { Mode::DryRun },
        reset,
        retries,
        retry_backoff: DEFAULT_RETRY_BACKOFF,
        inter_room_delay: Duration::from_millis(delay_ms),
    };
    let code = run_reset(
        &cfg,
        &creds,
        &login_id,
        &login.agent,
        config_path,
        &mut endpoint,
        &mut out,
    )
    .await;

    // Best-effort logout of the throwaway device; the temp dir is removed on
    // drop. A failed logout is a warning, not a failure of the reset itself.
    if let Err(e) = mc.client().logout().await {
        let _ = writeln!(out, "warning: logout failed: {e}");
    }

    code
}

/// Pick the login to operate on. The only configured login is implicit;
/// several require `--login` to name one by MXID.
fn select_login<'a>(
    cfg: &'a MatrixBridgeConfig,
    selected: Option<&str>,
    out: &mut impl Write,
) -> Result<&'a MatrixLoginConfig, u8> {
    if cfg.logins.is_empty() {
        let _ = writeln!(out, "no Matrix logins configured");
        return Err(2);
    }
    match selected {
        Some(id) => match cfg.logins.iter().find(|l| l.login.login_id() == id) {
            Some(l) => Ok(l),
            None => {
                let _ = writeln!(out, "no login '{id}' in config; configured logins:");
                for l in &cfg.logins {
                    let _ = writeln!(out, "  {}", l.login.login_id());
                }
                Err(2)
            }
        },
        None => {
            if cfg.logins.len() == 1 {
                Ok(&cfg.logins[0])
            } else {
                let _ = writeln!(out, "several logins configured; pick one with --login:");
                for l in &cfg.logins {
                    let _ = writeln!(out, "  {}", l.login.login_id());
                }
                Err(2)
            }
        }
    }
}

/// The decision/reporting core: print the banner, enumerate rooms (and, in
/// reset mode, pending invites), and (in execute mode) act on them — leaving
/// every joined room, and with `reset` also rejecting every pending invite and
/// forgetting each room after leaving it — reporting each outcome. Returns the
/// exit code. All output goes through `out`; the password is never in `creds`'
/// output.
///
/// In reset mode a forget that ultimately fails is a warning, not a failure:
/// forget support varies by server (some already forget on leave), so only the
/// membership operations — leaves and invite rejections — affect the exit code.
async fn run_reset<E: RoomEndpoint>(
    cfg: &EngineConfig,
    creds: &MatrixCredentials,
    login_id: &str,
    agent: &str,
    config_path: &Path,
    endpoint: &mut E,
    out: &mut impl Write,
) -> u8 {
    // Banner — account, homeserver, mode, and (in execute mode) the retry/delay
    // settings. Never the password.
    let mode_str = match (cfg.mode, cfg.reset) {
        (Mode::DryRun, false) => "dry-run",
        (Mode::DryRun, true) => "dry-run (reset)",
        (Mode::Execute, false) => "execute",
        (Mode::Execute, true) => "execute (reset: reject invites, forget after leave)",
    };
    let _ = writeln!(out, "config: {}", config_path.display());
    let _ = writeln!(out, "login: {login_id}");
    let _ = writeln!(out, "agent: {agent}");
    let _ = writeln!(out, "account: {}", creds.username);
    let _ = writeln!(out, "homeserver: {}", creds.homeserver_url);
    if cfg.mode == Mode::Execute {
        let _ = writeln!(
            out,
            "mode: {mode_str} (retries={}, delay={}ms)",
            cfg.retries,
            cfg.inter_room_delay.as_millis()
        );
    } else {
        let _ = writeln!(out, "mode: {mode_str}");
    }

    let mut rooms = match endpoint.joined_rooms().await {
        Ok(rooms) => rooms,
        Err(e) => {
            let _ = writeln!(out, "failed to enumerate joined rooms: {e}");
            return 2;
        }
    };
    rooms.sort();

    // Pending invites only matter for the reset path; plain leave-all
    // deliberately leaves them alone.
    let mut invites = Vec::new();
    if cfg.reset {
        invites = match endpoint.invited_rooms().await {
            Ok(invites) => invites,
            Err(e) => {
                let _ = writeln!(out, "failed to enumerate pending invites: {e}");
                return 2;
            }
        };
        invites.sort();
    }

    if rooms.is_empty() && invites.is_empty() {
        if cfg.reset {
            let _ = writeln!(out, "nothing to do (no joined rooms, no pending invites)");
        } else {
            let _ = writeln!(out, "nothing to do (no joined rooms)");
        }
        return 0;
    }

    match cfg.mode {
        Mode::DryRun => {
            if cfg.reset {
                for invite in &invites {
                    let _ = writeln!(out, "pending invite {invite}");
                }
                for room in &rooms {
                    let _ = writeln!(out, "joined {room}");
                }
                let _ = writeln!(
                    out,
                    "{} pending invite(s) to reject, {} joined room(s) to leave and forget; nothing was changed (dry-run)",
                    invites.len(),
                    rooms.len()
                );
                let _ = writeln!(
                    out,
                    "re-run `rooms leave-all --execute --reset` for a true reset (stop the bridge first)"
                );
            } else {
                for room in &rooms {
                    let _ = writeln!(out, "{room}");
                }
                let _ = writeln!(
                    out,
                    "{} joined room(s); nothing was left (dry-run)",
                    rooms.len()
                );
                let _ = writeln!(out, "re-run `rooms leave-all --execute` to leave them");
            }
            0
        }
        Mode::Execute => {
            let mut stats = ExecuteStats::default();
            // Reject invites before leaving joined rooms. Plain leave-all
            // never enumerates invites, so its first iterator is empty.
            let targets = invites
                .iter()
                .map(|room| (room, Target::Invited))
                .chain(rooms.iter().map(|room| (room, Target::Joined)));
            for (i, (room, target)) in targets.enumerate() {
                if i > 0 {
                    sleep(cfg.inter_room_delay).await;
                }
                execute_target(endpoint, cfg, room, target, out, &mut stats).await;
            }
            if cfg.reset {
                let _ = writeln!(
                    out,
                    "summary: rejected {}, left {}, failed {}, forgot {}, forget-failed {}",
                    stats.rejected, stats.left, stats.failed, stats.forgot, stats.forget_failed
                );
            } else {
                let _ = writeln!(out, "summary: left {}, failed {}", stats.left, stats.failed);
            }
            if stats.failed > 0 { 1 } else { 0 }
        }
    }
}

/// What an execute run is acting on for one room id.
#[derive(Debug, Clone, Copy)]
enum Target {
    /// A pending invite, rejected by leaving the invited room (per spec).
    Invited,
    /// A joined room, left.
    Joined,
}

/// Counters behind the execute-mode summary line.
#[derive(Debug, Default)]
struct ExecuteStats {
    rejected: usize,
    left: usize,
    failed: usize,
    forgot: usize,
    forget_failed: usize,
}

/// One execute-mode target: reject it or leave it (both are a leave call), and
/// in reset mode follow a successful membership change with a best-effort
/// forget of the same room. Reports every outcome through `out`.
async fn execute_target<E: RoomEndpoint>(
    endpoint: &mut E,
    cfg: &EngineConfig,
    room: &str,
    target: Target,
    out: &mut impl Write,
    stats: &mut ExecuteStats,
) {
    let label = match target {
        Target::Invited => "rejected invite",
        Target::Joined => "left",
    };
    match with_retries(endpoint, Op::Leave, room, cfg.retries, cfg.retry_backoff).await {
        Ok(()) => {
            let _ = writeln!(out, "{label} {room}");
            match target {
                Target::Invited => stats.rejected += 1,
                Target::Joined => stats.left += 1,
            }
            if cfg.reset {
                match with_retries(endpoint, Op::Forget, room, cfg.retries, cfg.retry_backoff).await
                {
                    Ok(()) => {
                        let _ = writeln!(out, "forgot {room}");
                        stats.forgot += 1;
                    }
                    Err(e) => {
                        let _ = writeln!(
                            out,
                            "warning: forget failed for {room} ({} attempts): {e}",
                            cfg.retries
                        );
                        stats.forget_failed += 1;
                    }
                }
            }
        }
        Err(e) => {
            let _ = writeln!(out, "FAILED {room} ({} attempts): {e}", cfg.retries);
            stats.failed += 1;
        }
    }
}

/// A membership call the retry loop can make.
#[derive(Debug, Clone, Copy)]
enum Op {
    Leave,
    Forget,
}

/// Run one membership `op` on `room`, retrying up to `retries` attempts with a
/// doubling backoff (1s, 2s, 4s… — zero in tests).
async fn with_retries<E: RoomEndpoint>(
    endpoint: &mut E,
    op: Op,
    room: &str,
    retries: u32,
    backoff: Duration,
) -> anyhow::Result<()> {
    let mut delay = backoff;
    let mut last_err = None;
    for attempt in 1..=retries {
        let result = match op {
            Op::Leave => endpoint.leave_room(room).await,
            Op::Forget => endpoint.forget_room(room).await,
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < retries {
                    sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
            }
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => anyhow::bail!("no attempts were made"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn creds() -> MatrixCredentials {
        MatrixCredentials {
            homeserver_url: "https://matrix.example".to_string(),
            username: "@chaz:example".to_string(),
            password: Some("hunter2".to_string()),
            allow_list: None,
            room_size_limit: None,
        }
    }

    fn dry_cfg() -> EngineConfig {
        EngineConfig {
            mode: Mode::DryRun,
            reset: false,
            retries: 3,
            retry_backoff: Duration::ZERO,
            inter_room_delay: Duration::ZERO,
        }
    }

    fn exec_cfg() -> EngineConfig {
        EngineConfig {
            mode: Mode::Execute,
            ..dry_cfg()
        }
    }

    fn reset_dry_cfg() -> EngineConfig {
        EngineConfig {
            reset: true,
            ..dry_cfg()
        }
    }

    fn reset_exec_cfg() -> EngineConfig {
        EngineConfig {
            reset: true,
            ..exec_cfg()
        }
    }

    async fn run<E: RoomEndpoint>(
        cfg: &EngineConfig,
        creds: &MatrixCredentials,
        endpoint: &mut E,
    ) -> (u8, String) {
        let mut buf = Vec::new();
        let code = run_reset(
            cfg,
            creds,
            "@chaz:example",
            "chaz",
            Path::new("/tmp/bridge.yaml"),
            endpoint,
            &mut buf,
        )
        .await;
        (code, String::from_utf8(buf).unwrap())
    }

    /// Scripted, stateful endpoint: `joined_rooms`/`invited_rooms` return the
    /// current membership, and a successful `leave_room` drops the room from
    /// joined (a leave) or from invited (an invite rejection) — so tests can
    /// assert on the end state the way a homeserver would report it.
    /// `fail_then_succeed` fails the first N leave attempts for a room before
    /// succeeding; `always_fail` fails forever; `forget_fail` fails every
    /// forget for a room. Every attempt is recorded.
    struct FakeEndpoint {
        rooms: Vec<String>,
        invited: Vec<String>,
        fail_then_succeed: HashMap<String, u32>,
        always_fail: HashSet<String>,
        forget_fail: HashSet<String>,
        fail_invite_enumeration: bool,
        leave_attempts: Vec<String>,
        forget_attempts: Vec<String>,
    }

    impl RoomEndpoint for FakeEndpoint {
        async fn joined_rooms(&mut self) -> anyhow::Result<Vec<String>> {
            Ok(self.rooms.clone())
        }

        async fn invited_rooms(&mut self) -> anyhow::Result<Vec<String>> {
            if self.fail_invite_enumeration {
                anyhow::bail!("cannot sync");
            }
            Ok(self.invited.clone())
        }

        async fn leave_room(&mut self, room_id: &str) -> anyhow::Result<()> {
            self.leave_attempts.push(room_id.to_string());
            if self.always_fail.contains(room_id) {
                anyhow::bail!("boom for {room_id}");
            }
            if let Some(remaining) = self.fail_then_succeed.get_mut(room_id)
                && *remaining > 0
            {
                *remaining -= 1;
                anyhow::bail!("transient for {room_id}");
            }
            self.rooms.retain(|r| r != room_id);
            self.invited.retain(|r| r != room_id);
            Ok(())
        }

        async fn forget_room(&mut self, room_id: &str) -> anyhow::Result<()> {
            self.forget_attempts.push(room_id.to_string());
            if self.forget_fail.contains(room_id) {
                anyhow::bail!("no forget for {room_id}");
            }
            Ok(())
        }
    }

    fn fake(rooms: Vec<String>) -> FakeEndpoint {
        FakeEndpoint {
            rooms,
            invited: Vec::new(),
            fail_then_succeed: HashMap::new(),
            always_fail: HashSet::new(),
            forget_fail: HashSet::new(),
            fail_invite_enumeration: false,
            leave_attempts: Vec::new(),
            forget_attempts: Vec::new(),
        }
    }

    fn fake_reset(rooms: Vec<String>, invited: Vec<String>) -> FakeEndpoint {
        let mut ep = fake(rooms);
        ep.invited = invited;
        ep
    }

    #[tokio::test]
    async fn dry_run_lists_rooms_without_leaving() {
        let mut ep = fake(vec!["!b:example".into(), "!a:example".into()]);
        let (code, out) = run(&dry_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(out.contains("!a:example"), "output: {out}");
        assert!(out.contains("!b:example"), "output: {out}");
        assert!(out.contains("dry-run"), "output: {out}");
        assert!(out.contains("nothing was left"), "output: {out}");
        assert!(ep.leave_attempts.is_empty());
    }

    #[tokio::test]
    async fn execute_leaves_every_room_in_sorted_order() {
        let mut ep = fake(vec![
            "!b:example".into(),
            "!a:example".into(),
            "!c:example".into(),
        ]);
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert_eq!(
            ep.leave_attempts,
            vec!["!a:example", "!b:example", "!c:example"]
        );
        assert!(out.contains("left !a:example"), "output: {out}");
        assert!(out.contains("left !b:example"), "output: {out}");
        assert!(out.contains("left !c:example"), "output: {out}");
        assert!(out.contains("summary: left 3, failed 0"), "output: {out}");
    }

    #[tokio::test]
    async fn execute_continues_past_a_failing_room() {
        let mut ep = fake(vec![
            "!b:example".into(),
            "!a:example".into(),
            "!c:example".into(),
        ]);
        ep.always_fail.insert("!b:example".into());
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 1);
        assert!(out.contains("FAILED !b:example"), "output: {out}");
        assert!(out.contains("left !a:example"), "output: {out}");
        assert!(out.contains("left !c:example"), "output: {out}");
        // Every room was attempted, and the two that succeed are left.
        assert!(ep.leave_attempts.contains(&"!a:example".to_string()));
        assert!(ep.leave_attempts.contains(&"!b:example".to_string()));
        assert!(ep.leave_attempts.contains(&"!c:example".to_string()));
        assert!(out.contains("summary: left 2, failed 1"), "output: {out}");
    }

    #[tokio::test]
    async fn empty_enumeration_is_nothing_to_do() {
        let mut ep = fake(vec![]);
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(out.contains("nothing to do"), "output: {out}");
        assert!(ep.leave_attempts.is_empty());
    }

    #[tokio::test]
    async fn leave_retries_until_success() {
        let mut ep = fake(vec!["!r:example".into()]);
        ep.fail_then_succeed.insert("!r:example".into(), 2);
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(out.contains("left !r:example"), "output: {out}");
        // Two failures then a success.
        assert_eq!(ep.leave_attempts.len(), 3);
    }

    #[tokio::test]
    async fn leave_always_failing_uses_exactly_retries_attempts() {
        let mut ep = fake(vec!["!r:example".into()]);
        ep.always_fail.insert("!r:example".into());
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 1);
        assert!(
            out.contains("FAILED !r:example (3 attempts)"),
            "output: {out}"
        );
        assert_eq!(ep.leave_attempts.len(), 3);
    }

    #[tokio::test]
    async fn reset_dry_run_previews_invites_and_rooms_without_mutating() {
        let mut ep = fake_reset(vec!["!b:example".into()], vec!["!inv:example".into()]);
        let (code, out) = run(&reset_dry_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(out.contains("pending invite !inv:example"), "output: {out}");
        assert!(out.contains("joined !b:example"), "output: {out}");
        assert!(out.contains("mode: dry-run (reset)"), "output: {out}");
        assert!(
            out.contains("1 pending invite(s) to reject, 1 joined room(s)"),
            "output: {out}"
        );
        assert!(out.contains("--execute --reset"), "output: {out}");
        assert!(ep.leave_attempts.is_empty());
        assert!(ep.forget_attempts.is_empty());
        assert_eq!(ep.rooms.len(), 1);
        assert_eq!(ep.invited.len(), 1);
    }

    #[tokio::test]
    async fn reset_execute_leaves_no_joined_rooms_and_no_pending_invites() {
        let mut ep = fake_reset(
            vec!["!a:example".into(), "!b:example".into()],
            vec!["!i2:example".into(), "!i1:example".into()],
        );
        let (code, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        // The reset's contract: zero joined rooms AND zero pending invites.
        assert!(ep.rooms.is_empty(), "joined rooms remain: {:?}", ep.rooms);
        assert!(ep.invited.is_empty(), "invites remain: {:?}", ep.invited);
        // Invites are rejected first (sorted), then rooms are left (sorted),
        // and every one of the four is forgotten afterwards.
        assert_eq!(
            ep.leave_attempts,
            vec![
                "!i1:example".to_string(),
                "!i2:example".to_string(),
                "!a:example".to_string(),
                "!b:example".to_string(),
            ]
        );
        assert!(out.contains("rejected invite !i1:example"), "output: {out}");
        assert!(out.contains("rejected invite !i2:example"), "output: {out}");
        assert!(out.contains("left !a:example"), "output: {out}");
        assert!(out.contains("left !b:example"), "output: {out}");
        assert_eq!(ep.forget_attempts.len(), 4);
        assert!(
            out.contains("summary: rejected 2, left 2, failed 0, forgot 4, forget-failed 0"),
            "output: {out}"
        );
    }

    #[tokio::test]
    async fn reset_execute_continues_past_failures() {
        let mut ep = fake_reset(
            vec!["!bad:example".into(), "!z:example".into()],
            vec!["!i-bad:example".into(), "!i-ok:example".into()],
        );
        ep.always_fail.insert("!bad:example".into());
        ep.always_fail.insert("!i-bad:example".into());
        let (code, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 1);
        assert!(out.contains("FAILED !bad:example"), "output: {out}");
        assert!(out.contains("FAILED !i-bad:example"), "output: {out}");
        assert!(
            out.contains("rejected invite !i-ok:example"),
            "output: {out}"
        );
        assert!(out.contains("left !z:example"), "output: {out}");
        // Both kinds of failed leave are skipped for forgetting, and neither
        // stops the next target from being handled.
        assert_eq!(ep.forget_attempts, vec!["!i-ok:example", "!z:example"]);
        assert_eq!(ep.rooms, vec!["!bad:example"]);
        assert_eq!(ep.invited, vec!["!i-bad:example"]);
        assert!(
            out.contains("summary: rejected 1, left 1, failed 2, forgot 2, forget-failed 0"),
            "output: {out}"
        );
    }

    #[tokio::test]
    async fn reset_forget_failure_is_a_warning_not_a_run_failure() {
        let mut ep = fake_reset(vec!["!a:example".into()], vec![]);
        ep.forget_fail.insert("!a:example".into());
        let (code, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(
            out.contains("warning: forget failed for !a:example"),
            "output: {out}"
        );
        // The leave itself still happened, and the forget was retried.
        assert!(ep.rooms.is_empty());
        assert_eq!(ep.forget_attempts.len(), 3);
        assert!(
            out.contains("summary: rejected 0, left 1, failed 0, forgot 0, forget-failed 1"),
            "output: {out}"
        );
    }

    #[tokio::test]
    async fn plain_execute_leaves_invites_and_forgetting_alone() {
        let mut ep = fake_reset(vec!["!a:example".into()], vec!["!inv:example".into()]);
        let (code, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(out.contains("left !a:example"), "output: {out}");
        assert!(!out.contains("rejected invite"), "output: {out}");
        assert_eq!(ep.invited.len(), 1);
        assert!(ep.forget_attempts.is_empty());
        assert!(out.contains("summary: left 1, failed 0"), "output: {out}");
    }

    #[tokio::test]
    async fn reset_with_nothing_to_do_is_success() {
        let mut ep = fake_reset(vec![], vec![]);
        let (code, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 0);
        assert!(
            out.contains("nothing to do (no joined rooms, no pending invites)"),
            "output: {out}"
        );
        assert!(ep.leave_attempts.is_empty());
        assert!(ep.forget_attempts.is_empty());
    }

    #[tokio::test]
    async fn reset_invite_enumeration_failure_is_a_setup_failure() {
        let mut ep = fake_reset(vec!["!a:example".into()], vec!["!inv:example".into()]);
        ep.fail_invite_enumeration = true;
        let (code, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert_eq!(code, 2);
        assert!(
            out.contains("failed to enumerate pending invites"),
            "output: {out}"
        );
        assert!(ep.leave_attempts.is_empty());
    }

    #[tokio::test]
    async fn secret_never_appears_in_output() {
        let secret = MatrixCredentials {
            homeserver_url: "https://matrix.example".to_string(),
            username: "@chaz:example".to_string(),
            password: Some("<sentinel-secret>".to_string()),
            allow_list: None,
            room_size_limit: None,
        };

        // Dry-run.
        let mut ep = fake(vec!["!a:example".into()]);
        let (_, out) = run(&dry_cfg(), &secret, &mut ep).await;
        assert!(!out.contains("<sentinel-secret>"), "output: {out}");

        // Success.
        let mut ep = fake(vec!["!a:example".into()]);
        let (_, out) = run(&exec_cfg(), &secret, &mut ep).await;
        assert!(!out.contains("<sentinel-secret>"), "output: {out}");

        // Failure.
        let mut ep = fake(vec!["!a:example".into()]);
        ep.always_fail.insert("!a:example".into());
        let (_, out) = run(&exec_cfg(), &secret, &mut ep).await;
        assert!(!out.contains("<sentinel-secret>"), "output: {out}");

        // Reset execute.
        let mut ep = fake_reset(vec!["!a:example".into()], vec!["!inv:example".into()]);
        let (_, out) = run(&reset_exec_cfg(), &secret, &mut ep).await;
        assert!(!out.contains("<sentinel-secret>"), "output: {out}");
    }

    /// Drive the real `run_rooms` path — throwaway login, enumeration, reset,
    /// logout — against a mock homeserver. The rooms login shares the bridge's
    /// E2EE-capable client builder, so this pins down that a reset run still
    /// publishes no device, one-time, or cross-signing keys, syncs with
    /// offline presence, rejects the invite and leaves the joined room each
    /// before forgetting it, and logs the throwaway device out.
    #[tokio::test]
    async fn reset_run_against_a_homeserver_uploads_no_keys_and_forgets_after_leave() {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ok(serde_json::json!({ "versions": ["v1.11"] })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/login"))
            .respond_with(ok(serde_json::json!({
                "user_id": "@chaz:example",
                "access_token": "disposable",
                "device_id": "THROWAWAY"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/joined_rooms"))
            .respond_with(ok(
                serde_json::json!({ "joined_rooms": ["!joined:example"] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ok(serde_json::json!({
                "next_batch": "s1",
                "rooms": { "invite": { "!invited:example": { "invite_state": { "events": [] } } } }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/[^/]+/(leave|forget)$",
            ))
            .respond_with(ok(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/_matrix/client/v3/logout"))
            .respond_with(ok(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let bridge_state = tempfile::tempdir().unwrap();
        let cfg: MatrixBridgeConfig = serde_yaml::from_str(&format!(
            r#"
state_dir: {}
unlock_password: fixture-unlock
logins:
  - agent: chaz
    type: matrix
    homeserver_url: {}
    username: "@chaz:example"
    password: fixture-password
"#,
            bridge_state.path().display(),
            server.uri()
        ))
        .unwrap();

        let code = run_rooms(Path::new("/tmp/bridge.yaml"), &cfg, None, true, true, 1, 0).await;
        assert_eq!(code, 0);

        let requests = server.received_requests().await.unwrap();
        let calls: Vec<String> = requests
            .iter()
            .map(|r| {
                let p = r.url.path();
                format!("{} {}", r.method, p.replace("%21", "!").replace("%3A", ":"))
            })
            .collect();
        assert!(
            !calls.iter().any(|c| c.contains("/keys/upload")
                || c.contains("/keys/device_signing/upload")
                || c.contains("/keys/signatures/upload")
                || (c.starts_with("PUT") && c.contains("/room_keys/"))),
            "a rooms run must not publish key material: {calls:?}"
        );

        let sync = requests
            .iter()
            .find(|r| r.url.path() == "/_matrix/client/v3/sync")
            .expect("reset enumerates invites with one sync");
        assert!(
            sync.url
                .query_pairs()
                .any(|(k, v)| k == "set_presence" && v == "offline"),
            "sync must not flip presence online: {}",
            sync.url
        );

        let membership: Vec<&String> = calls
            .iter()
            .filter(|c| c.ends_with("/leave") || c.ends_with("/forget"))
            .collect();
        assert_eq!(
            membership,
            [
                "POST /_matrix/client/v3/rooms/!invited:example/leave",
                "POST /_matrix/client/v3/rooms/!invited:example/forget",
                "POST /_matrix/client/v3/rooms/!joined:example/leave",
                "POST /_matrix/client/v3/rooms/!joined:example/forget",
            ]
        );
        assert_eq!(
            calls.last().map(String::as_str),
            Some("POST /_matrix/client/v3/logout")
        );

        // The bridge's own state dir is never used for the throwaway device.
        assert_eq!(std::fs::read_dir(bridge_state.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn banner_reports_account_homeserver_and_mode() {
        let mut ep = fake(vec![]);
        let (_, out) = run(&dry_cfg(), &creds(), &mut ep).await;
        assert!(out.contains("account: @chaz:example"), "output: {out}");
        assert!(
            out.contains("homeserver: https://matrix.example"),
            "output: {out}"
        );
        assert!(out.contains("mode: dry-run"), "output: {out}");

        let mut ep = fake(vec![]);
        let (_, out) = run(&exec_cfg(), &creds(), &mut ep).await;
        assert!(out.contains("mode: execute"), "output: {out}");

        let mut ep = fake(vec![]);
        let (_, out) = run(&reset_dry_cfg(), &creds(), &mut ep).await;
        assert!(out.contains("mode: dry-run (reset)"), "output: {out}");

        let mut ep = fake(vec![]);
        let (_, out) = run(&reset_exec_cfg(), &creds(), &mut ep).await;
        assert!(
            out.contains("mode: execute (reset: reject invites, forget after leave)"),
            "output: {out}"
        );
        assert!(out.contains("retries=3"), "output: {out}");
    }
}

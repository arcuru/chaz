//! `chaz-matrix rooms` — enumerate, and optionally leave, every joined Matrix
//! room for a bridge login.
//!
//! A maintenance path, not part of the bridge's steady-state runtime. It signs
//! in with a fresh throwaway device — never the bridge's own persisted session
//! file or device — lists the account's joined rooms, and in `--execute` mode
//! leaves them one by one before logging the throwaway device back out. The
//! bridge's own session is untouched, and pending invitations are left alone:
//! only rooms the account is already joined to are enumerated and left, so a
//! re-run after a reset sees fewer (or zero) rooms.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::bridge::{Login, MatrixClient};
use crate::config::{MatrixBridgeConfig, MatrixLoginConfig};
use crate::credentials::MatrixCredentials;

use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::api::client::membership::joined_rooms::v3 as joined_rooms;
use matrix_sdk::ruma::api::client::membership::leave_room::v3 as leave_room;
use tokio::time::sleep;

/// Whether `rooms` only reports or actually leaves rooms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// List joined rooms and mutate nothing.
    DryRun,
    /// Leave every joined room.
    Execute,
}

/// Tuning for one reset run. Durations are injectable so the decision core can
/// be exercised with zero-length sleeps in tests.
#[derive(Debug, Clone, Copy)]
struct EngineConfig {
    mode: Mode,
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
    async fn leave_room(&mut self, room_id: &str) -> anyhow::Result<()>;
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

    async fn leave_room(&mut self, room_id: &str) -> anyhow::Result<()> {
        let id: OwnedRoomId = matrix_sdk::ruma::RoomId::parse(room_id)?;
        self.client.send(leave_room::Request::new(id)).await?;
        Ok(())
    }
}

/// Base backoff between leave attempts for a real run: 1s, then 2s, 4s, …
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Run the `rooms` subcommand against a bridge config: select one login, sign
/// in with a throwaway device, enumerate (and in execute mode leave) the joined
/// rooms, and log the throwaway device back out. Returns the process exit code.
///
/// All human-facing output goes to stdout; tracing stays on stderr. The
/// password is never written to either.
pub async fn run_rooms(
    config_path: &Path,
    bridge_cfg: &MatrixBridgeConfig,
    selected_login: Option<&str>,
    execute: bool,
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

/// The decision/reporting core: print the banner, enumerate rooms, and (in
/// execute mode) leave them, reporting each outcome. Returns the exit code.
/// All output goes through `out`; the password is never in `creds`' output.
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
    let mode_str = match cfg.mode {
        Mode::DryRun => "dry-run",
        Mode::Execute => "execute",
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

    if rooms.is_empty() {
        let _ = writeln!(out, "nothing to do (no joined rooms)");
        return 0;
    }

    match cfg.mode {
        Mode::DryRun => {
            for room in &rooms {
                let _ = writeln!(out, "{room}");
            }
            let _ = writeln!(
                out,
                "{} joined room(s); nothing was left (dry-run)",
                rooms.len()
            );
            let _ = writeln!(out, "re-run with --execute to leave them");
            0
        }
        Mode::Execute => {
            let mut left = 0usize;
            let mut failed = 0usize;
            for (i, room) in rooms.iter().enumerate() {
                if i > 0 {
                    sleep(cfg.inter_room_delay).await;
                }
                match leave_with_retries(endpoint, room, cfg.retries, cfg.retry_backoff).await {
                    Ok(()) => {
                        let _ = writeln!(out, "left {room}");
                        left += 1;
                    }
                    Err(e) => {
                        let _ = writeln!(out, "FAILED {room} ({} attempts): {e}", cfg.retries);
                        failed += 1;
                    }
                }
            }
            let _ = writeln!(out, "summary: left {left}, failed {failed}");
            if failed > 0 { 1 } else { 0 }
        }
    }
}

/// Leave `room`, retrying up to `retries` attempts with a doubling backoff
/// (1s, 2s, 4s… — zero in tests).
async fn leave_with_retries<E: RoomEndpoint>(
    endpoint: &mut E,
    room: &str,
    retries: u32,
    backoff: Duration,
) -> anyhow::Result<()> {
    let mut delay = backoff;
    let mut last_err = None;
    for attempt in 1..=retries {
        match endpoint.leave_room(room).await {
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
        None => anyhow::bail!("no leave attempts were made"),
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
            retries: 3,
            retry_backoff: Duration::ZERO,
            inter_room_delay: Duration::ZERO,
        }
    }

    fn exec_cfg() -> EngineConfig {
        EngineConfig {
            mode: Mode::Execute,
            retries: 3,
            retry_backoff: Duration::ZERO,
            inter_room_delay: Duration::ZERO,
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

    /// Scripted endpoint: `joined_rooms` returns `rooms` verbatim; `leave_room`
    /// fails the first `fail_then_succeed[room]` times before succeeding, or
    /// forever for rooms in `always_fail`. Every attempt is recorded.
    struct FakeEndpoint {
        rooms: Vec<String>,
        fail_then_succeed: HashMap<String, u32>,
        always_fail: HashSet<String>,
        leave_attempts: Vec<String>,
    }

    impl RoomEndpoint for FakeEndpoint {
        async fn joined_rooms(&mut self) -> anyhow::Result<Vec<String>> {
            Ok(self.rooms.clone())
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
            Ok(())
        }
    }

    fn fake(rooms: Vec<String>) -> FakeEndpoint {
        FakeEndpoint {
            rooms,
            fail_then_succeed: HashMap::new(),
            always_fail: HashSet::new(),
            leave_attempts: Vec::new(),
        }
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
    }
}

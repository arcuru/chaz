//! Control socket — reach a running daemon with a `/command`.
//!
//! `chaz cmd` builds a server against the state directory. A daemon holding
//! that directory does not observe those writes and is not observed by them,
//! so the command has to run with the daemon stopped — which is exactly wrong
//! for the commands worth running while the peer is up (`/sharing requests`
//! on a bridge that is waiting, `/agents` on a daemon that is misbehaving).
//!
//! The daemon therefore listens on a unix socket beside its `eidetica.db`, and
//! `chaz cmd` prefers it, falling back to opening the state directory when
//! nothing is listening.
//!
//! Wire format: one JSON [`Request`] line in, one JSON [`Response`] line out,
//! then the connection closes. Commands are rendered to text by the daemon, so
//! output is identical whichever path served it.
//!
//! **Reaching this socket is the peer's full administrative surface** — share
//! tickets, bootstrap approvals, agent invitations, session transcripts. The
//! boundary is `SO_PEERCRED`: a connection whose uid differs from the socket
//! owner's is dropped before its request is read. Socket mode `0600` and the
//! state directory's own mode are defense in depth, not the boundary — `bind`
//! applies the umask, so the mode is briefly wider than intended and only the
//! credential check covers that window.
//!
//! Full rationale, including what was deliberately left out:
//! `docs/src/design/control_socket.md`.

use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chaz_core::config::Config;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

/// Wire protocol version. Bumped only for an incompatible change; a mismatch
/// is refused on sight rather than half-parsed, because a client and daemon
/// from different builds disagreeing silently is worse than not talking.
pub const PROTOCOL_VERSION: u32 = 1;

/// Socket file name inside the state directory.
pub const SOCKET_FILE_NAME: &str = "control.sock";

/// Session used for socket-delivered commands that name none. A stable
/// reused session rather than a fresh one per invocation: the direct path's
/// throwaway-per-run is affordable for a bring-up script against a cold state
/// directory, and not for a surface used routinely against a live peer, where
/// it would add a catalog row per `/agents`.
pub const CONTROL_SESSION_NAME: &str = "control";

/// How long a connected client gets to send its request line. Bounds a stalled
/// or hostile client's hold on a connection; the command it asks for is
/// deliberately unbounded, since legitimate verbs (`/agent import` against a
/// slow peer) do take a while.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a request line. A command line is short; anything approaching
/// this is a client bug or an attempt to grow the daemon's heap.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Ceiling on a response line. Transcripts (`/print`) are the large case, so
/// this is far more generous than the request side.
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    /// The command line, including its leading `/`.
    pub command: String,
    /// Session to run against. `None` uses [`CONTROL_SESSION_NAME`].
    #[serde(default)]
    pub session: Option<String>,
}

impl Request {
    pub fn new(command: String, session: Option<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            command,
            session,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    /// False when the command reported an error. The client turns this into
    /// its exit status, which is all a calling script can branch on.
    pub ok: bool,
    /// Rendered output, or the error text when `ok` is false.
    pub output: String,
}

impl Response {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            output: output.into(),
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            output: output.into(),
        }
    }
}

/// Where the control socket lives, or `None` when the operator disabled it.
///
/// Precedence: `control.path` (tilde-expanded), else `<state_dir>/control.sock`.
/// Without a state directory there is nowhere private to put it, so there is
/// no socket.
pub fn socket_path(config: &Config, state_dir: Option<&Path>) -> Option<PathBuf> {
    let control = config.control.clone().unwrap_or_default();
    if !control.enabled {
        return None;
    }
    if let Some(p) = control.path.as_deref() {
        return Some(chaz_core::util::expand_home(Path::new(p)));
    }
    state_dir.map(|d| d.join(SOCKET_FILE_NAME))
}

/// A bound control socket. Holds the uid that owns the socket file, which is
/// the daemon's own effective uid and the only uid allowed to connect.
#[derive(Debug)]
pub struct ControlListener {
    listener: UnixListener,
    path: PathBuf,
    owner_uid: u32,
}

impl ControlListener {
    /// Bind `path`, sweeping a socket file left behind by a crash.
    ///
    /// A unix socket file outlives its process, so "stale" and "another daemon
    /// is running" look identical on disk. Connecting tells them apart: an
    /// answer means a live daemon already holds this state directory and we
    /// refuse to start, since two daemons on one backend is precisely the
    /// condition this feature exists to make visible.
    pub async fn bind(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        match UnixStream::connect(&path).await {
            Ok(_) => anyhow::bail!(
                "another chaz daemon is already listening on {} — stop it, or point this one \
                 at a different state directory",
                path.display()
            ),
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                debug!(path = %path.display(), "Removing stale control socket");
                std::fs::remove_file(&path)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "cannot determine whether {} is live: {e}",
                    path.display()
                ));
            }
        }

        let listener = UnixListener::bind(&path)?;
        // `bind` applies the umask, so narrow it explicitly. The window
        // between the two is covered by the peer-credential check below.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let owner_uid = std::fs::metadata(&path)?.uid();
        warn_if_directory_is_writable_by_others(&path);

        info!(socket = %path.display(), "Control socket listening");
        Ok(Self {
            listener,
            path,
            owner_uid,
        })
    }
}

/// Someone who can write the socket's directory can unlink it and bind their
/// own in its place, and no permission on the socket itself prevents that. Say
/// so rather than silently `chmod`-ing a directory the operator chose.
fn warn_if_directory_is_writable_by_others(socket: &Path) {
    let Some(dir) = socket.parent() else { return };
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        warn!(
            dir = %dir.display(),
            mode = format!("{:o}", mode),
            "Control socket directory is writable by group or others; anyone who can write it \
             can replace the socket"
        );
    }
}

/// Live control socket. Dropping it stops accepting and unlinks the socket, so
/// the daemon leaves no file behind on a clean shutdown.
pub struct ControlHandle {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ControlHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve `listener` until the returned handle is dropped.
///
/// Each connection is handled in its own task with no serialization between
/// them: the daemon already runs the agent loop, sync, and the routine engine
/// against these databases, so control commands are not a new class of
/// concurrent writer.
pub fn serve<H, F>(listener: ControlListener, handler: H) -> ControlHandle
where
    H: Fn(Request) -> F + Send + Sync + 'static,
    F: Future<Output = Response> + Send + 'static,
{
    let path = listener.path.to_path_buf();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        let ControlListener {
            listener,
            owner_uid,
            ..
        } = listener;
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!("Control socket accept failed: {e}");
                    continue;
                }
            };
            if !peer_is_authorized(&stream, owner_uid) {
                continue;
            }
            let handler = handler.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_connection(stream, handler.as_ref()).await {
                    debug!("Control connection ended: {e}");
                }
            });
        }
    });
    ControlHandle { path, task }
}

/// The authorization boundary: kernel-supplied peer credentials, checked
/// before a single byte of the request is read.
fn peer_is_authorized(stream: &UnixStream, owner_uid: u32) -> bool {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == owner_uid => true,
        Ok(cred) => {
            warn!(
                peer_uid = cred.uid(),
                owner_uid, "Rejected control connection from another uid"
            );
            false
        }
        // No credentials, no authorization. This should not happen on a unix
        // socket, and guessing in its favour would guess about the one thing
        // guarding an administrative surface.
        Err(e) => {
            warn!("Rejected control connection with unreadable peer credentials: {e}");
            false
        }
    }
}

async fn serve_connection<H, F>(stream: UnixStream, handler: &H) -> anyhow::Result<()>
where
    H: Fn(Request) -> F,
    F: Future<Output = Response>,
{
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_BYTES));
    let mut line = String::new();

    let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line)).await;
    let response = match read {
        Err(_) => Response::error(format!(
            "timed out waiting for a request line after {}s",
            REQUEST_READ_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => return Err(e.into()),
        Ok(Ok(0)) => return Ok(()), // client hung up without asking anything
        Ok(Ok(_)) => match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) if req.v != PROTOCOL_VERSION => Response::error(format!(
                "unsupported control protocol version {} (this daemon speaks {PROTOCOL_VERSION}) \
                 — the client and the daemon are from different builds",
                req.v
            )),
            Ok(req) => handler(req).await,
            Err(e) => Response::error(format!("malformed control request: {e}")),
        },
    };

    let mut encoded = serde_json::to_vec(&response)?;
    encoded.push(b'\n');
    write_half.write_all(&encoded).await?;
    write_half.flush().await?;
    Ok(())
}

/// Outcome of a client attempt.
pub enum Attempt {
    /// No daemon is listening — the caller should fall back to opening the
    /// state directory itself.
    NotListening,
    Answered(Response),
}

/// Send one request to a listening daemon.
///
/// `NotListening` is returned only for the two conditions that genuinely mean
/// "nobody is there": the socket does not exist, or it exists with no listener.
/// Anything else — a permission error, a protocol error, a half-answered
/// request — is an error rather than a fallback. A daemon that accepted the
/// connection is holding the backend, and opening it a second time would
/// answer from a view missing everything the daemon has not flushed: a wrong
/// answer wearing a right answer's clothes.
pub async fn request(path: &Path, req: &Request) -> anyhow::Result<Attempt> {
    let stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            debug!(socket = %path.display(), "No control socket; using the state directory directly");
            return Ok(Attempt::NotListening);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "control socket {} exists but could not be reached: {e}",
                path.display()
            ));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    let mut encoded = serde_json::to_vec(req)?;
    encoded.push(b'\n');
    write_half.write_all(&encoded).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half.take(MAX_RESPONSE_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        anyhow::bail!("control socket {} closed without answering", path.display());
    }
    let response: Response = serde_json::from_str(line.trim())
        .map_err(|e| anyhow::anyhow!("malformed control response: {e}"))?;
    if response.v != PROTOCOL_VERSION {
        anyhow::bail!(
            "daemon speaks control protocol version {} and this client speaks \
             {PROTOCOL_VERSION} — they are from different builds",
            response.v
        );
    }
    Ok(Attempt::Answered(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaz_core::config::ControlConfig;

    async fn bound(dir: &tempfile::TempDir) -> (ControlHandle, PathBuf) {
        let path = dir.path().join("control.sock");
        let listener = ControlListener::bind(&path).await.expect("bind");
        let handle = serve(listener, |req: Request| async move {
            if req.command == "/boom" {
                Response::error("boom")
            } else {
                Response::ok(format!("ran {} on {:?}", req.command, req.session))
            }
        });
        (handle, path)
    }

    #[tokio::test]
    async fn a_request_round_trips_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;

        let req = Request::new("/agents".into(), Some("work".into()));
        let Attempt::Answered(resp) = request(&path, &req).await.unwrap() else {
            panic!("expected an answer from a listening daemon");
        };
        assert!(resp.ok);
        assert_eq!(resp.output, r#"ran /agents on Some("work")"#);
    }

    #[tokio::test]
    async fn an_error_outcome_stays_an_error_across_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;

        let Attempt::Answered(resp) = request(&path, &Request::new("/boom".into(), None))
            .await
            .unwrap()
        else {
            panic!("expected an answer");
        };
        // The exit status of `chaz cmd` is derived from this flag, so an error
        // arriving as a successful response would report a failed command as a
        // clean run.
        assert!(!resp.ok);
        assert_eq!(resp.output, "boom");
    }

    #[tokio::test]
    async fn an_absent_socket_reports_not_listening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nothing-here.sock");
        assert!(matches!(
            request(&path, &Request::new("/agents".into(), None))
                .await
                .unwrap(),
            Attempt::NotListening
        ));
    }

    #[tokio::test]
    async fn a_socket_with_no_listener_reports_not_listening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        // std's listener does not unlink on drop, which is exactly the file a
        // crashed daemon leaves behind.
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());

        assert!(matches!(
            request(&path, &Request::new("/agents".into(), None))
                .await
                .unwrap(),
            Attempt::NotListening
        ));
    }

    #[tokio::test]
    async fn bind_sweeps_a_socket_left_by_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);

        let listener = ControlListener::bind(&path)
            .await
            .expect("stale file swept");
        let _handle = serve(listener, |_req| async { Response::ok("fine") });

        let Attempt::Answered(resp) = request(&path, &Request::new("/agents".into(), None))
            .await
            .unwrap()
        else {
            panic!("expected the fresh listener to answer");
        };
        assert_eq!(resp.output, "fine");
    }

    #[tokio::test]
    async fn bind_refuses_when_another_daemon_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;

        let err = ControlListener::bind(&path).await.unwrap_err().to_string();
        assert!(
            err.contains("already listening"),
            "second bind must name the conflict, got: {err}"
        );
        // …and must not have unlinked the live daemon's socket on its way out.
        let Attempt::Answered(_) = request(&path, &Request::new("/agents".into(), None))
            .await
            .unwrap()
        else {
            panic!("the original daemon is still serving");
        };
    }

    #[tokio::test]
    async fn the_socket_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "control socket must not be group/world reachable"
        );
    }

    #[tokio::test]
    async fn dropping_the_handle_unlinks_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, path) = bound(&dir).await;
        assert!(path.exists());
        drop(handle);
        assert!(!path.exists(), "clean shutdown must leave no socket file");
    }

    /// Raw line in, raw line out — bypasses the typed client so malformed and
    /// mismatched input can be exercised.
    async fn raw_exchange(path: &Path, line: &str) -> Response {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(line.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();
        write_half.flush().await.unwrap();
        let mut out = String::new();
        BufReader::new(read_half).read_line(&mut out).await.unwrap();
        serde_json::from_str(out.trim()).unwrap()
    }

    #[tokio::test]
    async fn a_version_mismatch_is_refused_not_half_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;

        let resp = raw_exchange(&path, r#"{"v":99,"command":"/agents"}"#).await;
        assert!(!resp.ok);
        assert!(
            resp.output.contains("version 99"),
            "must name the version it refused, got: {}",
            resp.output
        );
    }

    #[tokio::test]
    async fn garbage_gets_an_error_rather_than_a_dead_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (_handle, path) = bound(&dir).await;

        let resp = raw_exchange(&path, "not json at all").await;
        assert!(!resp.ok);
        assert!(resp.output.contains("malformed"));

        // The listener survives a bad client and still serves the next one.
        let Attempt::Answered(good) = request(&path, &Request::new("/agents".into(), None))
            .await
            .unwrap()
        else {
            panic!("expected the daemon to still be serving");
        };
        assert!(good.ok);
    }

    #[test]
    fn socket_defaults_into_the_state_directory() {
        let config = Config::default();
        assert_eq!(
            socket_path(&config, Some(Path::new("/var/lib/chaz"))),
            Some(PathBuf::from("/var/lib/chaz/control.sock"))
        );
    }

    #[test]
    fn an_explicit_path_wins_and_expands_a_tilde() {
        let config = Config {
            control: Some(ControlConfig {
                enabled: true,
                path: Some("~/chaz.sock".into()),
            }),
            ..Default::default()
        };
        let home = dirs::home_dir().expect("home dir in test env");
        assert_eq!(
            socket_path(&config, Some(Path::new("/var/lib/chaz"))),
            Some(home.join("chaz.sock"))
        );
    }

    #[test]
    fn disabling_control_yields_no_socket() {
        let config = Config {
            control: Some(ControlConfig {
                enabled: false,
                path: Some("/tmp/ignored.sock".into()),
            }),
            ..Default::default()
        };
        assert!(socket_path(&config, Some(Path::new("/var/lib/chaz"))).is_none());
    }

    #[test]
    fn an_omitted_control_block_still_serves() {
        // Regression guard: a derived `Default` on ControlConfig would make
        // `enabled` false and silently turn "said nothing" into "opted out".
        assert!(ControlConfig::default().enabled);
    }

    #[test]
    fn without_a_state_directory_there_is_no_socket() {
        assert!(socket_path(&Config::default(), None).is_none());
    }
}

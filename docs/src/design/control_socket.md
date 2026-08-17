# Daemon Control Socket

**Status:** Implemented (2026-08-16).
**Scope:** `chaz daemon` and `chaz cmd`. Transport bridges (`chaz-matrix`,
`chaz-discord`) do not serve a control socket.

## Problem

`chaz cmd` runs one `/command` non-interactively by building the whole server
against the state directory — the same directory a running daemon has open.
Two processes on one eidetica backend do not observe each other's writes, so
the command has to run with the daemon stopped. Every use of it is documented
that way, and the Matrix end-to-end harness sequences around it by doing all
bring-up before the daemon starts.

That is workable for provisioning and wrong for operations. The commands most
worth running are the ones you want while the peer is up: `/sharing requests`
on a bridge that is waiting for an approval, `/agents` on a daemon that is
misbehaving, `/agent home-status` while a rehost is in flight. Today the only
way to look is to stop the process you are trying to inspect, which changes
the state you were trying to observe.

The command layer is already transport-neutral: bridges parse their own syntax
into a `Command`, call `dispatch`, and render a `CommandOutcome`. Nothing about
the commands needs to change. What is missing is a way to reach a `dispatch`
that runs _inside_ the live process.

## Design

The daemon listens on a unix domain socket. `chaz cmd` prefers it, and falls
back to opening the state directory directly when nothing is listening.

### Transport

A `SOCK_STREAM` unix socket at `<state_dir>/control.sock`.

The state directory is the right anchor because it is already the unit of
identity for a peer: two daemons with different state directories are different
peers and get different sockets, and two daemons sharing one state directory
are a mistake the socket now detects rather than tolerates. An abstract socket
would be Linux-only and would not inherit directory permissions; a TCP port
would be reachable off-host, which is not something this surface should ever be.

### Protocol

One request line in, one response line out, then the connection closes. Both
are single-line JSON.

```json
{"v":1,"command":"/sharing requests","session":null}
{"v":1,"ok":true,"output":"No pending requests."}
```

An error is a well-formed response, not a transport failure:

```json
{
  "v": 1,
  "ok": false,
  "output": "Unknown command: /nope. Type /help for available commands."
}
```

The client exits non-zero on `ok: false`, matching what `chaz cmd` already
promises a calling script.

Rationale for one-shot connections: the command grammar is request/response
with no streaming and no server-initiated messages, so a session-oriented
protocol would buy nothing and cost framing complexity. `v` is checked, and a
mismatch is refused with a message naming both versions — a client from a
different build must fail loudly rather than half-parse.

Rendering happens on the server. `CommandOutcome` is not serializable and
making it so would freeze internal shapes into a wire contract for one
consumer's benefit. The daemon renders exactly as the direct path does, so
output is identical whichever path served it.

### Authorization

**A control socket is an authorization boundary even though it is only
reachable locally.** Anything reachable through it can mint agent share
tickets, approve queued bootstrap requests, invite pubkeys onto agent DBs, and
read session transcripts. That is the full administrative surface of a peer
holding credentials, so the question is not whether to restrict it but what
the restriction actually rests on.

Three layers, in the order they bite:

1. **Peer credentials — the real boundary.** Every accepted connection is
   checked with `SO_PEERCRED`; a peer whose UID differs from the daemon's is
   logged and dropped before the request is read. This is kernel-supplied and
   unforgeable, it cannot be raced, and it does not depend on the filesystem
   being in any particular state.
2. **Socket mode `0600`,** set immediately after bind. `bind(2)` applies the
   process umask, so there is a window in which the socket is more permissive
   than intended; the peer-credential check is what makes that window
   harmless, and the mode is defense in depth rather than the thing being
   relied on.
3. **Directory mode.** The socket lives in the state directory, beside
   `eidetica.db`. If that directory is group- or world-writable the daemon logs
   a warning at bind: someone who can write the directory can unlink the socket
   and bind their own in its place, and no socket permission can prevent that.
   The daemon does not silently `chmod` a directory the operator chose.

Explicitly rejected: a shared-secret token in the request. It would have to be
stored beside the socket, readable by exactly the parties who can already reach
the socket, so it would add a file to protect and no protection.

Same-UID is the whole policy. There is deliberately no per-command
authorization split (read-only vs administrative) — a caller who can reach the
socket can also read and write the state directory directly, so a partial
restriction on the socket would describe a boundary that does not exist.

### Stale sockets and double-bind

A unix socket file survives the process that created it, so bind has to
distinguish "left over from a crash" from "another daemon is running". The
daemon tries to connect to the path first:

- connect succeeds → another daemon holds this state directory; refuse to
  start, naming the path. This is a bug worth failing on: two daemons on one
  backend is the exact condition this feature exists to make visible.
- connect fails with `ECONNREFUSED` → stale file from a crash; unlink and bind.
- path does not exist → bind.

The socket is unlinked on clean shutdown. A crash leaves it, and the next start
sweeps it by the rule above.

### Client behaviour and fallback

`chaz cmd` attempts the socket before building anything:

| Condition                             | Behaviour                                                             |
| ------------------------------------- | --------------------------------------------------------------------- |
| Socket absent, or connect refused     | Fall back to opening the state directory directly (today's behaviour) |
| Connected, request served             | Print the output; exit 0 or 1 on `ok`                                 |
| Connected, then I/O or protocol error | **Fail.** Do not fall back                                            |

The last row is the load-bearing one. A daemon that answered the connect is
holding the backend, so falling back would open a second view of it and produce
an answer computed against writes the daemon has not flushed and cannot see —
a wrong answer that looks like a right one. Failing names the real problem.

`--local` forces the direct path, for the case where the daemon is up and you
deliberately want to inspect what is on disk.

Trying the socket first also means the fast path skips the entire server build:
no eidetica open, no agent bootstrap, no sync. `chaz cmd` against a live daemon
answers in the time it takes to round-trip a line.

### Session resolution

`chaz cmd --session NAME` finds-or-creates that session, on either path.

Without `--session`, the two paths differ, deliberately:

- **Direct path:** a fresh ephemeral session per invocation, as today.
- **Socket path:** a single stable session named `control`, reused across
  invocations.

Creating a throwaway session per invocation is tolerable for a bring-up script
that runs a handful of commands against a cold state directory. It is not
tolerable for a surface meant to be used routinely against a live peer, where
it would grow the session catalog by one row per `/agents`. Peer-scoped
commands — which is nearly all of them, and all the ones motivating this — do
not read the session at all. Session-scoped commands (`/info`, `/share`,
`/print`) need `--session` to address anything meaningful on either path.

### Configuration

```yaml
control:
  enabled: true # default; false disables the listener entirely
  path: /run/user/1000/chaz-control.sock # default: <state_dir>/control.sock
```

Enabled by default, because a feature that must be switched on is a feature
that is off when you need it — and the moment you need this one is when
something is already wrong. `enabled: false` exists so an operator who does not
want the surface can remove it rather than merely constrain it.

## What this is not

- **Not a remote management API.** Unix socket only. No TCP, no TLS, no
  authentication beyond the peer UID, and none of those should be added to
  this surface — a remote surface needs its own design with its own identity
  model.
- **Not a general RPC layer.** It carries the slash-command grammar and
  nothing else. Structured/machine-readable output is a property of individual
  commands (see `chaz usage --json`), not of the transport.
- **Not served by the TUI.** The TUI holds the same backend and has the same
  problem, but it also has an operator sitting at it who can type the command.
  Serving from both would mean two processes racing for one path.
- **Not a streaming channel.** One request, one response. Long-running verbs
  (`/agent import` against a slow peer) hold the connection until they finish.

## Trade-offs accepted

- **Concurrency.** Connections are handled concurrently, each in its own task,
  with no serialization between them. The daemon already runs the agent loop,
  sync, and the routine engine against the same databases, so control commands
  are not a new class of concurrent writer.
- **Reading a request is bounded; running it is not.** A client that connects
  and never sends is dropped after a short timeout. A command that legitimately
  takes minutes is allowed to.
- **Output is text.** A caller wanting structure parses it, exactly as it does
  today with `chaz cmd`.

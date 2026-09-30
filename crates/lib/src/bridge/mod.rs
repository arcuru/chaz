//! Cross-crate bridge surface.
//!
//! The [`Bridge`] trait, [`ApprovalExchange`] struct, and
//! [`ApprovalDecision`] enum live in the library so runtime / security /
//! server code can reference them. Concrete bridge implementations
//! (Matrix, TUI, CLI) live in the binary crate.
//!
//! The session DB *is* the conversation; a bridge is a pure bidirectional
//! translator between one session DB and one transport. The translate
//! helpers below ([`inbound_user_entry`], [`render_outbound`]) and the
//! durable outbound path in [`delivery`] are the transport-generic half of
//! that contract — an external bridge binary (`chaz-discord`, …) links them
//! rather than re-deriving the DB↔surface invariants. The Matrix bridge is
//! itself written on top of them, so they stay honest about being
//! transport-agnostic.

use crate::server::Server;
use crate::session::{EntryRouting, EntryType, SessionEntry, TransportRef};
use crate::tool::ToolApprovalInfo;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Durable per-binding outbound delivery for transport bridges.
pub mod delivery;
/// Shared outbound helpers for transport bridge implementations and user interaction.
pub mod outbound;
/// Shared startup for standalone bridge executables.
pub mod process;

/// Trait for transport bridges (Matrix, TUI, etc.)
///
/// A bridge owns a transport connection and translates platform events
/// into session database entries. The server processes entries via
/// callbacks and delivers responses through the response channel.
pub trait Bridge {
    fn run(
        self,
        server: Arc<Server>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// An approval exchange: the runtime sends tool info and a channel to receive the decision.
pub struct ApprovalExchange {
    pub info: ToolApprovalInfo,
    pub decision_tx: oneshot::Sender<ApprovalDecision>,
}

/// Outcome of a tool approval request.
///
/// Three of these are a human's answer; [`ApprovalDecision::TimedOut`] is the
/// daemon's, written when nobody answered before the request's own deadline.
#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// Approve this and all remaining tool calls this turn
    ApproveAll,
    /// Nobody answered before the request's deadline. Not an approval, so the
    /// tool does not run — but distinct from a deliberate `Deny`, because the
    /// transcript and the transport both want to say which one happened.
    TimedOut,
}

impl ApprovalDecision {
    /// Stable wire token for an [`EntryType::ApprovalDecision`] payload.
    pub fn as_wire(&self) -> &'static str {
        match self {
            ApprovalDecision::Approve => "approve",
            ApprovalDecision::Deny => "deny",
            ApprovalDecision::ApproveAll => "approve_all",
            ApprovalDecision::TimedOut => "timed_out",
        }
    }

    /// Parse a wire token back into a decision. Unknown tokens are `None`.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(ApprovalDecision::Approve),
            "deny" => Some(ApprovalDecision::Deny),
            "approve_all" => Some(ApprovalDecision::ApproveAll),
            "timed_out" => Some(ApprovalDecision::TimedOut),
            _ => None,
        }
    }

    /// Whether the tool may run. Only an explicit human approval clears it.
    pub fn is_approval(&self) -> bool {
        matches!(
            self,
            ApprovalDecision::Approve | ApprovalDecision::ApproveAll
        )
    }
}

// ---------------------------------------------------------------------------
// Tool-approval proxy over the session DB (transport-generic protocol)
// ---------------------------------------------------------------------------
//
// A dumb bridge runs no agent, so the in-process approval callback can't reach
// the human on the transport. Instead the daemon's runtime blocks on
// `request_approval`; its per-session proxy writes an `ApprovalRequest` entry
// into the session DB; the bridge renders it, captures the human's reaction,
// and writes an `ApprovalDecision` entry; the proxy matches it back by
// `request_id` and unblocks the runtime. Both entry kinds are control records:
// excluded from LLM context and never delivered as chat. The payloads below are
// the contract both peers serialize into `SessionEntry::content`.

/// How long a tool-approval request may sit unanswered.
///
/// Parsed from the top-level `approvals:` block of the shared YAML config and
/// read by the daemon alone — it owns the blocked runtime, so it owns the
/// clock, and no bridge ever compares one. Each request the daemon writes
/// carries this ceiling in [`ApprovalRequestPayload::timeout_secs`] so a bridge
/// can tell its channel how long the prompt has.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ApprovalsConfig {
    /// Seconds a request may stay pending before it is denied.
    #[serde(default = "default_approval_timeout_secs")]
    pub timeout: u64,
}

fn default_approval_timeout_secs() -> u64 {
    300
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            timeout: default_approval_timeout_secs(),
        }
    }
}

impl ApprovalsConfig {
    /// The configured ceiling as a duration.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout)
    }
}

/// The ceiling an absent `approvals:` block implies.
pub fn approval_timeout_or_default(config: Option<&ApprovalsConfig>) -> std::time::Duration {
    config.cloned().unwrap_or_default().timeout()
}

/// JSON payload of an [`EntryType::ApprovalRequest`] session entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRequestPayload {
    /// Correlates the request with its [`ApprovalDecisionPayload`]. A fresh
    /// UUID per request.
    pub request_id: String,
    pub tool_name: String,
    /// `RiskLevel` rendered for display (`{:?}`); informational only.
    pub risk_level: String,
    /// Redacted argument preview the human sees before deciding.
    pub arguments_display: String,
    /// Seconds the daemon will wait for an answer. Travels with the request so
    /// a bridge can render the window ("expires in 5 minutes") off the request
    /// it is showing rather than out of its own config. Advisory only: nothing
    /// a bridge does is gated on it, because the daemon records the outcome and
    /// [`resolved_decisions`] is what settles a request.
    #[serde(default = "default_approval_timeout_secs")]
    pub timeout_secs: u64,
}

/// JSON payload of an [`EntryType::ApprovalDecision`] session entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalDecisionPayload {
    pub request_id: String,
    /// One of [`ApprovalDecision::as_wire`].
    pub decision: String,
}

/// Build a fresh `ApprovalRequest` session entry from a runtime approval ask.
/// Returns the generated `request_id` alongside the entry so the caller can
/// track the pending exchange.
pub fn approval_request_entry(
    agent_sender: &str,
    info: &ToolApprovalInfo,
    timeout: std::time::Duration,
) -> (String, SessionEntry) {
    let request_id = uuid::Uuid::new_v4().to_string();
    let payload = ApprovalRequestPayload {
        request_id: request_id.clone(),
        tool_name: info.name.clone(),
        risk_level: format!("{:?}", info.risk_level),
        arguments_display: info.arguments_display.clone(),
        timeout_secs: timeout.as_secs(),
    };
    let entry = SessionEntry {
        sender: agent_sender.to_string(),
        content: serde_json::to_string(&payload).unwrap_or_default(),
        timestamp: Utc::now(),
        entry_type: EntryType::ApprovalRequest,
        metadata: None,
        routing: None,
    };
    (request_id, entry)
}

/// Build an `ApprovalDecision` session entry resolving `request_id`.
pub fn approval_decision_entry(
    approver: &str,
    request_id: &str,
    decision: ApprovalDecision,
) -> SessionEntry {
    let payload = ApprovalDecisionPayload {
        request_id: request_id.to_string(),
        decision: decision.as_wire().to_string(),
    };
    SessionEntry {
        sender: approver.to_string(),
        content: serde_json::to_string(&payload).unwrap_or_default(),
        timestamp: Utc::now(),
        entry_type: EntryType::ApprovalDecision,
        metadata: None,
        routing: None,
    }
}

/// Parse an [`EntryType::ApprovalRequest`] entry's payload, or `None` if it is
/// a different entry kind or malformed.
pub fn parse_approval_request(entry: &SessionEntry) -> Option<ApprovalRequestPayload> {
    (entry.entry_type == EntryType::ApprovalRequest)
        .then(|| serde_json::from_str(&entry.content).ok())
        .flatten()
}

/// Parse an [`EntryType::ApprovalDecision`] entry's payload, or `None`.
pub fn parse_approval_decision(entry: &SessionEntry) -> Option<ApprovalDecisionPayload> {
    (entry.entry_type == EntryType::ApprovalDecision)
        .then(|| serde_json::from_str(&entry.content).ok())
        .flatten()
}

/// Map every resolved `request_id` to the decision the daemon acted on.
///
/// A request can collect more than one decision entry: the daemon writes
/// `TimedOut` when it stops waiting, and a human's answer can still land
/// afterwards from a bridge that had no way to know. Resolution has to name the
/// same outcome the daemon already gave the runtime, or the session would
/// record an approval for a call that never ran.
///
/// Two rules, neither of which consults a clock:
///
/// 1. **`TimedOut` absorbs.** The daemon writes it only after claiming the
///    pending slot, and it claims that slot under the same lock a landing
///    decision does — so exactly one of the two wins, and a `TimedOut` entry
///    existing is equivalent to "the runtime was told `TimedOut`". Whatever
///    else is written for that request, before or after, the daemon acted on
///    the timeout.
/// 2. **Otherwise the first answer wins.** Two bridges rendering one session
///    can each answer the same prompt; the daemon took whichever reached its
///    resolver first and ignored the rest. First-by-entry-order is the
///    deterministic stand-in, and every candidate is a genuine human answer.
///
/// Entry timestamps are deliberately not used to order this. They are one
/// peer's wall clock, and the entry that reached the daemon first is not
/// necessarily the one stamped earliest.
pub fn resolved_decisions(entries: &[SessionEntry]) -> HashMap<String, ApprovalDecision> {
    let mut resolved: HashMap<String, ApprovalDecision> = HashMap::new();
    for payload in entries.iter().filter_map(parse_approval_decision) {
        let Some(decision) = ApprovalDecision::from_wire(&payload.decision) else {
            continue;
        };
        match resolved.entry(payload.request_id) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(decision);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                // Rule 1: a timeout overrides an answer already recorded.
                // Rule 2: anything else leaves the first answer standing.
                if decision == ApprovalDecision::TimedOut {
                    slot.insert(decision);
                }
            }
        }
    }
    resolved
}

/// Approval requests still worth prompting for: no decision yet, and not
/// already posted this process (`seen`). The bridge renders these and adds
/// their `request_id`s to `seen` so a request is prompted exactly once even as
/// the session DB churns.
///
/// A request the daemon has given up on carries a `TimedOut` decision, so the
/// decided-filter already excludes it — a bridge reconnecting after downtime
/// re-renders only what is genuinely still open, without comparing any clocks.
pub fn unrendered_approval_requests(
    entries: &[SessionEntry],
    seen: &HashSet<String>,
) -> Vec<ApprovalRequestPayload> {
    let decided: HashSet<String> = entries
        .iter()
        .filter_map(parse_approval_decision)
        .map(|p| p.request_id)
        .collect();
    entries
        .iter()
        .filter_map(parse_approval_request)
        .filter(|p| !decided.contains(&p.request_id) && !seen.contains(&p.request_id))
        .collect()
}

/// How long a prompt has, rendered for a human: "5 minutes", "90 seconds".
///
/// A duration, never a clock time — the window is the one fact about the
/// deadline a bridge can state without knowing what time the daemon thinks it
/// is.
pub fn render_approval_window(timeout_secs: u64) -> String {
    match timeout_secs {
        0 => "immediately".to_string(),
        s if s % 60 == 0 && s >= 60 => match s / 60 {
            1 => "in 1 minute".to_string(),
            m => format!("in {m} minutes"),
        },
        1 => "in 1 second".to_string(),
        s => format!("in {s} seconds"),
    }
}

/// The decision already recorded for `request_id`, if any.
///
/// What a bridge checks before writing an answer, in place of asking whether
/// the deadline has passed: the question is not "is it late" but "did the
/// daemon already close this", and the session tree answers that directly.
/// Racing it is harmless — [`resolved_decisions`] resolves to the daemon's
/// outcome whether or not the answer got written.
pub fn existing_decision(entries: &[SessionEntry], request_id: &str) -> Option<ApprovalDecision> {
    resolved_decisions(entries).remove(request_id)
}

/// Requests the daemon gave up on and the transport has not yet mentioned: a
/// `TimedOut` decision whose `request_id` is absent from `announced`.
///
/// Returns the original request, because the notice names the tool that
/// expired. A request whose decision arrives without a matching request entry
/// (a truncated or partially-synced session) is skipped — there is nothing to
/// name.
pub fn unannounced_timeouts(
    entries: &[SessionEntry],
    announced: &HashSet<String>,
) -> Vec<ApprovalRequestPayload> {
    let timed_out: HashSet<String> = entries
        .iter()
        .filter_map(parse_approval_decision)
        .filter(|p| ApprovalDecision::from_wire(&p.decision) == Some(ApprovalDecision::TimedOut))
        .map(|p| p.request_id)
        .collect();
    entries
        .iter()
        .filter_map(parse_approval_request)
        .filter(|p| timed_out.contains(&p.request_id) && !announced.contains(&p.request_id))
        .collect()
}

/// The prompt an untargeted `approve`/`deny` answers in `channel`: the oldest
/// one still open there, by the order the bridge posted them.
///
/// A bridge keeps its posted prompts in a map keyed by the transport's message
/// id, and that map's iteration order is arbitrary — with two prompts open in
/// one channel, picking the first match answers whichever the map happened to
/// yield, so the same words resolve a different tool call each time. `seq` is
/// the bridge's post counter, and the lowest one is the request the human has
/// been looking at longest. Prompts in other channels are never candidates.
///
/// Pure, so the ordering rule is testable without a live transport.
pub fn oldest_pending<K, C: PartialEq>(
    prompts: impl IntoIterator<Item = (K, C, u64)>,
    channel: &C,
) -> Option<K> {
    prompts
        .into_iter()
        .filter(|(_, c, _)| c == channel)
        .min_by_key(|(_, _, seq)| *seq)
        .map(|(key, _, _)| key)
}

// ---------------------------------------------------------------------------
// Reconcile / translate helpers (transport-generic)
// ---------------------------------------------------------------------------

/// Build the inbound user entry for a message received on a transport.
///
/// Stamps the invariants every bridge ingester must get right: an
/// `EntryType::Message`, the receipt timestamp, and an [`EntryRouting`]
/// `source` recording which `(transport, login_id, channel)` it arrived on
/// and from whom. An agent's reply is routed back to this channel by
/// resolving that `source`, so a bridge author writes one call here instead
/// of re-deriving the routing shape.
pub fn inbound_user_entry(
    transport: &str,
    login_id: &str,
    channel: &str,
    sender: &str,
    sender_display: Option<String>,
    content: &str,
    message_id: Option<String>,
) -> SessionEntry {
    SessionEntry {
        sender: sender.to_string(),
        content: content.to_string(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: Some(EntryRouting {
            source: Some(TransportRef {
                transport: transport.to_string(),
                login_id: login_id.to_string(),
                channel: channel.to_string(),
                sender: Some(sender.to_string()),
                sender_display,
                message_id,
            }),
            ..Default::default()
        }),
    }
}

/// Render an agent's session write for delivery to a transport channel.
///
/// A login belongs to one agent. The owning agent speaks as the channel's
/// transport identity, so its writes go out plain; any other agent writing
/// into this session is a guest, shown with an `[AgentName]` prefix so
/// readers can tell speakers apart under the single identity.
pub fn render_outbound(owning_agent: &str, sender: &str, content: &str) -> String {
    if sender == owning_agent {
        content.to_string()
    } else {
        format!("[{sender}] {content}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    fn entry(sender: &str, content: &str, ts: i64, entry_type: EntryType) -> SessionEntry {
        SessionEntry {
            sender: sender.to_string(),
            content: content.to_string(),
            timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
            entry_type,
            metadata: None,
            routing: None,
        }
    }

    #[test]
    fn approve_answers_the_oldest_prompt_in_the_channel() {
        // Posted second, first, third — as a HashMap would hand them over.
        let prompts = [("m2", "chan", 1), ("m1", "chan", 0), ("m3", "chan", 2)];
        assert_eq!(oldest_pending(prompts, &"chan"), Some("m1"));
    }

    #[test]
    fn a_prompt_in_another_channel_is_never_answered() {
        // The only older prompt belongs to a different channel.
        let prompts = [("elsewhere", "other", 0), ("here", "chan", 1)];
        assert_eq!(oldest_pending(prompts, &"chan"), Some("here"));
        assert_eq!(oldest_pending(prompts, &"empty"), None);
        assert_eq!(oldest_pending([] as [(&str, &str, u64); 0], &"chan"), None);
    }

    #[test]
    fn owning_agent_writes_go_out_plain() {
        assert_eq!(
            render_outbound("chaz", "chaz", "clear skies"),
            "clear skies"
        );
    }

    #[test]
    fn guest_agent_writes_are_prefixed() {
        assert_eq!(
            render_outbound("chaz", "scout", "logs look clean"),
            "[scout] logs look clean"
        );
    }

    #[test]
    fn prefix_is_exact_name_match() {
        // A different-cased or partial name is a distinct agent: still a guest.
        assert_eq!(render_outbound("chaz", "Chaz", "hi"), "[Chaz] hi");
    }

    #[test]
    fn inbound_entry_stamps_transport_provenance() {
        let e = inbound_user_entry(
            "discord",
            "login-1",
            "chan-42",
            "@user:x",
            None,
            "hello there",
            Some("msg-7".to_string()),
        );
        assert_eq!(e.entry_type, EntryType::Message);
        assert_eq!(e.sender, "@user:x");
        assert_eq!(e.content, "hello there");
        let src = e.routing.unwrap().source.unwrap();
        assert_eq!(src.transport, "discord");
        assert_eq!(src.login_id, "login-1");
        assert_eq!(src.channel, "chan-42");
        assert_eq!(src.sender.as_deref(), Some("@user:x"));
        assert_eq!(src.message_id.as_deref(), Some("msg-7"));
    }

    fn approval_info(name: &str) -> ToolApprovalInfo {
        ToolApprovalInfo {
            name: name.to_string(),
            arguments_display: "{\"path\":\"/etc\"}".to_string(),
            risk_level: crate::tool::RiskLevel::High,
        }
    }

    /// The ceiling every test below hands to a request. Long enough that
    /// nothing expires mid-test unless the test says so.
    const CEILING: std::time::Duration = std::time::Duration::from_secs(300);

    #[test]
    fn approval_request_and_decision_round_trip() {
        let (request_id, req) = approval_request_entry("chaz", &approval_info("shell"), CEILING);
        assert_eq!(req.entry_type, EntryType::ApprovalRequest);
        assert_eq!(req.sender, "chaz");
        let parsed = parse_approval_request(&req).expect("request payload");
        assert_eq!(parsed.request_id, request_id);
        assert_eq!(parsed.tool_name, "shell");
        assert_eq!(parsed.risk_level, "High");
        assert_eq!(parsed.timeout_secs, 300);

        let dec = approval_decision_entry("@human:x", &request_id, ApprovalDecision::ApproveAll);
        assert_eq!(dec.entry_type, EntryType::ApprovalDecision);
        let dp = parse_approval_decision(&dec).expect("decision payload");
        assert_eq!(dp.request_id, request_id);
        assert_eq!(dp.decision, "approve_all");

        // The proxy's resolver view maps the request_id back to the decision.
        let resolved = resolved_decisions(&[req, dec]);
        assert_eq!(
            resolved.get(&request_id),
            Some(&ApprovalDecision::ApproveAll)
        );
    }

    #[test]
    fn wrong_entry_kind_parses_to_none() {
        let msg = entry("chaz", "hi", 1, EntryType::Message);
        assert!(parse_approval_request(&msg).is_none());
        assert!(parse_approval_decision(&msg).is_none());
    }

    #[test]
    fn unrendered_skips_decided_and_seen_requests() {
        let (rid_open, open) = approval_request_entry("chaz", &approval_info("a"), CEILING);
        let (rid_done, done) = approval_request_entry("chaz", &approval_info("b"), CEILING);
        let (rid_seen, seen_req) = approval_request_entry("chaz", &approval_info("c"), CEILING);
        let done_decision = approval_decision_entry("@h:x", &rid_done, ApprovalDecision::Deny);

        let entries = vec![open, done, seen_req, done_decision];
        let mut seen = HashSet::new();
        seen.insert(rid_seen.clone());

        let got: Vec<String> = unrendered_approval_requests(&entries, &seen)
            .into_iter()
            .map(|p| p.request_id)
            .collect();
        // `rid_done` has a decision, `rid_seen` is already rendered → only the
        // genuinely-open request surfaces.
        assert_eq!(got, vec![rid_open]);
        assert!(!got.contains(&rid_done));
        assert!(!got.contains(&rid_seen));
    }

    /// A bridge reconnecting after downtime must not re-post a prompt the
    /// daemon already gave up on. No clock is involved: the daemon's `TimedOut`
    /// entry is a decision, and decided requests are not rendered.
    #[test]
    fn unrendered_skips_a_request_the_daemon_timed_out() {
        let (rid, req) = approval_request_entry("chaz", &approval_info("shell"), CEILING);
        let seen = HashSet::new();

        // Still open before the daemon gives up — without this the assertion
        // below would pass against a helper that rendered nothing.
        assert_eq!(
            unrendered_approval_requests(std::slice::from_ref(&req), &seen).len(),
            1
        );

        let entries = vec![
            req,
            approval_decision_entry("system", &rid, ApprovalDecision::TimedOut),
        ];
        assert!(unrendered_approval_requests(&entries, &seen).is_empty());
    }

    /// The bridge's cue to tell its channel: a `TimedOut` decision it has not
    /// announced yet, resolved back to the request so the notice can name the
    /// tool. A plain deny is the human's own action and needs no notice.
    #[test]
    fn unannounced_timeouts_surface_only_expiries() {
        let (rid_out, timed_out) = approval_request_entry("chaz", &approval_info("a"), CEILING);
        let (rid_deny, denied) = approval_request_entry("chaz", &approval_info("b"), CEILING);
        let (_rid_open, open) = approval_request_entry("chaz", &approval_info("c"), CEILING);
        let entries = vec![
            timed_out,
            denied,
            open,
            approval_decision_entry("system", &rid_out, ApprovalDecision::TimedOut),
            approval_decision_entry("@h:x", &rid_deny, ApprovalDecision::Deny),
        ];

        let announced = HashSet::new();
        let got: Vec<String> = unannounced_timeouts(&entries, &announced)
            .into_iter()
            .map(|p| p.request_id)
            .collect();
        assert_eq!(got, vec![rid_out.clone()]);

        // Announced once, never again, however often the session churns.
        let announced: HashSet<String> = [rid_out].into_iter().collect();
        assert!(unannounced_timeouts(&entries, &announced).is_empty());
    }

    #[test]
    fn decision_wire_tokens_round_trip() {
        for d in [
            ApprovalDecision::Approve,
            ApprovalDecision::Deny,
            ApprovalDecision::ApproveAll,
            ApprovalDecision::TimedOut,
        ] {
            assert_eq!(ApprovalDecision::from_wire(d.as_wire()), Some(d));
        }
        assert_eq!(ApprovalDecision::from_wire("garbage"), None);
    }

    /// The guarantee that lets a bridge answer without checking any clock: an
    /// approval landing after the daemon gave up does not resolve the request.
    /// The daemon told the runtime `TimedOut` and the tool never ran, so this
    /// is what the session has to say too — in either write order, since the
    /// bridge's entry may be stamped earlier than the daemon's and still have
    /// arrived later.
    #[test]
    fn a_timeout_absorbs_an_answer_written_alongside_it() {
        let (rid, req) = approval_request_entry("chaz", &approval_info("shell"), CEILING);
        let timed_out = approval_decision_entry("system", &rid, ApprovalDecision::TimedOut);
        let approved = approval_decision_entry("@h:x", &rid, ApprovalDecision::Approve);

        for entries in [
            vec![req.clone(), timed_out.clone(), approved.clone()],
            vec![req.clone(), approved.clone(), timed_out.clone()],
        ] {
            assert_eq!(
                resolved_decisions(&entries).get(&rid),
                Some(&ApprovalDecision::TimedOut),
                "the daemon's outcome must win regardless of entry order"
            );
            assert_eq!(
                existing_decision(&entries, &rid),
                Some(ApprovalDecision::TimedOut)
            );
        }
    }

    /// Without a timeout in play, the first answer stands. Two bridges on one
    /// session can both answer a prompt; the daemon acted on one of them and
    /// ignored the rest, so a later entry must not rewrite the outcome.
    #[test]
    fn a_later_answer_does_not_overwrite_the_first() {
        let (rid, req) = approval_request_entry("chaz", &approval_info("shell"), CEILING);
        let entries = vec![
            req,
            approval_decision_entry("@first:x", &rid, ApprovalDecision::Deny),
            approval_decision_entry("@second:x", &rid, ApprovalDecision::Approve),
        ];
        assert_eq!(
            resolved_decisions(&entries).get(&rid),
            Some(&ApprovalDecision::Deny)
        );
    }

    /// An unanswered request has nothing on record, which is what tells a
    /// bridge to go ahead and write its answer.
    #[test]
    fn an_open_request_has_no_existing_decision() {
        let (rid, req) = approval_request_entry("chaz", &approval_info("shell"), CEILING);
        assert_eq!(existing_decision(&[req], &rid), None);
    }

    /// The window is rendered as a duration, never as a clock time — it is the
    /// one thing about the deadline a bridge can state without knowing what
    /// time the daemon thinks it is.
    #[test]
    fn the_approval_window_renders_as_a_duration() {
        assert_eq!(render_approval_window(300), "in 5 minutes");
        assert_eq!(render_approval_window(60), "in 1 minute");
        assert_eq!(render_approval_window(90), "in 90 seconds");
        assert_eq!(render_approval_window(1), "in 1 second");
        assert_eq!(render_approval_window(0), "immediately");
    }

    /// Only a human's approval runs the tool. A timeout is not a quiet yes.
    #[test]
    fn only_explicit_approvals_run_the_tool() {
        assert!(ApprovalDecision::Approve.is_approval());
        assert!(ApprovalDecision::ApproveAll.is_approval());
        assert!(!ApprovalDecision::Deny.is_approval());
        assert!(!ApprovalDecision::TimedOut.is_approval());
    }

    /// A request written before the ceiling travelled on the entry still
    /// parses, and falls back to the default rather than to an instant expiry.
    #[test]
    fn a_request_without_a_ceiling_falls_back_to_the_default() {
        let payload: ApprovalRequestPayload = serde_json::from_str(
            r#"{"request_id":"r1","tool_name":"shell","risk_level":"High","arguments_display":"x"}"#,
        )
        .expect("a pre-ceiling request payload must still parse");
        assert_eq!(payload.timeout_secs, 300);
    }
}

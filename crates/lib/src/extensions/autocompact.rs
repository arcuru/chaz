//! Opt-in automatic compaction policy and the `/autocompact` command.
//!
//! The extension owns only policy: whether autocompaction is on for a
//! session, the trigger size, how much recent history to keep and the summary
//! output ceiling. Source selection, background preparation, validation and
//! safe application are native (see `runtime::autocompact`); this extension
//! never edits a request.

use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
    read_settings_checked, write_settings,
};
use crate::runtime::autocompact::{DEFAULT_TRIGGER_PERCENT, Policy, Status};
use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub const NAME: &str = "autocompact";

/// Per-agent last-preparation status, kept apart from the settings so the
/// runtime never rewrites operator choices.
const STATUS_STORE: &str = "autocompact_status";

/// Pi's `keepRecentTokens` default.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
/// Default ceiling for one summary response, including reasoning tokens on
/// providers that count them against the output limit.
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8_192;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Model-facing request size that starts compaction. Unset means
    /// 80% of the current model's context window.
    pub trigger_tokens: Option<u64>,
    pub keep_recent_tokens: u64,
    pub summary_max_tokens: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_tokens: None,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        if self.trigger_tokens == Some(0) {
            return Err("trigger_tokens must be positive".into());
        }
        if self.keep_recent_tokens == 0 {
            return Err("keep_recent_tokens must be positive".into());
        }
        if self.summary_max_tokens == 0 {
            return Err("summary_max_tokens must be positive".into());
        }
        Ok(())
    }
}

async fn read(db: &Database) -> anyhow::Result<Settings> {
    let value = read_settings_checked(db, NAME).await?;
    let settings: Settings = if value.as_object().is_some_and(|o| o.is_empty()) {
        Settings::default()
    } else {
        serde_json::from_value(value)?
    };
    settings.validate().map_err(anyhow::Error::msg)?;
    Ok(settings)
}

/// The turn's effective policy, or `None` when autocompaction is off.
pub(crate) async fn policy(db: &Database) -> anyhow::Result<Option<Policy>> {
    let settings = read(db).await?;
    if !settings.enabled {
        return Ok(None);
    }
    let size = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
    Ok(Some(Policy {
        trigger_tokens: settings.trigger_tokens.map(size),
        keep_recent_tokens: size(settings.keep_recent_tokens),
        summary_max_tokens: settings.summary_max_tokens,
    }))
}

pub(crate) async fn record_status(
    db: &Database,
    agent: &str,
    status: &Status,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STATUS_STORE).await?;
    store
        .set_string(agent, serde_json::to_string(status)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn read_status(db: &Database, agent: &str) -> anyhow::Result<Option<Status>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STATUS_STORE).await?;
    match store.get_string(agent).await {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Parse a token count: `27000`, `27k` or `27K` (thousands). Rejects zero,
/// negative, fractional, malformed and overflowing values.
pub fn parse_length(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let (digits, scale) = match input.strip_suffix(['k', 'K']) {
        Some(digits) => (digits, 1_000u64),
        None => (input, 1),
    };
    let invalid =
        || format!("invalid length '{input}': use a positive token count like 27000 or 27k");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let value = digits
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(scale))
        .filter(|v| usize::try_from(*v).is_ok())
        .ok_or_else(|| format!("length '{input}' is too large"))?;
    if value == 0 {
        return Err(invalid());
    }
    Ok(value)
}

pub struct AutocompactExtension;

impl Extension for AutocompactExtension {
    fn name(&self) -> &'static str {
        NAME
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Command]
    }
    fn default_settings(&self) -> Value {
        serde_json::to_value(Settings::default()).expect("settings serialize")
    }
    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: NAME.into(),
            extension_ref: ExtensionRef::builtin(NAME),
            supported_hooks: self.supported_hooks().to_vec(),
            required_capabilities: vec![
                crate::extension::caps::CapabilityRequest::CommandRegistration,
            ],
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }
    fn instantiate<'a>(&'a self, _ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(Instance {
                manifest: AutocompactExtension.manifest(),
            }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct Instance {
    manifest: ExtensionManifest,
}

impl ExtensionInstance for Instance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn commands(&self) -> Vec<(String, Arc<dyn ExtensionCommand>)> {
        vec![(NAME.into(), Arc::new(AutocompactCommand))]
    }
}

const USAGE: &str = "Usage: /autocompact [<length> | on | off]  (length: 27000 or 27k)";

struct AutocompactCommand;

impl AutocompactCommand {
    async fn run(&self, args: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
        let db = ctx.session.lock().await.database().clone();
        let mut settings = match read(&db).await {
            Ok(settings) => settings,
            // A corrupt blob is reported, and only an explicit new value
            // below replaces it.
            Err(error) if args.is_empty() => {
                return ExtensionCommandOutcome::Error(format!(
                    "autocompact settings are unreadable: {error}"
                ));
            }
            Err(_) => Settings::default(),
        };
        let message = match args {
            "" => return ExtensionCommandOutcome::Text(describe(&settings, &db, ctx).await),
            "off" => {
                settings.enabled = false;
                "Autocompaction off. Existing history and manual /compact are unchanged."
                    .to_string()
            }
            "on" => {
                settings.enabled = true;
                settings.trigger_tokens = None;
                format!(
                    "Autocompaction on at {DEFAULT_TRIGGER_PERCENT}% of the model context window."
                )
            }
            length => match parse_length(length) {
                Ok(tokens) => {
                    settings.enabled = true;
                    settings.trigger_tokens = Some(tokens);
                    format!("Autocompaction on at {tokens} tokens of model-facing context.")
                }
                Err(error) => return ExtensionCommandOutcome::Error(format!("{error}\n{USAGE}")),
            },
        };
        let value = serde_json::to_value(&settings).expect("settings serialize");
        match write_settings(&db, NAME, value).await {
            Ok(()) => ExtensionCommandOutcome::Text(message),
            Err(error) => ExtensionCommandOutcome::Error(format!(
                "failed to save autocompact settings: {error}"
            )),
        }
    }
}

async fn describe(settings: &Settings, db: &Database, ctx: &HookContext) -> String {
    let trigger = match settings.trigger_tokens {
        Some(tokens) => format!("{tokens} tokens"),
        None => format!("{DEFAULT_TRIGGER_PERCENT}% of the model context window"),
    };
    let mut lines = vec![format!(
        "Autocompaction: {} (trigger {trigger}; keep ~{} recent tokens; summary ceiling {} tokens)",
        if settings.enabled { "on" } else { "off" },
        settings.keep_recent_tokens,
        settings.summary_max_tokens,
    )];
    match read_status(db, &ctx.agent_name).await {
        Ok(Some(status)) => {
            let mut line = format!("Last: {} — {}", status.state, status.detail);
            if let (Some(before), Some(after)) = (status.estimated_before, status.estimated_after) {
                line.push_str(&format!(" (estimated ~{before} -> ~{after} tokens)"));
            }
            lines.push(line);
            for usage in &status.summary_usage {
                let (input, output) = crate::session::usage::reported_fields(Some(usage));
                let count = |known, value: u32| {
                    if known {
                        value.to_string()
                    } else {
                        "unknown".into()
                    }
                };
                lines.push(format!(
                    "Summary call on {}: {} input / {} output tokens (provider-reported where available)",
                    usage.model, count(input, usage.usage.prompt_tokens), count(output, usage.usage.completion_tokens)
                ));
            }
        }
        Ok(None) => {}
        Err(error) => lines.push(format!("Last status unreadable: {error}")),
    }
    lines.push(USAGE.into());
    lines.join("\n")
}

impl ExtensionCommand for AutocompactCommand {
    fn description(&self) -> &'static str {
        "Automatically summarize older context when a request grows past a size"
    }
    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(self.run(args.trim(), ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_thousands_lengths() {
        assert_eq!(parse_length("27000"), Ok(27_000));
        assert_eq!(parse_length("27k"), Ok(27_000));
        assert_eq!(parse_length("27K"), Ok(27_000));
        assert_eq!(parse_length(" 128k "), Ok(128_000));
    }

    #[test]
    fn rejects_invalid_nonpositive_and_overflowing_lengths() {
        for bad in [
            "", "k", "0", "0k", "-5", "-5k", "2.5k", "27m", "abc", "27 k", "+27", "1e5",
        ] {
            assert!(parse_length(bad).is_err(), "accepted {bad:?}");
        }
        assert!(parse_length("18446744073709551615k").is_err());
        assert!(parse_length("99999999999999999999999").is_err());
    }

    async fn invoke(
        args: &str,
        session: &Arc<tokio::sync::Mutex<crate::session::Session>>,
    ) -> Result<String, String> {
        let ctx = HookContext {
            agent_name: "a".into(),
            model: None,
            call_depth: 0,
            session: session.clone(),
            active_extensions: Default::default(),
            routine_engine: None,
        };
        match AutocompactCommand.run(args, &ctx).await {
            ExtensionCommandOutcome::Text(t) => Ok(t),
            ExtensionCommandOutcome::Error(e) => Err(e),
        }
    }

    #[tokio::test]
    async fn command_sets_trigger_toggles_and_reports() {
        let (_instance, session) = crate::test_support::fresh_session().await;
        let db = session.lock().await.database().clone();
        assert_eq!(policy(&db).await.unwrap(), None, "off by default");
        assert!(
            invoke("27k", &session)
                .await
                .unwrap()
                .contains("27000 tokens")
        );
        assert_eq!(
            policy(&db).await.unwrap(),
            Some(Policy {
                trigger_tokens: Some(27_000),
                keep_recent_tokens: 20_000,
                summary_max_tokens: 8_192,
            })
        );
        assert!(invoke("27x", &session).await.is_err());
        assert_eq!(
            policy(&db).await.unwrap().unwrap().trigger_tokens,
            Some(27_000)
        );
        invoke("off", &session).await.unwrap();
        assert_eq!(policy(&db).await.unwrap(), None);
        invoke("on", &session).await.unwrap();
        assert_eq!(policy(&db).await.unwrap().unwrap().trigger_tokens, None);
        let report = invoke("", &session).await.unwrap();
        assert!(
            report.contains("on (trigger 80% of the model context window"),
            "{report}"
        );
        record_status(
            &db,
            "a",
            &Status {
                state: "applied".into(),
                detail: "summarized 1 segment(s)".into(),
                estimated_before: Some(900),
                estimated_after: Some(300),
                summary_usage: vec![],
                at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
        let report = invoke("", &session).await.unwrap();
        assert!(report.contains("Last: applied"), "{report}");
        assert!(report.contains("~900 -> ~300"), "{report}");
    }

    #[tokio::test]
    async fn status_does_not_label_missing_usage_as_reported_zero() {
        let (_instance, session) = crate::test_support::fresh_session().await;
        let db = session.lock().await.database().clone();
        let mut metadata = crate::runtime::ResponseMetadata::default();
        crate::session::usage::mark_usage_fields(&mut metadata.extra, false, false);
        record_status(
            &db,
            "a",
            &Status {
                state: "applied".into(),
                detail: "checkpoint".into(),
                estimated_before: Some(500),
                estimated_after: Some(200),
                summary_usage: vec![metadata],
                at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
        let report = invoke("", &session).await.unwrap();
        assert!(
            report.contains("unknown input / unknown output"),
            "{report}"
        );
        assert!(!report.contains("0 input / 0 output"));
    }

    #[tokio::test]
    async fn invalid_settings_are_refused_not_defaulted() {
        let (_instance, session) = crate::test_support::fresh_session().await;
        let db = session.lock().await.database().clone();
        write_settings(
            &db,
            NAME,
            serde_json::json!({"enabled": true, "trigger_tokens": 0}),
        )
        .await
        .unwrap();
        assert!(policy(&db).await.is_err());
        write_settings(&db, NAME, serde_json::json!({"enabled": true, "bogus": 1}))
            .await
            .unwrap();
        assert!(policy(&db).await.is_err());
    }

    #[test]
    fn defaults_are_off_with_pi_tail_and_8k_ceiling() {
        let settings = Settings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.trigger_tokens, None);
        assert_eq!(settings.keep_recent_tokens, 20_000);
        assert_eq!(settings.summary_max_tokens, 8_192);
        assert_eq!(DEFAULT_TRIGGER_PERCENT, 80);
    }
}

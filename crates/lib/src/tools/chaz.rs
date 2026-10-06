//! Read-only queries over the calling runtime's own session and tool scope.

use crate::session::usage::{UsageFilter, collect_session_usage};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicyRegistry};
use chrono::{DateTime, Datelike, Days, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct ChazTool {
    pub policies: Arc<ToolPolicyRegistry>,
}

#[derive(Clone, Copy)]
enum Period {
    All,
    Today,
    Week,
    Month,
}

impl Period {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Today => "today",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    fn since(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let today = now.date_naive();
        let date = match self {
            Self::All => return None,
            Self::Today => today,
            Self::Week => today - Days::new(today.weekday().num_days_from_monday() as u64),
            Self::Month => today.with_day(1).expect("first day exists"),
        };
        Some(
            date.and_hms_opt(0, 0, 0)
                .expect("midnight exists")
                .and_utc(),
        )
    }
}

enum Query {
    Session,
    Agent,
    Tools,
    Usage(Period),
}

impl Query {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let invalid = || {
            ToolError::InvalidArgument(
                concat!(
                    "Expected exactly query (session, agent, tools, usage) and period ",
                    "(all, today, week, month for usage; null otherwise)"
                )
                .into(),
            )
        };
        let object = arguments.as_object().ok_or_else(invalid)?;
        if object.len() != 2 || !object.contains_key("query") || !object.contains_key("period") {
            return Err(invalid());
        }
        match (object["query"].as_str(), &object["period"]) {
            (Some("session"), Value::Null) => Ok(Self::Session),
            (Some("agent"), Value::Null) => Ok(Self::Agent),
            (Some("tools"), Value::Null) => Ok(Self::Tools),
            (Some("usage"), period) => {
                let period = match period.as_str() {
                    Some("all") => Period::All,
                    Some("today") => Period::Today,
                    Some("week") => Period::Week,
                    Some("month") => Period::Month,
                    _ => return Err(invalid()),
                };
                Ok(Self::Usage(period))
            }
            _ => Err(invalid()),
        }
    }
}

impl Tool for ChazTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chaz".into(),
            description: "Query your current session, caller identity, callable tools and policies, or recorded session usage. Read-only; no other sessions or agents. Set period only for usage.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "enum": ["session", "agent", "tools", "usage"]},
                    "period": {
                        "type": ["string", "null"],
                        "enum": ["all", "today", "week", "month", null],
                        "description": "For usage: all, or the current UTC calendar day/week/month. For other queries: null."
                    }
                },
                "required": ["query", "period"],
                "additionalProperties": false
            }),
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            // Validate even without provider strict mode, before any state read.
            let query = Query::parse(&arguments)?;
            let output = match query {
                Query::Session => {
                    let session = ctx.session.lock().await;
                    json!({
                        "session_db_id": session.database().root_id().to_string(),
                        "name": session.read_meta().await.name
                    })
                }
                Query::Agent => json!({
                    "name": ctx.agent_name,
                    "call_depth": ctx.call_depth,
                    "max_call_depth": ctx.max_call_depth
                }),
                Query::Tools => {
                    let rows: Vec<_> = ctx
                        .tools
                        .permitted_names()
                        .into_iter()
                        .filter_map(|name| {
                            let tool = ctx.tools.get(&name)?;
                            let policy = self.policies.resolve(tool.as_ref());
                            Some(json!({
                                "name": name,
                                "presentation": ctx.profile.resolve_mode(&name),
                                "risk": policy.risk,
                                "approval": policy.approval,
                                "timeout_seconds": policy.timeout,
                                "rate_limit": policy.rate_limit,
                                "effective_grants": ctx.resolve_call_grants(&policy.grants, &name)
                            }))
                        })
                        .collect();
                    json!({"tools": rows})
                }
                Query::Usage(period) => {
                    let since = period.since(Utc::now());
                    let session = ctx.session.lock().await;
                    let mut per_model = BTreeMap::new();
                    let total = collect_session_usage(
                        session.entries(),
                        &UsageFilter {
                            since,
                            ..Default::default()
                        },
                        &mut per_model,
                    );
                    json!({
                        "session_db_id": session.database().root_id().to_string(),
                        "period": period.as_str(),
                        "since": since,
                        "total": total,
                        "per_model": per_model
                    })
                }
            };
            Ok(output.to_string())
        })
    }
}

#[cfg(test)]
#[path = "chaz_tests.rs"]
mod tests;

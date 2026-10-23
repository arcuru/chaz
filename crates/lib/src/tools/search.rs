//! Web search tool.
//!
//! Supports a few API-backed providers (Kagi, Tavily, Brave, Serper), any
//! SearxNG instance (self-hosted or public) at a configured URL, and falls
//! back to parsing DuckDuckGo's HTML results page when no API key is
//! configured. The tool holds an ordered preference list of backends and
//! tries each in turn, failing over to the next on any error. The last
//! entry is the final answer — if it also errors, that error is returned.
//!
//! Every backend request is a `Capability::HttpRequest` through the call's
//! `ToolHost` under its resolved grants, like `web_fetch`. The query is
//! model-chosen text sent to a third party, so it is tool egress: a network
//! grant that denies a backend's endpoint denies that backend. Configuring a
//! backend does not authorize it; a private or local SearxNG instance needs a
//! private-network grant on `web_search`.

use crate::tool::{ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolPolicy};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, info, warn};

/// A single normalized search result returned to the LLM.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Which search provider the tool routes queries through.
#[derive(Debug, Clone)]
pub enum SearchBackend {
    /// Kagi Search API — https://kagi.com/api/v0/search (invite-only beta)
    Kagi { api_key: String },
    /// Tavily REST — https://api.tavily.com/search
    Tavily { api_key: String },
    /// Brave Search API — https://api.search.brave.com/res/v1/web/search
    Brave { api_key: String },
    /// Serper (Google SERP proxy) — https://google.serper.dev/search
    Serper { api_key: String },
    /// SearxNG meta-search. `base_url` is the instance root (no trailing
    /// `/search` — we append that). JSON format must be enabled on the
    /// instance.
    Searxng { base_url: String },
    /// Keyless fallback — scrapes `html.duckduckgo.com/html/`.
    DuckDuckGo,
}

impl SearchBackend {
    /// Short name for logs / errors.
    fn name(&self) -> &'static str {
        match self {
            SearchBackend::Kagi { .. } => "kagi",
            SearchBackend::Tavily { .. } => "tavily",
            SearchBackend::Brave { .. } => "brave",
            SearchBackend::Serper { .. } => "serper",
            SearchBackend::Searxng { .. } => "searxng",
            SearchBackend::DuckDuckGo => "duckduckgo",
        }
    }
}

/// Per-backend deadline. A hung backend must not consume the whole tool
/// timeout (60 s by default), or failover would never reach the next one.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(15);

/// DuckDuckGo blocks bot UAs; send a generic browser UA to every backend.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0";

/// Config path that authorizes a private or local SearxNG instance.
const PRIVATE_GRANT_HINT: &str = "private SearxNG needs \
     `security.tool_policies.web_search.grants.network.allow_private: true`, \
     and session and agent capability ceilings must also allow private networks";

pub struct WebSearch {
    /// Non-empty ordered preference list. Tried in sequence on failure.
    /// An empty `new()` input is coerced to a single DuckDuckGo entry so
    /// the tool always has a fallback.
    backends: Vec<SearchBackend>,
    backend_timeout: Duration,
}

impl WebSearch {
    pub fn new(backends: Vec<SearchBackend>) -> Self {
        let backends = if backends.is_empty() {
            vec![SearchBackend::DuckDuckGo]
        } else {
            backends
        };
        Self {
            backends,
            backend_timeout: BACKEND_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_backend_timeout(mut self, timeout: Duration) -> Self {
        self.backend_timeout = timeout;
        self
    }
}

/// Configured SearxNG URLs at a private address or internal hostname that the
/// given `web_search` policy grant does not authorize. Session and agent
/// ceilings can only narrow the policy grant, so every one of these is denied
/// by the host at call time. Used for a startup warning; enforcement stays in
/// the host.
fn unauthorized_private_backends<'a>(
    backends: &'a [SearchBackend],
    policy_grants: &Grants,
) -> Vec<&'a str> {
    if allows_private(policy_grants) {
        return Vec::new();
    }
    backends
        .iter()
        .filter_map(|b| match b {
            SearchBackend::Searxng { base_url } if is_private_url(base_url) => {
                Some(base_url.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Log the startup warning for [`unauthorized_private_backends`].
pub fn warn_unauthorized_private_backends(backends: &[SearchBackend], policy_grants: &Grants) {
    for url in unauthorized_private_backends(backends, policy_grants) {
        warn!(
            %url,
            "web_search: SearxNG backend will be denied: {PRIVATE_GRANT_HINT}"
        );
    }
}

fn allows_private(grants: &Grants) -> bool {
    grants.network.as_ref().is_some_and(|n| n.allow_private)
}

/// Whether the host's private-address check would refuse this URL. Mirrors
/// the host policy for diagnostics only; the host makes the decision.
fn is_private_url(url: &str) -> bool {
    crate::security::NetworkPolicy::permissive()
        .check(url, "GET")
        .is_err_and(|e| e.starts_with("SSRF protection"))
}

impl Tool for WebSearch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "web_search".to_string(),
            description: "Search the web and return a list of results. Each result has a title, URL, and short snippet. Use this to find pages when you don't already have a URL — follow up with web_fetch to read a specific result.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum results to return (default 5, max 10)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            approval: ApprovalRequirement::Never,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        use crate::tool::ToolError;
        Box::pin(async move {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgument("Missing 'query' argument".into()))?
                .trim();
            if query.is_empty() {
                return Err(ToolError::InvalidArgument(
                    "'query' must not be empty".into(),
                ));
            }
            let max_results = arguments
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .clamp(1, 10) as usize;

            let chain: Vec<&'static str> = self.backends.iter().map(|b| b.name()).collect();
            info!(backends = ?chain, %query, max_results, "Searching web");

            let results = try_backends(
                &self.backends,
                ctx.host(),
                ctx.grants(),
                self.backend_timeout,
                query,
                max_results,
            )
            .await?;

            debug!(count = results.len(), "Search results");
            serde_json::to_string(&results)
                .map_err(|e| ToolError::Execution(format!("Failed to serialize results: {e}")))
        })
    }
}

/// Run the query against each backend in order, returning the first `Ok`.
/// If every backend errors, returns the last error. The chain is assumed
/// non-empty (`WebSearch::new` guarantees this).
///
/// Every request goes through the host with the call's resolved grants, so a
/// denial is an error for that backend like any other: failover may try the
/// next one, which the same grants judge in turn.
async fn try_backends(
    backends: &[SearchBackend],
    host: &dyn ToolHost,
    grants: &Grants,
    timeout: Duration,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    try_backends_with(backends, |backend| async move {
        let request = build_request(backend, query, max_results)?;
        let (status, text) = send(backend, host, grants, timeout, request).await?;
        parse_response(backend, status, &text, max_results)
    })
    .await
}

/// Generic failover loop extracted from `try_backends` for unit testing
/// without HTTP. Iterates `backends`, calling `search(backend)` for each;
/// returns the first Ok, or the last Err if all fail.
async fn try_backends_with<'a, F, Fut>(
    backends: &'a [SearchBackend],
    mut search: F,
) -> Result<Vec<SearchResult>, ToolError>
where
    F: FnMut(&'a SearchBackend) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<SearchResult>, ToolError>>,
{
    let mut last_err: Option<ToolError> = None;
    for backend in backends {
        match search(backend).await {
            Ok(results) => {
                if last_err.is_some() {
                    info!(backend = %backend.name(), "Failover succeeded");
                }
                return Ok(results);
            }
            Err(e) => {
                warn!(
                    backend = %backend.name(),
                    error = %e,
                    "Search backend failed; trying next"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| ToolError::Execution("No search backends configured".into())))
}

// ================================================================
// Provider requests and responses
// ================================================================

use crate::grants::Grants;
use crate::tool::ToolError;
use crate::tool_host::{Capability, CapabilityResult, ToolHost};
use std::collections::HashMap;

/// Build the `HttpRequest` capability for one backend. API keys travel in
/// headers (or Tavily's JSON body, as its API requires), never in the URL.
fn build_request(
    backend: &SearchBackend,
    query: &str,
    max_results: usize,
) -> Result<Capability, ToolError> {
    let limit = max_results.to_string();
    let mut headers = HashMap::from([("User-Agent".to_string(), USER_AGENT.to_string())]);
    let (method, url, body) = match backend {
        // Kagi's Search API is GET /api/v0/search?q=&limit= with
        // `Authorization: Bot <token>` (literal word "Bot", not "Bearer").
        SearchBackend::Kagi { api_key } => {
            headers.insert("Authorization".into(), format!("Bot {api_key}"));
            let url = with_query(
                backend,
                "https://kagi.com/api/v0/search",
                &[("q", query), ("limit", &limit)],
            )?;
            ("GET", url, None)
        }
        SearchBackend::Tavily { api_key } => {
            headers.insert("Content-Type".into(), "application/json".into());
            let body = serde_json::json!({
                "api_key": api_key,
                "query": query,
                "max_results": max_results,
            });
            (
                "POST",
                "https://api.tavily.com/search".to_string(),
                Some(body.to_string()),
            )
        }
        SearchBackend::Brave { api_key } => {
            headers.insert("Accept".into(), "application/json".into());
            headers.insert("X-Subscription-Token".into(), api_key.clone());
            let url = with_query(
                backend,
                "https://api.search.brave.com/res/v1/web/search",
                &[("q", query), ("count", &limit)],
            )?;
            ("GET", url, None)
        }
        SearchBackend::Serper { api_key } => {
            headers.insert("X-API-KEY".into(), api_key.clone());
            headers.insert("Content-Type".into(), "application/json".into());
            let body = serde_json::json!({ "q": query, "num": max_results });
            (
                "POST",
                "https://google.serper.dev/search".to_string(),
                Some(body.to_string()),
            )
        }
        // SearxNG exposes both HTML and JSON on the same `/search` endpoint;
        // `format=json` selects JSON. The instance must have the JSON output
        // format enabled (default on `searxng` ≥ 2022 but some public
        // instances disable it — error messages from the server are
        // informative).
        SearchBackend::Searxng { base_url } => {
            let base = format!("{}/search", base_url.trim_end_matches('/'));
            let url = with_query(backend, &base, &[("q", query), ("format", "json")])?;
            ("GET", url, None)
        }
        // DDG blocks GET requests with non-browser UAs; POST with form data
        // works reliably.
        SearchBackend::DuckDuckGo => {
            headers.insert(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            );
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("q", query)
                .finish();
            (
                "POST",
                "https://html.duckduckgo.com/html/".to_string(),
                Some(body),
            )
        }
    };
    Ok(Capability::HttpRequest {
        url,
        method: method.to_string(),
        headers,
        body,
    })
}

/// `base` with `params` appended as an encoded query string.
fn with_query(
    backend: &SearchBackend,
    base: &str,
    params: &[(&str, &str)],
) -> Result<String, ToolError> {
    url::Url::parse_with_params(base, params)
        .map(String::from)
        .map_err(|e| ToolError::Execution(format!("{}: invalid URL: {e}", backend.name())))
}

/// Send one backend request through the host under the call's grants,
/// bounded by `timeout`. Returns the status and the body as text.
async fn send(
    backend: &SearchBackend,
    host: &dyn ToolHost,
    grants: &Grants,
    timeout: Duration,
    request: Capability,
) -> Result<(u16, String), ToolError> {
    let name = backend.name();
    let result = match tokio::time::timeout(timeout, host.request(&request, grants)).await {
        Ok(result) => result,
        Err(_) => {
            return Err(ToolError::Network(format!(
                "{name}: no response within {}s",
                timeout.as_secs_f32()
            )));
        }
    };
    match result {
        Ok(CapabilityResult::HttpResponse(response)) => Ok((
            response.status,
            String::from_utf8_lossy(&response.body).into_owned(),
        )),
        Ok(_) => Err(ToolError::Execution(format!(
            "{name}: unexpected host result for HTTP capability"
        ))),
        Err(e) => Err(annotate_host_error(backend, grants, e)),
    }
}

/// Prefix a host error with the backend name. A private SearxNG instance
/// refused for lack of a private-network grant also gets the config fix.
fn annotate_host_error(backend: &SearchBackend, grants: &Grants, err: ToolError) -> ToolError {
    let name = backend.name();
    let hint = match backend {
        SearchBackend::Searxng { base_url }
            if !allows_private(grants) && is_private_url(base_url) =>
        {
            warn!(
                url = %base_url,
                "web_search: SearxNG backend denied: {PRIVATE_GRANT_HINT}"
            );
            format!(" ({PRIVATE_GRANT_HINT})")
        }
        _ => String::new(),
    };
    match err {
        ToolError::Network(m) => ToolError::Network(format!("{name}: {m}{hint}")),
        ToolError::Execution(m) => ToolError::Execution(format!("{name}: {m}{hint}")),
        other => other,
    }
}

/// Turn one backend's HTTP status and body into results. A non-success
/// status is an error (so failover moves on); an empty result list is not,
/// except for DuckDuckGo, whose empty page usually means rate limiting.
fn parse_response(
    backend: &SearchBackend,
    status: u16,
    text: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    let name = backend.name();
    if let SearchBackend::DuckDuckGo = backend {
        // DDG returns 202 when rate-limiting or blocking the request; treat
        // it as an error so the failover chain can try the next backend.
        if status != 200 {
            return Err(ToolError::Execution(format!(
                "duckduckgo returned HTTP {status}"
            )));
        }
        let results = parse_duckduckgo_html(text, max_results);
        if results.is_empty() {
            warn!("duckduckgo returned no parseable results (HTML may have changed)");
            return Err(ToolError::Execution(
                "duckduckgo returned no results (may be rate-limited or HTML changed)".into(),
            ));
        }
        return Ok(results);
    }
    if !(200..300).contains(&status) {
        return Err(ToolError::Execution(format!(
            "{name} returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(text)
        .map_err(|e| ToolError::Execution(format!("{name}: bad JSON: {e}")))?;
    Ok(match backend {
        SearchBackend::Kagi { .. } => parse_kagi_results(&parsed, max_results),
        SearchBackend::Tavily { .. } => {
            parse_result_list(parsed.get("results"), "url", "content", max_results)
        }
        SearchBackend::Brave { .. } => parse_result_list(
            parsed.get("web").and_then(|w| w.get("results")),
            "url",
            "description",
            max_results,
        ),
        SearchBackend::Serper { .. } => {
            parse_result_list(parsed.get("organic"), "link", "snippet", max_results)
        }
        SearchBackend::Searxng { .. } => parse_searxng_results(&parsed, max_results),
        SearchBackend::DuckDuckGo => unreachable!("handled above"),
    })
}

/// Map a JSON array of results with `title` plus the given URL and snippet
/// fields. A missing array is an empty result list.
fn parse_result_list(
    list: Option<&Value>,
    url_field: &str,
    snippet_field: &str,
    max_results: usize,
) -> Vec<SearchResult> {
    let field = |r: &Value, key: &str| {
        r.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    list.and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| SearchResult {
                    title: field(r, "title"),
                    url: field(r, url_field),
                    snippet: field(r, snippet_field),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Extract search-result (`t: 0`) entries from a Kagi response. Pulled out
/// so it can be unit-tested against a canned response without HTTP.
/// Response `data[]` is a discriminated list; `t: 0` entries are search
/// results (url/title/snippet), `t: 1` is related searches.
fn parse_kagi_results(parsed: &Value, max_results: usize) -> Vec<SearchResult> {
    let results: Vec<Value> = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|r| r.get("t").and_then(|t| t.as_u64()) == Some(0))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    parse_result_list(Some(&Value::Array(results)), "url", "snippet", max_results)
}

/// Pull result entries out of a SearxNG JSON response. `content` is the
/// snippet field upstream; we normalize to `snippet`.
fn parse_searxng_results(parsed: &Value, max_results: usize) -> Vec<SearchResult> {
    parse_result_list(parsed.get("results"), "url", "content", max_results)
}

/// Parse results out of DuckDuckGo's `html.duckduckgo.com/html/` response.
///
/// The page emits one `<div class="result ...">` per hit, with a
/// `<a class="result__a" href="...">Title</a>` and a
/// `<a class="result__snippet" ...>Snippet</a>`. DDG wraps outbound URLs in a
/// redirect — `/l/?uddg=<url-encoded>` — which we unwrap.
fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    static RESULT_BLOCK: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let block_re = RESULT_BLOCK.get_or_init(|| {
        Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("DDG block regex is valid")
    });
    let snippet_re = SNIPPET.get_or_init(|| {
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
            .expect("DDG snippet regex is valid")
    });

    let mut results = Vec::new();
    let mut snippets = snippet_re.captures_iter(html);
    for caps in block_re.captures_iter(html) {
        if results.len() >= max_results {
            break;
        }
        let href = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let title_html = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        let snippet_html = snippets
            .next()
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .unwrap_or_default();

        let url = unwrap_ddg_redirect(href);
        let title = strip_html_tags(title_html);
        let snippet = strip_html_tags(&snippet_html);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// DDG wraps outbound hrefs as `/l/?uddg=<percent-encoded-url>&rut=...` (or
/// occasionally `//duckduckgo.com/l/?...`). Extract the real URL when present;
/// otherwise return the input unchanged.
fn unwrap_ddg_redirect(href: &str) -> String {
    let href = href.trim();
    // Normalize protocol-relative DDG links.
    let href = href
        .strip_prefix("//")
        .map(|s| format!("https://{s}"))
        .unwrap_or_else(|| href.to_string());
    // Only try to unwrap the uddg= form.
    let needle = "uddg=";
    if let Some(idx) = href.find(needle) {
        let tail = &href[idx + needle.len()..];
        let end = tail.find('&').unwrap_or(tail.len());
        let encoded = &tail[..end];
        if let Ok(decoded) = urlencoding_decode(encoded) {
            return decoded;
        }
    }
    href
}

/// Minimal percent-decoder that also turns `+` into a space — good enough for
/// URL query values. Returns Err on malformed `%` sequences.
fn urlencoding_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(());
                }
                let hi = from_hex(bytes[i + 1]).ok_or(())?;
                let lo = from_hex(bytes[i + 2]).ok_or(())?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip HTML tags, collapse whitespace, decode a few common entities. DDG's
/// snippets wrap matches in `<b>`, include `&nbsp;`/`&amp;`/`&quot;` — enough
/// of a decode to produce clean text for the LLM.
fn strip_html_tags(input: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG.get_or_init(|| Regex::new(r"<[^>]*>").expect("tag regex is valid"));
    let without_tags = tag_re.replace_all(input, "");
    let decoded = without_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddg_parses_result_blocks() {
        // Shape lifted from a real DDG HTML page, trimmed to two hits.
        let html = r#"
            <div class="result">
              <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fexample.com%2Ffoo&amp;rut=x">Example <b>Foo</b></a>
              <a class="result__snippet" href="/l/?uddg=https%3A%2F%2Fexample.com%2Ffoo">A <b>foo</b>&nbsp;page that is very foo.</a>
            </div>
            <div class="result">
              <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fexample.com%2Fbar">Bar Title</a>
              <a class="result__snippet" href="/l/?uddg=https%3A%2F%2Fexample.com%2Fbar">Bar snippet text.</a>
            </div>
        "#;
        let r = parse_duckduckgo_html(html, 10);
        assert_eq!(r.len(), 2, "expected 2 results, got {r:?}");
        assert_eq!(r[0].url, "https://example.com/foo");
        assert_eq!(r[0].title, "Example Foo");
        assert!(r[0].snippet.contains("foo page"));
        assert_eq!(r[1].url, "https://example.com/bar");
        assert_eq!(r[1].title, "Bar Title");
    }

    #[test]
    fn ddg_respects_max_results() {
        let html = r#"
            <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fa.com">A</a>
            <a class="result__snippet">sa</a>
            <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fb.com">B</a>
            <a class="result__snippet">sb</a>
            <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fc.com">C</a>
            <a class="result__snippet">sc</a>
        "#;
        let r = parse_duckduckgo_html(html, 2);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].url, "https://b.com");
    }

    #[test]
    fn ddg_skips_results_with_no_url_or_title() {
        let html = r#"
            <a rel="nofollow" class="result__a" href="">Orphan</a>
            <a class="result__snippet">orphan snip</a>
            <a rel="nofollow" class="result__a" href="/l/?uddg=https%3A%2F%2Fb.com">B</a>
            <a class="result__snippet">sb</a>
        "#;
        let r = parse_duckduckgo_html(html, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://b.com");
    }

    #[test]
    fn unwrap_ddg_redirect_decodes_uddg() {
        assert_eq!(
            unwrap_ddg_redirect("/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fq%3D1&rut=x"),
            "https://example.com/a?q=1"
        );
    }

    #[test]
    fn unwrap_ddg_redirect_returns_raw_for_direct_urls() {
        assert_eq!(
            unwrap_ddg_redirect("https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn strip_html_tags_cleans_bold_and_entities() {
        assert_eq!(
            strip_html_tags("A <b>bold</b>&nbsp;&amp;&#39; thing"),
            "A bold &' thing"
        );
    }

    #[test]
    fn urlencoding_decode_handles_pluses_and_hex() {
        assert_eq!(
            urlencoding_decode("hello+world%20%26%20more").unwrap(),
            "hello world & more"
        );
    }

    #[test]
    fn urlencoding_decode_rejects_malformed() {
        assert!(urlencoding_decode("%2").is_err());
        assert!(urlencoding_decode("%XY").is_err());
    }

    #[test]
    fn kagi_parser_takes_search_results_and_skips_related() {
        // t=0 is a search result; t=1 is "related searches" (a list). The
        // parser must only pick up t=0.
        let body = serde_json::json!({
            "meta": {"id": "x", "ms": 123, "api_balance": 99.0},
            "data": [
                {"t": 0, "url": "https://a.com", "title": "A", "snippet": "about a"},
                {"t": 0, "url": "https://b.com", "title": "B", "snippet": "about b"},
                {"t": 1, "list": ["related 1", "related 2"]},
            ]
        });
        let out = parse_kagi_results(&body, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://a.com");
        assert_eq!(out[0].title, "A");
        assert_eq!(out[0].snippet, "about a");
        assert_eq!(out[1].url, "https://b.com");
    }

    #[test]
    fn kagi_parser_respects_max_results() {
        let body = serde_json::json!({
            "data": [
                {"t": 0, "url": "https://a.com", "title": "A", "snippet": ""},
                {"t": 0, "url": "https://b.com", "title": "B", "snippet": ""},
                {"t": 0, "url": "https://c.com", "title": "C", "snippet": ""},
            ]
        });
        let out = parse_kagi_results(&body, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].url, "https://b.com");
    }

    #[test]
    fn kagi_parser_empty_data_returns_empty() {
        let body = serde_json::json!({ "data": [] });
        assert!(parse_kagi_results(&body, 10).is_empty());
    }

    #[test]
    fn searxng_parser_maps_content_to_snippet() {
        // SearxNG's result snippet field is `content`; we normalize to `snippet`.
        let body = serde_json::json!({
            "query": "rust async",
            "number_of_results": 123,
            "results": [
                {"title": "Tokio", "url": "https://tokio.rs/", "content": "async runtime", "engine": "google"},
                {"title": "Async book", "url": "https://rust-lang.github.io/async-book/", "content": "the book", "engine": "bing"},
            ],
            "suggestions": []
        });
        let out = parse_searxng_results(&body, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Tokio");
        assert_eq!(out[0].url, "https://tokio.rs/");
        assert_eq!(out[0].snippet, "async runtime");
    }

    #[test]
    fn searxng_parser_respects_max_results() {
        let body = serde_json::json!({
            "results": [
                {"title": "A", "url": "https://a", "content": ""},
                {"title": "B", "url": "https://b", "content": ""},
                {"title": "C", "url": "https://c", "content": ""},
            ]
        });
        let out = parse_searxng_results(&body, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].url, "https://b");
    }

    #[test]
    fn searxng_parser_missing_results_returns_empty() {
        let body = serde_json::json!({"query": "x", "number_of_results": 0});
        assert!(parse_searxng_results(&body, 10).is_empty());
    }

    #[test]
    fn backend_name_is_stable() {
        assert_eq!(SearchBackend::DuckDuckGo.name(), "duckduckgo");
        assert_eq!(
            SearchBackend::Tavily { api_key: "".into() }.name(),
            "tavily"
        );
    }

    // ----- Failover loop -----
    //
    // These exercise `try_backends_with` against canned per-backend results
    // — no HTTP. The real `try_backends` is just this loop wrapping a closure
    // that dispatches to the provider-specific search fns.

    fn sample(url: &str) -> SearchResult {
        SearchResult {
            title: format!("t:{url}"),
            url: url.into(),
            snippet: "s".into(),
        }
    }

    #[tokio::test]
    async fn failover_returns_first_ok() {
        let backends = vec![
            SearchBackend::Tavily { api_key: "".into() },
            SearchBackend::DuckDuckGo,
        ];
        let out = try_backends_with(&backends, |b| async move {
            match b {
                SearchBackend::Tavily { .. } => Ok(vec![sample("https://a")]),
                _ => panic!("second backend should not be called"),
            }
        })
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://a");
    }

    #[tokio::test]
    async fn failover_falls_through_to_next_on_error() {
        let backends = vec![
            SearchBackend::Tavily { api_key: "".into() },
            SearchBackend::DuckDuckGo,
        ];
        let out = try_backends_with(&backends, |b| async move {
            match b {
                SearchBackend::Tavily { .. } => Err(ToolError::Network("boom".into())),
                SearchBackend::DuckDuckGo => Ok(vec![sample("https://b")]),
                _ => unreachable!(),
            }
        })
        .await
        .unwrap();
        assert_eq!(out[0].url, "https://b");
    }

    #[tokio::test]
    async fn failover_returns_last_error_when_all_fail() {
        let backends = vec![
            SearchBackend::Tavily { api_key: "".into() },
            SearchBackend::Brave { api_key: "".into() },
        ];
        let err = try_backends_with(&backends, |b| async move {
            match b {
                SearchBackend::Tavily { .. } => Err(ToolError::Network("first".into())),
                SearchBackend::Brave { .. } => Err(ToolError::Execution("last".into())),
                _ => unreachable!(),
            }
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("last"), "got: {err}");
    }

    #[tokio::test]
    async fn failover_empty_results_do_not_trigger_next_backend() {
        // An empty list is a legitimate answer (query has no hits). Treating
        // it as an error would mask bad queries.
        let backends = vec![
            SearchBackend::Tavily { api_key: "".into() },
            SearchBackend::DuckDuckGo,
        ];
        let out = try_backends_with(&backends, |b| async move {
            match b {
                SearchBackend::Tavily { .. } => Ok(vec![]),
                _ => panic!("DDG should not be called on empty-Ok"),
            }
        })
        .await
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn new_empty_list_coerces_to_duckduckgo() {
        let s = WebSearch::new(vec![]);
        assert_eq!(s.backends.len(), 1);
        assert!(matches!(s.backends[0], SearchBackend::DuckDuckGo));
    }

    #[test]
    fn ddg_parses_direct_url_format() {
        // DDG now emits direct URLs rather than /l/?uddg= redirects.
        let html = r#"
            <div class="result results_links results_links_deep web-result ">
              <div class="links_main links_deep result__body">
                <h2 class="result__title">
                  <a rel="nofollow" class="result__a" href="https://tokio.rs/">Tokio async runtime</a>
                </h2>
                <a class="result__snippet" href="https://tokio.rs/">A reliable, async runtime for Rust.</a>
              </div>
            </div>
            <div class="result results_links results_links_deep web-result ">
              <div class="links_main links_deep result__body">
                <h2 class="result__title">
                  <a rel="nofollow" class="result__a" href="https://async.rs/">async-std</a>
                </h2>
                <a class="result__snippet" href="https://async.rs/">Async version of the Rust standard library.</a>
              </div>
            </div>
        "#;
        let r = parse_duckduckgo_html(html, 10);
        assert_eq!(r.len(), 2, "expected 2 results, got {r:?}");
        assert_eq!(r[0].url, "https://tokio.rs/");
        assert_eq!(r[0].title, "Tokio async runtime");
        assert!(r[0].snippet.contains("async runtime"));
        assert_eq!(r[1].url, "https://async.rs/");
        assert_eq!(r[1].title, "async-std");
    }

    /// web_search through the `ToolHost`: request encoding per backend,
    /// grant enforcement with a real host against a local listener, and the
    /// failover/timeout behaviour the routing must preserve. No live network.
    mod host_routing {
        use super::*;
        use crate::grants::{Allowlist, EndpointPattern, NetworkGrant};
        use crate::test_support::{MockHost, fresh_session, tool_context_with_host};
        use crate::tool::ToolRegistry;
        use crate::tool_host::NativeToolHost;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const KEY: &str = "sk-test-secret-123";
        const QUERY: &str = "rust & tokio=fast/é?";

        async fn ctx(host: Arc<dyn ToolHost>) -> (eidetica::Instance, ToolContext) {
            let (instance, session) = fresh_session().await;
            let ctx = tool_context_with_host(session, Arc::new(ToolRegistry::new()), host);
            (instance, ctx)
        }

        fn net(endpoints: Allowlist<EndpointPattern>, allow_private: bool) -> Grants {
            Grants {
                network: Some(NetworkGrant {
                    endpoints,
                    allow_private,
                }),
                ..Grants::default()
            }
        }

        fn deny_all() -> Grants {
            // `allow_private` stays true so only the endpoint ceiling can be
            // what refuses the local listener.
            net(Allowlist::Only(vec![]), true)
        }

        fn http_parts(call: &Capability) -> (&str, &str, &HashMap<String, String>, Option<&str>) {
            match call {
                Capability::HttpRequest {
                    url,
                    method,
                    headers,
                    body,
                } => (method, url, headers, body.as_deref()),
                other => panic!("expected HttpRequest, got {other:?}"),
            }
        }

        /// Run one search against a MockHost answering `status`/`body`, and
        /// return the tool output and the single recorded request.
        async fn run_mock(
            backend: SearchBackend,
            status: u16,
            body: &str,
        ) -> (Result<String, ToolError>, Capability) {
            let host = Arc::new(MockHost::new());
            host.push_http(status, body.as_bytes().to_vec());
            let (_i, c) = ctx(host.clone()).await;
            let out = WebSearch::new(vec![backend])
                .execute(serde_json::json!({ "query": QUERY, "max_results": 3 }), &c)
                .await;
            let calls = host.recorded_calls();
            assert_eq!(calls.len(), 1, "exactly one host request per attempt");
            (out, calls[0].clone())
        }

        fn query_pairs(url: &str) -> Vec<(String, String)> {
            url::Url::parse(url)
                .unwrap()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        }

        #[tokio::test]
        async fn kagi_request_is_encoded_get_with_bot_auth_header() {
            let body = r#"{"data":[{"t":0,"url":"https://a","title":"A","snippet":"s"}]}"#;
            let (out, call) = run_mock(
                SearchBackend::Kagi {
                    api_key: KEY.into(),
                },
                200,
                body,
            )
            .await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "GET");
            assert!(url.starts_with("https://kagi.com/api/v0/search?"), "{url}");
            assert_eq!(
                query_pairs(url),
                vec![("q".into(), QUERY.into()), ("limit".into(), "3".into())]
            );
            assert_eq!(headers["Authorization"], format!("Bot {KEY}"));
            assert_eq!(headers["User-Agent"], USER_AGENT);
            assert!(req_body.is_none());
            assert!(!url.contains(KEY));
            assert!(out.unwrap().contains("https://a"));
        }

        #[tokio::test]
        async fn tavily_request_is_json_post_with_key_in_body() {
            let body = r#"{"results":[{"title":"A","url":"https://a","content":"s"}]}"#;
            let (out, call) = run_mock(
                SearchBackend::Tavily {
                    api_key: KEY.into(),
                },
                200,
                body,
            )
            .await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "POST");
            assert_eq!(url, "https://api.tavily.com/search");
            assert_eq!(headers["Content-Type"], "application/json");
            assert_eq!(headers["User-Agent"], USER_AGENT);
            let sent: Value = serde_json::from_str(req_body.unwrap()).unwrap();
            assert_eq!(
                sent,
                serde_json::json!({ "api_key": KEY, "query": QUERY, "max_results": 3 })
            );
            assert_eq!(
                serde_json::from_str::<Value>(&out.unwrap()).unwrap(),
                serde_json::json!([{ "title": "A", "url": "https://a", "snippet": "s" }])
            );
        }

        #[tokio::test]
        async fn brave_request_is_encoded_get_with_token_header() {
            let body = r#"{"web":{"results":[{"title":"A","url":"https://a","description":"s"}]}}"#;
            let (out, call) = run_mock(
                SearchBackend::Brave {
                    api_key: KEY.into(),
                },
                200,
                body,
            )
            .await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "GET");
            assert!(url.starts_with("https://api.search.brave.com/res/v1/web/search?"));
            assert_eq!(
                query_pairs(url),
                vec![("q".into(), QUERY.into()), ("count".into(), "3".into())]
            );
            assert_eq!(headers["X-Subscription-Token"], KEY);
            assert_eq!(headers["Accept"], "application/json");
            assert!(req_body.is_none());
            assert!(!url.contains(KEY));
            assert!(out.unwrap().contains("\"snippet\":\"s\""));
        }

        #[tokio::test]
        async fn serper_request_is_json_post_with_key_header() {
            let body = r#"{"organic":[{"title":"A","link":"https://a","snippet":"s"}]}"#;
            let (out, call) = run_mock(
                SearchBackend::Serper {
                    api_key: KEY.into(),
                },
                200,
                body,
            )
            .await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "POST");
            assert_eq!(url, "https://google.serper.dev/search");
            assert_eq!(headers["X-API-KEY"], KEY);
            assert_eq!(headers["Content-Type"], "application/json");
            let sent: Value = serde_json::from_str(req_body.unwrap()).unwrap();
            assert_eq!(sent, serde_json::json!({ "q": QUERY, "num": 3 }));
            assert!(out.unwrap().contains("https://a"));
        }

        #[tokio::test]
        async fn searxng_request_appends_search_path_and_json_format() {
            let body = r#"{"results":[{"title":"A","url":"https://a","content":"s"}]}"#;
            let backend = SearchBackend::Searxng {
                base_url: "https://searx.example/".into(),
            };
            let (out, call) = run_mock(backend, 200, body).await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "GET");
            assert!(url.starts_with("https://searx.example/search?"), "{url}");
            assert_eq!(
                query_pairs(url),
                vec![("q".into(), QUERY.into()), ("format".into(), "json".into())]
            );
            assert_eq!(headers.len(), 1, "only the User-Agent: {headers:?}");
            assert!(req_body.is_none());
            assert!(out.unwrap().contains("https://a"));
        }

        #[tokio::test]
        async fn duckduckgo_request_is_form_post() {
            let html =
                r#"<a class="result__a" href="https://a">A</a><a class="result__snippet">s</a>"#;
            let (out, call) = run_mock(SearchBackend::DuckDuckGo, 200, html).await;
            let (method, url, headers, req_body) = http_parts(&call);
            assert_eq!(method, "POST");
            assert_eq!(url, "https://html.duckduckgo.com/html/");
            assert_eq!(headers["Content-Type"], "application/x-www-form-urlencoded");
            assert_eq!(headers["User-Agent"], USER_AGENT);
            let body = req_body.unwrap();
            assert_eq!(body, "q=rust+%26+tokio%3Dfast%2F%C3%A9%3F");
            let decoded: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            assert_eq!(decoded, vec![("q".into(), QUERY.into())]);
            assert!(out.unwrap().contains("https://a"));
        }

        #[tokio::test]
        async fn error_status_and_host_errors_never_disclose_the_key() {
            for backend in [
                SearchBackend::Kagi {
                    api_key: KEY.into(),
                },
                SearchBackend::Tavily {
                    api_key: KEY.into(),
                },
                SearchBackend::Brave {
                    api_key: KEY.into(),
                },
                SearchBackend::Serper {
                    api_key: KEY.into(),
                },
            ] {
                let (out, _) = run_mock(backend.clone(), 401, "unauthorized").await;
                let err = format!("{}", out.unwrap_err());
                assert!(err.contains("HTTP 401"), "{err}");
                assert!(!err.contains(KEY), "{} leaked key: {err}", backend.name());

                let host = Arc::new(MockHost::new());
                host.push_err(ToolError::Network("connection reset".into()));
                let (_i, c) = ctx(host).await;
                let err = WebSearch::new(vec![backend.clone()])
                    .execute(serde_json::json!({ "query": "q" }), &c)
                    .await
                    .unwrap_err();
                let err = format!("{err}");
                assert!(err.contains(backend.name()), "{err}");
                assert!(!err.contains(KEY), "{} leaked key: {err}", backend.name());
            }
        }

        #[tokio::test]
        async fn redirect_is_not_followed_and_fails_over() {
            let host = Arc::new(MockHost::new());
            host.push_http_with_headers(
                302,
                [("location".to_string(), "http://10.0.0.1/".to_string())].into(),
                b"".to_vec(),
            );
            host.push_http(200, br#"{"results":[]}"#.to_vec());
            let (_i, c) = ctx(host.clone()).await;
            let out = WebSearch::new(vec![
                SearchBackend::Brave {
                    api_key: KEY.into(),
                },
                SearchBackend::Searxng {
                    base_url: "https://searx.example".into(),
                },
            ])
            .execute(serde_json::json!({ "query": "q" }), &c)
            .await
            .unwrap();
            assert_eq!(out, "[]");
            assert_eq!(host.recorded_calls().len(), 2, "no request to the Location");
        }

        #[tokio::test]
        async fn host_denial_fails_over_to_next_backend() {
            let host = Arc::new(MockHost::new());
            host.push_err(ToolError::Execution(
                "Network policy denied: GET https://kagi.com/... not in allowed endpoints".into(),
            ));
            host.push_http(
                200,
                br#"{"results":[{"title":"A","url":"https://a","content":"s"}]}"#.to_vec(),
            );
            let (_i, c) = ctx(host.clone()).await;
            let out = WebSearch::new(vec![
                SearchBackend::Kagi {
                    api_key: KEY.into(),
                },
                SearchBackend::Searxng {
                    base_url: "https://searx.example".into(),
                },
            ])
            .execute(serde_json::json!({ "query": "q" }), &c)
            .await
            .unwrap();
            assert!(out.contains("https://a"));
            assert_eq!(host.recorded_calls().len(), 2);
        }

        #[tokio::test]
        async fn empty_results_do_not_fail_over() {
            let host = Arc::new(MockHost::new());
            host.push_http(200, br#"{"results":[]}"#.to_vec());
            let (_i, c) = ctx(host.clone()).await;
            let out = WebSearch::new(vec![
                SearchBackend::Searxng {
                    base_url: "https://searx.example".into(),
                },
                SearchBackend::DuckDuckGo,
            ])
            .execute(serde_json::json!({ "query": "q" }), &c)
            .await
            .unwrap();
            assert_eq!(out, "[]");
            assert_eq!(host.recorded_calls().len(), 1);
        }

        /// Never answers its first request; later requests get `200 []`.
        struct HangFirstHost(AtomicUsize);

        impl ToolHost for HangFirstHost {
            fn request<'a>(
                &'a self,
                _capability: &'a Capability,
                _grants: &'a Grants,
            ) -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>> + Send + 'a>>
            {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        std::future::pending::<()>().await;
                    }
                    Ok(CapabilityResult::HttpResponse(
                        crate::tool_host::HttpResponse {
                            status: 200,
                            headers: HashMap::new(),
                            body: br#"{"results":[]}"#.to_vec(),
                        },
                    ))
                })
            }

            fn name(&self) -> &str {
                "hang-first"
            }
        }

        #[tokio::test]
        async fn hung_backend_times_out_and_fails_over() {
            let host = Arc::new(HangFirstHost(AtomicUsize::new(0)));
            let (_i, c) = ctx(host.clone()).await;
            let tool = WebSearch::new(vec![
                SearchBackend::Brave {
                    api_key: KEY.into(),
                },
                SearchBackend::Searxng {
                    base_url: "https://searx.example".into(),
                },
            ])
            .with_backend_timeout(Duration::from_millis(50));
            let out = tokio::time::timeout(
                Duration::from_secs(10),
                tool.execute(serde_json::json!({ "query": "q" }), &c),
            )
            .await
            .expect("bounded by the per-backend timeout")
            .unwrap();
            assert_eq!(out, "[]");
            assert_eq!(host.0.load(Ordering::SeqCst), 2);

            // Alone, the hung backend's timeout is the tool's error.
            let host = Arc::new(HangFirstHost(AtomicUsize::new(0)));
            let (_i, c) = ctx(host).await;
            let err = WebSearch::new(vec![SearchBackend::Brave {
                api_key: KEY.into(),
            }])
            .with_backend_timeout(Duration::from_millis(50))
            .execute(serde_json::json!({ "query": "q" }), &c)
            .await
            .unwrap_err();
            assert!(matches!(err, ToolError::Network(ref m) if m.contains("brave: no response")));
        }

        #[test]
        fn default_backend_timeout_is_bounded_below_tool_timeout() {
            let tool = WebSearch::new(vec![]);
            assert_eq!(tool.backend_timeout, Duration::from_secs(15));
            assert!(tool.backend_timeout.as_secs() < tool.default_policy().timeout);
        }

        // ── Real host against a local listener ──

        /// A local HTTP listener that counts requests and answers each with
        /// an empty SearxNG result list.
        struct Listener {
            base_url: String,
            hits: Arc<AtomicUsize>,
        }

        async fn listener() -> Listener {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let sock = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = sock.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = hits.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = sock.accept().await else {
                        return;
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let body = r#"{"results":[{"title":"L","url":"https://l","content":"c"}]}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            });
            Listener {
                base_url: format!("http://{addr}"),
                hits,
            }
        }

        /// Run web_search on `NativeToolHost` with the grants the runtime
        /// would resolve for this call from `policy` and `c`'s ceilings.
        async fn run_native(
            c: &mut ToolContext,
            policy: &Grants,
            base_url: &str,
        ) -> Result<String, ToolError> {
            c.grants = c.resolve_call_grants(policy, "web_search");
            WebSearch::new(vec![SearchBackend::Searxng {
                base_url: base_url.into(),
            }])
            .execute(serde_json::json!({ "query": "exfil-me" }), c)
            .await
        }

        fn private_policy() -> Grants {
            net(Allowlist::Permissive, true)
        }

        #[tokio::test]
        async fn native_host_reaches_listener_without_a_ceiling() {
            // Positive control: the harness does observe a request.
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            let out = run_native(&mut c, &private_policy(), &l.base_url)
                .await
                .unwrap();
            assert!(out.contains("https://l"), "{out}");
            assert_eq!(l.hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn session_deny_all_sends_nothing() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            c.session_capabilities = deny_all();
            let err = run_native(&mut c, &private_policy(), &l.base_url)
                .await
                .unwrap_err();
            assert!(
                format!("{err}").contains("not in allowed endpoints"),
                "{err}"
            );
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn agent_deny_all_sends_nothing() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            c.agent_capabilities = deny_all();
            assert!(
                run_native(&mut c, &private_policy(), &l.base_url)
                    .await
                    .is_err()
            );
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn per_tool_deny_all_sends_nothing() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            c.agent_grants.insert("web_search".into(), deny_all());
            assert!(
                run_native(&mut c, &private_policy(), &l.base_url)
                    .await
                    .is_err()
            );
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn policy_deny_all_sends_nothing() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            assert!(run_native(&mut c, &deny_all(), &l.base_url).await.is_err());
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn deny_all_across_failover_returns_last_error_and_sends_nothing() {
            let a = listener().await;
            let b = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            c.session_capabilities = deny_all();
            c.grants = c.resolve_call_grants(&private_policy(), "web_search");
            let err = WebSearch::new(vec![
                SearchBackend::Searxng {
                    base_url: a.base_url.clone(),
                },
                SearchBackend::Searxng {
                    base_url: b.base_url.clone(),
                },
            ])
            .execute(serde_json::json!({ "query": "q" }), &c)
            .await
            .unwrap_err();
            let port_b = b.base_url.rsplit(':').next().unwrap();
            assert!(format!("{err}").contains(port_b), "last error: {err}");
            assert_eq!(a.hits.load(Ordering::SeqCst), 0);
            assert_eq!(b.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn private_searxng_without_grant_is_denied_with_fix() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            let err = run_native(&mut c, &Grants::default(), &l.base_url)
                .await
                .unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("SSRF protection"), "{msg}");
            assert!(
                msg.contains("tool_policies.web_search.grants.network.allow_private"),
                "{msg}"
            );
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn private_grant_cannot_be_widened_past_a_ceiling() {
            // The per-tool policy allows private, the session ceiling does not.
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            c.session_capabilities = net(Allowlist::Permissive, false);
            assert!(
                run_native(&mut c, &private_policy(), &l.base_url)
                    .await
                    .is_err()
            );
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn private_grant_respects_endpoint_restrictions() {
            let l = listener().await;
            let (_i, mut c) = ctx(Arc::new(NativeToolHost)).await;
            let other_host = net(
                Allowlist::Only(vec![EndpointPattern {
                    host: "searx.example".into(),
                    ..EndpointPattern::default()
                }]),
                true,
            );
            assert!(run_native(&mut c, &other_host, &l.base_url).await.is_err());
            assert_eq!(l.hits.load(Ordering::SeqCst), 0);
        }

        #[test]
        fn startup_check_flags_only_unauthorized_private_searxng() {
            let backends = vec![
                SearchBackend::Searxng {
                    base_url: "http://127.0.0.1:8888".into(),
                },
                SearchBackend::Searxng {
                    base_url: "http://searx.local".into(),
                },
                SearchBackend::Searxng {
                    base_url: "https://searx.example".into(),
                },
                SearchBackend::DuckDuckGo,
            ];
            assert_eq!(
                unauthorized_private_backends(&backends, &Grants::default()),
                vec!["http://127.0.0.1:8888", "http://searx.local"]
            );
            assert!(unauthorized_private_backends(&backends, &private_policy()).is_empty());
        }
    }
}

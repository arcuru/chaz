//! Web search tool.
//!
//! Supports a few API-backed providers (Kagi, Tavily, Brave, Serper), any
//! SearxNG instance (self-hosted or public) at a configured URL, and falls
//! back to parsing DuckDuckGo's HTML results page when no API key is
//! configured. The tool holds an ordered preference list of backends and
//! tries each in turn, failing over to the next on any error. The last
//! entry is the final answer — if it also errors, that error is returned.

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

pub struct WebSearch {
    /// Non-empty ordered preference list. Tried in sequence on failure.
    /// An empty `new()` input is coerced to a single DuckDuckGo entry so
    /// the tool always has a fallback.
    backends: Vec<SearchBackend>,
}

impl WebSearch {
    pub fn new(backends: Vec<SearchBackend>) -> Self {
        let backends = if backends.is_empty() {
            vec![SearchBackend::DuckDuckGo]
        } else {
            backends
        };
        Self { backends }
    }

    fn http_client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            // DuckDuckGo blocks bot UAs; use a generic browser UA for all backends.
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0")
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {e}"))
    }
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
        _ctx: &'a ToolContext,
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

            let client = Self::http_client().map_err(ToolError::Execution)?;
            let results = try_backends(&self.backends, &client, query, max_results).await?;

            debug!(count = results.len(), "Search results");
            serde_json::to_string(&results)
                .map_err(|e| ToolError::Execution(format!("Failed to serialize results: {e}")))
        })
    }
}

/// Run the query against each backend in order, returning the first `Ok`.
/// If every backend errors, returns the last error. The chain is assumed
/// non-empty (`WebSearch::new` guarantees this).
async fn try_backends(
    backends: &[SearchBackend],
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    try_backends_with(backends, |backend| async move {
        match backend {
            SearchBackend::Kagi { api_key } => {
                kagi_search(client, api_key, query, max_results).await
            }
            SearchBackend::Tavily { api_key } => {
                tavily_search(client, api_key, query, max_results).await
            }
            SearchBackend::Brave { api_key } => {
                brave_search(client, api_key, query, max_results).await
            }
            SearchBackend::Serper { api_key } => {
                serper_search(client, api_key, query, max_results).await
            }
            SearchBackend::Searxng { base_url } => {
                searxng_search(client, base_url, query, max_results).await
            }
            SearchBackend::DuckDuckGo => duckduckgo_search(client, query, max_results).await,
        }
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
// Provider implementations
// ================================================================

use crate::tool::ToolError;

async fn kagi_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    // Kagi's Search API is GET /api/v0/search?q=&limit= with
    // `Authorization: Bot <token>` (literal word "Bot", not "Bearer").
    // Response `data[]` is a discriminated list; `t: 0` entries are
    // search results (url/title/snippet), `t: 1` is related searches.
    let response = client
        .get("https://kagi.com/api/v0/search")
        .query(&[("q", query), ("limit", &max_results.to_string())])
        .header("Authorization", format!("Bot {api_key}"))
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("kagi: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("kagi: {e}")))?;
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "kagi returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("kagi: bad JSON: {e}")))?;
    Ok(parse_kagi_results(&parsed, max_results))
}

/// Extract search-result (`t: 0`) entries from a Kagi response. Pulled out
/// so it can be unit-tested against a canned response without HTTP.
fn parse_kagi_results(parsed: &Value, max_results: usize) -> Vec<SearchResult> {
    parsed
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|r| r.get("t").and_then(|t| t.as_u64()) == Some(0))
                .take(max_results)
                .map(|r| SearchResult {
                    title: r
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: r
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    snippet: r
                        .get("snippet")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn tavily_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "max_results": max_results,
    });
    let response = client
        .post("https://api.tavily.com/search")
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("tavily: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("tavily: {e}")))?;
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "tavily returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("tavily: bad JSON: {e}")))?;
    Ok(parsed
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| SearchResult {
                    title: r
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: r
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    snippet: r
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn brave_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("brave: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("brave: {e}")))?;
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "brave returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("brave: bad JSON: {e}")))?;
    Ok(parsed
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| SearchResult {
                    title: r
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: r
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    snippet: r
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn serper_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    let body = serde_json::json!({ "q": query, "num": max_results });
    let response = client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("serper: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("serper: {e}")))?;
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "serper returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("serper: bad JSON: {e}")))?;
    Ok(parsed
        .get("organic")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| SearchResult {
                    title: r
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: r
                        .get("link")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    snippet: r
                        .get("snippet")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn searxng_search(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    // SearxNG exposes both HTML and JSON on the same `/search` endpoint;
    // `format=json` selects JSON. The instance must have the JSON output
    // format enabled (default on `searxng` ≥ 2022 but some public instances
    // disable it — error messages from the server are informative).
    let trimmed = base_url.trim_end_matches('/');
    let url = format!("{trimmed}/search");
    let response = client
        .get(&url)
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("searxng: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("searxng: {e}")))?;
    if !status.is_success() {
        return Err(ToolError::Execution(format!(
            "searxng returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("searxng: bad JSON: {e}")))?;
    Ok(parse_searxng_results(&parsed, max_results))
}

/// Pull result entries out of a SearxNG JSON response. `content` is the
/// snippet field upstream; we normalize to `snippet`.
fn parse_searxng_results(parsed: &Value, max_results: usize) -> Vec<SearchResult> {
    parsed
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| SearchResult {
                    title: r
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: r
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    snippet: r
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn duckduckgo_search(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, ToolError> {
    // DDG blocks GET requests with non-browser UAs; POST with form data works reliably.
    let response = client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| ToolError::Network(format!("duckduckgo: {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| ToolError::Network(format!("duckduckgo: {e}")))?;
    // DDG returns 202 when rate-limiting or blocking the request; treat it as
    // an error so the failover chain can try the next backend.
    if status.as_u16() != 200 {
        return Err(ToolError::Execution(format!(
            "duckduckgo returned HTTP {status}"
        )));
    }

    let results = parse_duckduckgo_html(&body, max_results);
    if results.is_empty() {
        warn!("duckduckgo returned no parseable results (HTML may have changed)");
        return Err(ToolError::Execution(
            "duckduckgo returned no results (may be rate-limited or HTML changed)".into(),
        ));
    }
    Ok(results)
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
}

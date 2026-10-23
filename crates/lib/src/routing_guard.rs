//! Regression guard: session-time tool and extension code reaches the OS and
//! the network only through [`ToolHost`](crate::tool_host::ToolHost).
//!
//! A tool that opens its own HTTP client, file, socket, or process skips the
//! grants the runtime resolved for the call, so session, agent and per-tool
//! ceilings stop binding it. This test scans the non-test source of
//! `src/tools/` and `src/extensions/` for direct OS/network APIs and fails on
//! any use that is not in [`EXEMPTIONS`].
//!
//! It is a tripwire for accidental regressions, not a sandbox: it matches
//! source text, so an alias or a helper outside the scanned trees evades it.
//! Behavioural enforcement is the host's grant check and its tests. Daemon
//! infrastructure (model providers, embedding, MCP transport, config, secrets,
//! bridges) legitimately uses these APIs and is deliberately not scanned.

use regex::Regex;
use std::path::{Path, PathBuf};

/// Directories (relative to the crate root) holding session-time tool code.
const SCANNED_DIRS: &[&str] = &["src/tools", "src/extensions"];

/// A direct API use allowed in one named function of one file.
struct Exemption {
    file: &'static str,
    function: &'static str,
    api: &'static str,
    reason: &'static str,
}

const EXEMPTIONS: &[Exemption] = &[
    Exemption {
        file: "src/extensions/skills.rs",
        function: "scan_paths",
        api: "std::fs::",
        reason: "startup catalog scan of the fixed operator skill directories; \
                 trusted shared catalog content, not a session FileRead",
    },
    Exemption {
        file: "src/extensions/skills.rs",
        function: "parse_skill_md",
        api: "std::fs::",
        reason: "reads one catalog file found by scan_paths at startup; \
                 trusted shared catalog content, not a session FileRead",
    },
];

fn patterns() -> &'static [Regex] {
    static PATTERNS: std::sync::OnceLock<Vec<Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"\b(std|tokio)::(fs|process|net)\b",
            r"\breqwest::",
            r"\buse (std|tokio)::\{[^}]*\b(fs|process|net)\b",
            r"\bFile::(open|create)\b",
            r"\bCommand::new\b",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("guard pattern is valid"))
        .collect()
    })
}

#[derive(Debug, Clone, PartialEq)]
struct Violation {
    line: usize,
    function: Option<String>,
    text: String,
}

/// Non-test, non-comment lines of `src` with their 1-based line numbers.
///
/// A column-0 `#[cfg(test)]` drops the item after it: `mod x;` (with any
/// attributes such as `#[path]`), a single-line item ending in `;`, or a
/// `mod x {` block through its rustfmt-placed column-0 `}`. Any other gated
/// item is still scanned, so an unusual layout errs towards a false alarm.
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut gated = false;
    let mut in_test_mod = false;
    for (idx, line) in src.lines().enumerate() {
        if in_test_mod {
            in_test_mod = line != "}";
            continue;
        }
        if line.starts_with("#[cfg(test)]") {
            gated = true;
            continue;
        }
        let trimmed = line.trim();
        if gated {
            if trimmed.starts_with("#[") {
                continue;
            }
            gated = false;
            if trimmed.ends_with(';') {
                continue;
            }
            if trimmed.ends_with('{') && trimmed.split_whitespace().any(|w| w == "mod") {
                in_test_mod = true;
                continue;
            }
        }
        if trimmed.starts_with("//") {
            continue;
        }
        out.push((idx + 1, line));
    }
    out
}

/// Direct OS/network API uses in the production part of `src`, each tagged
/// with the most recent `fn` declared above it.
fn violations(src: &str) -> Vec<Violation> {
    static FN_DECL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let fn_decl = FN_DECL.get_or_init(|| Regex::new(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    let mut function = None;
    let mut out = Vec::new();
    for (line, text) in production_lines(src) {
        if let Some(caps) = fn_decl.captures(text) {
            function = Some(caps[1].to_string());
        }
        if patterns().iter().any(|p| p.is_match(text)) {
            out.push(Violation {
                line,
                function: function.clone(),
                text: text.trim().to_string(),
            });
        }
    }
    out
}

/// Split `found` (from `file`) into unexempted violations, counting each
/// exemption that matched in `used` (indexed like [`EXEMPTIONS`]).
fn apply_exemptions(file: &str, found: Vec<Violation>, used: &mut [usize]) -> Vec<Violation> {
    found
        .into_iter()
        .filter(|v| {
            let hit = EXEMPTIONS.iter().position(|e| {
                e.file == file
                    && v.function.as_deref() == Some(e.function)
                    && v.text.contains(e.api)
            });
            if let Some(i) = hit {
                used[i] += 1;
            }
            hit.is_none()
        })
        .collect()
}

/// Exemptions that no longer match anything: their reason is out of date.
fn stale_exemptions(used: &[usize]) -> Vec<String> {
    EXEMPTIONS
        .iter()
        .zip(used)
        .filter(|(_, n)| **n == 0)
        .map(|(e, _)| format!("{} fn {} `{}`", e.file, e.function, e.api))
        .collect()
}

/// `tests.rs` and `*_tests.rs` hold `#[cfg(test)] mod` bodies declared by a
/// sibling module file.
fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "tests.rs" || n.ends_with("_tests.rs"))
}

fn crate_root() -> PathBuf {
    // Prefer the runtime value: test runners that relocate the workspace
    // (nextest `--workspace-remap`) set it to the current source tree.
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn session_tool_code_routes_os_and_network_through_tool_host() {
    let root = crate_root();
    let mut files = Vec::new();
    for dir in SCANNED_DIRS {
        rust_files(&root.join(dir), &mut files);
    }
    let names: Vec<String> = files.iter().map(|f| relative(&root, f)).collect();
    assert!(
        names.iter().any(|n| n == "src/tools/search.rs"),
        "guard is not scanning the tool tree under {}",
        root.display()
    );

    let mut used = vec![0; EXEMPTIONS.len()];
    let mut failures = Vec::new();
    for (path, name) in files.iter().zip(&names) {
        if is_test_file(path) {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        for v in apply_exemptions(name, violations(&src), &mut used) {
            failures.push(format!("{name}:{}: {}", v.line, v.text));
        }
    }
    assert!(
        failures.is_empty(),
        "direct OS/network API in session-time tool code; request a \
         `Capability` through `ctx.host()` with `ctx.grants()` instead \
         (or, for genuinely non-session code, add a narrow explained exemption):\n{}",
        failures.join("\n")
    );
    let stale = stale_exemptions(&used);
    assert!(
        stale.is_empty(),
        "stale routing-guard exemptions (remove them):\n{}",
        stale.join("\n")
    );
}

/// Every skipped test file must be declared only under `#[cfg(test)]`, or
/// the guard would be skipping production code.
#[test]
fn skipped_test_files_are_declared_only_under_cfg_test() {
    let root = crate_root();
    let mut files = Vec::new();
    for dir in SCANNED_DIRS {
        rust_files(&root.join(dir), &mut files);
    }
    for path in files.iter().filter(|p| is_test_file(p)) {
        let file = path.file_name().unwrap().to_str().unwrap();
        let (parent, needle) = if file == "tests.rs" {
            let dir = path.parent().unwrap();
            (dir.with_extension("rs"), "mod tests".to_string())
        } else {
            let stem = file.trim_end_matches("_tests.rs");
            (
                path.with_file_name(format!("{stem}.rs")),
                format!("\"{file}\""),
            )
        };
        let src = std::fs::read_to_string(&parent).unwrap_or_else(|e| {
            panic!(
                "{}: no parent module {}: {e}",
                path.display(),
                parent.display()
            )
        });
        assert!(
            src.contains(&needle),
            "{} does not declare {file}",
            parent.display()
        );
        assert!(
            production_lines(&src)
                .iter()
                .all(|(_, l)| !l.contains(&needle)),
            "{} declares {file} outside #[cfg(test)]",
            parent.display()
        );
    }
}

// ── Scanner self-tests (planted violations) ──

#[test]
fn planted_direct_io_is_flagged() {
    for planted in [
        "fn f() { let c = reqwest::Client::new(); }",
        "use std::fs;",
        "use std::{fs, io};",
        "use tokio::{net::TcpStream, io};",
        "fn f() { std::net::TcpStream::connect(a); }",
        "fn f() { tokio::process::Command::new(\"sh\"); }",
        "fn f() { Command::new(\"sh\"); }",
        "fn f() { File::open(p); }",
        "fn f() { tokio::fs::read(p).await; }",
    ] {
        assert_eq!(violations(planted).len(), 1, "not flagged: {planted}");
    }
}

#[test]
fn comments_and_test_modules_are_ignored_but_later_code_is_not() {
    let src = "\
// reqwest::Client::new() in a comment
/// Doc mentioning std::fs::read.
fn ok() {}

#[cfg(test)]
mod inline_tests {
    use std::fs;
    fn helper() {
        std::fs::write(p, b\"x\").unwrap();
    }
}

#[cfg(test)]
#[path = \"x_tests.rs\"]
mod x_tests;

fn after_tests() {
    let _ = std::fs::read(p);
}
";
    let found = violations(src);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].function.as_deref(), Some("after_tests"));
    assert_eq!(found[0].line, 18);
}

#[test]
fn reintroduced_private_search_client_is_flagged() {
    let src = "\
impl WebSearch {
    fn http_client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder().build().map_err(|e| e.to_string())
    }
}
";
    let mut used = vec![0; EXEMPTIONS.len()];
    let left = apply_exemptions("src/tools/search.rs", violations(src), &mut used);
    assert_eq!(left.len(), 2, "{left:?}");
}

#[test]
fn skills_exemption_is_limited_to_catalog_functions_and_api() {
    let src = "\
fn scan_paths() {
    let _ = std::fs::read_dir(d);
    let _ = reqwest::get(u);
}
fn parse_skill_md() {
    let _ = std::fs::read_to_string(p);
}
fn skill_show() {
    let _ = std::fs::read_to_string(p);
}
";
    let mut used = vec![0; EXEMPTIONS.len()];
    let left = apply_exemptions("src/extensions/skills.rs", violations(src), &mut used);
    let left: Vec<_> = left
        .iter()
        .map(|v| (v.function.as_deref(), v.line))
        .collect();
    assert_eq!(left, vec![(Some("scan_paths"), 3), (Some("skill_show"), 9)]);
    assert!(stale_exemptions(&used).is_empty());

    // The same functions in another file get no exemption.
    let mut used = vec![0; EXEMPTIONS.len()];
    let left = apply_exemptions("src/extensions/other.rs", violations(src), &mut used);
    assert_eq!(left.len(), 4);
}

#[test]
fn exemption_without_a_match_is_stale() {
    let src = "fn scan_paths() { let _ = walk(d); }\nfn parse_skill_md() {}\n";
    let mut used = vec![0; EXEMPTIONS.len()];
    assert!(apply_exemptions("src/extensions/skills.rs", violations(src), &mut used).is_empty());
    assert_eq!(stale_exemptions(&used).len(), EXEMPTIONS.len());
    for e in EXEMPTIONS {
        assert!(!e.reason.is_empty());
    }
}

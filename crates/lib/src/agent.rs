use crate::config::{AgentConfig, AgentPreset, Config, WorkerConfig};
use crate::grants::Grants;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Resolve an agent's or worker's effective system prompt: read each
/// `system_prompt_files` entry in order, concatenate their contents, then
/// append the inline `system_prompt`. This matches the documented assembly
/// order — files first, the inline string last. `~` / `~/…` are expanded
/// against the home directory.
///
/// A file that can't be read is logged at `warn` and skipped, so a stale or
/// missing path degrades the prompt rather than failing agent construction.
/// Called from the runtime `Agent` / `Worker` constructors, so the files are
/// re-read whenever an agent is (re)built — including the per-message
/// `hydrate_agent_from_db` path — keeping edits to the prompt files live
/// without a restart.
fn resolve_system_prompt(inline: &str, files: &[PathBuf]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(files.len() + 1);
    for path in files {
        let resolved = expand_home(path);
        match std::fs::read_to_string(&resolved) {
            Ok(content) => {
                let trimmed = content.trim_end();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Err(e) => tracing::warn!(
                path = %resolved.display(),
                error = %e,
                "system_prompt_files: skipping unreadable prompt file"
            ),
        }
    }
    if !inline.is_empty() {
        parts.push(inline.to_string());
    }
    parts.join("\n\n")
}

/// Expand a leading `~` / `~/…` in `path` against the home directory.
/// Returns `path` unchanged when there is no leading tilde or no home dir.
fn expand_home(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Agent definition — first-class entity with persistent identity, sessions,
/// schedules, memory, and a set of Worker templates this Agent can invoke.
///
/// See `~/brain/ava/research/chaz-ecosystem/conceptual-model.md` for the
/// Peer / Agent / Worker / Resource model.
#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub system_prompt: String,
    pub system_prompt_files: Vec<PathBuf>,
    pub default_model: Option<String>,
    /// Tool names this agent can use. None = all tools (no filtering).
    pub allowed_tools: Option<Vec<String>>,
    /// Worker templates this Agent can invoke via `spawn_worker`. Keyed by
    /// Worker name (unique within the Agent). Lookup is scoped — Workers
    /// are NOT in a global registry; each Agent sees only its own.
    pub workers: HashMap<String, Worker>,
    /// Maximum ReAct loop iterations.
    pub max_iterations: u32,
    /// Whether this agent can run without user input.
    pub autonomous: bool,
    /// Named override bundles for spawn-time configuration.
    pub presets: HashMap<String, AgentPreset>,
    /// Tool profile name (references a key in top-level tool_profiles config).
    pub tool_profile: Option<String>,
    /// Override context token limit for this agent (None = use global default).
    pub max_context_tokens: Option<usize>,
    /// Per-tool grant overrides. Merged per-kind over the config-level grants
    /// at tool-call time (see `Grants::merge_over`).
    pub grants: HashMap<String, Grants>,
}

/// Worker template owned by a single Agent. A Worker is a configured
/// one-shot LLM call — a tool, from the perspective of the Agent that
/// invokes it via `spawn_worker`.
///
/// Workers have no identity, no keys, and no persistent state of their
/// own. Entries written during a Worker invocation are signed by the
/// parent Agent's key. Optional fields fall back to the parent Agent's
/// defaults at spawn time (Stage C wires the fallback).
#[derive(Clone, Debug)]
pub struct Worker {
    pub name: String,
    pub system_prompt: String,
    pub system_prompt_files: Vec<PathBuf>,
    /// Override the model. None = inherit parent Agent's default_model.
    pub default_model: Option<String>,
    /// Tool names this Worker may use. None = inherit parent Agent's
    /// allowed_tools. When set, narrowed against the parent's list at
    /// spawn time (intersection).
    pub allowed_tools: Option<Vec<String>>,
    /// Worker-level cap on ReAct iterations. **Ignored when invoked
    /// under a parent Agent's iteration budget** — nested Workers share
    /// the top-level Agent's atomic counter (see
    /// `ToolContext::iteration_budget`) rather than each level getting a
    /// fresh allotment. Used only when no parent budget is in scope
    /// (test paths via direct `runtime::execute` calls).
    pub max_iterations: Option<u32>,
    /// Named override bundles selectable via the `preset` arg of `spawn_worker`.
    pub presets: HashMap<String, AgentPreset>,
}

impl Worker {
    pub fn from_worker_config(cfg: &WorkerConfig) -> Self {
        let system_prompt_files: Vec<PathBuf> = cfg
            .system_prompt_files
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Worker {
            name: cfg.name.clone(),
            system_prompt: resolve_system_prompt(
                &cfg.system_prompt.clone().unwrap_or_default(),
                &system_prompt_files,
            ),
            system_prompt_files,
            default_model: cfg.model.clone(),
            allowed_tools: cfg.tools.clone(),
            max_iterations: cfg.max_iterations,
            presets: cfg.presets.clone().unwrap_or_default(),
        }
    }

    pub fn from_worker_db_config(cfg: &crate::agent_db::WorkerDbConfig) -> Self {
        let system_prompt_files: Vec<PathBuf> = cfg
            .system_prompt_files
            .clone()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Worker {
            name: cfg.name.clone(),
            system_prompt: resolve_system_prompt(&cfg.system_prompt, &system_prompt_files),
            system_prompt_files,
            default_model: cfg.model.clone(),
            allowed_tools: cfg.tools.clone(),
            max_iterations: cfg.max_iterations,
            presets: cfg.presets.clone(),
        }
    }
}

/// Resolved overrides for a spawn_agent call.
/// All fields are final values after applying: definition defaults → preset → inline overrides.
pub struct ResolvedOverrides {
    pub model: Option<String>,
    pub max_iterations: u32,
    pub allowed_tools: Option<Vec<String>>,
    pub role_suffix: Option<String>,
    pub tool_profile: Option<String>,
}

impl Agent {
    fn from_agent_config(agent_config: &AgentConfig, _config: &Config) -> Self {
        let workers = agent_config
            .workers
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|wc| (wc.name.clone(), Worker::from_worker_config(wc)))
            .collect();
        let system_prompt_files: Vec<PathBuf> = agent_config
            .system_prompt_files
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Agent {
            name: agent_config.name.clone(),
            system_prompt: resolve_system_prompt(
                &agent_config.system_prompt.clone().unwrap_or_default(),
                &system_prompt_files,
            ),
            system_prompt_files,
            default_model: agent_config.model.clone(),
            allowed_tools: agent_config.tools.clone(),
            workers,
            max_iterations: agent_config.max_iterations.unwrap_or(10),
            autonomous: agent_config.autonomous,
            presets: agent_config.presets.clone().unwrap_or_default(),
            tool_profile: agent_config.tool_profile.clone(),
            max_context_tokens: agent_config.max_context_tokens,
            grants: agent_config.grants.clone().unwrap_or_default(),
        }
    }

    /// Build a runtime Agent from a Living Agent's DB config. Used by
    /// `/agent new` and `/agent import`.
    pub fn from_db_config(name: &str, cfg: &crate::agent_db::AgentDbConfig) -> Self {
        let workers = cfg
            .workers
            .iter()
            .map(|wc| (wc.name.clone(), Worker::from_worker_db_config(wc)))
            .collect();
        let system_prompt_files: Vec<PathBuf> = cfg
            .system_prompt_files
            .clone()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Agent {
            name: name.to_string(),
            system_prompt: resolve_system_prompt(&cfg.system_prompt, &system_prompt_files),
            system_prompt_files,
            default_model: cfg.model.clone(),
            allowed_tools: cfg.tools.clone(),
            workers,
            max_iterations: cfg.max_iterations.unwrap_or(10),
            autonomous: cfg.autonomous,
            presets: cfg.presets.clone(),
            tool_profile: cfg.tool_profile.clone(),
            max_context_tokens: cfg.max_context_tokens,
            grants: cfg.grants.clone(),
        }
    }

    /// Resolve a Worker template by name within this Agent's scope. Returns
    /// `None` if no Worker with that name is declared on this Agent — there
    /// is no fallback to other Agents.
    pub fn find_worker(&self, name: &str) -> Option<&Worker> {
        self.workers.get(name)
    }

    /// Resolve overrides from a preset name and/or inline overrides.
    /// Resolution order: agent defaults → preset → inline overrides.
    pub fn resolve_overrides(
        &self,
        preset: Option<&str>,
        model_override: Option<&str>,
        max_iterations_override: Option<u32>,
        tools_override: Option<&[String]>,
    ) -> ResolvedOverrides {
        let mut model = self.default_model.clone();
        let mut max_iterations = self.max_iterations;
        let mut allowed_tools = self.allowed_tools.clone();
        let mut role_suffix = None;
        let mut tool_profile = self.tool_profile.clone();

        // Apply preset if specified
        if let Some(preset_name) = preset
            && let Some(p) = self.presets.get(preset_name)
        {
            if let Some(ref m) = p.model {
                model = Some(m.clone());
            }
            if let Some(mi) = p.max_iterations {
                max_iterations = mi;
            }
            if let Some(ref t) = p.tools {
                // Preset tools must be subset of definition's allowed tools
                allowed_tools = Some(intersect_tools(&allowed_tools, t));
            }
            role_suffix = p.role_suffix.clone();
            if let Some(ref tp) = p.tool_profile {
                tool_profile = Some(tp.clone());
            }
        }

        // Apply inline overrides
        if let Some(m) = model_override {
            model = Some(m.to_string());
        }
        if let Some(mi) = max_iterations_override {
            max_iterations = mi;
        }
        if let Some(t) = tools_override {
            // Inline tools must be subset of current allowed tools
            allowed_tools = Some(intersect_tools(&allowed_tools, t));
        }

        ResolvedOverrides {
            model,
            max_iterations,
            allowed_tools,
            role_suffix,
            tool_profile,
        }
    }
}

/// Intersect a tool override list with an existing allowlist.
/// If base is None (all tools), the override becomes the allowlist.
/// If both are set, only tools in both lists are kept.
fn intersect_tools(base: &Option<Vec<String>>, override_tools: &[String]) -> Vec<String> {
    match base {
        None => override_tools.to_vec(),
        Some(base_tools) => override_tools
            .iter()
            .filter(|t| base_tools.contains(t))
            .cloned()
            .collect(),
    }
}

/// Registry of named agents. Callers (production: `main.rs`) are
/// responsible for ensuring at least one agent is registered before the
/// registry is used for routing — `default_agent()` panics on empty.
///
/// Backed by a `RwLock` so agents can be registered at runtime (via
/// `/agent new`); yaml-bootstrapped agents land here at startup, and
/// subsequently-created Living Agents join via [`AgentRegistry::register`].
/// All read accessors clone out `Agent` values — Agent is cheap to clone.
pub struct AgentRegistry {
    agents: std::sync::RwLock<Vec<Agent>>,
}

impl AgentRegistry {
    /// Build the registry from config. Maps `config.agents` 1:1 — an
    /// absent or empty list yields an empty registry. The `chaz`-with-no-
    /// `agents:`-block UX lives in `main.rs`, which calls
    /// [`AgentRegistry::register_default_chaz`] when this returns empty.
    pub fn from_config(config: &Config) -> Self {
        let agents = config
            .agents
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|ac| Agent::from_agent_config(ac, config))
            .collect();

        Self {
            agents: std::sync::RwLock::new(agents),
        }
    }

    /// Build a registry containing a single bare-bones agent named
    /// `"default"`. Test-only seam: anywhere production code expects a
    /// non-empty registry to satisfy a type, this is the cheapest way to
    /// produce one without dragging a [`Config`] through.
    #[cfg(test)]
    pub fn with_default_agent() -> Self {
        Self {
            agents: std::sync::RwLock::new(vec![Agent {
                name: "default".to_string(),
                system_prompt: String::new(),
                system_prompt_files: vec![],
                default_model: None,
                allowed_tools: None,
                workers: HashMap::new(),
                max_iterations: 10,
                autonomous: false,
                presets: HashMap::new(),
                tool_profile: None,
                max_context_tokens: None,
                grants: HashMap::new(),
            }]),
        }
    }

    /// Synthesize and register a `"chaz"` agent as the fallback when no
    /// agents are declared in config. Called by `main.rs` when
    /// [`from_config`] yields an empty registry.
    pub fn register_default_chaz(&self, _config: &Config) -> anyhow::Result<()> {
        self.register(Agent {
            name: "chaz".to_string(),
            system_prompt: String::new(),
            system_prompt_files: vec![],
            default_model: None,
            allowed_tools: None,
            workers: HashMap::new(),
            max_iterations: 10,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        })
    }

    /// Whether the registry has zero agents. Production uses this at
    /// startup to decide whether to synthesize the legacy default
    /// `"chaz"` agent.
    pub fn is_empty(&self) -> bool {
        self.agents.read().unwrap().is_empty()
    }

    /// Get the default agent (first in the list). Panics if the registry is
    /// empty — callers rely on `from_config` always seeding at least one.
    pub fn default_agent(&self) -> Agent {
        let agents = self.agents.read().unwrap();
        agents
            .first()
            .cloned()
            .expect("AgentRegistry always has at least one agent")
    }

    /// Look up an agent by name. Returns a cloned `Agent` to keep callers
    /// lock-free after the read.
    pub fn get(&self, name: &str) -> Option<Agent> {
        let agents = self.agents.read().unwrap();
        agents.iter().find(|a| a.name == name).cloned()
    }

    /// List all agent names (cloned).
    pub fn names(&self) -> Vec<String> {
        let agents = self.agents.read().unwrap();
        agents.iter().map(|a| a.name.clone()).collect()
    }

    /// Register a new agent at runtime. Rejects duplicates by display name.
    /// Used by `/agent new` and `/agent import` so Living Agents created
    /// after startup are routable without a config-reload.
    pub fn register(&self, agent: Agent) -> anyhow::Result<()> {
        let mut agents = self.agents.write().unwrap();
        if agents.iter().any(|a| a.name == agent.name) {
            anyhow::bail!("Agent '{}' already registered", agent.name);
        }
        agents.push(agent);
        Ok(())
    }

    /// Replace the runtime entry for `name` — or insert if absent. Used by
    /// live hydration so edits to `AgentDb::config` propagate into the
    /// in-memory registry at resolution time.
    pub fn upsert(&self, agent: Agent) {
        let mut agents = self.agents.write().unwrap();
        if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
            *existing = agent;
        } else {
            agents.push(agent);
        }
    }

    /// Remove the runtime entry for `name`. Returns `true` if an entry was
    /// removed, `false` if no agent by that name existed. Used by `/agent
    /// delete`; the DB-side record survives.
    pub fn unregister(&self, name: &str) -> bool {
        let mut agents = self.agents.write().unwrap();
        let before = agents.len();
        agents.retain(|a| a.name != name);
        agents.len() != before
    }

    /// Build a runtime `Agent` from a Living Agent's DB config.
    /// Delegates to [`Agent::from_db_config`].
    pub fn build_from_db_config(&self, name: &str, cfg: &crate::agent_db::AgentDbConfig) -> Agent {
        Agent::from_db_config(name, cfg)
    }

    /// Build a runtime `Agent` by looking up `name` in `config.agents`.
    /// Public reload path — used by the TUI's Peer→Agents `[r]` action so
    /// edits to system_prompt / default_model / tools / etc. in the yaml
    /// can be applied to one agent without restarting chaz. DB-side
    /// state for Living Agents is left alone; yaml only owns the
    /// declarative fields. Returns `None` when no yaml entry matches.
    pub fn build_from_yaml(&self, name: &str, config: &Config) -> Option<Agent> {
        config
            .agents
            .as_deref()?
            .iter()
            .find(|a| a.name == name)
            .map(|ac| Agent::from_agent_config(ac, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resolve_concatenates_files_then_inline() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "FILE A\n").unwrap();
        std::fs::write(&b, "FILE B\n").unwrap();

        let out = resolve_system_prompt("INLINE", &[a, b]);
        // Order: files in declared order, inline last; blank line between parts.
        assert_eq!(out, "FILE A\n\nFILE B\n\nINLINE");
    }

    #[test]
    fn resolve_inline_only_when_no_files() {
        assert_eq!(resolve_system_prompt("just inline", &[]), "just inline");
    }

    #[test]
    fn resolve_files_only_when_inline_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.md");
        let mut fh = std::fs::File::create(&f).unwrap();
        write!(fh, "ONLY FILE").unwrap();
        assert_eq!(resolve_system_prompt("", &[f]), "ONLY FILE");
    }

    #[test]
    fn resolve_skips_missing_file_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.md");
        std::fs::write(&present, "PRESENT").unwrap();
        let missing = dir.path().join("nope.md");

        // Missing file is dropped; the rest still assembles.
        let out = resolve_system_prompt("INLINE", &[missing, present]);
        assert_eq!(out, "PRESENT\n\nINLINE");
    }

    #[test]
    fn expand_home_rewrites_leading_tilde() {
        let home = dirs::home_dir().expect("home dir in test env");
        assert_eq!(
            expand_home(Path::new("~/brain/x.md")),
            home.join("brain/x.md")
        );
        assert_eq!(expand_home(Path::new("~")), home);
        // No tilde → untouched; mid-path tilde is not expanded.
        assert_eq!(
            expand_home(Path::new("/abs/p.md")),
            PathBuf::from("/abs/p.md")
        );
        assert_eq!(expand_home(Path::new("/a/~/b")), PathBuf::from("/a/~/b"));
    }

    fn make_agent(name: &str) -> Agent {
        Agent {
            name: name.to_string(),
            system_prompt: String::new(),
            system_prompt_files: vec![],
            default_model: None,
            allowed_tools: None,
            workers: HashMap::new(),
            max_iterations: 10,
            autonomous: false,
            presets: HashMap::new(),
            tool_profile: None,
            max_context_tokens: None,
            grants: HashMap::new(),
        }
    }

    #[test]
    fn test_resolve_overrides_defaults() {
        let agent = make_agent("test");
        let resolved = agent.resolve_overrides(None, None, None, None);
        assert_eq!(resolved.model, None);
        assert_eq!(resolved.max_iterations, 10);
        assert!(resolved.allowed_tools.is_none());
        assert!(resolved.role_suffix.is_none());
    }

    #[test]
    fn test_resolve_overrides_preset() {
        let mut agent = make_agent("test");
        agent.default_model = Some("sonnet".to_string());
        agent.presets.insert(
            "deep".to_string(),
            AgentPreset {
                model: Some("opus".to_string()),
                max_iterations: Some(40),
                tools: None,
                role_suffix: Some("Be thorough.".to_string()),
                tool_profile: None,
            },
        );

        let resolved = agent.resolve_overrides(Some("deep"), None, None, None);
        assert_eq!(resolved.model.as_deref(), Some("opus"));
        assert_eq!(resolved.max_iterations, 40);
        assert_eq!(resolved.role_suffix.as_deref(), Some("Be thorough."));
    }

    #[test]
    fn test_resolve_overrides_inline_wins() {
        let mut agent = make_agent("test");
        agent.presets.insert(
            "deep".to_string(),
            AgentPreset {
                model: Some("opus".to_string()),
                max_iterations: Some(40),
                tools: None,
                role_suffix: None,
                tool_profile: None,
            },
        );

        let resolved = agent.resolve_overrides(Some("deep"), Some("haiku"), Some(5), None);
        assert_eq!(resolved.model.as_deref(), Some("haiku"));
        assert_eq!(resolved.max_iterations, 5);
    }

    #[test]
    fn test_intersect_tools() {
        // None base = override is the list
        assert_eq!(
            intersect_tools(&None, &["a".into(), "b".into()]),
            vec!["a", "b"]
        );
        // Both set = intersection
        assert_eq!(
            intersect_tools(
                &Some(vec!["a".into(), "b".into(), "c".into()]),
                &["b".into(), "c".into(), "d".into()]
            ),
            vec!["b", "c"]
        );
    }

    // -----------------------------------------------------------------------
    // Runtime registration + from_db_config
    // -----------------------------------------------------------------------

    #[test]
    fn registry_register_adds_and_rejects_duplicates() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz")]),
        };
        let new_agent = make_agent("researcher");
        registry.register(new_agent).unwrap();
        assert!(registry.get("researcher").is_some());

        // Duplicate rejected.
        let dup = make_agent("researcher");
        assert!(registry.register(dup).is_err());

        // Names list reflects registration.
        let names = registry.names();
        assert!(names.contains(&"chaz".to_string()));
        assert!(names.contains(&"researcher".to_string()));
    }

    #[test]
    fn agent_from_db_config_maps_all_fields() {
        let cfg = crate::agent_db::AgentDbConfig {
            model: Some("opus".to_string()),
            tools: Some(vec!["get_time".into()]),
            max_iterations: Some(42),
            tool_profile: Some("deep".to_string()),
            max_context_tokens: Some(200_000),
            ..Default::default()
        };

        let agent = Agent::from_db_config("new-agent", &cfg);
        assert_eq!(agent.name, "new-agent");
        assert_eq!(agent.default_model.as_deref(), Some("opus"));
        assert_eq!(
            agent.allowed_tools.as_deref(),
            Some(&["get_time".to_string()][..])
        );
        assert_eq!(agent.max_iterations, 42);
        assert_eq!(agent.tool_profile.as_deref(), Some("deep"));
        assert_eq!(agent.max_context_tokens, Some(200_000));
        assert!(agent.workers.is_empty());
    }

    #[test]
    fn agent_from_db_config_uses_defaults_for_empty_config() {
        let cfg = crate::agent_db::AgentDbConfig::default();
        let agent = Agent::from_db_config("fresh", &cfg);
        assert_eq!(agent.name, "fresh");
        assert_eq!(agent.max_iterations, 10);
        assert!(agent.allowed_tools.is_none());
        assert!(agent.workers.is_empty());
    }

    #[test]
    fn agent_from_db_config_populates_workers() {
        let cfg = crate::agent_db::AgentDbConfig {
            workers: vec![
                crate::agent_db::WorkerDbConfig {
                    name: "researcher".into(),
                    system_prompt: "Cite sources.".into(),
                    max_iterations: Some(20),
                    ..Default::default()
                },
                crate::agent_db::WorkerDbConfig {
                    name: "librarian".into(),
                    model: Some("gpt-4".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let agent = Agent::from_db_config("ava", &cfg);
        assert_eq!(agent.workers.len(), 2);

        let researcher = agent.find_worker("researcher").expect("researcher");
        assert_eq!(researcher.system_prompt, "Cite sources.");
        assert_eq!(researcher.max_iterations, Some(20));

        let librarian = agent.find_worker("librarian").expect("librarian");
        assert_eq!(librarian.default_model.as_deref(), Some("gpt-4"));

        assert!(agent.find_worker("not-here").is_none());
    }

    // -----------------------------------------------------------------------
    // Live hydration from AgentDb config
    // -----------------------------------------------------------------------

    #[test]
    fn upsert_replaces_existing_entry() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz")]),
        };
        let mut updated = make_agent("chaz");
        updated.default_model = Some("opus".to_string());
        registry.upsert(updated);
        let got = registry.get("chaz").unwrap();
        assert_eq!(got.default_model.as_deref(), Some("opus"));
        // Still one entry total (upsert didn't append a duplicate).
        assert_eq!(registry.names().len(), 1);
    }

    #[test]
    fn upsert_inserts_when_absent() {
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![make_agent("chaz")]),
        };
        registry.upsert(make_agent("beta"));
        assert_eq!(registry.names().len(), 2);
        assert!(registry.get("beta").is_some());
    }

    #[test]
    fn build_from_db_config_picks_up_edits() {
        // Simulates: config writes V1 → runtime builds agent with V1 →
        // config writes V2 → runtime rebuilds → sees V2.
        let registry = AgentRegistry {
            agents: std::sync::RwLock::new(vec![]),
        };

        let v1 = crate::agent_db::AgentDbConfig {
            model: Some("haiku".to_string()),
            max_iterations: Some(5),
            ..Default::default()
        };
        let built_v1 = registry.build_from_db_config("alpha", &v1);
        registry.upsert(built_v1);
        assert_eq!(
            registry.get("alpha").unwrap().default_model.as_deref(),
            Some("haiku")
        );
        assert_eq!(registry.get("alpha").unwrap().max_iterations, 5);

        // Second build after a config edit picks up the new values.
        let v2 = crate::agent_db::AgentDbConfig {
            model: Some("opus".to_string()),
            max_iterations: Some(99),
            ..Default::default()
        };
        let built_v2 = registry.build_from_db_config("alpha", &v2);
        registry.upsert(built_v2);
        assert_eq!(
            registry.get("alpha").unwrap().default_model.as_deref(),
            Some("opus")
        );
        assert_eq!(registry.get("alpha").unwrap().max_iterations, 99);
    }
}

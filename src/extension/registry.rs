// Step 3 of the cap refactor — pure addition. The hub doesn't consume
// this yet; step 5 wires it in.
#![allow(dead_code)]

//! Capability registry — the host's index of cap impls.
//!
//! Two parts:
//!
//! * [`HostCaps`] — the host's impls of host-only kinds (`SessionRead`,
//!   `SessionWrite`, `Settings`, `ToolRegistration`,
//!   `CommandRegistration`). Populated by the host at startup
//!   (refactor step 4); extensions never write here.
//! * `by_kind` — a [`ProviderMap`] per extension-providable kind
//!   (`Messenger`, `MemoryAccess`). Populated by extensions in phase 1
//!   of `install_all` via their `build_providers()` impls; the
//!   operator's `capability_defaults:` map then chooses which named
//!   provider is the default that bare requests resolve to.
//!
//! Step 3 covers the types, registration, and operator-default
//! application. Bundle construction (the consumer-side resolution that
//! turns a manifest into an `ExtensionCaps`) lives on the hub and
//! lands in step 5.

use crate::extension::caps::{
    CapProvider, CapabilityKind, CommandRegistration, SessionRead, SessionWrite, Settings,
    ToolRegistration,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Host-provided impls of host-only capability kinds.
///
/// Each slot is `None` until the host populates it at startup. Step 4
/// adds the in-process backings (`InProcSessionWrite`, etc.) that fill
/// these in.
#[derive(Default)]
pub struct HostCaps {
    pub session_read: Option<Arc<dyn SessionRead>>,
    pub session_write: Option<Arc<dyn SessionWrite>>,
    pub settings: Option<Arc<dyn Settings>>,
    pub tool_registration: Option<Arc<dyn ToolRegistration>>,
    pub command_registration: Option<Arc<dyn CommandRegistration>>,
}

impl HostCaps {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when none of the host-only slots are populated. Useful
    /// in tests; production startup expects all slots filled.
    pub fn is_empty(&self) -> bool {
        self.session_read.is_none()
            && self.session_write.is_none()
            && self.settings.is_none()
            && self.tool_registration.is_none()
            && self.command_registration.is_none()
    }
}

impl std::fmt::Debug for HostCaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same redaction pattern as `ExtensionCaps::Debug` — slot
        // booleans only; the `Arc<dyn _>` payloads aren't `Debug`.
        f.debug_struct("HostCaps")
            .field("session_read", &self.session_read.is_some())
            .field("session_write", &self.session_write.is_some())
            .field("settings", &self.settings.is_some())
            .field("tool_registration", &self.tool_registration.is_some())
            .field("command_registration", &self.command_registration.is_some())
            .finish()
    }
}

/// All providers registered for one extension-providable capability
/// kind, plus the name of whichever one is currently the default.
///
/// Keyed by **extension name**: extensions can only publish under
/// their own name, so `(kind, extension_name)` is the registry's
/// composite key. An extension publishing two impls of the same kind
/// is a manifest error (caught at validation, not here).
#[derive(Default)]
pub struct ProviderMap {
    pub providers: HashMap<String, CapProvider>,
    /// Name of the provider currently bound as the operator default for
    /// this kind. Bare consumer requests (`Messenger { provider: None }`)
    /// resolve to `providers[default.as_ref()?]`.
    pub default: Option<String>,
}

impl ProviderMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Names of every registered provider, sorted for deterministic
    /// iteration / diagnostics.
    pub fn provider_names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }
}

impl std::fmt::Debug for ProviderMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderMap")
            .field("providers", &self.provider_names())
            .field("default", &self.default)
            .finish()
    }
}

/// The cap registry — `HostCaps` plus the per-kind provider maps.
///
/// Lives on the `ExtensionHub` (step 5 wires it in). Built up across
/// `install_all`:
///
/// 1. Host populates `host` at startup.
/// 2. Phase 1 calls each extension's `build_providers()` and routes
///    results through [`Self::register_provider`].
/// 3. After phase 1, [`Self::apply_operator_defaults`] resolves the
///    operator config's per-kind picks into `ProviderMap::default`.
/// 4. Phase 2 builds each consumer's `ExtensionCaps` bundle by
///    walking its manifest's requests against this registry (added
///    in step 5).
#[derive(Default, Debug)]
pub struct CapRegistry {
    pub host: HostCaps,
    pub by_kind: HashMap<CapabilityKind, ProviderMap>,
}

/// Cap registry failures. Per-manifest errors live on
/// [`crate::extension::manifest::ManifestError`]; this enum covers the
/// cross-manifest checks the registry performs at registration time
/// and the operator-default validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error(
        "extension '{extension}': capability '{kind}' is host-only — only the host may publish \
         this kind"
    )]
    HostOnlyProvider {
        extension: String,
        kind: CapabilityKind,
    },

    #[error(
        "capability '{kind}' provider name collision: extensions '{first}' and '{second}' both \
         registered under the same name"
    )]
    ProviderCollision {
        kind: CapabilityKind,
        first: String,
        second: String,
    },

    #[error(
        "extension '{extension}' published `CapProvider::{provider_variant}` for kind '{kind}' \
         — the provider variant must match the kind"
    )]
    ProviderVariantMismatch {
        extension: String,
        kind: CapabilityKind,
        provider_variant: CapabilityKind,
    },

    #[error(
        "operator default for capability '{kind}' names provider '{name}' but no extension \
         registered a provider for that kind under that name"
    )]
    UnknownDefaultProvider { kind: CapabilityKind, name: String },

    #[error(
        "operator default for capability '{kind}' is set but '{kind}' is host-only and has no \
         provider choice"
    )]
    HostOnlyDefault { kind: CapabilityKind },
}

impl CapRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one provider impl under `(kind, extension_name)`.
    ///
    /// Rejects:
    /// * host-only kinds (extensions can't publish those)
    /// * variant mismatch (manifest declared `Memory` but
    ///   `CapProvider::Messenger` was returned)
    /// * `(kind, extension_name)` collisions across the extension set
    ///   — let the operator rename one of them
    ///
    /// Successful registration adds to `by_kind` but does **not**
    /// affect the `default` slot. Operator defaults apply via
    /// [`Self::apply_operator_defaults`] after every extension has
    /// registered.
    pub fn register_provider(
        &mut self,
        extension: impl Into<String>,
        kind: CapabilityKind,
        provider: CapProvider,
    ) -> Result<(), RegistryError> {
        let extension = extension.into();
        if kind.is_host_only() {
            return Err(RegistryError::HostOnlyProvider { extension, kind });
        }
        if provider.kind() != kind {
            return Err(RegistryError::ProviderVariantMismatch {
                extension,
                kind,
                provider_variant: provider.kind(),
            });
        }

        let map = self.by_kind.entry(kind).or_default();
        if let Some(existing) = map.providers.keys().find(|k| **k == extension) {
            // Same extension name registering twice under one kind —
            // surface as collision so the operator (or extension
            // author) sees the dup name in their config.
            return Err(RegistryError::ProviderCollision {
                kind,
                first: existing.clone(),
                second: extension,
            });
        }
        map.providers.insert(extension, provider);
        Ok(())
    }

    /// Apply operator-configured default-provider picks per cap kind,
    /// then auto-default any kind with exactly one registered provider
    /// and no explicit pick.
    ///
    /// Rules (matches design doc D27):
    ///
    /// * Each operator entry names a `(kind, provider_name)` — the
    ///   provider must already be registered or this errors.
    /// * Host-only kinds in the operator config are rejected — no
    ///   default-provider concept for host-only caps.
    /// * For any kind not covered by the operator config that has
    ///   exactly one registered provider, that provider auto-becomes
    ///   the default. Kinds with multiple providers and no operator
    ///   pick stay defaultless — bare requests resolve to `None`.
    pub fn apply_operator_defaults(
        &mut self,
        operator_defaults: &HashMap<CapabilityKind, String>,
    ) -> Result<(), RegistryError> {
        for (&kind, name) in operator_defaults {
            if kind.is_host_only() {
                return Err(RegistryError::HostOnlyDefault { kind });
            }
            let map = self.by_kind.entry(kind).or_default();
            if !map.providers.contains_key(name) {
                return Err(RegistryError::UnknownDefaultProvider {
                    kind,
                    name: name.clone(),
                });
            }
            map.default = Some(name.clone());
        }

        // Auto-default: kind with exactly one provider and no operator
        // pick → that single provider becomes the default. Cheap
        // ergonomic win for the common case (one Messenger, one Memory).
        for map in self.by_kind.values_mut() {
            if map.default.is_none() && map.providers.len() == 1 {
                map.default = map.providers.keys().next().cloned();
            }
        }

        Ok(())
    }

    /// Lookup the registered default-provider name for a kind, if any.
    /// `None` means bare consumer requests resolve to no provider.
    pub fn default_provider_for(&self, kind: CapabilityKind) -> Option<&str> {
        self.by_kind.get(&kind)?.default.as_deref()
    }

    /// All providers registered for the given kind, sorted. Empty
    /// list if no extension has registered that kind.
    pub fn providers_for(&self, kind: CapabilityKind) -> Vec<&str> {
        match self.by_kind.get(&kind) {
            Some(map) => map.provider_names(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::caps::{
        CapFuture, MemoryAccess, MemoryHit, MemoryScope, MessageBody, Messenger,
    };

    // --- Tiny no-op cap impls used as registration material ---------------

    struct StubMessenger(&'static str);
    impl Messenger for StubMessenger {
        fn send<'a>(&'a self, _target: String, _body: MessageBody) -> CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    struct StubMemory(&'static str);
    impl MemoryAccess for StubMemory {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _scope: MemoryScope,
        ) -> CapFuture<'a, Vec<MemoryHit>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn remember<'a>(
            &'a self,
            _key: &'a str,
            _value: &'a str,
            _scope: MemoryScope,
        ) -> CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    fn messenger() -> CapProvider {
        CapProvider::Messenger(Arc::new(StubMessenger("matrix")))
    }

    fn memory() -> CapProvider {
        CapProvider::Memory(Arc::new(StubMemory("local")))
    }

    // --- HostCaps ---------------------------------------------------------

    #[test]
    fn host_caps_starts_empty() {
        let h = HostCaps::new();
        assert!(h.is_empty());
        assert!(h.session_read.is_none());
        assert!(h.command_registration.is_none());
        let dbg = format!("{h:?}");
        assert!(dbg.contains("session_read: false"), "{dbg}");
    }

    // --- register_provider ------------------------------------------------

    #[test]
    fn register_one_provider_lands_in_by_kind() {
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        assert_eq!(reg.providers_for(CapabilityKind::Messenger), vec!["matrix"]);
        // No operator default set yet — bare requests would resolve None.
        assert_eq!(reg.default_provider_for(CapabilityKind::Messenger), None);
    }

    #[test]
    fn register_two_providers_under_same_kind_keeps_both() {
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        reg.register_provider("email", CapabilityKind::Messenger, messenger())
            .unwrap();
        assert_eq!(
            reg.providers_for(CapabilityKind::Messenger),
            vec!["email", "matrix"]
        );
    }

    #[test]
    fn register_collision_under_same_extension_name_errors() {
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        let err = reg
            .register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap_err();
        assert!(
            matches!(
                err,
                RegistryError::ProviderCollision {
                    kind: CapabilityKind::Messenger,
                    ..
                }
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn register_host_only_kind_is_rejected() {
        let mut reg = CapRegistry::new();
        let err = reg
            .register_provider("rogue", CapabilityKind::SessionWrite, messenger())
            .unwrap_err();
        // The provider variant is wrong too (Messenger for SessionWrite),
        // but `is_host_only` is the earlier check so that's what fires.
        assert_eq!(
            err,
            RegistryError::HostOnlyProvider {
                extension: "rogue".into(),
                kind: CapabilityKind::SessionWrite,
            }
        );
    }

    #[test]
    fn register_provider_variant_must_match_kind() {
        let mut reg = CapRegistry::new();
        // Manifest says Memory but the build_providers returned a
        // Messenger — programming error in the extension impl.
        let err = reg
            .register_provider("oops", CapabilityKind::Memory, messenger())
            .unwrap_err();
        assert_eq!(
            err,
            RegistryError::ProviderVariantMismatch {
                extension: "oops".into(),
                kind: CapabilityKind::Memory,
                provider_variant: CapabilityKind::Messenger,
            }
        );
    }

    // --- apply_operator_defaults -----------------------------------------

    #[test]
    fn operator_default_binds_named_provider() {
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        reg.register_provider("email", CapabilityKind::Messenger, messenger())
            .unwrap();

        let defaults: HashMap<CapabilityKind, String> =
            [(CapabilityKind::Messenger, "email".to_string())]
                .into_iter()
                .collect();
        reg.apply_operator_defaults(&defaults).unwrap();
        assert_eq!(
            reg.default_provider_for(CapabilityKind::Messenger),
            Some("email")
        );
    }

    #[test]
    fn unknown_default_provider_errors() {
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        let defaults: HashMap<CapabilityKind, String> =
            [(CapabilityKind::Messenger, "irc".to_string())]
                .into_iter()
                .collect();
        let err = reg.apply_operator_defaults(&defaults).unwrap_err();
        assert_eq!(
            err,
            RegistryError::UnknownDefaultProvider {
                kind: CapabilityKind::Messenger,
                name: "irc".into(),
            }
        );
    }

    #[test]
    fn host_only_kind_in_operator_defaults_is_rejected() {
        let mut reg = CapRegistry::new();
        let defaults: HashMap<CapabilityKind, String> =
            [(CapabilityKind::Settings, "anything".to_string())]
                .into_iter()
                .collect();
        let err = reg.apply_operator_defaults(&defaults).unwrap_err();
        assert_eq!(
            err,
            RegistryError::HostOnlyDefault {
                kind: CapabilityKind::Settings,
            }
        );
    }

    #[test]
    fn single_provider_auto_defaults_without_operator_pick() {
        // Common case: one Messenger registered, operator config silent.
        // Auto-default makes bare `Messenger { provider: None }` requests
        // resolve to that single provider without configuration ceremony.
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();

        reg.apply_operator_defaults(&HashMap::new()).unwrap();
        assert_eq!(
            reg.default_provider_for(CapabilityKind::Messenger),
            Some("matrix")
        );
    }

    #[test]
    fn multiple_providers_with_no_operator_pick_have_no_default() {
        // Don't silently pick one — bare requests resolving to a random
        // provider would be surprising. Operator must choose.
        let mut reg = CapRegistry::new();
        reg.register_provider("matrix", CapabilityKind::Messenger, messenger())
            .unwrap();
        reg.register_provider("email", CapabilityKind::Messenger, messenger())
            .unwrap();

        reg.apply_operator_defaults(&HashMap::new()).unwrap();
        assert_eq!(reg.default_provider_for(CapabilityKind::Messenger), None);
    }

    #[test]
    fn operator_pick_takes_precedence_over_auto_default() {
        // Even with a single provider, the operator can explicitly
        // re-affirm the default — and apply_operator_defaults must not
        // overwrite that with the auto-default code path.
        let mut reg = CapRegistry::new();
        reg.register_provider("only", CapabilityKind::Memory, memory())
            .unwrap();
        let defaults: HashMap<CapabilityKind, String> =
            [(CapabilityKind::Memory, "only".to_string())]
                .into_iter()
                .collect();
        reg.apply_operator_defaults(&defaults).unwrap();
        assert_eq!(
            reg.default_provider_for(CapabilityKind::Memory),
            Some("only")
        );
    }
}

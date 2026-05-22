// The `CapabilityRequest` accessors are part of the declaration
// contract but have no consumer beyond validation + tests yet. Allow
// until a consumer (e.g. the WASM host-import binding) reads them.
#![allow(dead_code)]

//! Extension manifests — what each extension declares about itself.
//!
//! An [`ExtensionManifest`] is the static contract an extension publishes:
//! its name and identity, the hook kinds it intends to handle, which
//! capabilities it consumes from others (split into *required* and
//! *requested*), and which capabilities it provides for others.
//!
//! [`ExtensionManifest::validate`] runs per-manifest at install time
//! (`ExtensionHub::install_all`): non-empty name, no host-only kinds in
//! `provides_capabilities`, no duplicate entries, no contradictory
//! required/requested overlap. The declarations are otherwise
//! descriptive metadata today — runtime cap resolution happens through
//! the instance model ([`crate::extension::instance::ExtensionInstance`]
//! endpoints + [`crate::extension::instance::CapResolver`]), not by
//! consuming the manifest. The declarations are the contract the WASM
//! host-import boundary will bind against.
//!
//! See `chaz/src/extension/caps.rs` for the cap kinds and trait shapes.

use crate::extension::caps::{CapabilityKind, CapabilityRequest};
use crate::extension::{ExtensionRef, HookKind};
use std::collections::HashSet;

/// What an extension claims at registration time.
///
/// The triple `(required_capabilities, requested_capabilities,
/// provides_capabilities)` is the declared contract; everything else is
/// identity and routing metadata. Today the triple is descriptive — it
/// documents intent and is validated for internal consistency, but
/// runtime cap wiring goes through the instance model. It's the shape
/// the future WASM host-import binding will read.
///
/// * `required_capabilities` — caps the extension cannot run without.
/// * `requested_capabilities` — caps the extension uses if present and
///   degrades gracefully without.
/// * `provides_capabilities` — extension-providable caps the extension
///   publishes from its instance endpoints; host-only kinds are
///   rejected by [`Self::validate`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionManifest {
    pub name: String,
    pub extension_ref: ExtensionRef,
    pub supported_hooks: Vec<HookKind>,
    pub required_capabilities: Vec<CapabilityRequest>,
    pub requested_capabilities: Vec<CapabilityRequest>,
    pub provides_capabilities: Vec<CapabilityKind>,
}

/// Per-manifest validation failure. Cross-manifest checks
/// (collision across the extension set, required-cap satisfiability)
/// surface a different error type on the hub in step 5.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("extension manifest name must be non-empty")]
    EmptyName,

    #[error(
        "extension '{extension}': capability '{kind}' is host-only and cannot appear in \
         provides_capabilities"
    )]
    HostOnlyInProvides {
        extension: String,
        kind: CapabilityKind,
    },

    #[error("extension '{extension}': provides_capabilities lists '{kind}' more than once")]
    DuplicateProvides {
        extension: String,
        kind: CapabilityKind,
    },

    #[error("extension '{extension}': supported_hooks lists '{hook:?}' more than once")]
    DuplicateHook { extension: String, hook: HookKind },

    #[error("extension '{extension}': {list} contains duplicate capability request {request:?}")]
    DuplicateRequest {
        extension: String,
        list: RequestList,
        request: CapabilityRequest,
    },

    #[error(
        "extension '{extension}': capability request {request:?} appears in both \
         required_capabilities and requested_capabilities"
    )]
    RequiredAndRequested {
        extension: String,
        request: CapabilityRequest,
    },
}

/// Which list a duplicate request lives in. Carried in
/// [`ManifestError::DuplicateRequest`] so the error message tells the
/// extension author where to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestList {
    Required,
    Requested,
}

impl std::fmt::Display for RequestList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Required => f.write_str("required_capabilities"),
            Self::Requested => f.write_str("requested_capabilities"),
        }
    }
}

impl ExtensionManifest {
    /// Per-manifest validation. Does not consult any other manifest.
    ///
    /// Cross-manifest checks (`(kind, name)` collision across providers,
    /// required-cap satisfiability against the full provider set) are
    /// the hub's responsibility — they happen during `install_all`
    /// (refactor step 5).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.is_empty() {
            return Err(ManifestError::EmptyName);
        }

        // `provides_capabilities`: no host-only kinds, no duplicates.
        let mut seen_provides = HashSet::new();
        for &kind in &self.provides_capabilities {
            if kind.is_host_only() {
                return Err(ManifestError::HostOnlyInProvides {
                    extension: self.name.clone(),
                    kind,
                });
            }
            if !seen_provides.insert(kind) {
                return Err(ManifestError::DuplicateProvides {
                    extension: self.name.clone(),
                    kind,
                });
            }
        }

        // `supported_hooks`: no duplicates. Mirrors the existing
        // `register_extension` invariant that hooks are declared once.
        let mut seen_hooks = HashSet::new();
        for &hook in &self.supported_hooks {
            if !seen_hooks.insert(hook) {
                return Err(ManifestError::DuplicateHook {
                    extension: self.name.clone(),
                    hook,
                });
            }
        }

        // `required_capabilities` + `requested_capabilities`: no
        // duplicates within either list, and no overlap across them.
        // Two different requests for the same kind (e.g.
        // `Memory { provider: Some("a") }` and
        // `Memory { provider: Some("b") }`) are legitimate.
        let mut seen_required: HashSet<&CapabilityRequest> = HashSet::new();
        for req in &self.required_capabilities {
            if !seen_required.insert(req) {
                return Err(ManifestError::DuplicateRequest {
                    extension: self.name.clone(),
                    list: RequestList::Required,
                    request: req.clone(),
                });
            }
        }

        let mut seen_requested: HashSet<&CapabilityRequest> = HashSet::new();
        for req in &self.requested_capabilities {
            if !seen_requested.insert(req) {
                return Err(ManifestError::DuplicateRequest {
                    extension: self.name.clone(),
                    list: RequestList::Requested,
                    request: req.clone(),
                });
            }
            if seen_required.contains(req) {
                return Err(ManifestError::RequiredAndRequested {
                    extension: self.name.clone(),
                    request: req.clone(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str) -> ExtensionManifest {
        ExtensionManifest {
            name: name.into(),
            extension_ref: ExtensionRef::builtin(name),
            supported_hooks: Vec::new(),
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    #[test]
    fn empty_manifest_with_name_validates() {
        manifest("heartbeat").validate().unwrap();
    }

    #[test]
    fn missing_name_is_rejected() {
        let m = manifest("");
        assert_eq!(m.validate(), Err(ManifestError::EmptyName));
    }

    #[test]
    fn host_only_in_provides_is_rejected_for_every_host_only_kind() {
        let host_only = [
            CapabilityKind::SessionRead,
            CapabilityKind::SessionWrite,
            CapabilityKind::Settings,
            CapabilityKind::ToolRegistration,
            CapabilityKind::CommandRegistration,
        ];
        for kind in host_only {
            let mut m = manifest("ext");
            m.provides_capabilities = vec![kind];
            assert_eq!(
                m.validate(),
                Err(ManifestError::HostOnlyInProvides {
                    extension: "ext".into(),
                    kind,
                }),
                "host-only kind {kind} should be rejected in provides"
            );
        }
    }

    #[test]
    fn providable_in_provides_is_accepted() {
        let mut m = manifest("messenger-matrix");
        m.provides_capabilities = vec![CapabilityKind::Messenger];
        m.validate().unwrap();
    }

    #[test]
    fn duplicate_provides_kind_is_rejected() {
        let mut m = manifest("memory");
        m.provides_capabilities = vec![CapabilityKind::Memory, CapabilityKind::Memory];
        assert_eq!(
            m.validate(),
            Err(ManifestError::DuplicateProvides {
                extension: "memory".into(),
                kind: CapabilityKind::Memory,
            })
        );
    }

    #[test]
    fn duplicate_supported_hook_is_rejected() {
        let mut m = manifest("ext");
        m.supported_hooks = vec![HookKind::ToolCall, HookKind::ToolCall];
        assert_eq!(
            m.validate(),
            Err(ManifestError::DuplicateHook {
                extension: "ext".into(),
                hook: HookKind::ToolCall,
            })
        );
    }

    #[test]
    fn duplicate_required_request_is_rejected() {
        let mut m = manifest("daily-digest");
        m.required_capabilities = vec![
            CapabilityRequest::Memory { provider: None },
            CapabilityRequest::Memory { provider: None },
        ];
        let err = m.validate().unwrap_err();
        assert!(
            matches!(
                err,
                ManifestError::DuplicateRequest {
                    list: RequestList::Required,
                    ..
                }
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn duplicate_requested_request_is_rejected() {
        let mut m = manifest("ext");
        let req = CapabilityRequest::Messenger {
            provider: Some("matrix".into()),
        };
        m.requested_capabilities = vec![req.clone(), req];
        assert!(matches!(
            m.validate(),
            Err(ManifestError::DuplicateRequest {
                list: RequestList::Requested,
                ..
            })
        ));
    }

    #[test]
    fn distinct_requests_for_same_kind_are_allowed() {
        // Different `provider` names on the same kind are legitimate —
        // an extension that needs both upstream and cache memory.
        let mut m = manifest("memory-cache");
        m.required_capabilities = vec![
            CapabilityRequest::Memory {
                provider: Some("upstream".into()),
            },
            CapabilityRequest::Memory {
                provider: Some("cache".into()),
            },
        ];
        m.validate().unwrap();
    }

    #[test]
    fn same_request_in_required_and_requested_is_rejected() {
        // Contradictory: required says "load fails without it"; requested
        // says "degrades gracefully without it". They can't both be
        // true of the same cap.
        let mut m = manifest("ext");
        let req = CapabilityRequest::SessionWrite;
        m.required_capabilities = vec![req.clone()];
        m.requested_capabilities = vec![req.clone()];
        assert_eq!(
            m.validate(),
            Err(ManifestError::RequiredAndRequested {
                extension: "ext".into(),
                request: req,
            })
        );
    }

    #[test]
    fn different_providers_in_required_and_requested_are_allowed() {
        // `Memory { provider: Some("a") }` required and
        // `Memory { provider: Some("b") }` requested are two different
        // requests for the same kind. Legal.
        let mut m = manifest("ext");
        m.required_capabilities = vec![CapabilityRequest::Memory {
            provider: Some("primary".into()),
        }];
        m.requested_capabilities = vec![CapabilityRequest::Memory {
            provider: Some("fallback".into()),
        }];
        m.validate().unwrap();
    }

    #[test]
    fn realistic_manifest_validates() {
        // Shape close to what a future `heartbeat` extension might
        // declare: hooks, required SessionWrite + Settings, requested
        // default Messenger so it can notify on routine failure.
        let m = ExtensionManifest {
            name: "heartbeat".into(),
            extension_ref: ExtensionRef::builtin("heartbeat"),
            supported_hooks: vec![HookKind::SessionStart, HookKind::Tool],
            required_capabilities: vec![
                CapabilityRequest::SessionWrite,
                CapabilityRequest::Settings,
            ],
            requested_capabilities: vec![CapabilityRequest::Messenger { provider: None }],
            provides_capabilities: Vec::new(),
        };
        m.validate().unwrap();
    }

    #[test]
    fn request_list_display_matches_field_name() {
        assert_eq!(RequestList::Required.to_string(), "required_capabilities");
        assert_eq!(RequestList::Requested.to_string(), "requested_capabilities");
    }
}

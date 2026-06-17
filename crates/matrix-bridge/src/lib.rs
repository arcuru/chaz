//! chaz-matrix-bridge — the standalone Matrix transport bridge.
//!
//! A separate eidetica peer (its own `Instance`, backend, and key) that signs
//! in to Matrix and proxies messages into the chaz session DBs it has been
//! granted access to. chaz-core deliberately no longer holds transport
//! credentials; this crate owns the Matrix-shaped pieces:
//!
//! - [`credentials::MatrixCredentials`] — the secret blob this bridge stores in
//!   its own [`BridgeDb`](chaz_core::bridge_db::BridgeDb). chaz-core treats it
//!   as opaque; its shape is entirely the bridge's business.
//! - [`config::MatrixBridgeConfig`] — the bridge's own config file (state dir,
//!   settings-DB unlock password, the logins it manages), and the idempotent
//!   seeding that resolves `${ENV}` references and writes credentials into the
//!   bridge DB.
//!
//! The agent-facing bring-up (own key, access bootstrap, registering the public
//! `LoginRef` pointer) lives in chaz-core's `bridge_identity` and is driven by
//! the bridge binary.

pub mod bridge;
pub mod config;
pub mod credentials;

pub use bridge::MatrixBridge;
pub use config::{MatrixBridgeConfig, MatrixLoginConfig};
pub use credentials::MatrixCredentials;

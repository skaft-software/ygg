#![allow(missing_docs)]

//! Custom OpenAI-compatible endpoint credentials.
//!
//! A single custom endpoint can be configured and stored at
//! `~/.ygg/credentials/custom.json`. The file uses the same 0600 private-write
//! path as the Codex credential store.
//!
//! Gateways can opt into nonstandard prompt-cache behavior with a top-level
//! `cache` object using [`ygg_ai::CacheCompatibility`] field names, for example:
//!
//! ```text
//! "cache": {
//!   "cache_control_format": "anthropic",
//!   "send_session_affinity_headers": true,
//!   "session_affinity_format": "open_ai_no_session",
//!   "supports_long_retention": false
//! }
//! ```

mod store;

pub use store::{default_path, CredentialStore, CustomCredential, CustomModel};

/// Endpoint id registered in the model catalog for the custom endpoint.
pub const ENDPOINT_ID: &str = "custom-openai";

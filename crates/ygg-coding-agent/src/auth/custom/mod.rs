#![allow(missing_docs)]

//! Custom OpenAI-compatible provider credentials.
//!
//! Multiple named providers are configured together in
//! `~/.ygg/credentials/custom.json`. Each provider gets an isolated endpoint,
//! authentication scope, model inventory, cache namespace, and display label.
//!
//! The original single-endpoint object is accepted and normalized in memory so
//! existing installations continue to work without a separate runtime mode.

mod store;

pub use store::{
    default_path, CredentialStore, CustomAuthConfig, CustomCredential, CustomModel, CustomProvider,
    CustomRegistry,
};

/// Endpoint id used by the original single custom endpoint.
pub const ENDPOINT_ID: &str = "custom-openai";

/// Convert a stable provider key into its catalog endpoint id.
pub fn endpoint_id(provider_id: &str) -> String {
    if provider_id == ENDPOINT_ID {
        ENDPOINT_ID.to_owned()
    } else {
        format!("custom-{provider_id}")
    }
}

/// Whether a catalog endpoint belongs to the custom provider registry.
pub fn is_endpoint_id(endpoint_id: &str) -> bool {
    endpoint_id == ENDPOINT_ID || endpoint_id.starts_with("custom-")
}

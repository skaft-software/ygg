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
    default_path, CredentialStore, CustomAuthConfig, CustomCredential, CustomModel, CustomPricing,
    CustomProvider, CustomRegistry,
};

/// Endpoint id used by the original single custom endpoint.
pub const ENDPOINT_ID: &str = "custom-openai";
const ENDPOINT_NAMESPACE: &str = "custom-provider-";

/// Convert a stable provider key into its catalog endpoint id.
///
/// Length-framing keeps every configured provider injective while reserving the
/// historical `custom-openai` endpoint exclusively for the legacy provider id.
pub fn endpoint_id(provider_id: &str) -> String {
    if provider_id == ENDPOINT_ID {
        ENDPOINT_ID.to_owned()
    } else {
        format!("{ENDPOINT_NAMESPACE}{}-{provider_id}", provider_id.len())
    }
}

/// Whether a catalog endpoint belongs to the custom provider registry.
pub fn is_endpoint_id(endpoint_id: &str) -> bool {
    endpoint_id == ENDPOINT_ID || endpoint_id.starts_with(ENDPOINT_NAMESPACE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_namespaces_are_injective_and_preserve_the_legacy_alias() {
        assert_eq!(endpoint_id(ENDPOINT_ID), ENDPOINT_ID);
        assert_ne!(endpoint_id("openai"), ENDPOINT_ID);
        assert_ne!(endpoint_id("a-b"), endpoint_id("a_b"));
        assert!(is_endpoint_id(&endpoint_id("openai")));
        assert!(!is_endpoint_id("custom-unframed"));
    }
}

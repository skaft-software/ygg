#![allow(missing_docs)]

use ygg_sdk::provider::{
    builtin_provider_definitions, PricingProfile, ProviderAccess, ProviderCatalogKind,
};

#[test]
fn generated_builtin_definitions_are_credential_free() {
    let definitions = builtin_provider_definitions();
    assert_eq!(definitions.len(), 31);
    // Host-owned Copilot remains deliberately absent until an embedding host
    // completes discovery; it is not a generated CLI/configuration preset.
    assert!(!definitions
        .iter()
        .any(|definition| definition.id() == "github-copilot"));

    let rendered = format!("{definitions:?}");
    assert!(!rendered.contains("https://"));
    assert!(!rendered.contains("authorization"));
    assert!(!rendered.contains("x-api-key"));
    assert!(!rendered.contains("openai-beta"));
    assert!(!rendered.contains("originator"));
    assert!(!rendered.contains("CredentialStore"));

    let codex = definitions
        .iter()
        .find(|definition| definition.id() == "codex")
        .expect("generated Codex definition");
    assert!(matches!(
        codex.authentication(),
        ProviderAccess::Subscription { login } if login == "codex"
    ));
    assert_eq!(codex.catalog(), ProviderCatalogKind::Subscription);
    assert_eq!(codex.pricing(), PricingProfile::Subscription);
}

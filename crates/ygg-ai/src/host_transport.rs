//! Narrow host-mediated stream transport seam.
//!
//! This is intentionally not a generic HTTP hook. A registered transport sees
//! canonical requests and a secret-free selected-model view only; endpoint URL,
//! headers, and resolved credentials remain inside [`crate::AiClient`].

use async_trait::async_trait;

use crate::catalog::Model;
use crate::error::{AiError, Diagnostic};
use crate::pricing::Pricing;
use crate::stream::ResponseStream;
use crate::types::{ModelId, Protocol, Request};

/// Secret-free model facts supplied to a host-mediated stream transport.
#[derive(Clone, Debug)]
pub struct HostStreamModel {
    /// Canonical selected model identifier.
    pub id: ModelId,
    /// Canonical response protocol selected by the model catalog.
    pub protocol: Protocol,
    /// Immutable configured pricing, if any.
    pub pricing: Option<Pricing>,
}

impl From<&Model> for HostStreamModel {
    fn from(model: &Model) -> Self {
        Self {
            id: model.spec.id.clone(),
            protocol: model.spec.protocol,
            pricing: model.spec.pricing.clone(),
        }
    }
}

/// Host-owned transport for a selected catalog endpoint.
///
/// The client validates and normalizes the request before invoking this trait.
/// Implementations must not retry an accepted request implicitly, and should
/// use [`crate::CanonicalStreamAssembler`] for response construction. The
/// transport receives neither endpoint configuration nor credential material.
#[async_trait]
pub trait HostStreamTransport: Send + Sync {
    /// Starts one canonical request and returns its bounded response stream.
    async fn stream(
        &self,
        model: HostStreamModel,
        request: Request,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<ResponseStream, AiError>;
}

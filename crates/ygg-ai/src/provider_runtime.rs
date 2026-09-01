//! Object-safe provider execution boundary used by the agent kernel.
//!
//! Catalog selection and provider execution are intentionally separate. A
//! caller pins a [`crate::Model`] (normally through [`crate::ProviderRegistry`])
//! and then invokes one runtime. Implementations must return canonical guarded
//! stream events; provider-specific wire values never cross this boundary.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt as _;

use crate::{
    guard_stream, AiClient, AiError, Model, Request, Response, ResponseStream,
    ResponsesCompactRequest, ResponsesCompactResponse, StreamEvent, StreamProtocolError,
    UnsupportedError,
};

/// Object-safe execution boundary for built-in and extension-owned providers.
///
/// Implementations own transport only. The caller remains responsible for
/// request/model selection, retries, cancellation ownership, persistence, and
/// whether an operation is safe to replay. Returned streams must obey Ygg's
/// canonical stream contract: one `Started`, balanced parts, and exactly one
/// terminal `Finished` on success.
#[async_trait]
pub trait ProviderRuntime: Send + Sync {
    /// Starts one canonical provider stream.
    async fn stream(&self, model: &Model, request: Request) -> Result<ResponseStream, AiError>;

    /// Drives one canonical stream to its terminal response.
    async fn complete(&self, model: &Model, request: Request) -> Result<Response, AiError> {
        let mut stream = guard_stream(self.stream(model, request).await?);
        let mut final_response = None;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Finished(response) = event? {
                final_response = Some(response);
            }
        }
        final_response.ok_or_else(|| AiError::StreamProtocol(StreamProtocolError::MissingFinish))
    }

    /// Executes provider-native Responses compaction when supported.
    async fn compact_responses(
        &self,
        _model: &Model,
        _request: ResponsesCompactRequest,
    ) -> Result<ResponsesCompactResponse, AiError> {
        Err(UnsupportedError::ResponsesOptions.into())
    }

    /// Best-effort transport prewarming. Implementations without a reusable
    /// connection may retain the no-op default.
    async fn prewarm(&self, _model: &Model, _request: Request) -> Result<(), AiError> {
        Ok(())
    }
}

/// Shared object-safe provider runtime handle.
pub type ProviderRuntimeRef = Arc<dyn ProviderRuntime>;

#[async_trait]
impl ProviderRuntime for AiClient {
    async fn stream(&self, model: &Model, request: Request) -> Result<ResponseStream, AiError> {
        AiClient::stream(self, model, request).await
    }

    async fn complete(&self, model: &Model, request: Request) -> Result<Response, AiError> {
        AiClient::complete(self, model, request).await
    }

    async fn compact_responses(
        &self,
        model: &Model,
        request: ResponsesCompactRequest,
    ) -> Result<ResponsesCompactResponse, AiError> {
        AiClient::compact_responses(self, model, request).await
    }

    async fn prewarm(&self, model: &Model, request: Request) -> Result<(), AiError> {
        AiClient::prewarm_responses(self, model, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MinimalRuntime;

    #[async_trait]
    impl ProviderRuntime for MinimalRuntime {
        async fn stream(
            &self,
            _model: &Model,
            _request: Request,
        ) -> Result<ResponseStream, AiError> {
            unreachable!("object-safety test does not execute a request")
        }
    }

    #[test]
    fn provider_runtime_is_object_safe_and_ai_client_implements_it() {
        fn accepts_runtime(_: ProviderRuntimeRef) {}
        fn implements_runtime<T: ProviderRuntime>() {}

        accepts_runtime(Arc::new(MinimalRuntime));
        implements_runtime::<AiClient>();
    }
}

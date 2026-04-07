use async_trait::async_trait;

use crate::error::Result;
use crate::llm::{BudgetStatus, LlmChunk, LlmRequest, LlmResponse, LlmStream};

/// A single LLM provider (Anthropic, OpenAI, etc.).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider identifier (e.g., "anthropic", "openai").
    fn provider_id(&self) -> &str;

    /// Send a completion request.
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse>;

    /// Send a streaming completion request.
    async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmStream>;

    /// Health check / availability.
    async fn is_available(&self) -> bool;
}

/// Routes requests to the appropriate provider based on profile,
/// handles failover chains, and enforces budget limits.
#[async_trait]
pub trait LlmRouter: Send + Sync {
    /// Send a completion request. The router selects the provider
    /// based on the requested profile and current availability.
    async fn route(&self, request: &LlmRequest) -> Result<LlmResponse>;

    /// Send a streaming completion request. The router selects the
    /// provider based on the requested profile and streams the response.
    ///
    /// The default implementation falls back to `route()` and returns
    /// the full response as a single chunk, so existing implementations
    /// continue to work without changes.
    async fn route_streaming(&self, request: &LlmRequest) -> Result<LlmStream> {
        let response = self.route(request).await?;
        let chunk = LlmChunk {
            delta: response.content,
            is_final: true,
            is_thinking: false,
            tool_calls: vec![],
        };
        Ok(Box::pin(tokio_stream::once(Ok(chunk))))
    }

    /// Current budget status.
    async fn budget_remaining(&self) -> Result<BudgetStatus>;
}

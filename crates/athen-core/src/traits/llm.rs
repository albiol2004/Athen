use async_trait::async_trait;

use crate::error::Result;
use crate::llm::{BudgetStatus, LlmRequest, LlmResponse, LlmStream};

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

    /// Current budget status.
    async fn budget_remaining(&self) -> Result<BudgetStatus>;
}

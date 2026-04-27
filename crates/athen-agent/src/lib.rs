//! Agent worker for Athen.
//!
//! Executes tasks through LLM-driven steps, calling tools as needed.
//! This crate provides the core execution loop, audit logging,
//! timeout guards, and resource monitoring.

pub mod auditor;
pub mod executor;
pub mod resource;
pub mod timeout;
pub mod tool_grouping;
pub mod tools;
pub mod tools_doc;

pub use auditor::InMemoryAuditor;
pub use executor::DefaultExecutor;
pub use resource::DefaultResourceMonitor;
pub use timeout::DefaultTimeoutGuard;
pub use tools::ShellToolRegistry;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use athen_core::error::{AthenError, Result};
use athen_core::llm::ChatMessage;
use athen_core::traits::agent::StepAuditor;
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::tool::ToolRegistry;

/// Builder for constructing a [`DefaultExecutor`] with sensible defaults.
pub struct AgentBuilder {
    llm_router: Option<Box<dyn LlmRouter>>,
    tool_registry: Option<Box<dyn ToolRegistry>>,
    auditor: Option<Box<dyn StepAuditor>>,
    max_steps: u32,
    timeout: Duration,
    context_messages: Vec<ChatMessage>,
    stream_sender: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    tool_doc_path: Option<PathBuf>,
}

impl AgentBuilder {
    /// Create a new builder with default values.
    ///
    /// Defaults:
    /// - `max_steps`: 50
    /// - `timeout`: 5 minutes
    /// - `auditor`: [`InMemoryAuditor`]
    pub fn new() -> Self {
        Self {
            llm_router: None,
            tool_registry: None,
            auditor: None,
            max_steps: 50,
            timeout: Duration::from_secs(300),
            context_messages: Vec::new(),
            stream_sender: None,
            cancel_flag: None,
            tool_doc_path: None,
        }
    }

    /// Set the LLM router.
    pub fn llm_router(mut self, router: Box<dyn LlmRouter>) -> Self {
        self.llm_router = Some(router);
        self
    }

    /// Set the tool registry.
    pub fn tool_registry(mut self, registry: Box<dyn ToolRegistry>) -> Self {
        self.tool_registry = Some(registry);
        self
    }

    /// Set a custom step auditor. Defaults to [`InMemoryAuditor`].
    pub fn auditor(mut self, auditor: Box<dyn StepAuditor>) -> Self {
        self.auditor = Some(auditor);
        self
    }

    /// Set the maximum number of steps before the executor gives up.
    pub fn max_steps(mut self, n: u32) -> Self {
        self.max_steps = n;
        self
    }

    /// Set the maximum execution time for a task.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Set context messages to prepend to the conversation.
    ///
    /// These messages represent prior conversation history and are inserted
    /// before the current task's user message, giving the agent memory of
    /// earlier exchanges within the session.
    pub fn context_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.context_messages = messages;
        self
    }

    /// Set a channel sender for streaming text chunks from the final LLM response.
    ///
    /// When provided, the executor will use `LlmRouter::route_streaming()` and
    /// forward each text delta through this sender, enabling progressive
    /// rendering in the UI.
    pub fn stream_sender(
        mut self,
        sender: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        self.stream_sender = Some(sender);
        self
    }

    /// Set a cancellation flag that the executor checks at the top of each
    /// loop iteration and between tool calls. Setting the flag to `true`
    /// causes the executor to return immediately with a "cancelled" result.
    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(flag);
        self
    }

    /// Directory containing per-group markdown references (e.g. `calendar.md`,
    /// `files.md`). When set, the system prompt instructs the agent to
    /// `read` the relevant group file for full schemas of any tool whose
    /// arguments it doesn't already know.
    pub fn tool_doc_dir(mut self, dir: PathBuf) -> Self {
        self.tool_doc_path = Some(dir);
        self
    }

    /// Build the [`DefaultExecutor`].
    ///
    /// Returns an error if `llm_router` or `tool_registry` are not set.
    pub fn build(self) -> Result<DefaultExecutor> {
        let llm_router = self
            .llm_router
            .ok_or_else(|| AthenError::Config("llm_router is required".to_string()))?;

        let tool_registry = self
            .tool_registry
            .ok_or_else(|| AthenError::Config("tool_registry is required".to_string()))?;

        let auditor: Box<dyn StepAuditor> = self
            .auditor
            .unwrap_or_else(|| Box::new(InMemoryAuditor::new()));

        let mut executor = DefaultExecutor::new(
            llm_router,
            tool_registry,
            auditor,
            self.max_steps,
            self.timeout,
            self.context_messages,
        );

        if let Some(sender) = self.stream_sender {
            executor.set_stream_sender(sender);
        }

        if let Some(flag) = self.cancel_flag {
            executor.set_cancel_flag(flag);
        }

        if let Some(path) = self.tool_doc_path {
            executor.set_tool_doc_dir(path);
        }

        Ok(executor)
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use athen_core::llm::{BudgetStatus, LlmRequest, LlmResponse};
    use athen_core::tool::{ToolDefinition, ToolResult as CoreToolResult};

    struct DummyRouter;

    #[async_trait]
    impl LlmRouter for DummyRouter {
        async fn route(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            unimplemented!()
        }
        async fn budget_remaining(&self) -> Result<BudgetStatus> {
            unimplemented!()
        }
    }

    struct DummyRegistry;

    #[async_trait]
    impl ToolRegistry for DummyRegistry {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _name: &str,
            _args: serde_json::Value,
        ) -> Result<CoreToolResult> {
            unimplemented!()
        }
    }

    #[test]
    fn test_builder_missing_router() {
        let result = AgentBuilder::new()
            .tool_registry(Box::new(DummyRegistry))
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn test_builder_missing_registry() {
        let result = AgentBuilder::new()
            .llm_router(Box::new(DummyRouter))
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn test_builder_success() {
        let result = AgentBuilder::new()
            .llm_router(Box::new(DummyRouter))
            .tool_registry(Box::new(DummyRegistry))
            .max_steps(100)
            .timeout(Duration::from_secs(600))
            .build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_builder_defaults() {
        let result = AgentBuilder::new()
            .llm_router(Box::new(DummyRouter))
            .tool_registry(Box::new(DummyRegistry))
            .build();
        assert!(result.is_ok());
    }
}

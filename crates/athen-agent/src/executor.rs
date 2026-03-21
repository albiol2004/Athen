//! LLM-driven task execution loop.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::task::{StepStatus, TaskStep};
use athen_core::traits::agent::{AgentExecutor, StepAuditor, TaskResult, TimeoutGuard};
use athen_core::traits::llm::LlmRouter;
use athen_core::traits::tool::ToolRegistry;

use crate::timeout::DefaultTimeoutGuard;

/// LLM-driven executor that runs a task through iterative LLM calls,
/// invoking tools as requested by the model until the task is complete.
pub struct DefaultExecutor {
    llm_router: Box<dyn LlmRouter>,
    tool_registry: Box<dyn ToolRegistry>,
    auditor: Box<dyn StepAuditor>,
    max_steps: u32,
    timeout: Duration,
}

impl DefaultExecutor {
    /// Create a new executor with the given components and limits.
    pub fn new(
        llm_router: Box<dyn LlmRouter>,
        tool_registry: Box<dyn ToolRegistry>,
        auditor: Box<dyn StepAuditor>,
        max_steps: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            llm_router,
            tool_registry,
            auditor,
            max_steps,
            timeout,
        }
    }

    /// Build the system prompt for the agent, including available tool descriptions.
    fn build_system_prompt(
        task_description: &str,
        tools: &[athen_core::tool::ToolDefinition],
    ) -> String {
        let mut prompt = format!(
            "You are Athen, an AI agent that can execute tasks using tools.\n\n\
             Your current task:\n{}\n\n",
            task_description
        );

        if !tools.is_empty() {
            prompt.push_str("You have the following tools available:\n");
            for tool in tools {
                prompt.push_str(&format!("- **{}**: {}\n", tool.name, tool.description));
            }
            prompt.push('\n');
        }

        prompt.push_str(
            "Use tools when needed to accomplish the task. \
             When the task is complete, respond with your final answer without any tool calls.",
        );

        prompt
    }
}

#[async_trait]
impl AgentExecutor for DefaultExecutor {
    async fn execute(&self, task: athen_core::task::Task) -> Result<TaskResult> {
        let timeout_guard = DefaultTimeoutGuard::new(self.timeout);
        let task_id = task.id;
        let mut steps_completed: u32 = 0;
        let mut conversation: Vec<ChatMessage> = Vec::new();

        // Gather available tools for the LLM
        let available_tools = self.tool_registry.list_tools().await?;

        // Seed the conversation with the task description as a user message
        conversation.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(task.description.clone()),
        });

        let system_prompt = Self::build_system_prompt(&task.description, &available_tools);

        tracing::info!(task_id = %task_id, "Starting task execution");

        loop {
            // Check timeout
            if timeout_guard.is_expired() {
                tracing::warn!(task_id = %task_id, "Task execution timed out");
                return Err(AthenError::Timeout(self.timeout));
            }

            // Check step limit
            if steps_completed >= self.max_steps {
                tracing::warn!(
                    task_id = %task_id,
                    steps = steps_completed,
                    max = self.max_steps,
                    "Task reached max steps limit"
                );
                return Ok(TaskResult {
                    task_id,
                    success: false,
                    output: Some(serde_json::json!({
                        "reason": "max_steps_exceeded",
                        "steps_completed": steps_completed,
                    })),
                    steps_completed,
                    total_risk_used: 0,
                });
            }

            // Build LLM request
            let request = LlmRequest {
                profile: ModelProfile::Fast,
                messages: conversation.clone(),
                max_tokens: Some(4096),
                temperature: Some(0.7),
                tools: if available_tools.is_empty() {
                    None
                } else {
                    Some(available_tools.clone())
                },
                system_prompt: Some(system_prompt.clone()),
            };

            // Call the LLM
            let response = self.llm_router.route(&request).await?;

            // Add assistant response to conversation.
            // When the response includes tool calls, embed them in a Structured
            // message so downstream providers can reconstruct the API format.
            if response.tool_calls.is_empty() {
                conversation.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(response.content.clone()),
                });
            } else {
                conversation.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Structured(serde_json::json!({
                        "text": response.content,
                        "tool_calls": response.tool_calls,
                    })),
                });
            }

            if response.tool_calls.is_empty() {
                // No tool calls means the LLM considers the task complete
                let step = TaskStep {
                    id: Uuid::new_v4(),
                    index: steps_completed,
                    description: "Task completed".to_string(),
                    status: StepStatus::Completed,
                    started_at: Some(Utc::now()),
                    completed_at: Some(Utc::now()),
                    output: Some(serde_json::json!({ "response": response.content })),
                    checkpoint: None,
                };

                self.auditor.record_step(task_id, &step).await?;
                steps_completed += 1;

                tracing::info!(
                    task_id = %task_id,
                    steps = steps_completed,
                    "Task completed successfully"
                );

                return Ok(TaskResult {
                    task_id,
                    success: true,
                    output: Some(serde_json::json!({ "response": response.content })),
                    steps_completed,
                    total_risk_used: 0,
                });
            }

            // Execute each tool call
            for tool_call in &response.tool_calls {
                let started_at = Utc::now();

                tracing::debug!(
                    task_id = %task_id,
                    tool = %tool_call.name,
                    "Executing tool call"
                );

                let tool_result = self
                    .tool_registry
                    .call_tool(&tool_call.name, tool_call.arguments.clone())
                    .await;

                let (step_status, output) = match &tool_result {
                    Ok(result) => (
                        if result.success {
                            StepStatus::Completed
                        } else {
                            StepStatus::Failed
                        },
                        Some(serde_json::json!({
                            "tool": tool_call.name,
                            "result": result.output,
                        })),
                    ),
                    Err(e) => (
                        StepStatus::Failed,
                        Some(serde_json::json!({
                            "tool": tool_call.name,
                            "error": e.to_string(),
                        })),
                    ),
                };

                let step = TaskStep {
                    id: Uuid::new_v4(),
                    index: steps_completed,
                    description: format!("Tool call: {}", tool_call.name),
                    status: step_status,
                    started_at: Some(started_at),
                    completed_at: Some(Utc::now()),
                    output: output.clone(),
                    checkpoint: None,
                };

                self.auditor.record_step(task_id, &step).await?;
                steps_completed += 1;

                // Add tool result to conversation for the next LLM call.
                // Include the tool_call_id so the provider can match results
                // to their originating tool calls (required by OpenAI-compatible APIs).
                let tool_response_content = match &tool_result {
                    Ok(result) => serde_json::to_string(&result.output)
                        .unwrap_or_else(|_| "{}".to_string()),
                    Err(e) => format!("Error: {}", e),
                };

                conversation.push(ChatMessage {
                    role: Role::Tool,
                    content: MessageContent::Structured(serde_json::json!({
                        "tool_call_id": tool_call.id,
                        "content": tool_response_content,
                    })),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auditor::InMemoryAuditor;
    use athen_core::llm::{BudgetStatus, FinishReason, LlmResponse, TokenUsage, ToolCall};
    use athen_core::tool::{ToolDefinition, ToolResult as CoreToolResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // --- Mock LLM Router ---

    struct MockLlmRouter {
        responses: Vec<LlmResponse>,
        call_count: AtomicUsize,
    }

    impl MockLlmRouter {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }

        fn make_response(content: &str, tool_calls: Vec<ToolCall>) -> LlmResponse {
            let finish_reason = if tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            };
            LlmResponse {
                content: content.to_string(),
                model_used: "mock-model".to_string(),
                provider: "mock".to_string(),
                usage: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    estimated_cost_usd: None,
                },
                tool_calls,
                finish_reason,
            }
        }
    }

    #[async_trait]
    impl LlmRouter for MockLlmRouter {
        async fn route(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            if idx < self.responses.len() {
                Ok(self.responses[idx].clone())
            } else {
                // Return a "done" response if we run out of canned responses
                Ok(MockLlmRouter::make_response("Done", vec![]))
            }
        }

        async fn budget_remaining(&self) -> Result<BudgetStatus> {
            Ok(BudgetStatus {
                daily_limit_usd: None,
                spent_today_usd: 0.0,
                remaining_usd: None,
                tokens_used_today: 0,
            })
        }
    }

    // --- Mock Tool Registry ---

    struct MockToolRegistry {
        tools: Vec<ToolDefinition>,
        results: std::sync::Mutex<Vec<CoreToolResult>>,
        call_index: AtomicUsize,
    }

    impl MockToolRegistry {
        fn new(tools: Vec<ToolDefinition>, results: Vec<CoreToolResult>) -> Self {
            Self {
                tools,
                results: std::sync::Mutex::new(results),
                call_index: AtomicUsize::new(0),
            }
        }

        fn empty() -> Self {
            Self::new(vec![], vec![])
        }
    }

    #[async_trait]
    impl ToolRegistry for MockToolRegistry {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: serde_json::Value,
        ) -> Result<CoreToolResult> {
            let idx = self.call_index.fetch_add(1, Ordering::SeqCst);
            let results = self.results.lock().unwrap();
            if idx < results.len() {
                Ok(results[idx].clone())
            } else {
                Ok(CoreToolResult {
                    success: true,
                    output: serde_json::json!({"result": "ok"}),
                    error: None,
                    execution_time_ms: 1,
                })
            }
        }
    }

    fn make_task(description: &str) -> athen_core::task::Task {
        use athen_core::task::*;
        Task {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            source_event: None,
            domain: DomainType::Base,
            description: description.to_string(),
            priority: TaskPriority::Normal,
            status: TaskStatus::Pending,
            risk_score: None,
            risk_budget: None,
            risk_used: 0,
            assigned_agent: None,
            steps: vec![],
            deadline: None,
        }
    }

    #[tokio::test]
    async fn test_executor_completes_simple_task() {
        let router = MockLlmRouter::new(vec![MockLlmRouter::make_response(
            "Task is done.",
            vec![],
        )]);

        let executor = DefaultExecutor::new(
            Box::new(router),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            10,
            Duration::from_secs(60),
        );

        let task = make_task("Say hello");
        let result = executor.execute(task).await.unwrap();

        assert!(result.success);
        assert_eq!(result.steps_completed, 1);
    }

    #[tokio::test]
    async fn test_executor_handles_tool_calls() {
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"query": "test"}),
        };

        let responses = vec![
            // First response: request a tool call
            MockLlmRouter::make_response("Let me search for that.", vec![tool_call]),
            // Second response: done, no more tool calls
            MockLlmRouter::make_response("Found the answer.", vec![]),
        ];

        let tool_result = CoreToolResult {
            success: true,
            output: serde_json::json!({"results": ["item1"]}),
            error: None,
            execution_time_ms: 50,
        };

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::new(vec![], vec![tool_result])),
            Box::new(InMemoryAuditor::new()),
            10,
            Duration::from_secs(60),
        );

        let task = make_task("Search for something");
        let result = executor.execute(task).await.unwrap();

        assert!(result.success);
        // 1 tool call step + 1 completion step
        assert_eq!(result.steps_completed, 2);
    }

    #[tokio::test]
    async fn test_executor_respects_max_steps() {
        // LLM always requests tool calls, never finishes
        let tool_call = ToolCall {
            id: "call_loop".to_string(),
            name: "noop".to_string(),
            arguments: serde_json::json!({}),
        };

        let responses: Vec<LlmResponse> = (0..10)
            .map(|_| MockLlmRouter::make_response("Calling tool again.", vec![tool_call.clone()]))
            .collect();

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            3, // max 3 steps
            Duration::from_secs(60),
        );

        let task = make_task("Infinite loop task");
        let result = executor.execute(task).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.steps_completed, 3);
    }

    #[tokio::test]
    async fn test_executor_timeout() {
        // Use a zero-duration timeout so it expires immediately
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "slow_tool".to_string(),
            arguments: serde_json::json!({}),
        };

        let responses = vec![MockLlmRouter::make_response(
            "Calling tool.",
            vec![tool_call],
        )];

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            100,
            Duration::ZERO, // instant timeout
        );

        let task = make_task("Should timeout");
        // Sleep briefly so the timeout guard expires
        tokio::time::sleep(Duration::from_millis(1)).await;
        let result = executor.execute(task).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            AthenError::Timeout(_) => {} // expected
            other => panic!("Expected Timeout error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_auditor_records_steps_during_execution() {
        let auditor = Arc::new(InMemoryAuditor::new());

        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "test_tool".to_string(),
            arguments: serde_json::json!({}),
        };

        let responses = vec![
            MockLlmRouter::make_response("Calling tool.", vec![tool_call]),
            MockLlmRouter::make_response("All done.", vec![]),
        ];

        // We need an auditor that we can inspect after execution.
        // Since DefaultExecutor takes Box<dyn StepAuditor>, we wrap
        // our Arc<InMemoryAuditor> in a thin delegating wrapper.
        struct ArcAuditor(Arc<InMemoryAuditor>);

        #[async_trait]
        impl StepAuditor for ArcAuditor {
            async fn record_step(
                &self,
                task_id: athen_core::task::TaskId,
                step: &TaskStep,
            ) -> Result<()> {
                self.0.record_step(task_id, step).await
            }
            async fn get_steps(
                &self,
                task_id: athen_core::task::TaskId,
            ) -> Result<Vec<TaskStep>> {
                self.0.get_steps(task_id).await
            }
        }

        let task = make_task("Audited task");
        let task_id = task.id;

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(ArcAuditor(Arc::clone(&auditor))),
            10,
            Duration::from_secs(60),
        );

        let result = executor.execute(task).await.unwrap();
        assert!(result.success);

        let steps = auditor.get_steps(task_id).await.unwrap();
        assert_eq!(steps.len(), 2); // 1 tool call + 1 completion
    }
}

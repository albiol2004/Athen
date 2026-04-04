//! LLM-driven task execution loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio_stream::StreamExt;
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
    context_messages: Vec<ChatMessage>,
    stream_sender: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel_flag: Option<Arc<AtomicBool>>,
}

impl DefaultExecutor {
    /// Create a new executor with the given components and limits.
    pub fn new(
        llm_router: Box<dyn LlmRouter>,
        tool_registry: Box<dyn ToolRegistry>,
        auditor: Box<dyn StepAuditor>,
        max_steps: u32,
        timeout: Duration,
        context_messages: Vec<ChatMessage>,
    ) -> Self {
        Self {
            llm_router,
            tool_registry,
            auditor,
            max_steps,
            timeout,
            context_messages,
            stream_sender: None,
            cancel_flag: None,
        }
    }

    /// Set a channel sender for streaming text chunks from the final LLM response.
    ///
    /// When set, the executor uses `route_streaming` for the final LLM call
    /// (the call that produces the answer, with no tool calls) and forwards
    /// each text delta through this sender.
    pub fn set_stream_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<String>) {
        self.stream_sender = Some(sender);
    }

    /// Set a cancellation flag that the executor checks at the top of each
    /// iteration and between tool calls. When the flag is set to `true`, the
    /// executor returns immediately with a "cancelled" result.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel_flag = Some(flag);
    }

    /// Build the system prompt for the agent, including available tool descriptions.
    fn build_system_prompt(
        tools: &[athen_core::tool::ToolDefinition],
        has_context: bool,
    ) -> String {
        let mut prompt = String::from(
            "You are Athen, a proactive universal AI agent. You ACT first and talk second.\n\n",
        );

        if has_context {
            prompt.push_str(
                "You are in an ongoing conversation. The message history is provided. \
                 Continue naturally from where the conversation left off.\n\n",
            );
        }

        // Categorize tools for smarter guidance.
        let has_calendar = tools.iter().any(|t| t.name.starts_with("calendar_"));
        let has_shell = tools.iter().any(|t| t.name == "shell_execute");

        if !tools.is_empty() {
            prompt.push_str("You have the following tools available:\n");
            for tool in tools {
                prompt.push_str(&format!("- **{}**: {}\n", tool.name, tool.description));
            }
            prompt.push('\n');
        }

        // Calendar-specific guidance when tools are available.
        if has_calendar {
            prompt.push_str(
                "CALENDAR CAPABILITIES:\n\
                 You manage the user's calendar. You can create, update, list, and delete events.\n\
                 - When the user asks to schedule something, use calendar_create immediately.\n\
                 - When asked about upcoming events or what's on the schedule, use calendar_list.\n\
                 - When asked to reschedule or change an event, use calendar_list first to find it, then calendar_update.\n\
                 - When a calendar reminder arrives (system context message), you already have the event details. \
                   Help the user prepare — check their schedule for conflicts, suggest what to bring or review, \
                   and offer to reschedule if needed. Do NOT search random files.\n\
                 - Use ISO 8601 UTC format for times (e.g. '2026-04-05T14:00:00Z').\n\
                 - Set appropriate reminders (e.g. [15] for 15 min before, [60, 1440] for 1h and 1 day before).\n\
                 - Choose a fitting category: meeting, birthday, deadline, reminder, personal, work, other.\n\n",
            );
        }

        // Shell guidance.
        if has_shell {
            prompt.push_str(
                "SHELL & FILES:\n\
                 Use shell_execute, read_file, write_file, list_directory for filesystem and system tasks.\n\
                 Prefer specific tools (read_file, list_directory) over shell commands when possible.\n\n",
            );
        }

        prompt.push_str(
            "RULES YOU MUST FOLLOW:\n\
             1. NEVER say \"I'll do X\" or \"Let me do X\" — just DO IT by calling tools.\n\
             2. NEVER ask the user what to do next or suggest options — take initiative.\n\
             3. When a task requires tools, call them IMMEDIATELY in your first response.\n\
             4. Only respond with text (no tool calls) when the task is COMPLETE and you are reporting results.\n\
             5. Be concise in your final answer — report what you did and what you found.\n\
             6. If the user's message is ambiguous, make a reasonable choice and act on it.\n\
             7. When a system context message describes a calendar event or email, use that context — \
                do not redundantly search the filesystem for information you already have.\n\n\
             BAD: \"I'll list the files for you.\" (announces without acting)\n\
             GOOD: [calls list_directory tool, then reports results]\n\n\
             BAD: \"Would you like me to...?\" (asks instead of doing)\n\
             GOOD: [does the thing, reports what happened]",
        );

        prompt
    }
}

impl DefaultExecutor {
    /// Attempt a streaming LLM call. Collects text deltas and forwards them
    /// through `self.stream_sender`.
    ///
    /// Returns `Ok(Some(content))` if the stream produced non-empty text
    /// (indicating a final text response with no tool calls).
    /// Returns `Ok(None)` if the collected text was empty (indicating a
    /// tool-call response whose data is not available via streaming).
    async fn try_streaming_call(&self, request: &LlmRequest) -> Result<Option<String>> {
        let mut stream = self.llm_router.route_streaming(request).await?;
        let sender = self.stream_sender.as_ref();
        let mut collected = String::new();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if !chunk.delta.is_empty() {
                        collected.push_str(&chunk.delta);
                        if let Some(tx) = sender {
                            // Best-effort: if the receiver is dropped, we still
                            // finish collecting the response text.
                            let _ = tx.send(chunk.delta);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "error in LLM stream chunk, ignoring");
                }
            }
        }

        if collected.is_empty() {
            Ok(None)
        } else {
            Ok(Some(collected))
        }
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

        // Prepend context messages (prior conversation history) before the
        // current task's user message so the agent has session memory.
        conversation.extend(self.context_messages.iter().cloned());

        // Seed the conversation with the task description as a user message
        conversation.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(task.description.clone()),
        });

        let system_prompt =
            Self::build_system_prompt(&available_tools, !self.context_messages.is_empty());

        tracing::info!(task_id = %task_id, "Starting task execution");

        loop {
            // Check cancellation flag
            if let Some(ref flag) = self.cancel_flag {
                if flag.load(Ordering::Relaxed) {
                    tracing::info!(task_id = %task_id, "Task cancelled by user");
                    return Ok(TaskResult {
                        task_id,
                        success: false,
                        output: Some(serde_json::json!({
                            "reason": "cancelled",
                            "response": "Task cancelled by user.",
                        })),
                        steps_completed,
                        total_risk_used: 0,
                    });
                }
            }

            // Check timeout
            if timeout_guard.is_expired() {
                tracing::warn!(task_id = %task_id, "Task execution timed out");
                return Err(AthenError::Timeout(self.timeout));
            }

            // Check step limit — ask the LLM for a summary before giving up.
            if steps_completed >= self.max_steps {
                tracing::warn!(
                    task_id = %task_id,
                    steps = steps_completed,
                    max = self.max_steps,
                    "Task reached max steps limit"
                );

                // Ask the LLM to summarise what it found so far.
                conversation.push(ChatMessage {
                    role: Role::User,
                    content: MessageContent::Text(
                        "You've run out of steps. Summarise what you found and accomplished so far."
                            .to_string(),
                    ),
                });
                let summary_request = LlmRequest {
                    profile: ModelProfile::Fast,
                    messages: conversation.clone(),
                    max_tokens: Some(2048),
                    temperature: Some(0.5),
                    tools: None, // no tools — just summarise
                    system_prompt: Some(system_prompt.clone()),
                };
                let summary = match self.llm_router.route(&summary_request).await {
                    Ok(resp) => resp.content,
                    Err(_) => "Task reached step limit before completion.".to_string(),
                };

                return Ok(TaskResult {
                    task_id,
                    success: false,
                    output: Some(serde_json::json!({
                        "reason": "max_steps_exceeded",
                        "steps_completed": steps_completed,
                        "response": summary,
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

            // Call the LLM — use streaming when a stream sender is available.
            // Streaming allows the final text response to be forwarded chunk
            // by chunk for progressive rendering in the UI.
            let response = if self.stream_sender.is_some() {
                // Try streaming first. If we get text content, the chunks
                // have already been forwarded via the sender. If the
                // collected text is empty (tool call responses have no
                // content in the stream), fall back to non-streaming to
                // retrieve the tool call data.
                match self.try_streaming_call(&request).await {
                    Ok(Some(content)) => {
                        // Successful streaming text response (no tool calls).
                        // Build a synthetic LlmResponse for the rest of the loop.
                        athen_core::llm::LlmResponse {
                            content,
                            model_used: String::new(),
                            provider: String::new(),
                            usage: athen_core::llm::TokenUsage {
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                total_tokens: 0,
                                estimated_cost_usd: None,
                            },
                            tool_calls: vec![],
                            finish_reason: athen_core::llm::FinishReason::Stop,
                        }
                    }
                    Ok(None) => {
                        // Empty content from stream — likely a tool call response.
                        // Fall back to non-streaming to get the full response
                        // with tool call data.
                        self.llm_router.route(&request).await?
                    }
                    Err(_) => {
                        // Streaming failed — fall back to non-streaming.
                        self.llm_router.route(&request).await?
                    }
                }
            } else {
                self.llm_router.route(&request).await?
            };

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
                // If this is the FIRST response and tools are available,
                // the LLM might be narrating instead of acting. Nudge it
                // to use tools before accepting the response as final.
                if steps_completed == 0 && !available_tools.is_empty() {
                    // Check if the response looks like an announcement
                    // rather than a final answer.
                    let lower = response.content.to_lowercase();
                    let is_lazy = lower.contains("let me")
                        || lower.contains("i'll ")
                        || lower.contains("i will ")
                        || lower.contains("i can ")
                        || lower.contains("i would ")
                        || lower.contains("would you like me")
                        || lower.contains("shall i")
                        || lower.contains("do you want me");

                    if is_lazy {
                        tracing::info!(task_id = %task_id, "Nudging LLM to use tools instead of narrating");
                        conversation.push(ChatMessage {
                            role: Role::User,
                            content: MessageContent::Text(
                                "Don't tell me what you'll do — just do it. Use your tools now."
                                    .to_string(),
                            ),
                        });
                        steps_completed += 1;
                        continue;
                    }
                }

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
                // Check cancellation between tool calls
                if let Some(ref flag) = self.cancel_flag {
                    if flag.load(Ordering::Relaxed) {
                        tracing::info!(task_id = %task_id, "Task cancelled by user between tool calls");
                        return Ok(TaskResult {
                            task_id,
                            success: false,
                            output: Some(serde_json::json!({
                                "reason": "cancelled",
                                "response": "Task cancelled by user.",
                            })),
                            steps_completed,
                            total_risk_used: 0,
                        });
                    }
                }

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
            vec![],
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
            vec![],
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
            vec![],
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
            vec![],
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
            vec![],
        );

        let result = executor.execute(task).await.unwrap();
        assert!(result.success);

        let steps = auditor.get_steps(task_id).await.unwrap();
        assert_eq!(steps.len(), 2); // 1 tool call + 1 completion
    }

    #[tokio::test]
    async fn test_executor_cancel_flag_stops_execution() {
        // LLM always requests tool calls, so it would loop forever without cancellation.
        let tool_call = ToolCall {
            id: "call_loop".to_string(),
            name: "noop".to_string(),
            arguments: serde_json::json!({}),
        };

        let responses: Vec<LlmResponse> = (0..10)
            .map(|_| MockLlmRouter::make_response("Calling tool again.", vec![tool_call.clone()]))
            .collect();

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(true)); // pre-cancelled

        let mut executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            100,
            Duration::from_secs(60),
            vec![],
        );
        executor.set_cancel_flag(Arc::clone(&cancel_flag));

        let task = make_task("Should be cancelled");
        let result = executor.execute(task).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.steps_completed, 0);
        let reason = result
            .output
            .as_ref()
            .and_then(|o| o.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap();
        assert_eq!(reason, "cancelled");
    }

    #[tokio::test]
    async fn test_executor_cancel_flag_between_tool_calls() {
        // First LLM call requests 2 tool calls. Cancel flag is set after construction
        // but before execution starts. The executor should stop before executing any tools.
        let tool_call_1 = ToolCall {
            id: "call_1".to_string(),
            name: "tool_a".to_string(),
            arguments: serde_json::json!({}),
        };
        let tool_call_2 = ToolCall {
            id: "call_2".to_string(),
            name: "tool_b".to_string(),
            arguments: serde_json::json!({}),
        };

        let responses = vec![MockLlmRouter::make_response(
            "Calling two tools.",
            vec![tool_call_1, tool_call_2],
        )];

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            100,
            Duration::from_secs(60),
            vec![],
        );
        executor.set_cancel_flag(Arc::clone(&cancel_flag));

        // Set the flag right before execution -- this simulates cancellation
        // happening between the LLM call and tool execution.
        cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);

        let task = make_task("Should cancel between tools");
        let result = executor.execute(task).await.unwrap();

        assert!(!result.success);
        let reason = result
            .output
            .as_ref()
            .and_then(|o| o.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap();
        assert_eq!(reason, "cancelled");
    }
}

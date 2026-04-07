//! Integration tests for the agent executor with a real ShellToolRegistry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use athen_core::error::Result;
use athen_core::llm::*;
use athen_core::task::*;
use athen_core::traits::agent::AgentExecutor;
use athen_core::traits::llm::LlmRouter;

use athen_agent::{AgentBuilder, ShellToolRegistry};

// ---------------------------------------------------------------------------
// Mock LLM Router
// ---------------------------------------------------------------------------

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
            reasoning_content: None,
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
            Ok(MockLlmRouter::make_response("Done (fallback)", vec![]))
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

fn make_task(description: &str) -> Task {
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

/// The agent should execute a shell_execute tool call, get the real output,
/// and then complete on the second LLM call.
#[tokio::test]
async fn test_agent_executes_shell_tool_and_completes() {
    let tool_call = ToolCall {
        id: "call_1".to_string(),
        name: "shell_execute".to_string(),
        arguments: json!({"command": "echo hello_from_agent"}),
    };

    let responses = vec![
        // First call: LLM requests to run a shell command.
        MockLlmRouter::make_response("Let me run that command.", vec![tool_call]),
        // Second call: LLM sees the tool result and produces the final answer.
        MockLlmRouter::make_response("The command output was: hello_from_agent", vec![]),
    ];

    let registry = ShellToolRegistry::new().await;

    let executor = AgentBuilder::new()
        .llm_router(Box::new(MockLlmRouter::new(responses)))
        .tool_registry(Box::new(registry))
        .max_steps(10)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let task = make_task("Run echo hello_from_agent");
    let result = executor.execute(task).await.unwrap();

    assert!(result.success);
    // 1 tool call step + 1 completion step = 2
    assert_eq!(result.steps_completed, 2);

    // The final response should contain the agent's text.
    let output = result.output.unwrap();
    let response = output["response"].as_str().unwrap();
    assert!(response.contains("hello_from_agent"));
}

/// The agent should handle read_file tool calls with real filesystem access.
#[tokio::test]
async fn test_agent_reads_file_via_tool() {
    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("test_data.txt");
    std::fs::write(&file_path, "secret_content_42").unwrap();

    let tool_call = ToolCall {
        id: "call_read".to_string(),
        name: "read_file".to_string(),
        arguments: json!({"path": file_path.to_str().unwrap()}),
    };

    let responses = vec![
        MockLlmRouter::make_response("Reading the file.", vec![tool_call]),
        MockLlmRouter::make_response("File contains: secret_content_42", vec![]),
    ];

    let registry = ShellToolRegistry::new().await;

    let executor = AgentBuilder::new()
        .llm_router(Box::new(MockLlmRouter::new(responses)))
        .tool_registry(Box::new(registry))
        .max_steps(10)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let task = make_task("Read the test file");
    let result = executor.execute(task).await.unwrap();

    assert!(result.success);
    assert_eq!(result.steps_completed, 2);
}

/// Multiple tool calls in sequence: list directory, then read a file.
#[tokio::test]
async fn test_agent_multi_step_tool_usage() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("readme.md"), "# Hello").unwrap();

    let list_call = ToolCall {
        id: "call_list".to_string(),
        name: "list_directory".to_string(),
        arguments: json!({"path": dir.path().to_str().unwrap()}),
    };

    let read_call = ToolCall {
        id: "call_read".to_string(),
        name: "read_file".to_string(),
        arguments: json!({"path": dir.path().join("readme.md").to_str().unwrap()}),
    };

    let responses = vec![
        MockLlmRouter::make_response("Let me list the directory.", vec![list_call]),
        MockLlmRouter::make_response("Found readme.md, let me read it.", vec![read_call]),
        MockLlmRouter::make_response("The readme says: # Hello", vec![]),
    ];

    let registry = ShellToolRegistry::new().await;

    let executor = AgentBuilder::new()
        .llm_router(Box::new(MockLlmRouter::new(responses)))
        .tool_registry(Box::new(registry))
        .max_steps(10)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let task = make_task("Read the readme in the test dir");
    let result = executor.execute(task).await.unwrap();

    assert!(result.success);
    // 2 tool call steps + 1 completion step = 3
    assert_eq!(result.steps_completed, 3);
}

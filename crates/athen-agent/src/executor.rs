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

/// Check if a text response is an empty JSON blob (e.g. `{"response": ""}`).
///
/// Extract `<think>...</think>` blocks from model output.
///
/// Some servers (llama.cpp, Ollama) embed chain-of-thought in the content
/// field wrapped in `<think>` tags instead of using a separate
/// `reasoning_content` field. This function splits the text into
/// (content_without_think, thinking_text).
fn extract_think_tags(text: &str) -> (String, String) {
    let mut thinking = String::new();
    let mut content = text.to_string();

    // Extract all <think>...</think> blocks (greedy within each block).
    while let Some(start) = content.find("<think>") {
        if let Some(end) = content.find("</think>") {
            let think_end = end + "</think>".len();
            let think_content = &content[start + "<think>".len()..end];
            if !thinking.is_empty() {
                thinking.push('\n');
            }
            thinking.push_str(think_content.trim());
            content = format!("{}{}", &content[..start], &content[think_end..]);
        } else {
            // Unclosed <think> tag — treat the rest as thinking.
            let think_content = &content[start + "<think>".len()..];
            if !thinking.is_empty() {
                thinking.push('\n');
            }
            thinking.push_str(think_content.trim());
            content = content[..start].to_string();
            break;
        }
    }

    (content.trim().to_string(), thinking)
}

/// Clean up model responses that are wrapped in JSON or empty.
///
/// Small/local models sometimes output JSON like `{"response": "text"}` or
/// `{"response": ""}` instead of plain natural language. This function:
/// 1. Tries to parse as JSON object — extracts the first non-empty string value
/// 2. If all values are empty or the string is empty, returns a default message
/// 3. If not JSON, returns the original text as-is
fn clean_model_response(text: &str) -> String {
    let trimmed = text.trim();

    // Not JSON → return as-is.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Still handle completely empty text.
        if trimmed.is_empty() {
            return "I don't have enough information to answer that.".to_string();
        }
        return trimmed.to_string();
    };

    match value {
        serde_json::Value::Object(map) => {
            // Try to extract a meaningful text value from the JSON.
            for v in map.values() {
                if let serde_json::Value::String(s) = v {
                    if !s.trim().is_empty() {
                        return s.clone();
                    }
                }
            }
            // All values empty or no string values → model had nothing to say.
            "I don't have enough information to answer that.".to_string()
        }
        serde_json::Value::String(s) if s.trim().is_empty() => {
            "I don't have enough information to answer that.".to_string()
        }
        serde_json::Value::String(s) => s,
        // Other JSON types (array, number, etc.) — just stringify.
        other => other.to_string(),
    }
}

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
        let now = chrono::Local::now();
        let tz_offset = now.format("%:z"); // e.g. "+02:00"
        let mut prompt = format!(
            "You are Athen, a proactive universal AI agent. You ACT first and talk second.\n\
             Current date and time: {} ({}, UTC{})\n\n",
            now.format("%A, %B %-d, %Y at %H:%M"),
            now.format("%Z"),
            tz_offset,
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
        let has_contacts = tools.iter().any(|t| t.name.starts_with("contacts_"));
        let has_memory = tools.iter().any(|t| t.name == "memory_store");

        if !tools.is_empty() {
            prompt.push_str("You have the following tools available:\n");
            for tool in tools {
                prompt.push_str(&format!("- **{}**: {}\n", tool.name, tool.description));
            }
            prompt.push('\n');
        }

        // Calendar-specific guidance when tools are available.
        if has_calendar {
            prompt.push_str(&format!(
                "CALENDAR CAPABILITIES:\n\
                 You manage the user's calendar. You can create, update, list, and delete events.\n\
                 - When the user asks to schedule something, use calendar_create immediately.\n\
                 - When asked about upcoming events or what's on the schedule, use calendar_list.\n\
                 - When asked to reschedule or change an event, use calendar_list first to find it, then calendar_update.\n\
                 - When a calendar reminder arrives (system context message), you already have the event details. \
                   Help the user prepare — check their schedule for conflicts, suggest what to bring or review, \
                   and offer to reschedule if needed. Do NOT search random files.\n\
                 - IMPORTANT: The user's local timezone is UTC{tz_offset}. When the user says a time like '12:15', \
                   they mean LOCAL time. Use ISO 8601 with their offset: e.g. '2026-04-06T12:15:00{tz_offset}'. \
                   NEVER use 'Z' (UTC) unless the user explicitly says UTC.\n\
                 - Set appropriate reminders (e.g. [15] for 15 min before, [60, 1440] for 1h and 1 day before).\n\
                 - Choose a fitting category: meeting, birthday, deadline, reminder, personal, work, other.\n\n",
            ));
        }

        // Shell guidance.
        if has_shell {
            prompt.push_str(
                "SHELL & FILES:\n\
                 Use shell_execute, read_file, write_file, list_directory for filesystem and system tasks.\n\
                 Prefer specific tools (read_file, list_directory) over shell commands when possible.\n\n",
            );
        }

        // Contacts guidance.
        if has_contacts {
            prompt.push_str(
                "CONTACTS CAPABILITIES:\n\
                 You manage the user's contacts. You can create, search, update, and delete contacts.\n\
                 - Each contact has a name and multiple identifiers (email, phone, Telegram, WhatsApp, etc.)\n\
                 - When you learn about a person (from emails, messages, conversations), create or update their contact.\n\
                 - Use contacts_search to find contacts by name or identifier before creating duplicates.\n\
                 - Trust levels: Unknown (new), Neutral, Known (interacted), Trusted (explicitly marked).\n\n\
                 CONTACT MATCHING (important):\n\
                 When you receive a message from an external sender (email, Telegram, etc.), check if they \
                 match an existing contact:\n\
                 1. Use contacts_search with the sender's name or identifier.\n\
                 2. If you find a plausible match (same name, or related identifiers), ASK the user: \
                    \"I received a message from [sender]. Is this the same person as your contact [name] \
                    ([existing identifiers])? If so, I'll add their [new identifier type] to the contact.\"\n\
                 3. If the user confirms, use contacts_update to add the new identifier.\n\
                 4. If no match or user denies, use contacts_create with the new sender's info.\n\
                 5. NEVER auto-merge contacts without asking — always confirm with the user first.\n\n",
            );
        }

        // Memory guidance.
        if has_memory {
            prompt.push_str(
                "MEMORY & KNOWLEDGE:\n\
                 You have persistent memory that survives across conversations.\n\
                 - When the user asks you to remember something, use memory_store IMMEDIATELY.\n\
                 - When the user mentions a person (\"mi novia\", \"my boss\", a name), check memory_recall \
                   AND contacts_list to find what you know about them BEFORE responding.\n\
                 - When writing something for/about a person, ALWAYS look up their name and details first.\n\
                 - Relevant memories from past conversations may be provided as system context — use them.\n\n",
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
                do not redundantly search the filesystem for information you already have.\n\
             8. ALWAYS respond in natural language. NEVER output raw JSON. \
                If you don't know the answer, say so naturally in the user's language.\n\n\
             BAD: \"I'll list the files for you.\" / \"Voy a listar los archivos.\" (announces without acting)\n\
             GOOD: [calls list_directory tool, then reports results]\n\n\
             BAD: \"Would you like me to...?\" / \"¿Quieres que...?\" (asks instead of doing)\n\
             GOOD: [does the thing, reports what happened]\n\n\
             BAD: {\"response\": \"\"} (raw JSON output)\n\
             GOOD: \"I don't have that information.\" / \"No tengo esa información.\"",
        );

        prompt
    }
}

/// Result from a streaming LLM call, containing collected text, thinking
/// content, and any tool calls extracted from SSE chunks.
struct StreamResult {
    content: String,
    #[allow(dead_code)]
    thinking: String,
    tool_calls: Vec<athen_core::llm::ToolCall>,
}

impl DefaultExecutor {
    /// Ask a cheap LLM whether the agent actually completed the user's task.
    ///
    /// Returns `true` if the agent should CONTINUE (task is NOT done),
    /// `false` if the task is genuinely complete.
    async fn judge_completion(
        &self,
        user_request: &str,
        agent_response: &str,
        tools_called: &[String],
    ) -> bool {
        let tools_str = if tools_called.is_empty() {
            "NONE".to_string()
        } else {
            tools_called.join(", ")
        };

        let prompt = format!(
            "You are a strict task completion judge. Determine if the AI agent ACTUALLY \
             completed the user's request by using tools, or if it just talked about it.\n\n\
             User's request: \"{user_request}\"\n\
             Agent's response: \"{agent_response}\"\n\
             Tools actually called: [{tools_str}]\n\n\
             Rules:\n\
             - If the user asked for an ACTION (create, update, delete, modify, move, write, \
               execute) and the agent did NOT call the appropriate write tool \
               (calendar_update, calendar_create, calendar_delete, write_file, shell_execute), \
               answer CONTINUE.\n\
             - If the agent only used read tools (calendar_list, read_file, list_directory, \
               memory_recall) but the user wanted a write action, answer CONTINUE.\n\
             - If the agent just narrated what it would do without calling tools, answer CONTINUE.\n\
             - If the user asked a question or for information and the agent answered it \
               (with or without tools), answer DONE.\n\
             - If the agent genuinely completed the action using the right tools, answer DONE.\n\
             - If the user is just chatting (greeting, joke, conversation), answer DONE.\n\n\
             Reply with ONLY one word: DONE or CONTINUE"
        );

        let request = LlmRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text(prompt),
            }],
            profile: ModelProfile::Cheap,
            max_tokens: Some(5),
            temperature: Some(0.0),
            tools: None,
            system_prompt: None,
        };

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.llm_router.route(&request),
        ).await {
            Ok(Ok(resp)) => {
                let answer = resp.content.trim().to_uppercase();
                tracing::debug!("Completion judge verdict: {}", answer);
                answer.contains("CONTINUE")
            }
            Ok(Err(e)) => {
                tracing::warn!("Completion judge LLM error: {e}, defaulting to DONE");
                false // Don't block on judge failure
            }
            Err(_) => {
                tracing::warn!("Completion judge timed out, defaulting to DONE");
                false
            }
        }
    }

    /// Attempt a streaming LLM call. Collects text deltas and forwards them
    /// through `self.stream_sender`. Also collects tool calls from SSE chunks.
    ///
    /// Returns a `StreamResult` with the collected content, thinking text, and
    /// tool calls. The caller decides how to proceed based on whether content
    /// and/or tool calls are present.
    async fn try_streaming_call(&self, request: &LlmRequest) -> Result<StreamResult> {
        let mut stream = self.llm_router.route_streaming(request).await?;
        let sender = self.stream_sender.as_ref();
        let mut collected = String::new();
        let mut thinking = String::new();
        let mut tool_calls_collected: Vec<athen_core::llm::ToolCall> = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if !chunk.delta.is_empty() {
                        if chunk.is_thinking {
                            // Prefix with STX to mark as thinking content for the
                            // stream forwarder.
                            if let Some(tx) = sender {
                                let _ = tx.send(format!("\x02{}", chunk.delta));
                            }
                            thinking.push_str(&chunk.delta);
                        } else {
                            collected.push_str(&chunk.delta);
                            if let Some(tx) = sender {
                                // Best-effort: if the receiver is dropped, we still
                                // finish collecting the response text.
                                let _ = tx.send(chunk.delta);
                            }
                        }
                    }
                    if !chunk.tool_calls.is_empty() {
                        tool_calls_collected.extend(chunk.tool_calls);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "error in LLM stream chunk, ignoring");
                }
            }
        }

        // Some servers embed thinking in content with <think>...</think> tags
        // instead of using the separate reasoning_content field. Extract it.
        let (final_content, inline_thinking) = extract_think_tags(&collected);
        if !inline_thinking.is_empty() {
            // Re-send the thinking through the stream forwarder so the UI shows it.
            if let Some(tx) = sender {
                let _ = tx.send(format!("\x02{}", inline_thinking));
            }
            thinking.push_str(&inline_thinking);
        }

        if !thinking.is_empty() {
            tracing::debug!(
                thinking_len = thinking.len(),
                "collected reasoning/thinking content from stream"
            );
        }

        Ok(StreamResult {
            content: final_content,
            thinking,
            tool_calls: tool_calls_collected,
        })
    }
}

#[async_trait]
impl AgentExecutor for DefaultExecutor {
    async fn execute(&self, task: athen_core::task::Task) -> Result<TaskResult> {
        let timeout_guard = DefaultTimeoutGuard::new(self.timeout);
        let task_id = task.id;
        let mut steps_completed: u32 = 0;
        let mut has_been_judged = false;

        let mut tools_called: Vec<String> = Vec::new();
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
                    Ok(result) if !result.content.is_empty() || !result.tool_calls.is_empty() => {
                        // Got content and/or tool calls from streaming.
                        // Build a synthetic LlmResponse for the rest of the loop.
                        let finish_reason = if result.tool_calls.is_empty() {
                            athen_core::llm::FinishReason::Stop
                        } else {
                            athen_core::llm::FinishReason::ToolUse
                        };
                        athen_core::llm::LlmResponse {
                            content: result.content,
                            reasoning_content: if result.thinking.is_empty() {
                                None
                            } else {
                                Some(result.thinking)
                            },
                            model_used: String::new(),
                            provider: String::new(),
                            usage: athen_core::llm::TokenUsage {
                                prompt_tokens: 0,
                                completion_tokens: 0,
                                total_tokens: 0,
                                estimated_cost_usd: None,
                            },
                            tool_calls: result.tool_calls,
                            finish_reason,
                        }
                    }
                    Ok(_) => {
                        // No content AND no tool calls from stream — fall back to
                        // non-streaming to get the full response.
                        match self.llm_router.route(&request).await {
                            Ok(resp) => resp,
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task_id,
                                    error = %e,
                                    "non-streaming fallback failed after successful stream, using empty response"
                                );
                                athen_core::llm::LlmResponse {
                                    content: String::new(),
                                    reasoning_content: None,
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
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %e,
                            "streaming call failed, falling back to non-streaming"
                        );
                        // Streaming failed entirely — fall back to non-streaming.
                        self.llm_router.route(&request).await?
                    }
                }
            } else {
                self.llm_router.route(&request).await?
            };

            // Extract <think> tags from content (servers that embed thinking inline).
            let (stripped_content, inline_think) = extract_think_tags(&response.content);
            if !inline_think.is_empty() {
                tracing::debug!(thinking_len = inline_think.len(), "extracted inline <think> tags");
                // Forward thinking to UI via stream sender.
                if let Some(ref tx) = self.stream_sender {
                    let _ = tx.send(format!("\x02{}", inline_think));
                }
            }
            let response_content_clean = stripped_content;

            // Add assistant response to conversation.
            // When the response includes tool calls, embed them in a Structured
            // message so downstream providers can reconstruct the API format.
            if response.tool_calls.is_empty() {
                conversation.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Text(response_content_clean.clone()),
                });
            } else {
                conversation.push(ChatMessage {
                    role: Role::Assistant,
                    content: MessageContent::Structured(serde_json::json!({
                        "text": response_content_clean,
                        "tool_calls": response.tool_calls,
                    })),
                });
            }

            if response.tool_calls.is_empty() {
                // Clean up the response content: small models sometimes wrap
                // their answer in JSON like {"response": "text"} or return
                // empty JSON/empty strings. Fix it before proceeding.
                let cleaned_content = clean_model_response(&response_content_clean);

                // Update the conversation with the cleaned content.
                if cleaned_content != response_content_clean {
                    tracing::info!(
                        task_id = %task_id,
                        original = %response.content,
                        cleaned = %cleaned_content,
                        "cleaned up model response"
                    );
                    conversation.pop();
                    conversation.push(ChatMessage {
                        role: Role::Assistant,
                        content: MessageContent::Text(cleaned_content.clone()),
                    });
                }

                // Use the cleaned content from here on.
                let response_content = cleaned_content;

                // Completion judge: before accepting a text-only response as
                // "done", ask a cheap LLM whether the task was actually
                // completed.  This catches narration, false claims, and
                // incomplete tool use — in any language.
                if !available_tools.is_empty() && !has_been_judged {
                    let should_continue = self.judge_completion(
                        &task.description,
                        &response_content,
                        &tools_called,
                    ).await;

                    if should_continue {
                        tracing::info!(
                            task_id = %task_id,
                            "Completion judge: task NOT done, nudging agent"
                        );
                        has_been_judged = true;
                        conversation.push(ChatMessage {
                            role: Role::User,
                            content: MessageContent::Text(
                                "You have NOT completed the task. You MUST call the appropriate \
                                 tools to actually perform the action. Do it NOW — no narration, \
                                 no announcements, just tool calls."
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
                    output: Some(serde_json::json!({ "response": response_content })),
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
                    output: Some(serde_json::json!({ "response": response_content })),
                    steps_completed,
                    total_risk_used: 0,
                });
            }

            // Execute each tool call
            for tool_call in &response.tool_calls {
                tools_called.push(tool_call.name.clone());
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

    #[test]
    fn test_clean_model_response() {
        // JSON with empty response → default message
        assert_eq!(
            clean_model_response(r#"{"response": ""}"#),
            "I don't have enough information to answer that."
        );
        assert_eq!(
            clean_model_response(r#"{}"#),
            "I don't have enough information to answer that."
        );
        assert_eq!(
            clean_model_response(r#"  {"response": ""}  "#),
            "I don't have enough information to answer that."
        );

        // JSON with actual text → extract it
        assert_eq!(
            clean_model_response(r#"{"response": "hello world"}"#),
            "hello world"
        );
        assert_eq!(
            clean_model_response(r#"{"a": "", "b": "real answer"}"#),
            "real answer"
        );

        // Plain text → pass through
        assert_eq!(clean_model_response("just text"), "just text");
        assert_eq!(clean_model_response("  spaced  "), "spaced");

        // Empty string → default message
        assert_eq!(
            clean_model_response(""),
            "I don't have enough information to answer that."
        );
        assert_eq!(
            clean_model_response("   "),
            "I don't have enough information to answer that."
        );

        // JSON string value
        assert_eq!(clean_model_response(r#""hello""#), "hello");

        // JSON array/number → stringify
        assert_eq!(clean_model_response("[1,2,3]"), "[1,2,3]");
    }

    #[test]
    fn test_extract_think_tags() {
        // Basic think block
        let (content, thinking) = extract_think_tags("<think>I need to consider this</think>Hello!");
        assert_eq!(content, "Hello!");
        assert_eq!(thinking, "I need to consider this");

        // No think tags → pass through
        let (content, thinking) = extract_think_tags("Just normal text");
        assert_eq!(content, "Just normal text");
        assert!(thinking.is_empty());

        // Think with JSON response after
        let (content, thinking) = extract_think_tags(
            "<think>The user asks about X</think>{\"response\": \"\"}"
        );
        assert_eq!(content, "{\"response\": \"\"}");
        assert_eq!(thinking, "The user asks about X");

        // Only thinking, no content
        let (content, thinking) = extract_think_tags("<think>Just thinking here</think>");
        assert!(content.is_empty());
        assert_eq!(thinking, "Just thinking here");

        // Unclosed think tag
        let (content, thinking) = extract_think_tags("<think>Still thinking...");
        assert!(content.is_empty());
        assert_eq!(thinking, "Still thinking...");

        // Multiple think blocks
        let (content, thinking) = extract_think_tags(
            "<think>First thought</think>Middle<think>Second thought</think>End"
        );
        assert_eq!(content, "MiddleEnd");
        assert_eq!(thinking, "First thought\nSecond thought");
    }

    #[tokio::test]
    async fn test_executor_cleans_json_response() {
        // Model returns JSON blob — executor should extract the text.
        let responses = vec![
            MockLlmRouter::make_response(r#"{"response": ""}"#, vec![]),
        ];

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::empty()),
            Box::new(InMemoryAuditor::new()),
            10,
            Duration::from_secs(60),
            vec![],
        );

        let task = make_task("Tell me something");
        let result = executor.execute(task).await.unwrap();

        assert!(result.success);
        let response = result
            .output
            .as_ref()
            .and_then(|o| o.get("response"))
            .and_then(|r| r.as_str())
            .unwrap();
        assert_eq!(response, "I don't have enough information to answer that.");
    }
}

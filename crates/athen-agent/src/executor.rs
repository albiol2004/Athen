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
use crate::tool_grouping::{is_always_revealed, summarize_groups};
use std::collections::HashSet;
use std::path::PathBuf;

/// Clamp a shell tool's `timeout_ms` argument to the executor's remaining
/// budget (minus a 500ms buffer so the executor's own timeout always fires
/// first with a clean message rather than racing the shell timeout).
///
/// Mutates `args` in place to inject the clamped value. Returns
/// `Some(ToolResult)` to short-circuit the dispatch when there's effectively
/// no budget left (< 1 second after the buffer); the caller should return
/// that result instead of dispatching the tool.
fn clamp_shell_timeout(
    args: &mut serde_json::Value,
    executor_remaining_ms: u64,
) -> Option<athen_core::tool::ToolResult> {
    const BUFFER_MS: u64 = 500;
    const FLOOR_MS: u64 = 1000;

    let requested_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(60_000);

    let budget_ms = executor_remaining_ms.saturating_sub(BUFFER_MS);
    let clamped = requested_ms.min(budget_ms);

    if clamped < FLOOR_MS {
        return Some(athen_core::tool::ToolResult {
            success: false,
            output: serde_json::json!({
                "error": "executor budget exhausted, cannot start command",
                "executor_remaining_ms": executor_remaining_ms,
                "requested_timeout_ms": requested_ms,
            }),
            error: Some("executor budget exhausted, cannot start command".to_string()),
            execution_time_ms: 0,
        });
    }

    if let Some(obj) = args.as_object_mut() {
        obj.insert("timeout_ms".to_string(), serde_json::Value::from(clamped));
    }
    None
}

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
    /// Directory of per-group markdown references (typically
    /// `~/.athen/tools/`). When set, the system prompt instructs the agent
    /// to read the relevant `<group>.md` file for full schemas.
    tool_doc_dir: Option<PathBuf>,
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
            tool_doc_dir: None,
        }
    }

    /// Tell the executor where the per-group markdown reference files live.
    /// The agent will be told to `read_file` `<dir>/<group>.md` for any tool
    /// whose schema it doesn't already remember.
    pub fn set_tool_doc_dir(&mut self, dir: PathBuf) {
        self.tool_doc_dir = Some(dir);
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

    /// Build the system prompt for the agent.
    ///
    /// `tools` is the *complete* set of tools the agent can ever access this
    /// session. `revealed` is the subset whose full descriptions and schemas
    /// are surfaced inline — memory tools plus any tool the agent has already
    /// dispatched at least once this session. `tool_doc_dir` (when set)
    /// points at a directory of per-group markdown files the agent can read
    /// for full schemas of one group at a time.
    fn build_system_prompt(
        tools: &[athen_core::tool::ToolDefinition],
        revealed: &HashSet<String>,
        has_context: bool,
        tool_doc_dir: Option<&std::path::Path>,
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

        // Workspace + permission model. We deliberately do NOT leak the
        // host process's cwd here — when we did, the agent reflexively
        // wrote test files into whatever directory the user happened to
        // launch the app from (typically a real project folder), instead
        // of using its own workspace.
        let workspace = athen_core::paths::athen_workspace_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unavailable>".to_string());
        prompt.push_str(&format!(
            "Your workspace directory: {workspace}\n\
             This is YOUR folder. Anything you create — test files, scratch scripts, \
             HTML servers, etc. — goes here unless the user explicitly names a different \
             location. Do NOT invent paths under the user's home or assume the existence \
             of a 'project' directory: if the user wants a file somewhere else, they will \
             tell you the exact path. Relative paths in file tools and shell commands \
             already resolve against the workspace, so prefer them.\n\
             For paths the user explicitly hands you (absolute paths outside the \
             workspace), the first touch may prompt for approval; once granted, \
             subsequent operations on the same directory are silent.\n\n",
        ));

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
        let has_web = tools
            .iter()
            .any(|t| t.name == "web_search" || t.name == "web_fetch");

        // ── Tier 1: capability index (always shown, one line per group) ──
        if !tools.is_empty() {
            prompt.push_str(
                "AVAILABLE TOOL GROUPS — every tool listed below exists and is callable. \
                 If you already know a tool's arguments, call it directly. If you're \
                 unsure of the arguments for a tool that isn't in DETAILED TOOLS, \
                 you have two options:\n\
                 - Just try calling it; the response will tell you if anything is wrong.\n",
            );
            if let Some(dir) = tool_doc_dir {
                prompt.push_str(&format!(
                    "- Or read the schema file for the group you need: \
                     `read(path=\"{}/<group>.md\")` where `<group>` is one of \
                     the group ids below (e.g. `calendar`, `files`). Each file \
                     contains ONLY that group's schemas, so reads stay small.\n",
                    dir.display(),
                ));
            }
            prompt.push('\n');
            for group in summarize_groups(tools) {
                let count = group.tool_count();
                prompt.push_str(&format!(
                    "- **{}** [id: `{}`] ({} tool{}): {}\n  Tools: {}\n",
                    group.display_name,
                    group.id,
                    count,
                    if count == 1 { "" } else { "s" },
                    group.one_liner,
                    group.tool_names.join(", "),
                ));
            }
            prompt.push('\n');

            // ── Tier 2: full schemas for revealed tools ──
            let revealed_tools: Vec<&athen_core::tool::ToolDefinition> = tools
                .iter()
                .filter(|t| revealed.contains(&t.name))
                .collect();
            if !revealed_tools.is_empty() {
                prompt.push_str("DETAILED TOOLS (schemas already loaded — call directly):\n");
                for tool in revealed_tools {
                    prompt.push_str(&format!("- **{}**: {}\n", tool.name, tool.description));
                }
                prompt.push('\n');
            }
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

        // Shell guidance. The WORKSPACE block uses the runtime-resolved
        // workspace path so Windows / macOS users get their native location
        // instead of a hardcoded `~/.athen/workspace`. We split this from
        // the static rest of the section because the rest contains literal
        // `{` / `}` braces (in shell_spawn examples) that would collide
        // with `format!` placeholders.
        if has_shell {
            let ws_display = athen_core::paths::athen_workspace_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unavailable>".to_string());
            prompt.push_str(&format!(
                "SHELL & FILES:\n\
                 Use shell_execute for system commands. For files use the dedicated tools: \
                 read (offset/limit, prefer over cat/head/tail), edit (exact-string replace, \
                 prefer over sed/awk), write (full overwrite — read first), grep (ripgrep \
                 search, prefer over grep/find), list_directory.\n\
                 Edit and write require a prior read of the same file (except for new files).\n\
                 \n\
                 WORKSPACE:\n\
                 Your default working directory is `{ws_display}`. Relative paths in \
                 read/edit/write/grep AND in shell_execute/shell_spawn resolve there — NOT \
                 against the directory the user happens to have launched the app from. \
                 A bare `write` with path \"test.html\" lands in your workspace, and a \
                 bare `shell_spawn` of `python3 -m http.server 8002` serves files from \
                 there. To touch a file outside your workspace, pass an absolute path \
                 the user has explicitly provided.\n\
                 \n",
            ));
            prompt.push_str(
                "LONG-RUNNING COMMANDS:\n\
                 shell_execute waits for the command to fully exit and EOF its stdio. A bare \
                 trailing `&` is NOT enough — the child inherits stdio pipes and keeps the call \
                 hanging. Patterns:\n\
                 - Time-bounded: `timeout 30 CMD` to cap runtime at 30s.\n\
                 - Multi-line input: HEREDOC, e.g. `python3 <<'EOF' ... EOF`.\n\
                 - Fallback for one-shot detached jobs: `nohup CMD >/tmp/cmd.log 2>&1 &`.\n\
                 The default tool timeout is 60s; pass `timeout_ms` up to 600000 for longer \
                 commands. On timeout the process is killed and you'll get a clear error — \
                 fix the command pattern, don't just bump the timeout.\n\
                 \n\
                 BACKGROUND PROCESSES (servers, watchers, anything that should outlive a single call):\n\
                 Use shell_spawn instead of `nohup CMD &`. It returns a PID and a log file path:\n\
                 - shell_spawn { command: \"python3 -m http.server 8002\", label: \"http-server\" }\n\
                   → { pid: 12345, log_path: \"/home/.../spawn-logs/http-server-...log\" }\n\
                 - shell_logs { pid: 12345, tail: 50 } → recent stdout/stderr\n\
                 - shell_kill { pid: 12345 } → graceful SIGTERM, then SIGKILL if it doesn't exit\n\
                 After spawning, give the process a moment (e.g. `sleep 1` via shell_execute) before \
                 hitting it, then check shell_logs to confirm it started cleanly. \
                 TIP: programs like Python buffer stderr when piped to a file — if shell_logs \
                 returns empty, try `python3 -u` (unbuffered), `PYTHONUNBUFFERED=1 python3 ...`, \
                 or `stdbuf -oL -eL <command>` for line-buffering.\n\n",
            );
        }

        // Web guidance. Surfaced when web_search / web_fetch are wired so
        // the model reaches for them instead of curl/wget via shell_execute.
        if has_web {
            prompt.push_str(
                "WEB ACCESS:\n\
                 You have two dedicated tools for the open web. ALWAYS prefer them over \
                 shelling out to curl/wget/lynx — they return clean markdown/snippets and \
                 strip noise (scripts, styles, CSS).\n\
                 - web_search { query, max_results? } → ranked hits (title, url, snippet). \
                   Use for: current/factual questions, finding canonical URLs, anything the \
                   model might not know post-training cutoff. Often the snippets alone are \
                   enough — read them before deciding to fetch a full page.\n\
                 - web_fetch { url } → readable markdown of one page. Use after web_search \
                   when a snippet looks promising and you need the full content. Also use \
                   when the user gives you a URL directly. web_fetch auto-falls-back through \
                   a JS-rendering reader and the Wayback Machine, so SPAs and paywalled/blocked \
                   pages usually still come back readable. The `source` field in the result \
                   tells you which tier answered (`local-*`, `jina`, or `wayback`).\n\
                 FALLBACK PATTERN: if web_fetch still returns near-empty content after all \
                 tiers, the page is genuinely unscrapable — pivot to web_search and work \
                 from the snippets instead of retrying.\n\
                 Do NOT use shell_execute with curl/wget/lynx for web content. The output \
                 is raw HTML the model wastes tokens parsing.\n\n",
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
                 - Use memory_store ONLY when the user explicitly asks to remember, save, or note \
                   something (\"remember that...\", \"save this...\", \"note for later...\"). \
                   Do NOT call memory_store for tasks like writing code, running commands, testing \
                   features, or answering questions — those don't involve remembering.\n\
                 - When the user mentions a person (\"mi novia\", \"my boss\", a name), check memory_recall \
                   AND contacts_list to find what you know about them BEFORE responding.\n\
                 - When writing something for/about a person, look up their name and details first.\n\
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
               (calendar_update, calendar_create, calendar_delete, write, edit, shell_execute), \
               answer CONTINUE.\n\
             - If the agent only used read tools (calendar_list, read, grep, list_directory, \
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
            std::time::Duration::from_secs(30),
            self.llm_router.route(&request),
        )
        .await
        {
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

        // Loop guard: count how many times each `(name, args)` has been
        // dispatched in this run. If a model gets stuck calling the same
        // thing repeatedly we short-circuit to break the cycle.
        let mut call_signature_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        const SIGNATURE_REPEAT_LIMIT: u32 = 3;

        // Gather available tools for the LLM.
        let available_tools = self.tool_registry.list_tools().await?;

        // Two-tier surfacing: only the "revealed" subset has its full schema
        // sent to the LLM each turn. Memory tools are revealed by default;
        // others enter the set on first dispatch (tolerant dispatch).
        let mut revealed_tools: HashSet<String> = available_tools
            .iter()
            .map(|t| t.name.clone())
            .filter(|name| is_always_revealed(name))
            .collect();

        // Prepend context messages (prior conversation history) before the
        // current task's user message so the agent has session memory.
        conversation.extend(self.context_messages.iter().cloned());

        // Seed the conversation with the task description as a user message
        conversation.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(task.description.clone()),
        });

        tracing::info!(task_id = %task_id, "Starting task execution");

        let has_context = !self.context_messages.is_empty();

        loop {
            // Rebuild the system prompt each iteration so newly-revealed
            // tools' full schemas appear inline. The prompt itself is small;
            // rebuilding is cheap.
            let system_prompt = Self::build_system_prompt(
                &available_tools,
                &revealed_tools,
                has_context,
                self.tool_doc_dir.as_deref(),
            );

            // Tools sent to the LLM API: only the revealed subset carries
            // schemas. The model sees others in the system-prompt index;
            // tolerant dispatch reveals them on first call.
            let revealed_tool_defs: Vec<athen_core::tool::ToolDefinition> = available_tools
                .iter()
                .filter(|t| revealed_tools.contains(&t.name))
                .cloned()
                .collect();
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
                    system_prompt: Some(system_prompt),
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

            // Build LLM request — only the revealed tool subset is sent.
            let request = LlmRequest {
                profile: ModelProfile::Fast,
                messages: conversation.clone(),
                max_tokens: Some(4096),
                temperature: Some(0.7),
                tools: if revealed_tool_defs.is_empty() {
                    None
                } else {
                    Some(revealed_tool_defs.clone())
                },
                system_prompt: Some(system_prompt),
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
                tracing::debug!(
                    thinking_len = inline_think.len(),
                    "extracted inline <think> tags"
                );
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
                    let should_continue = self
                        .judge_completion(&task.description, &response_content, &tools_called)
                        .await;

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

            // ── Pre-scan: track tool names + reveals (synchronous) ──
            // Doing this up front means we can launch the actual dispatches
            // in parallel without each future racing on `revealed_tools`.
            for tool_call in &response.tool_calls {
                tools_called.push(tool_call.name.clone());
                if !revealed_tools.contains(&tool_call.name)
                    && available_tools.iter().any(|t| t.name == tool_call.name)
                {
                    revealed_tools.insert(tool_call.name.clone());
                }
            }

            // Cancellation check before launching the batch.
            if let Some(ref flag) = self.cancel_flag {
                if flag.load(Ordering::Relaxed) {
                    tracing::info!(task_id = %task_id, "Task cancelled by user before tool batch");
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

            // ── Dispatch all tool calls in parallel ──
            // Independent tool calls run concurrently. Results come back in
            // input order. Identical calls within the batch share a single
            // result, and any signature called more than
            // SIGNATURE_REPEAT_LIMIT times is short-circuited as a loop.
            let registry: &dyn ToolRegistry = &*self.tool_registry;

            // Update signature counts and decide which calls to actually run.
            // For duplicates within the batch (same name+args), only the first
            // is dispatched; the others reuse its result.
            let mut first_index_by_signature: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut should_loop_guard = vec![false; response.tool_calls.len()];
            let mut dedup_target = vec![None::<usize>; response.tool_calls.len()];
            for (idx, tc) in response.tool_calls.iter().enumerate() {
                let sig = format!("{}|{}", tc.name, tc.arguments);
                *call_signature_counts.entry(sig.clone()).or_insert(0) += 1;
                if call_signature_counts[&sig] > SIGNATURE_REPEAT_LIMIT {
                    should_loop_guard[idx] = true;
                    continue;
                }
                if let Some(&first) = first_index_by_signature.get(&sig) {
                    dedup_target[idx] = Some(first);
                } else {
                    first_index_by_signature.insert(sig, idx);
                }
            }

            // Snapshot of executor budget at dispatch time. Shell tools
            // that take a `timeout_ms` clamp to this so the executor's own
            // timeout always fires first with a clean error rather than
            // racing the shell's internal timeout.
            let executor_remaining_ms = timeout_guard.remaining().as_millis() as u64;

            let dispatches = response.tool_calls.iter().enumerate().map(|(idx, tc)| {
                let name = tc.name.clone();
                let mut args = tc.arguments.clone();
                let started_at = Utc::now();
                let loop_guarded = should_loop_guard[idx];
                let dedup_of = dedup_target[idx];

                // Clamp timeout_ms for shell tools that accept it.
                let clamped_short_circuit = if name == "shell_execute" || name == "shell_spawn" {
                    clamp_shell_timeout(&mut args, executor_remaining_ms)
                } else {
                    None
                };

                async move {
                    if loop_guarded {
                        return (
                            started_at,
                            Ok(athen_core::tool::ToolResult {
                                success: false,
                                output: serde_json::json!({
                                    "loop_guard": true,
                                    "error": format!(
                                        "STOP. You have called '{name}' {SIGNATURE_REPEAT_LIMIT}+ times with identical arguments and made no progress. You are stuck in a loop. \
                                        Re-read the user's ORIGINAL request right now. \
                                        Identify what the user actually asked for — it is almost certainly NOT another call to '{name}'. \
                                        Pick a DIFFERENT tool that addresses the real task, or if no tool fits, respond with text explaining what you cannot do. \
                                        DO NOT call '{name}' again with these arguments."
                                    ),
                                }),
                                error: Some("loop_guard".to_string()),
                                execution_time_ms: 0,
                            }),
                        );
                    }
                    if dedup_of.is_some() {
                        // Duplicate within the same batch — return a stub
                        // pointing at the first call's result. The model
                        // should batch unique calls, not repeats.
                        return (
                            started_at,
                            Ok(athen_core::tool::ToolResult {
                                success: false,
                                output: serde_json::json!({
                                    "error": "Duplicate call in batch. Each parallel tool_call must be unique — see the result of the earlier identical call."
                                }),
                                error: Some("duplicate_in_batch".to_string()),
                                execution_time_ms: 0,
                            }),
                        );
                    }
                    if let Some(short) = clamped_short_circuit {
                        return (started_at, Ok(short));
                    }
                    let result = registry.call_tool(&name, args).await;
                    (started_at, result)
                }
            });
            let outcomes = futures::future::join_all(dispatches).await;

            // ── Process results in order: audit + thread into conversation ──
            for (tool_call, (started_at, tool_result)) in response.tool_calls.iter().zip(outcomes) {
                let (step_status, output) = match &tool_result {
                    Ok(result) => (
                        if result.success {
                            StepStatus::Completed
                        } else {
                            StepStatus::Failed
                        },
                        Some(serde_json::json!({
                            "tool": tool_call.name,
                            "args": tool_call.arguments,
                            "result": result.output,
                        })),
                    ),
                    Err(e) => (
                        StepStatus::Failed,
                        Some(serde_json::json!({
                            "tool": tool_call.name,
                            "args": tool_call.arguments,
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
                    Ok(result) => {
                        serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string())
                    }
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

        async fn call_tool(&self, _name: &str, _args: serde_json::Value) -> Result<CoreToolResult> {
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
        let router =
            MockLlmRouter::new(vec![MockLlmRouter::make_response("Task is done.", vec![])]);

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
            async fn get_steps(&self, task_id: athen_core::task::TaskId) -> Result<Vec<TaskStep>> {
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
        let (content, thinking) =
            extract_think_tags("<think>I need to consider this</think>Hello!");
        assert_eq!(content, "Hello!");
        assert_eq!(thinking, "I need to consider this");

        // No think tags → pass through
        let (content, thinking) = extract_think_tags("Just normal text");
        assert_eq!(content, "Just normal text");
        assert!(thinking.is_empty());

        // Think with JSON response after
        let (content, thinking) =
            extract_think_tags("<think>The user asks about X</think>{\"response\": \"\"}");
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
            "<think>First thought</think>Middle<think>Second thought</think>End",
        );
        assert_eq!(content, "MiddleEnd");
        assert_eq!(thinking, "First thought\nSecond thought");
    }

    #[tokio::test]
    async fn test_executor_cleans_json_response() {
        // Model returns JSON blob — executor should extract the text.
        let responses = vec![MockLlmRouter::make_response(r#"{"response": ""}"#, vec![])];

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

    // ── Two-tier tool surfacing ─────────────────────────────────────────

    fn tool_def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: serde_json::json!({"type": "object"}),
            backend: athen_core::tool::ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: athen_core::risk::BaseImpact::Read,
        }
    }

    #[test]
    fn system_prompt_lists_groups_and_only_revealed_details() {
        let tools = vec![
            tool_def("memory_store", "store a memory"),
            tool_def("memory_recall", "recall a memory"),
            tool_def("calendar_create", "create a calendar event"),
            tool_def("files__write_file", "write a file"),
        ];
        let mut revealed = HashSet::new();
        revealed.insert("memory_store".to_string());
        revealed.insert("memory_recall".to_string());

        let prompt = DefaultExecutor::build_system_prompt(&tools, &revealed, false, None);

        // Group index lists every group with counts.
        assert!(prompt.contains("AVAILABLE TOOL GROUPS"));
        assert!(prompt.contains("**Memory**"));
        assert!(prompt.contains("**Calendar**"));
        assert!(prompt.contains("**Files**"));

        // Detailed section only includes the revealed ones.
        assert!(prompt.contains("DETAILED TOOLS"));
        assert!(prompt.contains("memory_store"));
        assert!(prompt.contains("memory_recall"));
        // The non-revealed tools' user-facing descriptions should NOT
        // appear in the prompt at all (their names do, in the group index,
        // so the model knows they exist).
        assert!(
            !prompt.contains("create a calendar event"),
            "non-revealed tool description leaked into prompt"
        );
        assert!(
            !prompt.contains("write a file"),
            "non-revealed tool description leaked into prompt"
        );
        // But the names should be visible in the group index so the model
        // knows what to call (via tolerant dispatch).
        assert!(prompt.contains("calendar_create"));
        assert!(prompt.contains("files__write_file"));
    }

    #[test]
    fn system_prompt_includes_tool_doc_dir_when_set() {
        let tools = vec![tool_def("calendar_create", "create event")];
        let revealed = HashSet::new();
        let dir = std::path::PathBuf::from("/tmp/athen-test/tools");
        let prompt = DefaultExecutor::build_system_prompt(&tools, &revealed, false, Some(&dir));
        // Pattern reference uses the directory + <group>.md placeholder.
        assert!(prompt.contains("/tmp/athen-test/tools"));
        assert!(prompt.contains("<group>.md"));
        assert!(prompt.contains("read("));
        // Group id is shown in the index so the model knows the filename.
        assert!(prompt.contains("[id: `calendar`]"));
    }

    #[test]
    fn system_prompt_omits_doc_pointer_when_unset() {
        let tools = vec![tool_def("calendar_create", "create event")];
        let revealed = HashSet::new();
        let prompt = DefaultExecutor::build_system_prompt(&tools, &revealed, false, None);
        assert!(!prompt.contains("read("));
    }

    /// Tool registry that records dispatch *order* and sleeps in each call so
    /// we can prove the executor runs concurrent tool calls in parallel.
    struct OrderedSleepyRegistry {
        order: Arc<std::sync::Mutex<Vec<String>>>,
        sleep: Duration,
    }

    #[async_trait]
    impl ToolRegistry for OrderedSleepyRegistry {
        async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
            Ok(vec![
                tool_def("a", ""),
                tool_def("b", ""),
                tool_def("c", ""),
            ])
        }
        async fn call_tool(&self, name: &str, _args: serde_json::Value) -> Result<CoreToolResult> {
            self.order.lock().unwrap().push(name.to_string());
            tokio::time::sleep(self.sleep).await;
            Ok(CoreToolResult {
                success: true,
                output: serde_json::json!({"name": name}),
                error: None,
                execution_time_ms: 0,
            })
        }
    }

    #[tokio::test]
    async fn batched_tool_calls_run_in_parallel() {
        // Three slow tool calls in one response. If sequential, total ≥ 3 × sleep.
        // If parallel, total ≈ 1 × sleep. Use 200ms sleep, assert <500ms total.
        let calls = vec![
            ToolCall {
                id: "1".into(),
                name: "a".into(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "2".into(),
                name: "b".into(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                id: "3".into(),
                name: "c".into(),
                arguments: serde_json::json!({}),
            },
        ];
        let responses = vec![
            MockLlmRouter::make_response("Calling all three.", calls),
            MockLlmRouter::make_response("Done.", vec![]),
        ];

        let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let registry = OrderedSleepyRegistry {
            order: Arc::clone(&order),
            sleep: Duration::from_millis(200),
        };

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(registry),
            Box::new(InMemoryAuditor::new()),
            10,
            Duration::from_secs(60),
            vec![],
        );

        let started = std::time::Instant::now();
        let result = executor
            .execute(make_task("Run three things"))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(result.success);
        // 3 tool calls + 1 completion step.
        assert_eq!(result.steps_completed, 4);
        // Three 200ms sleeps run in parallel should finish well under 500ms.
        assert!(
            elapsed < Duration::from_millis(500),
            "expected parallel execution (<500ms), got {elapsed:?}"
        );
        // All three calls landed on the registry.
        assert_eq!(order.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn tolerant_dispatch_reveals_unrequested_known_tool() {
        // Model directly calls "calendar_create" without first calling
        // get_tool_details. The registry knows the tool — it should dispatch
        // and add it to revealed for future requests.
        let calls = vec![ToolCall {
            id: "1".into(),
            name: "calendar_create".into(),
            arguments: serde_json::json!({}),
        }];
        let responses = vec![
            MockLlmRouter::make_response("Creating event.", calls),
            MockLlmRouter::make_response("Done.", vec![]),
        ];

        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::new(
                vec![tool_def("calendar_create", "create event")],
                vec![CoreToolResult {
                    success: true,
                    output: serde_json::json!({"ok": true}),
                    error: None,
                    execution_time_ms: 1,
                }],
            )),
            Box::new(InMemoryAuditor::new()),
            5,
            Duration::from_secs(60),
            vec![],
        );

        let result = executor
            .execute(make_task("Create an event"))
            .await
            .unwrap();
        assert!(result.success);
        // 1 dispatch + 1 completion = 2 steps (no get_tool_details round-trip).
        assert_eq!(result.steps_completed, 2);
    }

    #[tokio::test]
    async fn loop_guard_short_circuits_repeated_calls() {
        // Five consecutive identical calls — the 4th and 5th should hit the
        // loop guard rather than reaching the registry.
        let make_call = || ToolCall {
            id: "x".to_string(),
            name: "calendar_list".to_string(),
            arguments: serde_json::json!({"start": "a", "end": "b"}),
        };
        let responses = vec![
            MockLlmRouter::make_response("1", vec![make_call()]),
            MockLlmRouter::make_response("2", vec![make_call()]),
            MockLlmRouter::make_response("3", vec![make_call()]),
            MockLlmRouter::make_response("4", vec![make_call()]),
            MockLlmRouter::make_response("5", vec![make_call()]),
            MockLlmRouter::make_response("done", vec![]),
        ];

        let auditor = Arc::new(InMemoryAuditor::new());
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
            async fn get_steps(&self, task_id: athen_core::task::TaskId) -> Result<Vec<TaskStep>> {
                self.0.get_steps(task_id).await
            }
        }

        let task = make_task("loop test");
        let task_id = task.id;
        // Registry returns the same result every time the model calls.
        let make_result = || CoreToolResult {
            success: true,
            output: serde_json::json!({"events": []}),
            error: None,
            execution_time_ms: 0,
        };
        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::new(
                vec![tool_def("calendar_list", "list events")],
                vec![make_result(), make_result(), make_result()],
            )),
            Box::new(ArcAuditor(Arc::clone(&auditor))),
            20,
            Duration::from_secs(60),
            vec![],
        );

        executor.execute(task).await.unwrap();

        let steps = auditor.get_steps(task_id).await.unwrap();
        // Find a step whose output mentions the loop guard.
        let guarded = steps.iter().any(|s| {
            s.output
                .as_ref()
                .and_then(|o| o["result"]["loop_guard"].as_bool())
                == Some(true)
        });
        assert!(guarded, "expected at least one loop-guarded step");
    }

    #[tokio::test]
    async fn duplicate_calls_in_one_batch_are_deduped() {
        let one = ToolCall {
            id: "a".to_string(),
            name: "calendar_list".to_string(),
            arguments: serde_json::json!({"start": "a", "end": "b"}),
        };
        let two = ToolCall {
            id: "b".to_string(),
            name: "calendar_list".to_string(),
            arguments: serde_json::json!({"start": "a", "end": "b"}),
        };
        let responses = vec![
            MockLlmRouter::make_response("listing twice", vec![one, two]),
            MockLlmRouter::make_response("done", vec![]),
        ];

        let auditor = Arc::new(InMemoryAuditor::new());
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
            async fn get_steps(&self, task_id: athen_core::task::TaskId) -> Result<Vec<TaskStep>> {
                self.0.get_steps(task_id).await
            }
        }

        let task = make_task("dedup test");
        let task_id = task.id;
        let executor = DefaultExecutor::new(
            Box::new(MockLlmRouter::new(responses)),
            Box::new(MockToolRegistry::new(
                vec![tool_def("calendar_list", "list events")],
                vec![CoreToolResult {
                    success: true,
                    output: serde_json::json!({"events": []}),
                    error: None,
                    execution_time_ms: 0,
                }],
            )),
            Box::new(ArcAuditor(Arc::clone(&auditor))),
            10,
            Duration::from_secs(60),
            vec![],
        );

        executor.execute(task).await.unwrap();

        let steps = auditor.get_steps(task_id).await.unwrap();
        let cal_steps: Vec<&TaskStep> = steps
            .iter()
            .filter(|s| s.description.contains("calendar_list"))
            .collect();
        assert_eq!(cal_steps.len(), 2);
        // The second call should be deduped (failed with duplicate_in_batch).
        let dup_count = cal_steps
            .iter()
            .filter(|s| {
                s.output
                    .as_ref()
                    .and_then(|o| o["result"]["error"].as_str())
                    .map(|e| e.contains("Duplicate call in batch"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(dup_count, 1, "exactly one of the two should be deduped");
    }
}

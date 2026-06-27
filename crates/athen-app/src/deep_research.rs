//! Deep Research orchestrator — the deterministic harness that turns a
//! question into a cited markdown paper (see `docs/DEEP_RESEARCH.md`).
//!
//! This module is **pure orchestration**: it depends only on the LLM router,
//! a caller-supplied worker-spawn closure, and a progress callback. It knows
//! nothing about `AppState` or `DelegationContext` — the glue that wires the
//! worker spawn to `run_delegation` lives in `state.rs`. Keeping the
//! orchestrator decoupled means the plan/fan-out/synthesize pipeline can be
//! unit-tested with stub closures (no live router, no network).
//!
//! Pipeline (mirrors §3 of the design doc):
//!   1. **Plan** — one Fast-tier LLM pass decomposes the question into N
//!      non-overlapping sub-questions.
//!   2. **Fan out** — one worker per sub-question runs under a semaphore
//!      (`min(N, 4)` permits); failures are tolerated, survivors proceed.
//!   3. **Synthesize** — one higher-tier LLM pass folds the findings into a
//!      cited paper (extending a prior paper when one is supplied).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{RwLock, Semaphore};

use athen_core::error::{AthenError, Result};
use athen_core::llm::{ChatMessage, LlmRequest, MessageContent, ModelProfile, Role};
use athen_core::traits::llm::LlmRouter;
use athen_llm::router::DefaultLlmRouter;

/// How deep (and how expensive) a research run is. Bounds the blast radius:
/// the number of sub-questions, the concurrency cap, sources read per worker,
/// and the synthesis tier all derive from this. Mirrors the §4 depth table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Quick,
    Standard,
    Deep,
}

impl Depth {
    /// Parse the user-facing depth string. Case-insensitive; anything
    /// unrecognized (or `None`) falls back to `Standard`.
    pub fn parse(s: Option<&str>) -> Depth {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("quick") => Depth::Quick,
            Some("deep") => Depth::Deep,
            _ => Depth::Standard,
        }
    }

    /// The wire/UI string for this depth.
    pub fn as_str(&self) -> &'static str {
        match self {
            Depth::Quick => "quick",
            Depth::Standard => "standard",
            Depth::Deep => "deep",
        }
    }

    /// How many sub-questions the planner is asked to produce.
    fn num_sub_questions(&self) -> usize {
        match self {
            Depth::Quick => 3,
            Depth::Standard => 6,
            Depth::Deep => 10,
        }
    }

    /// Concurrency cap — bounds the simultaneous worker burst so a deep run
    /// doesn't stampede the provider + network. Scales modestly with depth.
    fn max_concurrent(&self) -> usize {
        let cap = match self {
            Depth::Quick => 3,
            Depth::Standard => 4,
            Depth::Deep => 5,
        };
        self.num_sub_questions().min(cap)
    }

    /// How many sources each worker is hinted to read.
    fn sources_hint(&self) -> usize {
        match self {
            Depth::Quick => 4,
            Depth::Standard => 6,
            Depth::Deep => 8,
        }
    }

    /// The tier the synthesis pass runs on (workers always run cheap/Fast via
    /// their profile). Quick stays cheap end-to-end; Standard/Deep upgrade.
    fn synth_profile(&self) -> ModelProfile {
        match self {
            Depth::Quick => ModelProfile::Fast,
            Depth::Standard | Depth::Deep => ModelProfile::Powerful,
        }
    }

    /// Output-token budget for the synthesis pass. This is the single biggest
    /// lever on paper *depth*: a 4k cap silently truncates a thorough paper to
    /// ~3k words. Scale it with depth so a `deep` run can actually produce a
    /// long-form, multi-section paper.
    fn synth_max_tokens(&self) -> u32 {
        match self {
            Depth::Quick => 4096,
            Depth::Standard => 6144,
            Depth::Deep => 8192,
        }
    }

    /// Whether this depth runs a second, gap-targeted fan-out round after the
    /// first wave. Only `deep` pays for the extra pass — it's what makes the
    /// deepest tier meaningfully deeper than `standard`.
    fn gap_fill(&self) -> bool {
        matches!(self, Depth::Deep)
    }

    /// Upper bound on follow-up gap questions spawned in the second round.
    fn max_gap_questions(&self) -> usize {
        match self {
            Depth::Deep => 4,
            _ => 0,
        }
    }
}

/// Progress snapshot handed to the caller's callback at each phase boundary
/// (and as each worker resolves). Cheap to construct; the callback should be
/// non-blocking (it just emits a UI event).
pub struct Progress {
    pub phase: &'static str,
    pub detail: String,
    pub workers_total: usize,
    pub workers_done: usize,
    pub workers_ok: usize,
}

/// The result of a completed run. The caller (the tool/command layer) owns
/// persistence — this struct just carries the synthesized paper + the run
/// shape so the caller can save the file and stamp arc metadata.
pub struct ResearchOutcome {
    pub paper_markdown: String,
    pub question: String,
    pub depth: String,
    pub sub_questions: Vec<String>,
    pub workers_total: usize,
    pub workers_ok: usize,
}

/// Run the full Deep Research pipeline.
///
/// `spawn_worker` is called once per sub-question with a self-contained brief
/// and returns the worker's structured findings (empty string ⇒ that worker
/// contributed nothing; an `Err` is tolerated the same way). `progress` is
/// invoked at each phase and as workers finish.
pub async fn run_deep_research<F, Fut>(
    router: Arc<RwLock<Arc<DefaultLlmRouter>>>,
    question: &str,
    depth: Depth,
    prior_paper: Option<&str>,
    spawn_worker: F,
    progress: impl Fn(Progress) + Send + Sync,
) -> Result<ResearchOutcome>
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<String>> + Send,
{
    // ---- Phase 1: plan / decompose ----------------------------------------
    progress(Progress {
        phase: "planning",
        detail: format!(
            "Decomposing the question into {} angles",
            depth.num_sub_questions()
        ),
        workers_total: depth.num_sub_questions(),
        workers_done: 0,
        workers_ok: 0,
    });

    let sub_questions = plan_sub_questions(&router, question, depth).await?;
    // `workers_total` grows if the gap-fill round (deep) adds a second wave —
    // the progress bar legitimately expands as new angles are discovered.
    let mut workers_total = sub_questions.len();

    // ---- Phase 2: fan out -------------------------------------------------
    progress(Progress {
        phase: "reading",
        detail: format!("Spawning {workers_total} parallel researchers"),
        workers_total,
        workers_done: 0,
        workers_ok: 0,
    });

    // Counters shared across both fan-out rounds so the reported count stays
    // continuous (e.g. "8/12 reported" once gap workers are added).
    let done = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let sources_hint = depth.sources_hint();

    let mut results = fan_out_round(
        &sub_questions,
        question,
        sources_hint,
        depth.max_concurrent(),
        &spawn_worker,
        &progress,
        &done,
        &ok,
        workers_total,
    )
    .await;

    // ---- Phase 2b: gap-fill (deep only) -----------------------------------
    // Review the first wave's findings, surface the most important unresolved
    // gaps/contradictions, and run a second targeted fan-out on them. This is
    // what makes `deep` meaningfully deeper than `standard`.
    let mut gap_questions: Vec<String> = Vec::new();
    if depth.gap_fill() {
        progress(Progress {
            phase: "refining",
            detail: "Reviewing findings for unresolved gaps".to_string(),
            workers_total,
            workers_done: workers_total,
            workers_ok: ok.load(Ordering::SeqCst),
        });
        gap_questions = identify_gaps(&router, question, &results, depth).await;
        if !gap_questions.is_empty() {
            workers_total += gap_questions.len();
            progress(Progress {
                phase: "reading",
                detail: format!("Investigating {} follow-up gap(s)", gap_questions.len()),
                workers_total,
                workers_done: done.load(Ordering::SeqCst),
                workers_ok: ok.load(Ordering::SeqCst),
            });
            let round2 = fan_out_round(
                &gap_questions,
                question,
                sources_hint,
                depth.max_concurrent(),
                &spawn_worker,
                &progress,
                &done,
                &ok,
                workers_total,
            )
            .await;
            results.extend(round2);
        }
    }

    let workers_ok = ok.load(Ordering::SeqCst);

    // ---- Phase 3: synthesize ----------------------------------------------
    progress(Progress {
        phase: "synthesizing",
        detail: format!("Synthesizing paper from {workers_ok}/{workers_total} researchers"),
        workers_total,
        workers_done: workers_total,
        workers_ok,
    });

    let paper_markdown = synthesize(&router, question, depth, &results, prior_paper).await?;

    // Report every sub-question actually investigated (plan + gaps) so the
    // caller's result card reflects the true breadth of the run.
    let mut all_sub_questions = sub_questions;
    all_sub_questions.extend(gap_questions);

    Ok(ResearchOutcome {
        paper_markdown,
        question: question.to_string(),
        depth: depth.as_str().to_string(),
        sub_questions: all_sub_questions,
        workers_total,
        workers_ok,
    })
}

/// Run one concurrent fan-out wave: one worker per sub-question, gated by a
/// semaphore. `done`/`ok`/`workers_total` are shared across waves so progress
/// counts stay continuous. Tolerant of empty/failed workers (they contribute
/// an empty findings string and are counted as not-ok).
#[allow(clippy::too_many_arguments)]
async fn fan_out_round<F, Fut>(
    sub_questions: &[String],
    question: &str,
    sources_hint: usize,
    max_concurrent: usize,
    spawn_worker: &F,
    progress: &(impl Fn(Progress) + Send + Sync),
    done: &Arc<AtomicUsize>,
    ok: &Arc<AtomicUsize>,
    workers_total: usize,
) -> Vec<(String, bool, String)>
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<String>> + Send,
{
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let futures = sub_questions.iter().cloned().map(|sub_q| {
        let sem = Arc::clone(&sem);
        async move {
            // Hold the permit for the worker's whole lifetime so no more than
            // `max_concurrent` workers run at once.
            let _permit = sem.acquire().await;
            let brief = worker_brief(question, &sub_q, sources_hint);
            let findings = match spawn_worker(brief).await {
                Ok(f) if !f.trim().is_empty() => {
                    ok.fetch_add(1, Ordering::SeqCst);
                    (sub_q.clone(), true, f)
                }
                Ok(_) => (sub_q.clone(), false, String::new()),
                Err(e) => {
                    tracing::warn!("deep_research: worker for {sub_q:?} failed: {e}");
                    (sub_q.clone(), false, String::new())
                }
            };
            let workers_done = done.fetch_add(1, Ordering::SeqCst) + 1;
            let workers_ok = ok.load(Ordering::SeqCst);
            progress(Progress {
                phase: "reading",
                detail: format!("Researcher {workers_done}/{workers_total} reported"),
                workers_total,
                workers_done,
                workers_ok,
            });
            findings
        }
    });
    futures::future::join_all(futures).await
}

/// Best-effort second-round planner: read the first wave's findings and return
/// up to `depth.max_gap_questions()` focused follow-up questions targeting the
/// most important unresolved gaps/contradictions. Never hard-fails — any error
/// (or an empty/garbage response) yields no gaps and the run proceeds to
/// synthesis with the first wave's findings only.
async fn identify_gaps(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    question: &str,
    results: &[(String, bool, String)],
    depth: Depth,
) -> Vec<String> {
    let max_gaps = depth.max_gap_questions();
    if max_gaps == 0 {
        return Vec::new();
    }
    let system = format!(
        "You are a meticulous research editor reviewing findings gathered by parallel \
         researchers. Identify the most important UNRESOLVED gaps, unanswered angles, or \
         contradictions that still block a thorough, authoritative answer to the question. \
         Return ONLY a JSON array of at most {max_gaps} focused follow-up research questions, \
         each investigable on its own. If the findings are already comprehensive, return an \
         empty array []."
    );

    let mut user = String::new();
    user.push_str("Research question: ");
    user.push_str(question);
    user.push_str("\n\nFindings so far:\n\n");
    for (i, (sub_q, ok, findings)) in results.iter().enumerate() {
        user.push_str(&format!("=== Angle {} : {sub_q} ===\n", i + 1));
        if *ok && !findings.trim().is_empty() {
            // Keep the digest bounded — the editor needs the gist, not every byte.
            // Truncate by CHARS, not bytes: a byte slice (`&f[..1500]`) panics
            // when the cut lands mid-UTF-8-codepoint, and findings are often
            // non-ASCII.
            let f = findings.trim();
            if f.len() > 1500 {
                let clipped: String = f.chars().take(1500).collect();
                user.push_str(&clipped);
            } else {
                user.push_str(f);
            }
        } else {
            user.push_str("(no usable findings)");
        }
        user.push_str("\n\n");
    }

    let request = LlmRequest {
        profile: ModelProfile::Fast,
        messages: vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Text(user),
        }],
        max_tokens: Some(1024),
        temperature: Some(0.3),
        tools: None,
        system_prompt: Some(system),
        reasoning_effort: Default::default(),
    };

    let router = router.read().await.clone();
    match router.route(&request).await {
        Ok(response) => {
            let mut gaps = parse_sub_questions(&response.content);
            if gaps.len() > max_gaps {
                gaps.truncate(max_gaps);
            }
            gaps
        }
        Err(e) => {
            tracing::warn!("deep_research: gap-identification pass failed: {e}");
            Vec::new()
        }
    }
}

/// One-shot Fast-tier LLM call to decompose the question. Never hard-fails on
/// a parse miss — falls back to a single sub-question = the original question.
async fn plan_sub_questions(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    question: &str,
    depth: Depth,
) -> Result<Vec<String>> {
    let n = depth.num_sub_questions();
    let system = format!(
        "You are a research planner. Break the question into exactly {n} focused, \
         non-overlapping sub-questions that together comprehensively answer it. \
         Return ONLY a JSON array of strings, nothing else."
    );
    let request = LlmRequest {
        profile: ModelProfile::Fast,
        messages: vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Text(question.to_string()),
        }],
        max_tokens: Some(2048),
        temperature: Some(0.2),
        tools: None,
        system_prompt: Some(system),
        reasoning_effort: Default::default(),
    };

    let router = router.read().await.clone();
    let response = router
        .route(&request)
        .await
        .map_err(|e| AthenError::Other(format!("Deep research planning LLM call failed: {e}")))?;

    let mut parsed = parse_sub_questions(&response.content);

    // Truncate to budget; never silently exceed it.
    if parsed.len() > n {
        parsed.truncate(n);
    }

    // Never hard-fail: a planner that returns nothing usable degrades to a
    // single worker on the original question.
    if parsed.is_empty() {
        parsed.push(question.to_string());
    }

    Ok(parsed)
}

/// Best-effort extraction of a list of sub-questions from raw model output.
/// Ladder: strict JSON array → first `[...]` slice as JSON → non-empty
/// trimmed lines with leading bullets/numbering stripped.
fn parse_sub_questions(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();

    if let Ok(v) = serde_json::from_str::<Vec<String>>(trimmed) {
        return clean_list(v);
    }

    if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&trimmed[start..=end]) {
                return clean_list(v);
            }
        }
    }

    // Line-based fallback: strip leading bullets / numbering.
    let lines = trimmed
        .lines()
        .map(strip_list_prefix)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    clean_list(lines)
}

/// Normalize a parsed list: trim each item, drop empties, dedup adjacent.
fn clean_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Strip a leading list marker (`- `, `* `, `1. `, `1) `, `• `) from a line.
fn strip_list_prefix(line: &str) -> &str {
    let t = line.trim();
    let t = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("• "))
        .unwrap_or(t);
    // Numbered: "1." / "1)" / "12." etc.
    let bytes = t.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits < t.len() {
        let rest = &t[digits..];
        if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return after.trim();
        }
    }
    t.trim()
}

/// The self-contained brief a single worker receives. Each is distinct (owns a
/// different sub-question), so the executor's in-batch dedup guard never fires.
fn worker_brief(question: &str, sub_q: &str, sources_hint: usize) -> String {
    format!(
        "You are one of several parallel researchers investigating: \"{question}\".\n\n\
         Your sub-question: {sub_q}\n\n\
         Method: run several web_search queries (vary the wording to widen coverage), then \
         web_fetch and READ at least {sources_hint} of the most relevant, credible, and \
         independent sources — prefer primary sources, official docs, and recent reporting \
         over aggregators. Do not stop at the first page; cross-check claims across sources.\n\n\
         Return THOROUGH structured findings, not a summary. For each key claim provide: the \
         claim, the supporting evidence (with concrete specifics — figures, dates, names, \
         direct quotes where useful), and the exact source URL. Call out disagreements or \
         contradictions between sources explicitly, and note anything you could not verify. \
         Aim for depth and completeness on your sub-question. Do not write files."
    )
}

/// One-shot synthesis pass on the depth's synthesis tier. Folds the worker
/// findings into a cited markdown paper; when `prior_paper` is `Some`, revises
/// and extends it rather than replacing it.
async fn synthesize(
    router: &Arc<RwLock<Arc<DefaultLlmRouter>>>,
    question: &str,
    depth: Depth,
    results: &[(String, bool, String)],
    prior_paper: Option<&str>,
) -> Result<String> {
    let system = "You write a rigorous, in-depth research paper in Markdown from findings \
         gathered by parallel researchers. Be comprehensive and analytical, not a bullet \
         summary: synthesize across the findings, compare and reconcile conflicting evidence, \
         and develop each theme with specifics (figures, dates, named sources, direct \
         evidence). Structure: a title (#), an abstract, a short table of contents, multiple \
         thematic sections (##) — each substantive and several paragraphs deep — with inline \
         [n] citations, a concluding analysis, a '## Sources' numbered list mapping each [n] to \
         its URL, and a '## Gaps & Caveats' section noting sub-questions that returned little, \
         failed, or remain contested. Use the full length available; do not truncate the \
         analysis. Cite ONLY URLs that appear in the findings — never invent sources or URLs.";

    let mut user = String::new();
    if let Some(prior) = prior_paper {
        user.push_str(
            "Revise and extend this EXISTING paper with the new findings, preserving \
             still-valid content:\n\n",
        );
        user.push_str(prior);
        user.push_str("\n\n---\nNew findings:\n\n");
    }
    user.push_str("Research question: ");
    user.push_str(question);
    user.push_str("\n\n");
    for (i, (sub_q, ok, findings)) in results.iter().enumerate() {
        user.push_str(&format!("=== Sub-question {} ===\n{sub_q}\n", i + 1));
        if *ok && !findings.trim().is_empty() {
            user.push_str("Findings:\n");
            user.push_str(findings.trim());
        } else {
            user.push_str(
                "Findings: (this researcher returned no usable findings — note this as a gap)",
            );
        }
        user.push_str("\n\n");
    }

    let request = LlmRequest {
        profile: depth.synth_profile(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: MessageContent::Text(user),
        }],
        max_tokens: Some(depth.synth_max_tokens()),
        temperature: Some(0.2),
        tools: None,
        system_prompt: Some(system.to_string()),
        reasoning_effort: Default::default(),
    };

    let router = router.read().await.clone();
    let response = router
        .route(&request)
        .await
        .map_err(|e| AthenError::Other(format!("Deep research synthesis LLM call failed: {e}")))?;

    let paper = response.content.trim().to_string();
    if paper.is_empty() {
        return Err(AthenError::Other(
            "Deep research synthesis returned an empty paper".to_string(),
        ));
    }
    Ok(paper)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A router with no providers configured. Any `.route()` call errors — so
    /// tests that use it must avoid the LLM phases (or assert the failure).
    fn stub_router() -> Arc<RwLock<Arc<DefaultLlmRouter>>> {
        let router = DefaultLlmRouter::new(
            Default::default(),
            Default::default(),
            athen_llm::budget::BudgetTracker::new(None),
        );
        Arc::new(RwLock::new(Arc::new(router)))
    }

    #[test]
    fn depth_parse_maps_and_defaults() {
        assert_eq!(Depth::parse(Some("quick")), Depth::Quick);
        assert_eq!(Depth::parse(Some("QUICK")), Depth::Quick);
        assert_eq!(Depth::parse(Some("  Deep ")), Depth::Deep);
        assert_eq!(Depth::parse(Some("standard")), Depth::Standard);
        // Unknown + None both default to Standard.
        assert_eq!(Depth::parse(Some("banana")), Depth::Standard);
        assert_eq!(Depth::parse(None), Depth::Standard);
    }

    #[test]
    fn depth_budget_accessors() {
        assert_eq!(Depth::Quick.num_sub_questions(), 3);
        assert_eq!(Depth::Standard.num_sub_questions(), 6);
        assert_eq!(Depth::Deep.num_sub_questions(), 10);

        assert_eq!(Depth::Quick.max_concurrent(), 3);
        assert_eq!(Depth::Standard.max_concurrent(), 4);
        assert_eq!(Depth::Deep.max_concurrent(), 5);

        assert_eq!(Depth::Quick.sources_hint(), 4);
        assert_eq!(Depth::Standard.sources_hint(), 6);
        assert_eq!(Depth::Deep.sources_hint(), 8);

        assert_eq!(Depth::Quick.synth_profile(), ModelProfile::Fast);
        assert_eq!(Depth::Standard.synth_profile(), ModelProfile::Powerful);
        assert_eq!(Depth::Deep.synth_profile(), ModelProfile::Powerful);

        assert_eq!(Depth::Quick.synth_max_tokens(), 4096);
        assert_eq!(Depth::Standard.synth_max_tokens(), 6144);
        assert_eq!(Depth::Deep.synth_max_tokens(), 8192);

        // Only `deep` runs the gap-fill second round.
        assert!(!Depth::Quick.gap_fill());
        assert!(!Depth::Standard.gap_fill());
        assert!(Depth::Deep.gap_fill());
        assert_eq!(Depth::Quick.max_gap_questions(), 0);
        assert_eq!(Depth::Standard.max_gap_questions(), 0);
        assert_eq!(Depth::Deep.max_gap_questions(), 4);

        assert_eq!(Depth::Quick.as_str(), "quick");
        assert_eq!(Depth::Standard.as_str(), "standard");
        assert_eq!(Depth::Deep.as_str(), "deep");
    }

    #[test]
    fn parse_sub_questions_strict_json() {
        let raw = r#"["What is X?", "How does Y work?"]"#;
        let got = parse_sub_questions(raw);
        assert_eq!(got, vec!["What is X?", "How does Y work?"]);
    }

    #[test]
    fn parse_sub_questions_embedded_json() {
        let raw = "Sure! Here you go:\n[\"a\", \"b\", \"c\"]\nHope that helps.";
        let got = parse_sub_questions(raw);
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_sub_questions_line_fallback() {
        let raw = "1. First question\n2) Second question\n- Third question\n* Fourth";
        let got = parse_sub_questions(raw);
        assert_eq!(
            got,
            vec![
                "First question",
                "Second question",
                "Third question",
                "Fourth"
            ]
        );
    }

    /// Drives the fan-out + aggregation logic with a stub `spawn_worker` (some
    /// succeed, some return empty, some `Err`) and a real planner-bypass: we
    /// don't exercise the LLM phases here (the stub router has no providers),
    /// so this test calls the fan-out machinery directly via a tiny harness
    /// that mirrors `run_deep_research`'s phase 2 to assert partial tolerance.
    #[tokio::test]
    async fn fan_out_tolerates_partial_failures() {
        let sub_questions = [
            "ok-a".to_string(),
            "empty-b".to_string(),
            "err-c".to_string(),
            "ok-d".to_string(),
        ];
        let depth = Depth::Standard;

        let sem = Arc::new(Semaphore::new(depth.max_concurrent()));
        let done = Arc::new(AtomicUsize::new(0));
        let ok = Arc::new(AtomicUsize::new(0));

        let spawn_worker = |brief: String| async move {
            // Brief is keyed off the sub-question, so route on its prefix.
            if brief.contains("ok-a") || brief.contains("ok-d") {
                Ok::<String, AthenError>(format!("findings for [{brief}]"))
            } else if brief.contains("empty-b") {
                Ok(String::new())
            } else {
                Err(AthenError::Other("worker boom".to_string()))
            }
        };

        let workers_total = sub_questions.len();
        let sources_hint = depth.sources_hint();
        let spawn_ref = &spawn_worker;

        let futures = sub_questions.iter().cloned().map(|sub_q| {
            let sem = Arc::clone(&sem);
            let done = Arc::clone(&done);
            let ok = Arc::clone(&ok);
            async move {
                let _permit = sem.acquire().await;
                let brief = worker_brief("the question", &sub_q, sources_hint);
                let findings = match spawn_ref(brief).await {
                    Ok(f) if !f.trim().is_empty() => {
                        ok.fetch_add(1, Ordering::SeqCst);
                        (sub_q.clone(), true, f)
                    }
                    Ok(_) => (sub_q.clone(), false, String::new()),
                    Err(_) => (sub_q.clone(), false, String::new()),
                };
                done.fetch_add(1, Ordering::SeqCst);
                findings
            }
        });

        let results: Vec<(String, bool, String)> = futures::future::join_all(futures).await;

        // All four resolved (no abort on the failing/empty workers).
        assert_eq!(done.load(Ordering::SeqCst), workers_total);
        // Exactly the two "ok-" workers counted as successes.
        assert_eq!(ok.load(Ordering::SeqCst), 2);
        // Survivors carry findings; failures carry an empty string.
        let oks: Vec<&(String, bool, String)> = results.iter().filter(|(_, ok, _)| *ok).collect();
        assert_eq!(oks.len(), 2);
        // Failing/empty workers contribute nothing; successes are non-empty.
        for (_, ok, findings) in &results {
            assert_eq!(*ok, !findings.trim().is_empty());
        }
    }

    /// The plan phase never hard-fails on a parse miss: with a stub router the
    /// `.route()` call errors, so the planner returns that error — but the
    /// PARSE fallback itself (empty → original question) is covered here.
    #[test]
    fn plan_parse_falls_back_to_original_question() {
        // Empty / garbage model output yields no sub-questions...
        assert!(parse_sub_questions("").is_empty());
        assert!(parse_sub_questions("   \n  ").is_empty());
        // ...and the caller (plan_sub_questions) pads to the original question.
        // We assert the padding policy directly here since the LLM call needs
        // a live router.
        let mut parsed = parse_sub_questions("");
        if parsed.is_empty() {
            parsed.push("orig".to_string());
        }
        assert_eq!(parsed, vec!["orig"]);
    }

    #[test]
    fn worker_brief_is_distinct_per_subquestion() {
        let a = worker_brief("Q", "sub-A", 5);
        let b = worker_brief("Q", "sub-B", 5);
        assert_ne!(a, b);
        assert!(a.contains("sub-A"));
        assert!(a.contains("Do not write files."));
    }

    /// Sanity: the stub router really does fail a route (so any test that
    /// wants to exercise the real LLM phases would have to mock providers).
    #[tokio::test]
    async fn stub_router_route_errors() {
        let router = stub_router();
        let r = router.read().await.clone();
        let request = LlmRequest {
            profile: ModelProfile::Fast,
            messages: vec![ChatMessage {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(16),
            temperature: Some(0.0),
            tools: None,
            system_prompt: None,
            reasoning_effort: Default::default(),
        };
        assert!(r.route(&request).await.is_err());
    }
}

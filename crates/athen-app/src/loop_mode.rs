//! Loop Mode orchestrator — the deterministic harness that drives a per-arc
//! **Lead → Evaluator** iteration loop for goal-shaped work (see
//! `docs/LOOP_MODE.md`).
//!
//! This module is **pure orchestration**, in the exact way `deep_research.rs`
//! is: it depends only on two caller-supplied role-spawn closures (one for the
//! Lead, one for the Evaluator), a `read_verdict` accessor for the human-owned
//! `loop/VERDICT.md` file, and a progress callback. It knows nothing about
//! `AppState`, `DelegationContext`, the DB, or Tauri — the glue that wires the
//! role spawns to `run_delegation` and the `loop/` file protocol to disk lives
//! in `state.rs`. Keeping the orchestrator decoupled means the whole loop —
//! verdict grammar, all stop conditions, the Lead-before-Evaluator ordering —
//! can be unit-tested with stub closures (no live router, no network, no DB).
//!
//! The loop, concretely (mirrors the design doc's pipeline):
//! ```text
//! for iter in 1..=max_iterations:
//!   lead    = spawn_lead(iter)       ← reads GOAL/VERDICT/STATE, plans,
//!                                       implements, writes REPORT.md
//!   verdict = spawn_evaluator(iter)  ← reads GOAL/PLAN + diff + own test runs
//!                                       FIRST, then REPORT; writes VERDICT.md
//!   match verdict:
//!     SHIP | BLOCKED       → stop
//!     ITERATE (same twice) → coerce to BLOCKED, stop
//!     ITERATE              → continue
//!   cap reached            → stop
//! ```
//!
//! The Evaluator delegation returns its verdict as a **structured deliverable**
//! (the first line of its returned text, parsed by [`Verdict::parse`]). That is
//! cross-checked against the first line of `loop/VERDICT.md` (via the
//! `read_verdict` closure), which stays the human-readable source of truth: on
//! disagreement the file wins and the divergence is recorded on the outcome, so
//! the orchestrator never string-parses the deliverable alone.

// This module lands the pure orchestrator layer, decoupled from `AppState` (see
// `docs/LOOP_MODE.md` phase 1). Its public surface (`run_trio_loop`, the
// `Verdict`/`LoopStop`/`LoopOutcome`/`LoopProgress` types, and the embedded role
// prompts) is consumed by `AppState::run_loop_for_arc` in `state.rs` (the state
// glue) as well as exercised by the unit tests below — so it is a live
// production API, no module-level `dead_code` allowance required.

use athen_core::error::Result;

/// The Evaluator's ruling on one iteration. The grammar is fixed: the first
/// line of `loop/VERDICT.md` (and of the evaluator delegation's returned text)
/// must be exactly `VERDICT: SHIP`, `VERDICT: ITERATE`, or `VERDICT: BLOCKED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// All acceptance criteria pass and the goal is satisfied — ready for human
    /// review (never "merged"). Stops the loop.
    Ship,
    /// A blocking issue remains; the Lead should try again. Continues the loop.
    Iterate,
    /// The loop cannot converge without a human decision. Stops the loop.
    Blocked,
}

impl Verdict {
    /// Parse a single verdict line per the grammar. The line must begin with the
    /// `VERDICT:` keyword followed by exactly one of `SHIP` / `ITERATE` /
    /// `BLOCKED`. The keyword and the value are matched case-insensitively and
    /// surrounding whitespace is tolerated, but nothing else may follow the
    /// value (so `VERDICT: SHIP IT` is rejected — it isn't a clean ruling).
    /// Anything that doesn't match ⇒ `None`.
    pub fn parse(first_line: &str) -> Option<Verdict> {
        let line = first_line.trim();
        // Split on the first ':' so we can match the keyword case-insensitively.
        let (keyword, rest) = line.split_once(':')?;
        if !keyword.trim().eq_ignore_ascii_case("verdict") {
            return None;
        }
        match rest.trim().to_ascii_uppercase().as_str() {
            "SHIP" => Some(Verdict::Ship),
            "ITERATE" => Some(Verdict::Iterate),
            "BLOCKED" => Some(Verdict::Blocked),
            _ => None,
        }
    }

    /// The canonical wire/UI string for this verdict.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Ship => "SHIP",
            Verdict::Iterate => "ITERATE",
            Verdict::Blocked => "BLOCKED",
        }
    }
}

/// Parse a verdict from the full text of a `loop/VERDICT.md` file (or an
/// evaluator delegation's returned deliverable). Only the **first non-empty
/// line** is consulted — the evaluator contract mandates the ruling be on that
/// line. An empty/whitespace-only file, or a first line that doesn't match the
/// grammar, yields `None`.
pub fn parse_verdict_file(contents: &str) -> Option<Verdict> {
    let first = contents.lines().find(|l| !l.trim().is_empty())?;
    Verdict::parse(first)
}

/// Why the loop stopped. Every terminal path is one of these — there is no
/// silent fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStop {
    /// Evaluator ruled SHIP — ready for human review.
    Ship,
    /// Evaluator ruled BLOCKED — needs a human decision.
    Blocked,
    /// Two consecutive identical ITERATE verdicts — the loop is stuck and is
    /// escalated to BLOCKED per the design doc's plateau rule.
    BlockedDoubleIterate,
    /// The iteration budget ran out without a SHIP/BLOCKED ruling.
    CapReached,
    /// The caller requested a stop (via the `should_stop` predicate — e.g. a
    /// user hitting "stop loop"). Cooperative: checked between role delegations,
    /// so an in-flight Lead/Evaluator completes before the loop unwinds.
    Stopped,
}

impl LoopStop {
    /// The wire/UI string for this stop reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopStop::Ship => "ship",
            LoopStop::Blocked => "blocked",
            LoopStop::BlockedDoubleIterate => "blocked_double_iterate",
            LoopStop::CapReached => "cap_reached",
            LoopStop::Stopped => "stopped",
        }
    }
}

/// The result of a completed loop run. The caller (state/command layer) owns
/// persistence — this struct just carries the terminal state so the caller can
/// stamp arc metadata and notify the user.
#[derive(Debug, Clone)]
pub struct LoopOutcome {
    /// Why the loop stopped.
    pub stop: LoopStop,
    /// How many Lead→Evaluator iterations actually ran (1-based count).
    pub iterations_run: u32,
    /// The verdict parsed on the final iteration (`None` if the last
    /// evaluator produced no parseable verdict).
    pub last_verdict: Option<Verdict>,
    /// `true` if, on any iteration, the verdict parsed from the evaluator's
    /// returned deliverable disagreed with the `loop/VERDICT.md` first line.
    /// The file was treated as the source of truth; this flags the divergence
    /// for the caller/human.
    pub verdict_source_disagreement: bool,
}

/// Progress snapshot handed to the caller's callback at each phase boundary.
/// Cheap to construct; the callback should be non-blocking (it just emits a UI
/// event).
pub struct LoopProgress {
    /// 1-based iteration number currently in flight.
    pub iteration: u32,
    /// The configured iteration cap.
    pub max_iterations: u32,
    /// Coarse phase within the iteration: `"lead"`, `"evaluator"`, or `"done"`.
    pub phase: &'static str,
    /// Human-readable detail line for the UI.
    pub detail: String,
    /// The verdict decided so far this run (`None` before the first evaluator
    /// completes), as its wire string.
    pub last_verdict: Option<&'static str>,
}

/// Run the full Lead → Evaluator loop.
///
/// `spawn_lead(iter)` runs one Lead delegation for iteration `iter` (it reads
/// GOAL/VERDICT/STATE, updates PLAN.md, implements, writes REPORT.md); its
/// returned text is informational. `spawn_evaluator(iter)` runs one Evaluator
/// delegation; its returned text's first line is parsed as the structured
/// verdict deliverable. `read_verdict()` returns the current first line of
/// `loop/VERDICT.md` (the human-readable source of truth) — the two are
/// cross-checked. `progress` is invoked at each phase boundary.
///
/// The loop is a sequential await chain (Lead, then Evaluator, per iteration);
/// there is no orchestrator-level fan-out — fan-out belongs to the Lead via
/// `spawn_subagent` inside `spawn_lead`.
///
/// `should_stop()` is a cooperative cancellation predicate polled at each
/// iteration boundary (before the Lead, and again after the verdict dispatch):
/// when it returns `true` the loop unwinds with [`LoopStop::Stopped`]. It is
/// checked BETWEEN role delegations — a stop request cannot interrupt an
/// in-flight Lead/Evaluator (same between-steps semantics as the executor's
/// cancel flag), so the tree is always left at a clean role boundary.
pub async fn run_trio_loop<FL, FutL, FE, FutE>(
    max_iterations: u32,
    spawn_lead: FL,
    spawn_evaluator: FE,
    read_verdict: impl Fn() -> Option<String> + Send + Sync,
    progress: impl Fn(LoopProgress) + Send + Sync,
    should_stop: impl Fn() -> bool + Send + Sync,
) -> Result<LoopOutcome>
where
    FL: Fn(u32) -> FutL + Send + Sync,
    FutL: std::future::Future<Output = Result<String>> + Send,
    FE: Fn(u32) -> FutE + Send + Sync,
    FutE: std::future::Future<Output = Result<String>> + Send,
{
    // The cap is 1-based and must be at least 1 — a zero cap would run nothing,
    // which is never the intent; clamp it up so the loop always does real work.
    let cap = max_iterations.max(1);

    let mut iterations_run: u32 = 0;
    let mut last_verdict: Option<Verdict> = None;
    let mut verdict_source_disagreement = false;
    // Track the previous iteration's verdict so two identical consecutive
    // ITERATEs can be coerced to BLOCKED (the design doc's plateau rule).
    let mut prev_verdict: Option<Verdict> = None;

    for iter in 1..=cap {
        // ---- Cooperative stop check (before starting a new iteration) ----
        // If the caller asked to stop, unwind here. `iterations_run` still
        // reflects the last COMPLETED iteration (0 if stopped before iter 1),
        // so a stop between iterations reports honestly.
        if should_stop() {
            let stopped_after = iterations_run;
            progress(LoopProgress {
                iteration: stopped_after,
                max_iterations: cap,
                phase: "done",
                detail: format!(
                    "Loop stopped by request after iteration {stopped_after}/{cap}"
                ),
                last_verdict: last_verdict.map(|v| v.as_str()),
            });
            return Ok(LoopOutcome {
                stop: LoopStop::Stopped,
                iterations_run,
                last_verdict,
                verdict_source_disagreement,
            });
        }

        iterations_run = iter;

        // ---- Lead: plan + implement -------------------------------------
        progress(LoopProgress {
            iteration: iter,
            max_iterations: cap,
            phase: "lead",
            detail: format!("Iteration {iter}/{cap} — Lead planning & implementing"),
            last_verdict: last_verdict.map(|v| v.as_str()),
        });
        // The Lead's returned text is informational (its real output is the
        // working tree + REPORT.md). We surface an error but do not swallow it:
        // a Lead delegation that hard-fails aborts the run.
        let _lead_output = spawn_lead(iter).await?;

        // ---- Evaluator: adversarial verify ------------------------------
        progress(LoopProgress {
            iteration: iter,
            max_iterations: cap,
            phase: "evaluator",
            detail: format!("Iteration {iter}/{cap} — Evaluator verifying"),
            last_verdict: last_verdict.map(|v| v.as_str()),
        });
        let eval_output = spawn_evaluator(iter).await?;

        // Structured deliverable: parse the verdict from the evaluator's
        // returned text (first line).
        let deliverable_verdict = parse_verdict_file(&eval_output);
        // Source of truth: the first line of loop/VERDICT.md.
        let file_verdict = read_verdict().and_then(|s| parse_verdict_file(&s));

        // Cross-check. The file wins (design doc: it "stays the human-readable
        // source of truth"); a mismatch is recorded but does not change the
        // ruling. When the file has no parseable verdict we fall back to the
        // deliverable so a filesystem hiccup doesn't strand the loop.
        let verdict = match (file_verdict, deliverable_verdict) {
            (Some(f), Some(d)) => {
                if f != d {
                    verdict_source_disagreement = true;
                    tracing::warn!(
                        "loop_mode: iteration {iter} verdict mismatch — VERDICT.md says {:?}, \
                         evaluator deliverable says {:?}; using the file",
                        f,
                        d
                    );
                }
                Some(f)
            }
            (Some(f), None) => Some(f),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        };

        last_verdict = verdict;

        // ---- Stop-condition dispatch ------------------------------------
        match verdict {
            Some(Verdict::Ship) => {
                emit_done(&progress, iter, cap, Verdict::Ship);
                return Ok(LoopOutcome {
                    stop: LoopStop::Ship,
                    iterations_run,
                    last_verdict,
                    verdict_source_disagreement,
                });
            }
            Some(Verdict::Blocked) => {
                emit_done(&progress, iter, cap, Verdict::Blocked);
                return Ok(LoopOutcome {
                    stop: LoopStop::Blocked,
                    iterations_run,
                    last_verdict,
                    verdict_source_disagreement,
                });
            }
            Some(Verdict::Iterate) => {
                // Plateau check: two identical consecutive ITERATE verdicts ⇒
                // the loop is stuck; escalate to BLOCKED.
                if prev_verdict == Some(Verdict::Iterate) {
                    emit_done(&progress, iter, cap, Verdict::Blocked);
                    return Ok(LoopOutcome {
                        stop: LoopStop::BlockedDoubleIterate,
                        iterations_run,
                        last_verdict,
                        verdict_source_disagreement,
                    });
                }
                // Otherwise continue to the next iteration.
                prev_verdict = Some(Verdict::Iterate);
            }
            None => {
                // No parseable verdict this iteration. Treat as a non-repeating
                // ITERATE for continuation purposes (keep going / hit the cap),
                // but do NOT let it satisfy the double-ITERATE plateau — reset
                // the streak so a garbage verdict can't masquerade as a repeat.
                tracing::warn!(
                    "loop_mode: iteration {iter} produced no parseable verdict; continuing"
                );
                prev_verdict = None;
            }
        }
    }

    // Fell out of the loop without SHIP/BLOCKED ⇒ the budget ran out.
    progress(LoopProgress {
        iteration: iterations_run,
        max_iterations: cap,
        phase: "done",
        detail: format!("Iteration cap reached ({cap}) without a SHIP/BLOCKED verdict"),
        last_verdict: last_verdict.map(|v| v.as_str()),
    });
    Ok(LoopOutcome {
        stop: LoopStop::CapReached,
        iterations_run,
        last_verdict,
        verdict_source_disagreement,
    })
}

/// Emit the terminal `"done"` progress event with the deciding verdict.
fn emit_done(
    progress: &(impl Fn(LoopProgress) + Send + Sync),
    iter: u32,
    cap: u32,
    verdict: Verdict,
) {
    progress(LoopProgress {
        iteration: iter,
        max_iterations: cap,
        phase: "done",
        detail: format!("Loop stopped on iteration {iter}/{cap}: {}", verdict.as_str()),
        last_verdict: Some(verdict.as_str()),
    });
}

/// Embedded Lead role prompt. Ported in substance from
/// `agent-trio-template/portable/prompts/lead.md`, adapted per the design doc:
/// this harness DOES have sub-agents, so the "you are also the worker" line
/// becomes an explicit invitation to fan out via `spawn_subagent`. All paths
/// are relative to the loop root.
pub const LEAD_PROMPT: &str = r#"# Role: Lead (plan + implement) — one iteration

You are the Lead in a two-agent loop (Lead → Evaluator). You have NO memory of
previous iterations: everything you need is in the `loop/` directory, and
everything you decide must be written back there. The Evaluator independently
grades your iteration afterward.

## Read, in this order
1. `loop/GOAL.md` — the human's mission. Immutable to agents; overrides
   everything. Note its `profile:` line (software | data).
2. `loop/VERDICT.md` — the Evaluator's last verdict (may not exist on
   iteration 1). Every blocking issue in it MUST be addressed this iteration.
3. `loop/STATE.md` — iteration number, plus "Approaches tried and rejected":
   never retry a rejected approach; when a verdict killed one, append it there
   with one line of why.
4. `loop/PLAN.md` — your own living plan from previous iterations.

## Phase 1 — Plan (update loop/PLAN.md)
Keep PLAN.md a living, prioritized task list toward GOAL.md. Fold in the
verdict's blocking issues, mark done items, then pick the SMALLEST next
increment that is independently verifiable. Record it as:

    ## Iteration N — current increment
    Objective (one sentence), numbered tasks each with a done-criterion,
    an out-of-scope fence, and acceptance criteria the Evaluator will check
    verbatim (commands and observable behaviors — not vibes).

`profile: data`: acceptance criteria must be data ground truth — reconciliation
queries (row counts/aggregates vs source), integrity checks (nulls, duplicate
keys, schema), an idempotent re-run — and build validation into the pipeline
itself where reasonable.

Judgment calls not grounded in GOAL.md or the code: pick the reasonable option
and flag it `DECISION:` so the human can veto. If the goal is complete or
unachievable, write `## Recommendation: SHIP` (or `BLOCKED — <why>`) at the top
of PLAN.md, skip implementation, and stop — the Evaluator rules on it.

## Phase 2 — Implement
Execute the increment. This harness HAS sub-agents: you MAY call
`spawn_subagent` to fan out scoped, well-specified work (recon sweeps,
mechanical edits, test scaffolding) onto cheaper tiers, ideally BEFORE
finalizing the plan so it is grounded in the real codebase. Keep architectural
judgment and final review for yourself — you own the diff. Quality bar:
- Run the project's build/tests/linters before reporting; "done" with failing
  checks is the cardinal sin of this role.
- NEVER weaken verification to pass it: no deleting/skipping tests, no
  loosening assertions, no hardcoding expected outputs — the Evaluator audits
  test diffs and treats it as an automatic ITERATE. A genuinely wrong test may
  be fixed, with justification in the report.
- Smallest diff that satisfies the increment; match existing style; stay inside
  your own out-of-scope fence. Do not commit — leave the tree for the Evaluator.

## Write before exiting
- `loop/REPORT.md` (overwrite):

      # Report — iteration N
      ## What was done          (task-by-task, with file paths)
      ## Deviations from plan   ("None" if none)
      ## How I verified it      (commands + actual output snippets, not claims)
      ## Known weaknesses       (where you'd look first if something is broken)

- Append to `loop/LOG.md`: `- iter N | lead | <one-line summary>`
- Never edit VERDICT.md or GOAL.md.
"#;

/// Embedded Evaluator role prompt. Ported in substance from
/// `agent-trio-template/portable/prompts/evaluator.md`. The evaluator NEVER
/// fixes code — its "grader never fixes" rule is additionally ENFORCED by the
/// `loop_evaluator` profile's `ToolSelection::Deny` on the write tools, but the
/// prompt states it too.
pub const EVALUATOR_PROMPT: &str = r#"# Role: Evaluator (adversarial verify) — one iteration

You are the Evaluator in a two-agent loop (Lead → Evaluator), with no memory of
previous iterations. You are adversarial by design: find the ways the iteration
is wrong, not confirm it is right. You NEVER fix code — a broken build gets an
ITERATE verdict, not a patch.

## Read order — this matters (anti-sycophancy protocol)
Form your own verdict BEFORE reading the Lead's claims:
1. `loop/GOAL.md` — the mission (immutable; note its `profile:` line).
2. `loop/PLAN.md` — the acceptance criteria of the current increment are your
   checklist; check them verbatim.
3. The working tree — the actual diff (`git diff`, `git status`) and your OWN
   execution of builds/tests/checks.
4. ONLY AFTER you have per-criterion results: read `loop/REPORT.md` and check it
   for discrepancies against what you observed. A claim you did not reproduce
   stays unverified.

## Method
- Run the acceptance checks yourself, from scratch, then go beyond them: edge
  cases, error paths, anything the criteria imply but weren't tested. Prefer
  executing code over reading it.
- Test-integrity audit (mandatory): `git diff` on test files. Any deleted,
  skipped, weakened, or newly-hardcoded assertion is an automatic ITERATE with
  a blocking issue.
- API currency: for each significant library/API the diff touches, check that
  the code uses the current recommended API for the version PINNED in this
  project. "Deprecated in the pinned version" is blocking; "not the latest
  major" is only a non-blocking observation.
- No SHIP on iteration 1 unless your verdict lists what you actively tried to
  break and couldn't.

`profile: data` — tests are NOT sufficient ground truth; ground the verdict in
the data: reconciliation of row counts/key aggregates in vs out of each step
(finance: to the cent); nulls/duplicate keys/schema drift/timezones; re-run the
pipeline from scratch yourself (same input ⇒ same output); leakage/lookahead
checks for anything feeding models or backtests; eyeball 10–20 real rows. Cite
actual query/command output for each.

## Write before exiting — loop/VERDICT.md (overwrite)
The FIRST LINE must be exactly `VERDICT: SHIP` or `VERDICT: ITERATE` or
`VERDICT: BLOCKED` — a script parses it.

    VERDICT: SHIP|ITERATE|BLOCKED
    # Verdict — iteration N
    ## What changed since last verdict
    (if the same checks are failing as last iteration, say so explicitly)
    ## Criteria results        (each criterion PASS/FAIL with actual output)
    ## Blocking issues         (numbered; what, how to reproduce, why it blocks)
    ## Non-blocking observations
    ## Guidance for next iteration
    (for SHIP: suggested commit message; for BLOCKED: exactly what the human must decide)

## Verdict semantics — choose honestly
- SHIP — all criteria pass AND GOAL.md is satisfied. Means "ready for human
  review", never "merged".
- ITERATE — only on BLOCKING issues; style nits go under non-blocking. An issue
  once classified non-blocking may never be promoted later unless the code
  around it changed.
- BLOCKED — the loop cannot converge without a human decision. Two consecutive
  ITERATEs citing the same blocking issue = stuck: escalate to BLOCKED.

Also: append `- iter N | evaluator | VERDICT: <verdict> — <one-liner>` to
`loop/LOG.md`. If you did not run a criterion's check yourself, it is not PASS.
"#;

// ---- Loop file protocol (glue-facing, but AppState-free) ------------------
//
// The `loop/` mailbox lives at `<loop_root>/loop/`. These helpers implement the
// two pieces of the protocol the ORCHESTRATOR GLUE owns (as opposed to the
// role-owned PLAN/REPORT/VERDICT/LOG files, which the Lead/Evaluator delegations
// write themselves through their tool registries):
//   * seeding `GOAL.md` (human-owned — written once, NEVER overwritten) and
//     `STATE.md` (iteration bookkeeping — seeded once if absent) at run start;
//   * reading `VERDICT.md` back for the orchestrator's `read_verdict` closure.
// They are plain filesystem functions so they can be unit-tested against a temp
// dir with no `AppState`, DB, router, or network — the same decoupling property
// the rest of this module has.

/// The `loop/` mailbox directory under a loop root.
pub(crate) fn loop_dir(loop_root: &std::path::Path) -> std::path::PathBuf {
    loop_root.join("loop")
}

/// Seed the human-owned + bookkeeping loop files at run start.
///
/// Creates `<loop_root>/loop/`, then writes `GOAL.md` from `goal` **only if it
/// does not already exist** (GOAL.md is human-owned — an existing goal, possibly
/// hand-edited between runs, is never clobbered), and seeds `STATE.md` **only if
/// absent** with the iteration header and the two append-only sections the Lead
/// prompt references. PLAN/REPORT/VERDICT/LOG are role-owned and are NOT created
/// here. Idempotent: calling it again on an already-seeded root is a no-op for
/// both files.
pub(crate) fn ensure_loop_files(
    loop_root: &std::path::Path,
    goal: &str,
    max_iterations: u32,
) -> std::io::Result<()> {
    let dir = loop_dir(loop_root);
    std::fs::create_dir_all(&dir)?;

    let goal_path = dir.join("GOAL.md");
    if !goal_path.exists() {
        // Ensure the goal body ends with a newline so the file is well-formed.
        let body = if goal.ends_with('\n') {
            goal.to_string()
        } else {
            format!("{goal}\n")
        };
        std::fs::write(&goal_path, body)?;
    }

    let state_path = dir.join("STATE.md");
    if !state_path.exists() {
        let state = format!(
            "iteration: 1\n\
             max_iterations: {max_iterations}\n\
             status: running\n\n\
             ## Approaches tried and rejected\n\
             (append-only; the Lead adds one line per dead end, with why)\n\n\
             ## Key decisions and rationale\n\
             (append-only)\n"
        );
        std::fs::write(&state_path, state)?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Mailbox ownership lock (Phase 4b) — wire-compatible with the trio-template
// bash driver (`portable/driver.sh`): an atomic `.lock/` directory created via
// `mkdir` (here `std::fs::create_dir`, which fails if the dir exists) inside the
// loop root, holding a `pid` file. A live pid (checked the `kill -0` way, via
// `sysinfo`) means the mailbox is owned; a dead pid is a stale lock we take over.
// The lock is removed on drop so it is released on EVERY exit path — the same
// discipline as the cancel-flag deregistration in `loop_start_core`.
// ─────────────────────────────────────────────────────────────────────────────

/// Why `acquire_loop_lock` refused (or failed) to take the mailbox lock.
#[derive(Debug)]
pub(crate) enum LoopLockError {
    /// The mailbox is owned by a LIVE process (`pid`). Refuse — same UX class as
    /// the double-start refusal (the human sees why).
    Busy { pid: u32 },
    /// A filesystem error while acquiring the lock (create/read/write).
    Io(std::io::Error),
}

impl std::fmt::Display for LoopLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopLockError::Busy { pid } => {
                write!(f, "mailbox is locked by a live process (pid {pid})")
            }
            LoopLockError::Io(e) => write!(f, "lock I/O error: {e}"),
        }
    }
}

impl std::error::Error for LoopLockError {}

/// The `.lock/` directory guarding a loop root's mailbox.
pub(crate) fn lock_dir(loop_root: &std::path::Path) -> std::path::PathBuf {
    loop_root.join(".lock")
}

/// RAII guard for a held mailbox lock. Dropping it removes the `.lock/`
/// directory, so the lock is released on normal return, on any error early
/// return, AND during a panic unwind (Drop runs while unwinding) — matching the
/// cancel-flag registry's "always deregister" discipline. Best-effort: a
/// removal failure is logged, never panics (Drop must not panic).
#[derive(Debug)]
#[must_use = "the mailbox lock is released when this guard is dropped; hold it for the run's lifetime"]
pub(crate) struct LoopLock {
    dir: std::path::PathBuf,
}

impl Drop for LoopLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            // Missing dir is fine (already released / never fully created).
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "loop_mode: failed to release mailbox lock {}: {e}",
                    self.dir.display()
                );
            }
        }
    }
}

/// Is `pid` a live process right now? The `kill -0` equivalent, via `sysinfo`
/// (already a dependency). Like `kill -0` it cannot distinguish PID reuse, which
/// matches the bash driver's semantics exactly.
fn pid_is_live(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[target]), true);
    sys.process(target).is_some()
}

/// Atomically acquire the mailbox ownership lock at `<loop_root>/.lock/`,
/// wire-compatible with the trio driver. On success writes `pid` (this process)
/// and `arc_id` into the lock dir and returns a [`LoopLock`] guard.
///
/// Semantics (mirroring `portable/driver.sh:33-42`):
/// - `create_dir` succeeds (atomic) → fresh lock; write `pid` + `arc_id`.
/// - `create_dir` fails `AlreadyExists` → read `.lock/pid`:
///   - live pid → [`LoopLockError::Busy`] (refuse; caller surfaces the in-band
///     error, same UX as the double-start refusal);
///   - dead / unreadable / empty pid → stale lock: log, remove and recreate
///     (take over), then write `pid` + `arc_id`.
///
/// The parent (`loop_root`) is `create_dir_all`'d first so the atomic
/// single-dir `create_dir` of `.lock/` is the only race-relevant step (mailbox
/// seeding later `create_dir_all`s `<loop_root>/loop/` independently — harmless).
pub(crate) fn acquire_loop_lock(
    loop_root: &std::path::Path,
    arc_id: &str,
) -> std::result::Result<LoopLock, LoopLockError> {
    std::fs::create_dir_all(loop_root).map_err(LoopLockError::Io)?;
    let dir = lock_dir(loop_root);

    match std::fs::create_dir(&dir) {
        Ok(()) => finalize_lock(dir, arc_id),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing_pid = std::fs::read_to_string(dir.join("pid"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            match existing_pid {
                Some(pid) if pid_is_live(pid) => Err(LoopLockError::Busy { pid }),
                other => {
                    // Stale (dead pid) or unreadable/empty pid file → take over.
                    tracing::warn!(
                        "loop_mode: taking over stale mailbox lock {} (pid {} is gone)",
                        dir.display(),
                        other.map(|p| p.to_string()).unwrap_or_else(|| "unknown".into())
                    );
                    std::fs::remove_dir_all(&dir).map_err(LoopLockError::Io)?;
                    std::fs::create_dir(&dir).map_err(LoopLockError::Io)?;
                    finalize_lock(dir, arc_id)
                }
            }
        }
        Err(e) => Err(LoopLockError::Io(e)),
    }
}

/// Write the `pid` + `arc_id` marker files into a freshly-created lock dir and
/// wrap it in a guard. On any write error the half-built lock is removed so we
/// never leave a lock we don't hold.
fn finalize_lock(
    dir: std::path::PathBuf,
    arc_id: &str,
) -> std::result::Result<LoopLock, LoopLockError> {
    let write = (|| -> std::io::Result<()> {
        std::fs::write(dir.join("pid"), std::process::id().to_string())?;
        std::fs::write(dir.join("arc_id"), arc_id)?;
        Ok(())
    })();
    match write {
        Ok(()) => Ok(LoopLock { dir }),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(LoopLockError::Io(e))
        }
    }
}

/// Read `<loop_root>/loop/VERDICT.md` for the orchestrator's `read_verdict`
/// closure. Returns the full file contents (the pure orchestrator parses the
/// first line via [`parse_verdict_file`]); `None` when the file is missing or
/// unreadable — a filesystem hiccup must not strand the loop, and the
/// orchestrator already treats an unparseable/absent verdict as non-repeating
/// continuation.
pub(crate) fn read_loop_verdict(loop_root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(loop_dir(loop_root).join("VERDICT.md")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ---- Verdict parsing --------------------------------------------------

    #[test]
    fn verdict_parses_all_three_rulings() {
        assert_eq!(Verdict::parse("VERDICT: SHIP"), Some(Verdict::Ship));
        assert_eq!(Verdict::parse("VERDICT: ITERATE"), Some(Verdict::Iterate));
        assert_eq!(Verdict::parse("VERDICT: BLOCKED"), Some(Verdict::Blocked));
    }

    #[test]
    fn verdict_is_case_insensitive_on_keyword_and_value() {
        assert_eq!(Verdict::parse("verdict: ship"), Some(Verdict::Ship));
        assert_eq!(Verdict::parse("Verdict: Iterate"), Some(Verdict::Iterate));
        assert_eq!(Verdict::parse("VeRdIcT: bLoCkEd"), Some(Verdict::Blocked));
    }

    #[test]
    fn verdict_tolerates_surrounding_whitespace() {
        assert_eq!(Verdict::parse("   VERDICT:   SHIP   "), Some(Verdict::Ship));
        assert_eq!(Verdict::parse("\tVERDICT:\tBLOCKED"), Some(Verdict::Blocked));
    }

    #[test]
    fn verdict_rejects_malformed_first_lines() {
        // Missing keyword.
        assert_eq!(Verdict::parse("SHIP"), None);
        // Wrong keyword shape (no colon).
        assert_eq!(Verdict::parse("verdict ship"), None);
        // Empty / whitespace.
        assert_eq!(Verdict::parse(""), None);
        assert_eq!(Verdict::parse("   "), None);
        // Unknown value.
        assert_eq!(Verdict::parse("VERDICT: MAYBE"), None);
        assert_eq!(Verdict::parse("VERDICT:MAYBE"), None);
        // Extra trailing content after a valid value is NOT a clean ruling.
        assert_eq!(Verdict::parse("VERDICT: SHIP IT"), None);
        // A different keyword entirely.
        assert_eq!(Verdict::parse("STATUS: SHIP"), None);
    }

    #[test]
    fn verdict_accepts_no_space_after_colon() {
        // "VERDICT:SHIP" is still a clean ruling (rest.trim() == "SHIP").
        assert_eq!(Verdict::parse("VERDICT:SHIP"), Some(Verdict::Ship));
    }

    #[test]
    fn verdict_as_str_round_trips() {
        assert_eq!(Verdict::Ship.as_str(), "SHIP");
        assert_eq!(Verdict::Iterate.as_str(), "ITERATE");
        assert_eq!(Verdict::Blocked.as_str(), "BLOCKED");
    }

    // ---- Verdict file parsing --------------------------------------------

    #[test]
    fn parse_verdict_file_reads_first_nonempty_line() {
        let contents = "VERDICT: ITERATE\n# Verdict — iteration 3\n## Blocking issues\n";
        assert_eq!(parse_verdict_file(contents), Some(Verdict::Iterate));
    }

    #[test]
    fn parse_verdict_file_skips_leading_blank_lines() {
        let contents = "\n\n   \nVERDICT: SHIP\nrest\n";
        assert_eq!(parse_verdict_file(contents), Some(Verdict::Ship));
    }

    #[test]
    fn parse_verdict_file_empty_is_none() {
        assert_eq!(parse_verdict_file(""), None);
        assert_eq!(parse_verdict_file("\n\n  \n"), None);
    }

    #[test]
    fn parse_verdict_file_garbage_first_line_is_none() {
        assert_eq!(parse_verdict_file("hello world\nVERDICT: SHIP\n"), None);
    }

    // ---- Prompt templates -------------------------------------------------

    #[test]
    fn lead_prompt_invites_subagents_not_forbids_them() {
        // The design doc's adaptation: this harness HAS sub-agents.
        assert!(LEAD_PROMPT.contains("spawn_subagent"));
        assert!(!LEAD_PROMPT.contains("this harness has no subagents"));
        // Carries the hardened contract.
        assert!(LEAD_PROMPT.contains("SMALLEST next"));
        assert!(LEAD_PROMPT.contains("NEVER weaken verification"));
        assert!(LEAD_PROMPT.contains("Approaches tried and rejected"));
        assert!(LEAD_PROMPT.contains("Never edit VERDICT.md or GOAL.md"));
    }

    #[test]
    fn evaluator_prompt_carries_verdict_grammar_and_read_order() {
        assert!(EVALUATOR_PROMPT.contains("VERDICT: SHIP"));
        assert!(EVALUATOR_PROMPT.contains("VERDICT: ITERATE"));
        assert!(EVALUATOR_PROMPT.contains("VERDICT: BLOCKED"));
        // Anti-sycophancy read order.
        assert!(EVALUATOR_PROMPT.contains("anti-sycophancy"));
        assert!(EVALUATOR_PROMPT.contains("Form your own verdict BEFORE"));
        // Test-integrity audit.
        assert!(EVALUATOR_PROMPT.contains("Test-integrity audit"));
        // The plateau rule.
        assert!(EVALUATOR_PROMPT.contains("Two consecutive"));
    }

    #[test]
    fn evaluator_prompt_forbids_fixing_code() {
        assert!(EVALUATOR_PROMPT.contains("NEVER fix"));
        // It must not instruct the evaluator to patch a broken build.
        assert!(EVALUATOR_PROMPT.contains("ITERATE verdict, not a patch"));
    }

    // ---- Orchestrator stop conditions ------------------------------------
    //
    // All of these run the real `run_trio_loop` with stub closures — no
    // router, no DB, no network — proving the orchestrator is fully decoupled
    // from AppState.

    /// A stub verdict source: yields a fixed sequence of VERDICT.md contents,
    /// one per iteration. `spawn_evaluator` returns the same text so the
    /// deliverable and the file agree (no disagreement) unless a test overrides.
    fn verdict_sequence(seq: Vec<&'static str>) -> Arc<Mutex<std::vec::IntoIter<&'static str>>> {
        Arc::new(Mutex::new(seq.into_iter()))
    }

    #[tokio::test]
    async fn ship_on_iteration_one_stops_immediately() {
        let outcome = run_trio_loop(
            10,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: SHIP\n".to_string()) },
            || Some("VERDICT: SHIP\n".to_string()),
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::Ship);
        assert_eq!(outcome.iterations_run, 1);
        assert_eq!(outcome.last_verdict, Some(Verdict::Ship));
        assert!(!outcome.verdict_source_disagreement);
    }

    #[tokio::test]
    async fn blocked_stops_immediately() {
        let outcome = run_trio_loop(
            10,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: BLOCKED\n".to_string()) },
            || Some("VERDICT: BLOCKED\n".to_string()),
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::Blocked);
        assert_eq!(outcome.iterations_run, 1);
        assert_eq!(outcome.last_verdict, Some(Verdict::Blocked));
    }

    #[tokio::test]
    async fn two_identical_iterates_coerce_to_blocked() {
        let seq = verdict_sequence(vec!["VERDICT: ITERATE\n", "VERDICT: ITERATE\n"]);
        let seq_eval = Arc::clone(&seq);
        let seq_file = Arc::clone(&seq);
        // The evaluator deliverable and the file both draw from the same source,
        // but we need each read to return the SAME value within one iteration.
        // Use a per-iteration cell: the evaluator pulls the next item, stores
        // it, and the file read returns the stored value.
        let current: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
        let current_eval = Arc::clone(&current);
        let current_file = Arc::clone(&current);

        let outcome = run_trio_loop(
            10,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            move |_iter| {
                let seq_eval = Arc::clone(&seq_eval);
                let current_eval = Arc::clone(&current_eval);
                async move {
                    let v = seq_eval.lock().unwrap().next().unwrap_or("VERDICT: ITERATE\n");
                    *current_eval.lock().unwrap() = Some(v);
                    Ok::<String, athen_core::error::AthenError>(v.to_string())
                }
            },
            move || {
                let _ = &seq_file; // keep the same source alive/owned by this test
                current_file.lock().unwrap().map(|s| s.to_string())
            },
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::BlockedDoubleIterate);
        // Stopped on the 2nd iteration (the repeat), well before the cap of 10.
        assert_eq!(outcome.iterations_run, 2);
        assert_eq!(outcome.last_verdict, Some(Verdict::Iterate));
    }

    #[tokio::test]
    async fn all_iterate_reaches_cap() {
        // Every iteration yields ITERATE. Because the double-ITERATE rule fires
        // on the SECOND consecutive identical verdict, a pure all-ITERATE run
        // would normally coerce to BlockedDoubleIterate at iteration 2. To test
        // the CAP path we alternate ITERATE with an UNPARSEABLE verdict, which
        // is treated as non-repeating continuation and resets the streak — so
        // the loop runs to the cap without ever seeing two identical in a row.
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_eval = Arc::clone(&counter);
        let counter_file = Arc::new(AtomicUsize::new(0));
        let cap = 5u32;

        let outcome = run_trio_loop(
            cap,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            move |_iter| {
                let counter_eval = Arc::clone(&counter_eval);
                async move {
                    let n = counter_eval.fetch_add(1, Ordering::SeqCst);
                    // Even iterations: ITERATE; odd: garbage (no parseable verdict).
                    let v = if n.is_multiple_of(2) { "VERDICT: ITERATE\n" } else { "no verdict here\n" };
                    Ok::<String, athen_core::error::AthenError>(v.to_string())
                }
            },
            move || {
                let n = counter_file.fetch_add(1, Ordering::SeqCst);
                let v = if n.is_multiple_of(2) { "VERDICT: ITERATE\n" } else { "no verdict here\n" };
                Some(v.to_string())
            },
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::CapReached);
        assert_eq!(outcome.iterations_run, cap);
    }

    #[tokio::test]
    async fn lead_runs_before_evaluator_each_iteration() {
        // A shared ordering log records the sequence of role invocations. The
        // Lead for an iteration must always be logged before that iteration's
        // Evaluator.
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let log_lead = Arc::clone(&log);
        let log_eval = Arc::clone(&log);

        let outcome = run_trio_loop(
            3,
            move |_iter| {
                let log_lead = Arc::clone(&log_lead);
                async move {
                    log_lead.lock().unwrap().push("lead");
                    Ok::<String, athen_core::error::AthenError>(String::new())
                }
            },
            move |iter| {
                let log_eval = Arc::clone(&log_eval);
                async move {
                    log_eval.lock().unwrap().push("evaluator");
                    // SHIP on iteration 2 so the run has >1 iteration to check
                    // ordering across, then stops cleanly.
                    let v = if iter >= 2 { "VERDICT: SHIP\n" } else { "VERDICT: ITERATE\n" };
                    Ok::<String, athen_core::error::AthenError>(v.to_string())
                }
            },
            || None, // no VERDICT.md file → fall back to the deliverable
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::Ship);
        let seq = log.lock().unwrap().clone();
        // Iteration 1: lead, evaluator. Iteration 2: lead, evaluator (SHIP).
        assert_eq!(seq, vec!["lead", "evaluator", "lead", "evaluator"]);
    }

    #[tokio::test]
    async fn verdict_source_disagreement_is_recorded_and_file_wins() {
        // The evaluator's deliverable says SHIP, but VERDICT.md says ITERATE.
        // The file is the source of truth → the loop must NOT ship on iter 1;
        // it treats the ruling as ITERATE (continues) and flags the mismatch.
        let outcome = run_trio_loop(
            1, // cap of 1 so an ITERATE ruling ends at CapReached
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: SHIP\n".to_string()) },
            || Some("VERDICT: ITERATE\n".to_string()),
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();

        assert!(outcome.verdict_source_disagreement);
        // File said ITERATE, so no SHIP; single-iteration cap ⇒ CapReached.
        assert_eq!(outcome.stop, LoopStop::CapReached);
        assert_eq!(outcome.last_verdict, Some(Verdict::Iterate));
    }

    #[tokio::test]
    async fn zero_cap_is_clamped_to_one_iteration() {
        let outcome = run_trio_loop(
            0,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: SHIP\n".to_string()) },
            || Some("VERDICT: SHIP\n".to_string()),
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await
        .unwrap();
        assert_eq!(outcome.iterations_run, 1);
        assert_eq!(outcome.stop, LoopStop::Ship);
    }

    #[tokio::test]
    async fn lead_hard_failure_aborts_the_run() {
        let result = run_trio_loop(
            5,
            |_iter| async {
                Err::<String, athen_core::error::AthenError>(athen_core::error::AthenError::Other(
                    "lead boom".to_string(),
                ))
            },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: SHIP\n".to_string()) },
            || Some("VERDICT: SHIP\n".to_string()),
            |_p| {},
            || false, // never stop: exercises the prior (no-cancel) behavior
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_trio_loop_stops_when_should_stop_fires() {
        // The verdict source always says ITERATE (so the loop would otherwise
        // run to the cap), but the cancel predicate flips to `true` after the
        // first completed iteration. The loop must then unwind with
        // `LoopStop::Stopped` at the NEXT iteration boundary — before the cap.
        let iters = Arc::new(AtomicUsize::new(0));
        let iters_eval = Arc::clone(&iters);
        // Cancel becomes true once at least one iteration has completed.
        let iters_stop = Arc::clone(&iters);

        let outcome = run_trio_loop(
            10,
            |_iter| async { Ok::<String, athen_core::error::AthenError>(String::new()) },
            move |_iter| {
                let iters_eval = Arc::clone(&iters_eval);
                async move {
                    iters_eval.fetch_add(1, Ordering::SeqCst);
                    Ok::<String, athen_core::error::AthenError>("VERDICT: ITERATE\n".to_string())
                }
            },
            || Some("VERDICT: ITERATE\n".to_string()),
            |_p| {},
            move || iters_stop.load(Ordering::SeqCst) >= 1,
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::Stopped);
        // One full iteration completed before the stop was observed.
        assert_eq!(outcome.iterations_run, 1);
        assert_eq!(outcome.last_verdict, Some(Verdict::Iterate));
        // "stopped" is the wire string the surface reads.
        assert_eq!(LoopStop::Stopped.as_str(), "stopped");
    }

    #[tokio::test]
    async fn should_stop_true_before_first_iteration_runs_nothing() {
        // A stop requested before any iteration starts must run zero Lead/
        // Evaluator delegations and report `iterations_run == 0`.
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_lead = Arc::clone(&ran);
        let outcome = run_trio_loop(
            5,
            move |_iter| {
                let ran_lead = Arc::clone(&ran_lead);
                async move {
                    ran_lead.fetch_add(1, Ordering::SeqCst);
                    Ok::<String, athen_core::error::AthenError>(String::new())
                }
            },
            |_iter| async { Ok::<String, athen_core::error::AthenError>("VERDICT: SHIP\n".to_string()) },
            || Some("VERDICT: SHIP\n".to_string()),
            |_p| {},
            || true, // stop immediately
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop, LoopStop::Stopped);
        assert_eq!(outcome.iterations_run, 0);
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no Lead should have run");
    }

    // ---- Loop file protocol (temp-dir, no AppState) ----------------------

    #[test]
    fn ensure_loop_files_seeds_goal_and_state_when_absent() {
        let tmp = std::env::temp_dir().join(format!("athen-loop-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        ensure_loop_files(&tmp, "profile: software\nBuild the thing.", 7).unwrap();

        let dir = loop_dir(&tmp);
        let goal = std::fs::read_to_string(dir.join("GOAL.md")).unwrap();
        assert!(goal.contains("Build the thing."));
        // A goal without a trailing newline is normalized to end with one.
        assert!(goal.ends_with('\n'));

        let state = std::fs::read_to_string(dir.join("STATE.md")).unwrap();
        assert!(state.contains("iteration: 1"));
        assert!(state.contains("max_iterations: 7"));
        assert!(state.contains("Approaches tried and rejected"));

        // Role-owned files are NOT created by the glue.
        assert!(!dir.join("VERDICT.md").exists());
        assert!(!dir.join("REPORT.md").exists());
        assert!(!dir.join("PLAN.md").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_loop_files_never_overwrites_human_owned_goal() {
        let tmp = std::env::temp_dir().join(format!("athen-loop-goal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = loop_dir(&tmp);
        std::fs::create_dir_all(&dir).unwrap();
        // A pre-existing, hand-edited GOAL.md must survive a re-seed verbatim.
        std::fs::write(dir.join("GOAL.md"), "HUMAN EDITED GOAL — do not touch\n").unwrap();

        ensure_loop_files(&tmp, "some other goal the glue would write", 3).unwrap();

        let goal = std::fs::read_to_string(dir.join("GOAL.md")).unwrap();
        assert_eq!(goal, "HUMAN EDITED GOAL — do not touch\n");
        // STATE.md was absent, so it still gets seeded.
        assert!(dir.join("STATE.md").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_loop_files_is_idempotent_on_state() {
        let tmp = std::env::temp_dir().join(format!("athen-loop-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        ensure_loop_files(&tmp, "g", 2).unwrap();
        let dir = loop_dir(&tmp);
        // Simulate the Lead advancing STATE.md between runs.
        std::fs::write(dir.join("STATE.md"), "iteration: 5\ncustom body\n").unwrap();
        ensure_loop_files(&tmp, "g", 2).unwrap();
        // A second seed does NOT reset the Lead's STATE.md.
        let state = std::fs::read_to_string(dir.join("STATE.md")).unwrap();
        assert_eq!(state, "iteration: 5\ncustom body\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_loop_verdict_reads_present_file_and_none_when_absent() {
        let tmp = std::env::temp_dir().join(format!("athen-loop-verdict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Absent → None.
        assert_eq!(read_loop_verdict(&tmp), None);

        let dir = loop_dir(&tmp);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("VERDICT.md"), "VERDICT: ITERATE\nbody\n").unwrap();
        let read = read_loop_verdict(&tmp).unwrap();
        assert_eq!(parse_verdict_file(&read), Some(Verdict::Iterate));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- Mailbox ownership lock (Phase 4b) --------------------------------
    //
    // A unique temp loop-root per test invocation. Uses pid + a monotonic
    // counter so parallel test threads never collide on the same dir.
    fn unique_lock_root(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "athen-loop-lock-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn acquire_loop_lock_writes_pid_and_arc_id() {
        let root = unique_lock_root("acquire");
        let guard = acquire_loop_lock(&root, "arc_test").unwrap();
        let ld = lock_dir(&root);
        assert!(ld.is_dir(), ".lock/ must exist after acquire");
        let pid = std::fs::read_to_string(ld.join("pid")).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
        let aid = std::fs::read_to_string(ld.join("arc_id")).unwrap();
        assert_eq!(aid, "arc_test");
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn acquire_loop_lock_refuses_live_foreign_pid() {
        // Driver wire-compat: a `.lock/` created by ANOTHER (non-Athen) actor
        // with a LIVE pid must be honored. We simulate the foreign actor with the
        // test process's own pid (guaranteed live) written into the lock dir by
        // hand — exactly the shape the bash driver leaves on disk.
        let root = unique_lock_root("foreign-live");
        let ld = lock_dir(&root);
        std::fs::create_dir_all(&ld).unwrap();
        std::fs::write(ld.join("pid"), std::process::id().to_string()).unwrap();

        let err = acquire_loop_lock(&root, "arc_test").unwrap_err();
        match err {
            LoopLockError::Busy { pid } => assert_eq!(pid, std::process::id()),
            other => panic!("expected Busy, got {other:?}"),
        }
        // The refusal must NOT disturb the existing lock.
        assert!(ld.join("pid").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn acquire_loop_lock_holds_off_second_acquire_while_live() {
        // A held guard (our own live pid) refuses a second acquire in-process.
        let root = unique_lock_root("held-live");
        let guard = acquire_loop_lock(&root, "arc_a").unwrap();
        let err = acquire_loop_lock(&root, "arc_b").unwrap_err();
        assert!(matches!(err, LoopLockError::Busy { .. }));
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn acquire_loop_lock_takes_over_stale_lock() {
        // A `.lock/` holding a DEAD pid is stale → taken over. Use pid 0, which
        // `sysinfo` never reports as a live user process (the kernel "swapper"
        // slot is not enumerated as a refreshable process).
        let root = unique_lock_root("stale");
        let ld = lock_dir(&root);
        std::fs::create_dir_all(&ld).unwrap();
        std::fs::write(ld.join("pid"), "0").unwrap();
        std::fs::write(ld.join("arc_id"), "arc_dead").unwrap();

        let guard = acquire_loop_lock(&root, "arc_new").unwrap();
        // Took over: pid + arc_id are now OURS.
        assert_eq!(
            std::fs::read_to_string(ld.join("pid")).unwrap().trim(),
            std::process::id().to_string()
        );
        assert_eq!(std::fs::read_to_string(ld.join("arc_id")).unwrap(), "arc_new");
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn acquire_loop_lock_takes_over_unreadable_pid() {
        // A `.lock/` with no/garbage pid file is also treated as stale.
        let root = unique_lock_root("garbage");
        let ld = lock_dir(&root);
        std::fs::create_dir_all(&ld).unwrap();
        std::fs::write(ld.join("pid"), "not-a-number").unwrap();

        let guard = acquire_loop_lock(&root, "arc_new").unwrap();
        assert_eq!(
            std::fs::read_to_string(ld.join("pid")).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loop_lock_released_on_drop() {
        let root = unique_lock_root("drop");
        let ld = lock_dir(&root);
        {
            let _guard = acquire_loop_lock(&root, "arc_test").unwrap();
            assert!(ld.is_dir());
        } // guard dropped here
        assert!(!ld.exists(), ".lock/ must be gone after the guard drops");
        // And the freed mailbox can be re-acquired.
        let g2 = acquire_loop_lock(&root, "arc_test").unwrap();
        assert!(ld.is_dir());
        drop(g2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loop_lock_released_on_panic() {
        // Drop runs during unwind, so a panic while the guard is held still
        // releases the lock — the same all-paths coverage the cancel-flag
        // deregistration has (verified against the panic path).
        let root = unique_lock_root("panic");
        let ld = lock_dir(&root);
        let root_for_closure = root.clone();
        let res = std::panic::catch_unwind(move || {
            let _guard = acquire_loop_lock(&root_for_closure, "arc_test").unwrap();
            panic!("boom inside the locked region");
        });
        assert!(res.is_err(), "the closure must have panicked");
        assert!(!ld.exists(), ".lock/ must be released even on panic");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn code_mode_loop_root_is_per_arc_nested() {
        // Path-shape guard for the Code-Mode branch mirror in
        // `run_loop_for_arc` / `loop_start_core`: a Code-Mode repo root anchors a
        // per-arc `loop-runs/<sanitized-arc-id>/` mailbox, so two arcs rooted at
        // the SAME repo can never share a mailbox.
        use crate::state::sanitize_arc_id_for_path;
        let repo = std::path::Path::new("/repos/myproj");
        let a = repo
            .join("loop-runs")
            .join(sanitize_arc_id_for_path("arc_a"));
        let b = repo
            .join("loop-runs")
            .join(sanitize_arc_id_for_path("arc_b"));
        assert_eq!(a, std::path::Path::new("/repos/myproj/loop-runs/arc_a"));
        assert_ne!(a, b, "distinct arcs at the same repo get distinct mailboxes");
    }

    // ---- Guardrails audit (design-doc invariants, code-enforced) ----------
    //
    // `docs/LOOP_MODE.md` and GOAL.md require these guardrails enforced in
    // code, not just prompts:
    //   (1) the CLIENT start surface (`loop_start`/`loop_stop`/`loop_status`,
    //       `run_loop_for_arc`, `run_trio_loop`) is never sense-triggered and
    //       is never re-exposed as an agent tool;
    //   (2) the loop never commits — no git-commit code path in the machinery;
    //   (3) roles cannot start loops — no recursion.
    //
    // PHASE-4a CHANGE (agent-callable `start_loop`): guardrail (3) used to be a
    // blanket ABSENCE property — "the string `start_loop` appears NOWHERE in the
    // agent tool registry". That invariant legitimately changes now that a
    // fenced, risk-gated `start_loop` agent tool exists. It is replaced below by
    // STRICTLY STRONGER STRUCTURAL invariants that constrain HOW the tool is
    // wired, not merely that a string is missing:
    //   (3a) the `start_loop` runner is INJECTION-GATED — advertised only when
    //        the runner closure is wired (`loop_start_runner.is_some()`), and the
    //        ONLY place a `Some(..)` loop runner is built is the top-level
    //        `assemble_app_tool_registry`; every sub-agent / worker / loop-role /
    //        save-only registry gets `None`, so a loop role or delegated worker
    //        structurally cannot start another loop (the deep_research-style
    //        recursion guarantee, applied to loops);
    //   (3b) a SENSE FENCE gates the runner on `ArcSource::UserInput` and fails
    //        closed on any other origin or ambiguity;
    //   (3c) a RISK GATE denies in Bunker, requires an approval round-trip in
    //        Assistant, and only auto-proceeds in Yolo.
    // Each of (3a)-(3c) is a tighter constraint than the old blanket absence:
    // the old test would pass for ANY wiring that merely avoided the literal
    // string; these tests fail unless the tool is wired exactly as the security
    // model requires. The still-valid halves of guardrail (1) — the client
    // start surface staying out of `sense_router.rs`, and the loop machinery
    // symbols staying out of `app_tools.rs` — are RETAINED unchanged below.
    //
    // The durable guard remains a STRUCTURAL assertion over the actual shipped
    // source, embedded at compile time via `include_str!` (no filesystem
    // dependency at test time — the bytes are the crate's own source). Any
    // regression that violates one of these invariants trips the tests.

    /// The CLIENT start surface (`loop_start`/`loop_stop`/`loop_status`,
    /// `run_loop_for_arc`, `run_trio_loop`) must NOT appear in the sense-trigger
    /// path (`sense_router.rs`) NOR in the agent tool registry (`app_tools.rs`)
    /// — that keeps the loop client-triggered only and keeps the loop
    /// dispatch/orchestration machinery out of the agent tool surface.
    ///
    /// PHASE-4a: the agent-callable `start_loop` TOOL NAME now legitimately
    /// exists in `app_tools.rs`, so `start_loop` is dropped from the
    /// `app_tools.rs` forbidden set (it is still forbidden in `sense_router.rs`
    /// — a loop must never be sense-triggered). Every OTHER client-surface
    /// symbol stays forbidden in `app_tools.rs`: the tool must reuse
    /// `loop_start_core` from the STATE/COMMANDS glue, never inline
    /// `run_loop_for_arc` / `run_trio_loop` / the `loop_start`/`stop`/`status`
    /// command names into the tool registry.
    #[test]
    fn guardrail_start_surface_absent_from_sense_and_agent_tool_paths() {
        let sense_router = include_str!("sense_router.rs");
        let app_tools = include_str!("app_tools.rs");

        // The sense path must reference NONE of the loop surface — including the
        // agent-tool name `start_loop`. A loop is never sense-triggered.
        let sense_forbidden = [
            "loop_start",
            "loop_stop",
            "loop_status",
            "run_loop_for_arc",
            "run_trio_loop",
            "start_loop",
        ];
        for sym in sense_forbidden {
            assert!(
                !sense_router.contains(sym),
                "guardrail violated: `{sym}` leaked into sense_router.rs; a loop must \
                 never be sense-triggered"
            );
        }

        // The agent tool registry must never inline the loop ORCHESTRATION /
        // DISPATCH surface: the `start_loop` tool reuses `loop_start_core` (the
        // client glue that owns the double-start refusal + background spawn) and
        // must NOT call the raw orchestration entry points. `run_loop_for_arc`
        // and `run_trio_loop` are the unambiguous markers of that surface —
        // neither may appear in app_tools.rs. (The `loop_start_*`/`loop_stop`/
        // `loop_status` command NAMES cannot be substring-checked here anymore:
        // the correct injection field `loop_start_runner` and the reuse-seam
        // reference `loop_start_core` legitimately contain `loop_start` — so
        // command-name leakage is instead precluded by the dispatch-symbol check
        // plus the registration/dispatch-arm checks below.)
        for sym in ["run_loop_for_arc", "run_trio_loop"] {
            assert!(
                !app_tools.contains(sym),
                "guardrail violated: `{sym}` leaked into app_tools.rs; the loop \
                 orchestration surface must stay in the client-only glue — the \
                 `start_loop` tool must reuse `loop_start_core`, not inline dispatch"
            );
        }
        // The client command names must not be REGISTERED as agent tools. The
        // only loop tool that may be pushed into the registry is `start_loop`;
        // assert none of the client command names appear as a tool `name:`.
        for cmd in ["loop_start", "loop_stop", "loop_status"] {
            let as_tool_name = format!("name: \"{cmd}\"");
            assert!(
                !app_tools.contains(&as_tool_name),
                "guardrail violated: the client command `{cmd}` is registered as an \
                 agent tool; only `start_loop` may be an agent-callable loop tool"
            );
        }

        // POSITIVE injection-gating check: the `start_loop` tool is advertised
        // ONLY when its runner closure is wired — never unconditionally.
        assert!(
            app_tools.contains("if self.loop_start_runner.is_some()"),
            "guardrail violated: the `start_loop` tool must be registered only \
             behind an `if self.loop_start_runner.is_some()` gate, so it is absent \
             from any registry (sub-agent/worker/loop-role/save-only) that gets \
             `None` — the structural no-recursion guarantee"
        );
        // POSITIVE: `start_loop` really is registered as a tool name (the tool
        // exists — this is the invariant that legitimately replaced the old
        // "absent everywhere" assertion).
        assert!(
            app_tools.contains("name: \"start_loop\""),
            "the agent-callable `start_loop` tool must be registered by name"
        );

    }

    /// The loop machinery must contain no git-commit / process-spawn code path
    /// — "the loop never commits" (GOAL line 39, guardrail 2). Every "commit"
    /// mention in `loop_mode.rs` is prompt text inside the role templates; this
    /// asserts there is no executable commit/process call in this module.
    #[test]
    fn guardrail_loop_machinery_has_no_commit_or_process_spawn_code() {
        // The loop's pure orchestrator lives in this module. The only executable
        // path it drives is `run_trio_loop` → the caller-supplied role/verdict/
        // progress closures; it neither shells out nor commits. We prove the
        // "no commit" property structurally in TWO ways.
        //
        // (a) The word "commit" that DOES appear in this module is confined to
        //     the two role-prompt CONSTANTS — never executable code. Assert the
        //     prompt text carries the intended instructions (positive check)…
        assert!(
            LEAD_PROMPT.contains("Do not commit"),
            "the Lead prompt must instruct the role not to commit"
        );
        assert!(
            EVALUATOR_PROMPT.contains("commit message"),
            "the Evaluator prompt references a suggested commit message for SHIP"
        );

        // (b) …and there is NO git-commit / subprocess-spawn call in the glue
        //     that runs the loop (`run_loop_for_arc` in state.rs, the
        //     `loop_*_core` fns in commands.rs). A real commit path would live in
        //     one of those, so scan their source for code-level markers. (These
        //     files do not embed this test's own marker literals, so the scan is
        //     free of the self-inclusion that scanning loop_mode.rs would incur.)
        let state_src = include_str!("state.rs");
        let commands_src = include_str!("commands.rs");
        // Narrow to the loop regions so an unrelated commit call elsewhere in
        // these large files can't spuriously trip (or mask) the guardrail.
        let state_region = loop_source_region(state_src, "async fn run_loop_for_arc", 400);
        let commands_region = loop_source_region(commands_src, "async fn loop_start_core", 400);
        for (name, region) in [
            ("state.rs::run_loop_for_arc", state_region),
            ("commands.rs::loop_start_core", commands_region),
        ] {
            for marker in ["Command::new", "process::Command", "git commit", ".commit("] {
                assert!(
                    !region.contains(marker),
                    "guardrail violated: `{marker}` appears in {name} — the loop \
                     machinery must never run a git commit or spawn a process"
                );
            }
        }
    }

    /// Return a window of `src` beginning at the first line containing `anchor`
    /// and spanning `lines` lines — used to scope guardrail source scans to the
    /// loop machinery region of a large file.
    fn loop_source_region(src: &str, anchor: &str, lines: usize) -> String {
        let all: Vec<&str> = src.lines().collect();
        let start = all
            .iter()
            .position(|l| l.contains(anchor))
            .unwrap_or_else(|| panic!("anchor `{anchor}` not found — loop machinery moved?"));
        all[start..(start + lines).min(all.len())].join("\n")
    }

    /// (3a) INJECTION-ONLY-AT-TOP (no recursion). Strictly stronger than the
    /// old blanket "`start_loop` absent from app_tools.rs": that would have
    /// passed for ANY wiring avoiding the literal string. This asserts the
    /// runner is wired EXACTLY as the recursion model requires — the `Some(..)`
    /// loop runner is built only in the top-level `assemble_app_tool_registry`,
    /// and every registry built by `run_loop_for_arc` (the loop roles) uses the
    /// bare base assembler with the loop-runner slot left `None`.
    #[test]
    fn guardrail_start_loop_runner_injected_only_at_top_level() {
        let state = include_str!("state.rs");

        // The role glue must build the bare base (client-only start surface
        // withheld). run_loop_for_arc calls the bare base assembler…
        let run_loop_region = loop_source_region(state, "async fn run_loop_for_arc", 200);
        assert!(
            run_loop_region.contains("assemble_base_app_tool_registry"),
            "run_loop_for_arc must build role registries via the bare base assembler"
        );
        // …and it must NOT build a `Some` loop runner: no `LoopStartRunner`
        // closure is constructed inside the loop-role glue (it would be the
        // recursion surface). The only place a loop runner is *set* to something
        // other than None is the top-level wrapper.
        assert!(
            !run_loop_region.contains("with_loop_start_runner(Some")
                && !run_loop_region.contains("as crate::app_tools::LoopStartRunner"),
            "guardrail violated: run_loop_for_arc constructs a start_loop runner — a \
             loop role could then start another loop (recursion). The loop-role \
             registry must pass `None` in the loop-runner slot."
        );

        // The ONLY builder of a `Some(..)` loop-start runner is the top-level
        // `assemble_app_tool_registry`. Assert exactly one closure is cast to
        // `LoopStartRunner`, and it lives after the top-level wrapper's
        // signature (not in any base/worker/role path).
        let cast_marker = "as crate::app_tools::LoopStartRunner";
        assert_eq!(
            state.matches(cast_marker).count(),
            1,
            "there must be exactly ONE start_loop runner closure in state.rs (the \
             top-level injection point); found a different count — a second builder \
             would be a recursion surface"
        );
        let top_level_region =
            loop_source_region(state, "pub(crate) async fn assemble_app_tool_registry", 400);
        assert!(
            top_level_region.contains(cast_marker),
            "the sole start_loop runner must be built inside the top-level \
             assemble_app_tool_registry (injection only at the top)"
        );
    }

    /// Iteration-budget clamp for an agent-started loop (DECISION-1): absent or
    /// zero → default; in-range preserved; over-ceiling clamped down.
    #[test]
    fn start_loop_clamps_iteration_budget() {
        use crate::state::{
            clamp_loop_max_iterations, LOOP_START_DEFAULT_MAX_ITERATIONS,
            LOOP_START_MAX_ITERATIONS_CEILING,
        };
        assert_eq!(
            clamp_loop_max_iterations(None),
            LOOP_START_DEFAULT_MAX_ITERATIONS
        );
        assert_eq!(
            clamp_loop_max_iterations(Some(0)),
            LOOP_START_DEFAULT_MAX_ITERATIONS
        );
        assert_eq!(clamp_loop_max_iterations(Some(1)), 1);
        assert_eq!(clamp_loop_max_iterations(Some(7)), 7);
        assert_eq!(
            clamp_loop_max_iterations(Some(1000)),
            LOOP_START_MAX_ITERATIONS_CEILING
        );
    }

    /// (3b) SENSE FENCE. The start_loop runner must gate on
    /// `ArcSource::UserInput` and fail closed otherwise. Asserted both
    /// structurally (the fence fn is referenced) and behaviorally (the pure
    /// `loop_start_sense_fence` decides correctly for every origin + ambiguity).
    #[test]
    fn guardrail_start_loop_has_sense_fence() {
        use athen_persistence::arcs::ArcSource;
        use crate::state::loop_start_sense_fence;

        let state = include_str!("state.rs");
        assert!(
            state.contains("loop_start_sense_fence"),
            "the start_loop runner must apply the sense fence"
        );

        // Direct chat → allowed to proceed to the gate.
        assert!(loop_start_sense_fence(Some(ArcSource::UserInput)).is_ok());
        // Every sense/automated origin → refused.
        for src in [
            ArcSource::Email,
            ArcSource::Calendar,
            ArcSource::Messaging,
            ArcSource::System,
        ] {
            assert!(
                loop_start_sense_fence(Some(src.clone())).is_err(),
                "sense fence must refuse {src:?}"
            );
        }
        // Ambiguity (arc unavailable) → fail closed (refuse).
        assert!(loop_start_sense_fence(None).is_err());
    }

    /// (3c) RISK GATE. Bunker denies; Assistant requires an approval round-trip;
    /// Yolo proceeds. Asserted both structurally (the runner references the
    /// approval round-trip and the Bunker branch) and behaviorally (the pure
    /// `loop_start_gate_for_mode` maps each mode correctly).
    #[test]
    fn guardrail_start_loop_risk_gated_per_mode() {
        use athen_core::config::SecurityMode;
        use crate::state::{loop_start_gate_for_mode, LoopStartGate};

        assert_eq!(
            loop_start_gate_for_mode(SecurityMode::Yolo),
            LoopStartGate::Proceed,
            "Yolo must proceed without a prompt"
        );
        assert_eq!(
            loop_start_gate_for_mode(SecurityMode::Assistant),
            LoopStartGate::RequireApproval,
            "Assistant must require an approval round-trip"
        );
        assert_eq!(
            loop_start_gate_for_mode(SecurityMode::Bunker),
            LoopStartGate::Deny,
            "Bunker must deny outright"
        );

        // Structural: the runner region wires the approval round-trip for the
        // Assistant path and does not auto-proceed. Scope to the runner closure.
        let state = include_str!("state.rs");
        let runner_region = loop_source_region(state, "let loop_runner:", 130);
        assert!(
            runner_region.contains("ask_with_escalation")
                && runner_region.contains("pick_primary"),
            "the start_loop runner must route Assistant approvals through the \
             approval router (ask_with_escalation / pick_primary)"
        );
        assert!(
            runner_region.contains("LoopStartGate::Deny"),
            "the start_loop runner must handle the Bunker deny branch"
        );
    }
}

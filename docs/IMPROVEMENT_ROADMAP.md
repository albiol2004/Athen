# Improvement roadmap

This document captures a repository-level review of Athen as an alpha local-first AI agent. It is intentionally practical: each section describes the current observation, why it matters, and a concrete step-by-step path to improve it.

The goal is not to add another feature. Athen already has a large and ambitious surface area. The highest-leverage work now is making the project more trustworthy, easier to review, easier to install, and safer to extend.

## Executive summary

Athen is already in a strong position for an early solo project:

- The product positioning is clear: local-first, native, user-owned keys, no telemetry.
- The Rust workspace is split into focused crates, with `athen-core` acting as the contract layer.
- The README explains the product well and is honest about alpha status and unsigned binaries.
- The project has CI for formatting, Clippy, and tests.
- Security is clearly part of the design: risk scoring, sandboxing, approval gates, credential vaults, and a security policy are already present.

The main risks are not lack of ambition. The main risks are operational trust, reviewability, and maintainability:

- The frontend is currently very large and centralized.
- Security-sensitive behavior needs more regression testing.
- CI should continuously enforce dependency/advisory checks.
- Packaging and signing are a major trust bottleneck.
- User-facing documentation should be clearer for non-technical users.

## Priority 1: security hardening and visible trust

Athen touches unusually sensitive surfaces: email, calendar, contacts, local files, shell execution, API credentials, Telegram, GitHub identity, MCP servers, and outbound phone calls. For that reason, visible security posture matters as much as implementation quality.

### 1.1 Enable a strict Tauri Content Security Policy

Current concern:

- The Tauri app currently disables CSP.
- The app exposes a large DOM surface that renders dynamic content, settings, tool output, markdown-like responses, credentials forms, and external links.
- A desktop agent with shell/file tools should treat frontend injection risk as high impact even if the app is local.

Recommended steps:

1. Add a restrictive CSP in `crates/athen-app/tauri.conf.json`.
2. Start with `default-src 'self'` and only open specific channels required by Tauri IPC, local provider URLs, and documented remote services.
3. Avoid broad `script-src 'unsafe-inline'` if possible.
4. Keep any unavoidable `style-src 'unsafe-inline'` documented as a temporary compromise.
5. Add a short `docs/SECURITY_NOTES.md` entry explaining which CSP permissions are needed and why.
6. Test onboarding, chat streaming, settings, provider tests, update banner, external links, and local model connections under the new CSP.

Example starting point, to be refined against real runtime requirements:

```json
"security": {
  "csp": "default-src 'self'; img-src 'self' asset: https: data:; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src ipc: http://localhost:* https:;"
}
```

### 1.2 Run dependency and advisory checks in CI

Current concern:

- `deny.toml` exists and documents an accepted advisory.
- However, advisory checks should run continuously, not manually.

Recommended steps:

1. Add a dedicated `cargo-deny` job to CI.
2. Keep the job read-only and safe for fork PRs.
3. Fail the build on untriaged advisories.
4. Document accepted advisories in both `deny.toml` and `SECURITY.md`.
5. Revisit ignored advisories during each minor release.

This PR wires the existing `deny.toml` into CI as a small first step.

### 1.3 Add release checksums and SBOMs

Current concern:

- The release workflow builds platform bundles and updater artifacts.
- Users installing unsigned binaries should have easy integrity checks.

Recommended steps:

1. Generate `SHA256SUMS` for all release artifacts.
2. Attach the checksum file to every release.
3. Generate an SBOM using `cargo auditable`, `cargo about`, or a comparable tool.
4. Include the SBOM in release assets.
5. Add a short installation verification section to the README.

### 1.4 Prioritize code signing

Current concern:

- The README is honest that binaries are unsigned.
- For a local agent that asks for credentials and file permissions, unsigned binaries are a major adoption blocker.

Recommended steps:

1. Add macOS signing and notarization.
2. Add Windows Trusted Signing or an equivalent signing path.
3. Keep Linux package repositories as the recommended Linux installation path.
4. Make AppImage fallback/experimental rather than the primary Linux route where system packages exist.
5. Document the signing status per platform.

## Priority 2: frontend maintainability

The frontend is currently implemented as large static HTML, CSS, and JavaScript files. This keeps the build simple, but it creates a long-term maintenance bottleneck.

### 2.1 Split the frontend into modules

Recommended target structure:

```text
frontend/
  src/
    main.js
    tauri.js
    state/
      arcs.js
      settings.js
      streaming.js
    views/
      chat.js
      settings.js
      calendar.js
      contacts.js
      memory.js
      wakeups.js
    components/
      modal.js
      toast.js
      tool-card.js
      sidebar.js
    services/
      api.js
      events.js
    utils/
      dates.js
      sanitize.js
      markdown.js
  styles/
    tokens.css
    layout.css
    chat.css
    settings.css
    calendar.css
```

Step-by-step migration:

1. Extract pure utility functions first: dates, formatting, escaping, token estimates, DOM helpers.
2. Extract Tauri invocation wrappers into a single API module.
3. Move each major view into its own file without changing behavior.
4. Split CSS by screen/feature while keeping the existing visual tokens.
5. Add linting only after the first split, so the initial refactor stays mechanical.
6. Add UI smoke tests after views are separable.

### 2.2 Add frontend checks

Recommended steps:

1. Add a minimal frontend package manifest if the project accepts Node tooling.
2. Add Prettier for formatting.
3. Add ESLint or Biome for basic correctness checks.
4. Add Playwright smoke tests for critical flows:
   - first launch/onboarding opens;
   - settings screen opens;
   - provider form can be filled without crashing;
   - arc switching works;
   - calendar view opens;
   - wake-up form opens;
   - permission modal appears and can be dismissed.

This can be done without migrating to React. The immediate goal is maintainability, not framework churn.

## Priority 3: safety regression tests

For a proactive agent, tests should focus on the behaviors users trust the most.

### 3.1 Risk system golden tests

Add test cases for:

- Destructive shell commands.
- External sends to unknown contacts.
- Sends to the authenticated owner.
- File reads inside allowed grants.
- File reads outside allowed grants.
- Writes inside project grants.
- Writes outside project grants.
- Wake-ups with restricted tool allowlists.
- Sub-agents inheriting restrictions.
- MCP tools attempting to exceed declared risk.

Suggested structure:

```text
crates/athen-risk/tests/golden_risk_cases.rs
crates/athen-agent/tests/wakeup_restrictions.rs
crates/athen-agent/tests/subagent_restrictions.rs
```

### 3.2 Shell and path gate tests

Add cases for:

- Relative path traversal.
- Symlinks escaping a granted directory.
- Writes to parent directories.
- Delete/rewrite operations with checkpoint snapshots.
- Platform-specific shell command parsing.
- Nushell vs native shell behavior.

### 3.3 Mock LLM test support

Introduce a small test-support crate or module with deterministic LLM responses.

Suggested structure:

```text
crates/athen-test-support/
  src/mock_llm.rs
  src/mock_tool_registry.rs
  golden_transcripts/
```

Benefits:

- Agent-loop tests become deterministic.
- CI does not depend on real LLM providers.
- Safety regressions can be tested without cost or network flakiness.

## Priority 4: release and packaging reliability

### 4.1 Clarify Linux install recommendations

The README already documents AppImage issues on some hosts. To reduce user confusion, provide a compatibility table.

Suggested table:

```markdown
| Platform | Recommended install | Status |
|---|---|---|
| Arch / EndeavourOS / Manjaro | AUR | Recommended |
| Fedora / RHEL-family | COPR/RPM | Recommended |
| Debian / Ubuntu | `.deb` | Recommended |
| Other Linux | AppImage | Fallback / experimental |
| macOS Apple Silicon | DMG | Alpha, unsigned until signing lands |
| Windows x64 | NSIS installer | Alpha, unsigned until signing lands |
```

### 4.2 Add release smoke checks

Recommended steps:

1. After building bundles, verify expected files exist.
2. Validate updater manifest contains all signed platforms expected for that release.
3. Generate checksums.
4. Upload artifacts even when publishing stays as a draft.
5. Add release notes that separate stable, beta, and experimental features.

## Priority 5: user-facing documentation

The technical documentation is strong, but everyday users need a simpler path.

Recommended docs:

```text
docs/user/
  getting-started.md
  connect-email.md
  connect-calendar.md
  local-models.md
  permissions.md
  privacy.md
  troubleshooting.md
```

Recommended content style:

- Short, task-based pages.
- Screenshots or GIFs where possible.
- Clear warnings for alpha limitations.
- Examples of safe vs risky configurations.
- A short explanation of Bunker / Assistant / Yolo modes in plain language.

## Priority 6: product clarity

Athen has a broad feature set. The README should continue to be ambitious, but the maturity level of each capability should be explicit.

Recommended change:

```markdown
| Feature | Maturity |
|---|---|
| Core chat and agent loop | Stable alpha |
| Local model routing | Stable alpha |
| Email monitor | Beta |
| Calendar CalDAV sync | Beta |
| MCP runtime | Beta |
| Voice calls | Experimental |
| Sub-agents | Experimental |
| Headless daemon | Planned |
```

This reduces expectation mismatch while still showing momentum.

## Suggested implementation order

1. Wire `cargo-deny` into CI.
2. Add a practical threat model.
3. Add CSP hardening work behind a focused PR.
4. Add golden tests for risk and shell gating.
5. Add checksums/SBOM to releases.
6. Split frontend utilities and API wrappers.
7. Split major frontend views.
8. Add Playwright smoke tests.
9. Improve user-facing setup docs.
10. Complete signing/notarization.

## Definition of done for the next hardening cycle

A reasonable next milestone would be:

- `cargo fmt`, Clippy, tests, and `cargo deny` pass in CI.
- The threat model covers the major agent-specific abuse cases.
- At least 20 risk/path golden tests are committed.
- Releases include SHA256 checksums.
- README clearly distinguishes recommended vs fallback install methods.
- Frontend has started moving from monolithic files into feature modules.

That would make Athen feel much more reviewable and safer to recommend without changing the core product vision.
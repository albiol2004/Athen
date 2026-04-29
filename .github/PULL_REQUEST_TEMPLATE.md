## What does this PR do

<!-- Short summary. One sentence is fine for small PRs. -->

## Why

<!-- The user-facing problem this solves, or the architectural reason. -->

## How

<!--
Brief overview of the approach. Highlight any non-obvious decisions or
tradeoffs. If you renamed something or restructured a module, call it
out here so reviewers don't have to reverse-engineer it from the diff.
-->

## Checklist

- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --workspace` passes locally
- [ ] New behavior has tests
- [ ] `docs/IMPLEMENTATION.md` updated if a crate's surface changed
- [ ] No new sibling-to-sibling crate dependencies (everyone goes through `athen-core`)
- [ ] `CLAUDE.md` workspace tree updated if a crate was added/renamed/removed

## Anything reviewers should know

<!-- Linked issues, related PRs, follow-up work you're deferring, etc. -->

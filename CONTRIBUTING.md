# Contributing to Athen

Thanks for taking the time to look. Athen is alpha and there's plenty
to do — code, docs, design, and bug reports are all welcome.

## Quick start

```bash
git clone https://github.com/albiol2004/Athen.git
cd Athen

# Build the whole workspace
cargo build --workspace

# Run tests
cargo test --workspace

# Lint — must be clean for CI to pass
cargo clippy --workspace --all-targets -- -D warnings
```

System dependencies for building the Tauri desktop app are listed in the
[README](README.md#system-dependencies).

## House rules

- **`athen-core` depends on nothing internal.** It defines every trait;
  every other crate implements adapters. Sibling crates do not depend on
  each other — they meet through `athen-core`. The single exception is
  `athen-app`, the composition root.
- **Independent testability.** Mock trait dependencies, not real services.
- **Zero clippy warnings.** CI runs with `-D warnings` — clippy is the
  contract.
- **Errors:** `thiserror` with `AthenError` enum, `Result<T>` from
  `athen-core::error`.
- **Async:** `tokio` runtime, `#[async_trait]` on trait definitions.
- **Logging:** `tracing` crate, never `println!`/`eprintln!` in library code.
- **HTTP:** `reqwest` with `rustls-tls` — please don't pull in OpenSSL.
- **Tests:** unit-test trait adapters in their own crate; integration tests
  go in `tests/` and exercise multiple crates together.
- **No config files for end users.** All configurable behavior should be
  reachable through the desktop UI. Athen targets non-technical users.

For deeper context read these in order:

1. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
2. [`docs/IMPLEMENTATION.md`](docs/IMPLEMENTATION.md)
3. [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md)
4. [`docs/TOOLS_AND_SENSES.md`](docs/TOOLS_AND_SENSES.md)

## Pull requests

- Fork → branch → PR. Keep PRs focused; one logical change per PR.
- Include tests for behavior changes.
- Update `docs/IMPLEMENTATION.md` when you add a crate or significantly
  change one.
- The PR template will ask about a few things — please fill it in, it
  saves a round trip.
- CI must be green before review.

## Bug reports

Bug reports go through GitHub Issues. The bug-report template asks for
the bits we always end up needing: OS, Rust version, repro steps,
expected vs actual, log excerpt with `RUST_LOG=athen_agent=info` if
relevant.

## Feature requests

Open an issue with the "feature request" template, or start a discussion
if you want to gauge interest first. The roadmap in the README is
intentionally short — there's a lot we'd like to do, and PRs that align
with the roadmap will land fastest.

## Code of conduct

Be a good neighbor. We follow the spirit of the
[Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).
Briefly: no harassment, no personal attacks, treat every drive-by
contributor like the senior developer you'd want them to grow into.

## Releasing (maintainers)

Tagged via `git tag vX.Y.Z && git push --tags`. The `release.yml` workflow
picks up the tag and builds Tauri bundles for Linux / macOS / Windows.

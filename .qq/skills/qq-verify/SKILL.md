---
description: Run the QQ verification gates in the right order after a code change and report exactly what passed, what failed, and why.
---

# QQ Verify

Use this after any Rust change in this repository, before calling work done.
The gates are the ones `AGENTS.md` lists; running them out of order wastes
minutes (clippy after a failed fmt reformats nothing; the full test suite
after a compile error tells you nothing new).

## Order

1. **Narrow first.** Run the smallest test that exercises what you changed:
   `cargo test -p <crate> <test_name_substring>`. Iterate here until green.
2. **Format.** `cargo fmt --all`, then `cargo fmt --all -- --check` must exit 0.
3. **Lint.** `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
   Fix findings; do not add `#[allow]` unless the lint is demonstrably wrong,
   and say so in a comment.
4. **Tests.** `cargo test --workspace`.
5. **Build.** `cargo build --workspace`.
6. **Minimal profile** — only when you touched `crates/qq-provider` manifests,
   features, `aws.rs`, `providers/bedrock.rs`, or `providers/mantle.rs`:
   `cargo clippy -p qq-provider --all-targets --no-default-features --features test-support -- -D warnings`
   then `cargo test -p qq-provider --no-default-features --features test-support`.
7. **Goldens** — if you changed anything in `crates/qq-protocol` that
   serializes: run `cargo test -p qq-protocol --test wire_fixtures`. If it
   fails because the shape legitimately changed, regenerate with
   `QQ_UPDATE_FIXTURES=1 cargo test -p qq-protocol --test wire_fixtures`,
   then inspect `git diff crates/qq-protocol/tests/fixtures/` and confirm
   every changed byte is intended. Bump `PROTOCOL_VERSION` if an existing
   client would fail to decode.
8. **Benchmarks** — if you touched plan compilation, the catalog, provider
   compilation, or the run loop: `cargo bench -p qq-core --bench plan_compile`
   and/or `cargo bench -p qq-provider --bench provider_compiler`. Compare
   against the last receipt in `docs/plans/`; anything over +5% needs an
   explanation or a fix.

## Reading failures

- `cargo test` output is long. Filter to what matters:
  `cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"`.
  Any line without `ok.` is the problem.
- Clippy lints this repo denies and that trip often: `manual_contains`,
  `clone_on_copy`, `redundant_locals`, `collapsible_match`, holding a
  `MutexGuard` across `.await`, and dead code (an unused pub item in a crate
  without a consumer is an error, not a warning).
- A test that passes alone but fails in the workspace run is almost always a
  shared temp directory, a fixed port, or a global `OnceLock`. Look there
  before suspecting the code under test.

## Report

State each gate you ran and its result in one line. Never say "tests pass"
if you ran a subset; say which subset. If a gate was skipped, say why it was
not applicable.

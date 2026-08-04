# xtask

Repository automation for QQ is available through `cargo xtask`.

- `cargo xtask providers check offline` runs the deterministic provider gate.
- `cargo xtask providers check live ...` runs explicitly enabled provider
  canaries.
- `cargo xtask eval run ...` builds a revision-stamped QQ binary and launches
  the pinned Harbor adapter.
- `cargo xtask eval classify ...` records one trajectory-grounded failure
  category.
- `cargo xtask eval report ...` verifies fixed trial identity and emits the
  baseline scorecard.

See `benchmarks/harbor/README.md` for the reproducible evaluation workflow.

```sh
cargo xtask providers check offline
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --provider google
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --all
```

The live command uses the checked-in matrix in `src/providers.rs`, emits one
redacted JSON record per case, and exits nonzero when a selected case fails or
has no credential. See `docs/design/providers.md` for credential, cadence, and
result-record policy. AWS-specific overrides are `QQ_CANARY_AWS_REGION`,
`QQ_CANARY_AWS_PROFILE`, `QQ_CANARY_BEDROCK_API_KEY`, and
`QQ_CANARY_MANTLE_API_KEY`.

# xtask

Repository maintenance tasks for QQ.

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

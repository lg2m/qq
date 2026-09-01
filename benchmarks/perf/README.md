# Phase 0 Performance Baseline

QQ's Phase 0 performance harness records the current default release artifact
and deterministic end-to-end runtime paths before extension work begins. Raw
reports and dependency trees are generated under `target/qq-perf/` and remain
untracked. `--output` is deliberately restricted to a new `.json` file in that
directory. Capability-relative create-new file handles stay open for the full
recording, so a concurrent path or symlink replacement cannot redirect either
receipt. The fixture and its regression policy are source controlled.

## Record A Baseline

```sh
cargo xtask perf baseline --machine-class linux-x86_64-dedicated
```

The Linux-only recorder resolves and pins the Rust host target, builds `qq` with
`cargo build --locked --release --bin qq --target <host>`, then
re-executes the benchmark worker from the optimized `xtask` binary. It defaults
to 10 requested warmups and 100 requested samples. Expensive process, tool,
stream, replay, and load cases use fixture-versioned caps; every metric records
its effective sample count. Use a stable, descriptive machine class for reports
that will be compared. A quick local smoke run can reduce the sample count, but
`--samples` must be at least five:

```sh
cargo xtask perf baseline \
  --machine-class local-smoke \
  --samples 5 \
  --warmups 0 \
  --output target/qq-perf/smoke.json
```

Every JSON report includes:

- Git revision, exact status digest, dirty-worktree flag, workspace-content and
  file-mode digest, and lockfile digest, captured before the build and verified
  again after measurement;
- build profile, feature set, host target, Rust/Cargo versions, and exact build
  commands;
- OS, architecture, kernel, CPU, logical cores, memory, load, CPU governor, and
  filesystem metadata when the host exposes them;
- release binary size/hash, dynamic libraries, and a hashed default dependency
  tree, with the artifact hash verified again after measurement;
- sample/warmup counts, monotonic-clock and percentile policy, timeout,
  durability, concurrency, and provider-fixture contracts;
- raw samples plus effective count, median, p95, p99 for sets of at least 100
  samples, min, max, and median absolute deviation; and
- correctness receipts and explicit unsupported measurement boundaries.

The first `qq --version` observation is a fresh process, not a guaranteed cold
page-cache measurement. True cache-cold startup requires a controlled fresh
machine or privileged cache control. RSS comes from `/proc`, with each active
load profile running in an isolated optimized child and a sidecar sampler. A
Linux recording fails if any required idle or active RSS sample is unavailable.
The recorder refuses non-Linux hosts until native path isolation and RSS
samplers exist, so it cannot touch a user's real QQ state on an unsupported
host. It removes inherited Rust/Cargo codegen and release-profile overrides,
pins the host target, and records hashed native-toolchain environment and Cargo
executable/configuration identities for compatibility checks. Captured metadata
subprocess output is drained into fixed-size buffers and all subprocesses use
bounded kill-and-reap cleanup.

## Compare Compatible Reports

```sh
cargo xtask perf check \
  --baseline target/qq-perf/baseline.json \
  --candidate target/qq-perf/candidate.json \
  --budgets benchmarks/perf/budgets-v1.json
```

`perf check` exits nonzero when a lower-is-better p95 regresses beyond its
relative or absolute limit, a throughput median falls beyond its lower bound, a
required p99 exceeds its cap, either report has a failed correctness receipt,
or noise exceeds the configured median absolute deviation limit. Noise checks
can be disabled only per metric when samples deliberately pool structural queue
positions; repeated batch and throughput metrics remain noise-gated. Summaries
are recomputed from raw samples before comparison. The command refuses
comparisons across incompatible schema/fixture, metric inventory, machine,
kernel, build, compiler, sample, concurrency, durability, or provider contracts.
`maximum_active_runs` metrics are informational; correctness receipts require
the observed concurrency to equal `min(session_count, configured_limit)`.

The checked-in p99 gate requires the default 100-sample qualification. A
five-sample smoke run validates execution and fixture receipts but is expected
to fail the full budget check with an insufficient-p99 diagnostic.

The checked-in budget file protects the contract, not a machine-specific raw
sample. Re-record the reference report on the same quiet machine class and
review intentional budget changes rather than copying local measurements into
source control.

## Measurement Inventory

| Area | Metrics | Boundary |
| --- | --- | --- |
| Artifact | release bytes, dependency closure, dynamic libraries | Default locked release build of the root `qq` package |
| Process startup | first and repeated `qq --version`, isolated `qq serve` readiness | Process spawn through successful exit or listening notice |
| Memory | idle/peak server RSS, isolated load-worker peak RSS | Linux `/proc`; load sidecar starts before concurrent submission |
| Runtime lifecycle | new-store open, existing-store reopen, idle shutdown | Public `SessionRuntime` lifecycle calls |
| Admission | direct and HTTP command acknowledgement | `SubmitPrompt` call through durable command receipt |
| Provider handoff | submit start to fake-provider stream entry | Public upper-bound proxy; exact scheduler claim/send instants are not exposed |
| Durable streaming | fake-provider semantic delta to committed core event | Public post-commit observation; SQLite commit instant is not exposed |
| Client delivery | fake-provider delta to authenticated QQ client SSE event | Provider network excluded; includes persistence and HTTP/SSE delivery |
| Completion | direct run completion | Submit call through committed `RunFinished` |
| State/replay | snapshot, runtime cursor replay, authenticated HTTP/SSE reconnect replay | Public runtime and `qq-client` APIs through a known terminal cursor |
| Tools | `read_file` and one-shot `shell` two-turn runs | Durable prompt through tool dispatch and final completion |
| Cancellation | cancel call through committed terminal event | Hanging deterministic provider, bounded by the runtime timeout |
| Stream scaling | 64 KiB, 512 KiB, 1 MiB and 1 MiB/512 KiB ratio | 1 KiB fake-provider deltas through durable completion |
| Load | ack, completion, batch, spread, throughput, active runs, RSS for 1/10/100 sessions | Concurrent admitted sessions with the default eight-run cap |

Exact claim/send and SQLite-commit instants are not public, so the provider
handoff and durable-stream metrics are explicitly upper-bound proxies. Exact
commit-to-TUI rendering, the full R4 long-stream/fairness suite, resolved-model
context planning/compaction, and sub-agent economics remain owned by their
readiness milestones. H0 does not mark those milestones complete. Live-provider
network latency and model quality are deliberately outside this deterministic
baseline.

## Measurement Discipline

- Prefer a clean checkout, fixed power mode, quiet host, and the same machine
  class for comparable runs.
- Do not compare reports with different sample counts or warmup counts.
- Do not remove outliers. Investigate high median absolute deviation.
- Interpret p99 only when the metric has at least 100 observations.
- Record provider/network experiments separately from these fake-provider
  runtime measurements.
- When changing a hot path, capture the pre-change report first and run the
  candidate against the same budget file.
